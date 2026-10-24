// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Bounded descriptions of recorded values and source operands, without guest-state inference.

use execution_tape::disasm::{CallTarget, ConstValue, InstrView, Operands};
use execution_tape::host::HostError;
use execution_tape::program::{HostSigId, Program, ValueType};
use execution_tape::value::{AggType, FuncId};
use execution_tape_recording::{AggregateId, RecordedValue, Recording};

pub(super) fn preview(text: &str, limit: usize) -> String {
    let mut output: String = text
        .chars()
        .take(limit)
        .flat_map(char::escape_debug)
        .collect();
    if text.chars().nth(limit).is_some() {
        output.push('…');
    }
    output
}

pub(super) fn function(program: &Program, id: FuncId) -> String {
    program
        .function_name(id.0)
        .map_or_else(|| format!("function #{}", id.0), |name| preview(name, 28))
}

pub(super) fn host(program: &Program, id: HostSigId) -> String {
    program
        .host_sig(id)
        .and_then(|entry| program.symbol_str(entry.symbol).ok())
        .map_or_else(|| format!("host #{}", id.0), |name| preview(name, 28))
}

pub(super) fn type_name(kind: ValueType) -> String {
    match kind {
        ValueType::Unit => "unit".into(),
        ValueType::Bool => "bool".into(),
        ValueType::I64 => "i64".into(),
        ValueType::U64 => "u64".into(),
        ValueType::F64 => "f64".into(),
        ValueType::Decimal => "decimal".into(),
        ValueType::Str => "text".into(),
        ValueType::Bytes => "bytes".into(),
        ValueType::Agg => "aggregate".into(),
        ValueType::Func => "function".into(),
        ValueType::Closure => "closure".into(),
        ValueType::Obj(id) => format!("object #{}", id.0),
    }
}

pub(super) fn types<'a>(
    values: impl IntoIterator<Item = &'a ValueType, IntoIter: ExactSizeIterator>,
) -> String {
    let values = values.into_iter();
    let count = values.len();
    let mut output = values
        .take(4)
        .map(|&kind| type_name(kind))
        .collect::<Vec<_>>()
        .join(", ");
    if count > 4 {
        output.push_str(&format!(", … +{}", count - 4));
    }
    format!("({output})")
}

pub(super) fn contracts(program: &Program, ids: &[execution_tape::contract::ContractId]) -> String {
    fn summary(program: &Program, id: execution_tape::contract::ContractId, depth: u8) -> String {
        use execution_tape::contract::Contract;
        let Some(contract) = program.contracts.get(id) else {
            return format!("<invalid contract #{}>", id.0);
        };
        if let Contract::Kind(kind) = contract {
            return type_name(kind);
        }
        if depth == 0 {
            return format!("aggregate #{}", id.0);
        }
        let members = |ids: &[Option<execution_tape::contract::ContractId>]| {
            let mut text = ids
                .iter()
                .take(4)
                .map(|id| id.map_or_else(|| "?".into(), |id| summary(program, id, depth - 1)))
                .collect::<Vec<_>>()
                .join(", ");
            if ids.len() > 4 {
                text.push_str(&format!(", … +{}", ids.len() - 4));
            }
            text
        };
        match contract {
            Contract::Kind(_) => unreachable!(),
            Contract::Tuple(ids) => format!("tuple({})", members(ids)),
            Contract::Struct { type_id, fields } => {
                let hidden = fields.len().saturating_sub(4);
                let fields: Vec<_> = fields.iter().take(4).copied().map(Some).collect();
                let mut preview = members(&fields);
                if hidden > 0 {
                    preview.push_str(&format!(", … +{hidden}"));
                }
                format!("struct #{}({preview})", type_id.0)
            }
            Contract::Array {
                elem_type_id,
                element,
            } => format!(
                "array #{}<{}>",
                elem_type_id.0,
                summary(program, element, depth - 1)
            ),
        }
    }
    let mut values = ids
        .iter()
        .take(4)
        .map(|&id| summary(program, id, 2))
        .collect::<Vec<_>>()
        .join(", ");
    if ids.len() > 4 {
        values.push_str(&format!(", … +{}", ids.len() - 4));
    }
    format!("({values})")
}

pub(super) fn signature(program: &Program, id: HostSigId) -> String {
    let Some(entry) = program.host_sig(id) else {
        return "signature unavailable".into();
    };
    format!(
        "{}{} → {}",
        host(program, id),
        program
            .host_sig_args(entry)
            .map(types)
            .unwrap_or_else(|_| "(<malformed argument types>)".into()),
        program
            .host_sig_rets(entry)
            .map(types)
            .unwrap_or_else(|_| "(<malformed return types>)".into())
    )
}

