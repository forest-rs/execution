// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Bytecode builder ("assembler") for `execution_tape`.
//!
//! This is a small, public helper for constructing bytecode streams without manually computing
//! byte offsets or varint encodings.
//!
//! The encoding matches the "Draft encoding for minimal implemented opcodes" section in
//! `docs/v1_spec.md`.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::format::{write_sleb128_i64, write_uleb128_u64};
use crate::host::HostSig;
use crate::opcode::Opcode;
use crate::program::{
    Const, ConstId, ElemTypeId, FunctionDef, HostSigDef, HostSigId, HostSymbol, Program, SpanEntry,
    StructTypeDef, SymbolId, TypeId, TypeTableDef, ValueType,
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
    /// Total registers used by the function (including `r0`).
    pub reg_count: u32,
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
///         reg_count: 4,
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
}

impl ProgramBuilder {
    /// Creates an empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Interns a host symbol and returns its [`SymbolId`].
    pub fn symbol(&mut self, symbol: &str) -> SymbolId {
        if let Some(i) = self.symbols.iter().position(|s| s.symbol == symbol) {
            return SymbolId(u32::try_from(i).unwrap_or(u32::MAX));
        }
        let id = SymbolId(u32::try_from(self.symbols.len()).unwrap_or(u32::MAX));
        self.symbols.push(HostSymbol {
            symbol: symbol.into(),
        });
        id
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
        let id = FuncId(u32::try_from(self.functions.len()).unwrap_or(u32::MAX));
        self.functions.push(FunctionDef {
            arg_types: sig.arg_types,
            ret_types: sig.ret_types,
            reg_count: sig.reg_count,
            bytecode: Vec::new(),
            spans: Vec::new(),
        });
        id
    }

    /// Defines the body of a previously declared function.
    pub fn define_function(&mut self, func: FuncId, a: Asm) -> Result<(), BuildError> {
        let parts = a.finish_parts()?;
        let Some(slot) = self.functions.get_mut(func.0 as usize) else {
            return Err(BuildError::BadFuncId { func: func.0 });
        };
        slot.bytecode = parts.bytecode;
        slot.spans = parts.spans;
        Ok(())
    }

    /// Defines the body and span table of a previously declared function.
    pub fn define_function_with_spans(
        &mut self,
        func: FuncId,
        a: Asm,
        spans: Vec<SpanEntry>,
    ) -> Result<(), BuildError> {
        let Some(slot) = self.functions.get_mut(func.0 as usize) else {
            return Err(BuildError::BadFuncId { func: func.0 });
        };
        slot.bytecode = a.finish_parts()?.bytecode;
        slot.spans = spans;
        Ok(())
    }

    /// Adds a function built from assembler `a` with signature metadata.
    ///
    /// This resolves labels and records the typed signature. Full verification (including host-call
    /// signature checks and cross-function call checks) is performed by `build_checked`.
    pub fn push_function_checked(&mut self, a: Asm, sig: FunctionSig) -> Result<FuncId, AsmError> {
        let parts = a.finish_parts()?;
        let id = FuncId(u32::try_from(self.functions.len()).unwrap_or(u32::MAX));
        self.functions.push(FunctionDef {
            arg_types: sig.arg_types,
            ret_types: sig.ret_types,
            reg_count: sig.reg_count,
            bytecode: parts.bytecode,
            spans: parts.spans,
        });
        Ok(id)
    }

    /// Builds the [`Program`].
    #[must_use]
    pub fn build(self) -> Program {
        Program::new(
            self.symbols,
            self.const_pool,
            self.host_sigs,
            self.types,
            self.functions,
        )
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
    next_label: u32,
    labels: Vec<Option<u32>>,
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

    /// Allocates a new label.
    #[must_use]
    pub fn label(&mut self) -> Label {
        let id = self.next_label;
        self.next_label = self.next_label.wrapping_add(1);
        self.labels.push(None);
        Label(id)
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

        Ok(AsmParts {
            bytecode: self.bytes,
            spans,
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
        let parts = self.finish_parts()?;
        let p = Program::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: sig.arg_types.clone(),
                ret_types: sig.ret_types.clone(),
                reg_count: sig.reg_count,
                bytecode: parts.bytecode.clone(),
                spans: parts.spans.clone(),
            }],
        );
        verify_program(&p, cfg)?;
        Ok(parts)
    }

