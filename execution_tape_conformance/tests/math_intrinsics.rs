// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `f32` math intrinsics and half-away rounding, through the VM, against `execution_tape::math`
//! and the pinned `libm`.
//!
//! The sweeps check that the VM computes exactly what `math` computes, and that `math` is `libm`.
//! The golden vectors pin exact result bits, so a run on any platform (CI covers Linux, macOS,
//! Windows and wasm) catches a result that drifts with the target or with a `libm` upgrade.
//! Non-NaN results compare by bits; a NaN result only has to be a NaN, which is all the
//! specification promises for these operations.

use std::sync::Arc;

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::disasm::disassemble_function;
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::math;
use execution_tape::opcode::Opcode;
use execution_tape::program::ValueType::{self, F32, F64, Unit};
use execution_tape::recording::{RecordLimits, Recorder, ReplayOutcome};
use execution_tape::trace::{TraceMask, TraceSink};
use execution_tape::value::{FuncId, FuncRef, Value};
use execution_tape::verifier::VerifiedProgram;
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

/// The new instructions, each as an `Asm` emitter, its arity and its `math` function.
#[derive(Clone, Copy, Debug)]
enum Op {
    RoundHalfAway,
    Exp,
    Ln,
    Sin,
    Cos,
    Pow,
    Atan2,
}
use Op::{Atan2, Cos, Exp, Ln, Pow, RoundHalfAway, Sin};

impl Op {
    fn arity(self) -> usize {
        match self {
            Pow | Atan2 => 2,
            _ => 1,
        }
    }

    fn emit(self, asm: &mut Asm) {
        match self {
            RoundHalfAway => asm.f32_round_half_away(4, 1),
            Exp => asm.f32_exp(4, 1),
            Ln => asm.f32_ln(4, 1),
            Sin => asm.f32_sin(4, 1),
            Cos => asm.f32_cos(4, 1),
            Pow => asm.f32_pow(4, 1, 2),
            Atan2 => asm.f32_atan2(4, 1, 2),
        };
    }

    /// What `execution_tape::math` computes.
    fn math(self, v: &[f32]) -> f32 {
        match self {
            RoundHalfAway => math::round_half_away_f32(v[0]),
            Exp => math::exp_f32(v[0]),
            Ln => math::ln_f32(v[0]),
            Sin => math::sin_f32(v[0]),
            Cos => math::cos_f32(v[0]),
            Pow => math::pow_f32(v[0], v[1]),
            Atan2 => math::atan2_f32(v[0], v[1]),
        }
    }

    /// What the pinned `libm` computes.
    fn libm(self, v: &[f32]) -> f32 {
        match self {
            RoundHalfAway => libm::roundf(v[0]),
            Exp => libm::expf(v[0]),
            Ln => libm::logf(v[0]),
            Sin => libm::sinf(v[0]),
            Cos => libm::cosf(v[0]),
            Pow => libm::powf(v[0], v[1]),
            Atan2 => libm::atan2f(v[0], v[1]),
        }
    }
}

/// Builds `f(r1, r2, r3) -> r4` whose body is `emit`.
fn program(emit: impl FnOnce(&mut Asm), arg: ValueType, ret: ValueType) -> Arc<VerifiedProgram> {
    let mut asm = Asm::new();
    asm.set_register_types(vec![Unit, arg, arg, arg, ret]);
    emit(&mut asm);
    asm.ret(0, &[4]);
    let mut builder = ProgramBuilder::new();
    builder
        .push_function_checked(
            asm,
            FunctionSig {
                arg_types: vec![arg, arg, arg],
                ret_types: vec![ret],
            },
        )
        .unwrap();
    Arc::new(builder.build_verified().unwrap())
}

struct Runner {
    vm: Vm<NoHost>,
    entry: FuncRef,
}

impl Runner {
    fn new(program: Arc<VerifiedProgram>) -> Self {
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(program).unwrap();
        Self {
            vm,
            entry: FuncRef::new(unit, FuncId(0)),
        }
    }

    fn for_op(op: Op) -> Self {
        Self::new(program(|asm| op.emit(asm), F32, F32))
    }

    fn run(&mut self, args: [Value; 3]) -> Result<Value, Trap> {
        self.vm
            .run(self.entry, &args, TraceMask::NONE, None)
            .map(|mut out| out.remove(0))
            .map_err(|e| e.trap)
    }

    fn run_f32(&mut self, v: &[f32]) -> f32 {
        let arg = |i: usize| Value::F32(v.get(i).copied().unwrap_or(0.0));
        match self.run([arg(0), arg(1), arg(2)]) {
            Ok(Value::F32(out)) => out,
            other => panic!("{v:?}: {other:?}"),
        }
    }
}