pub(super) fn host_error(error: &HostError) -> &'static str {
    match error {
        HostError::UnknownSymbol => "unknown symbol",
        HostError::SignatureMismatch => "signature mismatch",
        HostError::ProgramMismatch => "program mismatch",
        HostError::Failed => "host failed",
        HostError::Custom(_) => "typed host failure (payload retained)",
    }
}

pub(super) fn aggregate_kind(kind: AggType) -> &'static str {
    match kind {
        AggType::Tuple { .. } => "tuple",
        AggType::Struct { .. } => "struct",
        AggType::Array { .. } => "array",
    }
}

fn atom(record: &Recording, value: &RecordedValue, limit: usize) -> String {
    match value {
        RecordedValue::Unit => "()".into(),
        RecordedValue::Bool(value) => value.to_string(),
        RecordedValue::I64(value) => value.to_string(),
        RecordedValue::U64(value) => value.to_string(),
        RecordedValue::F64(bits) => {
            if f64::from_bits(*bits).is_nan() {
                format!("NaN [bits {bits:016x}]")
            } else {
                format!("{:?}", f64::from_bits(*bits))
            }
        }
        RecordedValue::Decimal(value) => format!("{}e-{}", value.mantissa, value.scale),
        RecordedValue::Str(value) => format!("\"{}\"", preview(value, limit)),
        RecordedValue::Bytes(value) => format!(
            "{} bytes [{}{}]",
            value.len(),
            value
                .iter()
                .take(6)
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" "),
            if value.len() > 6 { " …" } else { "" }
        ),
        RecordedValue::Obj(value) => {
            format!("object #{} token {}", value.host_type.0, value.handle.0)
        }
        RecordedValue::Func(id) => function(record.program().program(), *id),
        RecordedValue::Closure { func, env } => {
            format!(
                "{} [environment @{}]",
                function(record.program().program(), *func),
                env.get()
            )
        }
        RecordedValue::Agg(id) => format!(
            "{} @{}",
            record
                .aggregate(*id)
                .map_or("aggregate", |node| aggregate_kind(node.kind)),
            id.get()
        ),
    }
}

fn aggregate(record: &Recording, id: AggregateId) -> String {
    let Some(node) = record.aggregate(id) else {
        return format!("unavailable aggregate @{}", id.get());
    };
    let mut fields = node
        .values
        .iter()
        .take(3)
        .map(|value| atom(record, value, 12))
        .collect::<Vec<_>>()
        .join(", ");
    if node.values.len() > 3 {
        fields.push_str(&format!(", … +{} items", node.values.len() - 3));
    }
    match node.kind {
        AggType::Tuple { .. } => format!("({fields})"),
        AggType::Struct { type_id } => format!("struct #{} {{{fields}}}", type_id.0),
        AggType::Array { elem_type_id } => format!("array #{} [{fields}]", elem_type_id.0),
    }
}

pub(super) fn value(record: &Recording, value: &RecordedValue) -> String {
    if let RecordedValue::Str(text) = value {
        return format!("\"{}\" · text ({} bytes)", preview(text, 60), text.len());
    }
    if let RecordedValue::Agg(id) = value {
        return format!(
            "{} · {} @{}  ↵ open",
            aggregate(record, *id),
            record
                .aggregate(*id)
                .map_or("aggregate", |node| aggregate_kind(node.kind)),
            id.get()
        );
    }
    let kind = match value {
        RecordedValue::Unit => "unit",
        RecordedValue::Bool(_) => "bool",
        RecordedValue::I64(_) => "i64",
        RecordedValue::U64(_) => "u64",
        RecordedValue::F64(_) => "f64",
        RecordedValue::Decimal(_) => "decimal",
        RecordedValue::Str(_) => "text",
        RecordedValue::Bytes(_) => "bytes",
        RecordedValue::Obj(_) => "host object",
        RecordedValue::Func(_) => "function",
        RecordedValue::Closure { .. } => "closure · ↵ open environment",
        RecordedValue::Agg(_) => unreachable!(),
    };
    format!("{} · {kind}", atom(record, value, 60))
}

pub(super) fn arguments(record: &Recording, values: &[RecordedValue]) -> String {
    let mut output = values
        .iter()
        .take(2)
        .map(|value| match value {
            RecordedValue::Agg(id) => aggregate(record, *id),
            _ => atom(record, value, 16),
        })
        .collect::<Vec<_>>()
        .join(", ");
    if values.len() > 2 {
        output.push_str(&format!(", … +{}", values.len() - 2));
    }
    output
}

