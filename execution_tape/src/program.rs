// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Program container and (draft) binary encoding/decoding.
//!
//! This module focuses on the *portable container format*; it does not validate control-flow or
//! type semantics. That is the job of a verifier layer (v1 ticket `et-2ef3`; see
//! [`verifier`]).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use core::num::NonZeroU32;

use crate::format::{DecodeError, Reader, Writer};
use crate::host::{HostSig, SigHash, sig_hash};

#[cfg(doc)]
use crate::verifier;

/// The `execution_tape` binary format version supported by this crate (draft).
pub const VERSION_MAJOR: u16 = 0;
/// The `execution_tape` binary format version supported by this crate (draft).
pub const VERSION_MINOR: u16 = 1;

const MAGIC: &[u8; 8] = b"EXTAPE\0\0";

/// A host-call symbol table entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSymbol {
    /// UTF-8 symbol string (typically namespace-qualified).
    pub symbol: String,
}

/// A host-call symbol table entry stored in a compact representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolEntry {
    /// UTF-8 symbol bytes (range into [`Program::symbol_data`]).
    pub bytes: ByteRange,
}

/// A constant-pool literal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Const {
    /// `()`.
    Unit,
    /// Boolean literal.
    Bool(bool),
    /// `i64` literal.
    I64(i64),
    /// `u64` literal.
    U64(u64),
    /// `f64` literal (IEEE 754 bits are serialized as little-endian).
    F64(u64),
    /// Decimal literal (`mantissa: i64`, `scale: u8`).
    Decimal {
        /// Integer mantissa.
        mantissa: i64,
        /// Base-10 scale (number of fractional digits).
        scale: u8,
    },
    /// Raw byte string.
    Bytes(Vec<u8>),
    /// UTF-8 string.
    Str(String),
}

/// A `(pc_delta, span_id)` mapping entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpanEntry {
    /// Delta from previous `pc` (or from 0 for the first entry).
    pub pc_delta: u64,
    /// Stable span identifier.
    pub span_id: u64,
}

/// Symbol table identifier (index into [`Program::symbols`]).
///
/// This is a non-zero index: symbol index `0` is reserved (and encoded as an empty symbol) and is
/// never referenced by a [`SymbolId`]. This lets `Option<SymbolId>` use the niche optimization.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SymbolId(pub NonZeroU32);

impl SymbolId {
    /// Returns the underlying symbol-table index.
    #[must_use]
    pub fn index(self) -> u32 {
        self.0.get()
    }
}

/// Constant pool identifier (index into [`Program::const_pool`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConstId(pub u32);

/// Host signature identifier (index into [`Program::host_sigs`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostSigId(pub u32);

/// A byte range (offset/length) into a per-program arena buffer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ByteRange {
    /// Start offset in bytes.
    pub offset: u32,
    /// Length in bytes.
    pub len: u32,
}

impl ByteRange {
    pub(crate) fn end(self) -> Result<u32, DecodeError> {
        self.offset
            .checked_add(self.len)
            .ok_or(DecodeError::OutOfBounds)
    }
}

/// Function metadata and bytecode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    /// Number of value arguments (excluding effect token; effect is threaded separately by bytecode).
    pub arg_count: u32,
    /// Number of value returns.
    pub ret_count: u32,
    /// Total virtual registers used by the function.
    pub reg_count: u32,
    /// Raw bytecode stream range (into [`Program::bytecode_data`]).
    pub bytecode: ByteRange,
    /// Span mapping table range (into [`Program::spans`]).
    pub spans: ByteRange,
    /// Argument types range (into [`Program::value_types`]).
    pub arg_types: ByteRange,
    /// Return types range (into [`Program::value_types`]).
    pub ret_types: ByteRange,
}

impl Function {
    /// Returns this function's argument types slice from `program`.
    #[inline]
    pub fn arg_types<'a>(&self, program: &'a Program) -> Result<&'a [ValueType], DecodeError> {
        program.function_arg_types(self)
    }

    /// Returns this function's return types slice from `program`.
    #[inline]
    pub fn ret_types<'a>(&self, program: &'a Program) -> Result<&'a [ValueType], DecodeError> {
        program.function_ret_types(self)
    }

    /// Returns this function's bytecode stream from `program`.
    #[inline]
    pub fn bytecode<'a>(&self, program: &'a Program) -> Result<&'a [u8], DecodeError> {
        program.function_bytecode(self)
    }

    /// Returns this function's span table entries from `program`.
    #[inline]
    pub fn spans<'a>(&self, program: &'a Program) -> Result<&'a [SpanEntry], DecodeError> {
        program.function_spans(self)
    }
}

/// An input function definition used to construct a [`Program`] with packed arenas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionDef {
    /// Argument types (excluding the implicit effect token).
    pub arg_types: Vec<ValueType>,
    /// Return types.
    pub ret_types: Vec<ValueType>,
    /// Total virtual registers used by the function.
    pub reg_count: u32,
    /// Raw bytecode stream.
    pub bytecode: Vec<u8>,
    /// Span mapping table entries.
    pub spans: Vec<SpanEntry>,
}

/// An input host signature definition used to construct a [`Program`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSigDef {
    /// Host symbol id.
    pub symbol: SymbolId,
    /// Argument types.
    pub args: Vec<ValueType>,
    /// Return types.
    pub rets: Vec<ValueType>,
}

/// A decoded `execution_tape` program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    /// Symbol table (host-call symbols and optional debug metadata names).
    pub symbols: Vec<SymbolEntry>,
    /// Packed UTF-8 symbol data.
    pub symbol_data: String,
    /// Constant pool.
    pub const_pool: Vec<ConstEntry>,
    /// Packed blob arena for constant bytes.
    pub const_bytes_data: Vec<u8>,
    /// Packed UTF-8 string arena for constant strings.
    pub const_str_data: String,
    /// Host signature table.
    pub host_sigs: Vec<HostSigEntry>,
    /// Type table (struct layouts and well-known ids).
    pub types: TypeTable,
    /// Packed value-type arena for function/host signatures.
    pub value_types: Vec<ValueType>,
    /// Packed bytecode arena; functions reference slices by [`ByteRange`].
    pub bytecode_data: Vec<u8>,
    /// Packed span arena; functions reference slices by [`ByteRange`] interpreted as a span-entry range.
    pub spans: Vec<SpanEntry>,
    /// Program functions.
    pub functions: Vec<Function>,
    /// Optional program name.
    pub program_name: Option<SymbolId>,
    /// Optional function-name entries.
    ///
    /// Frontends may emit these for debugging, profiling, and diagnostics. These are not required
    /// for execution.
    pub function_names: Vec<FunctionNameEntry>,
    /// Optional label-name entries (per-function pc -> name).
    ///
    /// Frontends may emit these for debugging, profiling, and diagnostics. These are not required
    /// for execution.
    pub labels: Vec<LabelNameEntry>,
}

/// A function-name entry.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FunctionNameEntry {
    /// Function index within the program.
    pub func: u32,
    /// Symbol id naming the function.
    pub name: SymbolId,
}

/// A label-name entry (per-function pc -> name).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LabelNameEntry {
    /// Function index within the program.
    pub func: u32,
    /// Bytecode pc (byte offset within the function).
    pub pc: u32,
    /// Symbol id naming the label.
    pub name: SymbolId,
}

/// A constant-pool entry stored in a compact representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConstEntry {
    /// `()`.
    Unit,
    /// Boolean literal.
    Bool(bool),
    /// `i64` literal.
    I64(i64),
    /// `u64` literal.
    U64(u64),
    /// `f64` literal (IEEE 754 bits are serialized as little-endian).
    F64(u64),
    /// Decimal literal (`mantissa: i64`, `scale: u8`).
    Decimal {
        /// Integer mantissa.
        mantissa: i64,
        /// Base-10 scale (number of fractional digits).
        scale: u8,
    },
    /// Raw byte string (range into [`Program::const_bytes_data`]).
    Bytes(ByteRange),
    /// UTF-8 string (range into [`Program::const_str_data`]).
    Str(ByteRange),
}

/// Type id index into [`TypeTable::structs`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypeId(pub u32);

/// Field-name id index into [`TypeTable::field_name_ranges`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FieldNameId(pub u32);

/// Element type id index into [`TypeTable::array_elems`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ElemTypeId(pub u32);

/// A stable identifier for a host-provided object type.
///
/// The embedder defines the meaning and registry of these ids.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostTypeId(pub u64);