/// Bitwise equality, except that any NaN matches any NaN.
fn same(a: f32, b: f32) -> bool {
    (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
}

/// Signed zeros, subnormals, infinities, NaNs, and huge and tiny arguments.
fn edge_cases() -> Vec<f32> {
    let mut xs = vec![
        0.0,
        f32::from_bits(1),
        f32::from_bits(0x007F_FFFF),
        f32::MIN_POSITIVE,
        1e-30,
        1e-7,
        0.5,
        0.499_999_97,
        1.0,
        1.5,
        2.0,
        2.5,
        std::f32::consts::FRAC_PI_4,
        std::f32::consts::FRAC_PI_2,
        std::f32::consts::PI,
        std::f32::consts::TAU,
        10.0,
        88.72283,
        89.0,
        1e5,
        8_388_607.5,
        8_388_609.0,
        1e30,
        f32::MAX,
        f32::INFINITY,
        f32::NAN,
        f32::from_bits(0x7FA0_0001),
    ];
    let negated: Vec<f32> = xs.iter().map(|x| -x).collect();
    xs.extend(negated);
    xs
}

/// The edge cases and an even stride over every `f32` bit pattern.
fn sweep(stride: usize) -> Vec<f32> {
    let mut xs = edge_cases();
    xs.extend((0..=u32::MAX).step_by(stride).map(f32::from_bits));
    xs
}

#[test]
fn intrinsics_compute_exactly_what_math_and_libm_compute() {
    let xs = sweep(0x3_0001);
    let pairs = sweep(0x100_0001);
    let mut checked = 0;
    for op in [RoundHalfAway, Exp, Ln, Sin, Cos, Pow, Atan2] {
        let mut runner = Runner::for_op(op);
        let inputs: Vec<Vec<f32>> = if op.arity() == 1 {
            xs.iter().map(|&x| vec![x]).collect()
        } else {
            let mut v: Vec<Vec<f32>> = pairs
                .iter()
                .flat_map(|&a| pairs.iter().map(move |&b| vec![a, b]))
                .collect();
            // Dapple's `pow(r.clamp(0.0, 1.0), 10.0)` and friends.
            v.extend((0..=256).map(|i| vec![i as f32 / 256.0, 10.0]));
            v
        };
        for v in inputs {
            let (vm, ours, theirs) = (runner.run_f32(&v), op.math(&v), op.libm(&v));
            assert!(
                same(vm, ours) && same(ours, theirs),
                "{op:?}{v:?}: vm {vm:e} ({:#x}), math {ours:e} ({:#x}), libm {theirs:e} ({:#x})",
                vm.to_bits(),
                ours.to_bits(),
                theirs.to_bits()
            );
            checked += 1;
        }
    }
    assert!(checked > 250_000, "{checked}");
}

/// Exact expected bits. An expected NaN (`0x7FC0_0000`) only requires a NaN result.
const GOLDEN: &[(Op, &[u32], u32)] = &[
    (Exp, &[0x3F80_0000], 0x402D_F854),                // e^1
    (Exp, &[0xBF80_0000], 0x3EBC_5AB2),                // e^-1
    (Exp, &[0x3F00_0000], 0x3FD3_094C),                // e^0.5
    (Exp, &[0x42B0_0000], 0x7EF8_82B7),                // e^88
    (Exp, &[0x42B2_0000], 0x7F80_0000),                // e^89 overflows
    (Exp, &[0xC2C8_0000], 0x0000_001B),                // e^-100 is subnormal
    (Exp, &[0x33D6_BF95], 0x3F80_0001),                // e^1e-7
    (Exp, &[0x8000_0000], 0x3F80_0000),                // e^-0
    (Ln, &[0x4000_0000], 0x3F31_7218),                 // ln 2
    (Ln, &[0x4120_0000], 0x4013_5D8E),                 // ln 10
    (Ln, &[0x3DCC_CCCD], 0xC013_5D8E),                 // ln 0.1
    (Ln, &[0x0001_16C2], 0xC2B8_34F2),                 // ln 1e-40 (subnormal)
    (Ln, &[0x7F7F_FFFF], 0x42B1_7218),                 // ln MAX
    (Ln, &[0x3F80_0000], 0x0000_0000),                 // ln 1
    (Ln, &[0x8000_0000], 0xFF80_0000),                 // ln -0
    (Ln, &[0xBF80_0000], 0x7FC0_0000),                 // ln -1
    (Sin, &[0x3F80_0000], 0x3F57_6AA4),                // sin 1
    (Sin, &[0x3F00_0000], 0x3EF5_7744),                // sin 0.5
    (Sin, &[0x47C3_5000], 0x3D12_6D55),                // sin 1e5
    (Sin, &[0x7149_F2CA], 0xBF4A_89B0),                // sin 1e30
    (Sin, &[0x7F7F_FFFF], 0xBF05_99B3),                // sin MAX
    (Sin, &[0x4049_0FDB], 0xB3BB_BD2E),                // sin pi (as f32)
    (Sin, &[0x0001_16C2], 0x0001_16C2),                // sin 1e-40
    (Sin, &[0x8000_0000], 0x8000_0000),                // sin -0
    (Sin, &[0x7F80_0000], 0x7FC0_0000),                // sin inf
    (Cos, &[0x3F80_0000], 0x3F0A_5140),                // cos 1
    (Cos, &[0x3F00_0000], 0x3F60_A940),                // cos 0.5
    (Cos, &[0x47C3_5000], 0xBF7F_D61C),                // cos 1e5
    (Cos, &[0x7149_F2CA], 0xBF1C_9222),                // cos 1e30
    (Cos, &[0x7F7F_FFFF], 0x3F5A_5F96),                // cos MAX
    (Cos, &[0x4049_0FDB], 0xBF80_0000),                // cos pi (as f32)
    (Cos, &[0x0001_16C2], 0x3F80_0000),                // cos 1e-40
    (Cos, &[0x8000_0000], 0x3F80_0000),                // cos -0
    (Cos, &[0x7F80_0000], 0x7FC0_0000),                // cos inf
    (Pow, &[0x4000_0000, 0x4120_0000], 0x4480_0000),   // 2^10
    (Pow, &[0x3F00_0000, 0x3F00_0000], 0x3F35_04F3),   // 0.5^0.5
    (Pow, &[0x3F33_3333, 0x4120_0000], 0x3CE7_6758),   // 0.7^10
    (Pow, &[0x3E99_999A, 0x400C_CCCD], 0x3D90_E048),   // 0.3^2.2
    (Pow, &[0x3F80_0347, 0x47C3_5000], 0x46AC_4808),   // 1.0001^1e5
    (Pow, &[0x4120_0000, 0xC040_0000], 0x3A83_126F),   // 10^-3
    (Pow, &[0x0000_0000, 0xBF80_0000], 0x7F80_0000),   // 0^-1
    (Pow, &[0xC100_0000, 0x3EAA_AAAB], 0x7FC0_0000),   // (-8)^(1/3)
    (Pow, &[0x7FC0_0000, 0x0000_0000], 0x3F80_0000),   // NaN^0
    (Atan2, &[0x3F80_0000, 0x3F80_0000], 0x3F49_0FDB), // atan2(1, 1)
    (Atan2, &[0x3F80_0000, 0xBF80_0000], 0x4016_CBE4), // atan2(1, -1)
    (Atan2, &[0x8000_0000, 0xBF80_0000], 0xC049_0FDB), // atan2(-0, -1)
    (Atan2, &[0x4040_0000, 0x4080_0000], 0x3F24_BC7D), // atan2(3, 4)
    (Atan2, &[0x0001_16C2, 0x7149_F2CA], 0x0000_0000), // atan2(1e-40, 1e30)
    (Atan2, &[0x7F80_0000, 0x7F80_0000], 0x3F49_0FDB), // atan2(inf, inf)
    (RoundHalfAway, &[0x3F00_0000], 0x3F80_0000),      // 0.5 -> 1
    (RoundHalfAway, &[0xBF00_0000], 0xBF80_0000),      // -0.5 -> -1
    (RoundHalfAway, &[0x3FC0_0000], 0x4000_0000),      // 1.5 -> 2
    (RoundHalfAway, &[0x4020_0000], 0x4040_0000),      // 2.5 -> 3 (not 2)
    (RoundHalfAway, &[0xC020_0000], 0xC040_0000),      // -2.5 -> -3
    (RoundHalfAway, &[0x3EFF_FFFF], 0x0000_0000),      // 0.49999997 -> 0
    (RoundHalfAway, &[0xBEFF_FFFF], 0x8000_0000),      // -0.49999997 -> -0
    (RoundHalfAway, &[0x4AFF_FFFF], 0x4B00_0000),      // 2^23 - 0.5 -> 2^23
    (RoundHalfAway, &[0xCAFF_FFFF], 0xCB00_0000),      // -(2^23 - 0.5) -> -2^23
    (RoundHalfAway, &[0x4B00_0001], 0x4B00_0001),      // 2^23 + 1 is unchanged
    (RoundHalfAway, &[0x4B7F_FFFF], 0x4B7F_FFFF),      // 2^24 - 1 is unchanged
    (RoundHalfAway, &[0xC0A6_6666], 0xC0A0_0000),      // -5.2 -> -5
    (RoundHalfAway, &[0x8000_0000], 0x8000_0000),      // -0 -> -0
    (RoundHalfAway, &[0x8001_0000], 0x8000_0000),      // -subnormal -> -0
    (RoundHalfAway, &[0x7F80_0000], 0x7F80_0000),      // inf
    (RoundHalfAway, &[0xFF80_0000], 0xFF80_0000),      // -inf
    (RoundHalfAway, &[0x7FC0_0000], 0x7FC0_0000),      // NaN
];

#[test]
fn golden_vectors_hold_on_every_platform() {
    let mut runners: Vec<(String, Runner)> = Vec::new();
    for &(op, args, want) in GOLDEN {
        let key = format!("{op:?}");
        let i = match runners.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => {
                runners.push((key, Runner::for_op(op)));
                runners.len() - 1
            }
        };
        let args: Vec<f32> = args.iter().copied().map(f32::from_bits).collect();
        let want = f32::from_bits(want);
        for (source, got) in [
            ("vm", runners[i].1.run_f32(&args)),
            ("math", op.math(&args)),
            ("libm", op.libm(&args)),
        ] {
            assert!(
                same(got, want),
                "{source} {op:?}{args:?}: got {got:e} ({:#010x}), want {want:e} ({:#010x})",
                got.to_bits(),
                want.to_bits()
            );
        }
    }
}

