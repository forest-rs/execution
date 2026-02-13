// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Bytecode builder ("assembler") for `execution_tape`.
//!
//! This is a small, public helper for constructing bytecode streams without manually computing
//! byte offsets or varint encodings.
//!
//! The encoding matches the "Draft encoding for minimal implemented opcodes" section in
//! `docs/v1_spec.md`.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::format::{write_sleb128_i64, write_uleb128_u64};
use crate::host::HostSig;
use crate::opcode::Opcode;
use crate::program::{
    Const, ConstId, ElemTypeId, FunctionDef, FunctionNameEntry, HostSigDef, HostSigId, HostSymbol,
    LabelNameEntry, Program, SpanEntry, StructTypeDef, SymbolId, TypeId, TypeTableDef, ValueType,
};
use crate::value::Decimal;
use crate::value::FuncId;
use crate::verifier::{
    VerifiedProgram, VerifyConfig, VerifyError, verify_program, verify_program_owned,
};

#[cfg(doc)]
use crate::vm::Vm;

/// A label for control-flow targets.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Label(u32);

/// A label that has not been placed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnresolvedLabel;

impl fmt::Display for UnresolvedLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "label was referenced but never placed")
    }
}

impl core::error::Error for UnresolvedLabel {}

/// A bytecode builder error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AsmError {
    /// A label was referenced but never placed.
    UnresolvedLabel,
    /// The produced bytecode failed verification under the provided function signature.
    Verify(VerifyError),
}

impl fmt::Display for AsmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnresolvedLabel => write!(f, "unresolved label"),
            Self::Verify(e) => write!(f, "verification failed: {e}"),
        }
    }
}

impl core::error::Error for AsmError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Verify(e) => Some(e),
            _ => None,
        }
    }
}

/// Final bytecode + span table produced by [`Asm`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsmParts {
    /// Final bytecode stream.
    pub bytecode: Vec<u8>,
    /// Span table entries, encoded as `(pc_delta, span_id)`.
    pub spans: Vec<SpanEntry>,
    /// Optional named labels placed within the function.
    pub labels: Vec<NamedLabel>,
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

impl From<VerifyError> for AsmError {
    fn from(e: VerifyError) -> Self {
        Self::Verify(e)
    }
}

/// A [`ProgramBuilder`] error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildError {
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
            _ => None,
        }
    }
}