    /// `0x00 nop`.
    pub fn nop(&mut self) -> &mut Self {
        self.opcode(Opcode::Nop);
        self
    }

    /// `0x01 mov dst, src`.
    pub fn mov(&mut self, dst: u32, src: u32) -> &mut Self {
        self.opcode(Opcode::Mov);
        self.reg(dst);
        self.reg(src);
        self
    }

    /// `0x02 trap code`.
    pub fn trap(&mut self, code: u32) -> &mut Self {
        self.opcode(Opcode::Trap);
        self.uleb(code);
        self
    }

    /// `0x10 const_unit dst`.
    pub fn const_unit(&mut self, dst: u32) -> &mut Self {
        self.opcode(Opcode::ConstUnit);
        self.reg(dst);
        self
    }

    /// `0x11 const_bool dst, imm_u8`.
    pub fn const_bool(&mut self, dst: u32, imm: bool) -> &mut Self {
        self.opcode(Opcode::ConstBool);
        self.reg(dst);
        self.bytes.push(u8::from(imm));
        self
    }

    /// `0x12 const_i64 dst, imm_sleb`.
    pub fn const_i64(&mut self, dst: u32, imm: i64) -> &mut Self {
        self.opcode(Opcode::ConstI64);
        self.reg(dst);
        write_sleb128_i64(&mut self.bytes, imm);
        self
    }

    /// `0x13 const_u64 dst, imm_uleb`.
    pub fn const_u64(&mut self, dst: u32, imm: u64) -> &mut Self {
        self.opcode(Opcode::ConstU64);
        self.reg(dst);
        write_uleb128_u64(&mut self.bytes, imm);
        self
    }

    /// `0x14 const_f64 dst, bits_u64le`.
    pub fn const_f64_bits(&mut self, dst: u32, bits: u64) -> &mut Self {
        self.opcode(Opcode::ConstF64);
        self.reg(dst);
        self.bytes.extend_from_slice(&bits.to_le_bytes());
        self
    }

    /// `0x14 const_f64 dst, bits_u64le` from an `f64` value.
    pub fn const_f64(&mut self, dst: u32, v: f64) -> &mut Self {
        self.const_f64_bits(dst, v.to_bits())
    }

    /// `0x15 const_decimal dst, mantissa_sleb, scale_u8`.
    pub fn const_decimal(&mut self, dst: u32, mantissa: i64, scale: u8) -> &mut Self {
        self.opcode(Opcode::ConstDecimal);
        self.reg(dst);
        write_sleb128_i64(&mut self.bytes, mantissa);
        self.bytes.push(scale);
        self
    }

    /// `0x15 const_decimal ...` from a [`Decimal`].
    pub fn const_decimal_value(&mut self, dst: u32, v: Decimal) -> &mut Self {
        self.const_decimal(dst, v.mantissa, v.scale)
    }

    /// `0x16 const_pool dst, idx`.
    pub fn const_pool(&mut self, dst: u32, idx: ConstId) -> &mut Self {
        self.opcode(Opcode::ConstPool);
        self.reg(dst);
        self.uleb(idx.0);
        self
    }