#[test]
fn round_half_away_f64_is_exact_and_matches_libm() {
    let mut runner = Runner::new(program(|asm| _ = asm.f64_round_half_away(4, 1), F64, F64));
    for (x, want) in [
        (0x3FE0_0000_0000_0000_u64, 0x3FF0_0000_0000_0000_u64), // 0.5 -> 1
        (0xBFE0_0000_0000_0000, 0xBFF0_0000_0000_0000),         // -0.5 -> -1
        (0x4004_0000_0000_0000, 0x4008_0000_0000_0000),         // 2.5 -> 3
        (0xC004_0000_0000_0000, 0xC008_0000_0000_0000),         // -2.5 -> -3
        (0x3FDF_FFFF_FFFF_FFFF, 0x0000_0000_0000_0000),         // 0.49999999999999994 -> 0
        (0xBFDF_FFFF_FFFF_FFFF, 0x8000_0000_0000_0000),         // -0.49999999999999994 -> -0
        (0x432F_FFFF_FFFF_FFFF, 0x4330_0000_0000_0000),         // 2^52 - 0.5 -> 2^52
        (0x4330_0000_0000_0001, 0x4330_0000_0000_0001),         // 2^52 + 1 is unchanged
        (0x433F_FFFF_FFFF_FFFF, 0x433F_FFFF_FFFF_FFFF),         // 2^53 - 1 is unchanged
        (0x8000_0000_0000_0000, 0x8000_0000_0000_0000),         // -0 -> -0
        (0x7FF0_0000_0000_0000, 0x7FF0_0000_0000_0000),         // inf
        (0xFFF0_0000_0000_0000, 0xFFF0_0000_0000_0000),         // -inf
    ] {
        let x = f64::from_bits(x);
        let Ok(Value::F64(got)) = runner.run([Value::F64(x), Value::F64(0.0), Value::F64(0.0)])
        else {
            panic!("{x:e}");
        };
        assert_eq!(got.to_bits(), want, "{x:e}");
        assert_eq!(math::round_half_away_f64(x).to_bits(), want, "{x:e}");
        assert_eq!(libm::round(x).to_bits(), want, "{x:e}");
    }
    // A NaN input keeps its sign and payload and is quieted, like the other rounding operations.
    let Ok(Value::F64(nan)) = runner.run([
        Value::F64(f64::from_bits(0xFFF4_0000_0000_0001)),
        Value::F64(0.0),
        Value::F64(0.0),
    ]) else {
        panic!()
    };
    assert_eq!(nan.to_bits(), 0xFFFC_0000_0000_0001);
}