/// A value type used in signatures and aggregate layouts.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ValueType {
    /// `()`.
    Unit,
    /// Boolean.
    Bool,
    /// Signed 64-bit integer.
    I64,
    /// Unsigned 64-bit integer.
    U64,
    /// IEEE 754 64-bit float.
    F64,
    /// Decimal (`i64` mantissa, per-value `u8` scale).
    Decimal,
    /// Bytes.
    Bytes,
    /// UTF-8 string.
    Str,
    /// Host object with a stable host-defined type id.
    Obj(HostTypeId),
    /// Aggregate handle.
    Agg,
    /// Function reference.
    Func,
}

/// A host-call signature table entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSigEntry {
    /// Host symbol id.
    pub symbol: SymbolId,
    /// Stable signature hash (must match the canonical hash for `args`/`rets`).
    pub sig_hash: SigHash,
    /// Argument types.
    pub args: ByteRange,
    /// Return types.
    pub rets: ByteRange,
}

/// An input struct type definition used to construct a [`TypeTable`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructTypeDef {
    /// Field names in stable order.
    pub field_names: Vec<String>,
    /// Field types in the same order as `field_names`.
    pub field_types: Vec<ValueType>,
}

/// An input program type table used to construct a [`Program`].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TypeTableDef {
    /// Struct definitions. [`TypeId`] is the index into this vector.
    pub structs: Vec<StructTypeDef>,
    /// Array element definitions. [`ElemTypeId`] is the index into this vector.
    pub array_elems: Vec<ValueType>,
}

/// A packed struct type definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructType {
    /// Field name ids (range into [`TypeTable::field_name_ids`]).
    pub field_name_ids: ByteRange,
    /// Field types (range into [`TypeTable::field_types`]).
    pub field_types: ByteRange,
}

/// A program type table.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TypeTable {
    /// UTF-8 interned field name bytes.
    pub name_data: Vec<u8>,
    /// Interned field name ranges (indexed by [`FieldNameId`], each range is into `name_data`).
    pub field_name_ranges: Vec<ByteRange>,
    /// Packed per-field field name ids.
    pub field_name_ids: Vec<FieldNameId>,
    /// Packed struct field types.
    pub field_types: Vec<ValueType>,
    /// Struct definitions. [`TypeId`] is the index into this vector.
    pub structs: Vec<StructType>,
    /// Array element definitions. [`ElemTypeId`] is the index into this vector.
    pub array_elems: Vec<ValueType>,
}

fn intern_field(
    map: &mut BTreeMap<String, FieldNameId>,
    arena: &mut Vec<u8>,
    interned_ranges: &mut Vec<ByteRange>,
    s: &str,
) -> FieldNameId {
    if let Some(&id) = map.get(s) {
        return id;
    }
    let offset = u32::try_from(arena.len()).unwrap_or(u32::MAX);
    arena.extend_from_slice(s.as_bytes());
    let len = u32::try_from(s.len()).unwrap_or(u32::MAX);
    let id = FieldNameId(u32::try_from(interned_ranges.len()).unwrap_or(u32::MAX));
    interned_ranges.push(ByteRange { offset, len });
    map.insert(String::from(s), id);
    id
}

impl TypeTable {
    fn pack(def: TypeTableDef) -> Self {
        let mut name_data: Vec<u8> = Vec::new();
        let mut field_name_ranges: Vec<ByteRange> = Vec::new();
        let mut field_name_ids: Vec<FieldNameId> = Vec::new();
        let mut field_types: Vec<ValueType> = Vec::new();
        let mut structs: Vec<StructType> = Vec::with_capacity(def.structs.len());
        let mut interned_field_names: BTreeMap<String, FieldNameId> = BTreeMap::new();

        for st in def.structs {
            let field_count = core::cmp::min(st.field_names.len(), st.field_types.len());
            debug_assert_eq!(
                st.field_names.len(),
                st.field_types.len(),
                "StructTypeDef field_names/field_types length mismatch"
            );

            let names_off = u32::try_from(field_name_ids.len()).unwrap_or(u32::MAX);
            for n in st.field_names.iter().take(field_count) {
                let id = intern_field(
                    &mut interned_field_names,
                    &mut name_data,
                    &mut field_name_ranges,
                    n,
                );
                field_name_ids.push(id);
            }
            let names_len = u32::try_from(field_count).unwrap_or(u32::MAX);

            let types_off = u32::try_from(field_types.len()).unwrap_or(u32::MAX);
            field_types.extend_from_slice(&st.field_types[..field_count]);
            let types_len = u32::try_from(field_count).unwrap_or(u32::MAX);

            structs.push(StructType {
                field_name_ids: ByteRange {
                    offset: names_off,
                    len: names_len,
                },
                field_types: ByteRange {
                    offset: types_off,
                    len: types_len,
                },
            });
        }

        Self {
            name_data,
            field_name_ranges,
            field_name_ids,
            field_types,
            structs,
            array_elems: def.array_elems,
        }
    }

    /// Returns the UTF-8 bytes for an interned field name id.
    pub fn field_name_bytes(&self, id: FieldNameId) -> Option<&[u8]> {
        let idx = usize::try_from(id.0).ok()?;
        let name = self.field_name_ranges.get(idx)?;
        let start = name.offset as usize;
        let end = name.end().ok()? as usize;
        self.name_data.get(start..end)
    }

    /// Returns the UTF-8 string slice for an interned field name id.
    pub fn field_name_str(&self, id: FieldNameId) -> Option<&str> {
        core::str::from_utf8(self.field_name_bytes(id)?).ok()
    }

    /// Returns the field-name ids for a packed struct definition.
    pub fn struct_field_name_ids(&self, st: &StructType) -> Result<&[FieldNameId], DecodeError> {
        let start = st.field_name_ids.offset as usize;
        let end = st.field_name_ids.end()? as usize;
        self.field_name_ids
            .get(start..end)
            .ok_or(DecodeError::OutOfBounds)
    }

    /// Returns the field types for a packed struct definition.
    pub fn struct_field_types(&self, st: &StructType) -> Result<&[ValueType], DecodeError> {
        let start = st.field_types.offset as usize;
        let end = st.field_types.end()? as usize;
        self.field_types
            .get(start..end)
            .ok_or(DecodeError::OutOfBounds)
    }
}

