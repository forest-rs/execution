// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Bytecode builder ("assembler") for `execution_tape`.
//!
//! This is a small, public helper for constructing bytecode streams without manually computing
//! byte offsets or varint encodings.
//!
//! The encoding matches the "Draft encoding for minimal implemented opcodes" section in
//! `docs/v1_spec.md`.

mod calls;
pub use calls::{CallArg, CallArgs, CallBuildError, CallTarget};
mod closures;
pub use closures::{ClosureBuildError, ClosureLayout};
mod regions;
mod sources;
pub use regions::Catch;
mod tags;
pub use tags::TagBuildError;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::contract::{Contract, ContractBuildError, ContractId, ContractTableBuilder};
use crate::format::{write_sleb128_i64, write_sleb128_i128, write_uleb128_u64};
use crate::host::HostSig;
use crate::host_binding::HostImport;
use crate::opcode::Opcode;
use crate::program::{
    ByteRange, CallSigEntry, CallSigId, Const, ConstId, ElemTypeId, FunctionNameEntry,
    GuestFunctionDef, HostSigDef, HostSigId, HostSymbol, LabelNameEntry, Program, RegionDef,
    RegionId, RegionRun, SpanEntry, SpanId, StructTypeDef, SymbolId, TagEntry, TagId, TypeId,
    TypeTableDef, ValueType,
};
use crate::value::FuncId;
use crate::value::{Decimal, DecimalRounding};
use crate::verifier::{
    VerifiedProgram, VerifyConfig, VerifyError, verify_program, verify_program_owned,
};

#[cfg(doc)]
use crate::{program::FunctionDef, vm::Vm};

/// A label for control-flow targets.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Label(u32);

impl Label {
    /// Supplies arguments for a typed block target. Values are transferred simultaneously.
    #[must_use]
    pub fn with_args(self, args: &[u32]) -> BlockTarget<'_> {
        BlockTarget { label: self, args }
    }
}

/// A control-flow target with borrowed block arguments.
///
/// Pass a label directly to [`Asm::jmp`] or [`Asm::br`] for an argument-free edge.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BlockTarget<'a> {
    label: Label,
    args: &'a [u32],
}

impl From<Label> for BlockTarget<'_> {
    fn from(label: Label) -> Self {
        Self { label, args: &[] }
    }
}

/// A label unavailable for resolution or block placement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnresolvedLabel;

impl fmt::Display for UnresolvedLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "label is unresolved or its declared block was already placed"
        )
    }
}

impl core::error::Error for UnresolvedLabel {}

/// A bytecode builder error.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AsmError {
    /// A label was referenced but never placed.
    UnresolvedLabel,
    /// The produced bytecode failed verification under the provided function signature.
    Verify(VerifyError),
    /// Isolated assembly checking cannot resolve program-local contracts; use [`ProgramBuilder`].
    ProgramContract(ContractId),
    /// A signature references an absent contract or exceeds the table address space.
    Contract(ContractBuildError),
}

impl fmt::Display for AsmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnresolvedLabel => write!(f, "unresolved label"),
            Self::Verify(e) => write!(f, "verification failed: {e}"),
            Self::ProgramContract(id) => write!(
                f,
                "contract {} needs its ProgramBuilder for verification",
                id.0
            ),
            Self::Contract(error) => error.fmt(f),
        }
    }
}

impl core::error::Error for AsmError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Verify(e) => Some(e),
            Self::Contract(error) => Some(error),
            _ => None,
        }
    }
}

/// Complete assembled function body: register declarations, bytecode, spans and label names.
///
/// Keep the register declarations when constructing a [`FunctionDef`]. Bytecode alone does not
/// contain the required register contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsmParts {
    /// Declared kind of every register, including r0 and argument registers.
    pub register_types: Vec<ValueType>,
    /// Final bytecode stream.
    pub bytecode: Vec<u8>,
    /// Span table entries, encoded as `(pc_delta, span_id)`.
    pub spans: Vec<SpanEntry>,
    /// Optional named labels placed within the function.
    pub labels: Vec<NamedLabel>,
    /// Protected regions with resolved handler targets, indexed by [`RegionId`].
    pub regions: Vec<RegionDef>,
    /// Instruction ranges attributed to their innermost region, sorted by offset.
    pub region_runs: Vec<RegionRun>,
}

/// A named label placed within a function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedLabel {
    /// Bytecode pc (byte offset within the function).
    pub pc: u32,
    /// Label name.
    pub name: String,
}

impl From<UnresolvedLabel> for AsmError {
    fn from(_: UnresolvedLabel) -> Self {
        Self::UnresolvedLabel
    }
}

impl From<ContractBuildError> for AsmError {
    fn from(error: ContractBuildError) -> Self {
        Self::Contract(error)
    }
}

impl From<VerifyError> for AsmError {
    fn from(e: VerifyError) -> Self {
        Self::Verify(e)
    }
}

/// A [`ProgramBuilder`] error.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BuildError {
    /// A signature references an absent compositional contract.
    Contract(ContractBuildError),
    /// Signature-aware call emission was denied before mutation.
    Call(CallBuildError),
    /// Closure capture emission was denied before mutation.
    Closure(ClosureBuildError),
    /// Exception tag declaration or `throw` emission was denied before mutation.
    Tag(TagBuildError),
    /// A function id was out of range for the builder.
    BadFuncId {
        /// The invalid function id.
        func: u32,
    },
    /// An argument index was out of range for the function signature.
    BadArgIndex {
        /// The function id.
        func: u32,
        /// The invalid argument index.
        arg: u32,
        /// The number of arguments in the function signature.
        arg_count: u32,
    },
    /// A return index was out of range for the function signature.
    BadRetIndex {
        /// The function id.
        func: u32,
        /// The invalid return index.
        ret: u32,
        /// The number of returns in the function signature.
        ret_count: u32,
    },
    /// A function was declared but never defined.
    MissingFunctionBody {
        /// The function id that is missing a body.
        func: u32,
    },
    /// The produced program failed verification.
    Verify(VerifyError),
    /// A label was referenced but never placed.
    UnresolvedLabel,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contract(error) => error.fmt(f),
            Self::Call(error) => error.fmt(f),
            Self::Closure(error) => error.fmt(f),
            Self::Tag(error) => error.fmt(f),
            Self::BadFuncId { func } => write!(f, "invalid function id {func}"),
            Self::BadArgIndex {
                func,
                arg,
                arg_count,
            } => {
                if *arg_count == 0 {
                    write!(
                        f,
                        "invalid argument index {arg} for function {func} (function takes 0 arguments)"
                    )
                } else {
                    write!(
                        f,
                        "invalid argument index {arg} for function {func} (valid range: 0..={})",
                        arg_count - 1
                    )
                }
            }
            Self::BadRetIndex {
                func,
                ret,
                ret_count,
            } => {
                if *ret_count == 0 {
                    write!(
                        f,
                        "invalid return index {ret} for function {func} (function returns 0 values)"
                    )
                } else {
                    write!(
                        f,
                        "invalid return index {ret} for function {func} (valid range: 0..={})",
                        ret_count - 1
                    )
                }
            }
            Self::MissingFunctionBody { func } => write!(f, "missing function body for {func}"),
            Self::Verify(e) => write!(f, "verification failed: {e}"),
            Self::UnresolvedLabel => write!(f, "unresolved label"),
        }
    }
}

impl core::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Verify(e) => Some(e),
            Self::Contract(error) => Some(error),
            Self::Call(error) => Some(error),
            Self::Closure(error) => Some(error),
            Self::Tag(error) => Some(error),
            _ => None,
        }
    }
}

impl From<VerifyError> for BuildError {
    fn from(e: VerifyError) -> Self {
        Self::Verify(e)
    }
}

impl From<ContractBuildError> for BuildError {
    fn from(error: ContractBuildError) -> Self {
        Self::Contract(error)
    }
}
impl From<CallBuildError> for BuildError {
    fn from(error: CallBuildError) -> Self {
        Self::Call(error)
    }
}

impl From<ClosureBuildError> for BuildError {
    fn from(error: ClosureBuildError) -> Self {
        Self::Closure(error)
    }
}

impl From<TagBuildError> for BuildError {
    fn from(error: TagBuildError) -> Self {
        Self::Tag(error)
    }
}

impl From<UnresolvedLabel> for BuildError {
    fn from(_: UnresolvedLabel) -> Self {
        Self::UnresolvedLabel
    }
}

impl From<AsmError> for BuildError {
    fn from(e: AsmError) -> Self {
        match e {
            AsmError::UnresolvedLabel => Self::UnresolvedLabel,
            AsmError::Verify(e) => Self::Verify(e),
            AsmError::ProgramContract(id) => Self::Contract(ContractBuildError::UnknownChild(id)),
            AsmError::Contract(error) => Self::Contract(error),
        }
    }
}

/// Function signature metadata used for `finish_checked`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionSig {
    /// Argument types (excluding the implicit effect token).
    pub arg_types: Vec<ValueType>,
    /// Return types.
    pub ret_types: Vec<ValueType>,
}

impl FunctionSig {
    /// Creates a closure body signature from caller-visible call signature parts.
    ///
    /// [`Asm::call_indirect`] uses the caller-visible `call_sig` for both function values and
    /// closure values. When the callee is a closure, the VM injects the captured environment as the
    /// callee body's first argument. This helper creates that body signature by prepending
    /// [`ValueType::Agg`] to the caller-visible argument list and copying the caller-visible return
    /// list unchanged.
    ///
    /// ```
    /// # use execution_tape::asm::{FunctionSig, ProgramBuilder};
    /// # use execution_tape::program::ValueType;
    /// let call_args = [ValueType::I64];
    /// let call_rets = [ValueType::Bool];
    ///
    /// let mut builder = ProgramBuilder::new();
    /// let call_sig = builder.call_sig(FunctionSig {arg_types:(&call_args).to_vec(),ret_types:(&call_rets).to_vec()}).unwrap();
    /// let body_sig = FunctionSig::closure_body(&call_args, &call_rets);
    ///
    /// assert_eq!(body_sig.arg_types, vec![ValueType::Agg, ValueType::I64]);
    /// assert_eq!(body_sig.ret_types, vec![ValueType::Bool]);
    /// # let _ = call_sig;
    /// ```
    #[must_use]
    pub fn closure_body(call_arg_types: &[ValueType], call_ret_types: &[ValueType]) -> Self {
        let mut arg_types = Vec::with_capacity(call_arg_types.len().saturating_add(1));
        arg_types.push(ValueType::Agg);
        arg_types.extend_from_slice(call_arg_types);
        Self {
            arg_types,
            ret_types: call_ret_types.to_vec(),
        }
    }
}

/// A guest signature position, supplied as an ABI kind or a program-owned rich contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractRef {
    /// Unrefined storage/ABI kind; the builder interns it as a contract.
    Kind(ValueType),
    /// A contract previously created in this program builder.
    Id(ContractId),
}
impl From<ValueType> for ContractRef {
    fn from(kind: ValueType) -> Self {
        Self::Kind(kind)
    }
}
impl From<ContractId> for ContractRef {
    fn from(id: ContractId) -> Self {
        Self::Id(id)
    }
}

/// Authoritative compositional guest signature supplied to the ordinary function builder.
///
/// [`FunctionSig`] is the kind-only shorthand. Rich contracts use the same argument/result lists
/// and are local to the program builder that created their IDs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionAbi {
    /// Value arguments, excluding the implicit effect token.
    pub args: Vec<ContractRef>,
    /// Results in return-signature order.
    pub rets: Vec<ContractRef>,
}
impl FunctionAbi {
    /// Converts a kind-only signature to an unrefined contract signature.
    #[must_use]
    pub fn new(sig: FunctionSig) -> Self {
        sig.into()
    }
}
impl From<FunctionSig> for FunctionAbi {
    fn from(sig: FunctionSig) -> Self {
        Self {
            args: sig.arg_types.into_iter().map(ContractRef::Kind).collect(),
            rets: sig.ret_types.into_iter().map(ContractRef::Kind).collect(),
        }
    }
}
impl From<&FunctionSig> for FunctionAbi {
    fn from(sig: &FunctionSig) -> Self {
        sig.clone().into()
    }
}
impl From<&Self> for FunctionAbi {
    fn from(abi: &Self) -> Self {
        abi.clone()
    }
}

