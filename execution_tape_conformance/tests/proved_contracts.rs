// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Boundaries whose contracts the verifier proved are not checked again; every other boundary
//! still is.

use execution_tape::asm::{Asm, FunctionAbi, FunctionSig, ProgramBuilder, Reg};
use execution_tape::contract::{Contract, ContractId};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::opcode::Opcode;
use execution_tape::program::ValueType;
use execution_tape::value::{Closure, FuncId, FuncRef, UnitId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, RunStatus, Trap, TurnOptions, TurnOutcome, Vm};
use std::sync::Arc;

/// Returns a one-element tuple holding `self.0`, or a closure over one when `self.1` names a
/// body.
struct Maker(Value, Option<FuncRef>);
impl Host for Maker {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        rets: &mut [Value],
        mut ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let env = ctx.tuple_new(vec![self.0.clone()])?;
        rets[0] = match self.1 {
            Some(func) => Value::Closure(Closure { func, env }),
            None => Value::Agg(env),
        };
        Ok(0)
    }
}

fn body(types: &[ValueType]) -> Asm {
    let mut asm = Asm::new();
    asm.set_register_types(types.to_vec());
    asm
}

/// `entry() -> i64` gets a `(i64)` tuple, from the host or made locally, and passes it down a
/// chain of `depth` functions taking and returning it, then reads its member.
fn chain(depth: usize, from_host: bool) -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let pair: ContractId = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let make = b.host_sig_for(
        "make",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Agg],
        },
    );
    let abi = FunctionAbi {
        args: vec![pair.into()],
        rets: vec![pair.into()],
    };
    let mut next = None;
    for _ in 0..depth {
        let mut link = body(&[ValueType::Unit, ValueType::Agg, ValueType::Agg]);
        match next {
            Some(next) => link.call(Reg::EFFECT, next, Reg::EFFECT, &[Reg(1)], &[Reg(2)]),
            None => link.mov(Reg(2), Reg(1)),
        };
        link.ret(Reg::EFFECT, &[Reg(2)]);
        next = Some(b.push_function_checked(link, abi.clone()).unwrap());
    }
    let mut main = body(&[
        ValueType::Unit,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::I64,
    ]);
    if from_host {
        main.host_call(Reg::EFFECT, make, Reg::EFFECT, &[], &[Reg(1)]);
    } else {
        main.const_i64(Reg(3), 7).tuple_new(Reg(1), &[Reg(3)]);
    }
    main.call(
        Reg::EFFECT,
        next.expect("a chain"),
        Reg::EFFECT,
        &[Reg(1)],
        &[Reg(2)],
    )
    .tuple_get(Reg(3), Reg(2), 0)
    .ret(Reg::EFFECT, &[Reg(3)]);
    let entry = b
        .push_function_checked(
            main,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

/// Runs `entry` in one turn and returns its result, or its trap, and the contract work spent.
fn run(host: Maker, program: Arc<VerifiedProgram>, entry: FuncId) -> (Result<Value, Trap>, u64) {
    let mut vm = Vm::new(host, Limits::default());
    let unit = vm.install(program).unwrap();
    let mut turn = vm
        .start(FuncRef::new(unit, entry), &[], TurnOptions::default())
        .unwrap();
    let outcome = turn.drive(u64::MAX);
    let work = turn.work().contract;
    match (outcome, turn.finish()) {
        (Ok(RunStatus::Complete), Ok(TurnOutcome::Returned(values))) => {
            (Ok(values[0].clone()), work)
        }
        (Err(trap), _) => (Err(trap.trap), work),
        _ => panic!("the turn ends"),
    }
}

#[test]
fn values_proved_to_satisfy_their_contracts_are_not_checked_again() {
    for depth in [1, 8] {
        let (program, entry) = chain(depth, false);
        let (result, work) = run(Maker(Value::I64(7), None), program, entry);
        assert_eq!(result.unwrap(), Value::I64(7));
        assert_eq!(work, 0, "a local tuple is proved at every call and return");
    }
}

#[test]
fn an_unproved_value_is_checked_once_where_it_enters() {
    let (shallow, entry) = chain(1, true);
    let (result, once) = run(Maker(Value::I64(7), None), shallow, entry);
    assert_eq!(result.unwrap(), Value::I64(7));
    assert!(once > 0, "the host's tuple is checked at the first call");
    // Deeper calls take it from a parameter whose contract was checked, so they are proved.
    let (deep, entry) = chain(8, true);
    let proved = |func: FuncId| {
        deep.function(func)
            .unwrap()
            .instructions()
            .filter(|instruction| instruction.opcode == Opcode::Call)
            .map(|instruction| instruction.boundary_proved)
            .collect::<Vec<_>>()
    };
    assert_eq!(proved(entry), [false]);
    assert!((0..entry.0).all(|func| proved(FuncId(func)).iter().all(|&proved| proved)));
    let (result, work) = run(Maker(Value::I64(7), None), deep, entry);
    assert_eq!(result.unwrap(), Value::I64(7));
    assert_eq!(work, once);
}

#[test]
fn an_unproved_value_that_breaks_its_contract_still_traps() {
    let (program, entry) = chain(8, true);
    let (result, _) = run(Maker(Value::Str("seven".into()), None), program, entry);
    assert!(matches!(result, Err(Trap::ContractMismatch { .. })));
}

#[test]
fn a_closure_environment_is_checked_even_when_the_arguments_are_proved() {
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let environment = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let make = b.host_sig_for(
        "make",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Closure],
        },
    );
    let mut closure = body(&[ValueType::Unit, ValueType::Agg, ValueType::I64]);
    closure.ret(Reg::EFFECT, &[Reg(2)]);
    let closure = b
        .push_function_checked(
            closure,
            FunctionAbi {
                args: vec![environment.into(), integer.into()],
                rets: vec![integer.into()],
            },
        )
        .unwrap();
    let sig = b
        .call_sig(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut main = body(&[ValueType::Unit, ValueType::Closure, ValueType::I64]);
    main.host_call(Reg::EFFECT, make, Reg::EFFECT, &[], &[Reg(1)])
        .const_i64(Reg(2), 3)
        .call_indirect(Reg::EFFECT, sig, Reg(1), Reg::EFFECT, &[Reg(2)], &[Reg(2)])
        .ret(Reg::EFFECT, &[Reg(2)]);
    let entry = b
        .push_function_checked(
            main,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let body = Some(FuncRef::new(UnitId(0), closure));
    let (result, _) = run(Maker(Value::I64(1), body), Arc::clone(&program), entry);
    assert_eq!(result.unwrap(), Value::I64(3));
    // The host made this closure's environment, so no fact proves it.
    let (result, _) = run(Maker(Value::Bool(true), body), program, entry);
    assert!(matches!(result, Err(Trap::ContractMismatch { .. })));
}

#[test]
fn a_nested_member_no_fact_knows_is_still_checked() {
    // `entry` wraps the host's tuple in a local one, whose own shape is known but whose member
    // is not, and passes it to a function requiring `((i64))`.
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let inner = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let outer = b.contract(Contract::Tuple(&[Some(inner)])).unwrap();
    let make = b.host_sig_for(
        "make",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Agg],
        },
    );
    let mut read = body(&[
        ValueType::Unit,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::I64,
    ]);
    read.tuple_get(Reg(2), Reg(1), 0)
        .tuple_get(Reg(3), Reg(2), 0)
        .ret(Reg::EFFECT, &[Reg(3)]);
    let read = b
        .push_function_checked(
            read,
            FunctionAbi {
                args: vec![outer.into()],
                rets: vec![integer.into()],
            },
        )
        .unwrap();
    let mut main = body(&[
        ValueType::Unit,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::I64,
    ]);
    main.host_call(Reg::EFFECT, make, Reg::EFFECT, &[], &[Reg(1)])
        .tuple_new(Reg(2), &[Reg(1)])
        .call(Reg::EFFECT, read, Reg::EFFECT, &[Reg(2)], &[Reg(3)])
        .ret(Reg::EFFECT, &[Reg(3)]);
    let entry = b
        .push_function_checked(
            main,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let (result, _) = run(Maker(Value::I64(5), None), Arc::clone(&program), entry);
    assert_eq!(result.unwrap(), Value::I64(5));
    let (result, _) = run(Maker(Value::Str("five".into()), None), program, entry);
    assert!(matches!(result, Err(Trap::ContractMismatch { .. })));
}

