// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Overflow-reporting integer arithmetic (`i64.*_ovf`, `u64.*_ovf`) through the public API.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::opcode::Opcode;
use execution_tape::program::{Program, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::FuncRef;
use execution_tape::value::{FuncId, Value};
use execution_tape::verifier::{
    VerifiedProgram, VerifyConfig, VerifyError, verify_program, verify_program_owned,
};
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

/// Builds `f(a, b) -> (a + b, ovf, a - b, ovf, a * b, ovf)` for `kind` (`I64` or `U64`),
/// roundtripped through the wire format.
fn program(kind: ValueType) -> VerifiedProgram {
    let mut builder = ProgramBuilder::new();
    let rets = [
        kind,
        ValueType::Bool,
        kind,
        ValueType::Bool,
        kind,
        ValueType::Bool,
    ];
    let id = builder
        .declare_function(FunctionSig {
            arg_types: vec![kind, kind],
            ret_types: rets.to_vec(),
        })
        .unwrap();
    let mut body = builder.function_asm(id).unwrap();
    let regs: Vec<u32> = rets.iter().map(|&t| body.alloc_register(t)).collect();
    if kind == ValueType::I64 {
        body.i64_add_ovf(regs[0], regs[1], 1, 2)
            .i64_sub_ovf(regs[2], regs[3], 1, 2)
            .i64_mul_ovf(regs[4], regs[5], 1, 2);
    } else {
        body.u64_add_ovf(regs[0], regs[1], 1, 2)
            .u64_sub_ovf(regs[2], regs[3], 1, 2)
            .u64_mul_ovf(regs[4], regs[5], 1, 2);
    }
    body.ret(0, &regs);
    builder.define_function(id, body).unwrap();
    let bytes = builder.build().encode();
    verify_program_owned(Program::decode(&bytes).unwrap(), &VerifyConfig::default()).unwrap()
}

fn run(vm: &mut Vm<NoHost>, program: &VerifiedProgram, a: Value, b: Value) -> Vec<Value> {
    let unit = vm.install(program.clone()).unwrap();
    vm.run(
        FuncRef::new(unit, FuncId(0)),
        &[a, b],
        TraceMask::NONE,
        None,
    )
    .unwrap()
}

/// `SplitMix64`: a tiny deterministic generator for reproducible inputs.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value with a uniformly chosen bit length, so small and large magnitudes both occur.
    fn bits(&mut self) -> u64 {
        let shift = self.next() % 64;
        self.next() >> shift
    }
}

#[test]
fn signed_overflow_flags_match_checked_arithmetic() {
    let program = program(ValueType::I64);
    let mut vm = Vm::new(NoHost, Limits::default());
    let values = [
        i64::MIN,
        i64::MIN + 1,
        i64::MIN / 2,
        -(1 << 32),
        -3,
        -2,
        -1,
        0,
        1,
        2,
        3,
        1 << 32,
        i64::MAX / 2,
        i64::MAX / 2 + 1,
        i64::MAX - 1,
        i64::MAX,
    ];
    let mut rng = SplitMix64(1);
    let mut pairs: Vec<(i64, i64)> = values
        .iter()
        .flat_map(|&a| values.iter().map(move |&b| (a, b)))
        .collect();
    for _ in 0..20_000 {
        let a = rng.bits().cast_signed() * if rng.next() & 1 == 0 { 1 } else { -1 };
        let b = rng.bits().cast_signed() * if rng.next() & 1 == 0 { 1 } else { -1 };
        pairs.push((a, b));
    }
    for (a, b) in pairs {
        let want = [
            Value::I64(a.wrapping_add(b)),
            Value::Bool(a.checked_add(b).is_none()),
            Value::I64(a.wrapping_sub(b)),
            Value::Bool(a.checked_sub(b).is_none()),
            Value::I64(a.wrapping_mul(b)),
            Value::Bool(a.checked_mul(b).is_none()),
        ];
        let got = run(&mut vm, &program, Value::I64(a), Value::I64(b));
        assert_eq!(got, want, "a = {a}, b = {b}");
    }
}

