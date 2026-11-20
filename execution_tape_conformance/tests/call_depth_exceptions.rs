// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A guest call past the call-depth limit throws a configured tag instead of trapping.

use execution_tape::{
    asm::{Catch, FunctionSig, ProgramBuilder, Reg},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::ValueType,
    recording::{RecordLimits, Recorder, ReplayOutcome},
    trace::{TraceMask, TraceSink},
    value::{FuncId, FuncRef, TagRef, Value},
    verifier::VerifiedProgram,
    vm::{CallDepthException, Limits, Trap, Vm},
};
use std::sync::Arc;

/// A host no program here calls.
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

/// The call depth the fixtures run under.
const DEPTH: usize = 10;

struct Fixture {
    program: Arc<VerifiedProgram>,
    /// `down(n)`: `n` calls deep, returning `n`.
    down: FuncId,
    /// `guarded(n)`: `down(n)`, or -1 when the overflow tag reaches it.
    guarded: FuncId,
    /// `twice(n)`: `guarded(n) + guarded(n)`.
    twice: FuncId,
    /// `nested(n)`: `nested(n - 1) + 1`, each level catching the overflow tag with a handler
    /// that returns `100 + down(3)`, which needs frames past the limit.
    nested: FuncId,
    /// `parked(n)`: `n` calls deep, a continuation of `down` made and dropped; -1 when the
    /// overflow tag reaches the deepest level.
    parked: FuncId,
    /// `pair(n)`: `hop(n) + 1`, catching the overflow tag with a handler that returns
    /// `100 + down(3)`; `hop(n)` calls `pair(n)` and catches nothing, so a call `hop` makes
    /// past the limit is caught a frame below it.
    pair: FuncId,
}

fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}