/// The source of the `(i64)` tuple a path passes or returns.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Source {
    /// Made by the guest, so proved.
    Local,
    /// Returned by the host, so checked where it crosses.
    Host,
}

/// What every path test builds on: the `(i64)` contract, the host import that makes a tuple,
/// and a function `read(t: (i64)) -> i64` returning its member.
struct Parts {
    integer: ContractId,
    single: ContractId,
    make: execution_tape::program::HostSigId,
    read: FuncId,
}

fn parts(b: &mut ProgramBuilder) -> Parts {
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let single = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let make = b.host_sig_for(
        "make",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Agg],
        },
    );
    let mut read = body(&[ValueType::Unit, ValueType::Agg, ValueType::I64]);
    read.tuple_get(Reg(2), Reg(1), 0)
        .ret(Reg::EFFECT, &[Reg(2)]);
    let read = b
        .push_function_checked(
            read,
            FunctionAbi {
                args: vec![single.into()],
                rets: vec![integer.into()],
            },
        )
        .unwrap();
    Parts {
        integer,
        single,
        make,
        read,
    }
}

/// Puts a `(i64)` tuple from `source` in `dst`, using `scratch`, an `i64` register.
fn tuple_from(asm: &mut Asm, parts: &Parts, source: Source, dst: Reg, scratch: Reg) {
    match source {
        Source::Local => {
            asm.const_i64(scratch, 7).tuple_new(dst, &[scratch]);
        }
        Source::Host => {
            asm.host_call(Reg::EFFECT, parts.make, Reg::EFFECT, &[], &[dst]);
        }
    }
}