fn clamp_runner() -> Runner {
    Runner::new(program(|asm| _ = asm.f32_clamp(4, 1, 2, 3), F32, F32))
}

#[test]
fn clamp_matches_rust_and_traps_on_unordered_bounds() {
    let mut runner = clamp_runner();
    let mut clamp =
        |x: f32, lo: f32, hi: f32| runner.run([Value::F32(x), Value::F32(lo), Value::F32(hi)]);
    let xs = [
        0.0,
        -0.0,
        0.5,
        1.0,
        -1.0,
        2.5,
        -1e30,
        f32::from_bits(1),
        f32::MAX,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        f32::from_bits(0xFFC0_1234),
    ];
    for &lo in &xs {
        for &hi in &xs {
            for &x in &xs {
                let got = clamp(x, lo, hi);
                if lo <= hi {
                    let Ok(Value::F32(got)) = got else {
                        panic!("clamp({x:e}, {lo:e}, {hi:e}): {got:?}")
                    };
                    assert_eq!(got.to_bits(), x.clamp(lo, hi).to_bits());
                } else {
                    assert!(
                        matches!(got, Err(Trap::ClampBoundsInvalid)),
                        "clamp({x:e}, {lo:e}, {hi:e}): {got:?}"
                    );
                }
            }
        }
    }
    // `x` itself is returned when in range: the sign of zero and NaN bits survive.
    let Ok(Value::F32(z)) = clamp(-0.0, 0.0, 1.0) else {
        panic!()
    };
    assert_eq!(z.to_bits(), 0x8000_0000);
    let Ok(Value::F32(n)) = clamp(f32::from_bits(0x7FA0_0001), 0.0, 1.0) else {
        panic!()
    };
    assert_eq!(n.to_bits(), 0x7FA0_0001);
    assert!(matches!(
        clamp(0.5, 0.0, f32::NAN),
        Err(Trap::ClampBoundsInvalid)
    ));
}

