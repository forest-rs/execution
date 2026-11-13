// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

#![doc = "Code generator for `execution_tape` opcode tables.\n\n\
          This is a std-only build tool crate. It is not shipped as part of the core VM.\n"]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Deserialize, Clone)]
struct Spec {
    version: u32,
    /// Prefix bytes that open a second opcode byte. Prefixed opcodes name one of these.
    #[serde(default)]
    prefixes: Vec<PrefixSpec>,
    opcodes: Vec<OpcodeSpec>,
}

#[derive(Deserialize, Clone)]
struct PrefixSpec {
    name: String,
    byte: String,
    doc: Option<String>,
}

#[derive(Deserialize, Clone)]
struct OpcodeSpec {
    name: String,
    mnemonic: String,
    /// The prefix byte for opcodes in a prefixed space; absent for single-byte opcodes.
    #[serde(default)]
    prefix: Option<String>,
    byte: String,
    terminator: bool,
    flags: Vec<String>,
    doc: Option<String>,
    operands: Vec<OperandSpec>,
}

#[derive(Deserialize, Clone)]
struct OperandSpec {
    kind: String,
    role: String,
    encoding: String,
    field: String,
    #[serde(default)]
    access: Option<String>,
    #[serde(default)]
    count_field: Option<String>,
}

/// Lead bytes from here up are reserved for prefixes and never name a single-byte opcode.
const PREFIX_SPACE_START: u8 = 0xF0;

fn parse_u8_hex(s: &str) -> Result<u8> {
    let s = s.trim();
    let raw = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u8::from_str_radix(raw, 16).with_context(|| format!("invalid opcode byte '{s}'"))
}

fn fmt_hex_u8(b: u8) -> String {
    format!("0x{b:02X}")
}

/// Splits an opcode code into its prefix byte (if any) and its final byte.
fn split_code(code: u16) -> (Option<u8>, u8) {
    let [prefix, byte] = code.to_be_bytes();
    ((prefix != 0).then_some(prefix), byte)
}

/// Formats an opcode code as a Rust discriminant literal.
fn fmt_code(code: u16) -> String {
    match split_code(code) {
        (None, byte) => fmt_hex_u8(byte),
        (Some(_), _) => format!("0x{code:04X}"),
    }
}

/// Formats an opcode code as its wire bytes.
fn fmt_code_bytes(code: u16) -> String {
    match split_code(code) {
        (None, byte) => fmt_hex_u8(byte),
        (Some(prefix), byte) => format!("{} {}", fmt_hex_u8(prefix), fmt_hex_u8(byte)),
    }
}

/// Validated opcode table: declared prefixes, and opcodes keyed by code in code order.
///
/// A code is the opcode byte for single-byte opcodes and `prefix << 8 | byte` for prefixed ones.
struct Opcodes {
    prefixes: Vec<(u8, PrefixSpec)>,
    ops: Vec<(u16, OpcodeSpec)>,
}

impl Opcodes {
    fn single_byte(&self) -> impl Iterator<Item = (u8, &OpcodeSpec)> {
        self.ops
            .iter()
            .filter_map(|(code, op)| match split_code(*code) {
                (None, byte) => Some((byte, op)),
                (Some(_), _) => None,
            })
    }

    fn under_prefix(&self, prefix: u8) -> impl Iterator<Item = (u8, &OpcodeSpec)> {
        self.ops
            .iter()
            .filter_map(move |(code, op)| match split_code(*code) {
                (Some(p), byte) if p == prefix => Some((byte, op)),
                _ => None,
            })
    }
}

fn load(spec: Spec) -> Result<Opcodes> {
    if spec.version != 1 {
        bail!("unsupported opcodes.json version {}", spec.version);
    }

    let mut prefixes: Vec<(u8, PrefixSpec)> = Vec::with_capacity(spec.prefixes.len());
    for prefix in spec.prefixes {
        let b = parse_u8_hex(&prefix.byte)?;
        if b < PREFIX_SPACE_START {
            bail!(
                "prefix {} byte {} is below the reserved prefix space {}..=0xFF",
                prefix.name,
                fmt_hex_u8(b),
                fmt_hex_u8(PREFIX_SPACE_START)
            );
        }
        if prefix.doc.is_none() {
            bail!("missing doc for prefix {}", prefix.name);
        }
        if let Some((_, other)) = prefixes
            .iter()
            .find(|(pb, p)| *pb == b || p.name == prefix.name)
        {
            bail!(
                "duplicate prefix {} ({}): conflicts with {}",
                prefix.name,
                fmt_hex_u8(b),
                other.name
            );
        }
        prefixes.push((b, prefix));
    }
    prefixes.sort_by_key(|(b, _)| *b);

    let mut ops: Vec<(u16, OpcodeSpec)> = Vec::with_capacity(spec.opcodes.len());
    for op in spec.opcodes {
        let b = parse_u8_hex(&op.byte)?;
        let code = match op.prefix.as_deref() {
            None => {
                if b >= PREFIX_SPACE_START {
                    bail!(
                        "opcode {} byte {} is in the reserved prefix space; add a \"prefix\"",
                        op.name,
                        fmt_hex_u8(b)
                    );
                }
                u16::from(b)
            }
            Some(prefix) => {
                let p = parse_u8_hex(prefix)?;
                if !prefixes.iter().any(|(pb, _)| *pb == p) {
                    bail!(
                        "opcode {} names undeclared prefix {}",
                        op.name,
                        fmt_hex_u8(p)
                    );
                }
                u16::from_be_bytes([p, b])
            }
        };
        ops.push((code, op));
    }

    ops.sort_by(|(c0, o0), (c1, o1)| c0.cmp(c1).then_with(|| o0.name.cmp(&o1.name)));
    for w in ops.windows(2) {
        let (c0, o0) = &w[0];
        let (c1, o1) = &w[1];
        if c0 == c1 {
            bail!(
                "duplicate opcode {}: {} and {}",
                fmt_code_bytes(*c0),
                o0.name,
                o1.name
            );
        }
    }
    let mut names = BTreeSet::new();
    let mut mnemonics = BTreeSet::new();
    for (_, op) in &ops {
        if !names.insert(op.name.as_str()) {
            bail!("duplicate opcode name '{}'", op.name);
        }
        if !mnemonics.insert(op.mnemonic.as_str()) {
            bail!("duplicate opcode mnemonic '{}'", op.mnemonic);
        }
    }
    validate_operand_access(&ops)?;
    Ok(Opcodes { prefixes, ops })
}

/// Converts a `CamelCase` prefix name into an `UPPER_SNAKE` constant prefix.
fn upper_snake(name: &str) -> String {
    let mut s = String::with_capacity(name.len() + 4);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() && i != 0 {
            s.push('_');
        }
        s.extend(ch.to_uppercase());
    }
    s
}

