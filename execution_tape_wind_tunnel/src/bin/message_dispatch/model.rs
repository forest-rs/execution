// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tiny compiled fixtures: source snapshots are supplied directly by this example frontend.

use execution_tape::{
    asm::{FunctionAbi, FunctionSig, ProgramBuilder},
    contract::Contract,
    host::HostSig,
    program::{HostTypeId, ValueType},
    source::{SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping},
    trace::TraceLocation,
    value::{FuncId, Obj, ObjHandle},
    verifier::VerifiedProgram,
};
use std::{error::Error, ops::Range, sync::Arc};

pub(super) type Failure = Box<dyn Error + Send + Sync>;
pub(super) type Result<T> = std::result::Result<T, Failure>;
pub(super) const CELL: HostTypeId = HostTypeId(1);
pub(super) fn receiver() -> Obj {
    Obj {
        host_type: CELL,
        handle: ObjHandle(42),
    }
}
pub(super) struct Compiled {
    pub(super) program: Arc<VerifiedProgram>,
    pub(super) entry: FuncId,
    pub(super) sources: SourceMap,
}
fn mapping(func: FuncId, pc: u32, range: Range<usize>) -> SourceMapping {
    SourceMapping {
        location: TraceLocation { func, pc },
        document: SourceId(0),
        range,
    }
}
fn fragment(source: &str, text: &str) -> Range<usize> {
    let start = source.find(text).expect("authored fixture fragment");
    start..start + text.len()
}
fn finish(
    b: ProgramBuilder,
    entry: FuncId,
    name: &str,
    text: String,
    mappings: Vec<SourceMapping>,
) -> Result<Compiled> {
    let program = Arc::new(b.build_verified()?);
    let sources = SourceMap::new(
        program.program(),
        vec![SourceDocument {
            name: name.into(),
            text,
        }],
        mappings,
        SourceLimits::default(),
    )?;
    Ok(Compiled {
        program,
        entry,
        sources,
    })
}
pub(super) fn caller(selector: &str) -> Result<Compiled> {
    let source = format!(
        "fn main(counter) {{\n    let first = counter.{selector}();\n    let second = counter.{selector}();\n    return (first, second);\n}}\n"
    );
    let mut b = ProgramBuilder::new();
    b.set_program_name("Guest message sender");
    let integer = b.contract(Contract::Kind(ValueType::I64))?;
    let answers = b.contract(Contract::Tuple(&[Some(integer), Some(integer)]))?;
    let entry = b.declare_function(FunctionAbi {
        args: vec![ValueType::Obj(CELL).into()],
        rets: vec![answers.into()],
    })?;
    let send = b.host_sig_for(
        "Object.send",
        HostSig {
            args: vec![ValueType::Obj(CELL), ValueType::Str],
            rets: vec![ValueType::I64],
        },
    );
    let name = b.const_str(selector);
    let mut a = b.function_asm(entry)?;
    let selector_reg = a.alloc_register(ValueType::Str);
    let first = a.alloc_register(ValueType::I64);
    let second = a.alloc_register(ValueType::I64);
    let tuple = a.alloc_register(ValueType::Agg);
    let mut mappings = vec![mapping(entry, a.pc(), fragment(&source, selector))];
    a.const_pool(selector_reg, name);
    let expression = format!("counter.{selector}()");
    mappings.push(mapping(entry, a.pc(), fragment(&source, &expression)));
    a.host_call_sig_id(0, send, 0, &[1, selector_reg], &[first]);
    let last = source.rfind(&expression).expect("second authored send");
    mappings.push(mapping(entry, a.pc(), last..last + expression.len()));
    a.host_call_sig_id(0, send, 0, &[1, selector_reg], &[second]);
    mappings.push(mapping(entry, a.pc(), fragment(&source, "(first, second)")));
    a.tuple_new(tuple, &[first, second]);
    mappings.push(mapping(
        entry,
        a.pc(),
        fragment(&source, "return (first, second);"),
    ));
    a.ret(0, &[tuple]);
    b.define_function(entry, a)?;
    b.set_function_name(entry, "main")?;
    b.set_function_input_name(entry, 0, "counter")?;
    finish(b, entry, "messages.expr", source, mappings)
}
pub(super) fn method(delta: i64) -> Result<Compiled> {
    let source =
        format!("fn increment(receiver) {{\n    return CounterCell.add(receiver, {delta});\n}}\n");
    let mut b = ProgramBuilder::new();
    b.set_program_name(&format!("Counter.increment · +{delta}"));
    let add = b.host_sig_for(
        "CounterCell.add",
        HostSig {
            args: vec![ValueType::Obj(CELL), ValueType::I64],
            rets: vec![ValueType::I64],
        },
    );
    let entry = b.declare_function(FunctionSig {
        arg_types: vec![ValueType::Obj(CELL)],
        ret_types: vec![ValueType::I64],
    })?;
    let mut a = b.function_asm(entry)?;
    let amount = a.alloc_register(ValueType::I64);
    let result = a.alloc_register(ValueType::I64);
    let mut mappings = vec![mapping(
        entry,
        a.pc(),
        fragment(&source, &delta.to_string()),
    )];
    a.const_i64(amount, delta);
    mappings.push(mapping(
        entry,
        a.pc(),
        fragment(&source, &format!("CounterCell.add(receiver, {delta})")),
    ));
    a.host_call_sig_id(0, add, 0, &[1, amount], &[result]);
    mappings.push(mapping(
        entry,
        a.pc(),
        fragment(
            &source,
            &format!("return CounterCell.add(receiver, {delta});"),
        ),
    ));
    a.ret(0, &[result]);
    b.define_function(entry, a)?;
    b.set_function_name(entry, "increment")?;
    b.set_function_input_name(entry, 0, "receiver")?;
    finish(b, entry, "counter.expr", source, mappings)
}