fn fixture() -> Fixture {
    let mut b = ProgramBuilder::new();
    let overflow = b.import_tag("overflow", &[]).unwrap();
    let unary = sig(&[ValueType::I64], &[ValueType::I64]);
    let down = b.declare_function(&unary).unwrap();
    let guarded = b.declare_function(&unary).unwrap();
    let twice = b.declare_function(&unary).unwrap();
    let nested = b.declare_function(&unary).unwrap();
    let parked = b.declare_function(&unary).unwrap();
    let pair = b.declare_function(&unary).unwrap();
    let hop = b.declare_function(&unary).unwrap();
    let down_sig = b.call_sig(unary.clone()).unwrap();

    // down(n) = if n == 0 { 0 } else { down(n - 1) + 1 }
    let mut a = b.function_asm(down).unwrap();
    let n = b.argument(down, 0).unwrap();
    let (zero, one, done) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Bool),
    );
    let (base, step) = (a.label(), a.label());
    a.const_i64(zero, 0)
        .const_i64(one, 1)
        .i64_eq(done, n, zero)
        .br(done, base, step);
    a.place(base).unwrap();
    a.ret(Reg::EFFECT, &[zero]);
    a.place(step).unwrap();
    let smaller = a.alloc_register(ValueType::I64);
    a.i64_sub(smaller, n, one);
    let [inner] = b.emit_call(&mut a, down, &[smaller]).unwrap();
    a.i64_add(inner, inner, one).ret(Reg::EFFECT, &[inner]);
    b.define_function(down, a).unwrap();

    // guarded(n) = down(n), or -1 when the overflow tag reaches it.
    let mut a = b.function_asm(guarded).unwrap();
    let n = b.argument(guarded, 0).unwrap();
    let start = a.label();
    a.jmp(start);
    let handler = a.block(&[]);
    a.place(handler).unwrap();
    let minus = a.alloc_register(ValueType::I64);
    a.const_i64(minus, -1).ret(Reg::EFFECT, &[minus]);
    a.place(start).unwrap();
    a.begin_region(&[Catch::Tag(overflow, handler)]);
    let [result] = b.emit_call(&mut a, down, &[n]).unwrap();
    a.end_region();
    a.ret(Reg::EFFECT, &[result]);
    b.define_function(guarded, a).unwrap();

    // twice(n) = guarded(n) + guarded(n)
    let mut a = b.function_asm(twice).unwrap();
    let n = b.argument(twice, 0).unwrap();
    let [first] = b.emit_call(&mut a, guarded, &[n]).unwrap();
    let [second] = b.emit_call(&mut a, guarded, &[n]).unwrap();
    a.i64_add(first, first, second).ret(Reg::EFFECT, &[first]);
    b.define_function(twice, a).unwrap();

    // nested(n) = nested(n - 1) + 1, each level handling the tag with 100 + down(3).
    let mut a = b.function_asm(nested).unwrap();
    let n = b.argument(nested, 0).unwrap();
    let start = a.label();
    a.jmp(start);
    let handler = a.block(&[]);
    a.place(handler).unwrap();
    let three = a.alloc_register(ValueType::I64);
    let hundred = a.alloc_register(ValueType::I64);
    a.const_i64(three, 3).const_i64(hundred, 100);
    let [deeper] = b.emit_call(&mut a, down, &[three]).unwrap();
    a.i64_add(deeper, deeper, hundred)
        .ret(Reg::EFFECT, &[deeper]);
    a.place(start).unwrap();
    let one = a.alloc_register(ValueType::I64);
    let smaller = a.alloc_register(ValueType::I64);
    a.const_i64(one, 1).i64_sub(smaller, n, one);
    a.begin_region(&[Catch::Tag(overflow, handler)]);
    let [inner] = b.emit_call(&mut a, nested, &[smaller]).unwrap();
    a.end_region();
    a.i64_add(inner, inner, one).ret(Reg::EFFECT, &[inner]);
    b.define_function(nested, a).unwrap();

    // parked(n) = if n == 0 { drop(cont_new(down)); 0 } else { parked(n - 1) }, each level
    // returning -1 for the overflow tag.
    let mut a = b.function_asm(parked).unwrap();
    let n = b.argument(parked, 0).unwrap();
    let start = a.label();
    a.jmp(start);
    let handler = a.block(&[]);
    a.place(handler).unwrap();
    let minus = a.alloc_register(ValueType::I64);
    a.const_i64(minus, -1).ret(Reg::EFFECT, &[minus]);
    a.place(start).unwrap();
    let (zero, one, done) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Bool),
    );
    let (base, step) = (a.label(), a.label());
    a.begin_region(&[Catch::Tag(overflow, handler)]);
    a.const_i64(zero, 0)
        .const_i64(one, 1)
        .i64_eq(done, n, zero)
        .br(done, base, step);
    a.place(base).unwrap();
    let (callee, cont) = (
        a.alloc_register(ValueType::Func),
        a.alloc_register(ValueType::Cont),
    );
    a.const_func(callee, down)
        .cont_new(cont, down_sig, callee)
        .cont_drop(cont)
        .ret(Reg::EFFECT, &[zero]);
    a.place(step).unwrap();
    let smaller = a.alloc_register(ValueType::I64);
    a.i64_sub(smaller, n, one);
    let [inner] = b.emit_call(&mut a, parked, &[smaller]).unwrap();
    a.ret(Reg::EFFECT, &[inner]);
    a.end_region();
    b.define_function(parked, a).unwrap();

    let mut a = b.function_asm(pair).unwrap();
    let n = b.argument(pair, 0).unwrap();
    let start = a.label();
    a.jmp(start);
    let handler = a.block(&[]);
    a.place(handler).unwrap();
    let (three, hundred) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
    );
    a.const_i64(three, 3).const_i64(hundred, 100);
    let [deeper] = b.emit_call(&mut a, down, &[three]).unwrap();
    a.i64_add(deeper, deeper, hundred)
        .ret(Reg::EFFECT, &[deeper]);
    a.place(start).unwrap();
    let one = a.alloc_register(ValueType::I64);
    a.const_i64(one, 1);
    a.begin_region(&[Catch::Tag(overflow, handler)]);
    let [inner] = b.emit_call(&mut a, hop, &[n]).unwrap();
    a.end_region();
    a.i64_add(inner, inner, one).ret(Reg::EFFECT, &[inner]);
    b.define_function(pair, a).unwrap();

    let mut a = b.function_asm(hop).unwrap();
    let n = b.argument(hop, 0).unwrap();
    let [inner] = b.emit_call(&mut a, pair, &[n]).unwrap();
    a.ret(Reg::EFFECT, &[inner]);
    b.define_function(hop, a).unwrap();

    Fixture {
        program: Arc::new(b.build_verified().unwrap()),
        down,
        guarded,
        twice,
        nested,
        parked,
        pair,
    }
}