impl From<VerifyError> for BuildError {
    fn from(e: VerifyError) -> Self {
        Self::Verify(e)
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

/// Convenience builder for constructing small [`Program`]s.
///
/// This is primarily intended for tests and prototypes. For production usage you may want a more
/// structured frontend that emits bytecode with spans and stable ids.
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
/// use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
/// use execution_tape::program::ValueType;
///
/// let mut a = Asm::new();
/// a.const_i64(1, 1);
/// a.const_i64(2, 2);
/// a.i64_add(3, 1, 2);
/// a.ret(0, &[3]);
///
/// let mut pb = ProgramBuilder::new();
/// pb.push_function_checked(
///     a,
///     FunctionSig {
///         arg_types: vec![],
///         ret_types: vec![ValueType::I64],
///     },
/// )?;
/// let _program = pb.build_verified()?;
/// # Ok::<(), execution_tape::asm::BuildError>(())
/// ```
#[derive(Clone, Debug, Default)]
pub struct ProgramBuilder {
    symbols: Vec<HostSymbol>,
    const_pool: Vec<Const>,
    host_sigs: Vec<HostSigDef>,
    types: TypeTableDef,
    functions: Vec<FunctionDef>,
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
        if (arg as usize) >= def.arg_types.len() {
            let arg_count = u32::try_from(def.arg_types.len()).unwrap_or(u32::MAX);
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
        if (ret as usize) >= def.ret_types.len() {
            let ret_count = u32::try_from(def.ret_types.len()).unwrap_or(u32::MAX);
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

    /// Declares a function signature and returns its [`FuncId`].
    ///
    /// This is useful when assembling mutually recursive or out-of-order functions: you can
    /// declare all functions up front, then reference them by [`FuncId`] in [`Asm::call`].
    pub fn declare_function(&mut self, sig: FunctionSig) -> FuncId {
        let reg_count = min_reg_count_for_arg_count(sig.arg_types.len());
        let id = FuncId(u32::try_from(self.functions.len()).unwrap_or(u32::MAX));
        self.functions.push(FunctionDef {
            arg_types: sig.arg_types,
            ret_types: sig.ret_types,
            reg_count,
            bytecode: Vec::new(),
            spans: Vec::new(),
        });
        id
    }

    /// Defines the body of a previously declared function.
    pub fn define_function(&mut self, func: FuncId, a: Asm) -> Result<(), BuildError> {
        let inferred_reg_count = a.inferred_reg_count();
        let parts = a.finish_parts()?;
        let Some(slot) = self.functions.get_mut(func.0 as usize) else {
            return Err(BuildError::BadFuncId { func: func.0 });
        };
        let reg_count = inferred_reg_count_for_arg_count(inferred_reg_count, slot.arg_types.len());
        slot.reg_count = reg_count;
        slot.bytecode = parts.bytecode;
        slot.spans = parts.spans;
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
        let inferred_reg_count = a.inferred_reg_count();
        let parts = a.finish_parts()?;
        let Some(slot) = self.functions.get_mut(func.0 as usize) else {
            return Err(BuildError::BadFuncId { func: func.0 });
        };
        let reg_count = inferred_reg_count_for_arg_count(inferred_reg_count, slot.arg_types.len());
        slot.reg_count = reg_count;
        slot.bytecode = parts.bytecode;
        slot.spans = spans;
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
    pub fn push_function_checked(&mut self, a: Asm, sig: FunctionSig) -> Result<FuncId, AsmError> {
        let reg_count = a.inferred_reg_count_for_args(sig.arg_types.len());
        let parts = a.finish_parts()?;
        let id = FuncId(u32::try_from(self.functions.len()).unwrap_or(u32::MAX));
        self.functions.push(FunctionDef {
            arg_types: sig.arg_types,
            ret_types: sig.ret_types,
            reg_count,
            bytecode: parts.bytecode,
            spans: parts.spans,
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
    pub fn build(self) -> Program {
        let mut p = Program::new(
            self.symbols,
            self.const_pool,
            self.host_sigs,
            self.types,
            self.functions,
        );
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
                f.arg_name_ids = crate::program::ByteRange { offset, len };
            }
            if has_ret_names[i] {
                let offset = u32::try_from(p.value_name_ids.len()).unwrap_or(u32::MAX);
                let len = f.ret_count;
                p.value_name_ids
                    .resize(p.value_name_ids.len() + len as usize, 0);
                f.ret_name_ids = crate::program::ByteRange { offset, len };
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
    max_reg_plus_one: u32,
    next_label: u32,
    labels: Vec<Option<u32>>,
    label_names: Vec<Option<String>>,
    fixups: Vec<Fixup>,
    span_marks: Vec<(u32, u64)>,
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

    fn inferred_reg_count_for_args(&self, arg_count: usize) -> u32 {
        inferred_reg_count_for_arg_count(self.inferred_reg_count(), arg_count)
    }

    fn inferred_reg_count(&self) -> u32 {
        self.max_reg_plus_one.max(1)
    }

    /// Allocates a new label.
    #[must_use]
    pub fn label(&mut self) -> Label {
        let id = self.next_label;
        self.next_label = self.next_label.wrapping_add(1);
        self.labels.push(None);
        self.label_names.push(None);
        Label(id)
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

    /// Places `label` at the current `pc`.
    pub fn place(&mut self, label: Label) -> Result<(), UnresolvedLabel> {
        let pc = self.pc();
        let slot = self
            .labels
            .get_mut(label.0 as usize)
            .ok_or(UnresolvedLabel)?;
        *slot = Some(pc);
        Ok(())
    }

    /// Assigns a name to `label`, then places it at the current `pc`.
    pub fn place_named(&mut self, label: Label, name: &str) -> Result<(), UnresolvedLabel> {
        self.name_label(label, name)?;
        self.place(label)
    }

    /// Records that subsequent instructions map to `span_id` (until overwritten).
    ///
    /// This emits a span-table entry at the current `pc`. Repeated calls at the same `pc` overwrite
    /// the previous entry (so the encoded `pc_delta` sequence remains valid).
    pub fn span(&mut self, span_id: u64) -> &mut Self {
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

    /// Finalizes and returns the encoded bytecode.
    pub fn finish(self) -> Result<Vec<u8>, UnresolvedLabel> {
        Ok(self.finish_parts()?.bytecode)
    }

    /// Finalizes and returns the encoded bytecode and span table.
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

        Ok(AsmParts {
            bytecode: self.bytes,
            spans,
            labels: labels_out,
        })
    }

    /// Finalizes, then verifies the resulting bytecode under `sig`.
    ///
    /// This constructs a tiny single-function [`Program`] wrapper and runs the verifier. It is
    /// intended as a quick sanity check for builder users.
    pub fn finish_checked(self, sig: &FunctionSig) -> Result<Vec<u8>, AsmError> {
        self.finish_checked_with(sig, &VerifyConfig::default())
    }

    /// Finalizes, then verifies the resulting bytecode and span table under `sig`.
    pub fn finish_checked_parts(self, sig: &FunctionSig) -> Result<AsmParts, AsmError> {
        self.finish_checked_parts_with(sig, &VerifyConfig::default())
    }

    /// Finalizes, then verifies the resulting bytecode under `sig` with a custom verifier config.
    pub fn finish_checked_with(
        self,
        sig: &FunctionSig,
        cfg: &VerifyConfig,
    ) -> Result<Vec<u8>, AsmError> {
        Ok(self.finish_checked_parts_with(sig, cfg)?.bytecode)
    }

    /// Finalizes, then verifies the resulting bytecode and span table under `sig` with a custom
    /// verifier config.
    pub fn finish_checked_parts_with(
        self,
        sig: &FunctionSig,
        cfg: &VerifyConfig,
    ) -> Result<AsmParts, AsmError> {
        let reg_count = self.inferred_reg_count_for_args(sig.arg_types.len());
        let parts = self.finish_parts()?;
        let p = Program::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: sig.arg_types.clone(),
                ret_types: sig.ret_types.clone(),
                reg_count,
                bytecode: parts.bytecode.clone(),
                spans: parts.spans.clone(),
            }],
        );
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
    pub fn const_decimal(&mut self, dst: u32, mantissa: i64, scale: u8) -> &mut Self {
        self.opcode(Opcode::ConstDecimal);
        self.reg(dst);
        write_sleb128_i64(&mut self.bytes, mantissa);
        self.bytes.push(scale);
        self
    }

    /// Encodes `const_decimal ...` from a [`Decimal`].
    pub fn const_decimal_value(&mut self, dst: u32, v: Decimal) -> &mut Self {
        self.const_decimal(dst, v.mantissa, v.scale)
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

    /// Encodes `dec_add dst, a, b`.
    pub fn dec_add(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::DecAdd);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// Encodes `dec_sub dst, a, b`.
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

    /// Encodes `br cond, pc_true, pc_false`.
    pub fn br(&mut self, cond: u32, pc_true: Label, pc_false: Label) -> &mut Self {
        self.opcode(Opcode::Br);
        self.reg(cond);
        self.pc_label(pc_true);
        self.pc_label(pc_false);
        self
    }

    /// Encodes `jmp pc_target`.
    pub fn jmp(&mut self, pc_target: Label) -> &mut Self {
        self.opcode(Opcode::Jmp);
        self.pc_label(pc_target);
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

    /// Encodes `f64_to_u64 dst, a` (truncates, traps on NaN/inf/out-of-range/negative).
    pub fn f64_to_u64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64ToU64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `dec_to_i64 dst, a` (traps if `a.scale != 0`).
    pub fn dec_to_i64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::DecToI64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// Encodes `dec_to_u64 dst, a` (traps if `a.scale != 0` or `a.mantissa < 0`).
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
        self.bytes.push(opcode as u8);
    }

    fn reg(&mut self, r: u32) {
        self.max_reg_plus_one = self.max_reg_plus_one.max(r.saturating_add(1));
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

fn min_reg_count_for_arg_count(arg_count: usize) -> u32 {
    u32::try_from(arg_count)
        .unwrap_or(u32::MAX)
        .saturating_add(1)
}

fn inferred_reg_count_for_arg_count(inferred_reg_count: u32, arg_count: usize) -> u32 {
    inferred_reg_count.max(min_reg_count_for_arg_count(arg_count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::StructTypeDef;
    use alloc::vec;

    #[test]
    fn asm_resolves_labels() {
        let mut a = Asm::new();
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
        let foo = pb.declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        });
        let bar = pb.declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        });

        // Define `foo` first; it calls `bar` by its declared `FuncId`.
        let mut a_foo = Asm::new();
        a_foo.call(0, bar, 0, &[], &[1]);
        a_foo.ret(0, &[1]);
        pb.define_function(foo, a_foo).unwrap();

        let mut a_bar = Asm::new();
        a_bar.const_i64(1, 7);
        a_bar.ret(0, &[1]);
        pb.define_function(bar, a_bar).unwrap();

        let p = pb.build_checked().unwrap();
        assert_eq!(p.functions.len(), 2);
    }

    #[test]
    fn program_builder_infers_reg_count_from_max_used_register() {
        let mut pb = ProgramBuilder::new();
        let func = pb.declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![],
        });
        let mut a = Asm::new();
        a.const_i64(7, 1);
        a.ret(0, &[]);
        pb.define_function(func, a).unwrap();

        let p = pb.build_checked().unwrap();
        assert_eq!(p.functions[func.0 as usize].reg_count, 8);
    }

    #[test]
    fn program_builder_infers_reg_count_floor_from_argument_count() {
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
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
    fn asm_span_overwrites_at_same_pc() {
        let mut a = Asm::new();
        a.span(1);
        a.span(2);
        a.const_unit(0);
        a.span(3);
        a.trap(0);

        let parts = a.finish_parts().unwrap();
        assert_eq!(
            parts.spans,
            vec![
                SpanEntry {
                    pc_delta: 0,
                    span_id: 2
                },
                SpanEntry {
                    pc_delta: 2,
                    span_id: 3
                }
            ]
        );
    }
}