fn validate_operand_access(ops: &[(u16, OpcodeSpec)]) -> Result<()> {
    for (_b, op) in ops {
        for operand in &op.operands {
            match operand.kind.as_str() {
                "reg" | "reg_list" => match operand.access.as_deref() {
                    Some("read") | Some("write") => {}
                    Some(other) => bail!(
                        "invalid operand access '{}' for opcode {} field {}",
                        other,
                        op.name,
                        operand.field
                    ),
                    None => bail!(
                        "missing operand access for opcode {} field {} (kind={})",
                        op.name,
                        operand.field,
                        operand.kind
                    ),
                },
                _ => {
                    if operand.access.is_some() {
                        bail!(
                            "unexpected operand access for opcode {} field {} (kind={})",
                            op.name,
                            operand.field,
                            operand.kind
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn operand_kind_rust(operand: &str) -> Result<&'static str> {
    Ok(match operand {
        "reg" => "OperandKind::Reg",
        "reg_list" => "OperandKind::RegList",
        "pc" => "OperandKind::Pc",

        "imm_bool" => "OperandKind::ImmBool",
        "imm_u8" => "OperandKind::ImmU8",
        "imm_u32" => "OperandKind::ImmU32",
        "imm_i32" => "OperandKind::ImmI32",
        "imm_i64" => "OperandKind::ImmI64",
        "decimal" => "OperandKind::Decimal",
        "imm_u64" => "OperandKind::ImmU64",

        "const_id" => "OperandKind::ConstId",
        "func_id" => "OperandKind::FuncId",
        "host_sig_id" => "OperandKind::HostSigId",
        "type_id" => "OperandKind::TypeId",
        "elem_type_id" => "OperandKind::ElemTypeId",

        other => bail!("unknown operand kind '{other}'"),
    })
}

fn operand_role_rust(role: &str) -> Result<String> {
    let mut s = String::with_capacity("OperandRole::".len() + role.len());
    s.push_str("OperandRole::");
    let mut upper_next = true;
    for ch in role.chars() {
        if ch == '_' {
            upper_next = true;
            continue;
        }
        if upper_next {
            s.extend(ch.to_uppercase());
            upper_next = false;
        } else {
            s.push(ch);
        }
    }
    Ok(s)
}

fn operand_encoding_rust(enc: &str) -> Result<&'static str> {
    Ok(match enc {
        "reg_u32_uleb" => "OperandEncoding::RegU32Uleb",
        "reg_list_u32_uleb_count_then_regs" => "OperandEncoding::RegListU32UlebCountThenRegs",

        "bool_u8" => "OperandEncoding::BoolU8",
        "u8_raw" => "OperandEncoding::U8Raw",
        "u32_uleb" => "OperandEncoding::U32Uleb",
        "i32_sleb" => "OperandEncoding::I32Sleb",
        "i64_sleb" => "OperandEncoding::I64Sleb",
        "decimal_sleb" => "OperandEncoding::DecimalSleb",
        "u64_uleb" => "OperandEncoding::U64Uleb",
        "u64_le" => "OperandEncoding::U64Le",
        "u32_le" => "OperandEncoding::U32Le",

        other => bail!("unknown operand encoding '{other}'"),
    })
}

fn operand_access_rust(access: &str) -> Result<&'static str> {
    Ok(match access {
        "read" => "OperandAccess::Read",
        "write" => "OperandAccess::Write",
        other => bail!("unknown operand access '{other}'"),
    })
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Layout {
    start: u16,
    len: u8,
}

fn generate(spec: Spec, src: &Path) -> Result<String> {
    let opcodes = load(spec)?;
    let ops = &opcodes.ops;

    let mut out = String::new();
    out.push_str("// Copyright 2026 the Execution Tape Authors\n");
    out.push_str("// SPDX-License-Identifier: Apache-2.0 OR MIT\n\n");
    out.push_str("// @generated by execution_tape_codegen. Do not edit by hand.\n");
    let _ = src;
    out.push('\n');

    out.push_str("/// Operand kinds used by the opcode table.\n");
    out.push_str("#[derive(Copy, Clone, Debug, PartialEq, Eq)]\n");
    out.push_str("pub enum OperandKind {\n");
    out.push_str("    /// A single virtual register.\n");
    out.push_str("    Reg,\n");
    out.push_str("    /// A list of virtual registers.\n");
    out.push_str("    RegList,\n");
    out.push_str("    /// A bytecode PC (byte offset).\n");
    out.push_str("    Pc,\n");
    out.push_str("    /// An immediate `bool`.\n");
    out.push_str("    ImmBool,\n");
    out.push_str("    /// An immediate `u8`.\n");
    out.push_str("    ImmU8,\n");
    out.push_str("    /// An immediate `u32`.\n");
    out.push_str("    ImmU32,\n");
    out.push_str("    /// An immediate `i32`.\n");
    out.push_str("    ImmI32,\n");
    out.push_str("    /// An immediate `i64`.\n");
    out.push_str("    ImmI64,\n");
    out.push_str("    /// A complete decimal literal.\n");
    out.push_str("    Decimal,\n");
    out.push_str("    /// An immediate `u64`.\n");
    out.push_str("    ImmU64,\n");
    out.push_str("    /// A constant pool index.\n");
    out.push_str("    ConstId,\n");
    out.push_str("    /// A function index.\n");
    out.push_str("    FuncId,\n");
    out.push_str("    /// A host signature index.\n");
    out.push_str("    HostSigId,\n");
    out.push_str("    /// A struct type index.\n");
    out.push_str("    TypeId,\n");
    out.push_str("    /// An array element type index.\n");
    out.push_str("    ElemTypeId,\n");
    out.push_str("}\n\n");

    out.push_str("/// Operand roles used by the opcode table.\n");
    out.push_str("///\n");
    out.push_str("/// Roles are a best-effort description for disassembly/tooling.\n");
    out.push_str("#[allow(missing_docs, reason = \"generated\")]\n");
    out.push_str("#[derive(Copy, Clone, Debug, PartialEq, Eq)]\n");
    out.push_str("pub enum OperandRole {\n");
    out.push_str("    Dst,\n");
    out.push_str("    Src,\n");
    out.push_str("    A,\n");
    out.push_str("    B,\n");
    out.push_str("    Cond,\n");
    out.push_str("    PcTrue,\n");
    out.push_str("    PcFalse,\n");
    out.push_str("    PcTarget,\n");
    out.push_str("    Imm,\n");
    out.push_str("    Bits,\n");
    out.push_str("    Mantissa,\n");
    out.push_str("    Scale,\n");
    out.push_str("    TrapCode,\n");
    out.push_str("    Const,\n");
    out.push_str("    Func,\n");
    out.push_str("    HostSig,\n");
    out.push_str("    Type,\n");
    out.push_str("    ElemType,\n");
    out.push_str("    EffIn,\n");
    out.push_str("    EffOut,\n");
    out.push_str("    Args,\n");
    out.push_str("    Rets,\n");
    out.push_str("    Values,\n");
    out.push_str("    Tuple,\n");
    out.push_str("    St,\n");
    out.push_str("    Arr,\n");
    out.push_str("    Index,\n");
    out.push_str("    FieldIndex,\n");
    out.push_str("    Start,\n");
    out.push_str("    End,\n");
    out.push_str("    Bytes,\n");
    out.push_str("    S,\n");
    out.push_str("    Rounding,\n");
    out.push_str("}\n\n");

    out.push_str("/// Operand encodings used by the bytecode codec.\n");
    out.push_str("#[allow(missing_docs, reason = \"generated\")]\n");
    out.push_str("#[derive(Copy, Clone, Debug, PartialEq, Eq)]\n");
    out.push_str("pub enum OperandEncoding {\n");
    out.push_str("    RegU32Uleb,\n");
    out.push_str("    RegListU32UlebCountThenRegs,\n");
    out.push_str("    BoolU8,\n");
    out.push_str("    U8Raw,\n");
    out.push_str("    U32Uleb,\n");
    out.push_str("    I32Sleb,\n");
    out.push_str("    I64Sleb,\n");
    out.push_str("    DecimalSleb,\n");
    out.push_str("    U64Uleb,\n");
    out.push_str("    U64Le,\n");
    out.push_str("    U32Le,\n");
    out.push_str("}\n\n");

    out.push_str("/// Operand access for register operands.\n");
    out.push_str("#[allow(missing_docs, reason = \"generated\")]\n");
    out.push_str("#[derive(Copy, Clone, Debug, PartialEq, Eq)]\n");
    out.push_str("pub enum OperandAccess {\n");
    out.push_str("    Read,\n");
    out.push_str("    Write,\n");
    out.push_str("}\n\n");

    out.push_str("/// Operand schema metadata (kind/role/encoding).\n");
    out.push_str("#[derive(Copy, Clone, Debug, PartialEq, Eq)]\n");
    out.push_str("pub struct OperandSchema {\n");
    out.push_str("    /// Operand kind (semantic type).\n");
    out.push_str("    pub kind: OperandKind,\n");
    out.push_str("    /// Operand role (how this operand is used).\n");
    out.push_str("    pub role: OperandRole,\n");
    out.push_str("    /// Operand encoding used by the bytecode codec.\n");
    out.push_str("    pub encoding: OperandEncoding,\n");
    out.push_str("    /// Operand access, if this is a register operand.\n");
    out.push_str("    pub access: Option<OperandAccess>,\n");
    out.push_str("}\n\n");

    out.push_str("impl OperandSchema {\n");
    out.push_str("    const fn new(kind: OperandKind, role: OperandRole, encoding: OperandEncoding, access: Option<OperandAccess>) -> Self {\n");
    out.push_str("        Self { kind, role, encoding, access }\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    out.push_str("/// Operand layout for an opcode (indices into `OPERANDS`).\n");
    out.push_str("#[derive(Copy, Clone, Debug, PartialEq, Eq)]\n");
    out.push_str("pub struct OperandLayout {\n");
    out.push_str("    /// Start index in `OPERANDS`.\n");
    out.push_str("    pub start: u16,\n");
    out.push_str("    /// Number of operand entries.\n");
    out.push_str("    pub len: u8,\n");
    out.push_str("}\n\n");

    let mut operands: Vec<String> = Vec::new();
    let mut layout_by_code: BTreeMap<u16, Layout> = BTreeMap::new();
    for (code, op) in ops {
        let start: u16 = operands
            .len()
            .try_into()
            .context("too many operands to index in u16")?;
        for operand in &op.operands {
            let kind = operand_kind_rust(&operand.kind)?;
            let role = operand_role_rust(&operand.role)?;
            let encoding = operand_encoding_rust(&operand.encoding)?;
            let access = if operand.kind == "reg" || operand.kind == "reg_list" {
                let a = operand
                    .access
                    .as_deref()
                    .expect("validated by validate_operand_access");
                let a = operand_access_rust(a)?;
                format!("Some({a})")
            } else {
                "None".to_string()
            };
            operands.push(format!(
                "OperandSchema::new({kind}, {role}, {encoding}, {access})"
            ));
        }
        let len: u8 = op
            .operands
            .len()
            .try_into()
            .with_context(|| format!("too many operands for opcode {}", op.name))?;
        layout_by_code.insert(*code, Layout { start, len });
    }

    out.push_str("/// Flat operand schema table indexed by `OperandLayout`.\n");
    out.push_str("pub const OPERANDS: &[OperandSchema] = &[\n");
    for operand in &operands {
        out.push_str(&format!("    {operand},\n"));
    }
    out.push_str("];\n\n");

    out.push_str("/// Per-opcode metadata used by decode, disasm, and verification.\n");
    out.push_str("#[derive(Copy, Clone, Debug, PartialEq, Eq)]\n");
    out.push_str("pub struct OpcodeInfo {\n");
    out.push_str("    /// Stable, parseable opcode name.\n");
    out.push_str("    pub mnemonic: &'static str,\n");
    out.push_str("    /// Whether this opcode terminates the current basic block.\n");
    out.push_str("    pub is_terminator: bool,\n");
    out.push_str("    /// Optional per-opcode traits.\n");
    out.push_str("    pub flags: OpcodeFlags,\n");
    out.push_str("    /// Operand layout for this opcode.\n");
    out.push_str("    pub operands: OperandLayout,\n");
    out.push_str("}\n\n");

    out.push_str("/// Optional per-opcode traits.\n");
    out.push_str("#[derive(Copy, Clone, Debug, PartialEq, Eq)]\n");
    out.push_str("#[repr(transparent)]\n");
    out.push_str("pub struct OpcodeFlags(u8);\n\n");

    out.push_str("impl OpcodeFlags {\n");
    out.push_str("    /// No flags set.\n");
    out.push_str("    pub const NONE: Self = Self(0);\n");
    out.push_str("    /// Call-like instruction (`call`, `host_call`, `ret`).\n");
    out.push_str("    pub const CALL_LIKE: Self = Self(1 << 0);\n");
    out.push_str("\n    /// Returns `true` if `other` is a subset of `self`.\n");
    out.push_str("    #[must_use]\n");
    out.push_str("    pub const fn contains(self, other: Self) -> bool {\n");
    out.push_str("        (self.0 & other.0) == other.0\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    // One metadata table per opcode space, indexed by the opcode's final byte.
    let info_table = |out: &mut String, prefix: Option<u8>, entries: Vec<(u8, &OpcodeSpec)>| {
        let len = entries
            .iter()
            .map(|(b, _)| usize::from(*b) + 1)
            .max()
            .unwrap_or(0);
        let mut by_byte: Vec<Option<&OpcodeSpec>> = vec![None; len];
        for (b, op) in entries {
            by_byte[usize::from(b)] = Some(op);
        }
        for (i, op) in by_byte.iter().enumerate() {
            let i = u8::try_from(i).expect("opcode byte");
            let code = match prefix {
                None => u16::from(i),
                Some(p) => u16::from_be_bytes([p, i]),
            };
            let at = fmt_code_bytes(code);
            if let Some(op) = op {
                let layout = layout_by_code[&code];
                let flags = if op.flags.iter().any(|f| f == "call_like") {
                    "OpcodeFlags::CALL_LIKE"
                } else {
                    "OpcodeFlags::NONE"
                };
                out.push_str(&format!(
                    "    OpcodeInfo {{ mnemonic: \"{}\", is_terminator: {}, flags: {}, operands: OperandLayout {{ start: {}, len: {} }} }}, // {} {}\n",
                    op.mnemonic, op.terminator, flags, layout.start, layout.len, at, op.name
                ));
            } else {
                out.push_str(&format!(
                    "    OpcodeInfo {{ mnemonic: \"<invalid>\", is_terminator: false, flags: OpcodeFlags::NONE, operands: OperandLayout {{ start: 0, len: 0 }} }}, // {at}\n"
                ));
            }
        }
    };

    out.push_str("/// Metadata for single-byte opcodes, indexed by opcode byte.\n");
    out.push_str("///\n");
    out.push_str("/// Prefixed opcodes are not listed here; use [`Opcode::info`].\n");
    out.push_str("pub const OPCODE_INFO_BY_BYTE: &[OpcodeInfo] = &[\n");
    info_table(&mut out, None, opcodes.single_byte().collect());
    out.push_str("];\n\n");

    for (pb, prefix) in &opcodes.prefixes {
        out.push_str(&format!(
            "/// Metadata for opcodes under [`OpcodePrefix::{}`], indexed by the byte after the prefix.\n",
            prefix.name
        ));
        out.push_str(&format!(
            "const {}_OPCODE_INFO: &[OpcodeInfo] = &[\n",
            upper_snake(&prefix.name)
        ));
        info_table(&mut out, Some(*pb), opcodes.under_prefix(*pb).collect());
        out.push_str("];\n\n");
    }

    out.push_str("/// Lead bytes that introduce a prefixed opcode.\n");
    out.push_str("///\n");
    out.push_str(
        "/// A prefixed opcode is encoded as its prefix byte followed by one opcode byte, so\n",
    );
    out.push_str(
        "/// each prefix opens a further 256 opcodes. Bytes `0xF0..=0xFF` are reserved for\n",
    );
    out.push_str("/// prefixes; the ones not listed here are unassigned.\n");
    out.push_str("#[derive(Copy, Clone, Debug, PartialEq, Eq)]\n");
    out.push_str("#[repr(u8)]\n");
    out.push_str("pub enum OpcodePrefix {\n");
    for (pb, prefix) in &opcodes.prefixes {
        let doc = prefix.doc.as_deref().expect("validated by load");
        for line in doc.lines() {
            out.push_str(&format!("    /// {line}\n"));
        }
        out.push_str(&format!("    {} = {},\n", prefix.name, fmt_hex_u8(*pb)));
    }
    out.push_str("}\n\n");

    out.push_str("impl OpcodePrefix {\n");
    out.push_str(
        "    /// Decodes a prefix byte, or returns `None` if `b` is not an assigned prefix.\n",
    );
    out.push_str("    #[must_use]\n");
    out.push_str("    pub const fn from_u8(b: u8) -> Option<Self> {\n");
    out.push_str("        Some(match b {\n");
    for (pb, prefix) in &opcodes.prefixes {
        out.push_str(&format!(
            "            {} => Self::{},\n",
            fmt_hex_u8(*pb),
            prefix.name
        ));
    }
    out.push_str("            _ => return None,\n");
    out.push_str("        })\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    out.push_str("/// Bytecode opcode for the v1 instruction set.\n");
    out.push_str("///\n");
    out.push_str("/// Each discriminant is the opcode's code (see [`Opcode::code`]).\n");
    out.push_str("#[derive(Copy, Clone, Debug, PartialEq, Eq)]\n");
    out.push_str("#[repr(u16)]\n");
    out.push_str("pub enum Opcode {\n");
    for (code, op) in ops {
        let doc = op
            .doc
            .as_deref()
            .with_context(|| format!("missing doc for opcode {}", op.name))?;
        for line in doc.lines() {
            out.push_str(&format!("    /// {line}\n"));
        }
        out.push_str(&format!("    {} = {},\n", op.name, fmt_code(*code)));
    }
    out.push_str("}\n\n");

    out.push_str("impl Opcode {\n");
    out.push_str("    /// Decodes a single-byte opcode.\n");
    out.push_str("    ///\n");
    out.push_str(
        "    /// Returns `None` for prefix bytes and unassigned bytes; prefixed opcodes are\n",
    );
    out.push_str("    /// decoded with [`Opcode::from_prefixed`].\n");
    out.push_str("    #[must_use]\n");
    out.push_str("    pub fn from_u8(b: u8) -> Option<Self> {\n");
    out.push_str("        Some(match b {\n");
    for (b, op) in opcodes.single_byte() {
        out.push_str(&format!(
            "            {} => Self::{},\n",
            fmt_hex_u8(b),
            op.name
        ));
    }
    out.push_str("            _ => return None,\n");
    out.push_str("        })\n");
    out.push_str("    }\n\n");

    out.push_str("    /// Decodes the opcode byte that follows `prefix`.\n");
    out.push_str("    #[must_use]\n");
    out.push_str("    pub fn from_prefixed(prefix: OpcodePrefix, b: u8) -> Option<Self> {\n");
    if opcodes
        .ops
        .iter()
        .all(|(code, _)| split_code(*code).0.is_none())
    {
        out.push_str("        let _ = (prefix, b);\n");
        out.push_str("        None\n");
    } else {
        out.push_str("        Some(match (prefix, b) {\n");
        for (pb, prefix) in &opcodes.prefixes {
            for (b, op) in opcodes.under_prefix(*pb) {
                out.push_str(&format!(
                    "            (OpcodePrefix::{}, {}) => Self::{},\n",
                    prefix.name,
                    fmt_hex_u8(b),
                    op.name
                ));
            }
        }
        out.push_str("            _ => return None,\n");
        out.push_str("        })\n");
    }
    out.push_str("    }\n\n");

    out.push_str("    /// Decodes an opcode code (see [`Opcode::code`]).\n");
    out.push_str("    #[must_use]\n");
    out.push_str("    pub fn from_code(code: u16) -> Option<Self> {\n");
    out.push_str("        Some(match code {\n");
    for (code, op) in ops {
        out.push_str(&format!(
            "            {} => Self::{},\n",
            fmt_code(*code),
            op.name
        ));
    }
    out.push_str("            _ => return None,\n");
    out.push_str("        })\n");
    out.push_str("    }\n\n");

    out.push_str("    /// Returns `true` if this opcode terminates the current basic block.\n");
    out.push_str("    #[must_use]\n");
    out.push_str("    pub fn is_terminator(self) -> bool {\n");
    out.push_str("        self.info().is_terminator\n");
    out.push_str("    }\n");

    out.push_str("\n    /// Stable, parseable opcode name.\n");
    out.push_str("    ///\n");
    out.push_str("    /// This string is used by the disassembler output.\n");
    out.push_str("    #[must_use]\n");
    out.push_str("    pub fn mnemonic(self) -> &'static str {\n");
    out.push_str("        self.info().mnemonic\n");
    out.push_str("    }\n");

    out.push_str("\n    /// Returns opcode metadata for this opcode.\n");
    out.push_str("    #[must_use]\n");
    out.push_str("    pub fn info(self) -> &'static OpcodeInfo {\n");
    out.push_str("        let [prefix, byte] = self.code().to_be_bytes();\n");
    out.push_str("        let table = match OpcodePrefix::from_u8(prefix) {\n");
    out.push_str("            None => OPCODE_INFO_BY_BYTE,\n");
    for (_, prefix) in &opcodes.prefixes {
        out.push_str(&format!(
            "            Some(OpcodePrefix::{}) => {}_OPCODE_INFO,\n",
            prefix.name,
            upper_snake(&prefix.name)
        ));
    }
    out.push_str("        };\n");
    out.push_str("        &table[usize::from(byte)]\n");
    out.push_str("    }\n");

    out.push_str("\n    /// Returns `true` if this opcode is call-like.\n");
    out.push_str("    #[must_use]\n");
    out.push_str("    pub fn is_call_like(self) -> bool {\n");
    out.push_str("        self.info().flags.contains(OpcodeFlags::CALL_LIKE)\n");
    out.push_str("    }\n");

    out.push_str("\n    /// Returns operand schema descriptors for this opcode.\n");
    out.push_str("    #[must_use]\n");
    out.push_str("    pub fn operands(self) -> &'static [OperandSchema] {\n");
    out.push_str("        let layout = self.info().operands;\n");
    out.push_str("        let start = usize::from(layout.start);\n");
    out.push_str("        let end = start + usize::from(layout.len);\n");
    out.push_str("        &OPERANDS[start..end]\n");
    out.push_str("    }\n");
    out.push_str("}\n");

    Ok(out)
}

fn rust_field_name(field: &str) -> Result<&str> {
    // Keep it intentionally strict: codegen should fail if the JSON isn't explicit
    // about field bindings, rather than guessing.
    if field.is_empty() {
        bail!("empty operand field binding");
    }
    // Rust identifiers. (We don't try to be clever; opcode JSON must provide valid names.)
    let mut chars = field.chars();
    let Some(first) = chars.next() else {
        bail!("empty operand field binding");
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        bail!("invalid operand field binding '{field}'");
    }
    for ch in chars {
        if !(ch == '_' || ch.is_ascii_alphanumeric()) {
            bail!("invalid operand field binding '{field}'");
        }
    }
    Ok(field)
}

fn generate_bytecode_decode(spec: Spec, src: &Path) -> Result<String> {
    let opcodes = load(spec)?;
    let ops = &opcodes.ops;

    let mut out = String::new();
    out.push_str("// Copyright 2026 the Execution Tape Authors\n");
    out.push_str("// SPDX-License-Identifier: Apache-2.0 OR MIT\n\n");
    out.push_str("// @generated by execution_tape_codegen. Do not edit by hand.\n");
    let _ = src;
    out.push('\n');

    out.push_str("#[rustfmt::skip]\n");
    out.push_str("pub(crate) fn decode_instr(opcode: Opcode, r: &mut Reader<'_>) -> Result<Instr, DecodeError> {\n");
    out.push_str("    Ok(match opcode {\n");

    for (_b, op) in ops {
        if op.operands.is_empty() {
            out.push_str(&format!(
                "        Opcode::{} => Instr::{},\n",
                op.name, op.name
            ));
            continue;
        }

        out.push_str(&format!("        Opcode::{} => {{\n", op.name));

        let mut field_names: Vec<&str> = Vec::with_capacity(op.operands.len() + 1);

        for operand in &op.operands {
            let field = rust_field_name(&operand.field)
                .with_context(|| format!("bad operand field for opcode {}", op.name))?;

            match operand.kind.as_str() {
                "reg" => {
                    if operand.encoding.as_str() != "reg_u32_uleb" {
                        bail!(
                            "unsupported reg encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            let {field} = crate::codec_primitives::read_reg(r)?;\n"
                    ));
                }
                "pc" | "imm_u32" => {
                    let read = match (operand.kind.as_str(), operand.encoding.as_str()) {
                        (_, "u32_uleb") => "read_u32_uleb",
                        ("imm_u32", "u32_le") => "read_u32_le",
                        _ => bail!(
                            "unsupported u32 encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        ),
                    };
                    out.push_str(&format!(
                        "            let {field} = crate::codec_primitives::{read}(r)?;\n"
                    ));
                }
                "imm_i32" => {
                    if operand.encoding.as_str() != "i32_sleb" {
                        bail!(
                            "unsupported imm_i32 encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            let {field} = crate::codec_primitives::read_i32_sleb(r)?;\n"
                    ));
                }
                "imm_u8" => {
                    if operand.encoding.as_str() != "u8_raw" {
                        bail!(
                            "unsupported imm_u8 encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            let {field} = crate::codec_primitives::read_u8_raw(r)?;\n"
                    ));
                }
                "imm_bool" => {
                    if operand.encoding.as_str() != "bool_u8" {
                        bail!(
                            "unsupported imm_bool encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            let {field} = crate::codec_primitives::read_bool_u8(r)?;\n"
                    ));
                }
                "decimal" => {
                    if operand.encoding.as_str() != "decimal_sleb" {
                        bail!(
                            "unsupported decimal encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            let {field} = crate::codec_primitives::read_decimal(r)?;\n"
                    ));
                }
                "imm_i64" => {
                    if operand.encoding.as_str() != "i64_sleb" {
                        bail!(
                            "unsupported imm_i64 encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            let {field} = crate::codec_primitives::read_i64_sleb(r)?;\n"
                    ));
                }
                "imm_u64" => match operand.encoding.as_str() {
                    "u64_uleb" => {
                        out.push_str(&format!(
                            "            let {field} = crate::codec_primitives::read_u64_uleb(r)?;\n"
                        ));
                    }
                    "u64_le" => {
                        out.push_str(&format!(
                            "            let {field} = crate::codec_primitives::read_u64_le(r)?;\n"
                        ));
                    }
                    other => bail!(
                        "unsupported imm_u64 encoding '{other}' for opcode {}",
                        op.name
                    ),
                },

                "const_id" => {
                    if operand.encoding.as_str() != "u32_uleb" {
                        bail!(
                            "unsupported const_id encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            let {field} = ConstId(crate::codec_primitives::read_u32_uleb(r)?);\n"
                    ));
                }
                "func_id" => {
                    if operand.encoding.as_str() != "u32_uleb" {
                        bail!(
                            "unsupported func_id encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            let {field} = FuncId(crate::codec_primitives::read_u32_uleb(r)?);\n"
                    ));
                }
                "host_sig_id" => {
                    if operand.encoding.as_str() != "u32_uleb" {
                        bail!(
                            "unsupported host_sig_id encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            let {field} = HostSigId(crate::codec_primitives::read_u32_uleb(r)?);\n"
                    ));
                }
                "type_id" => {
                    if operand.encoding.as_str() != "u32_uleb" {
                        bail!(
                            "unsupported type_id encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            let {field} = TypeId(crate::codec_primitives::read_u32_uleb(r)?);\n"
                    ));
                }
                "elem_type_id" => {
                    if operand.encoding.as_str() != "u32_uleb" {
                        bail!(
                            "unsupported elem_type_id encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            let {field} = ElemTypeId(crate::codec_primitives::read_u32_uleb(r)?);\n"
                    ));
                }

                "reg_list" => {
                    if operand.encoding.as_str() != "reg_list_u32_uleb_count_then_regs" {
                        bail!(
                            "unsupported reg_list encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }

                    let count_field = operand.count_field.as_deref();
                    if let Some(cf) = count_field {
                        let _ = rust_field_name(cf).with_context(|| {
                            format!("bad reg_list count_field for opcode {}", op.name)
                        })?;
                    }

                    if count_field.is_some() {
                        let count_var = format!("{field}_count");
                        out.push_str(&format!(
                            "            let {count_var} = crate::codec_primitives::read_u32_uleb(r)?;\n"
                        ));
                        out.push_str(&format!(
                            "            let mut {field} = Vec::with_capacity({count_var} as usize);\n"
                        ));
                        out.push_str(&format!(
                            "            for _ in 0..({count_var} as usize) {{\n                {field}.push(crate::codec_primitives::read_reg(r)?);\n            }}\n"
                        ));
                        if let Some(cf) = count_field {
                            out.push_str(&format!("            let {cf} = {count_var};\n"));
                            field_names.push(cf);
                        }
                    } else {
                        out.push_str(&format!(
                            "            let {field} = crate::codec_primitives::read_reg_list(r)?;\n"
                        ));
                    }
                }

                other => bail!("unsupported operand kind '{other}' for opcode {}", op.name),
            }

            field_names.push(field);
        }

        out.push_str(&format!("            Instr::{} {{\n", op.name));
        for field in field_names {
            out.push_str(&format!("                {field},\n"));
        }
        out.push_str("            }\n");
        out.push_str("        },\n");
    }

    out.push_str("    })\n");
    out.push_str("}\n");

    Ok(out)
}

fn generate_bytecode_encode(spec: Spec, src: &Path) -> Result<String> {
    let opcodes = load(spec)?;
    let ops = &opcodes.ops;

    let mut out = String::new();
    out.push_str("// Copyright 2026 the Execution Tape Authors\n");
    out.push_str("// SPDX-License-Identifier: Apache-2.0 OR MIT\n\n");
    out.push_str("// @generated by execution_tape_codegen. Do not edit by hand.\n");
    let _ = src;
    out.push('\n');

    out.push_str("#[rustfmt::skip]\n");
    out.push_str(
        "pub(crate) fn encode_instr(instr: &Instr, out: &mut Vec<u8>) -> Result<(), EncodeError> {\n",
    );
    out.push_str("    match instr {\n");

    for (code, op) in ops {
        // Single-byte opcodes keep the one-push form; prefixed ones write prefix then byte.
        let emit_opcode = match split_code(*code) {
            (None, _) => format!("out.push(Opcode::{} as u8);", op.name),
            (Some(prefix), byte) => format!(
                "out.extend_from_slice(&[{}, {}]);",
                fmt_hex_u8(prefix),
                fmt_hex_u8(byte)
            ),
        };
        if op.operands.is_empty() {
            out.push_str(&format!(
                "        Instr::{} => {{ {emit_opcode} Ok(()) }},\n",
                op.name
            ));
            continue;
        }

        let mut pattern_fields: Vec<&str> = Vec::with_capacity(op.operands.len() + 1);
        for operand in &op.operands {
            let field = rust_field_name(&operand.field)
                .with_context(|| format!("bad operand field for opcode {}", op.name))?;
            pattern_fields.push(field);
            if let Some(cf) = operand.count_field.as_deref() {
                let cf = rust_field_name(cf)
                    .with_context(|| format!("bad reg_list count_field for opcode {}", op.name))?;
                pattern_fields.push(cf);
            }
        }
        pattern_fields.sort();
        pattern_fields.dedup();

        out.push_str(&format!("        Instr::{} {{ ", op.name));
        for (i, field) in pattern_fields.iter().enumerate() {
            if i != 0 {
                out.push_str(", ");
            }
            out.push_str(field);
        }
        out.push_str(" } => {\n");
        out.push_str(&format!("            {emit_opcode}\n"));

        for operand in &op.operands {
            let field = rust_field_name(&operand.field)
                .with_context(|| format!("bad operand field for opcode {}", op.name))?;

            match operand.kind.as_str() {
                "reg" => {
                    if operand.encoding.as_str() != "reg_u32_uleb" {
                        bail!(
                            "unsupported reg encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            crate::codec_primitives::write_reg(out, *{field});\n"
                    ));
                }
                "pc" | "imm_u32" => {
                    let write = match (operand.kind.as_str(), operand.encoding.as_str()) {
                        (_, "u32_uleb") => "write_u32_uleb",
                        ("imm_u32", "u32_le") => "write_u32_le",
                        _ => bail!(
                            "unsupported u32 encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        ),
                    };
                    out.push_str(&format!(
                        "            crate::codec_primitives::{write}(out, *{field});\n"
                    ));
                }
                "imm_i32" => {
                    if operand.encoding.as_str() != "i32_sleb" {
                        bail!(
                            "unsupported imm_i32 encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            crate::codec_primitives::write_i32_sleb(out, *{field});\n"
                    ));
                }
                "imm_u8" => {
                    if operand.encoding.as_str() != "u8_raw" {
                        bail!(
                            "unsupported imm_u8 encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            crate::codec_primitives::write_u8_raw(out, *{field});\n"
                    ));
                }
                "imm_bool" => {
                    if operand.encoding.as_str() != "bool_u8" {
                        bail!(
                            "unsupported imm_bool encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            crate::codec_primitives::write_bool_u8(out, *{field});\n"
                    ));
                }
                "decimal" => {
                    if operand.encoding.as_str() != "decimal_sleb" {
                        bail!(
                            "unsupported decimal encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            crate::codec_primitives::write_decimal(out, *{field});\n"
                    ));
                }
                "imm_i64" => {
                    if operand.encoding.as_str() != "i64_sleb" {
                        bail!(
                            "unsupported imm_i64 encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            crate::codec_primitives::write_i64_sleb(out, *{field});\n"
                    ));
                }
                "imm_u64" => match operand.encoding.as_str() {
                    "u64_uleb" => {
                        out.push_str(&format!(
                            "            crate::codec_primitives::write_u64_uleb(out, *{field});\n"
                        ));
                    }
                    "u64_le" => {
                        out.push_str(&format!(
                            "            crate::codec_primitives::write_u64_le(out, *{field});\n"
                        ));
                    }
                    other => bail!(
                        "unsupported imm_u64 encoding '{other}' for opcode {}",
                        op.name
                    ),
                },

                "const_id" => {
                    if operand.encoding.as_str() != "u32_uleb" {
                        bail!(
                            "unsupported const_id encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            crate::codec_primitives::write_u32_uleb(out, {field}.0);\n"
                    ));
                }
                "func_id" => {
                    if operand.encoding.as_str() != "u32_uleb" {
                        bail!(
                            "unsupported func_id encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            crate::codec_primitives::write_u32_uleb(out, {field}.0);\n"
                    ));
                }
                "host_sig_id" => {
                    if operand.encoding.as_str() != "u32_uleb" {
                        bail!(
                            "unsupported host_sig_id encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            crate::codec_primitives::write_u32_uleb(out, {field}.0);\n"
                    ));
                }
                "type_id" => {
                    if operand.encoding.as_str() != "u32_uleb" {
                        bail!(
                            "unsupported type_id encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            crate::codec_primitives::write_u32_uleb(out, {field}.0);\n"
                    ));
                }
                "elem_type_id" => {
                    if operand.encoding.as_str() != "u32_uleb" {
                        bail!(
                            "unsupported elem_type_id encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    out.push_str(&format!(
                        "            crate::codec_primitives::write_u32_uleb(out, {field}.0);\n"
                    ));
                }

                "reg_list" => {
                    if operand.encoding.as_str() != "reg_list_u32_uleb_count_then_regs" {
                        bail!(
                            "unsupported reg_list encoding '{}' for opcode {}",
                            operand.encoding,
                            op.name
                        );
                    }
                    if let Some(cf) = operand.count_field.as_deref() {
                        let cf = rust_field_name(cf).with_context(|| {
                            format!("bad reg_list count_field for opcode {}", op.name)
                        })?;
                        out.push_str(&format!(
                            "            if {field}.len() != (*{cf} as usize) {{\n"
                        ));
                        out.push_str(&format!(
                            "                return Err(EncodeError::RegListCountMismatch {{ opcode: Opcode::{}, field: \"{}\", count: *{}, actual: {}.len() }});\n",
                            op.name, field, cf, field
                        ));
                        out.push_str("            }\n");
                        out.push_str(&format!(
                            "            crate::codec_primitives::write_u32_uleb(out, *{cf});\n"
                        ));
                        out.push_str(&format!(
                            "            for &r in {field}.iter() {{ crate::codec_primitives::write_reg(out, r); }}\n"
                        ));
                    } else {
                        out.push_str(&format!(
                            "            crate::codec_primitives::write_reg_list(out, {field}).map_err(|_| EncodeError::OutOfBounds)?;\n"
                        ));
                    }
                }

                other => bail!("unsupported operand kind '{other}' for opcode {}", op.name),
            }
        }

        out.push_str("            Ok(())\n");
        out.push_str("        },\n");
    }

    out.push_str("    }\n");
    out.push_str("}\n");

    Ok(out)
}

fn generate_bytecode_instr_helpers(spec: Spec, src: &Path) -> Result<String> {
    let opcodes = load(spec)?;
    let ops = &opcodes.ops;

    let mut out = String::new();
    out.push_str("// Copyright 2026 the Execution Tape Authors\n");
    out.push_str("// SPDX-License-Identifier: Apache-2.0 OR MIT\n\n");
    out.push_str("// @generated by execution_tape_codegen. Do not edit by hand.\n");
    let _ = src;
    out.push('\n');

    out.push_str("impl Instr {\n");
    out.push_str("    /// Returns the opcode for this instruction.\n");
    out.push_str("    #[must_use]\n");
    out.push_str("    pub(crate) fn opcode(&self) -> Opcode {\n");
    out.push_str("        match self {\n");
    for (_b, op) in ops {
        if op.operands.is_empty() {
            out.push_str(&format!(
                "            Self::{} => Opcode::{},\n",
                op.name, op.name
            ));
        } else {
            out.push_str(&format!(
                "            Self::{} {{ .. }} => Opcode::{},\n",
                op.name, op.name
            ));
        }
    }
    out.push_str("        }\n");
    out.push_str("    }\n\n");

    out.push_str("    /// Returns operand schema descriptors for this instruction.\n");
    out.push_str("    #[must_use]\n");
    out.push_str(
        "    pub(crate) fn operand_schema(&self) -> &'static [crate::opcode::OperandSchema] {\n",
    );
    out.push_str("        self.opcode().operands()\n");
    out.push_str("    }\n");
    out.push_str("}\n");

    Ok(out)
}

fn generate_instr_operands(spec: Spec, src: &Path) -> Result<String> {
    let opcodes = load(spec)?;
    let ops = &opcodes.ops;

    let mut out = String::new();
    out.push_str("// Copyright 2026 the Execution Tape Authors\n");
    out.push_str("// SPDX-License-Identifier: Apache-2.0 OR MIT\n\n");
    out.push_str("// @generated by execution_tape_codegen. Do not edit by hand.\n");
    let _ = src;
    out.push('\n');

    out.push_str("use crate::bytecode::Instr;\n");
    out.push_str("use crate::program::{ConstId, ElemTypeId, HostSigId, TypeId};\n");
    out.push_str("use crate::value::FuncId;\n\n");

    out.push_str("#[rustfmt::skip]\n");
    out.push_str("pub(crate) fn visit_pcs(instr: &Instr, mut f: impl FnMut(u32)) {\n");
    let mut pc_arms: Vec<(&str, Vec<&str>)> = Vec::new();
    for (_b, op) in ops {
        let pc_fields: Vec<&str> = op
            .operands
            .iter()
            .filter(|o| o.kind == "pc")
            .map(|o| {
                rust_field_name(&o.field)
                    .with_context(|| format!("bad field name '{}' for opcode {}", o.field, op.name))
            })
            .collect::<Result<_>>()?;
        if !pc_fields.is_empty() {
            pc_arms.push((op.name.as_str(), pc_fields));
        }
    }

    match pc_arms.len() {
        0 => {
            out.push_str("    let _ = instr;\n");
            out.push_str("    let _ = f;\n");
        }
        1 => {
            let (op_name, fields) = &pc_arms[0];
            out.push_str(&format!("    if let Instr::{op_name} {{ "));
            for (i, field) in fields.iter().enumerate() {
                if i != 0 {
                    out.push_str(", ");
                }
                out.push_str(field);
            }
            out.push_str(", .. } = instr {\n");
            for field in fields {
                out.push_str(&format!("        f(*{field});\n"));
            }
            out.push_str("    }\n");
        }
        _ => {
            out.push_str("    match instr {\n");
            for (op_name, fields) in &pc_arms {
                out.push_str(&format!("        Instr::{op_name} {{ "));
                for (i, field) in fields.iter().enumerate() {
                    if i != 0 {
                        out.push_str(", ");
                    }
                    out.push_str(field);
                }
                out.push_str(", .. } => {\n");
                for field in fields {
                    out.push_str(&format!("            f(*{field});\n"));
                }
                out.push_str("        }\n");
            }
            out.push_str("        _ => {}\n");
            out.push_str("    }\n");
        }
    }
    out.push_str("}\n\n");

    fn gen_id_visitor(
        out: &mut String,
        ops: &[(u16, OpcodeSpec)],
        fn_name: &str,
        kind: &str,
        ty: &str,
    ) -> Result<()> {
        out.push_str("#[rustfmt::skip]\n");
        out.push_str(&format!(
            "pub(crate) fn {fn_name}(instr: &Instr, mut f: impl FnMut({ty})) {{\n"
        ));

        let mut arms: Vec<(&str, Vec<&str>)> = Vec::new();
        for (_b, op) in ops {
            let fields: Vec<&str> = op
                .operands
                .iter()
                .filter(|o| o.kind == kind)
                .map(|o| {
                    rust_field_name(&o.field).with_context(|| {
                        format!("bad field name '{}' for opcode {}", o.field, op.name)
                    })
                })
                .collect::<Result<_>>()?;
            if !fields.is_empty() {
                arms.push((op.name.as_str(), fields));
            }
        }

        match arms.len() {
            0 => {
                out.push_str("    let _ = instr;\n");
                out.push_str("    let _ = f;\n");
            }
            1 => {
                let (op_name, fields) = &arms[0];
                out.push_str(&format!("    if let Instr::{op_name} {{ "));
                for (i, field) in fields.iter().enumerate() {
                    if i != 0 {
                        out.push_str(", ");
                    }
                    out.push_str(field);
                }
                out.push_str(", .. } = instr {\n");
                for field in fields {
                    out.push_str(&format!("        f(*{field});\n"));
                }
                out.push_str("    }\n");
            }
            _ => {
                out.push_str("    match instr {\n");
                for (op_name, fields) in &arms {
                    out.push_str(&format!("        Instr::{op_name} {{ "));
                    for (i, field) in fields.iter().enumerate() {
                        if i != 0 {
                            out.push_str(", ");
                        }
                        out.push_str(field);
                    }
                    out.push_str(", .. } => {\n");
                    for field in fields {
                        out.push_str(&format!("            f(*{field});\n"));
                    }
                    out.push_str("        }\n");
                }
                out.push_str("        _ => {}\n");
                out.push_str("    }\n");
            }
        }

        out.push_str("}\n\n");
        Ok(())
    }

    gen_id_visitor(&mut out, ops, "visit_const_ids", "const_id", "ConstId")?;
    gen_id_visitor(&mut out, ops, "visit_func_ids", "func_id", "FuncId")?;
    gen_id_visitor(
        &mut out,
        ops,
        "visit_host_sig_ids",
        "host_sig_id",
        "HostSigId",
    )?;
    gen_id_visitor(&mut out, ops, "visit_type_ids", "type_id", "TypeId")?;
    gen_id_visitor(
        &mut out,
        ops,
        "visit_elem_type_ids",
        "elem_type_id",
        "ElemTypeId",
    )?;

    Ok(out)
}

fn generate_reads_writes(spec: Spec, src: &Path) -> Result<String> {
    let opcodes = load(spec)?;
    let ops = &opcodes.ops;

    let mut out = String::new();
    out.push_str("// Copyright 2026 the Execution Tape Authors\n");
    out.push_str("// SPDX-License-Identifier: Apache-2.0 OR MIT\n\n");
    out.push_str("// @generated by execution_tape_codegen. Do not edit by hand.\n");
    let _ = src;
    out.push('\n');

    let capacity = |kind: &str, access: &str| {
        ops.iter()
            .map(|(_, op)| {
                op.operands
                    .iter()
                    .filter(|operand| {
                        operand.kind == kind && operand.access.as_deref() == Some(access)
                    })
                    .count()
            })
            .max()
            .unwrap_or(0)
    };
    let max_scalars = capacity("reg", "read").max(capacity("reg", "write")).max(3);
    let max_lists = capacity("reg_list", "read")
        .max(capacity("reg_list", "write"))
        .max(1);
    u8::try_from(max_scalars).context("too many scalar register operands")?;
    u8::try_from(max_lists).context("too many register lists")?;
    out.push_str(&format!(
        "pub(crate) const MAX_SCALAR_OPERANDS: usize = {max_scalars};\n"
    ));
    out.push_str(&format!(
        "pub(crate) const MAX_REGISTER_LISTS: usize = {max_lists};\n\n"
    ));
    for (name, access) in [("ReadsIter", "read"), ("WritesIter", "write")] {
        out.push_str(&format!(
            "pub(crate) type {name}<'a> = OperandIter<'a, {}, {}>;\n",
            capacity("reg", access),
            capacity("reg_list", access)
        ));
    }
    out.push('\n');
    out.push_str("impl Instr {\n");
    for (method, access, iter) in [
        ("reads", "read", "ReadsIter"),
        ("writes", "write", "WritesIter"),
    ] {
        let scalar_capacity = capacity("reg", access);
        let list_capacity = capacity("reg_list", access);
        out.push_str(&format!("    /// Iterates virtual registers {access} by this instruction without allocation.\n    #[must_use]\n    pub(crate) fn {method}(&self) -> {iter}<'_> {{\n        match self {{\n"));
        for (_, op) in ops {
            let operands: Vec<_> = op
                .operands
                .iter()
                .filter(|operand| operand.access.as_deref() == Some(access))
                .collect();
            let mut scalar = Vec::new();
            let mut lists = Vec::new();
            let mut fields = Vec::new();
            for operand in operands {
                let field = rust_field_name(&operand.field)
                    .with_context(|| format!("bad operand field for opcode {}", op.name))?;
                fields.push(field);
                match operand.kind.as_str() {
                    "reg" => scalar.push(format!("*{field}")),
                    "reg_list" => lists.push(format!("{field}.as_slice()")),
                    _ => unreachable!("validated register access"),
                }
            }
            if op.operands.is_empty() {
                out.push_str(&format!("            Self::{} => ", op.name));
            } else if fields.is_empty() {
                out.push_str(&format!("            Self::{} {{ .. }} => ", op.name));
            } else {
                out.push_str(&format!(
                    "            Self::{} {{ {}, .. }} => ",
                    op.name,
                    fields.join(", ")
                ));
            }
            let scalar_count = scalar.len();
            scalar.resize(scalar_capacity, "0".into());
            lists.resize(list_capacity, "&[]".into());
            out.push_str(&format!(
                "{iter}::from_parts([{}], {scalar_count}, [{}]),\n",
                scalar.join(", "),
                lists.join(", ")
            ));
        }
        out.push_str("        }\n    }\n");
    }
    out.push_str("}\n");

    Ok(out)
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let spec_path: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("execution_tape/opcodes.json"));
    let opcode_out_path: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("execution_tape/src/opcodes_gen.rs"));
    let decode_out_path: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("execution_tape/src/bytecode_decode_gen.rs"));
    let encode_out_path: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("execution_tape/src/bytecode_encode_gen.rs"));
    let bytecode_instr_out_path: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("execution_tape/src/bytecode_instr_gen.rs"));
    let instr_operands_out_path: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("execution_tape/src/instr_operands_gen.rs"));
    let reads_writes_out_path: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("execution_tape/src/bytecode_reads_writes_gen.rs"));
    if args.next().is_some() {
        bail!(
            "usage: execution_tape_codegen [spec.json] [opcodes_out.rs] [decode_out.rs] [encode_out.rs] [bytecode_instr_out.rs] [instr_operands_out.rs] [reads_writes_out.rs]"
        );
    }

    let json =
        fs::read_to_string(&spec_path).with_context(|| format!("read {}", spec_path.display()))?;
    let spec: Spec =
        serde_json::from_str(&json).with_context(|| format!("parse {}", spec_path.display()))?;

    let opcode_rendered = generate(spec.clone(), &spec_path)?;
    let decode_rendered = generate_bytecode_decode(spec.clone(), &spec_path)?;
    let encode_rendered = generate_bytecode_encode(spec.clone(), &spec_path)?;
    let bytecode_instr_rendered = generate_bytecode_instr_helpers(spec.clone(), &spec_path)?;
    let instr_operands_rendered = generate_instr_operands(spec.clone(), &spec_path)?;
    let reads_writes_rendered = generate_reads_writes(spec, &spec_path)?;

    if let Some(parent) = opcode_out_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&opcode_out_path, opcode_rendered.as_bytes())
        .with_context(|| format!("write {}", opcode_out_path.display()))?;

    if let Some(parent) = decode_out_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&decode_out_path, decode_rendered.as_bytes())
        .with_context(|| format!("write {}", decode_out_path.display()))?;
    fs::write(&encode_out_path, encode_rendered.as_bytes())
        .with_context(|| format!("write {}", encode_out_path.display()))?;
    fs::write(&bytecode_instr_out_path, bytecode_instr_rendered.as_bytes())
        .with_context(|| format!("write {}", bytecode_instr_out_path.display()))?;
    fs::write(&instr_operands_out_path, instr_operands_rendered.as_bytes())
        .with_context(|| format!("write {}", instr_operands_out_path.display()))?;
    fs::write(&reads_writes_out_path, reads_writes_rendered.as_bytes())
        .with_context(|| format!("write {}", reads_writes_out_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Spec, generate, generate_bytecode_decode, generate_bytecode_encode,
        generate_bytecode_instr_helpers, generate_instr_operands, generate_reads_writes, load,
    };
    use std::fs;
    use std::path::PathBuf;

    fn normalize_newlines(s: &str) -> String {
        // On Windows, git autocrlf can check in generated `.rs` files with `\r\n` line endings.
        // Normalize so the drift test validates content, not platform line terminators.
        s.replace("\r\n", "\n").replace('\r', "\n")
    }

    fn assert_eq_normalized(what: &str, rendered: &str, existing: &str) {
        assert_eq!(
            normalize_newlines(rendered),
            normalize_newlines(existing),
            "{what} is out of date; re-run: cargo run -p execution_tape_codegen"
        );
    }

    #[test]
    fn generated_register_iteration_executes_multiple_lists_and_wide_scalar_operands() {
        use std::process::Command;
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        struct Temp(PathBuf);
        impl Drop for Temp {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let dir = Temp(std::env::temp_dir().join(format!(
            "execution-register-iteration-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        fs::create_dir(&dir.0).expect("create fixture directory");
        let spec: Spec = serde_json::from_value(serde_json::json!({
            "version":1,
            "opcodes":[
                {"name":"ReadLists","mnemonic":"read_lists","byte":"0x01","terminator":false,"flags":[],"operands":[
                    {"kind":"reg","role":"cond","encoding":"reg_u32_uleb","field":"cond","access":"read"},
                    {"kind":"reg_list","role":"args","encoding":"reg_list_u32_uleb_count_then_regs","field":"left","access":"read"},
                    {"kind":"reg_list","role":"args","encoding":"reg_list_u32_uleb_count_then_regs","field":"right","access":"read"}
                ]},
                {"name":"WriteLists","mnemonic":"write_lists","byte":"0x02","terminator":false,"flags":[],"operands":[
                    {"kind":"reg_list","role":"rets","encoding":"reg_list_u32_uleb_count_then_regs","field":"left","access":"write"},
                    {"kind":"reg_list","role":"rets","encoding":"reg_list_u32_uleb_count_then_regs","field":"right","access":"write"}
                ]},
                {"name":"Wide","mnemonic":"wide","byte":"0x03","terminator":false,"flags":[],"operands":[
                    {"kind":"reg","role":"src","encoding":"reg_u32_uleb","field":"a","access":"read"},
                    {"kind":"reg","role":"src","encoding":"reg_u32_uleb","field":"b","access":"read"},
                    {"kind":"reg","role":"src","encoding":"reg_u32_uleb","field":"c","access":"read"},
                    {"kind":"reg","role":"src","encoding":"reg_u32_uleb","field":"d","access":"read"},
                    {"kind":"reg","role":"dst","encoding":"reg_u32_uleb","field":"x","access":"write"},
                    {"kind":"reg","role":"dst","encoding":"reg_u32_uleb","field":"y","access":"write"}
                ]}
            ]
        })).expect("parse fixture schema");
        let generated = dir.0.join("generated.rs");
        fs::write(&generated, generate_reads_writes(spec, &generated).unwrap()).unwrap();
        let iterator = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../execution_tape/src/register_operands.rs")
            .canonicalize()
            .unwrap();
        let source = dir.0.join("main.rs");
        fs::write(&source,format!(r#"
            #[allow(dead_code, reason="fixture tests generated iteration")]
            #[path={iterator:?}] mod register_operands;
            mod bytecode {{
                use crate::register_operands::OperandIter;
                enum Instr {{
                    ReadLists{{cond:u32,left:Vec<u32>,right:Vec<u32>}},
                    WriteLists{{left:Vec<u32>,right:Vec<u32>}},
                    Wide{{a:u32,b:u32,c:u32,d:u32,x:u32,y:u32}},
                }}
                include!({generated:?});
                pub fn check() {{
                    assert_eq!(MAX_SCALAR_OPERANDS,4);
                    assert_eq!(MAX_REGISTER_LISTS,2);
                    for (left,right) in [(vec![],vec![9,10]),(vec![4,5],vec![]),(vec![4,5],vec![9,10]),(vec![],vec![])] {{
                        let read=Instr::ReadLists{{cond:2,left:left.clone(),right:right.clone()}};
                        let expected:Vec<_>=std::iter::once(2).chain(left.iter().copied()).chain(right.iter().copied()).collect();
                        assert_eq!(read.reads().collect::<Vec<_>>(),expected);
                        assert_eq!(read.writes().count(),0);
                        let mut partial=read.reads();assert_eq!(partial.next(),Some(2));
                        assert_eq!(partial.clone().collect::<Vec<_>>(),partial.collect::<Vec<_>>());
                        let write=Instr::WriteLists{{left:left.clone(),right:right.clone()}};
                        assert_eq!(write.reads().count(),0);
                        let expected:Vec<_>=left.into_iter().chain(right).collect();
                        let mut result=write.writes();assert_eq!(result.by_ref().collect::<Vec<_>>(),expected);
                        assert_eq!(result.next(),None);assert_eq!(result.next(),None);
                    }}
                    let wide=Instr::Wide{{a:1,b:2,c:3,d:4,x:5,y:6}};
                    assert_eq!(wide.reads().collect::<Vec<_>>(),vec![1,2,3,4]);
                    assert_eq!(wide.writes().collect::<Vec<_>>(),vec![5,6]);
                }}
            }}
            fn main(){{bytecode::check();}}
        "#)).unwrap();
        let binary = dir
            .0
            .join(format!("fixture{}", std::env::consts::EXE_SUFFIX));
        let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args(["--edition=2024", "-Dwarnings"])
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .output()
            .expect("compile fixture");
        assert!(
            output.status.success(),
            "fixture compile failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = Command::new(&binary).output().expect("execute fixture");
        assert!(
            output.status.success(),
            "fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn load_error(spec: serde_json::Value) -> String {
        let spec: Spec = serde_json::from_value(spec).expect("parse fixture schema");
        match load(spec) {
            Ok(_) => panic!("fixture should be rejected"),
            Err(e) => e.to_string(),
        }
    }

    fn op(name: &str, prefix: Option<&str>, byte: &str) -> serde_json::Value {
        let mut op = serde_json::json!({
            "name": name, "mnemonic": name.to_lowercase(), "byte": byte, "terminator": false,
            "flags": [], "doc": "Fixture.", "operands": []
        });
        if let Some(prefix) = prefix {
            op["prefix"] = prefix.into();
        }
        op
    }

    #[test]
    fn prefix_space_is_validated() {
        let prefix = serde_json::json!({"name": "Extended", "byte": "0xFC", "doc": "Fixture."});
        let reject = |prefixes: serde_json::Value, opcodes: Vec<serde_json::Value>| {
            load_error(serde_json::json!({"version": 1, "prefixes": prefixes, "opcodes": opcodes}))
        };
        assert!(
            reject(serde_json::json!([prefix]), vec![op("Lead", None, "0xF0")])
                .contains("reserved prefix space")
        );
        assert!(
            reject(
                serde_json::json!([prefix]),
                vec![op("Ext", Some("0xFD"), "0x00")]
            )
            .contains("undeclared prefix 0xFD")
        );
        assert!(
            reject(
                serde_json::json!([{"name": "Low", "byte": "0x80", "doc": "Fixture."}]),
                vec![]
            )
            .contains("below the reserved prefix space")
        );
        assert!(
            reject(
                serde_json::json!([prefix, {"name": "Other", "byte": "0xFC", "doc": "Fixture."}]),
                vec![]
            )
            .contains("duplicate prefix")
        );
        assert!(
            reject(
                serde_json::json!([prefix]),
                vec![op("A", Some("0xFC"), "0x00"), op("B", Some("0xFC"), "0x00")]
            )
            .contains("duplicate opcode 0xFC 0x00")
        );
        assert!(
            reject(
                serde_json::json!([prefix]),
                vec![op("A", None, "0x00"), op("A", Some("0xFC"), "0x00")]
            )
            .contains("duplicate opcode name")
        );

        // The same byte may be used once in each opcode space.
        let spec: Spec = serde_json::from_value(serde_json::json!({
            "version": 1,
            "prefixes": [prefix],
            "opcodes": [op("Single", None, "0x00"), op("Prefixed", Some("0xFC"), "0x00")]
        }))
        .expect("parse fixture schema");
        let codes: Vec<u16> = load(spec).unwrap().ops.iter().map(|(c, _)| *c).collect();
        assert_eq!(codes, [0x0000, 0xFC00]);
    }

    #[test]
    fn generated_file_is_up_to_date() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = workspace_root.parent().expect("workspace root");

        let spec_path = workspace_root.join("execution_tape/opcodes.json");
        let opcode_out_path = workspace_root.join("execution_tape/src/opcodes_gen.rs");
        let decode_out_path = workspace_root.join("execution_tape/src/bytecode_decode_gen.rs");
        let encode_out_path = workspace_root.join("execution_tape/src/bytecode_encode_gen.rs");
        let bytecode_instr_out_path =
            workspace_root.join("execution_tape/src/bytecode_instr_gen.rs");
        let instr_operands_out_path =
            workspace_root.join("execution_tape/src/instr_operands_gen.rs");
        let reads_writes_out_path =
            workspace_root.join("execution_tape/src/bytecode_reads_writes_gen.rs");

        let json = fs::read_to_string(&spec_path).expect("read opcodes.json");
        let spec: Spec = serde_json::from_str(&json).expect("parse opcodes.json");

        let opcode_rendered = generate(spec.clone(), &spec_path).expect("render opcodes_gen.rs");
        let decode_rendered = generate_bytecode_decode(spec.clone(), &spec_path)
            .expect("render bytecode_decode_gen.rs");
        let encode_rendered = generate_bytecode_encode(spec.clone(), &spec_path)
            .expect("render bytecode_encode_gen.rs");
        let bytecode_instr_rendered = generate_bytecode_instr_helpers(spec.clone(), &spec_path)
            .expect("render bytecode_instr_gen.rs");
        let instr_operands_rendered = generate_instr_operands(spec.clone(), &spec_path)
            .expect("render instr_operands_gen.rs");
        let reads_writes_rendered =
            generate_reads_writes(spec, &spec_path).expect("render bytecode_reads_writes_gen.rs");

        let opcode_existing = fs::read_to_string(&opcode_out_path).expect("read opcodes_gen.rs");
        let decode_existing =
            fs::read_to_string(&decode_out_path).expect("read bytecode_decode_gen.rs");
        let encode_existing =
            fs::read_to_string(&encode_out_path).expect("read bytecode_encode_gen.rs");
        let bytecode_instr_existing =
            fs::read_to_string(&bytecode_instr_out_path).expect("read bytecode_instr_gen.rs");
        let instr_operands_existing =
            fs::read_to_string(&instr_operands_out_path).expect("read instr_operands_gen.rs");
        let reads_writes_existing =
            fs::read_to_string(&reads_writes_out_path).expect("read bytecode_reads_writes_gen.rs");

        assert_eq_normalized("opcodes_gen.rs", &opcode_rendered, &opcode_existing);
        assert_eq_normalized("bytecode_decode_gen.rs", &decode_rendered, &decode_existing);
        assert_eq_normalized("bytecode_encode_gen.rs", &encode_rendered, &encode_existing);
        assert_eq_normalized(
            "bytecode_instr_gen.rs",
            &bytecode_instr_rendered,
            &bytecode_instr_existing,
        );
        assert_eq_normalized(
            "instr_operands_gen.rs",
            &instr_operands_rendered,
            &instr_operands_existing,
        );
        assert_eq_normalized(
            "bytecode_reads_writes_gen.rs",
            &reads_writes_rendered,
            &reads_writes_existing,
        );
    }
}