/// A VM whose calls past [`DEPTH`] throw a tag with `headroom`, or trap with `None`. `tag`
/// picks the tag from the VM's `overflow` tag and a `paid` one, which has a payload.
fn vm(
    f: &Fixture,
    headroom: Option<usize>,
    tag: impl FnOnce(TagRef, TagRef) -> TagRef,
) -> (Vm<NoHost>, FuncRef) {
    let mut vm = Vm::new(NoHost, Limits::default());
    let overflow = vm.define_tag("overflow", &[]).unwrap();
    let paid = vm.define_tag("paid", &[ValueType::I64]).unwrap();
    vm.set_limits(Limits {
        max_call_depth: DEPTH,
        call_depth_exception: headroom.map(|headroom| CallDepthException {
            tag: tag(overflow, paid),
            headroom,
        }),
        ..Limits::default()
    });
    let unit = vm.install(f.program.clone()).unwrap();
    (vm, FuncRef::new(unit, FuncId(0)))
}

fn run(headroom: Option<usize>, entry: FuncId, n: i64) -> Result<Vec<Value>, Trap> {
    let (mut vm, base) = vm(&fixture(), headroom, |overflow, _| overflow);
    vm.run(
        FuncRef::new(base.unit, entry),
        &[Value::I64(n)],
        TraceMask::NONE,
        None,
    )
    .map_err(|error| error.trap)
}

#[test]
fn without_an_exception_a_call_past_the_limit_traps() {
    let f = fixture();
    assert_eq!(run(None, f.down, 5).unwrap(), [Value::I64(5)]);
    assert!(matches!(
        run(None, f.guarded, 100),
        Err(Trap::CallDepthExceeded)
    ));
}

#[test]
fn a_call_past_the_limit_throws_the_tag_where_it_is_made() {
    let f = fixture();
    // Calls within the limit run as before.
    assert_eq!(run(Some(4), f.guarded, 5).unwrap(), [Value::I64(5)]);
    // `guarded` is the entry frame, so `down` may be DEPTH - 1 calls deep.
    let deepest = i64::try_from(DEPTH).unwrap() - 2;
    assert_eq!(
        run(Some(4), f.guarded, deepest).unwrap(),
        [Value::I64(deepest)]
    );
    assert_eq!(
        run(Some(4), f.guarded, deepest + 1).unwrap(),
        [Value::I64(-1)]
    );
    assert_eq!(run(Some(4), f.guarded, 100).unwrap(), [Value::I64(-1)]);
    // Uncaught, it leaves the turn as any uncaught exception does.
    match run(Some(4), f.down, 100) {
        Err(Trap::Uncaught(exception)) => assert_eq!(exception.name(), "overflow"),
        other => panic!("expected the overflow tag, got {other:?}"),
    }
}

#[test]
fn a_continuation_past_the_limit_throws_where_it_is_made() {
    let f = fixture();
    // Level `n` runs `n + 1` frames deep, and the continuation's entry frame is one more.
    let fits = i64::try_from(DEPTH).unwrap() - 2;
    assert_eq!(run(Some(4), f.parked, fits).unwrap(), [Value::I64(0)]);
    assert_eq!(run(Some(4), f.parked, fits + 1).unwrap(), [Value::I64(-1)]);
    assert!(matches!(
        run(None, f.parked, fits + 1),
        Err(Trap::CallDepthExceeded)
    ));
}

