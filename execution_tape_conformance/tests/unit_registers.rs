// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Unit has no runtime payload; verification and source observations still distinguish registers.

use std::sync::Arc;

use execution_tape::{
    asm::{BuildError, FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    opcode::Opcode,
    program::{Const, Program, ValueType},
    recording::{RecordLimits, Recorder, ReplayOutcome},
    trace::{RegisterEvent, RegisterPhase, TraceMask, TraceSink},
    value::{FuncId, Value},
    verifier::{VerifiedProgram, VerifyError},
    vm::{Limits, Trap, Vm},
};

struct UnitHost {
    bad_return: bool,
}
impl Host for UnitHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        assert_eq!(
            args.len(),
            10,
            "all repeated Unit arguments must reach the host"
        );
        assert!(
            args.iter().all(|value| matches!(value, ValueRef::Unit)),
            "host arguments must preserve Unit kinds"
        );
        rets.fill(Value::Unit);
        if self.bad_return {
            rets[9] = Value::Bool(true);
        }
        Ok(0)
    }
}

#[derive(Default)]
struct Observe {
    instructions: Vec<(u32, u8)>,
    writes: Vec<(u32, Value)>,
}
impl TraceSink for Observe {
    fn instr(&mut self, _: &Program, _: FuncId, pc: u32, _: u32, _: Option<u64>, opcode: u8) {
        self.instructions.push((pc, opcode));
    }
    fn registers(&mut self, _: &Program, event: RegisterEvent<'_>) {
        if event.phase == RegisterPhase::Outputs {
            self.writes.extend(
                event
                    .values
                    .iter()
                    .map(|(reg, value)| (reg, value.unwrap().to_value())),
            );
        }
    }
}

fn operations() -> VerifiedProgram {
    let mut builder = ProgramBuilder::new();
    let pool = builder.constant(Const::Unit);
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Bool],
            ret_types: vec![ValueType::Unit; 3],
        })
        .unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let constant = body.alloc_register(ValueType::Unit);
    let pooled = body.alloc_register(ValueType::Unit);
    let copied = body.alloc_register(ValueType::Unit);
    let selected = body.alloc_register(ValueType::Unit);
    body.const_unit(constant)
        .const_pool(pooled, pool)
        .mov(copied, constant)
        .select(selected, 1, pooled, copied)
        .ret(0, &[selected, constant, pooled]);
    builder.define_function(entry, body).unwrap();
    builder.build_verified().unwrap()
}

#[test]
fn unit_operations_preserve_source_instructions_registers_and_fuel() {
    let program = operations();
    let source = program.program();
    let instructions: Vec<_> = program
        .function(FuncId(0))
        .unwrap()
        .instructions()
        .collect();
    assert_eq!(instructions.len(), 5);
    let expected_opcodes = [
        Opcode::ConstUnit,
        Opcode::ConstPool,
        Opcode::Mov,
        Opcode::Select,
        Opcode::Ret,
    ];
    assert_eq!(
        instructions.iter().map(|i| i.opcode).collect::<Vec<_>>(),
        expected_opcodes
    );
    assert_eq!(
        program.function(FuncId(0)).unwrap().stats().register_bytes,
        size_of::<bool>()
    );
    for condition in [false, true] {
        let mut observe = Observe::default();
        let mut vm = Vm::new(
            UnitHost { bad_return: false },
            Limits {
                fuel: 5,
                ..Limits::default()
            },
        );
        let result = vm
            .run(
                &program,
                FuncId(0),
                &[Value::Bool(condition)],
                TraceMask::INSTR | TraceMask::REGISTERS,
                Some(&mut observe),
            )
            .unwrap();
        assert_eq!(result, vec![Value::Unit; 3]);
        assert_eq!(
            observe.instructions,
            instructions
                .iter()
                .map(|i| (
                    i.pc,
                    source.functions[0].bytecode(source).unwrap()[i.pc as usize]
                ))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            observe.writes,
            [
                (2, Value::Unit),
                (3, Value::Unit),
                (4, Value::Unit),
                (5, Value::Unit)
            ]
        );
        let error = Vm::new(
            UnitHost { bad_return: false },
            Limits {
                fuel: 4,
                ..Limits::default()
            },
        )
        .run(
            &program,
            FuncId(0),
            &[Value::Bool(condition)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
        assert!(matches!(error.trap, Trap::FuelExceeded));
        assert_eq!(error.pc, instructions[4].pc);
    }
}

#[test]
fn unit_declarations_and_unselected_operands_still_require_initialization() {
    for select in [false, true] {
        let mut builder = ProgramBuilder::new();
        let entry = builder
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::Bool],
                ret_types: vec![ValueType::Unit],
            })
            .unwrap();
        let mut body = builder.function_asm(entry).unwrap();
        let uninitialized = body.alloc_register(ValueType::Unit);
        if select {
            let initialized = body.alloc_register(ValueType::Unit);
            let result = body.alloc_register(ValueType::Unit);
            body.const_unit(initialized)
                .select(result, 1, initialized, uninitialized)
                .ret(0, &[result]);
        } else {
            body.ret(0, &[uninitialized]);
        }
        builder.define_function(entry, body).unwrap();
        assert!(matches!(
            builder.build_verified(),
            Err(BuildError::Verify(VerifyError::UninitializedRead {
                reg: 2,
                ..
            }))
        ));
    }
}

#[test]
fn repeated_guest_host_unit_boundaries_validate_kinds_and_replay() {
    let mut builder = ProgramBuilder::new();
    let sig = FunctionSig {
        arg_types: vec![ValueType::Unit; 10],
        ret_types: vec![ValueType::Unit; 10],
    };
    let entry = builder.declare_function(sig.clone()).unwrap();
    let helper = builder.declare_function(sig).unwrap();
    let host = builder.host_sig_for(
        "units",
        HostSig {
            args: vec![ValueType::Unit; 10],
            rets: vec![ValueType::Unit; 10],
        },
    );
    let registers: Vec<_> = (1..=10).collect();
    let mut body = builder.function_asm(helper).unwrap();
    body.ret(0, &registers);
    builder.define_function(helper, body).unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    body.call(0, helper, 0, &registers, &registers)
        .host_call_sig_id(0, host, 0, &registers, &registers)
        .ret(0, &registers);
    builder.define_function(entry, body).unwrap();
    let program = Arc::new(builder.build_verified().unwrap());
    for bad_return in [false, true] {
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        let mut vm = Vm::new(UnitHost { bad_return }, Limits::default());
        let result = vm.run(
            &program,
            entry,
            &vec![Value::Unit; 10],
            recorder.mask(),
            Some(&mut recorder),
        );
        let recording = recorder.finish();
        drop(vm);
        if bad_return {
            assert!(matches!(
                result.unwrap_err().trap,
                Trap::TypeMismatch {
                    expected: ValueType::Unit,
                    actual: ValueType::Bool
                }
            ));
            assert!(matches!(recording.replay(), Ok(ReplayOutcome::Trapped(_))));
        } else {
            assert_eq!(result.unwrap(), vec![Value::Unit; 10]);
            assert!(matches!(recording.replay(), Ok(ReplayOutcome::Returned(_))));
        }
    }
    let mut bad_args = vec![Value::Unit; 10];
    bad_args[9] = Value::Bool(true);
    let error = Vm::new(UnitHost { bad_return: false }, Limits::default())
        .run(&program, entry, &bad_args, TraceMask::NONE, None)
        .unwrap_err();
    assert!(matches!(
        error.trap,
        Trap::TypeMismatch {
            expected: ValueType::Unit,
            actual: ValueType::Bool
        }
    ));
}
