// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Deterministic guest generation, an independent expression model, and failure shrinking.

use execution_tape::{
    asm::{Asm, FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::{Program, ValueType},
    trace::TraceMask,
    value::{FuncId, Value},
    verifier::{VerifyConfig, verify_program_owned},
    vm::{InvocationOptions, Limits, RunStatus, Vm},
};

#[derive(Clone, Debug)]
enum Expr {
    Input,
    Literal(i64),
    Add(Box<Self>, Box<Self>),
    Xor(Box<Self>, Box<Self>),
    Choose(Box<Self>, Box<Self>, Box<Self>, Box<Self>),
    Pick(Box<Self>, Box<Self>, u8),
    Call(Box<Self>),
}

impl Expr {
    fn eval(&self, input: i64) -> i64 {
        match self {
            Self::Input => input,
            Self::Literal(n) => *n,
            Self::Add(a, b) => a.eval(input).wrapping_add(b.eval(input)),
            Self::Xor(a, b) => a.eval(input) ^ b.eval(input),
            Self::Choose(a, b, yes, no) => {
                if a.eval(input) < b.eval(input) {
                    yes.eval(input)
                } else {
                    no.eval(input)
                }
            }
            Self::Pick(a, b, index) => {
                if *index == 1 {
                    b.eval(input)
                } else {
                    a.eval(input)
                }
            }
            Self::Call(a) => a.eval(input).wrapping_mul(3).wrapping_add(1),
        }
    }

    fn children(&self) -> Vec<&Self> {
        match self {
            Self::Input | Self::Literal(_) => vec![],
            Self::Add(a, b) | Self::Xor(a, b) | Self::Pick(a, b, _) => vec![a, b],
            Self::Choose(a, b, yes, no) => vec![a, b, yes, no],
            Self::Call(a) => vec![a],
        }
    }

    fn replace_child(&self, index: usize, replacement: Self) -> Self {
        let mut result = self.clone();
        let children = match &mut result {
            Self::Add(a, b) | Self::Xor(a, b) | Self::Pick(a, b, _) => vec![a, b],
            Self::Choose(a, b, yes, no) => vec![a, b, yes, no],
            Self::Call(a) => vec![a],
            _ => unreachable!(),
        };
        **children.into_iter().nth(index).unwrap() = replacement;
        result
    }

    // Lexicographic and strictly decreasing for every accepted shrink.
    fn size(&self) -> (usize, usize) {
        let nonzero_leaf = match self {
            Self::Input => true,
            Self::Literal(n) => *n != 0,
            _ => false,
        };
        self.children().into_iter().fold(
            (1, usize::from(nonzero_leaf)),
            |(nodes, leaves), child| {
                let (n, l) = child.size();
                (nodes + n, leaves + l)
            },
        )
    }

    fn shrinks(&self) -> Vec<Self> {
        let mut candidates = vec![Self::Literal(0), Self::Input];
        for (index, child) in self.children().into_iter().enumerate() {
            candidates.push(child.clone());
            candidates.extend(
                child
                    .shrinks()
                    .into_iter()
                    .map(|s| self.replace_child(index, s)),
            );
        }
        candidates.retain(|candidate| candidate.size() < self.size());
        candidates
    }
}

fn shrink(mut expr: Expr, fails: impl Fn(&Expr) -> bool) -> Expr {
    while let Some(smaller) = expr
        .shrinks()
        .into_iter()
        .find(|candidate| fails(candidate))
    {
        expr = smaller;
    }
    expr
}

struct Random(u64);
impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut n = self.0;
        n = (n ^ (n >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        n = (n ^ (n >> 27)).wrapping_mul(0x94d049bb133111eb);
        n ^ (n >> 31)
    }

    fn expr(&mut self, depth: usize) -> Expr {
        let choice = self.next() % if depth == 0 { 2 } else { 7 };
        match choice {
            0 => Expr::Input,
            1 => Expr::Literal(self.next() as i64),
            2 => Expr::Add(
                Box::new(self.expr(depth - 1)),
                Box::new(self.expr(depth - 1)),
            ),
            3 => Expr::Xor(
                Box::new(self.expr(depth - 1)),
                Box::new(self.expr(depth - 1)),
            ),
            4 => Expr::Choose(
                Box::new(self.expr(depth - 1)),
                Box::new(self.expr(depth - 1)),
                Box::new(self.expr(depth - 1)),
                Box::new(self.expr(depth - 1)),
            ),
            5 => Expr::Pick(
                Box::new(self.expr(depth - 1)),
                Box::new(self.expr(depth - 1)),
                (self.next() % 3) as u8,
            ),
            _ => Expr::Call(Box::new(self.expr(depth - 1))),
        }
    }
}

fn emit(expr: &Expr, body: &mut Asm, helper: FuncId) -> u32 {
    let dst = body.alloc_register(ValueType::I64);
    match expr {
        Expr::Input => {
            body.mov(dst, 1);
        }
        Expr::Literal(n) => {
            body.const_i64(dst, *n);
        }
        Expr::Add(a, b) | Expr::Xor(a, b) => {
            let a = emit(a, body, helper);
            let b = emit(b, body, helper);
            if matches!(expr, Expr::Add(..)) {
                body.i64_add(dst, a, b);
            } else {
                body.i64_xor(dst, a, b);
            }
        }
        Expr::Choose(a, b, yes, no) => {
            let a = emit(a, body, helper);
            let b = emit(b, body, helper);
            let condition = body.alloc_register(ValueType::Bool);
            body.i64_lt(condition, a, b);
            let then_arm = body.label();
            let else_arm = body.label();
            let join = body.block(&[dst]);
            body.br(condition, then_arm, else_arm);
            body.place(then_arm).unwrap();
            let value = emit(yes, body, helper);
            body.jmp(join.with_args(&[value]));
            body.place(else_arm).unwrap();
            let value = emit(no, body, helper);
            body.jmp(join.with_args(&[value]));
            body.place(join).unwrap();
        }
        Expr::Pick(a, b, index) => {
            let a = emit(a, body, helper);
            let b = emit(b, body, helper);
            let tuple = body.alloc_register(ValueType::Agg);
            let alias = body.alloc_register(ValueType::Agg);
            body.tuple_new(tuple, &[a, b, a])
                .mov(alias, tuple)
                .tuple_get(dst, alias, u32::from(*index));
        }
        Expr::Call(a) => {
            let a = emit(a, body, helper);
            body.call(0, helper, 0, &[a], &[dst]);
        }
    }
    dst
}

fn compile(expr: &Expr) -> (Program, u32) {
    let mut builder = ProgramBuilder::new();
    let sig = FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
    };
    let entry = builder.declare_function(sig.clone()).unwrap();
    let helper = builder.declare_function(sig).unwrap();
    let mut body = builder.function_asm(helper).unwrap();
    let one = body.alloc_register(ValueType::I64);
    let three = body.alloc_register(ValueType::I64);
    let result = body.alloc_register(ValueType::I64);
    body.const_i64(one, 1)
        .const_i64(three, 3)
        .i64_mul(result, 1, three)
        .i64_add(result, result, one)
        .ret(0, &[result]);
    builder.define_function(helper, body).unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let result = emit(expr, &mut body, helper);
    body.ret(0, &[result]);
    builder.define_function(entry, body).unwrap();
    (builder.build(), result)
}

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