pub(super) fn values(record: &Recording, values: &[RecordedValue]) -> String {
    let contents = arguments(record, values);
    if values.len() == 1 {
        contents
    } else {
        format!("({contents})")
    }
}

fn registers(mut values: impl Iterator<Item = u32>) -> String {
    let visible = values
        .by_ref()
        .take(8)
        .map(|reg| format!("r{reg}"))
        .collect::<Vec<_>>()
        .join(", ");
    let omitted = values.count();
    if omitted == 0 {
        visible
    } else {
        format!("{visible}, … +{omitted} more")
    }
}

fn assignment(registers: &[u32], expression: String) -> String {
    if registers.is_empty() {
        expression
    } else {
        format!(
            "{} = {expression}",
            self::registers(registers.iter().copied())
        )
    }
}

pub(super) fn instruction(program: &Program, instruction: InstrView<'_>) -> String {
    match instruction.operands() {
        Operands::Call(call) => {
            let callee = match call.callee {
                CallTarget::HostSig(id, _) => format!("host {}", host(program, id)),
                CallTarget::Func(id) => format!("call {}", function(program, id)),
                CallTarget::Indirect { callee, call_sig } => {
                    format!("call r{callee} [signature #{}]", call_sig.0)
                }
            };
            assignment(
                call.rets,
                format!("{callee}({})", registers(call.args.iter().copied())),
            )
        }
        Operands::Ret { rets, .. } => format!("return {}", registers(rets.iter().copied())),
        Operands::Jmp { pc_target } => format!("goto {pc_target:06}"),
        Operands::Br {
            cond,
            pc_true,
            pc_false,
        } => format!("if r{cond} goto {pc_true:06} else {pc_false:06}"),
        Operands::Block { params } => format!("join ({})", registers(params.iter().copied())),
        Operands::JmpArgs { pc_target, args } => {
            format!("goto {pc_target:06} ({})", registers(args.iter().copied()))
        }
        Operands::BrArgs {
            cond,
            pc_true,
            pc_false,
            ..
        } => format!(
            "if r{cond} goto {pc_true:06} ({}) else {pc_false:06} ({})",
            registers(
                instruction
                    .edge(0)
                    .into_iter()
                    .flat_map(|edge| edge.args.iter().copied())
            ),
            registers(
                instruction
                    .edge(1)
                    .into_iter()
                    .flat_map(|edge| edge.args.iter().copied())
            )
        ),
        Operands::Simple => {
            let expression = if let Some(value) = instruction.const_value() {
                match value {
                    ConstValue::Unit => "()".into(),
                    ConstValue::Bool(value) => value.to_string(),
                    ConstValue::I64(value) => value.to_string(),
                    ConstValue::U64(value) => format!("{value}u64"),
                    ConstValue::F64Bits(bits) => {
                        format!("{:?}f64 [bits {bits:016x}]", f64::from_bits(bits))
                    }
                    ConstValue::Decimal { mantissa, scale } => format!("{mantissa}e-{scale}"),
                    ConstValue::Str(value) => format!("\"{}\"", preview(value, 32)),
                    ConstValue::Bytes(value) => format!("bytes[{}]", value.len()),
                }
            } else {
                let immediate = instruction
                    .input_index()
                    .map_or_else(String::new, |index| format!(" [{index}]"));
                format!(
                    "{:?}({}){immediate}",
                    instruction.opcode(),
                    registers(instruction.reads())
                )
            };
            let mut writes = instruction.writes();
            let first = writes.next();
            match (first, writes.next()) {
                (None, _) => expression,
                (Some(reg), None) => format!("r{reg} = {expression}"),
                _ => format!("{} = {expression}", registers(instruction.writes())),
            }
        }
    }
}

/// Format a diagnostic without allocating an arbitrarily large aggregate-shape description.
pub(super) fn diagnostic(value: &impl std::fmt::Display) -> String {
    use std::fmt::Write;
    struct Bounded {
        text: String,
        remaining: usize,
    }
    impl Write for Bounded {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            for ch in text.chars() {
                if self.remaining == 0 {
                    return Err(std::fmt::Error);
                }
                self.text.push(ch);
                self.remaining -= 1;
            }
            Ok(())
        }
    }
    let mut output = Bounded {
        text: String::new(),
        remaining: 600,
    };
    if write!(output, "{value}").is_err() {
        output.text.push('…');
    }
    preview(&output.text, 601)
}