/// Convenience builder for constructing small [`Program`]s.
///
/// Owns program tables and declared guest signatures. Frontends supply their language semantics,
/// source spans, and lowering policy. Use [`Self::function_asm`] to seed function bodies from their
/// declarations and [`Asm::alloc_register`] for typed temporaries; pre-numbered frontends can
/// transfer a complete ledger through [`Asm::set_register_types`]. [`Self::emit_call`] allocates
/// call results from declared signatures without repeating result kinds or r0 operands.
///
/// For execution, see [`Vm::run`].
///
/// ## Example
///
/// ```no_run
/// extern crate alloc;
///
/// use alloc::vec;
///
/// use execution_tape::asm::{FunctionSig, ProgramBuilder};
/// use execution_tape::program::ValueType;
///
/// let mut program = ProgramBuilder::new();
/// let add_one = program.declare_function(FunctionSig {
///     arg_types: vec![ValueType::I64],
///     ret_types: vec![ValueType::I64],
/// })?;
/// let mut body = program.function_asm(add_one)?;
/// let argument = program.argument(add_one, 0)?;
/// let one = body.alloc_register(ValueType::I64);
/// let answer = body.alloc_register(ValueType::I64);
/// body.const_i64(one, 1).i64_add(answer, argument, one).ret(0, &[answer]);
/// program.define_function(add_one, body)?;
/// let _program = program.build_verified()?;
/// # Ok::<(), execution_tape::asm::BuildError>(())
/// ```
#[derive(Clone, Debug, Default)]
pub struct ProgramBuilder {
    symbols: Vec<HostSymbol>,
    const_pool: Vec<Const>,
    call_sigs: Vec<CallSigDef>,
    tags: Vec<TagDef>,
    host_sigs: Vec<HostSigDef>,
    types: TypeTableDef,
    functions: Vec<GuestFunctionDef>,
    contracts: ContractTableBuilder,
    program_name: Option<SymbolId>,
    function_names: Vec<FunctionNameEntry>,
    labels: Vec<LabelNameEntry>,
    function_input_names: Vec<FunctionInputNameDef>,
    function_output_names: Vec<FunctionOutputNameDef>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct FunctionInputNameDef {
    func: u32,
    arg: u32,
    name: SymbolId,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct FunctionOutputNameDef {
    func: u32,
    ret: u32,
    name: SymbolId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CallSigDef {
    args: Vec<ContractId>,
    rets: Vec<ContractId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TagDef {
    name: SymbolId,
    payload: Vec<ContractId>,
    results: Vec<ContractId>,
    import: bool,
}

impl ProgramBuilder {
    /// Creates an empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Interns a host symbol and returns its [`SymbolId`].
    ///
    /// This symbol table is also used for optional debug metadata (program/function/label names).
    ///
    /// Note that `SymbolId` indices start at `1` (index `0` is reserved in the encoded symbol
    /// table).
    pub fn symbol(&mut self, symbol: &str) -> SymbolId {
        if let Some(i) = self.symbols.iter().position(|s| s.symbol == symbol) {
            let raw = u32::try_from(i + 1).unwrap_or(u32::MAX);
            return SymbolId(core::num::NonZeroU32::new(raw).unwrap());
        }
        let raw = u32::try_from(self.symbols.len() + 1).unwrap_or(u32::MAX);
        let id = SymbolId(core::num::NonZeroU32::new(raw).unwrap());
        self.symbols.push(HostSymbol {
            symbol: symbol.into(),
        });
        id
    }

    /// Sets a human-readable program name (stored as a symbol id).
    pub fn set_program_name(&mut self, name: &str) -> SymbolId {
        let sym = self.symbol(name);
        self.program_name = Some(sym);
        sym
    }

    /// Sets a human-readable function name (stored as a symbol id).
    pub fn set_function_name(&mut self, func: FuncId, name: &str) -> Result<SymbolId, BuildError> {
        if self.functions.get(func.0 as usize).is_none() {
            return Err(BuildError::BadFuncId { func: func.0 });
        }
        let sym = self.symbol(name);
        if let Some(entry) = self.function_names.iter_mut().find(|e| e.func == func.0) {
            entry.name = sym;
        } else {
            self.function_names.push(FunctionNameEntry {
                func: func.0,
                name: sym,
            });
        }
        Ok(sym)
    }

    /// Sets a human-readable input name for `arg` in `func` (stored as a symbol id).
    pub fn set_function_input_name(
        &mut self,
        func: FuncId,
        arg: u32,
        name: &str,
    ) -> Result<SymbolId, BuildError> {
        let Some(def) = self.functions.get(func.0 as usize) else {
            return Err(BuildError::BadFuncId { func: func.0 });
        };
        if (arg as usize) >= def.args.len() {
            let arg_count = u32::try_from(def.args.len()).unwrap_or(u32::MAX);
            return Err(BuildError::BadArgIndex {
                func: func.0,
                arg,
                arg_count,
            });
        }

        let sym = self.symbol(name);
        if let Some(entry) = self
            .function_input_names
            .iter_mut()
            .find(|e| e.func == func.0 && e.arg == arg)
        {
            entry.name = sym;
        } else {
            self.function_input_names.push(FunctionInputNameDef {
                func: func.0,
                arg,
                name: sym,
            });
        }
        Ok(sym)
    }

    /// Sets a human-readable output name for `ret` in `func` (stored as a symbol id).
    pub fn set_function_output_name(
        &mut self,
        func: FuncId,
        ret: u32,
        name: &str,
    ) -> Result<SymbolId, BuildError> {
        let Some(def) = self.functions.get(func.0 as usize) else {
            return Err(BuildError::BadFuncId { func: func.0 });
        };
        if (ret as usize) >= def.rets.len() {
            let ret_count = u32::try_from(def.rets.len()).unwrap_or(u32::MAX);
            return Err(BuildError::BadRetIndex {
                func: func.0,
                ret,
                ret_count,
            });
        }

        let sym = self.symbol(name);
        if let Some(entry) = self
            .function_output_names
            .iter_mut()
            .find(|e| e.func == func.0 && e.ret == ret)
        {
            entry.name = sym;
        } else {
            self.function_output_names.push(FunctionOutputNameDef {
                func: func.0,
                ret,
                name: sym,
            });
        }
        Ok(sym)
    }

    /// Sets a human-readable label name for `pc` in `func` (stored as a symbol id).
    pub fn set_label_name_pc(
        &mut self,
        func: FuncId,
        pc: u32,
        name: &str,
    ) -> Result<SymbolId, BuildError> {
        if self.functions.get(func.0 as usize).is_none() {
            return Err(BuildError::BadFuncId { func: func.0 });
        }
        let sym = self.symbol(name);
        if let Some(entry) = self
            .labels
            .iter_mut()
            .find(|e| e.func == func.0 && e.pc == pc)
        {
            entry.name = sym;
        } else {
            self.labels.push(LabelNameEntry {
                func: func.0,
                pc,
                name: sym,
            });
        }
        Ok(sym)
    }

    /// Interns a constant and returns its [`ConstId`].
    pub fn constant(&mut self, c: Const) -> ConstId {
        if let Some(i) = self.const_pool.iter().position(|x| *x == c) {
            return ConstId(u32::try_from(i).unwrap_or(u32::MAX));
        }
        let id = ConstId(u32::try_from(self.const_pool.len()).unwrap_or(u32::MAX));
        self.const_pool.push(c);
        id
    }

    /// Interns a UTF-8 string constant and returns its [`ConstId`].
    pub fn const_str(&mut self, value: &str) -> ConstId {
        self.constant(Const::Str(value.into()))
    }

    /// Interns a byte string constant and returns its [`ConstId`].
    pub fn const_bytes(&mut self, value: &[u8]) -> ConstId {
        self.constant(Const::Bytes(value.into()))
    }

    /// Returns a mutable reference to the type table.
    pub fn types_mut(&mut self) -> &mut TypeTableDef {
        &mut self.types
    }

    /// Interns a struct type and returns its [`TypeId`].
    pub fn struct_type(&mut self, t: StructTypeDef) -> TypeId {
        if let Some(i) = self.types.structs.iter().position(|x| *x == t) {
            return TypeId(u32::try_from(i).unwrap_or(u32::MAX));
        }
        let id = TypeId(u32::try_from(self.types.structs.len()).unwrap_or(u32::MAX));
        self.types.structs.push(t);
        id
    }

    /// Interns an array element type and returns its [`ElemTypeId`].
    pub fn array_elem(&mut self, t: ValueType) -> ElemTypeId {
        if let Some(i) = self.types.array_elems.iter().position(|x| *x == t) {
            return ElemTypeId(u32::try_from(i).unwrap_or(u32::MAX));
        }
        let id = ElemTypeId(u32::try_from(self.types.array_elems.len()).unwrap_or(u32::MAX));
        self.types.array_elems.push(t);
        id
    }

    /// Interns a host-call signature for `symbol` and returns its [`HostSigId`].
    pub fn host_sig(&mut self, symbol: SymbolId, sig: HostSig) -> HostSigId {
        let def = HostSigDef {
            symbol,
            args: sig.args,
            rets: sig.rets,
        };
        if let Some(i) = self.host_sigs.iter().position(|x| *x == def) {
            return HostSigId(u32::try_from(i).unwrap_or(u32::MAX));
        }
        let id = HostSigId(u32::try_from(self.host_sigs.len()).unwrap_or(u32::MAX));
        self.host_sigs.push(def);
        id
    }

    /// Interns a host-call signature for `symbol` (interning the symbol string) and returns its
    /// [`HostSigId`].
    pub fn host_sig_for(&mut self, symbol: &str, sig: HostSig) -> HostSigId {
        let sym = self.symbol(symbol);
        self.host_sig(sym, sig)
    }

    /// Interns the import of a declared host operation and returns its [`HostSigId`].
    ///
    /// Equal to [`Self::host_sig_for`] with the import's symbol and signature; the host
    /// registers the same declaration with [`HostFunctions::implement`].
    ///
    /// [`HostFunctions::implement`]: crate::host_binding::HostFunctions::implement
    pub fn host_import(&mut self, import: HostImport) -> HostSigId {
        self.host_sig_for(import.symbol(), import.signature())
    }

    /// Interns an exact compositional indirect-call signature.
    pub fn call_sig(
        &mut self,
        abi: impl Into<FunctionAbi>,
    ) -> Result<CallSigId, ContractBuildError> {
        let (args, rets) = self.resolve_signature(abi.into())?;
        if let Some(i) = self
            .call_sigs
            .iter()
            .position(|x| x.args == args && x.rets == rets)
        {
            return Ok(CallSigId(u32::try_from(i).unwrap_or(u32::MAX)));
        }
        let id = CallSigId(u32::try_from(self.call_sigs.len()).unwrap_or(u32::MAX));
        self.call_sigs.push(CallSigDef { args, rets });
        Ok(id)
    }

    /// Interns a compositional contract. Create child contracts before their parents.
    ///
    /// IDs belong to this builder. Struct and array refinements are checked against the nominal
    /// schema during program verification, including contracts unused by any function.
    pub fn contract(&mut self, contract: Contract<'_>) -> Result<ContractId, ContractBuildError> {
        self.contracts.intern(contract)
    }

    fn resolve_signature(
        &mut self,
        abi: FunctionAbi,
    ) -> Result<(Vec<ContractId>, Vec<ContractId>), ContractBuildError> {
        // Preflight every explicit ID before interning any shorthand kinds.
        for reference in abi.args.iter().chain(&abi.rets) {
            if let ContractRef::Id(id) = reference
                && self.contracts.table().get(*id).is_none()
            {
                return Err(ContractBuildError::UnknownChild(*id));
            }
        }
        let mut resolve = |refs: Vec<ContractRef>| -> Result<Vec<ContractId>, ContractBuildError> {
            refs.into_iter()
                .map(|reference| match reference {
                    ContractRef::Kind(kind) => self.contracts.intern(Contract::Kind(kind)),
                    ContractRef::Id(id) => Ok(id),
                })
                .collect()
        };
        Ok((resolve(abi.args)?, resolve(abi.rets)?))
    }

    /// Declares a function signature and returns its [`FuncId`].
    ///
    /// This is useful when assembling mutually recursive or out-of-order functions: you can
    /// declare all functions up front, then reference them by [`FuncId`] in [`Asm::call`].
    pub fn declare_function(
        &mut self,
        abi: impl Into<FunctionAbi>,
    ) -> Result<FuncId, ContractBuildError> {
        let (args, rets) = self.resolve_signature(abi.into())?;
        let id = FuncId(u32::try_from(self.functions.len()).unwrap_or(u32::MAX));
        self.functions.push(GuestFunctionDef {
            args,
            rets,
            register_types: Vec::new(),
            bytecode: Vec::new(),
            spans: Vec::new(),
            regions: Vec::new(),
            region_runs: Vec::new(),
        });
        Ok(id)
    }

    /// Creates an empty body for a declared function, with r0 and its arguments declared.
    ///
    /// The returned assembler owns its register ledger and does not borrow this builder. Keep
    /// interning constants, contracts, and imports while emitting its body. Allocate temporaries
    /// with [`Asm::alloc_register`], then pass the body to [`Self::define_function`]. Rich argument
    /// contracts supply their storage kinds here; verification still enforces their full contracts.
    /// A function's declaration is unchanged, and calling this again creates another empty body.
    pub fn function_asm(&self, func: FuncId) -> Result<Asm, BuildError> {
        let function = self
            .functions
            .get(func.0 as usize)
            .ok_or(BuildError::BadFuncId { func: func.0 })?;
        let mut types = Vec::with_capacity(function.args.len() + 1);
        types.push(ValueType::Unit);
        types.extend(function.args.iter().map(|id| {
            self.contracts
                .table()
                .kind(*id)
                .expect("declared argument contracts remain in the builder")
        }));
        let mut body = Asm::new();
        body.set_register_types(types);
        Ok(body)
    }

    /// Defines the body of a previously declared function.
    pub fn define_function(&mut self, func: FuncId, a: Asm) -> Result<(), BuildError> {
        let parts = a.finish_parts()?;
        let Some(slot) = self.functions.get_mut(func.0 as usize) else {
            return Err(BuildError::BadFuncId { func: func.0 });
        };
        slot.register_types = parts.register_types;
        slot.bytecode = parts.bytecode;
        slot.spans = parts.spans;
        slot.regions = parts.regions;
        slot.region_runs = parts.region_runs;
        for l in parts.labels {
            let sym = self.symbol(&l.name);
            self.labels.push(LabelNameEntry {
                func: func.0,
                pc: l.pc,
                name: sym,
            });
        }
        Ok(())
    }

    /// Defines the body and span table of a previously declared function.
    pub fn define_function_with_spans(
        &mut self,
        func: FuncId,
        a: Asm,
        spans: Vec<SpanEntry>,
    ) -> Result<(), BuildError> {
        let parts = a.finish_parts()?;
        let Some(slot) = self.functions.get_mut(func.0 as usize) else {
            return Err(BuildError::BadFuncId { func: func.0 });
        };
        slot.register_types = parts.register_types;
        slot.bytecode = parts.bytecode;
        slot.spans = spans;
        slot.regions = parts.regions;
        slot.region_runs = parts.region_runs;
        for l in parts.labels {
            let sym = self.symbol(&l.name);
            self.labels.push(LabelNameEntry {
                func: func.0,
                pc: l.pc,
                name: sym,
            });
        }
        Ok(())
    }

    /// Adds a function built from assembler `a` with signature metadata.
    ///
    /// This resolves labels and records the typed signature. Full verification (including host-call
    /// signature checks and cross-function call checks) is performed by `build_checked`.
    pub fn push_function_checked(
        &mut self,
        a: Asm,
        abi: impl Into<FunctionAbi>,
    ) -> Result<FuncId, AsmError> {
        let parts = a.finish_parts()?;
        let (args, rets) = self.resolve_signature(abi.into())?;
        let id = FuncId(u32::try_from(self.functions.len()).unwrap_or(u32::MAX));
        self.functions.push(GuestFunctionDef {
            args,
            rets,
            register_types: parts.register_types,
            bytecode: parts.bytecode,
            spans: parts.spans,
            regions: parts.regions,
            region_runs: parts.region_runs,
        });
        for l in parts.labels {
            let sym = self.symbol(&l.name);
            self.labels.push(LabelNameEntry {
                func: id.0,
                pc: l.pc,
                name: sym,
            });
        }
        Ok(id)
    }

    /// Builds the [`Program`].
    #[must_use]
    pub fn build(mut self) -> Program {
        let protection: Vec<_> = self
            .functions
            .iter_mut()
            .map(|f| {
                (
                    core::mem::take(&mut f.regions),
                    core::mem::take(&mut f.region_runs),
                )
            })
            .collect();
        let mut p = Program::new_with_contracts(
            self.symbols,
            self.const_pool,
            self.host_sigs,
            self.types,
            self.functions,
            self.contracts,
        );
        for cs in self.call_sigs {
            let args_off = u32::try_from(p.contract_ids.len()).unwrap_or(u32::MAX);
            p.contract_ids.extend_from_slice(&cs.args);
            let args_len = u32::try_from(cs.args.len()).unwrap_or(u32::MAX);

            let rets_off = u32::try_from(p.contract_ids.len()).unwrap_or(u32::MAX);
            p.contract_ids.extend_from_slice(&cs.rets);
            let rets_len = u32::try_from(cs.rets.len()).unwrap_or(u32::MAX);

            p.call_sigs.push(CallSigEntry {
                args: ByteRange {
                    offset: args_off,
                    len: args_len,
                },
                rets: ByteRange {
                    offset: rets_off,
                    len: rets_len,
                },
            });
        }
        for (function, (regions, runs)) in protection.into_iter().enumerate() {
            p.set_protection(function, regions, runs);
        }
        // Tags follow call signatures in the contract arena, matching decoding.
        for (index, tag) in self.tags.into_iter().enumerate() {
            if tag.import {
                p.tag_imports
                    .push(TagId(u32::try_from(index).unwrap_or(u32::MAX)));
            }
            let mut pack = |ids: &[ContractId]| {
                let offset = u32::try_from(p.contract_ids.len()).unwrap_or(u32::MAX);
                p.contract_ids.extend_from_slice(ids);
                let len = u32::try_from(ids.len()).unwrap_or(u32::MAX);
                ByteRange { offset, len }
            };
            let payload = pack(&tag.payload);
            let results = pack(&tag.results);
            p.tags.push(TagEntry {
                name: tag.name,
                payload,
                results,
            });
        }
        p.program_name = self.program_name;
        p.function_names = self.function_names;
        p.labels = self.labels;

        let func_count = p.functions.len();
        let mut has_arg_names = vec![false; func_count];
        let mut has_ret_names = vec![false; func_count];
        for e in &self.function_input_names {
            if let Some(slot) = has_arg_names.get_mut(e.func as usize) {
                *slot = true;
            }
        }
        for e in &self.function_output_names {
            if let Some(slot) = has_ret_names.get_mut(e.func as usize) {
                *slot = true;
            }
        }

        for (i, f) in p.functions.iter_mut().enumerate() {
            if has_arg_names[i] {
                let offset = u32::try_from(p.value_name_ids.len()).unwrap_or(u32::MAX);
                let len = f.arg_count;
                p.value_name_ids
                    .resize(p.value_name_ids.len() + len as usize, 0);
                f.arg_name_ids = ByteRange { offset, len };
            }
            if has_ret_names[i] {
                let offset = u32::try_from(p.value_name_ids.len()).unwrap_or(u32::MAX);
                let len = f.ret_count;
                p.value_name_ids
                    .resize(p.value_name_ids.len() + len as usize, 0);
                f.ret_name_ids = ByteRange { offset, len };
            }
        }

        for e in &self.function_input_names {
            let Some(f) = p.functions.get(e.func as usize) else {
                continue;
            };
            if e.arg >= f.arg_count {
                continue;
            }
            if f.arg_name_ids.len == 0 {
                continue;
            }
            let base = f.arg_name_ids.offset as usize;
            if let Some(slot) = p.value_name_ids.get_mut(base + e.arg as usize) {
                *slot = e.name.index();
            }
        }
        for e in &self.function_output_names {
            let Some(f) = p.functions.get(e.func as usize) else {
                continue;
            };
            if e.ret >= f.ret_count {
                continue;
            }
            if f.ret_name_ids.len == 0 {
                continue;
            }
            let base = f.ret_name_ids.offset as usize;
            if let Some(slot) = p.value_name_ids.get_mut(base + e.ret as usize) {
                *slot = e.name.index();
            }
        }

        p
    }

    /// Builds the [`Program`], then verifies it.
    pub fn build_checked(self) -> Result<Program, BuildError> {
        self.build_checked_with(&VerifyConfig::default())
    }

    /// Builds the [`Program`], then verifies it with a custom verifier config.
    pub fn build_checked_with(self, cfg: &VerifyConfig) -> Result<Program, BuildError> {
        for (i, f) in self.functions.iter().enumerate() {
            if f.bytecode.is_empty() {
                return Err(BuildError::MissingFunctionBody {
                    func: u32::try_from(i).unwrap_or(u32::MAX),
                });
            }
        }
        let p = self.build();
        verify_program(&p, cfg)?;
        Ok(p)
    }

    /// Builds the [`Program`], verifies it, then wraps it as a [`VerifiedProgram`].
    pub fn build_verified(self) -> Result<VerifiedProgram, BuildError> {
        self.build_verified_with(&VerifyConfig::default())
    }

    /// Builds the [`Program`], verifies it with `cfg`, then wraps it as a [`VerifiedProgram`].
    pub fn build_verified_with(self, cfg: &VerifyConfig) -> Result<VerifiedProgram, BuildError> {
        for (i, f) in self.functions.iter().enumerate() {
            if f.bytecode.is_empty() {
                return Err(BuildError::MissingFunctionBody {
                    func: u32::try_from(i).unwrap_or(u32::MAX),
                });
            }
        }
        let p = self.build();
        Ok(verify_program_owned(p, cfg)?)
    }
}

/// Bytecode builder.
#[derive(Clone, Debug, Default)]
pub struct Asm {
    bytes: Vec<u8>,
    register_types: Vec<ValueType>,
    next_label: u32,
    labels: Vec<Option<u32>>,
    label_names: Vec<Option<String>>,
    block_params: Vec<Option<Vec<u32>>>,
    fixups: Vec<Fixup>,
    span_marks: Vec<(u32, SpanId)>,
    sources: sources::Sources,
    regions: regions::Regions,
}

#[derive(Clone, Debug)]
struct Fixup {
    at: usize,
    label: Label,
    kind: FixupKind,
}

#[derive(Copy, Clone, Debug)]
enum FixupKind {
    PcUleb32,
}

impl Asm {
    /// Creates an empty assembler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the current byte offset ("pc") in the output.
    #[must_use]
    pub fn pc(&self) -> u32 {
        u32::try_from(self.bytes.len()).unwrap_or(u32::MAX)
    }

    /// Takes ownership of the complete register kind ledger, replacing previous declarations.
    ///
    /// Include r0 (`Unit`), argument registers in signature order, and every temporary. Declaring
    /// a kind does not initialize a register or supply aggregate shape facts. Instruction methods
    /// do not infer or grow this ledger; verification rejects missing or incompatible declarations.
    pub fn set_register_types(&mut self, types: Vec<ValueType>) -> &mut Self {
        self.register_types = types;
        self
    }

    /// Appends one register declaration and returns its source register ID.
    ///
    /// This declares storage without initializing it. Instruction emission does not infer kinds.
    /// Start with [`ProgramBuilder::function_asm`] to seed r0 and the arguments from a declaration.
    /// Frontends with pre-numbered registers can transfer their ledger with
    /// [`Self::set_register_types`] and allocate additional temporaries afterward. A raw
    /// [`Self::new`] lazily reserves r0 as `Unit` on the first allocation, returning r1 for the
    /// first value register. When arguments exist, allocate their kinds in signature order before
    /// temporaries, or transfer the known r0/argument prefix with [`Self::set_register_types`].
    /// This also supports emitters that discover return types during emission and supply the
    /// completed signature to [`ProgramBuilder::push_function_checked`] afterward.
    ///
    /// # Panics
    ///
    /// Panics if the next register ID cannot be represented by `u32`.
    #[inline]
    pub fn alloc_register(&mut self, kind: ValueType) -> u32 {
        if self.register_types.is_empty() {
            self.register_types.push(ValueType::Unit);
        }
        let register = u32::try_from(self.register_types.len())
            .expect("register count exceeds the u32 address space");
        self.register_types.push(kind);
        register
    }

    /// Allocates a new label.
    #[must_use]
    pub fn label(&mut self) -> Label {
        let id = self.next_label;
        self.next_label = self.next_label.wrapping_add(1);
        self.labels.push(None);
        self.label_names.push(None);
        self.block_params.push(None);
        Label(id)
    }

    /// Reserves a block with typed parameter registers, for placement with [`Self::place`].
    ///
    /// All incoming edges must supply matching arguments. `Agg` declares only the value kind;
    /// aggregate shape facts are preserved only when all incoming values agree. Parameters may
    /// not include the effect token `r0`. The entry block uses the function signature instead.
    #[must_use]
    pub fn block(&mut self, params: &[u32]) -> Label {
        let label = self.label();
        self.block_params[label.0 as usize] = Some(params.into());
        label
    }

    /// Allocates a new label and assigns it a human-readable name.
    #[must_use]
    pub fn label_named(&mut self, name: &str) -> Label {
        let l = self.label();
        // Fresh labels are always in range.
        self.label_names[l.0 as usize] = Some(name.into());
        l
    }

    /// Assigns a human-readable name to an existing label.
    pub fn name_label(&mut self, label: Label, name: &str) -> Result<(), UnresolvedLabel> {
        let slot = self
            .label_names
            .get_mut(label.0 as usize)
            .ok_or(UnresolvedLabel)?;
        *slot = Some(name.into());
        Ok(())
    }

    fn check_place(&self, label: Label) -> Result<(), UnresolvedLabel> {
        let slot = self.labels.get(label.0 as usize).ok_or(UnresolvedLabel)?;
        if self.block_params[label.0 as usize].is_some() && slot.is_some() {
            return Err(UnresolvedLabel);
        }
        Ok(())
    }

    /// Places `label` at the current `pc`.
    ///
    /// Fails for unknown labels or repeated placement of a block reserved with [`Self::block`].
    pub fn place(&mut self, label: Label) -> Result<(), UnresolvedLabel> {
        self.check_place(label)?;
        let pc = self.pc();
        let slot = self
            .labels
            .get_mut(label.0 as usize)
            .ok_or(UnresolvedLabel)?;
        *slot = Some(pc);
        if let Some(params) = self.block_params[label.0 as usize].take() {
            self.opcode(Opcode::Block);
            self.uleb(u32::try_from(params.len()).unwrap_or(u32::MAX));
            for &reg in &params {
                self.reg(reg);
            }
            // Retain the declaration so repeated placement cannot orphan its original header.
            self.block_params[label.0 as usize] = Some(params);
        }
        Ok(())
    }

    /// Assigns a name to `label`, then places it at the current `pc`.
    pub fn place_named(&mut self, label: Label, name: &str) -> Result<(), UnresolvedLabel> {
        self.check_place(label)?;
        self.name_label(label, name)?;
        self.place(label)
    }

    /// Records that subsequent instructions map to `span_id` (until overwritten).
    ///
    /// This emits a span-table entry at the current `pc`. Repeated calls at the same `pc` overwrite
    /// the previous entry (so the encoded `pc_delta` sequence remains valid).
    pub fn span(&mut self, span_id: SpanId) -> &mut Self {
        let pc = self.pc();
        if let Some((last_pc, last_span)) = self.span_marks.last_mut()
            && *last_pc == pc
        {
            *last_span = span_id;
            return self;
        }
        self.span_marks.push((pc, span_id));
        self
    }

    /// Finalizes and returns only the encoded instruction bytes.
    ///
    /// This discards register declarations, spans and labels. Use [`Self::finish_parts`] to retain
    /// the complete function body needed by [`Program`].
    pub fn finish(self) -> Result<Vec<u8>, UnresolvedLabel> {
        Ok(self.finish_parts()?.bytecode)
    }

    /// Finalizes and returns the complete assembled function body.
    pub fn finish_parts(mut self) -> Result<AsmParts, UnresolvedLabel> {
        // Apply fixups.
        for f in &self.fixups {
            let Some(target_pc) = self.labels.get(f.label.0 as usize).and_then(|x| *x) else {
                return Err(UnresolvedLabel);
            };
            match f.kind {
                FixupKind::PcUleb32 => patch_uleb128_u32(&mut self.bytes, f.at, target_pc),
            }
        }

        let mut spans: Vec<SpanEntry> = Vec::new();
        let mut prev_pc: u64 = 0;
        for &(pc, span_id) in &self.span_marks {
            let pc = u64::from(pc);
            let pc_delta = pc.saturating_sub(prev_pc);
            prev_pc = pc;
            spans.push(SpanEntry { pc_delta, span_id });
        }

        let mut labels_out: Vec<NamedLabel> = Vec::new();
        for (i, name) in self.label_names.into_iter().enumerate() {
            let Some(name) = name else {
                continue;
            };
            let Some(pc) = self.labels.get(i).and_then(|x| *x) else {
                return Err(UnresolvedLabel);
            };
            labels_out.push(NamedLabel { pc, name });
        }

        let (regions, region_runs) = self.regions.finish(&self.labels, self.bytes.len())?;
        Ok(AsmParts {
            register_types: self.register_types,
            bytecode: self.bytes,
            spans,
            labels: labels_out,
            regions,
            region_runs,
        })
    }

    /// Finalizes, then verifies the resulting body under `sig`, returning only instruction bytes.
    ///
    /// This discards the required declarations; prefer [`Self::finish_checked_parts`].
    ///
    /// This constructs a tiny single-function [`Program`] wrapper and runs the verifier. It is
    /// intended as a quick sanity check for builder users. Only kind references are accepted;
    /// program-local contract IDs require [`ProgramBuilder::push_function_checked`].
    pub fn finish_checked(self, abi: impl Into<FunctionAbi>) -> Result<Vec<u8>, AsmError> {
        self.finish_checked_with(abi, &VerifyConfig::default())
    }

    /// Finalizes, then verifies the resulting bytecode and span table under `sig`.
    ///
    /// Only kind references are accepted. Use [`ProgramBuilder::push_function_checked`] for
    /// signatures containing program-local contract IDs.
    pub fn finish_checked_parts(self, abi: impl Into<FunctionAbi>) -> Result<AsmParts, AsmError> {
        self.finish_checked_parts_with(abi, &VerifyConfig::default())
    }

    /// Finalizes, then verifies the resulting bytecode under `sig` with a custom verifier config.
    ///
    /// Only kind references are accepted; see [`Self::finish_checked`].
    pub fn finish_checked_with(
        self,
        abi: impl Into<FunctionAbi>,
        cfg: &VerifyConfig,
    ) -> Result<Vec<u8>, AsmError> {
        Ok(self.finish_checked_parts_with(abi, cfg)?.bytecode)
    }

    /// Finalizes, then verifies the resulting bytecode and span table under `sig` with a custom
    /// verifier config. Only kind references are accepted; see [`Self::finish_checked_parts`].
    pub fn finish_checked_parts_with(
        self,
        abi: impl Into<FunctionAbi>,
        cfg: &VerifyConfig,
    ) -> Result<AsmParts, AsmError> {
        let abi = abi.into();
        if let Some(id) = abi
            .args
            .iter()
            .chain(&abi.rets)
            .find_map(|reference| match reference {
                ContractRef::Id(id) => Some(*id),
                _ => None,
            })
        {
            return Err(AsmError::ProgramContract(id));
        }
        let parts = self.finish_parts()?;
        let mut builder = ProgramBuilder::new();
        let (args, rets) = builder.resolve_signature(abi)?;
        builder.functions.push(GuestFunctionDef {
            args,
            rets,
            register_types: parts.register_types.clone(),
            bytecode: parts.bytecode.clone(),
            spans: parts.spans.clone(),
            regions: parts.regions.clone(),
            region_runs: parts.region_runs.clone(),
        });
        let p = builder.build();
        verify_program(&p, cfg)?;
        Ok(parts)
    }

    /// Encodes `nop`.
    pub fn nop(&mut self) -> &mut Self {
        self.opcode(Opcode::Nop);
        self
    }

    /// Encodes `mov dst, src`.
    pub fn mov(&mut self, dst: u32, src: u32) -> &mut Self {
        self.opcode(Opcode::Mov);
        self.reg(dst);
        self.reg(src);
        self
    }

    /// Encodes `trap code`.
    pub fn trap(&mut self, code: u32) -> &mut Self {
        self.opcode(Opcode::Trap);
        self.uleb(code);
        self
    }

    /// Encodes `throw eff_in, tag, payload...`.
    ///
    /// Prefer [`ProgramBuilder::emit_throw`], which checks the payload against the tag's
    /// declaration before emitting.
    pub fn throw(&mut self, eff_in: u32, tag: TagId, payload: &[u32]) -> &mut Self {
        self.opcode(Opcode::Throw);
        self.reg(eff_in);
        self.uleb(tag.0);
        self.uleb(u32::try_from(payload.len()).unwrap_or(u32::MAX));
        for &r in payload {
            self.reg(r);
        }
        self
    }

    /// Encodes `throw_ref eff_in, exn`, rethrowing the exception referenced by `exn`.
    ///
    /// The exception keeps its original tag, payload and throw site. `exn` must be an initialized
    /// `Exn` register, as bound by a [`Catch::TagRef`] or [`Catch::AllRef`] handler.
    pub fn throw_ref(&mut self, eff_in: u32, exn: u32) -> &mut Self {
        self.opcode(Opcode::ThrowRef);
        self.reg(eff_in);
        self.reg(exn);
        self
    }

    /// Encodes `cont.new dst, call_sig, func`, creating an unstarted continuation.
    ///
    /// `func` holds a `Func` or `Closure`, checked against `call_sig` as [`Self::call_indirect`]
    /// checks its callee; a closure's environment is bound as its first argument. The
    /// continuation's type is `call_sig`: [`Self::resume`] supplies its arguments and receives
    /// its results.
    pub fn cont_new(&mut self, dst: u32, call_sig: CallSigId, func: u32) -> &mut Self {
        self.opcode(Opcode::ContNew);
        self.reg(dst);
        self.uleb(call_sig.0);
        self.reg(func);
        self
    }

    /// Encodes `cont.bind dst, from_sig, to_sig, cont, args...`.
    ///
    /// Consumes the continuation in `cont`, typed `from_sig`, and binds `args` to its leading
    /// inputs. `dst` receives the bound continuation, typed `to_sig`: the remaining inputs and
    /// the same results.
    pub fn cont_bind(
        &mut self,
        dst: u32,
        from_sig: CallSigId,
        to_sig: CallSigId,
        cont: u32,
        args: &[u32],
    ) -> &mut Self {
        self.opcode(Opcode::ContBind);
        self.reg(dst);
        self.uleb(from_sig.0);
        self.uleb(to_sig.0);
        self.reg(cont);
        self.reg_list(args);
        self
    }

    /// Encodes `cont.drop cont`.
    ///
    /// Consumes the continuation in `cont` without resuming it, releasing its frames and slot.
    /// Its activations never run again, so their protected regions do not either; raise into
    /// it with [`Self::resume_throw`] instead to unwind through them. Continuations its
    /// registers refer to are not released with it: drop each abandoned one.
    pub fn cont_drop(&mut self, cont: u32) -> &mut Self {
        self.opcode(Opcode::ContDrop);
        self.reg(cont);
        self
    }

    /// Encodes `resume eff_out, call_sig, cont, eff_in, args..., rets...`.
    ///
    /// Consumes the continuation in `cont`, whose type must be `call_sig`, and runs it on top of
    /// this activation with `args`. When it returns, `rets` receive its results. When it
    /// suspends to a tag, the `on` clauses of the regions protecting this instruction handle
    /// it ([`Catch::On`]); exceptions escaping it reach their catch clauses.
    pub fn resume(
        &mut self,
        eff_out: u32,
        call_sig: CallSigId,
        cont: u32,
        eff_in: u32,
        args: &[u32],
        rets: &[u32],
    ) -> &mut Self {
        self.opcode(Opcode::Resume);
        self.reg(eff_out);
        self.uleb(call_sig.0);
        self.reg(cont);
        self.reg(eff_in);
        self.reg_list(args);
        self.reg_list(rets);
        self
    }

    /// Encodes `resume_throw eff_out, call_sig, cont, eff_in, tag, payload..., rets...`.
    ///
    /// Like [`Self::resume`], but the continuation continues by raising `tag` with `payload` at
    /// its suspension point, where its own regions may catch it. An unstarted continuation is
    /// discarded and the exception raised here. `rets` receive its results if it handles the
    /// exception and returns.
    #[allow(
        clippy::too_many_arguments,
        reason = "one parameter per encoded operand, as for the other call-like instructions"
    )]
    pub fn resume_throw(
        &mut self,
        eff_out: u32,
        call_sig: CallSigId,
        cont: u32,
        eff_in: u32,
        tag: TagId,
        payload: &[u32],
        rets: &[u32],
    ) -> &mut Self {
        self.opcode(Opcode::ResumeThrow);
        self.reg(eff_out);
        self.uleb(call_sig.0);
        self.reg(cont);
        self.reg(eff_in);
        self.uleb(tag.0);
        self.reg_list(payload);
        self.reg_list(rets);
        self
    }

    /// Encodes `suspend eff_out, tag, eff_in, payload..., rets...`.
    ///
    /// Suspends the continuations up to the nearest `resume` with an `on` clause for `tag`,
    /// whose handler receives `payload` and the suspended continuation. Resuming that
    /// continuation continues here, with `rets` receiving the tag's results.
    pub fn suspend(
        &mut self,
        eff_out: u32,
        tag: TagId,
        eff_in: u32,
        payload: &[u32],
        rets: &[u32],
    ) -> &mut Self {
        self.opcode(Opcode::Suspend);
        self.reg(eff_out);
        self.uleb(tag.0);
        self.reg(eff_in);
        self.reg_list(payload);
        self.reg_list(rets);
        self
    }

    fn reg_list(&mut self, regs: &[u32]) {
        self.uleb(u32::try_from(regs.len()).unwrap_or(u32::MAX));
        for &reg in regs {
            self.reg(reg);
        }
    }

    /// Encodes `const_unit dst`.
    pub fn const_unit(&mut self, dst: u32) -> &mut Self {
        self.opcode(Opcode::ConstUnit);
        self.reg(dst);
        self
    }

    /// Encodes `const_bool dst, imm_u8`.
    pub fn const_bool(&mut self, dst: u32, imm: bool) -> &mut Self {
        self.opcode(Opcode::ConstBool);
        self.reg(dst);
        self.bytes.push(u8::from(imm));
        self
    }

    /// Encodes `const_i64 dst, imm_sleb`.
    pub fn const_i64(&mut self, dst: u32, imm: i64) -> &mut Self {
        self.opcode(Opcode::ConstI64);
        self.reg(dst);
        write_sleb128_i64(&mut self.bytes, imm);
        self
    }

    /// Encodes `const_u64 dst, imm_uleb`.
    pub fn const_u64(&mut self, dst: u32, imm: u64) -> &mut Self {
        self.opcode(Opcode::ConstU64);
        self.reg(dst);
        write_uleb128_u64(&mut self.bytes, imm);
        self
    }

    /// Encodes `const_f64 dst, bits_u64le`.
    pub fn const_f64_bits(&mut self, dst: u32, bits: u64) -> &mut Self {
        self.opcode(Opcode::ConstF64);
        self.reg(dst);
        self.bytes.extend_from_slice(&bits.to_le_bytes());
        self
    }

    /// Encodes `const_f64 dst, bits_u64le` from an `f64` value.
    pub fn const_f64(&mut self, dst: u32, v: f64) -> &mut Self {
        self.const_f64_bits(dst, v.to_bits())
    }

    /// Encodes `const_decimal dst, mantissa_sleb, scale_u8`.
    pub fn const_decimal(&mut self, dst: u32, mantissa: i128, scale: u8) -> &mut Self {
        self.opcode(Opcode::ConstDecimal);
        self.reg(dst);
        write_sleb128_i128(&mut self.bytes, mantissa);
        self.bytes.push(scale);
        self
    }

    /// Encodes `const_decimal ...` from a [`Decimal`].
    pub fn const_decimal_value(&mut self, dst: u32, v: Decimal) -> &mut Self {
        self.const_decimal(dst, v.mantissa(), v.scale())
    }

    /// Encodes `const_pool dst, idx`.
    pub fn const_pool(&mut self, dst: u32, idx: ConstId) -> &mut Self {
        self.opcode(Opcode::ConstPool);
        self.reg(dst);
        self.uleb(idx.0);
        self
    }

    /// Encodes `const.func dst, func_id`.
    pub fn const_func(&mut self, dst: u32, func_id: FuncId) -> &mut Self {
        self.opcode(Opcode::ConstFunc);
        self.reg(dst);
        self.uleb(func_id.0);
        self
    }

    /// Encodes `dec_add dst, a, b` (exact, at the larger of the operand scales).
    pub fn dec_add(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::DecAdd);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `dec_sub dst, a, b` (exact, at the larger of the operand scales).
    pub fn dec_sub(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::DecSub);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `dec_mul dst, a, b`.
    pub fn dec_mul(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::DecMul);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `dec_div dst, a, b, scale, rounding`: `a / b` at the `u64` scale held in register
    /// `scale`, rounded once from the exact quotient (traps on a zero divisor, a scale above 255
    /// or overflow). A scaled numerator beyond 128 bits costs one more fuel per long-division
    /// bit step, up to 256.
    pub fn dec_div(
        &mut self,
        dst: u32,
        a: u32,
        b: u32,
        scale: u32,
        rounding: DecimalRounding,
    ) -> &mut Self {
        self.opcode(Opcode::DecDiv);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self.reg(scale);
        self.bytes.push(rounding.byte());
        self
    }

    /// Encodes `dec_rescale dst, a, scale, rounding`: `a` at the `u64` scale held in register
    /// `scale`, exact when the scale grows and rounded when it shrinks (traps on a scale above 255
    /// or overflow).
    pub fn dec_rescale(
        &mut self,
        dst: u32,
        a: u32,
        scale: u32,
        rounding: DecimalRounding,
    ) -> &mut Self {
        self.opcode(Opcode::DecRescale);
        self.reg(dst);
        self.reg(a);
        self.reg(scale);
        self.bytes.push(rounding.byte());
        self
    }

    /// Encodes `dec_scale dst, a` (the scale of `a` as a `u64`).
    pub fn dec_scale(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::DecScale);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `dec_cmp dst, a, b` (`-1`, `0` or `1` as an `i64`, comparing numeric values
    /// regardless of scale).
    pub fn dec_cmp(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::DecCmp);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_add dst, a, b`.
    pub fn f64_add(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Add);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_sub dst, a, b`.
    pub fn f64_sub(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Sub);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_mul dst, a, b`.
    pub fn f64_mul(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Mul);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_div dst, a, b`.
    pub fn f64_div(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Div);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_neg dst, a`.
    pub fn f64_neg(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64Neg);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64_abs dst, a`.
    pub fn f64_abs(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64Abs);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64_min dst, a, b` (NaN-propagating).
    pub fn f64_min(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Min);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_max dst, a, b` (NaN-propagating).
    pub fn f64_max(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Max);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_min_num dst, a, b` (number-favoring).
    pub fn f64_min_num(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64MinNum);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_max_num dst, a, b` (number-favoring).
    pub fn f64_max_num(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64MaxNum);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_rem dst, a, b`.
    pub fn f64_rem(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Rem);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_to_bits dst, a`.
    pub fn f64_to_bits(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64ToBits);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64_from_bits dst, a`.
    pub fn f64_from_bits(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64FromBits);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64_floor dst, a` (round toward negative infinity).
    pub fn f64_floor(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64Floor);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64_ceil dst, a` (round toward positive infinity).
    pub fn f64_ceil(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64Ceil);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64_trunc dst, a` (round toward zero).
    pub fn f64_trunc(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64Trunc);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64_nearest dst, a` (round to nearest integer, ties to even).
    pub fn f64_nearest(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64Nearest);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64_sqrt dst, a` (correctly rounded).
    pub fn f64_sqrt(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64Sqrt);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64.round_half_away dst, a` (round to nearest integer, ties away from zero; see
    /// [`math::round_half_away_f64`](crate::math::round_half_away_f64)).
    pub fn f64_round_half_away(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64RoundHalfAway);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64_copysign dst, a, b` (magnitude of `a`, sign bit of `b`).
    pub fn f64_copysign(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64CopySign);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_add dst, a, b`.
    pub fn i64_add(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Add);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_sub dst, a, b`.
    pub fn i64_sub(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Sub);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_mul dst, a, b`.
    pub fn i64_mul(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Mul);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_add_ovf dst, overflow, a, b`: the wrapped sum in `dst` (`i64`) and whether
    /// the signed result overflowed in `overflow` (`bool`).
    pub fn i64_add_ovf(&mut self, dst: u32, overflow: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64AddOvf);
        self.reg(dst);
        self.reg(overflow);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_sub_ovf dst, overflow, a, b`: the wrapped difference in `dst` (`i64`) and whether
    /// the signed result overflowed in `overflow` (`bool`).
    pub fn i64_sub_ovf(&mut self, dst: u32, overflow: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64SubOvf);
        self.reg(dst);
        self.reg(overflow);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_mul_ovf dst, overflow, a, b`: the wrapped product in `dst` (`i64`) and whether
    /// the signed result overflowed in `overflow` (`bool`).
    pub fn i64_mul_ovf(&mut self, dst: u32, overflow: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64MulOvf);
        self.reg(dst);
        self.reg(overflow);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_add dst, a, b`.
    pub fn u64_add(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Add);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_sub dst, a, b`.
    pub fn u64_sub(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Sub);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_mul dst, a, b`.
    pub fn u64_mul(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Mul);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_add_ovf dst, overflow, a, b`: the wrapped sum in `dst` (`u64`) and whether
    /// the sum carried out in `overflow` (`bool`).
    pub fn u64_add_ovf(&mut self, dst: u32, overflow: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64AddOvf);
        self.reg(dst);
        self.reg(overflow);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_sub_ovf dst, overflow, a, b`: the wrapped difference in `dst` (`u64`) and whether
    /// the difference borrowed in `overflow` (`bool`).
    pub fn u64_sub_ovf(&mut self, dst: u32, overflow: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64SubOvf);
        self.reg(dst);
        self.reg(overflow);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_mul_ovf dst, overflow, a, b`: the wrapped product in `dst` (`u64`) and whether
    /// the product overflowed in `overflow` (`bool`).
    pub fn u64_mul_ovf(&mut self, dst: u32, overflow: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64MulOvf);
        self.reg(dst);
        self.reg(overflow);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_and dst, a, b`.
    pub fn u64_and(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64And);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_or dst, a, b`.
    pub fn u64_or(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Or);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_eq dst, a, b`.
    pub fn i64_eq(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Eq);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_lt dst, a, b`.
    pub fn i64_lt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Lt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_eq dst, a, b`.
    pub fn u64_eq(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Eq);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_lt dst, a, b`.
    pub fn u64_lt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Lt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_gt dst, a, b`.
    pub fn u64_gt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Gt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_xor dst, a, b`.
    pub fn u64_xor(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Xor);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_shl dst, a, b` (shift amount masked with `& 63`).
    pub fn u64_shl(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Shl);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_clz dst, a` (64 when `a == 0`).
    pub fn u64_clz(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U64Clz);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u64_ctz dst, a` (64 when `a == 0`).
    pub fn u64_ctz(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U64Ctz);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u64_popcnt dst, a`.
    pub fn u64_popcnt(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U64Popcnt);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u64_rotl dst, a, b` (rotation amount masked with `& 63`).
    pub fn u64_rotl(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Rotl);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_rotr dst, a, b` (rotation amount masked with `& 63`).
    pub fn u64_rotr(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Rotr);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `const.i32 dst, imm_sleb`.
    pub fn const_i32(&mut self, dst: u32, imm: i32) -> &mut Self {
        self.opcode(Opcode::ConstI32);
        self.reg(dst);
        write_sleb128_i64(&mut self.bytes, i64::from(imm));
        self
    }

    /// Encodes `const.u32 dst, imm_uleb`.
    pub fn const_u32(&mut self, dst: u32, imm: u32) -> &mut Self {
        self.opcode(Opcode::ConstU32);
        self.reg(dst);
        write_uleb128_u64(&mut self.bytes, u64::from(imm));
        self
    }

    /// Encodes `const.f32 dst, bits_u32le` from raw IEEE bits.
    pub fn const_f32_bits(&mut self, dst: u32, bits: u32) -> &mut Self {
        self.opcode(Opcode::ConstF32);
        self.reg(dst);
        self.bytes.extend_from_slice(&bits.to_le_bytes());
        self
    }

    /// Encodes `const.f32 dst, bits_u32le` from an `f32` value.
    pub fn const_f32(&mut self, dst: u32, v: f32) -> &mut Self {
        self.const_f32_bits(dst, v.to_bits())
    }

    /// Encodes `i32.add dst, a, b`: `dst = a + b` (`i32`, wrapping).
    pub fn i32_add(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32Add);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.sub dst, a, b`: `dst = a - b` (`i32`, wrapping).
    pub fn i32_sub(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32Sub);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.mul dst, a, b`: `dst = a * b` (`i32`, wrapping).
    pub fn i32_mul(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32Mul);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.div dst, a, b`: `dst = a / b` (`i32`, truncating; traps on zero divisor or `MIN / -1`).
    pub fn i32_div(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32Div);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.rem dst, a, b`: `dst = a % b` (`i32`, sign of `a`; traps on zero divisor or `MIN % -1`).
    pub fn i32_rem(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32Rem);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.add_ovf dst, overflow, a, b`: `(dst, overflow) = a + b` (`i32` wrapped sum; `bool` true if the signed sum overflowed).
    pub fn i32_add_ovf(&mut self, dst: u32, overflow: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32AddOvf);
        self.reg(dst);
        self.reg(overflow);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.sub_ovf dst, overflow, a, b`: `(dst, overflow) = a - b` (`i32` wrapped difference; `bool` true on signed overflow).
    pub fn i32_sub_ovf(&mut self, dst: u32, overflow: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32SubOvf);
        self.reg(dst);
        self.reg(overflow);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.mul_ovf dst, overflow, a, b`: `(dst, overflow) = a * b` (`i32` wrapped product; `bool` true on signed overflow).
    pub fn i32_mul_ovf(&mut self, dst: u32, overflow: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32MulOvf);
        self.reg(dst);
        self.reg(overflow);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.and dst, a, b`: `dst = a & b` (`i32`).
    pub fn i32_and(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32And);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.or dst, a, b`: `dst = a | b` (`i32`).
    pub fn i32_or(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32Or);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.xor dst, a, b`: `dst = a ^ b` (`i32`).
    pub fn i32_xor(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32Xor);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.shl dst, a, b`: `dst = a << (b & 31)` (`i32`).
    pub fn i32_shl(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32Shl);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.shr dst, a, b`: `dst = a >> (b & 31)` (`i32`, arithmetic).
    pub fn i32_shr(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32Shr);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.eq dst, a, b`: `dst = (a == b)` (`i32` -> `bool`).
    pub fn i32_eq(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32Eq);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.lt dst, a, b`: `dst = (a < b)` (`i32` -> `bool`).
    pub fn i32_lt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32Lt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.gt dst, a, b`: `dst = (a > b)` (`i32` -> `bool`).
    pub fn i32_gt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32Gt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.le dst, a, b`: `dst = (a <= b)` (`i32` -> `bool`).
    pub fn i32_le(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32Le);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i32.ge dst, a, b`: `dst = (a >= b)` (`i32` -> `bool`).
    pub fn i32_ge(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I32Ge);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.add dst, a, b`: `dst = a + b` (`u32`, wrapping).
    pub fn u32_add(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Add);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.sub dst, a, b`: `dst = a - b` (`u32`, wrapping).
    pub fn u32_sub(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Sub);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.mul dst, a, b`: `dst = a * b` (`u32`, wrapping).
    pub fn u32_mul(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Mul);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.div dst, a, b`: `dst = a / b` (`u32`; traps on zero divisor).
    pub fn u32_div(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Div);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.rem dst, a, b`: `dst = a % b` (`u32`; traps on zero divisor).
    pub fn u32_rem(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Rem);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.add_ovf dst, overflow, a, b`: `(dst, overflow) = a + b` (`u32` wrapped sum; `bool` carry).
    pub fn u32_add_ovf(&mut self, dst: u32, overflow: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32AddOvf);
        self.reg(dst);
        self.reg(overflow);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.sub_ovf dst, overflow, a, b`: `(dst, overflow) = a - b` (`u32` wrapped difference; `bool` borrow).
    pub fn u32_sub_ovf(&mut self, dst: u32, overflow: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32SubOvf);
        self.reg(dst);
        self.reg(overflow);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.mul_ovf dst, overflow, a, b`: `(dst, overflow) = a * b` (`u32` wrapped product; `bool` true on overflow).
    pub fn u32_mul_ovf(&mut self, dst: u32, overflow: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32MulOvf);
        self.reg(dst);
        self.reg(overflow);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.and dst, a, b`: `dst = a & b` (`u32`).
    pub fn u32_and(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32And);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.or dst, a, b`: `dst = a | b` (`u32`).
    pub fn u32_or(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Or);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.xor dst, a, b`: `dst = a ^ b` (`u32`).
    pub fn u32_xor(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Xor);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.shl dst, a, b`: `dst = a << (b & 31)` (`u32`).
    pub fn u32_shl(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Shl);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.shr dst, a, b`: `dst = a >> (b & 31)` (`u32`, logical).
    pub fn u32_shr(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Shr);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.clz dst, a`: `dst = a.leading_zeros()` (`u32`; 32 when `a == 0`).
    pub fn u32_clz(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U32Clz);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u32.ctz dst, a`: `dst = a.trailing_zeros()` (`u32`; 32 when `a == 0`).
    pub fn u32_ctz(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U32Ctz);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u32.popcnt dst, a`: `dst = a.count_ones()` (`u32`).
    pub fn u32_popcnt(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U32Popcnt);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u32.rotl dst, a, b`: `dst = a.rotate_left(b & 31)` (`u32`).
    pub fn u32_rotl(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Rotl);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.rotr dst, a, b`: `dst = a.rotate_right(b & 31)` (`u32`).
    pub fn u32_rotr(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Rotr);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.eq dst, a, b`: `dst = (a == b)` (`u32` -> `bool`).
    pub fn u32_eq(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Eq);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.lt dst, a, b`: `dst = (a < b)` (`u32` -> `bool`).
    pub fn u32_lt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Lt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.gt dst, a, b`: `dst = (a > b)` (`u32` -> `bool`).
    pub fn u32_gt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Gt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.le dst, a, b`: `dst = (a <= b)` (`u32` -> `bool`).
    pub fn u32_le(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Le);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u32.ge dst, a, b`: `dst = (a >= b)` (`u32` -> `bool`).
    pub fn u32_ge(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U32Ge);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.add dst, a, b`: `dst = a + b` (`f32`).
    pub fn f32_add(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32Add);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.sub dst, a, b`: `dst = a - b` (`f32`).
    pub fn f32_sub(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32Sub);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.mul dst, a, b`: `dst = a * b` (`f32`).
    pub fn f32_mul(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32Mul);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.div dst, a, b`: `dst = a / b` (`f32`).
    pub fn f32_div(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32Div);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.rem dst, a, b`: `dst = a % b` (`f32`, sign of `a`).
    pub fn f32_rem(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32Rem);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.neg dst, a`: `dst = -a` (`f32`, flips the sign bit).
    pub fn f32_neg(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32Neg);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.abs dst, a`: `dst = abs(a)` (`f32`, clears the sign bit).
    pub fn f32_abs(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32Abs);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.min dst, a, b`: `dst = min(a, b)` (`f32`, NaN-propagating).
    pub fn f32_min(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32Min);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.max dst, a, b`: `dst = max(a, b)` (`f32`, NaN-propagating).
    pub fn f32_max(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32Max);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.min_num dst, a, b`: `dst = min_num(a, b)` (`f32`, number-favoring).
    pub fn f32_min_num(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32MinNum);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.max_num dst, a, b`: `dst = max_num(a, b)` (`f32`, number-favoring).
    pub fn f32_max_num(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32MaxNum);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.copysign dst, a, b`: `dst = copysign(a, b)` (`f32`: magnitude of `a`, sign of `b`).
    pub fn f32_copysign(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32CopySign);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.floor dst, a`: `dst = floor(a)` (`f32`, rounds toward negative infinity).
    pub fn f32_floor(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32Floor);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.ceil dst, a`: `dst = ceil(a)` (`f32`, rounds toward positive infinity).
    pub fn f32_ceil(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32Ceil);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.trunc dst, a`: `dst = trunc(a)` (`f32`, rounds toward zero).
    pub fn f32_trunc(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32Trunc);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.nearest dst, a`: `dst = nearest(a)` (`f32`, rounds to nearest integer, ties to even).
    pub fn f32_nearest(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32Nearest);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.sqrt dst, a`: `dst = sqrt(a)` (`f32`, correctly rounded).
    pub fn f32_sqrt(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32Sqrt);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.eq dst, a, b`: `dst = (a == b)` (`f32` -> `bool`; false if NaN).
    pub fn f32_eq(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32Eq);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.ne dst, a, b`: `dst = (a != b)` (`f32` -> `bool`; true if NaN).
    pub fn f32_ne(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32Ne);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.lt dst, a, b`: `dst = (a < b)` (`f32` -> `bool`; false if NaN).
    pub fn f32_lt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32Lt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.gt dst, a, b`: `dst = (a > b)` (`f32` -> `bool`; false if NaN).
    pub fn f32_gt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32Gt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.le dst, a, b`: `dst = (a <= b)` (`f32` -> `bool`; false if NaN).
    pub fn f32_le(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32Le);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.ge dst, a, b`: `dst = (a >= b)` (`f32` -> `bool`; false if NaN).
    pub fn f32_ge(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32Ge);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.round_half_away dst, a`: `dst = a` rounded to the nearest integer, ties away from zero (see [`math::round_half_away_f32`](crate::math::round_half_away_f32)).
    pub fn f32_round_half_away(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32RoundHalfAway);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.clamp dst, a, lo, hi`: `dst = a` limited to `lo..=hi`; traps with
    /// `ClampBoundsInvalid` unless `lo <= hi` (see [`math::clamp_f32`](crate::math::clamp_f32)).
    pub fn f32_clamp(&mut self, dst: u32, a: u32, lo: u32, hi: u32) -> &mut Self {
        self.opcode(Opcode::F32Clamp);
        self.reg(dst);
        self.reg(a);
        self.reg(lo);
        self.reg(hi);
        self
    }

    /// Encodes `f32.pow dst, a, b`: `dst = a` raised to `b` (see
    /// [`math::pow_f32`](crate::math::pow_f32)).
    pub fn f32_pow(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F32Pow);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f32.exp dst, a`: `dst = e^a` (see [`math::exp_f32`](crate::math::exp_f32)).
    pub fn f32_exp(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32Exp);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.ln dst, a`: `dst` = the natural logarithm of `a` (see [`math::ln_f32`](crate::math::ln_f32)).
    pub fn f32_ln(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32Ln);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.sin dst, a`: `dst = sin(a)`, `a` in radians (see [`math::sin_f32`](crate::math::sin_f32)).
    pub fn f32_sin(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32Sin);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.cos dst, a`: `dst = cos(a)`, `a` in radians (see [`math::cos_f32`](crate::math::cos_f32)).
    pub fn f32_cos(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32Cos);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.atan2 dst, y, x`: `dst` = the angle of the point `(x, y)` in radians (see
    /// [`math::atan2_f32`](crate::math::atan2_f32)). Note the operand order: `y` first.
    pub fn f32_atan2(&mut self, dst: u32, y: u32, x: u32) -> &mut Self {
        self.opcode(Opcode::F32Atan2);
        self.reg(dst);
        self.reg(y);
        self.reg(x);
        self
    }

    /// Encodes `i32.to_i64 dst, a`: `dst = i32_to_i64(a)` (`i32` -> `i64`; sign-extends).
    pub fn i32_to_i64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::I32ToI64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `i64.to_i32 dst, a`: `dst = i64_to_i32(a)` (`i64` -> `i32`; traps when out of range).
    pub fn i64_to_i32(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::I64ToI32);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `i64.to_i32_wrap dst, a`: `dst = i64_to_i32_wrap(a)` (`i64` -> `i32`; keeps the low 32 bits).
    pub fn i64_to_i32_wrap(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::I64ToI32Wrap);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u32.to_u64 dst, a`: `dst = u32_to_u64(a)` (`u32` -> `u64`; zero-extends).
    pub fn u32_to_u64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U32ToU64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u64.to_u32 dst, a`: `dst = u64_to_u32(a)` (`u64` -> `u32`; traps when out of range).
    pub fn u64_to_u32(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U64ToU32);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u64.to_u32_wrap dst, a`: `dst = u64_to_u32_wrap(a)` (`u64` -> `u32`; keeps the low 32 bits).
    pub fn u64_to_u32_wrap(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U64ToU32Wrap);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u32.to_i64 dst, a`: `dst = u32_to_i64(a)` (`u32` -> `i64`; zero-extends).
    pub fn u32_to_i64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U32ToI64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `i32.to_u32 dst, a`: `dst = i32_to_u32(a)` (`i32` -> `u32`; traps on negative).
    pub fn i32_to_u32(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::I32ToU32);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u32.to_i32 dst, a`: `dst = u32_to_i32(a)` (`u32` -> `i32`; traps above `i32::MAX`).
    pub fn u32_to_i32(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U32ToI32);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `i32.to_u32_wrap dst, a`: `dst = i32_to_u32_wrap(a)` (`i32` -> `u32`; reinterprets the bits).
    pub fn i32_to_u32_wrap(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::I32ToU32Wrap);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u32.to_i32_wrap dst, a`: `dst = u32_to_i32_wrap(a)` (`u32` -> `i32`; reinterprets the bits).
    pub fn u32_to_i32_wrap(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U32ToI32Wrap);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `i64.to_u64_wrap dst, a`: `dst = i64_to_u64_wrap(a)` (`i64` -> `u64`; reinterprets the bits).
    pub fn i64_to_u64_wrap(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::I64ToU64Wrap);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u64.to_i64_wrap dst, a`: `dst = u64_to_i64_wrap(a)` (`u64` -> `i64`; reinterprets the bits).
    pub fn u64_to_i64_wrap(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U64ToI64Wrap);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.to_f64 dst, a`: `dst = f32_to_f64(a)` (`f32` -> `f64`; exact).
    pub fn f32_to_f64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32ToF64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64.to_f32 dst, a`: `dst = f64_to_f32(a)` (`f64` -> `f32`; rounds to nearest, ties to even; overflows to infinity).
    pub fn f64_to_f32(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64ToF32);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.to_bits dst, a`: `dst = f32_to_bits(a)` (`f32` -> `u32`; raw IEEE bits).
    pub fn f32_to_bits(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32ToBits);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.from_bits dst, a`: `dst = f32_from_bits(a)` (`u32` -> `f32`; raw IEEE bits).
    pub fn f32_from_bits(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32FromBits);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.to_i32 dst, a`: `dst = f32_to_i32(a)` (`f32` -> `i32`; truncates; traps on NaN, infinities and out of range).
    pub fn f32_to_i32(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32ToI32);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.to_u32 dst, a`: `dst = f32_to_u32(a)` (`f32` -> `u32`; truncates; traps on NaN, infinities and out of range).
    pub fn f32_to_u32(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32ToU32);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.to_i32_sat dst, a`: `dst = f32_to_i32_sat(a)` (`f32` -> `i32`; truncates and saturates; NaN -> 0).
    pub fn f32_to_i32_sat(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32ToI32Sat);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f32.to_u32_sat dst, a`: `dst = f32_to_u32_sat(a)` (`f32` -> `u32`; truncates and saturates; NaN -> 0).
    pub fn f32_to_u32_sat(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F32ToU32Sat);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64.to_i32 dst, a`: `dst = f64_to_i32(a)` (`f64` -> `i32`; truncates; traps on NaN, infinities and out of range).
    pub fn f64_to_i32(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64ToI32);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64.to_u32 dst, a`: `dst = f64_to_u32(a)` (`f64` -> `u32`; truncates; traps on NaN, infinities and out of range).
    pub fn f64_to_u32(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64ToU32);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64.to_i32_sat dst, a`: `dst = f64_to_i32_sat(a)` (`f64` -> `i32`; truncates and saturates; NaN -> 0).
    pub fn f64_to_i32_sat(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64ToI32Sat);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64.to_u32_sat dst, a`: `dst = f64_to_u32_sat(a)` (`f64` -> `u32`; truncates and saturates; NaN -> 0).
    pub fn f64_to_u32_sat(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64ToU32Sat);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `i32.to_f32 dst, a`: `dst = i32_to_f32(a)` (`i32` -> `f32`; rounds to nearest, ties to even).
    pub fn i32_to_f32(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::I32ToF32);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u32.to_f32 dst, a`: `dst = u32_to_f32(a)` (`u32` -> `f32`; rounds to nearest, ties to even).
    pub fn u32_to_f32(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U32ToF32);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `i64.to_f32 dst, a`: `dst = i64_to_f32(a)` (`i64` -> `f32`; rounds once to nearest, ties to even).
    pub fn i64_to_f32(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::I64ToF32);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u64.to_f32 dst, a`: `dst = u64_to_f32(a)` (`u64` -> `f32`; rounds once to nearest, ties to even).
    pub fn u64_to_f32(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U64ToF32);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `i32.to_f64 dst, a`: `dst = i32_to_f64(a)` (`i32` -> `f64`; exact).
    pub fn i32_to_f64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::I32ToF64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u32.to_f64 dst, a`: `dst = u32_to_f64(a)` (`u32` -> `f64`; exact).
    pub fn u32_to_f64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U32ToF64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u64_shr dst, a, b` (shift amount masked with `& 63`).
    pub fn u64_shr(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Shr);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `bool_not dst, a`.
    pub fn bool_not(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::BoolNot);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `bool_and dst, a, b`.
    pub fn bool_and(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::BoolAnd);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `bool_or dst, a, b`.
    pub fn bool_or(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::BoolOr);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `bool_xor dst, a, b`.
    pub fn bool_xor(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::BoolXor);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_eq dst, a, b`.
    pub fn f64_eq(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Eq);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_lt dst, a, b`.
    pub fn f64_lt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Lt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_gt dst, a, b`.
    pub fn f64_gt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Gt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_le dst, a, b`.
    pub fn f64_le(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Le);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_ge dst, a, b`.
    pub fn f64_ge(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Ge);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `f64_ne dst, a, b` (true if either input is NaN).
    pub fn f64_ne(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Ne);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_and dst, a, b`.
    pub fn i64_and(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64And);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_le dst, a, b`.
    pub fn u64_le(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Le);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_ge dst, a, b`.
    pub fn u64_ge(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Ge);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_or dst, a, b`.
    pub fn i64_or(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Or);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_xor dst, a, b`.
    pub fn i64_xor(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Xor);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_to_i64 dst, a` (traps on overflow).
    pub fn u64_to_i64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U64ToI64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `i64_to_u64 dst, a` (traps on negative).
    pub fn i64_to_u64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::I64ToU64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `select dst, cond, a, b`.
    pub fn select(&mut self, dst: u32, cond: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::Select);
        self.reg(dst);
        self.reg(cond);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_gt dst, a, b`.
    pub fn i64_gt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Gt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_le dst, a, b`.
    pub fn i64_le(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Le);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_ge dst, a, b`.
    pub fn i64_ge(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Ge);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_shl dst, a, b` (shift amount masked with `& 63`).
    pub fn i64_shl(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Shl);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_shr dst, a, b` (shift amount masked with `& 63`).
    pub fn i64_shr(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Shr);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Branches to a label or a label with arguments from [`Label::with_args`].
    ///
    /// Only the selected edge transfers its arguments. All transfers read the pre-branch values,
    /// including cycles and fan-out. Both edges are checked during verification.
    pub fn br<'a>(
        &mut self,
        cond: u32,
        pc_true: impl Into<BlockTarget<'a>>,
        pc_false: impl Into<BlockTarget<'a>>,
    ) -> &mut Self {
        let t = pc_true.into();
        let f = pc_false.into();
        let has_args = !t.args.is_empty() || !f.args.is_empty();
        self.opcode(if has_args { Opcode::BrArgs } else { Opcode::Br });
        self.reg(cond);
        self.pc_label(t.label);
        self.pc_label(f.label);
        if has_args {
            self.uleb(u32::try_from(t.args.len()).unwrap_or(u32::MAX));
            self.uleb(u32::try_from(t.args.len().saturating_add(f.args.len())).unwrap_or(u32::MAX));
            for &arg in t.args.iter().chain(f.args) {
                self.reg(arg);
            }
        }
        self
    }

    /// Jumps to a label or a label with simultaneous arguments from [`Label::with_args`].
    pub fn jmp<'a>(&mut self, pc_target: impl Into<BlockTarget<'a>>) -> &mut Self {
        let target = pc_target.into();
        self.opcode(if target.args.is_empty() {
            Opcode::Jmp
        } else {
            Opcode::JmpArgs
        });
        self.pc_label(target.label);
        if !target.args.is_empty() {
            self.uleb(u32::try_from(target.args.len()).unwrap_or(u32::MAX));
            for &arg in target.args {
                self.reg(arg);
            }
        }
        self
    }

    /// Encodes `call eff_out, func_id, eff_in, argc, args..., retc, rets...`.
    pub fn call(
        &mut self,
        eff_out: u32,
        func_id: FuncId,
        eff_in: u32,
        args: &[u32],
        rets: &[u32],
    ) -> &mut Self {
        self.opcode(Opcode::Call);
        self.reg(eff_out);
        self.uleb(func_id.0);
        self.reg(eff_in);
        self.uleb(u32::try_from(args.len()).unwrap_or(u32::MAX));
        for &a in args {
            self.reg(a);
        }
        self.uleb(u32::try_from(rets.len()).unwrap_or(u32::MAX));
        for &r in rets {
            self.reg(r);
        }
        self
    }

    /// Encodes `call.indirect eff_out, call_sig, callee, eff_in, argc, args..., retc, rets...`.
    ///
    /// `call_sig` describes the caller-visible arguments. If `callee` holds a `Closure`, the VM injects
    /// its captured environment as the callee's first argument (which must be an `Agg`), so the callee's
    /// declared signature is `[Agg, <call_sig args>...]`. If `callee` holds a `Func`, nothing is injected
    /// and `call_sig` matches the callee signature 1:1. Return types match 1:1 in both cases. Use
    /// [`FunctionSig::closure_body`] to build the declared signature for a closure body.
    pub fn call_indirect(
        &mut self,
        eff_out: u32,
        call_sig: CallSigId,
        callee: u32,
        eff_in: u32,
        args: &[u32],
        rets: &[u32],
    ) -> &mut Self {
        self.opcode(Opcode::CallIndirect);
        self.reg(eff_out);
        self.uleb(call_sig.0);
        self.reg(callee);
        self.reg(eff_in);
        self.uleb(u32::try_from(args.len()).unwrap_or(u32::MAX));
        for &a in args {
            self.reg(a);
        }
        self.uleb(u32::try_from(rets.len()).unwrap_or(u32::MAX));
        for &r in rets {
            self.reg(r);
        }
        self
    }

    /// Encodes `ret eff_in, retc, rets...`.
    pub fn ret(&mut self, eff_in: u32, rets: &[u32]) -> &mut Self {
        self.opcode(Opcode::Ret);
        self.reg(eff_in);
        self.uleb(u32::try_from(rets.len()).unwrap_or(u32::MAX));
        for &r in rets {
            self.reg(r);
        }
        self
    }

    /// Encodes `host_call eff_out, symbol_id, sig_hash_u64le, eff_in, argc, args..., retc, rets...`.
    pub fn host_call(
        &mut self,
        eff_out: u32,
        host_sig: HostSigId,
        eff_in: u32,
        args: &[u32],
        rets: &[u32],
    ) -> &mut Self {
        self.opcode(Opcode::HostCall);
        self.reg(eff_out);
        self.uleb(host_sig.0);
        self.reg(eff_in);
        self.uleb(u32::try_from(args.len()).unwrap_or(u32::MAX));
        for &a in args {
            self.reg(a);
        }
        self.uleb(u32::try_from(rets.len()).unwrap_or(u32::MAX));
        for &r in rets {
            self.reg(r);
        }
        self
    }

    /// Encodes `tuple_new dst, arity, values...`.
    pub fn tuple_new(&mut self, dst: u32, values: &[u32]) -> &mut Self {
        self.opcode(Opcode::TupleNew);
        self.reg(dst);
        self.uleb(u32::try_from(values.len()).unwrap_or(u32::MAX));
        for &v in values {
            self.reg(v);
        }
        self
    }

    /// Encodes `tuple_get dst, tuple, index`.
    pub fn tuple_get(&mut self, dst: u32, tuple: u32, index: u32) -> &mut Self {
        self.opcode(Opcode::TupleGet);
        self.reg(dst);
        self.reg(tuple);
        self.uleb(index);
        self
    }

    /// Encodes `struct_new dst, type_id, field_count, values...`.
    pub fn struct_new(&mut self, dst: u32, type_id: TypeId, values: &[u32]) -> &mut Self {
        self.opcode(Opcode::StructNew);
        self.reg(dst);
        self.uleb(type_id.0);
        self.uleb(u32::try_from(values.len()).unwrap_or(u32::MAX));
        for &v in values {
            self.reg(v);
        }
        self
    }

    /// Encodes `struct_get dst, st, field_index`.
    pub fn struct_get(&mut self, dst: u32, st: u32, field_index: u32) -> &mut Self {
        self.opcode(Opcode::StructGet);
        self.reg(dst);
        self.reg(st);
        self.uleb(field_index);
        self
    }

    /// Encodes `array_new dst, elem_type_id, len, values...`.
    pub fn array_new(&mut self, dst: u32, elem_type_id: ElemTypeId, values: &[u32]) -> &mut Self {
        self.opcode(Opcode::ArrayNew);
        self.reg(dst);
        self.uleb(elem_type_id.0);
        self.uleb(u32::try_from(values.len()).unwrap_or(u32::MAX));
        for &v in values {
            self.reg(v);
        }
        self
    }

    /// Encodes `array_len dst, arr`.
    pub fn array_len(&mut self, dst: u32, arr: u32) -> &mut Self {
        self.opcode(Opcode::ArrayLen);
        self.reg(dst);
        self.reg(arr);
        self
    }

    /// Encodes `array_get dst, arr, index_reg`.
    pub fn array_get(&mut self, dst: u32, arr: u32, index_reg: u32) -> &mut Self {
        self.opcode(Opcode::ArrayGet);
        self.reg(dst);
        self.reg(arr);
        self.reg(index_reg);
        self
    }

    /// Encodes `tuple_len dst, tuple`.
    pub fn tuple_len(&mut self, dst: u32, tuple: u32) -> &mut Self {
        self.opcode(Opcode::TupleLen);
        self.reg(dst);
        self.reg(tuple);
        self
    }

    /// Encodes `struct_field_count dst, st`.
    pub fn struct_field_count(&mut self, dst: u32, st: u32) -> &mut Self {
        self.opcode(Opcode::StructFieldCount);
        self.reg(dst);
        self.reg(st);
        self
    }

    /// Encodes `array_get_imm dst, arr, index_uleb`.
    pub fn array_get_imm(&mut self, dst: u32, arr: u32, index: u32) -> &mut Self {
        self.opcode(Opcode::ArrayGetImm);
        self.reg(dst);
        self.reg(arr);
        self.uleb(index);
        self
    }

    /// Encodes `bytes_len dst, bytes`.
    pub fn bytes_len(&mut self, dst: u32, bytes: u32) -> &mut Self {
        self.opcode(Opcode::BytesLen);
        self.reg(dst);
        self.reg(bytes);
        self
    }

    /// Encodes `closure.new dst, func, env`.
    ///
    /// Pairs the function reference in `func` with the captured environment aggregate in `env`. When the
    /// resulting closure is invoked via `call.indirect`, `env` is passed as the callee's first argument
    /// (an `Agg`); see [`Asm::call_indirect`].
    pub fn closure_new(&mut self, dst: u32, func: u32, env: u32) -> &mut Self {
        self.opcode(Opcode::ClosureNew);
        self.reg(dst);
        self.reg(func);
        self.reg(env);
        self
    }

    /// Encodes `str_len dst, s` (length in UTF-8 bytes).
    pub fn str_len(&mut self, dst: u32, s: u32) -> &mut Self {
        self.opcode(Opcode::StrLen);
        self.reg(dst);
        self.reg(s);
        self
    }

    /// Encodes `i64_div dst, a, b` (traps on divide-by-zero and `i64::MIN / -1`).
    pub fn i64_div(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Div);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_rem dst, a, b` (traps on divide-by-zero and `i64::MIN % -1`).
    pub fn i64_rem(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Rem);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_div dst, a, b` (traps on divide-by-zero).
    pub fn u64_div(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Div);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `u64_rem dst, a, b` (traps on divide-by-zero).
    pub fn u64_rem(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Rem);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `i64_to_f64 dst, a`.
    pub fn i64_to_f64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::I64ToF64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `u64_to_f64 dst, a`.
    pub fn u64_to_f64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U64ToF64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64_to_i64 dst, a` (truncates, traps on NaN/inf/out-of-range).
    pub fn f64_to_i64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64ToI64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64_to_u64 dst, a` (truncates, traps on NaN/inf/out-of-range; `-0.5` converts to `0`).
    pub fn f64_to_u64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64ToU64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64_to_i64_sat dst, a` (truncates and saturates; NaN converts to `0`, never traps).
    pub fn f64_to_i64_sat(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64ToI64Sat);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `f64_to_u64_sat dst, a` (truncates and saturates; NaN converts to `0`, never traps).
    pub fn f64_to_u64_sat(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64ToU64Sat);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `dec_to_i64 dst, a` (traps if `a.scale != 0` or outside `i64`).
    pub fn dec_to_i64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::DecToI64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `dec_to_u64 dst, a` (traps if `a.scale != 0` or outside `u64`).
    pub fn dec_to_u64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::DecToU64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `i64_to_dec dst, a, scale_u8` (traps on overflow).
    pub fn i64_to_dec(&mut self, dst: u32, a: u32, scale: u8) -> &mut Self {
        self.opcode(Opcode::I64ToDec);
        self.reg(dst);
        self.reg(a);
        self.bytes.push(scale);
        self
    }

    /// Encodes `u64_to_dec dst, a, scale_u8` (traps on overflow).
    pub fn u64_to_dec(&mut self, dst: u32, a: u32, scale: u8) -> &mut Self {
        self.opcode(Opcode::U64ToDec);
        self.reg(dst);
        self.reg(a);
        self.bytes.push(scale);
        self
    }

    /// Encodes `bytes_eq dst, a, b`.
    pub fn bytes_eq(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::BytesEq);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `str_eq dst, a, b`.
    pub fn str_eq(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::StrEq);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `bytes_concat dst, a, b`.
    pub fn bytes_concat(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::BytesConcat);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `str_concat dst, a, b`.
    pub fn str_concat(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::StrConcat);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `bytes_get dst, bytes, index_reg` (returns `u64` in `0..=255`, traps on OOB).
    pub fn bytes_get(&mut self, dst: u32, bytes: u32, index_reg: u32) -> &mut Self {
        self.opcode(Opcode::BytesGet);
        self.reg(dst);
        self.reg(bytes);
        self.reg(index_reg);
        self
    }

    /// Encodes `bytes_get_imm dst, bytes, index_uleb` (returns `u64` in `0..=255`, traps on OOB).
    pub fn bytes_get_imm(&mut self, dst: u32, bytes: u32, index: u32) -> &mut Self {
        self.opcode(Opcode::BytesGetImm);
        self.reg(dst);
        self.reg(bytes);
        self.uleb(index);
        self
    }

    /// Encodes `bytes_slice dst, bytes, start, end` (traps on invalid range).
    pub fn bytes_slice(&mut self, dst: u32, bytes: u32, start: u32, end: u32) -> &mut Self {
        self.opcode(Opcode::BytesSlice);
        self.reg(dst);
        self.reg(bytes);
        self.reg(start);
        self.reg(end);
        self
    }

    /// Encodes `str_slice dst, s, start, end` (byte indices; traps on invalid range/non-boundary).
    pub fn str_slice(&mut self, dst: u32, s: u32, start: u32, end: u32) -> &mut Self {
        self.opcode(Opcode::StrSlice);
        self.reg(dst);
        self.reg(s);
        self.reg(start);
        self.reg(end);
        self
    }

    /// Encodes `str_to_bytes dst, s`.
    pub fn str_to_bytes(&mut self, dst: u32, s: u32) -> &mut Self {
        self.opcode(Opcode::StrToBytes);
        self.reg(dst);
        self.reg(s);
        self
    }

    /// Encodes `bytes_to_str dst, bytes` (traps on invalid UTF-8).
    pub fn bytes_to_str(&mut self, dst: u32, bytes: u32) -> &mut Self {
        self.opcode(Opcode::BytesToStr);
        self.reg(dst);
        self.reg(bytes);
        self
    }

    /// Encodes a `host_call` for a known [`HostSigId`].
    pub fn host_call_sig_id(
        &mut self,
        eff_out: u32,
        host_sig: HostSigId,
        eff_in: u32,
        args: &[u32],
        rets: &[u32],
    ) -> &mut Self {
        self.host_call(eff_out, host_sig, eff_in, args, rets)
    }

    fn uleb(&mut self, v: u32) {
        write_uleb128_u64(&mut self.bytes, u64::from(v));
    }

    fn opcode(&mut self, opcode: Opcode) {
        self.sources.instruction(self.pc());
        opcode.encode(&mut self.bytes);
    }

    fn reg(&mut self, r: u32) {
        self.uleb(r);
    }

    fn pc_label(&mut self, label: Label) {
        // Reserve space for uleb128(u32). We'll patch later.
        let at = self.bytes.len();
        self.bytes
            .extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x00]); // max 5 bytes
        self.fixups.push(Fixup {
            at,
            label,
            kind: FixupKind::PcUleb32,
        });
    }
}

fn patch_uleb128_u32(bytes: &mut [u8], at: usize, value: u32) {
    // Patch into the reserved 5-byte window using a fixed-width ULEB128 encoding.
    //
    // This is intentionally not canonical/minimal: it keeps instruction byte offsets stable so we
    // can patch branch targets without shifting the byte stream.
    let b0 = (value & 0x7F) as u8 | 0x80;
    let b1 = ((value >> 7) & 0x7F) as u8 | 0x80;
    let b2 = ((value >> 14) & 0x7F) as u8 | 0x80;
    let b3 = ((value >> 21) & 0x7F) as u8 | 0x80;
    let b4 = ((value >> 28) & 0x7F) as u8;
    bytes[at] = b0;
    bytes[at + 1] = b1;
    bytes[at + 2] = b2;
    bytes[at + 3] = b3;
    bytes[at + 4] = b4;
}

#[cfg(test)]
mod tests {
    #[test]
    fn declared_body_owns_argument_kinds_and_allows_further_builder_work() {
        let mut builder = ProgramBuilder::new();
        let integer = builder.contract(Contract::Kind(ValueType::I64)).unwrap();
        let tuple = builder.contract(Contract::Tuple(&[Some(integer)])).unwrap();
        let function = builder
            .declare_function(FunctionAbi {
                args: vec![tuple.into()],
                rets: vec![integer.into()],
            })
            .unwrap();
        let mut body = builder.function_asm(function).unwrap();
        assert_eq!(body.register_types, [ValueType::Unit, ValueType::Agg]);
        let result = body.alloc_register(ValueType::I64);
        assert_eq!(result, 2);
        let text = body.alloc_register(ValueType::Str);
        let literal = builder.const_str("still mutable");
        body.const_pool(text, literal)
            .tuple_get(result, 1, 0)
            .ret(0, &[result]);
        builder.define_function(function, body).unwrap();
        builder.build_verified().unwrap();
    }

    #[test]
    fn allocated_temporary_remains_uninitialized_until_written() {
        let mut builder = ProgramBuilder::new();
        let function = builder
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let mut body = builder.function_asm(function).unwrap();
        let result = body.alloc_register(ValueType::I64);
        body.ret(0, &[result]);
        builder.define_function(function, body).unwrap();
        assert!(matches!(
            builder.build_verified(),
            Err(BuildError::Verify(VerifyError::UninitializedRead {
                reg: 1,
                ..
            }))
        ));
    }

    #[test]
    fn declared_body_rejects_missing_function_without_mutation() {
        let builder = ProgramBuilder::new();
        assert!(matches!(
            builder.function_asm(FuncId(7)),
            Err(BuildError::BadFuncId { func: 7 })
        ));
        assert!(builder.functions.is_empty());
    }

    #[test]
    fn pre_numbered_ledger_can_append_typed_temporaries() {
        let mut body = Asm::new();
        body.set_register_types(vec![ValueType::Unit, ValueType::Bool, ValueType::I64]);
        assert_eq!(body.alloc_register(ValueType::Str), 3);
        assert_eq!(
            body.finish_parts().unwrap().register_types,
            [
                ValueType::Unit,
                ValueType::Bool,
                ValueType::I64,
                ValueType::Str
            ]
        );
    }

    #[test]
    fn streaming_allocation_reserves_effect_without_a_signature() {
        let mut body = Asm::new();
        let result = body.alloc_register(ValueType::I64);
        assert_eq!(result, 1);
        body.const_i64(result, 42).ret(0, &[result]);
        let mut builder = ProgramBuilder::new();
        builder
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        builder.build_verified().unwrap();
    }

    #[test]
    fn streaming_allocation_can_declare_arguments_before_temporaries() {
        let mut body = Asm::new();
        let argument = body.alloc_register(ValueType::I64);
        let one = body.alloc_register(ValueType::I64);
        let result = body.alloc_register(ValueType::I64);
        body.const_i64(one, 1)
            .i64_add(result, argument, one)
            .ret(0, &[result]);
        let mut builder = ProgramBuilder::new();
        builder
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        builder.build_verified().unwrap();
    }

    #[test]
    fn duplicate_declared_block_placement_preserves_name_and_bytes() {
        let mut asm = Asm::new();
        let block = asm.block(&[]);
        asm.place_named(block, "original").unwrap();
        let bytes = asm.bytes.clone();
        assert!(asm.place_named(block, "replacement").is_err());
        assert_eq!(asm.bytes, bytes);
        assert_eq!(
            asm.label_names[block.0 as usize].as_deref(),
            Some("original")
        );
    }

    use super::*;
    use crate::bytecode::{Instr, decode_instructions};
    use crate::program::StructTypeDef;
    use alloc::vec;

    #[test]
    fn asm_resolves_labels() {
        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::Bool, ValueType::I64]);
        let l_then = a.label();
        let l_else = a.label();

        a.const_bool(1, true);
        a.br(1, l_then, l_else);
        a.place(l_then).unwrap();
        a.const_i64(2, 1);
        a.ret(0, &[2]);
        a.place(l_else).unwrap();
        a.const_i64(2, 2);
        a.ret(0, &[2]);

        let bytes = a.finish().unwrap();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn asm_finish_checked_accepts_simple_program() {
        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::I64]);
        a.const_i64(1, 7);
        a.ret(0, &[1]);
        let bytes = a
            .finish_checked(&FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn asm_finish_checked_rejects_uninitialized_read() {
        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::Unit]);
        let l0 = a.label();
        a.place(l0).unwrap();
        // br r1, l0, l0  (r1 is uninitialized under arg_count=0)
        a.br(1, l0, l0);
        let err = a
            .finish_checked(&FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            })
            .unwrap_err();
        assert!(matches!(
            err,
            AsmError::Verify(VerifyError::UninitializedRead { .. })
        ));
    }

    #[test]
    fn program_builder_can_declare_then_define_and_verify_cross_calls() {
        let mut pb = ProgramBuilder::new();
        let foo = pb
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let bar = pb
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();

        // Define `foo` first; it calls `bar` by its declared `FuncId`.
        let mut a_foo = Asm::new();
        a_foo.set_register_types(vec![ValueType::Unit, ValueType::I64]);
        a_foo.call(0, bar, 0, &[], &[1]);
        a_foo.ret(0, &[1]);
        pb.define_function(foo, a_foo).unwrap();

        let mut a_bar = Asm::new();
        a_bar.set_register_types(vec![ValueType::Unit, ValueType::I64]);
        a_bar.const_i64(1, 7);
        a_bar.ret(0, &[1]);
        pb.define_function(bar, a_bar).unwrap();

        let p = pb.build_checked().unwrap();
        assert_eq!(p.functions.len(), 2);
    }

    #[test]
    fn program_builder_preserves_declared_unused_registers() {
        let mut pb = ProgramBuilder::new();
        let func = pb
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            })
            .unwrap();
        let mut a = Asm::new();
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::Unit,
            ValueType::Unit,
            ValueType::Unit,
            ValueType::Unit,
            ValueType::Unit,
            ValueType::Unit,
            ValueType::I64,
        ]);
        a.const_i64(7, 1);
        a.ret(0, &[]);
        pb.define_function(func, a).unwrap();

        let p = pb.build_checked().unwrap();
        assert_eq!(p.functions[func.0 as usize].reg_count, 8);
    }

    #[test]
    fn program_builder_preserves_declared_argument_registers() {
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::Bool]);
        a.ret(0, &[]);
        let func = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![ValueType::I64, ValueType::Bool],
                    ret_types: vec![],
                },
            )
            .unwrap();

        let p = pb.build_checked().unwrap();
        assert_eq!(p.functions[func.0 as usize].reg_count, 3);
    }

    #[test]
    fn program_builder_interns_ids() {
        let mut pb = ProgramBuilder::new();

        let s0 = pb.symbol("x");
        let s1 = pb.symbol("x");
        assert_eq!(s0, s1);

        let c0 = pb.constant(Const::I64(7));
        let c1 = pb.constant(Const::I64(7));
        assert_eq!(c0, c1);

        let t0 = pb.struct_type(StructTypeDef {
            field_names: vec!["a".into()],
            field_types: vec![ValueType::I64],
        });
        let t1 = pb.struct_type(StructTypeDef {
            field_names: vec!["a".into()],
            field_types: vec![ValueType::I64],
        });
        assert_eq!(t0, t1);

        let e0 = pb.array_elem(ValueType::Bool);
        let e1 = pb.array_elem(ValueType::Bool);
        assert_eq!(e0, e1);
    }

    #[test]
    fn program_builder_const_literal_helpers_intern_constants() {
        let mut pb = ProgramBuilder::new();

        let text = pb.const_str("hello");
        let text_again = pb.constant(Const::Str("hello".into()));
        assert_eq!(text, text_again);

        let bytes = pb.const_bytes(&[1, 2, 3]);
        let bytes_again = pb.constant(Const::Bytes(vec![1, 2, 3]));
        assert_eq!(bytes, bytes_again);

        let p = pb.build();
        assert_eq!(p.const_str(text).unwrap(), "hello");
        assert_eq!(p.const_bytes(bytes).unwrap(), &[1, 2, 3]);
    }

    #[test]
    fn program_builder_call_sig_interns_and_builds_program_table() {
        let mut pb = ProgramBuilder::new();
        let s0 = pb
            .call_sig(FunctionSig {
                arg_types: vec![ValueType::I64, ValueType::Bool],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let s1 = pb
            .call_sig(FunctionSig {
                arg_types: vec![ValueType::I64, ValueType::Bool],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let s2 = pb
            .call_sig(FunctionSig {
                arg_types: vec![ValueType::Bool],
                ret_types: vec![],
            })
            .unwrap();
        assert_eq!(s0, s1);
        assert_ne!(s0, s2);

        let p = pb.build();
        let cs0 = p.call_sig(s0).unwrap();
        let cs1 = p.call_sig(s2).unwrap();
        assert_eq!(
            p.call_sig_args(cs0).unwrap(),
            &[ValueType::I64, ValueType::Bool]
        );
        assert_eq!(p.call_sig_rets(cs0).unwrap(), &[ValueType::I64]);
        assert_eq!(p.call_sig_args(cs1).unwrap(), &[ValueType::Bool]);
        assert_eq!(p.call_sig_rets(cs1).unwrap(), &[]);
    }

    #[test]
    fn asm_encodes_closure_new_and_call_indirect() {
        let mut a = Asm::new();
        a.closure_new(3, 1, 2);
        a.call_indirect(0, CallSigId(7), 3, 0, &[4, 5], &[6]);

        let bytes = a.finish().unwrap();
        let decoded = decode_instructions(&bytes).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(
            decoded[0].instr,
            Instr::ClosureNew {
                dst: 3,
                func: 1,
                env: 2
            }
        );
        assert_eq!(
            decoded[1].instr,
            Instr::CallIndirect {
                eff_out: 0,
                call_sig: 7,
                callee: 3,
                eff_in: 0,
                args: vec![4, 5],
                rets: vec![6],
            }
        );
    }

    #[test]
    fn function_sig_closure_body_prepends_env_argument() {
        let sig = FunctionSig::closure_body(
            &[ValueType::I64, ValueType::Bool],
            &[ValueType::Obj(crate::program::HostTypeId(3))],
        );

        assert_eq!(
            sig,
            FunctionSig {
                arg_types: vec![ValueType::Agg, ValueType::I64, ValueType::Bool],
                ret_types: vec![ValueType::Obj(crate::program::HostTypeId(3))],
            }
        );
    }

    #[test]
    fn function_abi_keeps_compositional_signature_positions() {
        let mut builder = ProgramBuilder::new();
        let integer = builder.contract(Contract::Kind(ValueType::I64)).unwrap();
        let env = builder.contract(Contract::Tuple(&[Some(integer)])).unwrap();
        let abi = FunctionAbi {
            args: vec![env.into(), ValueType::I64.into()],
            rets: vec![ValueType::I64.into()],
        };
        assert_eq!(abi.args[0], ContractRef::Id(env));
        assert_eq!(abi.args[1], ContractRef::Kind(ValueType::I64));
        assert_eq!(abi.rets, vec![ContractRef::Kind(ValueType::I64)]);
    }

    #[test]
    fn program_builder_preserves_compositional_argument_contracts() {
        let mut pb = ProgramBuilder::new();
        let integer = pb.contract(Contract::Kind(ValueType::I64)).unwrap();
        let tuple = pb.contract(Contract::Tuple(&[Some(integer)])).unwrap();
        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::Agg, ValueType::I64]);
        a.tuple_get(2, 1, 0).ret(0, &[2]);
        let func = pb
            .push_function_checked(
                a,
                FunctionAbi {
                    args: vec![tuple.into()],
                    rets: vec![ValueType::I64.into()],
                },
            )
            .unwrap();
        let program = pb.build_checked().unwrap();
        assert_eq!(
            program.functions[func.0 as usize]
                .arg_contracts(&program)
                .unwrap(),
            &[tuple]
        );
        assert_eq!(
            program.contracts.get(tuple),
            Some(Contract::Tuple(&[Some(integer)]))
        );
    }

    #[test]
    fn program_builder_build_verified_preserves_call_sigs_for_call_indirect() {
        let mut pb = ProgramBuilder::new();
        let caller = pb
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let callee = pb
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();

        let call_sig = pb
            .call_sig(FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();

        let mut callee_asm = Asm::new();
        callee_asm.set_register_types(vec![ValueType::Unit, ValueType::I64]);
        callee_asm.ret(0, &[1]);
        pb.define_function(callee, callee_asm).unwrap();

        let mut caller_asm = Asm::new();
        caller_asm.set_register_types(vec![
            ValueType::Unit,
            ValueType::Func,
            ValueType::I64,
            ValueType::I64,
        ]);
        caller_asm.const_func(1, callee);
        caller_asm.const_i64(2, 7);
        caller_asm.call_indirect(0, call_sig, 1, 0, &[2], &[3]);
        caller_asm.ret(0, &[3]);
        pb.define_function(caller, caller_asm).unwrap();

        let vp = pb.build_verified().unwrap();
        let program = vp.program();
        let entry = program.call_sig(call_sig).unwrap();
        assert_eq!(program.call_sig_args(entry).unwrap(), &[ValueType::I64]);
        assert_eq!(program.call_sig_rets(entry).unwrap(), &[ValueType::I64]);
    }

    #[test]
    fn program_builder_roundtrips_with_functions_host_sigs_and_call_sigs() {
        // Regression: builder packs the value-type arena as [host, function, call]
        // sigs; `Program::decode` must repack in the same order, or this roundtrip
        // fails on `value_types` and the offsets that index into it.
        let mut pb = ProgramBuilder::new();

        pb.host_sig_for(
            "host.do_thing",
            HostSig {
                args: vec![ValueType::I64, ValueType::Bool],
                rets: vec![ValueType::U64],
            },
        );

        let call_sig = pb
            .call_sig(FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();

        let caller = pb
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let callee = pb
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();

        let mut callee_asm = Asm::new();
        callee_asm.set_register_types(vec![ValueType::Unit, ValueType::I64]);
        callee_asm.ret(0, &[1]);
        pb.define_function(callee, callee_asm).unwrap();

        let mut caller_asm = Asm::new();
        caller_asm.set_register_types(vec![
            ValueType::Unit,
            ValueType::Func,
            ValueType::I64,
            ValueType::I64,
        ]);
        caller_asm.const_func(1, callee);
        caller_asm.const_i64(2, 7);
        caller_asm.call_indirect(0, call_sig, 1, 0, &[2], &[3]);
        caller_asm.ret(0, &[3]);
        pb.define_function(caller, caller_asm).unwrap();

        let program = pb.build();

        // All three groups must be present, or the test wouldn't exercise the order.
        assert!(!program.host_sigs.is_empty());
        assert!(!program.call_sigs.is_empty());
        assert!(!program.functions.is_empty());

        let bytes = program.encode();
        let back = Program::decode(&bytes).unwrap();
        assert_eq!(back, program);
    }

    #[test]
    fn asm_span_overwrites_at_same_pc() {
        let mut a = Asm::new();
        a.span(SpanId::try_from(1).unwrap());
        a.span(SpanId::try_from(2).unwrap());
        a.const_unit(0);
        a.span(SpanId::try_from(3).unwrap());
        a.trap(0);

        let parts = a.finish_parts().unwrap();
        assert_eq!(
            parts.spans,
            vec![
                SpanEntry {
                    pc_delta: 0,
                    span_id: SpanId::try_from(2).unwrap()
                },
                SpanEntry {
                    pc_delta: 2,
                    span_id: SpanId::try_from(3).unwrap()
                }
            ]
        );
    }
}