    /// `0x17 dec_add dst, a, b`.
    pub fn dec_add(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::DecAdd);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x18 dec_sub dst, a, b`.
    pub fn dec_sub(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::DecSub);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x19 dec_mul dst, a, b`.
    pub fn dec_mul(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::DecMul);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x1A f64_add dst, a, b`.
    pub fn f64_add(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Add);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x1B f64_sub dst, a, b`.
    pub fn f64_sub(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Sub);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x1C f64_mul dst, a, b`.
    pub fn f64_mul(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Mul);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x82 f64_div dst, a, b`.
    pub fn f64_div(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Div);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x8B f64_neg dst, a`.
    pub fn f64_neg(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64Neg);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// `0x8C f64_abs dst, a`.
    pub fn f64_abs(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64Abs);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// `0x8D f64_min dst, a, b` (NaN-propagating).
    pub fn f64_min(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Min);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x8E f64_max dst, a, b` (NaN-propagating).
    pub fn f64_max(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Max);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x8F f64_min_num dst, a, b` (number-favoring).
    pub fn f64_min_num(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64MinNum);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x90 f64_max_num dst, a, b` (number-favoring).
    pub fn f64_max_num(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64MaxNum);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x91 f64_rem dst, a, b`.
    pub fn f64_rem(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Rem);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x92 f64_to_bits dst, a`.
    pub fn f64_to_bits(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64ToBits);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// `0x93 f64_from_bits dst, a`.
    pub fn f64_from_bits(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64FromBits);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// `0x20 i64_add dst, a, b`.
    pub fn i64_add(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Add);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x21 i64_sub dst, a, b`.
    pub fn i64_sub(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Sub);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x22 i64_mul dst, a, b`.
    pub fn i64_mul(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Mul);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x23 u64_add dst, a, b`.
    pub fn u64_add(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Add);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x24 u64_sub dst, a, b`.
    pub fn u64_sub(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Sub);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x25 u64_mul dst, a, b`.
    pub fn u64_mul(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Mul);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x26 u64_and dst, a, b`.
    pub fn u64_and(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64And);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x27 u64_or dst, a, b`.
    pub fn u64_or(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Or);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x28 i64_eq dst, a, b`.
    pub fn i64_eq(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Eq);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x29 i64_lt dst, a, b`.
    pub fn i64_lt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Lt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x2A u64_eq dst, a, b`.
    pub fn u64_eq(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Eq);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x2B u64_lt dst, a, b`.
    pub fn u64_lt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Lt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x2F u64_gt dst, a, b`.
    pub fn u64_gt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Gt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x2C u64_xor dst, a, b`.
    pub fn u64_xor(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Xor);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x2D u64_shl dst, a, b` (shift amount masked with `& 63`).
    pub fn u64_shl(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Shl);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x2E u64_shr dst, a, b` (shift amount masked with `& 63`).
    pub fn u64_shr(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Shr);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x30 bool_not dst, a`.
    pub fn bool_not(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::BoolNot);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// `0x88 bool_and dst, a, b`.
    pub fn bool_and(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::BoolAnd);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x89 bool_or dst, a, b`.
    pub fn bool_or(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::BoolOr);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x8A bool_xor dst, a, b`.
    pub fn bool_xor(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::BoolXor);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x83 f64_eq dst, a, b`.
    pub fn f64_eq(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Eq);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x84 f64_lt dst, a, b`.
    pub fn f64_lt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Lt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x85 f64_gt dst, a, b`.
    pub fn f64_gt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Gt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x86 f64_le dst, a, b`.
    pub fn f64_le(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Le);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x87 f64_ge dst, a, b`.
    pub fn f64_ge(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::F64Ge);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x33 i64_and dst, a, b`.
    pub fn i64_and(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64And);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x31 u64_le dst, a, b`.
    pub fn u64_le(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Le);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x32 u64_ge dst, a, b`.
    pub fn u64_ge(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Ge);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x36 i64_or dst, a, b`.
    pub fn i64_or(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Or);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x37 i64_xor dst, a, b`.
    pub fn i64_xor(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Xor);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x34 u64_to_i64 dst, a` (traps on overflow).
    pub fn u64_to_i64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U64ToI64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// `0x35 i64_to_u64 dst, a` (traps on negative).
    pub fn i64_to_u64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::I64ToU64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// `0x38 select dst, cond, a, b`.
    pub fn select(&mut self, dst: u32, cond: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::Select);
        self.reg(dst);
        self.reg(cond);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x39 i64_gt dst, a, b`.
    pub fn i64_gt(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Gt);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x3A i64_le dst, a, b`.
    pub fn i64_le(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Le);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x3B i64_ge dst, a, b`.
    pub fn i64_ge(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Ge);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x3C i64_shl dst, a, b` (shift amount masked with `& 63`).
    pub fn i64_shl(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Shl);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x3D i64_shr dst, a, b` (shift amount masked with `& 63`).
    pub fn i64_shr(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Shr);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x40 br cond, pc_true, pc_false`.
    pub fn br(&mut self, cond: u32, pc_true: Label, pc_false: Label) -> &mut Self {
        self.opcode(Opcode::Br);
        self.reg(cond);
        self.pc_label(pc_true);
        self.pc_label(pc_false);
        self
    }

    /// `0x41 jmp pc_target`.
    pub fn jmp(&mut self, pc_target: Label) -> &mut Self {
        self.opcode(Opcode::Jmp);
        self.pc_label(pc_target);
        self
    }

    /// `0x50 call eff_out, func_id, eff_in, argc, args..., retc, rets...`.
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

    /// `0x51 ret eff_in, retc, rets...`.
    pub fn ret(&mut self, eff_in: u32, rets: &[u32]) -> &mut Self {
        self.opcode(Opcode::Ret);
        self.reg(eff_in);
        self.uleb(u32::try_from(rets.len()).unwrap_or(u32::MAX));
        for &r in rets {
            self.reg(r);
        }
        self
    }

    /// `0x52 host_call eff_out, symbol_id, sig_hash_u64le, eff_in, argc, args..., retc, rets...`.
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

    /// `0x60 tuple_new dst, arity, values...`.
    pub fn tuple_new(&mut self, dst: u32, values: &[u32]) -> &mut Self {
        self.opcode(Opcode::TupleNew);
        self.reg(dst);
        self.uleb(u32::try_from(values.len()).unwrap_or(u32::MAX));
        for &v in values {
            self.reg(v);
        }
        self
    }

    /// `0x61 tuple_get dst, tuple, index`.
    pub fn tuple_get(&mut self, dst: u32, tuple: u32, index: u32) -> &mut Self {
        self.opcode(Opcode::TupleGet);
        self.reg(dst);
        self.reg(tuple);
        self.uleb(index);
        self
    }

    /// `0x62 struct_new dst, type_id, field_count, values...`.
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

    /// `0x63 struct_get dst, st, field_index`.
    pub fn struct_get(&mut self, dst: u32, st: u32, field_index: u32) -> &mut Self {
        self.opcode(Opcode::StructGet);
        self.reg(dst);
        self.reg(st);
        self.uleb(field_index);
        self
    }

    /// `0x64 array_new dst, elem_type_id, len, values...`.
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

    /// `0x65 array_len dst, arr`.
    pub fn array_len(&mut self, dst: u32, arr: u32) -> &mut Self {
        self.opcode(Opcode::ArrayLen);
        self.reg(dst);
        self.reg(arr);
        self
    }

    /// `0x66 array_get dst, arr, index_reg`.
    pub fn array_get(&mut self, dst: u32, arr: u32, index_reg: u32) -> &mut Self {
        self.opcode(Opcode::ArrayGet);
        self.reg(dst);
        self.reg(arr);
        self.reg(index_reg);
        self
    }

    /// `0x67 tuple_len dst, tuple`.
    pub fn tuple_len(&mut self, dst: u32, tuple: u32) -> &mut Self {
        self.opcode(Opcode::TupleLen);
        self.reg(dst);
        self.reg(tuple);
        self
    }

    /// `0x68 struct_field_count dst, st`.
    pub fn struct_field_count(&mut self, dst: u32, st: u32) -> &mut Self {
        self.opcode(Opcode::StructFieldCount);
        self.reg(dst);
        self.reg(st);
        self
    }

    /// `0x69 array_get_imm dst, arr, index_uleb`.
    pub fn array_get_imm(&mut self, dst: u32, arr: u32, index: u32) -> &mut Self {
        self.opcode(Opcode::ArrayGetImm);
        self.reg(dst);
        self.reg(arr);
        self.uleb(index);
        self
    }

    /// `0x6A bytes_len dst, bytes`.
    pub fn bytes_len(&mut self, dst: u32, bytes: u32) -> &mut Self {
        self.opcode(Opcode::BytesLen);
        self.reg(dst);
        self.reg(bytes);
        self
    }

    /// `0x6B str_len dst, s` (length in UTF-8 bytes).
    pub fn str_len(&mut self, dst: u32, s: u32) -> &mut Self {
        self.opcode(Opcode::StrLen);
        self.reg(dst);
        self.reg(s);
        self
    }

    /// `0x6C i64_div dst, a, b` (traps on divide-by-zero and `i64::MIN / -1`).
    pub fn i64_div(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Div);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x6D i64_rem dst, a, b` (traps on divide-by-zero and `i64::MIN % -1`).
    pub fn i64_rem(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::I64Rem);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x6E u64_div dst, a, b` (traps on divide-by-zero).
    pub fn u64_div(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Div);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x6F u64_rem dst, a, b` (traps on divide-by-zero).
    pub fn u64_rem(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::U64Rem);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x70 i64_to_f64 dst, a`.
    pub fn i64_to_f64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::I64ToF64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// `0x71 u64_to_f64 dst, a`.
    pub fn u64_to_f64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::U64ToF64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// `0x72 f64_to_i64 dst, a` (truncates, traps on NaN/inf/out-of-range).
    pub fn f64_to_i64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64ToI64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// `0x73 f64_to_u64 dst, a` (truncates, traps on NaN/inf/out-of-range/negative).
    pub fn f64_to_u64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::F64ToU64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// `0x74 dec_to_i64 dst, a` (traps if `a.scale != 0`).
    pub fn dec_to_i64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::DecToI64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// `0x75 dec_to_u64 dst, a` (traps if `a.scale != 0` or `a.mantissa < 0`).
    pub fn dec_to_u64(&mut self, dst: u32, a: u32) -> &mut Self {
        self.opcode(Opcode::DecToU64);
        self.reg(dst);
        self.reg(a);
        self
    }

    /// `0x76 i64_to_dec dst, a, scale_u8` (traps on overflow).
    pub fn i64_to_dec(&mut self, dst: u32, a: u32, scale: u8) -> &mut Self {
        self.opcode(Opcode::I64ToDec);
        self.reg(dst);
        self.reg(a);
        self.bytes.push(scale);
        self
    }

    /// `0x77 u64_to_dec dst, a, scale_u8` (traps on overflow).
    pub fn u64_to_dec(&mut self, dst: u32, a: u32, scale: u8) -> &mut Self {
        self.opcode(Opcode::U64ToDec);
        self.reg(dst);
        self.reg(a);
        self.bytes.push(scale);
        self
    }

    /// `0x78 bytes_eq dst, a, b`.
    pub fn bytes_eq(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::BytesEq);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x79 str_eq dst, a, b`.
    pub fn str_eq(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::StrEq);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x7A bytes_concat dst, a, b`.
    pub fn bytes_concat(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::BytesConcat);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x7B str_concat dst, a, b`.
    pub fn str_concat(&mut self, dst: u32, a: u32, b: u32) -> &mut Self {
        self.opcode(Opcode::StrConcat);
        self.reg(dst);
        self.reg(a);
        self.reg(b);
        self
    }