/// Whether each instruction of `func` with `opcode` is a proved boundary.
fn proofs(program: &VerifiedProgram, func: FuncId, opcode: Opcode) -> Vec<bool> {
    program
        .function(func)
        .unwrap()
        .instructions()
        .filter(|instruction| instruction.opcode == opcode)
        .map(|instruction| instruction.boundary_proved)
        .collect()
}

/// Runs a program built for each source: a local tuple and a good host tuple read 7, and a host
/// tuple of the wrong member kind traps on its contract.
fn check_path(build: impl Fn(Source) -> (Arc<VerifiedProgram>, FuncId, Vec<bool>)) {
    for source in [Source::Local, Source::Host] {
        let (program, entry, proved) = build(source);
        assert_eq!(
            proved.iter().all(|&proved| proved),
            source == Source::Local,
            "{source:?} {proved:?}"
        );
        let (result, _) = run(Maker(Value::I64(7), None), Arc::clone(&program), entry);
        assert_eq!(result.unwrap(), Value::I64(7), "{source:?}");
        if source == Source::Host {
            let (result, _) = run(Maker(Value::Str("seven".into()), None), program, entry);
            assert!(
                matches!(result, Err(Trap::ContractMismatch { .. })),
                "{result:?}"
            );
        }
    }
}

fn entry_abi() -> FunctionSig {
    FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    }
}

#[test]
fn an_indirect_call_through_a_function_value_skips_only_proved_arguments() {
    check_path(|source| {
        let mut b = ProgramBuilder::new();
        let parts = parts(&mut b);
        let sig = b
            .call_sig(FunctionAbi {
                args: vec![parts.single.into()],
                rets: vec![parts.integer.into()],
            })
            .unwrap();
        let mut main = body(&[
            ValueType::Unit,
            ValueType::Agg,
            ValueType::I64,
            ValueType::Func,
        ]);
        tuple_from(&mut main, &parts, source, Reg(1), Reg(2));
        main.const_func(Reg(3), parts.read)
            .call_indirect(Reg::EFFECT, sig, Reg(3), Reg::EFFECT, &[Reg(1)], &[Reg(2)])
            .ret(Reg::EFFECT, &[Reg(2)]);
        let entry = b.push_function_checked(main, entry_abi()).unwrap();
        let program = Arc::new(b.build_verified().unwrap());
        let proved = proofs(&program, entry, Opcode::CallIndirect);
        assert_eq!(proved.len(), 1);
        (program, entry, proved)
    });
}

#[test]
fn a_linked_call_relinked_to_another_unit_skips_only_proved_arguments() {
    for source in [Source::Local, Source::Host] {
        let mut b = ProgramBuilder::new();
        let parts = parts(&mut b);
        let link = b.declare_link("read", parts.read).unwrap();
        let mut main = body(&[ValueType::Unit, ValueType::Agg, ValueType::I64]);
        tuple_from(&mut main, &parts, source, Reg(1), Reg(2));
        main.call_link(Reg::EFFECT, link, Reg::EFFECT, &[Reg(1)], &[Reg(2)])
            .ret(Reg::EFFECT, &[Reg(2)]);
        let entry = b.push_function_checked(main, entry_abi()).unwrap();
        let program = Arc::new(b.build_verified().unwrap());
        assert_eq!(
            proofs(&program, entry, Opcode::CallLink),
            [source == Source::Local]
        );
        for (value, good) in [(Value::I64(7), true), (Value::Str("seven".into()), false)] {
            let mut vm = Vm::new(Maker(value, None), Limits::default());
            let caller = vm.install(Arc::clone(&program)).unwrap();
            let other = vm.install(Arc::clone(&program)).unwrap();
            vm.relink(caller, link, FuncRef::new(other, parts.read))
                .unwrap();
            let result = vm.run(
                FuncRef::new(caller, entry),
                &[],
                execution_tape::trace::TraceMask::NONE,
                None,
            );
            match (source, good) {
                (Source::Local, _) | (Source::Host, true) => {
                    assert_eq!(result.unwrap(), [Value::I64(7)]);
                }
                (Source::Host, false) => {
                    let trap = result.unwrap_err();
                    assert!(matches!(trap.trap, Trap::ContractMismatch { .. }));
                    // Entering the other unit's `read` refused it, at the call.
                    assert_eq!((trap.unit, trap.func), (caller, entry));
                }
            }
        }
    }
}

