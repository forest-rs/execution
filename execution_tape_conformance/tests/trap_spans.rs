// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A trap reports the span the frontend marked with `Asm::span` for the trapping instruction.

use execution_tape::asm::{FunctionSig, ProgramBuilder, Reg};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::{SpanId, ValueType::U32};
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncRef, Value};
use execution_tape::vm::{Limits, Trap, Vm};

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

const FIRST: SpanId = SpanId::new(1);
const SECOND: SpanId = SpanId::new(2);
const CALLEE: SpanId = SpanId::new(3);

#[test]
fn traps_report_the_span_of_the_trapping_instruction() {
    let mut builder = ProgramBuilder::new();
    // f(x, y, z) = g(((x / y) / z) / x), where g(v) = 1 / v.
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![U32, U32, U32],
            ret_types: vec![U32],
        })
        .unwrap();
    let callee = builder
        .declare_function(FunctionSig {
            arg_types: vec![U32],
            ret_types: vec![U32],
        })
        .unwrap();
    let [x, y, z] = [0, 1, 2].map(|i| builder.argument(entry, i).unwrap());
    let mut body = builder.function_asm(entry).unwrap();
    let [a, b, c] = [(); 3].map(|()| body.alloc_register(U32));
    // Unmarked: no span until the first mark.
    body.u32_div(a, x, y);
    body.span(FIRST).u32_div(b, a, z);
    body.span(SECOND).u32_div(c, b, x);
    let [result] = builder.emit_call(&mut body, callee, &[c]).unwrap();
    body.ret(Reg::EFFECT, &[result]);
    builder.define_function(entry, body).unwrap();

    let v = builder.argument(callee, 0).unwrap();
    let mut body = builder.function_asm(callee).unwrap();
    let [one, quotient] = [(); 2].map(|()| body.alloc_register(U32));
    body.span(CALLEE)
        .const_u32(one, 1)
        .u32_div(quotient, one, v)
        .ret(Reg::EFFECT, &[quotient]);
    builder.define_function(callee, body).unwrap();
    let program = builder.build_verified().unwrap();

    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(program).unwrap();
    let mut run = |args: [u32; 3]| {
        let args = args.map(Value::U32);
        vm.run(FuncRef::new(unit, entry), &args, TraceMask::NONE, None)
    };

    assert_eq!(run([2, 1, 1]).unwrap(), [Value::U32(1)]);
    for (args, func, span) in [
        ([1, 0, 1], entry, None),
        ([1, 1, 0], entry, Some(FIRST)),
        ([0, 1, 1], entry, Some(SECOND)),
        ([1, 2, 1], callee, Some(CALLEE)),
    ] {
        let trap = run(args).unwrap_err();
        assert!(matches!(trap.trap, Trap::DivByZero), "{trap}");
        assert_eq!((trap.func, trap.span_id), (func, span), "{args:?}: {trap}");
    }
}