    /// `0x7C bytes_get dst, bytes, index_reg` (returns `u64` in `0..=255`, traps on OOB).
    pub fn bytes_get(&mut self, dst: u32, bytes: u32, index_reg: u32) -> &mut Self {
        self.opcode(Opcode::BytesGet);
        self.reg(dst);
        self.reg(bytes);
        self.reg(index_reg);
        self
    }

    /// `0x7D bytes_get_imm dst, bytes, index_uleb` (returns `u64` in `0..=255`, traps on OOB).
    pub fn bytes_get_imm(&mut self, dst: u32, bytes: u32, index: u32) -> &mut Self {
        self.opcode(Opcode::BytesGetImm);
        self.reg(dst);
        self.reg(bytes);
        self.uleb(index);
        self
    }

    /// `0x7E bytes_slice dst, bytes, start, end` (traps on invalid range).
    pub fn bytes_slice(&mut self, dst: u32, bytes: u32, start: u32, end: u32) -> &mut Self {
        self.opcode(Opcode::BytesSlice);
        self.reg(dst);
        self.reg(bytes);
        self.reg(start);
        self.reg(end);
        self
    }

    /// `0x7F str_slice dst, s, start, end` (byte indices; traps on invalid range/non-boundary).
    pub fn str_slice(&mut self, dst: u32, s: u32, start: u32, end: u32) -> &mut Self {
        self.opcode(Opcode::StrSlice);
        self.reg(dst);
        self.reg(s);
        self.reg(start);
        self.reg(end);
        self
    }

    /// `0x80 str_to_bytes dst, s`.
    pub fn str_to_bytes(&mut self, dst: u32, s: u32) -> &mut Self {
        self.opcode(Opcode::StrToBytes);
        self.reg(dst);
        self.reg(s);
        self
    }

    /// `0x81 bytes_to_str dst, bytes` (traps on invalid UTF-8).
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
                reg_count: 2,
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
                reg_count: 2,
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
            reg_count: 2,
        });
        let bar = pb.declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
            reg_count: 2,
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