#[test]
fn a_handler_past_the_limit_has_the_headroom_and_no_more() {
    let f = fixture();
    // The deepest `nested` frame catches the tag at the limit; its handler's `down(3)` needs
    // four frames beyond it.
    let levels = i64::try_from(DEPTH).unwrap() - 1;
    assert_eq!(
        run(Some(4), f.nested, 100).unwrap(),
        [Value::I64(103 + levels)]
    );
    // With less headroom the handler's calls trap rather than throw again.
    assert!(matches!(
        run(Some(3), f.nested, 100),
        Err(Trap::CallDepthExceeded)
    ));
}

#[test]
fn a_catch_just_under_the_limit_keeps_the_headroom_for_its_handler() {
    let f = fixture();
    // `pair` runs at odd depths. At an even limit `hop` makes the refused call and the
    // `pair` under it catches; at an odd one `pair` catches its own. Either way its
    // handler's `down(3)` runs in the headroom, and each `pair` below adds one.
    for (depth, pairs_below) in [(10, 4), (11, 5)] {
        let mut vm = Vm::new(NoHost, Limits::default());
        let overflow = vm.define_tag("overflow", &[]).unwrap();
        vm.set_limits(Limits {
            max_call_depth: depth,
            call_depth_exception: Some(CallDepthException {
                tag: overflow,
                headroom: 4,
            }),
            ..Limits::default()
        });
        let unit = vm.install(f.program.clone()).unwrap();
        let result = vm
            .run(
                FuncRef::new(unit, f.pair),
                &[Value::I64(0)],
                TraceMask::NONE,
                None,
            )
            .unwrap();
        assert_eq!(result, [Value::I64(103 + pairs_below)], "depth {depth}");
    }
}

#[test]
fn a_catch_back_under_the_limit_arms_the_exception_again() {
    let f = fixture();
    assert_eq!(run(Some(4), f.twice, 100).unwrap(), [Value::I64(-2)]);
    // Each turn starts armed.
    let (mut vm, base) = vm(&f, Some(4), |overflow, _| overflow);
    for _ in 0..2 {
        let result = vm
            .run(
                FuncRef::new(base.unit, f.guarded),
                &[Value::I64(100)],
                TraceMask::NONE,
                None,
            )
            .unwrap();
        assert_eq!(result, [Value::I64(-1)]);
    }
}

#[test]
fn a_tag_that_cannot_be_thrown_without_a_payload_traps() {
    let f = fixture();
    for payload in [false, true] {
        let (mut vm, base) = vm(
            &f,
            Some(4),
            |_, paid| if payload { paid } else { TagRef(999) },
        );
        let error = vm
            .run(
                FuncRef::new(base.unit, f.guarded),
                &[Value::I64(100)],
                TraceMask::NONE,
                None,
            )
            .unwrap_err();
        if payload {
            assert!(matches!(error.trap, Trap::ArityMismatch), "{error:?}");
        } else {
            assert!(matches!(error.trap, Trap::UnknownTag { .. }), "{error:?}");
        }
    }
}

#[test]
fn recordings_replay_the_throw() {
    let f = fixture();
    let (mut vm, base) = vm(&f, Some(4), |overflow, _| overflow);
    let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
    let result = vm
        .run(
            FuncRef::new(base.unit, f.nested),
            &[Value::I64(100)],
            recorder.mask(),
            Some(&mut recorder),
        )
        .unwrap();
    let recording = recorder.finish();
    recording.status().unwrap();
    match recording.replay().unwrap() {
        ReplayOutcome::Returned(values) => assert_eq!(values.values(), &result[..]),
        ReplayOutcome::Trapped(error) => panic!("replay trapped: {error:?}"),
    }
}