#[test]
fn clamp_traps_replay_from_a_recording() {
    let program = program(|asm| _ = asm.f32_clamp(4, 1, 2, 3), F32, F32);
    for (args, trapped) in [([0.5, 0.0, 1.0], false), ([0.5, 1.0, 0.0], true)] {
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(Arc::clone(&program)).unwrap();
        let args = args.map(Value::F32);
        let live = vm.run(
            FuncRef::new(unit, FuncId(0)),
            &args,
            recorder.mask(),
            Some(&mut recorder),
        );
        assert_eq!(live.is_err(), trapped);
        let recording = recorder.finish();
        drop(vm);
        match recording.replay().unwrap() {
            ReplayOutcome::Trapped(info) => {
                assert!(trapped);
                assert!(matches!(info.trap, Trap::ClampBoundsInvalid));
            }
            ReplayOutcome::Returned(_) => assert!(!trapped),
        }
    }
}

#[test]
fn intrinsics_disassemble_with_their_operands() {
    let mut asm = Asm::new();
    asm.set_register_types(vec![Unit, F32, F32, F32, F32]);
    asm.f32_pow(4, 1, 2)
        .f32_atan2(4, 1, 2)
        .f32_clamp(4, 1, 2, 3)
        .f32_round_half_away(4, 4)
        .ret(0, &[4]);
    let mut builder = ProgramBuilder::new();
    builder
        .push_function_checked(
            asm,
            FunctionSig {
                arg_types: vec![F32, F32, F32],
                ret_types: vec![F32],
            },
        )
        .unwrap();
    let program = builder.build();
    let text: Vec<String> = disassemble_function(&program, FuncId(0))
        .unwrap()
        .instrs()
        .map(|i| i.to_string())
        .collect();
    for (line, mnemonic) in
        text.iter()
            .zip(["f32.pow", "f32.atan2", "f32.clamp", "f32.round_half_away"])
    {
        assert!(line.contains(mnemonic), "{text:?}");
    }
    assert!(text[2].contains("r3"), "{text:?}");
    assert_eq!(Opcode::F32Pow.code(), 0xFDA0);
    assert_eq!(Opcode::F64RoundHalfAway.code(), 0xFC0B);
}

#[test]
fn verifier_rejects_f64_operands_to_f32_intrinsics() {
    let mut asm = Asm::new();
    asm.set_register_types(vec![Unit, F32, F64, F32]);
    asm.f32_pow(3, 1, 2).ret(0, &[3]);
    let mut builder = ProgramBuilder::new();
    builder
        .push_function_checked(
            asm,
            FunctionSig {
                arg_types: vec![F32, F64],
                ret_types: vec![F32],
            },
        )
        .unwrap();
    let err = builder.build_verified().unwrap_err();
    assert!(
        format!("{err:?}").contains("RegisterTypeMismatch"),
        "{err:?}"
    );
}