impl Program {
    /// Constructs a program by packing function bytecode and span entries into per-program arenas.
    ///
    /// This keeps the in-memory representation compact (no `Vec<Vec<_>>` or per-function allocations)
    /// while preserving a simple per-function interface via [`FunctionDef`].
    #[must_use]
    pub fn new(
        symbols: Vec<HostSymbol>,
        const_pool: Vec<Const>,
        host_sigs: Vec<HostSigDef>,
        types: TypeTableDef,
        functions: Vec<FunctionDef>,
    ) -> Self {
        // Reserve symbol index 0 (encoded as an empty symbol) so `SymbolId` can be non-zero.
        let mut symbol_data: String = String::new();
        let mut packed_symbols: Vec<SymbolEntry> = Vec::with_capacity(symbols.len() + 1);
        packed_symbols.push(SymbolEntry {
            bytes: ByteRange { offset: 0, len: 0 },
        });
        for s in symbols {
            let offset = u32::try_from(symbol_data.len()).unwrap_or(u32::MAX);
            let len = u32::try_from(s.symbol.len()).unwrap_or(u32::MAX);
            symbol_data.push_str(&s.symbol);
            packed_symbols.push(SymbolEntry {
                bytes: ByteRange { offset, len },
            });
        }

        let mut const_bytes_data: Vec<u8> = Vec::new();
        let mut const_str_data: String = String::new();
        let mut packed_consts: Vec<ConstEntry> = Vec::with_capacity(const_pool.len());
        for c in &const_pool {
            match c {
                Const::Unit => packed_consts.push(ConstEntry::Unit),
                Const::Bool(v) => packed_consts.push(ConstEntry::Bool(*v)),
                Const::I64(v) => packed_consts.push(ConstEntry::I64(*v)),
                Const::U64(v) => packed_consts.push(ConstEntry::U64(*v)),
                Const::F64(bits) => packed_consts.push(ConstEntry::F64(*bits)),
                Const::Decimal { mantissa, scale } => packed_consts.push(ConstEntry::Decimal {
                    mantissa: *mantissa,
                    scale: *scale,
                }),
                Const::Bytes(b) => {
                    let offset = u32::try_from(const_bytes_data.len()).unwrap_or(u32::MAX);
                    const_bytes_data.extend_from_slice(b);
                    let len = u32::try_from(b.len()).unwrap_or(u32::MAX);
                    packed_consts.push(ConstEntry::Bytes(ByteRange { offset, len }));
                }
                Const::Str(s) => {
                    let offset = u32::try_from(const_str_data.len()).unwrap_or(u32::MAX);
                    let len = u32::try_from(s.len()).unwrap_or(u32::MAX);
                    const_str_data.push_str(s);
                    packed_consts.push(ConstEntry::Str(ByteRange { offset, len }));
                }
            }
        }

        let types = TypeTable::pack(types);

        let mut bytecode_data: Vec<u8> = Vec::new();
        let mut spans: Vec<SpanEntry> = Vec::new();
        let mut value_types: Vec<ValueType> = Vec::new();
        let mut packed_functions: Vec<Function> = Vec::with_capacity(functions.len());
        let mut packed_host_sigs: Vec<HostSigEntry> = Vec::with_capacity(host_sigs.len());

        for hs in host_sigs {
            let args_off = u32::try_from(value_types.len()).unwrap_or(u32::MAX);
            value_types.extend_from_slice(&hs.args);
            let args_len = u32::try_from(hs.args.len()).unwrap_or(u32::MAX);
            let rets_off = u32::try_from(value_types.len()).unwrap_or(u32::MAX);
            value_types.extend_from_slice(&hs.rets);
            let rets_len = u32::try_from(hs.rets.len()).unwrap_or(u32::MAX);

            let sig = HostSig {
                args: hs.args,
                rets: hs.rets,
            };
            packed_host_sigs.push(HostSigEntry {
                symbol: hs.symbol,
                sig_hash: sig_hash(&sig),
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

        for f in functions {
            let byte_off = u32::try_from(bytecode_data.len()).unwrap_or(u32::MAX);
            bytecode_data.extend_from_slice(&f.bytecode);
            let byte_len = u32::try_from(f.bytecode.len()).unwrap_or(u32::MAX);

            let span_off = u32::try_from(spans.len()).unwrap_or(u32::MAX);
            spans.extend_from_slice(&f.spans);
            let span_len = u32::try_from(f.spans.len()).unwrap_or(u32::MAX);

            let arg_off = u32::try_from(value_types.len()).unwrap_or(u32::MAX);
            value_types.extend_from_slice(&f.arg_types);
            let arg_len = u32::try_from(f.arg_types.len()).unwrap_or(u32::MAX);
            let ret_off = u32::try_from(value_types.len()).unwrap_or(u32::MAX);
            value_types.extend_from_slice(&f.ret_types);
            let ret_len = u32::try_from(f.ret_types.len()).unwrap_or(u32::MAX);

            packed_functions.push(Function {
                arg_count: u32::try_from(f.arg_types.len()).unwrap_or(u32::MAX),
                ret_count: u32::try_from(f.ret_types.len()).unwrap_or(u32::MAX),
                reg_count: f.reg_count,
                bytecode: ByteRange {
                    offset: byte_off,
                    len: byte_len,
                },
                spans: ByteRange {
                    offset: span_off,
                    len: span_len,
                },
                arg_types: ByteRange {
                    offset: arg_off,
                    len: arg_len,
                },
                ret_types: ByteRange {
                    offset: ret_off,
                    len: ret_len,
                },
            });
        }

        Self {
            symbols: packed_symbols,
            symbol_data,
            const_pool: packed_consts,
            const_bytes_data,
            const_str_data,
            host_sigs: packed_host_sigs,
            types,
            value_types,
            bytecode_data,
            spans,
            functions: packed_functions,
            program_name: None,
            function_names: Vec::new(),
            labels: Vec::new(),
        }
    }

    /// Returns the program name, if present.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.program_name.and_then(|id| self.symbol_str(id).ok())
    }

    /// Returns the function name for `func`, if present.
    #[must_use]
    pub fn function_name(&self, func: u32) -> Option<&str> {
        self.function_names
            .iter()
            .find(|e| e.func == func)
            .and_then(|e| self.symbol_str(e.name).ok())
    }

    /// Returns the label name for `func` at `pc`, if present.
    #[must_use]
    pub fn label_name(&self, func: u32, pc: u32) -> Option<&str> {
        self.labels
            .iter()
            .find(|e| e.func == func && e.pc == pc)
            .and_then(|e| self.symbol_str(e.name).ok())
    }

    /// Returns a host-call symbol string for `id`.
    pub fn symbol_str(&self, id: SymbolId) -> Result<&str, DecodeError> {
        let e = self
            .symbols
            .get(id.index() as usize)
            .ok_or(DecodeError::OutOfBounds)?;
        let start = usize::try_from(e.bytes.offset).map_err(|_| DecodeError::OutOfBounds)?;
        let end = usize::try_from(e.bytes.end()?).map_err(|_| DecodeError::OutOfBounds)?;
        self.symbol_data
            .get(start..end)
            .ok_or(DecodeError::InvalidUtf8)
    }

    /// Returns a byte slice for a [`ConstEntry::Bytes`] constant.
    pub fn const_bytes(&self, id: ConstId) -> Result<&[u8], DecodeError> {
        let Some(ConstEntry::Bytes(r)) = self.const_pool.get(id.0 as usize) else {
            return Err(DecodeError::OutOfBounds);
        };
        let start = usize::try_from(r.offset).map_err(|_| DecodeError::OutOfBounds)?;
        let end = usize::try_from(r.end()?).map_err(|_| DecodeError::OutOfBounds)?;
        self.const_bytes_data
            .get(start..end)
            .ok_or(DecodeError::OutOfBounds)
    }

    /// Returns a UTF-8 string slice for a [`ConstEntry::Str`] constant.
    pub fn const_str(&self, id: ConstId) -> Result<&str, DecodeError> {
        let Some(ConstEntry::Str(r)) = self.const_pool.get(id.0 as usize) else {
            return Err(DecodeError::OutOfBounds);
        };
        let start = usize::try_from(r.offset).map_err(|_| DecodeError::OutOfBounds)?;
        let end = usize::try_from(r.end()?).map_err(|_| DecodeError::OutOfBounds)?;
        self.const_str_data
            .get(start..end)
            .ok_or(DecodeError::InvalidUtf8)
    }

    /// Returns a slice of argument types for `func`.
    pub fn function_arg_types(&self, func: &Function) -> Result<&[ValueType], DecodeError> {
        let start = func.arg_types.offset as usize;
        let end = func.arg_types.end()? as usize;
        self.value_types
            .get(start..end)
            .ok_or(DecodeError::OutOfBounds)
    }

    /// Returns a slice of return types for `func`.
    pub fn function_ret_types(&self, func: &Function) -> Result<&[ValueType], DecodeError> {
        let start = func.ret_types.offset as usize;
        let end = func.ret_types.end()? as usize;
        self.value_types
            .get(start..end)
            .ok_or(DecodeError::OutOfBounds)
    }

    /// Returns the host signature entry for `id`.
    pub fn host_sig(&self, id: HostSigId) -> Option<&HostSigEntry> {
        self.host_sigs.get(id.0 as usize)
    }

    /// Returns the host signature argument types for `entry`.
    pub fn host_sig_args(&self, entry: &HostSigEntry) -> Result<&[ValueType], DecodeError> {
        let start = entry.args.offset as usize;
        let end = entry.args.end()? as usize;
        self.value_types
            .get(start..end)
            .ok_or(DecodeError::OutOfBounds)
    }

    /// Returns the host signature return types for `entry`.
    pub fn host_sig_rets(&self, entry: &HostSigEntry) -> Result<&[ValueType], DecodeError> {
        let start = entry.rets.offset as usize;
        let end = entry.rets.end()? as usize;
        self.value_types
            .get(start..end)
            .ok_or(DecodeError::OutOfBounds)
    }

    /// Returns the raw bytecode slice for `func`.
    pub fn function_bytecode(&self, func: &Function) -> Result<&[u8], DecodeError> {
        let start = usize::try_from(func.bytecode.offset).map_err(|_| DecodeError::OutOfBounds)?;
        let end = usize::try_from(func.bytecode.end()?).map_err(|_| DecodeError::OutOfBounds)?;
        self.bytecode_data
            .get(start..end)
            .ok_or(DecodeError::OutOfBounds)
    }

    /// Returns the span entries for `func`.
    pub fn function_spans(&self, func: &Function) -> Result<&[SpanEntry], DecodeError> {
        let start = usize::try_from(func.spans.offset).map_err(|_| DecodeError::OutOfBounds)?;
        let end = usize::try_from(func.spans.end()?).map_err(|_| DecodeError::OutOfBounds)?;
        self.spans.get(start..end).ok_or(DecodeError::OutOfBounds)
    }

    /// Encodes this program into the draft v1 container format.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        // v1 draft container format:
        //
        // header:
        // - magic [8]
        // - major u16le
        // - minor u16le
        //
        // sections (tagged):
        // - u8 tag, uleb len, payload[len]
        //
        // Tags:
        // 1 = symbols
        // 2 = const_pool
        // 3 = types
        // 4 = function_table
        // 5 = bytecode_blobs
        // 6 = span_tables
        let mut w = Writer::new();
        w.write_bytes(MAGIC);
        w.write_u16_le(VERSION_MAJOR);
        w.write_u16_le(VERSION_MINOR);

        // symbols section
        {
            let mut payload = Writer::new();
            payload.write_uleb128_u64(self.symbols.len() as u64);
            let bytes = self.symbol_data.as_bytes();
            for s in &self.symbols {
                let start = s.bytes.offset as usize;
                let end = s.bytes.end().unwrap_or(0) as usize;
                let b = bytes.get(start..end).unwrap_or(&[]);
                payload.write_uleb128_u64(b.len() as u64);
                payload.write_bytes(b);
            }
            write_section(&mut w, SectionTag::Symbols, payload.as_slice());
        }

        // const pool section
        {
            let mut payload = Writer::new();
            payload.write_uleb128_u64(self.const_pool.len() as u64);
            let str_bytes = self.const_str_data.as_bytes();
            for c in &self.const_pool {
                encode_const(&mut payload, c, &self.const_bytes_data, str_bytes);
            }
            write_section(&mut w, SectionTag::ConstPool, payload.as_slice());
        }

        // types section
        {
            let mut payload = Writer::new();
            encode_types(&mut payload, &self.types);
            write_section(&mut w, SectionTag::Types, payload.as_slice());
        }

        // bytecode blobs section (one per function for now).
        {
            let mut payload = Writer::new();
            payload.write_uleb128_u64(self.functions.len() as u64);
            for f in &self.functions {
                let bytecode = self.function_bytecode(f).unwrap_or(&[]);
                payload.write_uleb128_u64(bytecode.len() as u64);
                payload.write_bytes(bytecode);
            }
            write_section(&mut w, SectionTag::BytecodeBlobs, payload.as_slice());
        }

        // span tables section (one per function for now).
        {
            let mut payload = Writer::new();
            payload.write_uleb128_u64(self.functions.len() as u64);
            for f in &self.functions {
                let spans = self.function_spans(f).unwrap_or(&[]);
                payload.write_uleb128_u64(spans.len() as u64);
                for s in spans {
                    payload.write_uleb128_u64(s.pc_delta);
                    payload.write_uleb128_u64(s.span_id);
                }
            }
            write_section(&mut w, SectionTag::SpanTables, payload.as_slice());
        }

        // function table section
        {
            let mut payload = Writer::new();
            payload.write_uleb128_u64(self.functions.len() as u64);
            for (i, f) in self.functions.iter().enumerate() {
                payload.write_uleb128_u32(f.arg_count);
                payload.write_uleb128_u32(f.ret_count);
                payload.write_uleb128_u32(f.reg_count);
                payload.write_uleb128_u64(i as u64); // bytecode index
                payload.write_uleb128_u64(i as u64); // span table index
            }
            write_section(&mut w, SectionTag::FunctionTable, payload.as_slice());
        }

        // function signatures section (typed, mandatory)
        {
            let mut payload = Writer::new();
            payload.write_uleb128_u64(self.functions.len() as u64);
            for f in &self.functions {
                let arg_types = self.function_arg_types(f).unwrap_or(&[]);
                payload.write_uleb128_u64(arg_types.len() as u64);
                for &t in arg_types {
                    encode_value_type(&mut payload, t);
                }
                let ret_types = self.function_ret_types(f).unwrap_or(&[]);
                payload.write_uleb128_u64(ret_types.len() as u64);
                for &t in ret_types {
                    encode_value_type(&mut payload, t);
                }
            }
            write_section(&mut w, SectionTag::FunctionSigs, payload.as_slice());
        }

        // host signature table section (typed, mandatory; may be empty)
        {
            let mut payload = Writer::new();
            payload.write_uleb128_u64(self.host_sigs.len() as u64);
            for hs in &self.host_sigs {
                payload.write_uleb128_u32(hs.symbol.index());
                payload.write_u64_le(hs.sig_hash.0);
                let args = self.host_sig_args(hs).unwrap_or(&[]);
                payload.write_uleb128_u64(args.len() as u64);
                for &t in args {
                    encode_value_type(&mut payload, t);
                }
                let rets = self.host_sig_rets(hs).unwrap_or(&[]);
                payload.write_uleb128_u64(rets.len() as u64);
                for &t in rets {
                    encode_value_type(&mut payload, t);
                }
            }
            write_section(&mut w, SectionTag::HostSigs, payload.as_slice());
        }

        if self.program_name.is_some() || !self.function_names.is_empty() || !self.labels.is_empty()
        {
            let mut payload = Writer::new();

            match self.program_name {
                None => payload.write_u8(0),
                Some(name) => {
                    payload.write_u8(1);
                    payload.write_uleb128_u32(name.index());
                }
            }

            let mut function_names = self.function_names.clone();
            function_names.sort_by(|a, b| {
                a.func
                    .cmp(&b.func)
                    .then_with(|| a.name.index().cmp(&b.name.index()))
            });
            payload.write_uleb128_u64(function_names.len() as u64);
            for e in &function_names {
                payload.write_uleb128_u32(e.func);
                payload.write_uleb128_u32(e.name.index());
            }

            let mut labels = self.labels.clone();
            labels.sort_by(|a, b| {
                a.func
                    .cmp(&b.func)
                    .then_with(|| a.pc.cmp(&b.pc))
                    .then_with(|| a.name.index().cmp(&b.name.index()))
            });
            payload.write_uleb128_u64(labels.len() as u64);
            for e in &labels {
                payload.write_uleb128_u32(e.func);
                payload.write_uleb128_u32(e.pc);
                payload.write_uleb128_u32(e.name.index());
            }

            write_section(&mut w, SectionTag::Names, payload.as_slice());
        }

        w.into_vec()
    }

    /// Decodes a draft v1 container-format program from `bytes`.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes);
        let magic = r.read_bytes(MAGIC.len())?;
        if magic != MAGIC {
            return Err(DecodeError::BadMagic);
        }

        let major = r.read_u16_le()?;
        let minor = r.read_u16_le()?;
        if major != VERSION_MAJOR {
            return Err(DecodeError::UnsupportedVersion { major, minor });
        }
        if minor != VERSION_MINOR {
            return Err(DecodeError::UnsupportedVersion { major, minor });
        }

        decode_current(bytes, r)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SectionTag {
    Symbols = 1,
    ConstPool = 2,
    Types = 3,
    FunctionTable = 4,
    BytecodeBlobs = 5,
    SpanTables = 6,
    FunctionSigs = 7,
    HostSigs = 8,
    Names = 9,
}

impl SectionTag {
    fn from_u8_opt(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Symbols),
            2 => Some(Self::ConstPool),
            3 => Some(Self::Types),
            4 => Some(Self::FunctionTable),
            5 => Some(Self::BytecodeBlobs),
            6 => Some(Self::SpanTables),
            7 => Some(Self::FunctionSigs),
            8 => Some(Self::HostSigs),
            9 => Some(Self::Names),
            _ => None,
        }
    }
}