#[test]
fn a_continuation_root_returns_without_checking_only_proved_results() {
    check_path(|source| {
        let mut b = ProgramBuilder::new();
        let parts = parts(&mut b);
        // produce() -> (i64) runs as a continuation's root.
        let mut produce = body(&[ValueType::Unit, ValueType::Agg, ValueType::I64]);
        tuple_from(&mut produce, &parts, source, Reg(1), Reg(2));
        produce.ret(Reg::EFFECT, &[Reg(1)]);
        let produce = b
            .push_function_checked(
                produce,
                FunctionAbi {
                    args: vec![],
                    rets: vec![parts.single.into()],
                },
            )
            .unwrap();
        let sig = b
            .call_sig(FunctionAbi {
                args: vec![],
                rets: vec![parts.single.into()],
            })
            .unwrap();
        let mut main = body(&[
            ValueType::Unit,
            ValueType::Func,
            ValueType::Cont,
            ValueType::Agg,
            ValueType::I64,
        ]);
        main.const_func(Reg(1), produce)
            .cont_new(Reg(2), sig, Reg(1))
            .resume(Reg::EFFECT, sig, Reg(2), Reg::EFFECT, &[], &[Reg(3)])
            .tuple_get(Reg(4), Reg(3), 0)
            .ret(Reg::EFFECT, &[Reg(4)]);
        let entry = b.push_function_checked(main, entry_abi()).unwrap();
        let program = Arc::new(b.build_verified().unwrap());
        let proved = proofs(&program, produce, Opcode::Ret);
        (program, entry, proved)
    });
}

#[test]
fn a_callback_return_to_its_host_request_is_always_checked() {
    for source in [Source::Local, Source::Host] {
        let mut b = ProgramBuilder::new();
        let parts = parts(&mut b);
        let ask = b.host_sig_for(
            "ask",
            HostSig {
                args: vec![],
                rets: vec![ValueType::I64],
            },
        );
        // callback() -> (i64) runs as a guest callback from the `ask` request.
        let mut callback = body(&[ValueType::Unit, ValueType::Agg, ValueType::I64]);
        tuple_from(&mut callback, &parts, source, Reg(1), Reg(2));
        callback.ret(Reg::EFFECT, &[Reg(1)]);
        let callback = b
            .push_function_checked(
                callback,
                FunctionAbi {
                    args: vec![],
                    rets: vec![parts.single.into()],
                },
            )
            .unwrap();
        let mut main = body(&[ValueType::Unit, ValueType::I64]);
        main.host_call(Reg::EFFECT, ask, Reg::EFFECT, &[], &[Reg(1)])
            .ret(Reg::EFFECT, &[Reg(1)]);
        let entry = b.push_function_checked(main, entry_abi()).unwrap();
        let program = Arc::new(b.build_verified().unwrap());
        assert_eq!(
            proofs(&program, callback, Opcode::Ret),
            [source == Source::Local]
        );
        for (value, good) in [(Value::I64(7), true), (Value::Str("seven".into()), false)] {
            let mut vm = Vm::new(Maker(value, None), Limits::default());
            let unit = vm.install(Arc::clone(&program)).unwrap();
            let mut turn = vm
                .start(
                    FuncRef::new(unit, entry),
                    &[],
                    TurnOptions::default().yield_host_calls(),
                )
                .unwrap();
            assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::AwaitingHost);
            turn.pending_host_call()
                .unwrap()
                .call_guest(Value::Func(FuncRef::new(unit, callback)), vec![])
                .unwrap();
            let returned = loop {
                match turn.drive(u64::MAX) {
                    Ok(RunStatus::AwaitingHost) => {
                        turn.pending_host_call().unwrap().dispatch().unwrap();
                    }
                    Ok(RunStatus::CallbackReturned) => break Ok(()),
                    Ok(status) => panic!("unexpected {status:?}"),
                    Err(trap) => break Err(trap),
                }
            };
            if source == Source::Local || good {
                returned.unwrap();
                let mut request = turn.pending_host_call().unwrap();
                let results = request.take_callback_results().unwrap();
                assert_eq!(results.len(), 1);
                request.complete(&mut [Value::I64(7)], Ok(0)).unwrap();
                assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete);
            } else {
                let trap = returned.unwrap_err();
                assert!(matches!(trap.trap, Trap::ContractMismatch { .. }));
                assert_eq!(trap.func, callback);
            }
        }
    }
}