#[test]
fn signed_overflow_boundaries() {
    let program = program(ValueType::I64);
    let mut vm = Vm::new(NoHost, Limits::default());
    // (a, b) => (add overflows, sub overflows, mul overflows)
    let cases = [
        (i64::MAX, 1, (true, false, false)),
        (i64::MAX, 0, (false, false, false)),
        (i64::MIN, -1, (true, false, true)),
        (i64::MIN, 1, (false, true, false)),
        (0, i64::MIN, (false, true, false)),
        (-1, i64::MIN, (true, false, true)),
        (i64::MIN, i64::MIN, (true, false, true)),
        (i64::MAX, i64::MAX, (true, false, true)),
        (1 << 32, 1 << 30, (false, false, false)),
        // 2^63 does not fit, but -2^63 is exactly `i64::MIN`.
        (1 << 32, 1 << 31, (false, false, true)),
        (-(1 << 32), 1 << 31, (false, false, false)),
        (1 << 32, 1 << 32, (false, false, true)),
    ];
    for (a, b, (add, sub, mul)) in cases {
        let got = run(&mut vm, &program, Value::I64(a), Value::I64(b));
        assert_eq!(
            [&got[1], &got[3], &got[5]],
            [&Value::Bool(add), &Value::Bool(sub), &Value::Bool(mul)],
            "a = {a}, b = {b}"
        );
    }
    // `i64::MIN * -1` wraps back to `i64::MIN`.
    let got = run(&mut vm, &program, Value::I64(i64::MIN), Value::I64(-1));
    assert_eq!(got[4], Value::I64(i64::MIN));
}

#[test]
fn unsigned_overflow_flags_match_checked_arithmetic() {
    let program = program(ValueType::U64);
    let mut vm = Vm::new(NoHost, Limits::default());
    let values = [
        0,
        1,
        2,
        1 << 32,
        (1 << 32) - 1,
        1 << 63,
        u64::MAX - 1,
        u64::MAX,
    ];
    let mut pairs: Vec<(u64, u64)> = values
        .iter()
        .flat_map(|&a| values.iter().map(move |&b| (a, b)))
        .collect();
    let mut rng = SplitMix64(2);
    for _ in 0..20_000 {
        pairs.push((rng.bits(), rng.bits()));
    }
    for (a, b) in pairs {
        let want = [
            Value::U64(a.wrapping_add(b)),
            Value::Bool(a.checked_add(b).is_none()),
            Value::U64(a.wrapping_sub(b)),
            Value::Bool(a.checked_sub(b).is_none()),
            Value::U64(a.wrapping_mul(b)),
            Value::Bool(a.checked_mul(b).is_none()),
        ];
        let got = run(&mut vm, &program, Value::U64(a), Value::U64(b));
        assert_eq!(got, want, "a = {a}, b = {b}");
    }
}

#[test]
fn overflow_opcode_bytes_and_disassembly_are_stable() {
    for (opcode, byte, mnemonic) in [
        (Opcode::I64AddOvf, 0xA1, "i64.add_ovf"),
        (Opcode::I64SubOvf, 0xA2, "i64.sub_ovf"),
        (Opcode::I64MulOvf, 0xA3, "i64.mul_ovf"),
        (Opcode::U64AddOvf, 0xA4, "u64.add_ovf"),
        (Opcode::U64SubOvf, 0xA5, "u64.sub_ovf"),
        (Opcode::U64MulOvf, 0xA6, "u64.mul_ovf"),
    ] {
        assert_eq!(opcode.byte(), byte, "{mnemonic} byte");
        assert_eq!(Opcode::from_byte(byte), Some(opcode), "{mnemonic} decode");
        assert_eq!(opcode.mnemonic(), mnemonic);
    }
    for kind in [ValueType::I64, ValueType::U64] {
        let program = program(kind);
        let prefix = if kind == ValueType::I64 { "i64" } else { "u64" };
        let disassembly =
            execution_tape::disasm::disassemble_function(program.program(), FuncId(0)).unwrap();
        let lines: Vec<_> = disassembly
            .instrs()
            .map(|instruction| {
                let writes: Vec<u32> = instruction.writes().collect();
                (instruction.to_string(), instruction.dst(), writes)
            })
            .collect();
        for (line, op, (dst, flag)) in [(0, "add", (3, 4)), (1, "sub", (5, 6)), (2, "mul", (7, 8))]
        {
            let (text, primary, writes) = &lines[line];
            assert!(
                text.ends_with(&format!("{prefix}.{op}_ovf r{dst}, r{flag}, [r1, r2]")),
                "{text}"
            );
            assert_eq!(*primary, Some(dst));
            assert_eq!(*writes, [dst, flag]);
        }
    }
}

#[test]
fn verifier_requires_a_bool_flag_and_matching_operands() {
    // (result, flag, operand) register kinds.
    for (result, flag, operand) in [
        (ValueType::I64, ValueType::I64, ValueType::I64),
        (ValueType::Bool, ValueType::Bool, ValueType::I64),
        (ValueType::I64, ValueType::Bool, ValueType::U64),
    ] {
        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, operand, operand, result, flag]);
        a.i64_add_ovf(3, 4, 1, 2);
        a.ret(0, &[]);
        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![operand, operand],
                ret_types: vec![],
            },
        )
        .unwrap();
        let error = verify_program(&pb.build(), &VerifyConfig::default()).unwrap_err();
        assert!(
            matches!(error, VerifyError::RegisterTypeMismatch { .. }),
            "{result:?}/{flag:?}/{operand:?}: {error:?}"
        );
    }
}