fn write_section(w: &mut Writer, tag: SectionTag, payload: &[u8]) {
    w.write_u8(tag as u8);
    w.write_uleb128_u64(payload.len() as u64);
    w.write_bytes(payload);
}

#[derive(Clone, Debug, Default)]
struct NamesDef {
    program_name: Option<SymbolId>,
    function_names: Vec<FunctionNameEntry>,
    labels: Vec<LabelNameEntry>,
}

fn decode_names(payload: &[u8]) -> Result<NamesDef, DecodeError> {
    let mut r = Reader::new(payload);
    let has_program_name = r.read_u8()?;
    let program_name = if has_program_name == 0 {
        None
    } else if has_program_name == 1 {
        Some(SymbolId(r.read_uleb128_u32_nz()?))
    } else {
        return Err(DecodeError::OutOfBounds);
    };

    let n_funcs = read_usize(&mut r)?;
    let mut function_names = Vec::with_capacity(n_funcs);
    for _ in 0..n_funcs {
        let func = r.read_uleb128_u32()?;
        let name = r.read_uleb128_u32_nz()?;
        function_names.push(FunctionNameEntry {
            func,
            name: SymbolId(name),
        });
    }

    let n_labels = read_usize(&mut r)?;
    let mut labels = Vec::with_capacity(n_labels);
    for _ in 0..n_labels {
        let func = r.read_uleb128_u32()?;
        let pc = r.read_uleb128_u32()?;
        let name = r.read_uleb128_u32_nz()?;
        labels.push(LabelNameEntry {
            func,
            pc,
            name: SymbolId(name),
        });
    }

    if r.offset() != payload.len() {
        return Err(DecodeError::OutOfBounds);
    }

    Ok(NamesDef {
        program_name,
        function_names,
        labels,
    })
}

