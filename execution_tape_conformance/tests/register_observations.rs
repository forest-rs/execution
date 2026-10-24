// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Borrowed register observations are explicit and never expose unwritten destinations.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::{Program, ValueType};
use execution_tape::trace::{RegisterEvent, RegisterPhase, TraceLocation, TraceMask, TraceSink};
use execution_tape::value::{FuncId, Value};
use execution_tape::vm::{Limits, Vm};

struct NoHost;
impl Host for NoHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        Err(HostError::UnknownSymbol)
    }
}
type Observation = (u32, RegisterPhase, Vec<(u32, Value)>);

#[derive(Default)]
struct Observe {
    masks: Vec<TraceMask>,
    events: Vec<Observation>,
    selected: Option<u32>,
}
impl TraceSink for Observe {
    fn run_mask(&mut self, _: &Program, mask: TraceMask) {
        self.masks.push(mask);
    }
    fn wants_registers(&self, location: TraceLocation, _: RegisterPhase) -> bool {
        self.selected.is_none_or(|pc| location.pc == pc)
    }
    fn registers(&mut self, _: &Program, event: RegisterEvent<'_>) {
        self.events.push((
            event.location.pc,
            event.phase,
            event
                .values
                .iter()
                .map(|(reg, value)| (reg, value.expect("available operand").to_value()))
                .collect(),
        ));
    }
}
#[test]
fn aliases_borrow_old_inputs_and_new_outputs_with_exact_scalar_and_text_values() {
    let mut b = ProgramBuilder::new();
    let mut a = Asm::new();
    a.set_register_types(vec![
        ValueType::Unit,
        ValueType::Str,
        ValueType::Str,
        ValueType::U64,
    ]);
    a.mov(2, 1).str_concat(1, 1, 2).str_len(3, 1).ret(0, &[1]);
    b.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![ValueType::Str],
            ret_types: vec![ValueType::Str],
        },
    )
    .unwrap();
    let p = b.build_verified().unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut sink = Observe::default();
    let result = vm
        .run(
            &p,
            FuncId(0),
            &[Value::Str("hello".into())],
            TraceMask::REGISTERS,
            Some(&mut sink),
        )
        .unwrap();
    assert_eq!(result, vec![Value::Str("hellohello".into())]);
    assert_eq!(sink.masks, vec![TraceMask::REGISTERS]);
    assert_eq!(sink.events.len(), 7); // three local operations, then only Ret inputs
    assert_eq!(sink.events[0].2, vec![(1, Value::Str("hello".into()))]);
    assert_eq!(
        sink.events[2].2,
        vec![
            (1, Value::Str("hello".into())),
            (2, Value::Str("hello".into()))
        ]
    );
    assert_eq!(sink.events[3].2, vec![(1, Value::Str("hellohello".into()))]);
    assert_eq!(sink.events[5].2, vec![(3, Value::U64(10))]);
    assert_eq!(sink.events[6].1, RegisterPhase::Inputs);
    assert!(
        sink.events
            .iter()
            .flat_map(|(_, _, values)| values)
            .all(|(reg, _)| *reg != 0)
    );
    sink.events.clear();
    sink.selected = Some(sink_selected_pc(&p));
    vm.run(
        &p,
        FuncId(0),
        &[Value::Str("x".into())],
        TraceMask::NONE,
        Some(&mut sink),
    )
    .unwrap();
    assert!(sink.events.is_empty());
    assert_eq!(sink.masks.len(), 1);
    vm.run(
        &p,
        FuncId(0),
        &[Value::Str("x".into())],
        TraceMask::REGISTERS,
        Some(&mut sink),
    )
    .unwrap();
    assert_eq!(sink.events.len(), 2);
    assert!(
        sink.events
            .iter()
            .all(|(pc, _, _)| Some(*pc) == sink.selected)
    );
}
fn sink_selected_pc(p: &execution_tape::verifier::VerifiedProgram) -> u32 {
    execution_tape::disasm::disassemble_function(p.program(), FuncId(0))
        .unwrap()
        .instrs()
        .nth(1)
        .unwrap()
        .pc()
}
