// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tiny compiled fixtures: source snapshots are supplied directly by this example frontend.

use execution_tape::{
    asm::{FunctionAbi, FunctionSig, ProgramBuilder},
    contract::Contract,
    host::HostSig,
    program::{HostTypeId, ValueType},
    source::{SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping},
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
    let counter = b.argument(entry, 0)?;
    a.source(SourceId(0), fragment(&source, selector))
        .const_pool(selector_reg, name);
    let expression = format!("counter.{selector}()");
    a.source(SourceId(0), fragment(&source, &expression));
    let [first] = b.emit_call(&mut a, send, &[counter, selector_reg])?;
    let last = source.rfind(&expression).expect("second authored send");
    a.source(SourceId(0), last..last + expression.len());
    let [second] = b.emit_call(&mut a, send, &[counter, selector_reg])?;
    let tuple = a.alloc_register(ValueType::Agg);
    a.source(SourceId(0), fragment(&source, "(first, second)"))
        .tuple_new(tuple, &[first, second]);
    a.source(SourceId(0), fragment(&source, "return (first, second);"))
        .ret(0, &[tuple]);
    let mappings = a.source_mappings(entry).collect();
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
    let receiver = b.argument(entry, 0)?;
    a.source(SourceId(0), fragment(&source, &delta.to_string()))
        .const_i64(amount, delta);
    a.source(
        SourceId(0),
        fragment(&source, &format!("CounterCell.add(receiver, {delta})")),
    );
    let [result] = b.emit_call(&mut a, add, &[receiver, amount])?;
    a.source(
        SourceId(0),
        fragment(
            &source,
            &format!("return CounterCell.add(receiver, {delta});"),
        ),
    )
    .ret(0, &[result]);
    let mappings = a.source_mappings(entry).collect();
    b.define_function(entry, a)?;
    b.set_function_name(entry, "increment")?;
    b.set_function_input_name(entry, 0, "receiver")?;
    finish(b, entry, "counter.expr", source, mappings)
}