fn decode_symbols(payload: &[u8]) -> Result<(Vec<SymbolEntry>, String), DecodeError> {
    let mut r = Reader::new(payload);
    let n = read_usize(&mut r)?;
    let mut symbols: Vec<SymbolEntry> = Vec::with_capacity(n);
    let mut symbol_data: String = String::new();
    for _ in 0..n {
        let len = read_usize(&mut r)?;
        let s = r.read_str(len)?;
        let offset = u32::try_from(symbol_data.len()).map_err(|_| DecodeError::OutOfBounds)?;
        let len = u32::try_from(s.len()).map_err(|_| DecodeError::OutOfBounds)?;
        symbol_data.push_str(s);
        symbols.push(SymbolEntry {
            bytes: ByteRange { offset, len },
        });
    }
    // Symbol index 0 is reserved so `SymbolId` can be non-zero. We encode it as an empty string so
    // it cannot accidentally collide with a real symbol.
    if symbols.is_empty() || symbols[0].bytes.len != 0 {
        return Err(DecodeError::OutOfBounds);
    }
    Ok((symbols, symbol_data))
}

fn decode_current(bytes: &[u8], mut r: Reader<'_>) -> Result<Program, DecodeError> {
    let mut symbols: Vec<SymbolEntry> = Vec::new();
    let mut symbol_data: String = String::new();
    let mut const_pool: Vec<ConstEntry> = Vec::new();
    let mut const_bytes_data: Vec<u8> = Vec::new();
    let mut const_str_data: String = String::new();
    let mut host_sig_defs: Vec<(SymbolId, SigHash, Vec<ValueType>, Vec<ValueType>)> = Vec::new();
    let mut types: TypeTableDef = TypeTableDef::default();
    let mut function_table: Vec<FunctionTableEntry> = Vec::new();
    let mut function_sig_defs: Vec<(Vec<ValueType>, Vec<ValueType>)> = Vec::new();
    let mut bytecode_blobs: Vec<Vec<u8>> = Vec::new();
    let mut span_tables: Vec<Vec<SpanEntry>> = Vec::new();
    let mut names: NamesDef = NamesDef::default();

    let mut saw_symbols = false;
    let mut saw_const_pool = false;
    let mut saw_host_sigs = false;
    let mut saw_types = false;
    let mut saw_function_table = false;
    let mut saw_function_sigs = false;
    let mut saw_bytecode_blobs = false;
    let mut saw_span_tables = false;
    let mut saw_names = false;

    while r.offset() < bytes.len() {
        let tag = SectionTag::from_u8_opt(r.read_u8()?);
        let len = read_usize(&mut r)?;
        let payload = r.read_bytes(len)?;
        match tag {
            Some(SectionTag::Symbols) => {
                if saw_symbols {
                    return Err(DecodeError::DuplicateSection);
                }
                saw_symbols = true;
                (symbols, symbol_data) = decode_symbols(payload)?;
            }
            Some(SectionTag::ConstPool) => {
                if saw_const_pool {
                    return Err(DecodeError::DuplicateSection);
                }
                saw_const_pool = true;
                const_pool =
                    decode_const_pool(payload, &mut const_bytes_data, &mut const_str_data)?;
            }
            Some(SectionTag::Types) => {
                if saw_types {
                    return Err(DecodeError::DuplicateSection);
                }
                saw_types = true;
                types = decode_types(payload)?;
            }
            Some(SectionTag::FunctionSigs) => {
                if saw_function_sigs {
                    return Err(DecodeError::DuplicateSection);
                }
                saw_function_sigs = true;
                function_sig_defs = decode_function_sigs(payload)?;
            }
            Some(SectionTag::FunctionTable) => {
                if saw_function_table {
                    return Err(DecodeError::DuplicateSection);
                }
                saw_function_table = true;
                function_table = decode_function_table(payload)?;
            }
            Some(SectionTag::BytecodeBlobs) => {
                if saw_bytecode_blobs {
                    return Err(DecodeError::DuplicateSection);
                }
                saw_bytecode_blobs = true;
                bytecode_blobs = decode_blob_list(payload)?;
            }
            Some(SectionTag::SpanTables) => {
                if saw_span_tables {
                    return Err(DecodeError::DuplicateSection);
                }
                saw_span_tables = true;
                span_tables = decode_span_tables(payload)?;
            }
            Some(SectionTag::HostSigs) => {
                if saw_host_sigs {
                    return Err(DecodeError::DuplicateSection);
                }
                saw_host_sigs = true;
                host_sig_defs = decode_host_sigs(payload)?;
            }
            Some(SectionTag::Names) => {
                if saw_names {
                    return Err(DecodeError::DuplicateSection);
                }
                saw_names = true;
                names = decode_names(payload)?;
            }
            None => {
                // Forward-compat: skip unknown section tags.
            }
        }
    }

    if !saw_symbols {
        return Err(DecodeError::MissingSection {
            tag: SectionTag::Symbols as u8,
        });
    }
    if !saw_const_pool {
        return Err(DecodeError::MissingSection {
            tag: SectionTag::ConstPool as u8,
        });
    }
    if !saw_types {
        return Err(DecodeError::MissingSection {
            tag: SectionTag::Types as u8,
        });
    }
    if !saw_function_sigs {
        return Err(DecodeError::MissingSection {
            tag: SectionTag::FunctionSigs as u8,
        });
    }
    if !saw_function_table {
        return Err(DecodeError::MissingSection {
            tag: SectionTag::FunctionTable as u8,
        });
    }
    if !saw_bytecode_blobs {
        return Err(DecodeError::MissingSection {
            tag: SectionTag::BytecodeBlobs as u8,
        });
    }
    if !saw_span_tables {
        return Err(DecodeError::MissingSection {
            tag: SectionTag::SpanTables as u8,
        });
    }
    if !saw_host_sigs {
        return Err(DecodeError::MissingSection {
            tag: SectionTag::HostSigs as u8,
        });
    }

    // Pack arenas.
    let mut bytecode_data: Vec<u8> = Vec::new();
    let mut bytecode_ranges: Vec<ByteRange> = Vec::with_capacity(bytecode_blobs.len());
    for b in bytecode_blobs {
        let offset = u32::try_from(bytecode_data.len()).map_err(|_| DecodeError::OutOfBounds)?;
        bytecode_data.extend_from_slice(&b);
        let len = u32::try_from(b.len()).map_err(|_| DecodeError::OutOfBounds)?;
        bytecode_ranges.push(ByteRange { offset, len });
    }

    let mut spans: Vec<SpanEntry> = Vec::new();
    let mut span_ranges: Vec<ByteRange> = Vec::with_capacity(span_tables.len());
    for s in span_tables {
        let offset = u32::try_from(spans.len()).map_err(|_| DecodeError::OutOfBounds)?;
        spans.extend_from_slice(&s);
        let len = u32::try_from(s.len()).map_err(|_| DecodeError::OutOfBounds)?;
        span_ranges.push(ByteRange { offset, len });
    }

    let mut value_types: Vec<ValueType> = Vec::new();
    let mut host_sigs: Vec<HostSigEntry> = Vec::with_capacity(host_sig_defs.len());
    for (symbol, sig_hash, args, rets) in host_sig_defs {
        let args_off = u32::try_from(value_types.len()).map_err(|_| DecodeError::OutOfBounds)?;
        value_types.extend_from_slice(&args);
        let args_len = u32::try_from(args.len()).map_err(|_| DecodeError::OutOfBounds)?;
        let rets_off = u32::try_from(value_types.len()).map_err(|_| DecodeError::OutOfBounds)?;
        value_types.extend_from_slice(&rets);
        let rets_len = u32::try_from(rets.len()).map_err(|_| DecodeError::OutOfBounds)?;
        host_sigs.push(HostSigEntry {
            symbol,
            sig_hash,
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

    let mut functions = Vec::with_capacity(function_table.len());
    if function_sig_defs.len() != function_table.len() {
        return Err(DecodeError::OutOfBounds);
    }
    for (entry, (arg_types, ret_types)) in function_table.into_iter().zip(function_sig_defs) {
        let bc = *bytecode_ranges
            .get(usize::try_from(entry.bytecode_index).map_err(|_| DecodeError::OutOfBounds)?)
            .ok_or(DecodeError::OutOfBounds)?;
        let sp = *span_ranges
            .get(usize::try_from(entry.span_index).map_err(|_| DecodeError::OutOfBounds)?)
            .ok_or(DecodeError::OutOfBounds)?;

        let arg_off = u32::try_from(value_types.len()).map_err(|_| DecodeError::OutOfBounds)?;
        value_types.extend_from_slice(&arg_types);
        let arg_len = u32::try_from(arg_types.len()).map_err(|_| DecodeError::OutOfBounds)?;
        let ret_off = u32::try_from(value_types.len()).map_err(|_| DecodeError::OutOfBounds)?;
        value_types.extend_from_slice(&ret_types);
        let ret_len = u32::try_from(ret_types.len()).map_err(|_| DecodeError::OutOfBounds)?;

        functions.push(Function {
            arg_count: entry.arg_count,
            ret_count: entry.ret_count,
            reg_count: entry.reg_count,
            bytecode: bc,
            spans: sp,
            arg_types: ByteRange {
                offset: arg_off,
                len: arg_len,
            },
            ret_types: ByteRange {
                offset: ret_off,
                len: ret_len,
            },
        });
    }

    // Validate optional name metadata.
    if let Some(name) = names.program_name
        && symbols.get(name.index() as usize).is_none()
    {
        return Err(DecodeError::OutOfBounds);
    }
    for e in &names.function_names {
        if usize::try_from(e.func)
            .ok()
            .and_then(|i| functions.get(i))
            .is_none()
        {
            return Err(DecodeError::OutOfBounds);
        }
        if symbols.get(e.name.index() as usize).is_none() {
            return Err(DecodeError::OutOfBounds);
        }
    }
    for e in &names.labels {
        let Some(func) = usize::try_from(e.func).ok().and_then(|i| functions.get(i)) else {
            return Err(DecodeError::OutOfBounds);
        };
        if e.pc > func.bytecode.len {
            return Err(DecodeError::OutOfBounds);
        }
        if symbols.get(e.name.index() as usize).is_none() {
            return Err(DecodeError::OutOfBounds);
        }
    }

    Ok(Program {
        symbols,
        symbol_data,
        const_pool,
        const_bytes_data,
        const_str_data,
        host_sigs,
        types: TypeTable::pack(types),
        value_types,
        bytecode_data,
        spans,
        functions,
        program_name: names.program_name,
        function_names: names.function_names,
        labels: names.labels,
    })
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
enum ConstTag {
    Unit = 0,
    Bool = 1,
    I64 = 2,
    U64 = 3,
    F64 = 4,
    Decimal = 5,
    Bytes = 6,
    Str = 7,
}

impl ConstTag {
    fn from_u8(v: u8) -> Result<Self, DecodeError> {
        match v {
            0 => Ok(Self::Unit),
            1 => Ok(Self::Bool),
            2 => Ok(Self::I64),
            3 => Ok(Self::U64),
            4 => Ok(Self::F64),
            5 => Ok(Self::Decimal),
            6 => Ok(Self::Bytes),
            7 => Ok(Self::Str),
            _ => Err(DecodeError::OutOfBounds),
        }
    }
}

fn encode_const(w: &mut Writer, c: &ConstEntry, const_bytes: &[u8], const_str: &[u8]) {
    match c {
        ConstEntry::Unit => w.write_u8(ConstTag::Unit as u8),
        ConstEntry::Bool(v) => {
            w.write_u8(ConstTag::Bool as u8);
            w.write_u8(u8::from(*v));
        }
        ConstEntry::I64(v) => {
            w.write_u8(ConstTag::I64 as u8);
            w.write_sleb128_i64(*v);
        }
        ConstEntry::U64(v) => {
            w.write_u8(ConstTag::U64 as u8);
            w.write_uleb128_u64(*v);
        }
        ConstEntry::F64(bits) => {
            w.write_u8(ConstTag::F64 as u8);
            w.write_u64_le(*bits);
        }
        ConstEntry::Decimal { mantissa, scale } => {
            w.write_u8(ConstTag::Decimal as u8);
            w.write_sleb128_i64(*mantissa);
            w.write_u8(*scale);
        }
        ConstEntry::Bytes(r) => {
            w.write_u8(ConstTag::Bytes as u8);
            let start = r.offset as usize;
            let end = r.end().unwrap_or(0) as usize;
            let b = const_bytes.get(start..end).unwrap_or(&[]);
            w.write_uleb128_u64(b.len() as u64);
            w.write_bytes(b);
        }
        ConstEntry::Str(r) => {
            w.write_u8(ConstTag::Str as u8);
            let start = r.offset as usize;
            let end = r.end().unwrap_or(0) as usize;
            let b = const_str.get(start..end).unwrap_or(&[]);
            w.write_uleb128_u64(b.len() as u64);
            w.write_bytes(b);
        }
    }
}

fn decode_const_pool(
    payload: &[u8],
    const_bytes: &mut Vec<u8>,
    const_str: &mut String,
) -> Result<Vec<ConstEntry>, DecodeError> {
    let mut r = Reader::new(payload);
    let n = read_usize(&mut r)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let tag = ConstTag::from_u8(r.read_u8()?)?;
        let c = match tag {
            ConstTag::Unit => ConstEntry::Unit,
            ConstTag::Bool => ConstEntry::Bool(r.read_u8()? != 0),
            ConstTag::I64 => ConstEntry::I64(r.read_sleb128_i64()?),
            ConstTag::U64 => ConstEntry::U64(r.read_uleb128_u64()?),
            ConstTag::F64 => ConstEntry::F64(r.read_u64_le()?),
            ConstTag::Decimal => ConstEntry::Decimal {
                mantissa: r.read_sleb128_i64()?,
                scale: r.read_u8()?,
            },
            ConstTag::Bytes => {
                let len = read_usize(&mut r)?;
                let bytes = r.read_bytes(len)?;
                let offset =
                    u32::try_from(const_bytes.len()).map_err(|_| DecodeError::OutOfBounds)?;
                const_bytes.extend_from_slice(bytes);
                let len = u32::try_from(bytes.len()).map_err(|_| DecodeError::OutOfBounds)?;
                ConstEntry::Bytes(ByteRange { offset, len })
            }
            ConstTag::Str => {
                let len = read_usize(&mut r)?;
                let s = r.read_str(len)?;
                let offset =
                    u32::try_from(const_str.len()).map_err(|_| DecodeError::OutOfBounds)?;
                let len = u32::try_from(s.len()).map_err(|_| DecodeError::OutOfBounds)?;
                const_str.push_str(s);
                ConstEntry::Str(ByteRange { offset, len })
            }
        };
        out.push(c);
    }
    Ok(out)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
enum ValueTypeTag {
    Unit = 0,
    Bool = 1,
    I64 = 2,
    U64 = 3,
    F64 = 4,
    Decimal = 5,
    Bytes = 6,
    Str = 7,
    Obj = 8,
    Agg = 9,
    Func = 10,
}

impl ValueTypeTag {
    fn from_u8(v: u8) -> Result<Self, DecodeError> {
        match v {
            0 => Ok(Self::Unit),
            1 => Ok(Self::Bool),
            2 => Ok(Self::I64),
            3 => Ok(Self::U64),
            4 => Ok(Self::F64),
            5 => Ok(Self::Decimal),
            6 => Ok(Self::Bytes),
            7 => Ok(Self::Str),
            8 => Ok(Self::Obj),
            9 => Ok(Self::Agg),
            10 => Ok(Self::Func),
            _ => Err(DecodeError::OutOfBounds),
        }
    }
}

fn encode_value_type(w: &mut Writer, t: ValueType) {
    let tag = match t {
        ValueType::Unit => ValueTypeTag::Unit,
        ValueType::Bool => ValueTypeTag::Bool,
        ValueType::I64 => ValueTypeTag::I64,
        ValueType::U64 => ValueTypeTag::U64,
        ValueType::F64 => ValueTypeTag::F64,
        ValueType::Decimal => ValueTypeTag::Decimal,
        ValueType::Bytes => ValueTypeTag::Bytes,
        ValueType::Str => ValueTypeTag::Str,
        ValueType::Obj(_) => ValueTypeTag::Obj,
        ValueType::Agg => ValueTypeTag::Agg,
        ValueType::Func => ValueTypeTag::Func,
    };
    w.write_u8(tag as u8);
    if let ValueType::Obj(host_type) = t {
        w.write_u64_le(host_type.0);
    }
}

fn decode_value_type(r: &mut Reader<'_>) -> Result<ValueType, DecodeError> {
    let tag = ValueTypeTag::from_u8(r.read_u8()?)?;
    Ok(match tag {
        ValueTypeTag::Unit => ValueType::Unit,
        ValueTypeTag::Bool => ValueType::Bool,
        ValueTypeTag::I64 => ValueType::I64,
        ValueTypeTag::U64 => ValueType::U64,
        ValueTypeTag::F64 => ValueType::F64,
        ValueTypeTag::Decimal => ValueType::Decimal,
        ValueTypeTag::Bytes => ValueType::Bytes,
        ValueTypeTag::Str => ValueType::Str,
        ValueTypeTag::Obj => ValueType::Obj(HostTypeId(r.read_u64_le()?)),
        ValueTypeTag::Agg => ValueType::Agg,
        ValueTypeTag::Func => ValueType::Func,
    })
}

fn encode_types(w: &mut Writer, t: &TypeTable) {
    w.write_uleb128_u64(t.field_name_ranges.len() as u64);
    for name in &t.field_name_ranges {
        let start = name.offset as usize;
        let end = name.end().unwrap_or(0) as usize;
        let b = t.name_data.get(start..end).unwrap_or(&[]);
        w.write_uleb128_u64(b.len() as u64);
        w.write_bytes(b);
    }

    w.write_uleb128_u64(t.structs.len() as u64);
    for s in &t.structs {
        let field_count = s.field_name_ids.len as usize;
        w.write_uleb128_u64(field_count as u64);
        let names = t.struct_field_name_ids(s).unwrap_or(&[]);

        let types = t.struct_field_types(s).unwrap_or(&[]);
        for (name_id, ty) in names.iter().zip(types.iter()) {
            w.write_uleb128_u32(name_id.0);
            encode_value_type(w, *ty);
        }
    }
    w.write_uleb128_u64(t.array_elems.len() as u64);
    for ty in &t.array_elems {
        encode_value_type(w, *ty);
    }
}

fn decode_types(payload: &[u8]) -> Result<TypeTableDef, DecodeError> {
    let mut r = Reader::new(payload);
    let field_name_count = read_usize(&mut r)?;
    let mut interned_field_names = Vec::with_capacity(field_name_count);
    for _ in 0..field_name_count {
        let len = read_usize(&mut r)?;
        interned_field_names.push(String::from(r.read_str(len)?));
    }

    let struct_count = read_usize(&mut r)?;
    let mut structs = Vec::with_capacity(struct_count);
    for _ in 0..struct_count {
        let field_count = read_usize(&mut r)?;
        let mut field_names = Vec::with_capacity(field_count);
        let mut field_types = Vec::with_capacity(field_count);
        for _ in 0..field_count {
            let field_name_id = read_usize(&mut r)?;
            let field_name = interned_field_names
                .get(field_name_id)
                .ok_or(DecodeError::OutOfBounds)?;
            field_names.push(field_name.clone());
            field_types.push(decode_value_type(&mut r)?);
        }
        structs.push(StructTypeDef {
            field_names,
            field_types,
        });
    }
    let elem_count = read_usize(&mut r)?;
    let mut array_elems = Vec::with_capacity(elem_count);
    for _ in 0..elem_count {
        array_elems.push(decode_value_type(&mut r)?);
    }
    Ok(TypeTableDef {
        structs,
        array_elems,
    })
}

type DecodedFunctionSig = (Vec<ValueType>, Vec<ValueType>);
type DecodedHostSig = (SymbolId, SigHash, Vec<ValueType>, Vec<ValueType>);

fn decode_function_sigs(payload: &[u8]) -> Result<Vec<DecodedFunctionSig>, DecodeError> {
    let mut r = Reader::new(payload);
    let n = read_usize(&mut r)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let argc = read_usize(&mut r)?;
        let mut args = Vec::with_capacity(argc);
        for _ in 0..argc {
            args.push(decode_value_type(&mut r)?);
        }
        let retc = read_usize(&mut r)?;
        let mut rets = Vec::with_capacity(retc);
        for _ in 0..retc {
            rets.push(decode_value_type(&mut r)?);
        }
        out.push((args, rets));
    }
    Ok(out)
}

fn decode_host_sigs(payload: &[u8]) -> Result<Vec<DecodedHostSig>, DecodeError> {
    let mut r = Reader::new(payload);
    let n = read_usize(&mut r)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let symbol = SymbolId(r.read_uleb128_u32_nz()?);
        let sig_hash = SigHash(r.read_u64_le()?);
        let argc = read_usize(&mut r)?;
        let mut args = Vec::with_capacity(argc);
        for _ in 0..argc {
            args.push(decode_value_type(&mut r)?);
        }
        let retc = read_usize(&mut r)?;
        let mut rets = Vec::with_capacity(retc);
        for _ in 0..retc {
            rets.push(decode_value_type(&mut r)?);
        }
        out.push((symbol, sig_hash, args, rets));
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FunctionTableEntry {
    arg_count: u32,
    ret_count: u32,
    reg_count: u32,
    bytecode_index: u64,
    span_index: u64,
}

fn decode_function_table(payload: &[u8]) -> Result<Vec<FunctionTableEntry>, DecodeError> {
    let mut r = Reader::new(payload);
    let n = read_usize(&mut r)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let arg_count = r.read_uleb128_u32()?;
        let ret_count = r.read_uleb128_u32()?;
        let reg_count = r.read_uleb128_u32()?;
        let bytecode_index = r.read_uleb128_u64()?;
        let span_index = r.read_uleb128_u64()?;
        out.push(FunctionTableEntry {
            arg_count,
            ret_count,
            reg_count,
            bytecode_index,
            span_index,
        });
    }
    Ok(out)
}

fn decode_blob_list(payload: &[u8]) -> Result<Vec<Vec<u8>>, DecodeError> {
    let mut r = Reader::new(payload);
    let n = read_usize(&mut r)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let len = read_usize(&mut r)?;
        out.push(r.read_bytes(len)?.to_vec());
    }
    Ok(out)
}

fn decode_span_tables(payload: &[u8]) -> Result<Vec<Vec<SpanEntry>>, DecodeError> {
    let mut r = Reader::new(payload);
    let n = read_usize(&mut r)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let span_count = read_usize(&mut r)?;
        let mut spans = Vec::with_capacity(span_count);
        for _ in 0..span_count {
            spans.push(SpanEntry {
                pc_delta: r.read_uleb128_u64()?,
                span_id: r.read_uleb128_u64()?,
            });
        }
        out.push(spans);
    }
    Ok(out)
}

fn read_usize(r: &mut Reader<'_>) -> Result<usize, DecodeError> {
    let v = r.read_uleb128_u64()?;
    usize::try_from(v).map_err(|_| DecodeError::OutOfBounds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn program_roundtrips() {
        let p = Program::new(
            vec![
                HostSymbol {
                    symbol: "mesh.make_cube".into(),
                },
                HostSymbol {
                    symbol: "price.lookup".into(),
                },
            ],
            vec![
                Const::Unit,
                Const::Bool(true),
                Const::I64(-123),
                Const::U64(456),
                Const::F64(1.25_f64.to_bits()),
                Const::Decimal {
                    mantissa: 999,
                    scale: 2,
                },
                Const::Bytes(vec![0, 1, 2, 3]),
                Const::Str("hello".into()),
            ],
            vec![],
            TypeTableDef {
                structs: vec![StructTypeDef {
                    field_names: vec!["x".into(), "y".into()],
                    field_types: vec![ValueType::I64, ValueType::I64],
                }],
                array_elems: vec![ValueType::U64],
            },
            vec![
                FunctionDef {
                    arg_types: vec![ValueType::I64, ValueType::Bool],
                    ret_types: vec![ValueType::U64],
                    reg_count: 8,
                    bytecode: vec![1, 2, 3],
                    spans: vec![SpanEntry {
                        pc_delta: 0,
                        span_id: 123,
                    }],
                },
                FunctionDef {
                    arg_types: vec![],
                    ret_types: vec![],
                    reg_count: 1,
                    bytecode: vec![0xff; 32],
                    spans: vec![SpanEntry {
                        pc_delta: 5,
                        span_id: 9999,
                    }],
                },
            ],
        );

        let bytes = p.encode();
        let back = Program::decode(&bytes).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn type_table_interns_duplicate_field_names_across_structs() {
        let types = TypeTable::pack(TypeTableDef {
            structs: vec![
                StructTypeDef {
                    field_names: vec!["x".into(), "y".into()],
                    field_types: vec![ValueType::I64, ValueType::U64],
                },
                StructTypeDef {
                    field_names: vec!["y".into(), "x".into()],
                    field_types: vec![ValueType::U64, ValueType::I64],
                },
            ],
            array_elems: vec![],
        });

        assert_eq!(types.field_name_ranges.len(), 2);
        assert_eq!(types.field_name_ids.len(), 4);
        assert_eq!(types.name_data.len(), 2);

        let names0 = types.struct_field_name_ids(&types.structs[0]).unwrap();
        let names1 = types.struct_field_name_ids(&types.structs[1]).unwrap();
        assert_eq!(names0.len(), 2);
        assert_eq!(names1.len(), 2);
        assert_eq!(names0[0], names1[1]);
        assert_eq!(names0[1], names1[0]);
        assert_eq!(types.field_name_str(names0[0]), Some("x"));
        assert_eq!(types.field_name_str(names0[1]), Some("y"));
    }

    #[test]
    fn program_roundtrips_with_repeated_field_names_across_structs() {
        let p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef {
                structs: vec![
                    StructTypeDef {
                        field_names: vec!["id".into(), "value".into(), "id".into()],
                        field_types: vec![ValueType::I64, ValueType::U64, ValueType::I64],
                    },
                    StructTypeDef {
                        field_names: vec!["value".into(), "meta".into(), "id".into()],
                        field_types: vec![ValueType::U64, ValueType::Bool, ValueType::I64],
                    },
                ],
                array_elems: vec![ValueType::I64],
            },
            vec![],
        );

        let bytes = p.encode();
        let back = Program::decode(&bytes).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn decode_types_rejects_out_of_range_field_name_id() {
        let mut p = Writer::new();
        p.write_uleb128_u64(1); // field_name_count
        p.write_uleb128_u64(1); // field_name[0].len
        p.write_bytes(b"a");
        p.write_uleb128_u64(1); // struct_count
        p.write_uleb128_u64(1); // field_count
        p.write_uleb128_u64(1); // field_name_id (OOB; valid is only 0)
        encode_value_type(&mut p, ValueType::I64);
        p.write_uleb128_u64(0); // array elem_count
        assert_eq!(decode_types(p.as_slice()), Err(DecodeError::OutOfBounds));
    }

    #[test]
    fn program_names_roundtrip() {
        let mut p = Program::new(
            vec![
                HostSymbol {
                    symbol: "my_program".into(),
                },
                HostSymbol {
                    symbol: "main".into(),
                },
                HostSymbol {
                    symbol: "entry".into(),
                },
            ],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![],
                ret_types: vec![],
                reg_count: 1,
                bytecode: vec![0x51, 0x00, 0x00], // ret r0, []
                spans: vec![],
            }],
        );
        p.program_name = Some(SymbolId(NonZeroU32::new(1).unwrap()));
        p.function_names = vec![FunctionNameEntry {
            func: 0,
            name: SymbolId(NonZeroU32::new(2).unwrap()),
        }];
        p.labels = vec![LabelNameEntry {
            func: 0,
            pc: 0,
            name: SymbolId(NonZeroU32::new(3).unwrap()),
        }];

        let bytes = p.encode();
        let back = Program::decode(&bytes).unwrap();
        assert_eq!(back, p);

        assert_eq!(back.name(), Some("my_program"));
        assert_eq!(back.function_name(0), Some("main"));
        assert_eq!(back.label_name(0, 0), Some("entry"));
    }

    #[test]
    fn unknown_section_is_skipped() {
        let p = Program::new(
            vec![HostSymbol { symbol: "x".into() }],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![],
        );
        let mut bytes = p.encode();

        // Append an unknown section tag 0xFE with a 3-byte payload.
        bytes.push(0xFE);
        bytes.push(3); // uleb128(3)
        bytes.extend_from_slice(&[9, 9, 9]);

        let back = Program::decode(&bytes).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn duplicate_section_is_rejected() {
        let p = Program::new(
            vec![HostSymbol { symbol: "x".into() }],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![],
        );

        let mut bytes = p.encode();
        let dup = bytes[12..].to_vec(); // duplicate first section onwards
        bytes.extend_from_slice(&dup);

        assert_eq!(Program::decode(&bytes), Err(DecodeError::DuplicateSection));
    }

    #[test]
    fn missing_function_table_is_rejected() {
        let p = Program::new(
            vec![HostSymbol { symbol: "x".into() }],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![],
        );

        let bytes = p.encode();
        let bytes = strip_section(&bytes, SectionTag::FunctionTable as u8);
        assert_eq!(
            Program::decode(&bytes),
            Err(DecodeError::MissingSection {
                tag: SectionTag::FunctionTable as u8
            })
        );
    }

    #[test]
    fn bad_function_table_index_is_rejected() {
        // Manually build a program where the function table points at a non-existent bytecode blob.
        let mut w = Writer::new();
        w.write_bytes(MAGIC);
        w.write_u16_le(VERSION_MAJOR);
        w.write_u16_le(VERSION_MINOR);

        // symbols
        {
            let mut p = Writer::new();
            p.write_uleb128_u64(0);
            write_section(&mut w, SectionTag::Symbols, p.as_slice());
        }
        // const pool
        {
            let mut p = Writer::new();
            p.write_uleb128_u64(0);
            write_section(&mut w, SectionTag::ConstPool, p.as_slice());
        }
        // types
        {
            let mut p = Writer::new();
            encode_types(&mut p, &TypeTable::default());
            write_section(&mut w, SectionTag::Types, p.as_slice());
        }
        // bytecode blobs: 1 blob
        {
            let mut p = Writer::new();
            p.write_uleb128_u64(1);
            p.write_uleb128_u64(1);
            p.write_bytes(&[0]);
            write_section(&mut w, SectionTag::BytecodeBlobs, p.as_slice());
        }
        // span tables: 1 table
        {
            let mut p = Writer::new();
            p.write_uleb128_u64(1);
            p.write_uleb128_u64(0);
            write_section(&mut w, SectionTag::SpanTables, p.as_slice());
        }
        // function table: 1 entry, bytecode_index=1 (OOB)
        {
            let mut p = Writer::new();
            p.write_uleb128_u64(1);
            p.write_uleb128_u64(0);
            p.write_uleb128_u64(0);
            p.write_uleb128_u64(0);
            p.write_uleb128_u64(1);
            p.write_uleb128_u64(0);
            write_section(&mut w, SectionTag::FunctionTable, p.as_slice());
        }
        // function signatures: 1 entry
        {
            let mut p = Writer::new();
            p.write_uleb128_u64(1);
            p.write_uleb128_u64(0); // argc
            p.write_uleb128_u64(0); // retc
            write_section(&mut w, SectionTag::FunctionSigs, p.as_slice());
        }
        // host signatures: empty
        {
            let mut p = Writer::new();
            p.write_uleb128_u64(0);
            write_section(&mut w, SectionTag::HostSigs, p.as_slice());
        }

        assert_eq!(Program::decode(w.as_slice()), Err(DecodeError::OutOfBounds));
    }

    fn strip_section(bytes: &[u8], tag_to_strip: u8) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&bytes[..12]);

        let sections = &bytes[12..];
        let mut r = Reader::new(sections);
        while r.offset() < sections.len() {
            let tag = r.read_u8().unwrap();
            let len = read_usize(&mut r).unwrap();
            let payload = r.read_bytes(len).unwrap();
            if tag != tag_to_strip {
                out.push(tag);
                let mut w = Writer::new();
                w.write_uleb128_u64(len as u64);
                out.extend_from_slice(w.as_slice());
                out.extend_from_slice(payload);
            }
        }
        out
    }
}
