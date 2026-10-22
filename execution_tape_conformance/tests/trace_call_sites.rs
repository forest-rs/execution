// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Actual guest call-site provenance survives ordinary returns and trap unwinding.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::{Program, ValueType};
use execution_tape::trace::{ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink};
use execution_tape::value::{FuncId, Value};
use execution_tape::vm::{Limits, Vm};

struct NeverHost;
impl Host for NeverHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        panic!("no host imports");
    }
}

#[derive(Default)]
struct Calls {
    entered: Vec<(ScopeKind, usize)>,
    exited: Vec<(ScopeKind, usize, TraceLocation, bool)>,
}
impl TraceSink for Calls {
    fn scope_enter(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        _: FuncId,
        _: u32,
        _: Option<u64>,
    ) {
        self.entered.push((kind, depth));
    }
    fn scope_exit(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        _: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        self.exited.push((
            kind,
            depth,
            TraceLocation { func, pc },
            matches!(outcome, TraceOutcome::Trap(_)),
        ));
    }
}

#[test]
fn direct_function_and_closure_calls_preserve_entry_provenance_on_every_exit() {
    for mode in 0..3 {
        for traps in [false, true] {
            let mut builder = ProgramBuilder::new();
            let closure = mode == 2;
            let mut leaf = Asm::new();
            let mut kinds = vec![ValueType::Unit];
            if closure {
                kinds.push(ValueType::Agg);
            }
            kinds.push(ValueType::I64);
            leaf.set_register_types(kinds);
            if traps {
                leaf.trap(42);
            } else {
                leaf.ret(0, &[if closure { 2 } else { 1 }]);
            }
            let leaf = builder
                .push_function_checked(
                    leaf,
                    FunctionSig {
                        arg_types: if closure {
                            vec![ValueType::Agg, ValueType::I64]
                        } else {
                            vec![ValueType::I64]
                        },
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            let sig = builder.call_sig(&[ValueType::I64], &[ValueType::I64]);
            let mut caller = Asm::new();
            caller.set_register_types(vec![
                ValueType::Unit,
                ValueType::I64,
                ValueType::Func,
                ValueType::Agg,
                ValueType::Closure,
                ValueType::I64,
            ]);
            caller.const_i64(1, 7);
            if mode == 0 {
                caller.call(0, leaf, 0, &[1], &[5]);
            } else {
                caller.const_func(2, leaf);
                if closure {
                    caller.tuple_new(3, &[]).closure_new(4, 2, 3);
                }
                caller.call_indirect(0, sig, if closure { 4 } else { 2 }, 0, &[1], &[5]);
            }
            caller.ret(0, &[5]);
            let caller = builder
                .push_function_checked(
                    caller,
                    FunctionSig {
                        arg_types: vec![],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            let program = builder.build_verified().unwrap();
            let call_pc = execution_tape::disasm::disassemble_function(program.program(), caller)
                .unwrap()
                .instrs()
                .find(|instruction| {
                    matches!(
                        instruction.operands(),
                        execution_tape::disasm::Operands::Call(_)
                    )
                })
                .unwrap()
                .pc();
            let mut sink = Calls::default();
            let result = Vm::new(NeverHost, Limits::default()).run(
                &program,
                caller,
                &[],
                TraceMask::CALL,
                Some(&mut sink),
            );
            assert_eq!(result.is_err(), traps);
            assert_eq!(
                sink.entered,
                [
                    (
                        ScopeKind::CallFrame {
                            func: caller,
                            caller: None
                        },
                        1
                    ),
                    (
                        ScopeKind::CallFrame {
                            func: leaf,
                            caller: Some(TraceLocation {
                                func: caller,
                                pc: call_pc
                            })
                        },
                        2
                    ),
                ]
            );
            assert_eq!(sink.exited.len(), 2);
            for (exit, entry) in sink.exited.iter().zip(sink.entered.iter().rev()) {
                assert_eq!((exit.0, exit.1), *entry);
                assert_eq!(exit.3, traps);
                if traps {
                    assert_eq!(exit.2.func, leaf);
                }
            }
        }
    }
}