fn check(expr: &Expr, input: i64) -> Result<(), String> {
    let (program, _) = compile(expr);
    // Execute both the authored program and its actual wire roundtrip.
    let decoded = Program::decode(&program.encode()).map_err(|e| format!("decode: {e:?}"))?;
    for program in [program, decoded] {
        let verified = verify_program_owned(program, &VerifyConfig::default())
            .map_err(|e| format!("verify: {e:?}"))?;
        let output = Vm::new(NoHost, Limits::default())
            .run(
                &verified,
                FuncId(0),
                &[Value::I64(input)],
                TraceMask::NONE,
                None,
            )
            .map_err(|e| format!("run: {e:?}"))?;
        for quantum in [1, 7, u64::MAX] {
            let mut vm = Vm::new(NoHost, Limits::default());
            let args = [Value::I64(input)];
            let mut invocation = vm
                .start(&verified, FuncId(0), &args, InvocationOptions::default())
                .map_err(|e| format!("start: {e:?}"))?;
            while invocation
                .resume(quantum)
                .map_err(|e| format!("resume: {e:?}"))?
                == RunStatus::Suspended
            {}
            if invocation.take_outputs().unwrap() != output {
                return Err(format!("scheduled execution differed at quantum {quantum}"));
            }
        }
        if output != [Value::I64(expr.eval(input))] {
            return Err(format!("expected {}, got {output:?}", expr.eval(input)));
        }
    }
    Ok(())
}

#[test]
fn generated_guests_match_model_and_reject_corrupted_declarations() {
    let seeds: Vec<u64> = match std::env::var("EXECUTION_TAPE_CASE") {
        Ok(seed) => vec![seed.parse().expect("EXECUTION_TAPE_CASE must be a u64")],
        Err(_) => (0..256).collect(),
    };
    for seed in seeds {
        let expr = Random(seed).expr(4);
        for input in [i64::MIN, -1, 0, 1, 7, i64::MAX] {
            if let Err(error) = check(&expr, input) {
                let minimal = shrink(expr.clone(), |e| check(e, input).is_err());
                panic!(
                    "seed={seed}, input={input}, error={error}; minimized={minimal:?}; minimized error={:?}",
                    check(&minimal, input)
                );
            }
        }
        let (mut malformed, result) = compile(&expr);
        // Change only the result register's declaration, leaving code and ABI intact.
        let wrong_kind = u32::try_from(malformed.value_types.len()).unwrap();
        malformed.value_types.push(ValueType::Bool);
        let offset = malformed.functions[0].register_types.offset as usize + result as usize;
        malformed.register_type_ids[offset] = wrong_kind;
        assert!(
            verify_program_owned(malformed, &VerifyConfig::default()).is_err(),
            "seed={seed}"
        );
        let (mut truncated, _) = compile(&expr);
        truncated.functions[0].bytecode.len -= 1;
        assert!(
            verify_program_owned(truncated, &VerifyConfig::default()).is_err(),
            "truncated return, seed={seed}"
        );
    }
}

#[test]
fn shrinker_preserves_failure_and_reaches_a_local_minimum() {
    // An injected model defect: every guest Call is incorrectly treated as the identity.
    fn erase_calls(expr: &Expr) -> Expr {
        if let Expr::Call(child) = expr {
            return erase_calls(child);
        }
        let mut result = expr.clone();
        for (index, child) in expr.children().into_iter().enumerate() {
            result = result.replace_child(index, erase_calls(child));
        }
        result
    }
    let fails = |expr: &Expr| expr.eval(7) != erase_calls(expr).eval(7);
    let original = Expr::Add(
        Box::new(Random(29).expr(4)),
        Box::new(Expr::Call(Box::new(Expr::Literal(42)))),
    );
    assert!(fails(&original));
    let minimal = shrink(original.clone(), fails);
    assert!(fails(&minimal));
    assert!(minimal.size() < original.size());
    assert!(minimal.shrinks().iter().all(|candidate| !fails(candidate)));
    assert!(matches!(minimal, Expr::Call(inner) if matches!(*inner, Expr::Literal(0))));
}
