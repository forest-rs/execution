// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The float determinism contract (`docs/v1_spec.md`, "Determinism and portability"), pinned to
//! literal bit patterns.
//!
//! Every expected result below is a constant computed outside this crate (exact rational
//! arithmetic rounded to nearest, ties to even), not by a Rust reference evaluated on the
//! machine running the test. CI runs this file on Linux, macOS, Windows and `wasm32-wasip1`, so a
//! platform that rounded, flushed a subnormal, fused an operation or saturated differently fails
//! here. Arithmetic NaN results only have to be NaN; the bit-preserving operations are checked
//! bit for bit, signaling NaNs included.

use std::sync::Arc;

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::codec::{Operand, encode_instruction};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::opcode::Opcode;
use execution_tape::program::{FunctionDef, Program, TypeTableDef, ValueType};
use execution_tape::recording::{Outcome, RecordLimits, RecordedValue, Recorder};
use execution_tape::trace::{TraceMask, TraceSink as _};
use execution_tape::value::{FuncId, FuncRef, Value};
use execution_tape::verifier::{VerifiedProgram, VerifyConfig, verify_program_owned};
use execution_tape::vm::{Limits, Trap, Vm};

use ValueType::{F32, F64, I32, I64, U32, U64};

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

fn f32b(bits: u32) -> Value {
    Value::F32(f32::from_bits(bits))
}

fn f64b(bits: u64) -> Value {
    Value::F64(f64::from_bits(bits))
}

/// What one instruction must produce.
enum Want {
    /// Exactly this value; floats compare by bits.
    Exact(Value),
    /// Some NaN of the operand width; payload and sign are unspecified.
    Nan,
    /// This trap, from an instruction whose result has this type.
    Trap(Trap, ValueType),
}

#[rustfmt::skip]
fn cases() -> Vec<(Opcode, Vec<Value>, Want)> {
    vec![
        (Opcode::F32Add, vec![f32b(0x3F800000), f32b(0x33800000)], Want::Exact(f32b(0x3F800000))),
        (Opcode::F32Add, vec![f32b(0x3F800001), f32b(0x33800000)], Want::Exact(f32b(0x3F800002))),
        (Opcode::F32Add, vec![f32b(0x3DCCCCCD), f32b(0x3E4CCCCD)], Want::Exact(f32b(0x3E99999A))),
        (Opcode::F32Add, vec![f32b(0x80000000), f32b(0x80000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Add, vec![f32b(0x00000000), f32b(0x80000000)], Want::Exact(f32b(0x00000000))),
        (Opcode::F32Add, vec![f32b(0x3F800000), f32b(0xBF800000)], Want::Exact(f32b(0x00000000))),
        (Opcode::F32Add, vec![f32b(0x00000001), f32b(0x00000001)], Want::Exact(f32b(0x00000002))),
        (Opcode::F32Sub, vec![f32b(0x00800000), f32b(0x00000001)], Want::Exact(f32b(0x007FFFFF))),
        (Opcode::F32Sub, vec![f32b(0x80000000), f32b(0x00000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Sub, vec![f32b(0x00000000), f32b(0x00000000)], Want::Exact(f32b(0x00000000))),
        (Opcode::F32Mul, vec![f32b(0x00800000), f32b(0x3F000000)], Want::Exact(f32b(0x00400000))),
        (Opcode::F32Mul, vec![f32b(0x00000001), f32b(0x3F000000)], Want::Exact(f32b(0x00000000))),
        (Opcode::F32Mul, vec![f32b(0x00000001), f32b(0x3FC00000)], Want::Exact(f32b(0x00000002))),
        (Opcode::F32Mul, vec![f32b(0xBF800000), f32b(0x00000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Mul, vec![f32b(0x3F800800), f32b(0x3F800800)], Want::Exact(f32b(0x3F801000))),
        (Opcode::F32Div, vec![f32b(0x3F800000), f32b(0x40400000)], Want::Exact(f32b(0x3EAAAAAB))),
        (Opcode::F32Div, vec![f32b(0x00800000), f32b(0x40400000)], Want::Exact(f32b(0x002AAAAB))),
        (Opcode::F32Add, vec![f32b(0x7F7FFFFF), f32b(0x7F7FFFFF)], Want::Exact(f32b(0x7F800000))),
        (Opcode::F32Add, vec![f32b(0x7F800000), f32b(0xFF800000)], Want::Nan),
        (Opcode::F32Mul, vec![f32b(0x00000000), f32b(0x7F800000)], Want::Nan),
        (Opcode::F32Div, vec![f32b(0x3F800000), f32b(0x00000000)], Want::Exact(f32b(0x7F800000))),
        (Opcode::F32Div, vec![f32b(0x3F800000), f32b(0x80000000)], Want::Exact(f32b(0xFF800000))),
        (Opcode::F32Div, vec![f32b(0x00000000), f32b(0x00000000)], Want::Nan),
        (Opcode::F64Add, vec![f64b(0x3FF0000000000000), f64b(0x3CA0000000000000)], Want::Exact(f64b(0x3FF0000000000000))),
        (Opcode::F64Add, vec![f64b(0x3FF0000000000001), f64b(0x3CA0000000000000)], Want::Exact(f64b(0x3FF0000000000002))),
        (Opcode::F64Add, vec![f64b(0x3FB999999999999A), f64b(0x3FC999999999999A)], Want::Exact(f64b(0x3FD3333333333334))),
        (Opcode::F64Add, vec![f64b(0x8000000000000000), f64b(0x8000000000000000)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F64Add, vec![f64b(0x0000000000000000), f64b(0x8000000000000000)], Want::Exact(f64b(0x0000000000000000))),
        (Opcode::F64Add, vec![f64b(0x3FF0000000000000), f64b(0xBFF0000000000000)], Want::Exact(f64b(0x0000000000000000))),
        (Opcode::F64Add, vec![f64b(0x0000000000000001), f64b(0x0000000000000001)], Want::Exact(f64b(0x0000000000000002))),
        (Opcode::F64Sub, vec![f64b(0x0010000000000000), f64b(0x0000000000000001)], Want::Exact(f64b(0x000FFFFFFFFFFFFF))),
        (Opcode::F64Sub, vec![f64b(0x8000000000000000), f64b(0x0000000000000000)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F64Mul, vec![f64b(0x0010000000000000), f64b(0x3FE0000000000000)], Want::Exact(f64b(0x0008000000000000))),
        (Opcode::F64Mul, vec![f64b(0x0000000000000001), f64b(0x3FE0000000000000)], Want::Exact(f64b(0x0000000000000000))),
        (Opcode::F64Mul, vec![f64b(0x0000000000000001), f64b(0x3FF8000000000000)], Want::Exact(f64b(0x0000000000000002))),
        (Opcode::F64Mul, vec![f64b(0xBFF0000000000000), f64b(0x0000000000000000)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F64Mul, vec![f64b(0x3FF0000000400000), f64b(0x3FF0000000400000)], Want::Exact(f64b(0x3FF0000000800000))),
        (Opcode::F64Div, vec![f64b(0x3FF0000000000000), f64b(0x4008000000000000)], Want::Exact(f64b(0x3FD5555555555555))),
        (Opcode::F64Div, vec![f64b(0x0010000000000000), f64b(0x4008000000000000)], Want::Exact(f64b(0x0005555555555555))),
        (Opcode::F64Add, vec![f64b(0x7FEFFFFFFFFFFFFF), f64b(0x7FEFFFFFFFFFFFFF)], Want::Exact(f64b(0x7FF0000000000000))),
        (Opcode::F64Add, vec![f64b(0x7FF0000000000000), f64b(0xFFF0000000000000)], Want::Nan),
        (Opcode::F64Mul, vec![f64b(0x0000000000000000), f64b(0x7FF0000000000000)], Want::Nan),
        (Opcode::F64Div, vec![f64b(0x3FF0000000000000), f64b(0x8000000000000000)], Want::Exact(f64b(0xFFF0000000000000))),
        (Opcode::F64Div, vec![f64b(0x0000000000000000), f64b(0x0000000000000000)], Want::Nan),
        (Opcode::F32Rem, vec![f32b(0x40B00000), f32b(0x40000000)], Want::Exact(f32b(0x3FC00000))),
        (Opcode::F32Rem, vec![f32b(0xC0B00000), f32b(0x40000000)], Want::Exact(f32b(0xBFC00000))),
        (Opcode::F32Rem, vec![f32b(0x40B00000), f32b(0xC0000000)], Want::Exact(f32b(0x3FC00000))),
        (Opcode::F32Rem, vec![f32b(0xC0800000), f32b(0x40000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Rem, vec![f32b(0x80000000), f32b(0x3F800000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Rem, vec![f32b(0x7F7FFFFF), f32b(0x40400000)], Want::Exact(f32b(0x00000000))),
        (Opcode::F32Rem, vec![f32b(0x7F7FFFFF), f32b(0x00000001)], Want::Exact(f32b(0x00000000))),
        (Opcode::F32Rem, vec![f32b(0x00C00000), f32b(0x00800000)], Want::Exact(f32b(0x00400000))),
        (Opcode::F32Rem, vec![f32b(0x3DCCCCCD), f32b(0x3CF5C28F)], Want::Exact(f32b(0x3C23D70E))),
        (Opcode::F32Rem, vec![f32b(0x7149F2CA), f32b(0x00079F51)], Want::Exact(f32b(0x00041A8B))),
        (Opcode::F32Rem, vec![f32b(0x3F800000), f32b(0x7F800000)], Want::Exact(f32b(0x3F800000))),
        (Opcode::F32Rem, vec![f32b(0x80000000), f32b(0xFF800000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Rem, vec![f32b(0x7F800000), f32b(0x3F800000)], Want::Nan),
        (Opcode::F32Rem, vec![f32b(0x3F800000), f32b(0x00000000)], Want::Nan),
        (Opcode::F64Rem, vec![f64b(0x4016000000000000), f64b(0x4000000000000000)], Want::Exact(f64b(0x3FF8000000000000))),
        (Opcode::F64Rem, vec![f64b(0xC016000000000000), f64b(0x4000000000000000)], Want::Exact(f64b(0xBFF8000000000000))),
        (Opcode::F64Rem, vec![f64b(0x4016000000000000), f64b(0xC000000000000000)], Want::Exact(f64b(0x3FF8000000000000))),
        (Opcode::F64Rem, vec![f64b(0xC010000000000000), f64b(0x4000000000000000)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F64Rem, vec![f64b(0x7FEFFFFFFFFFFFFF), f64b(0x4008000000000000)], Want::Exact(f64b(0x4000000000000000))),
        (Opcode::F64Rem, vec![f64b(0x7FEFFFFFFFFFFFFF), f64b(0x0000000000000001)], Want::Exact(f64b(0x0000000000000000))),
        (Opcode::F64Rem, vec![f64b(0x0018000000000000), f64b(0x0010000000000000)], Want::Exact(f64b(0x0008000000000000))),
        (Opcode::F64Rem, vec![f64b(0x3FB999999999999A), f64b(0x3F9EB851EB851EB8)], Want::Exact(f64b(0x3F847AE147AE1480))),
        (Opcode::F64Rem, vec![f64b(0x7E37E43C8800759C), f64b(0x000080DBD0164B2D)], Want::Exact(f64b(0x00000D8205F9C56E))),
        (Opcode::F64Rem, vec![f64b(0xFE37E43C8800759C), f64b(0x4008000000000000)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F64Rem, vec![f64b(0x4008000000000000), f64b(0xFFF0000000000000)], Want::Exact(f64b(0x4008000000000000))),
        (Opcode::F64Rem, vec![f64b(0x7FF0000000000000), f64b(0x3FF0000000000000)], Want::Nan),
        (Opcode::F64Rem, vec![f64b(0x3FF0000000000000), f64b(0x8000000000000000)], Want::Nan),
        (Opcode::F32Sqrt, vec![f32b(0x40000000)], Want::Exact(f32b(0x3FB504F3))),
        (Opcode::F32Sqrt, vec![f32b(0x40400000)], Want::Exact(f32b(0x3FDDB3D7))),
        (Opcode::F32Sqrt, vec![f32b(0x00000001)], Want::Exact(f32b(0x1A3504F3))),
        (Opcode::F32Sqrt, vec![f32b(0x00000004)], Want::Exact(f32b(0x1AB504F3))),
        (Opcode::F32Sqrt, vec![f32b(0x7F7FFFFF)], Want::Exact(f32b(0x5F7FFFFF))),
        (Opcode::F32Sqrt, vec![f32b(0x3C23D70A)], Want::Exact(f32b(0x3DCCCCCD))),
        (Opcode::F32Sqrt, vec![f32b(0x80000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Sqrt, vec![f32b(0x7F800000)], Want::Exact(f32b(0x7F800000))),
        (Opcode::F32Sqrt, vec![f32b(0xBF800000)], Want::Nan),
        (Opcode::F64Sqrt, vec![f64b(0x4000000000000000)], Want::Exact(f64b(0x3FF6A09E667F3BCD))),
        (Opcode::F64Sqrt, vec![f64b(0x0000000000000001)], Want::Exact(f64b(0x1E60000000000000))),
        (Opcode::F64Sqrt, vec![f64b(0x7FEFFFFFFFFFFFFF)], Want::Exact(f64b(0x5FEFFFFFFFFFFFFF))),
        (Opcode::F64Sqrt, vec![f64b(0x3F847AE147AE147B)], Want::Exact(f64b(0x3FB999999999999A))),
        (Opcode::F64Sqrt, vec![f64b(0x8000000000000000)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F64Sqrt, vec![f64b(0xFFF0000000000000)], Want::Nan),
        (Opcode::F32Floor, vec![f32b(0xBF000000)], Want::Exact(f32b(0xBF800000))),
        (Opcode::F32Floor, vec![f32b(0x3F000000)], Want::Exact(f32b(0x00000000))),
        (Opcode::F32Floor, vec![f32b(0x40200000)], Want::Exact(f32b(0x40000000))),
        (Opcode::F32Floor, vec![f32b(0x40600000)], Want::Exact(f32b(0x40400000))),
        (Opcode::F32Floor, vec![f32b(0xC0200000)], Want::Exact(f32b(0xC0400000))),
        (Opcode::F32Floor, vec![f32b(0xBF333333)], Want::Exact(f32b(0xBF800000))),
        (Opcode::F32Floor, vec![f32b(0x4AFFFFFF)], Want::Exact(f32b(0x4AFFFFFE))),
        (Opcode::F32Floor, vec![f32b(0x80000001)], Want::Exact(f32b(0xBF800000))),
        (Opcode::F32Floor, vec![f32b(0x00000001)], Want::Exact(f32b(0x00000000))),
        (Opcode::F64Floor, vec![f64b(0xBFE0000000000000)], Want::Exact(f64b(0xBFF0000000000000))),
        (Opcode::F64Floor, vec![f64b(0x4004000000000000)], Want::Exact(f64b(0x4000000000000000))),
        (Opcode::F64Floor, vec![f64b(0xC004000000000000)], Want::Exact(f64b(0xC008000000000000))),
        (Opcode::F64Floor, vec![f64b(0x432FFFFFFFFFFFFF)], Want::Exact(f64b(0x432FFFFFFFFFFFFE))),
        (Opcode::F64Floor, vec![f64b(0x3FDFFFFFFFFFFFFF)], Want::Exact(f64b(0x0000000000000000))),
        (Opcode::F64Floor, vec![f64b(0x8000000000000001)], Want::Exact(f64b(0xBFF0000000000000))),
        (Opcode::F32Floor, vec![f32b(0x80000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F64Floor, vec![f64b(0xFFF0000000000000)], Want::Exact(f64b(0xFFF0000000000000))),
        (Opcode::F32Ceil, vec![f32b(0xBF000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Ceil, vec![f32b(0x3F000000)], Want::Exact(f32b(0x3F800000))),
        (Opcode::F32Ceil, vec![f32b(0x40200000)], Want::Exact(f32b(0x40400000))),
        (Opcode::F32Ceil, vec![f32b(0x40600000)], Want::Exact(f32b(0x40800000))),
        (Opcode::F32Ceil, vec![f32b(0xC0200000)], Want::Exact(f32b(0xC0000000))),
        (Opcode::F32Ceil, vec![f32b(0xBF333333)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Ceil, vec![f32b(0x4AFFFFFF)], Want::Exact(f32b(0x4B000000))),
        (Opcode::F32Ceil, vec![f32b(0x80000001)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Ceil, vec![f32b(0x00000001)], Want::Exact(f32b(0x3F800000))),
        (Opcode::F64Ceil, vec![f64b(0xBFE0000000000000)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F64Ceil, vec![f64b(0x4004000000000000)], Want::Exact(f64b(0x4008000000000000))),
        (Opcode::F64Ceil, vec![f64b(0xC004000000000000)], Want::Exact(f64b(0xC000000000000000))),
        (Opcode::F64Ceil, vec![f64b(0x432FFFFFFFFFFFFF)], Want::Exact(f64b(0x4330000000000000))),
        (Opcode::F64Ceil, vec![f64b(0x3FDFFFFFFFFFFFFF)], Want::Exact(f64b(0x3FF0000000000000))),
        (Opcode::F64Ceil, vec![f64b(0x8000000000000001)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F32Ceil, vec![f32b(0x80000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F64Ceil, vec![f64b(0xFFF0000000000000)], Want::Exact(f64b(0xFFF0000000000000))),
        (Opcode::F32Trunc, vec![f32b(0xBF000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Trunc, vec![f32b(0x3F000000)], Want::Exact(f32b(0x00000000))),
        (Opcode::F32Trunc, vec![f32b(0x40200000)], Want::Exact(f32b(0x40000000))),
        (Opcode::F32Trunc, vec![f32b(0x40600000)], Want::Exact(f32b(0x40400000))),
        (Opcode::F32Trunc, vec![f32b(0xC0200000)], Want::Exact(f32b(0xC0000000))),
        (Opcode::F32Trunc, vec![f32b(0xBF333333)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Trunc, vec![f32b(0x4AFFFFFF)], Want::Exact(f32b(0x4AFFFFFE))),
        (Opcode::F32Trunc, vec![f32b(0x80000001)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Trunc, vec![f32b(0x00000001)], Want::Exact(f32b(0x00000000))),
        (Opcode::F64Trunc, vec![f64b(0xBFE0000000000000)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F64Trunc, vec![f64b(0x4004000000000000)], Want::Exact(f64b(0x4000000000000000))),
        (Opcode::F64Trunc, vec![f64b(0xC004000000000000)], Want::Exact(f64b(0xC000000000000000))),
        (Opcode::F64Trunc, vec![f64b(0x432FFFFFFFFFFFFF)], Want::Exact(f64b(0x432FFFFFFFFFFFFE))),
        (Opcode::F64Trunc, vec![f64b(0x3FDFFFFFFFFFFFFF)], Want::Exact(f64b(0x0000000000000000))),
        (Opcode::F64Trunc, vec![f64b(0x8000000000000001)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F32Trunc, vec![f32b(0x80000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F64Trunc, vec![f64b(0xFFF0000000000000)], Want::Exact(f64b(0xFFF0000000000000))),
        (Opcode::F32Nearest, vec![f32b(0xBF000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Nearest, vec![f32b(0x3F000000)], Want::Exact(f32b(0x00000000))),
        (Opcode::F32Nearest, vec![f32b(0x40200000)], Want::Exact(f32b(0x40000000))),
        (Opcode::F32Nearest, vec![f32b(0x40600000)], Want::Exact(f32b(0x40800000))),
        (Opcode::F32Nearest, vec![f32b(0xC0200000)], Want::Exact(f32b(0xC0000000))),
        (Opcode::F32Nearest, vec![f32b(0xBF333333)], Want::Exact(f32b(0xBF800000))),
        (Opcode::F32Nearest, vec![f32b(0x4AFFFFFF)], Want::Exact(f32b(0x4B000000))),
        (Opcode::F32Nearest, vec![f32b(0x80000001)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Nearest, vec![f32b(0x00000001)], Want::Exact(f32b(0x00000000))),
        (Opcode::F64Nearest, vec![f64b(0xBFE0000000000000)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F64Nearest, vec![f64b(0x4004000000000000)], Want::Exact(f64b(0x4000000000000000))),
        (Opcode::F64Nearest, vec![f64b(0xC004000000000000)], Want::Exact(f64b(0xC000000000000000))),
        (Opcode::F64Nearest, vec![f64b(0x432FFFFFFFFFFFFF)], Want::Exact(f64b(0x4330000000000000))),
        (Opcode::F64Nearest, vec![f64b(0x3FDFFFFFFFFFFFFF)], Want::Exact(f64b(0x0000000000000000))),
        (Opcode::F64Nearest, vec![f64b(0x8000000000000001)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F32Nearest, vec![f32b(0x80000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F64Nearest, vec![f64b(0xFFF0000000000000)], Want::Exact(f64b(0xFFF0000000000000))),
        (Opcode::F32Min, vec![f32b(0x00000000), f32b(0x80000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Min, vec![f32b(0x80000000), f32b(0x00000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32Min, vec![f32b(0x7FC00000), f32b(0x3F800000)], Want::Nan),
        (Opcode::F32Min, vec![f32b(0x3F800000), f32b(0x7FC00000)], Want::Nan),
        (Opcode::F32Min, vec![f32b(0x7FC00000), f32b(0x7FC00000)], Want::Nan),
        (Opcode::F32Max, vec![f32b(0x00000000), f32b(0x80000000)], Want::Exact(f32b(0x00000000))),
        (Opcode::F32Max, vec![f32b(0x80000000), f32b(0x00000000)], Want::Exact(f32b(0x00000000))),
        (Opcode::F32Max, vec![f32b(0x7FC00000), f32b(0x3F800000)], Want::Nan),
        (Opcode::F32Max, vec![f32b(0x3F800000), f32b(0x7FC00000)], Want::Nan),
        (Opcode::F32Max, vec![f32b(0x7FC00000), f32b(0x7FC00000)], Want::Nan),
        (Opcode::F32MinNum, vec![f32b(0x00000000), f32b(0x80000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32MinNum, vec![f32b(0x80000000), f32b(0x00000000)], Want::Exact(f32b(0x80000000))),
        (Opcode::F32MinNum, vec![f32b(0x7FC00000), f32b(0x3F800000)], Want::Exact(f32b(0x3F800000))),
        (Opcode::F32MinNum, vec![f32b(0x3F800000), f32b(0x7FC00000)], Want::Exact(f32b(0x3F800000))),
        (Opcode::F32MinNum, vec![f32b(0x7FC00000), f32b(0x7FC00000)], Want::Nan),
        (Opcode::F32MaxNum, vec![f32b(0x00000000), f32b(0x80000000)], Want::Exact(f32b(0x00000000))),
        (Opcode::F32MaxNum, vec![f32b(0x80000000), f32b(0x00000000)], Want::Exact(f32b(0x00000000))),
        (Opcode::F32MaxNum, vec![f32b(0x7FC00000), f32b(0x3F800000)], Want::Exact(f32b(0x3F800000))),
        (Opcode::F32MaxNum, vec![f32b(0x3F800000), f32b(0x7FC00000)], Want::Exact(f32b(0x3F800000))),
        (Opcode::F32MaxNum, vec![f32b(0x7FC00000), f32b(0x7FC00000)], Want::Nan),
        (Opcode::F64Min, vec![f64b(0x0000000000000000), f64b(0x8000000000000000)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F64Min, vec![f64b(0x8000000000000000), f64b(0x0000000000000000)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F64Min, vec![f64b(0x7FF8000000000000), f64b(0x3FF0000000000000)], Want::Nan),
        (Opcode::F64Min, vec![f64b(0x3FF0000000000000), f64b(0x7FF8000000000000)], Want::Nan),
        (Opcode::F64Min, vec![f64b(0x7FF8000000000000), f64b(0x7FF8000000000000)], Want::Nan),
        (Opcode::F64Max, vec![f64b(0x0000000000000000), f64b(0x8000000000000000)], Want::Exact(f64b(0x0000000000000000))),
        (Opcode::F64Max, vec![f64b(0x8000000000000000), f64b(0x0000000000000000)], Want::Exact(f64b(0x0000000000000000))),
        (Opcode::F64Max, vec![f64b(0x7FF8000000000000), f64b(0x3FF0000000000000)], Want::Nan),
        (Opcode::F64Max, vec![f64b(0x3FF0000000000000), f64b(0x7FF8000000000000)], Want::Nan),
        (Opcode::F64Max, vec![f64b(0x7FF8000000000000), f64b(0x7FF8000000000000)], Want::Nan),
        (Opcode::F64MinNum, vec![f64b(0x0000000000000000), f64b(0x8000000000000000)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F64MinNum, vec![f64b(0x8000000000000000), f64b(0x0000000000000000)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F64MinNum, vec![f64b(0x7FF8000000000000), f64b(0x3FF0000000000000)], Want::Exact(f64b(0x3FF0000000000000))),
        (Opcode::F64MinNum, vec![f64b(0x3FF0000000000000), f64b(0x7FF8000000000000)], Want::Exact(f64b(0x3FF0000000000000))),
        (Opcode::F64MinNum, vec![f64b(0x7FF8000000000000), f64b(0x7FF8000000000000)], Want::Nan),
        (Opcode::F64MaxNum, vec![f64b(0x0000000000000000), f64b(0x8000000000000000)], Want::Exact(f64b(0x0000000000000000))),
        (Opcode::F64MaxNum, vec![f64b(0x8000000000000000), f64b(0x0000000000000000)], Want::Exact(f64b(0x0000000000000000))),
        (Opcode::F64MaxNum, vec![f64b(0x7FF8000000000000), f64b(0x3FF0000000000000)], Want::Exact(f64b(0x3FF0000000000000))),
        (Opcode::F64MaxNum, vec![f64b(0x3FF0000000000000), f64b(0x7FF8000000000000)], Want::Exact(f64b(0x3FF0000000000000))),
        (Opcode::F64MaxNum, vec![f64b(0x7FF8000000000000), f64b(0x7FF8000000000000)], Want::Nan),
        (Opcode::F32Neg, vec![f32b(0x7FA00001)], Want::Exact(f32b(0xFFA00001))),
        (Opcode::F32Abs, vec![f32b(0xFFA00001)], Want::Exact(f32b(0x7FA00001))),
        (Opcode::F32CopySign, vec![f32b(0x7FA00001), f32b(0xBF800000)], Want::Exact(f32b(0xFFA00001))),
        (Opcode::F64Neg, vec![f64b(0x7FF0000000000001)], Want::Exact(f64b(0xFFF0000000000001))),
        (Opcode::F64Abs, vec![f64b(0xFFF0000000000001)], Want::Exact(f64b(0x7FF0000000000001))),
        (Opcode::F64CopySign, vec![f64b(0x7FF0000000000001), f64b(0xBFF0000000000000)], Want::Exact(f64b(0xFFF0000000000001))),
        (Opcode::F32ToBits, vec![f32b(0x7FA00001)], Want::Exact(Value::U32(0x7FA00001))),
        (Opcode::F32FromBits, vec![Value::U32(0x7FA00001)], Want::Exact(f32b(0x7FA00001))),
        (Opcode::F64ToBits, vec![f64b(0x7FF0000000000001)], Want::Exact(Value::U64(0x7FF0000000000001))),
        (Opcode::F64FromBits, vec![Value::U64(0x7FF0000000000001)], Want::Exact(f64b(0x7FF0000000000001))),
        (Opcode::F32Eq, vec![f32b(0x00000000), f32b(0x80000000)], Want::Exact(Value::Bool(true))),
        (Opcode::F32Eq, vec![f32b(0x7FC00000), f32b(0x7FC00000)], Want::Exact(Value::Bool(false))),
        (Opcode::F32Ne, vec![f32b(0x7FC00000), f32b(0x7FC00000)], Want::Exact(Value::Bool(true))),
        (Opcode::F32Lt, vec![f32b(0x7FC00000), f32b(0x00000000)], Want::Exact(Value::Bool(false))),
        (Opcode::F32Ge, vec![f32b(0x80000000), f32b(0x00000000)], Want::Exact(Value::Bool(true))),
        (Opcode::F32Gt, vec![f32b(0x00000000), f32b(0x7FC00000)], Want::Exact(Value::Bool(false))),
        (Opcode::F32Le, vec![f32b(0x80000000), f32b(0x00000000)], Want::Exact(Value::Bool(true))),
        (Opcode::F64Eq, vec![f64b(0x0000000000000000), f64b(0x8000000000000000)], Want::Exact(Value::Bool(true))),
        (Opcode::F64Eq, vec![f64b(0x7FF8000000000000), f64b(0x7FF8000000000000)], Want::Exact(Value::Bool(false))),
        (Opcode::F64Ne, vec![f64b(0x7FF8000000000000), f64b(0x7FF8000000000000)], Want::Exact(Value::Bool(true))),
        (Opcode::F64Lt, vec![f64b(0x7FF8000000000000), f64b(0x0000000000000000)], Want::Exact(Value::Bool(false))),
        (Opcode::F64Ge, vec![f64b(0x8000000000000000), f64b(0x0000000000000000)], Want::Exact(Value::Bool(true))),
        (Opcode::F64Gt, vec![f64b(0x0000000000000000), f64b(0x7FF8000000000000)], Want::Exact(Value::Bool(false))),
        (Opcode::F64Le, vec![f64b(0x8000000000000000), f64b(0x0000000000000000)], Want::Exact(Value::Bool(true))),
        (Opcode::F64ToF32, vec![f64b(0x3FF0000010000000)], Want::Exact(f32b(0x3F800000))),
        (Opcode::F64ToF32, vec![f64b(0x3FF0000010000001)], Want::Exact(f32b(0x3F800001))),
        (Opcode::F64ToF32, vec![f64b(0x3FF0000030000000)], Want::Exact(f32b(0x3F800002))),
        (Opcode::F64ToF32, vec![f64b(0x36A0000000000000)], Want::Exact(f32b(0x00000001))),
        (Opcode::F64ToF32, vec![f64b(0x3690000000000000)], Want::Exact(f32b(0x00000000))),
        (Opcode::F64ToF32, vec![f64b(0x3690000100000000)], Want::Exact(f32b(0x00000001))),
        (Opcode::F64ToF32, vec![f64b(0x36A8000000000000)], Want::Exact(f32b(0x00000002))),
        (Opcode::F64ToF32, vec![f64b(0x47EFFFFFF0000000)], Want::Exact(f32b(0x7F800000))),
        (Opcode::F64ToF32, vec![f64b(0x47EFFFFFEFFFFFFF)], Want::Exact(f32b(0x7F7FFFFF))),
        (Opcode::F64ToF32, vec![f64b(0x3FB999999999999A)], Want::Exact(f32b(0x3DCCCCCD))),
        (Opcode::F64ToF32, vec![f64b(0x358DEE7A4AD4B81F)], Want::Exact(f32b(0x00000000))),
        (Opcode::F64ToF32, vec![f64b(0xB58DEE7A4AD4B81F)], Want::Exact(f32b(0x80000000))),
        (Opcode::F64ToF32, vec![f64b(0x7FEFFFFFFFFFFFFF)], Want::Exact(f32b(0x7F800000))),
        (Opcode::F32ToF64, vec![f32b(0x00000001)], Want::Exact(f64b(0x36A0000000000000))),
        (Opcode::F32ToF64, vec![f32b(0x7F7FFFFF)], Want::Exact(f64b(0x47EFFFFFE0000000))),
        (Opcode::F32ToF64, vec![f32b(0x80000000)], Want::Exact(f64b(0x8000000000000000))),
        (Opcode::F32ToF64, vec![f32b(0x3DCCCCCD)], Want::Exact(f64b(0x3FB99999A0000000))),
        (Opcode::I32ToF32, vec![Value::I32(16777217)], Want::Exact(f32b(0x4B800000))),
        (Opcode::I32ToF32, vec![Value::I32(16777219)], Want::Exact(f32b(0x4B800002))),
        (Opcode::I32ToF32, vec![Value::I32(i32::MIN)], Want::Exact(f32b(0xCF000000))),
        (Opcode::I32ToF32, vec![Value::I32(2147483647)], Want::Exact(f32b(0x4F000000))),
        (Opcode::U32ToF32, vec![Value::U32(0xFFFFFFFF)], Want::Exact(f32b(0x4F800000))),
        (Opcode::U32ToF32, vec![Value::U32(0x01000001)], Want::Exact(f32b(0x4B800000))),
        (Opcode::I64ToF32, vec![Value::I64(16777217)], Want::Exact(f32b(0x4B800000))),
        (Opcode::I64ToF32, vec![Value::I64(9223372036854775807)], Want::Exact(f32b(0x5F000000))),
        (Opcode::I64ToF32, vec![Value::I64(1152921573326323713)], Want::Exact(f32b(0x5D800001))),
        (Opcode::I64ToF32, vec![Value::I64(-1152921573326323713)], Want::Exact(f32b(0xDD800001))),
        (Opcode::I64ToF32, vec![Value::I64(i64::MIN)], Want::Exact(f32b(0xDF000000))),
        (Opcode::U64ToF32, vec![Value::U64(0xFFFFFFFFFFFFFFFF)], Want::Exact(f32b(0x5F800000))),
        (Opcode::U64ToF32, vec![Value::U64(0x1000001000000001)], Want::Exact(f32b(0x5D800001))),
        (Opcode::I64ToF64, vec![Value::I64(9007199254740993)], Want::Exact(f64b(0x4340000000000000))),
        (Opcode::I64ToF64, vec![Value::I64(9007199254740995)], Want::Exact(f64b(0x4340000000000002))),
        (Opcode::I64ToF64, vec![Value::I64(9223372036854775807)], Want::Exact(f64b(0x43E0000000000000))),
        (Opcode::I64ToF64, vec![Value::I64(i64::MIN)], Want::Exact(f64b(0xC3E0000000000000))),
        (Opcode::U64ToF64, vec![Value::U64(0xFFFFFFFFFFFFFFFF)], Want::Exact(f64b(0x43F0000000000000))),
        (Opcode::U64ToF64, vec![Value::U64(0x0020000000000001)], Want::Exact(f64b(0x4340000000000000))),
        (Opcode::I32ToF64, vec![Value::I32(i32::MIN)], Want::Exact(f64b(0xC1E0000000000000))),
        (Opcode::U32ToF64, vec![Value::U32(0xFFFFFFFF)], Want::Exact(f64b(0x41EFFFFFFFE00000))),
        (Opcode::F64ToI64, vec![f64b(0x43E0000000000000)], Want::Trap(Trap::IntCastOverflow, I64)),
        (Opcode::F64ToI64, vec![f64b(0xC3E0000000000000)], Want::Exact(Value::I64(i64::MIN))),
        (Opcode::F64ToI64, vec![f64b(0x43DFFFFFFFFFFFFF)], Want::Exact(Value::I64(9223372036854774784))),
        (Opcode::F64ToI64, vec![f64b(0xBFFE666666666666)], Want::Exact(Value::I64(-1))),
        (Opcode::F64ToI64, vec![f64b(0x7FF8000000000000)], Want::Trap(Trap::FloatToIntInvalid, I64)),
        (Opcode::F64ToI64, vec![f64b(0xFFF0000000000000)], Want::Trap(Trap::FloatToIntInvalid, I64)),
        (Opcode::F64ToU64, vec![f64b(0x43F0000000000000)], Want::Trap(Trap::IntCastOverflow, U64)),
        (Opcode::F64ToU64, vec![f64b(0x43EFFFFFFFFFFFFF)], Want::Exact(Value::U64(0xFFFFFFFFFFFFF800))),
        (Opcode::F64ToI64Sat, vec![f64b(0x7FF8000000000000)], Want::Exact(Value::I64(0))),
        (Opcode::F64ToI64Sat, vec![f64b(0x7FF0000000000000)], Want::Exact(Value::I64(9223372036854775807))),
        (Opcode::F64ToI64Sat, vec![f64b(0xFE37E43C8800759C)], Want::Exact(Value::I64(i64::MIN))),
        (Opcode::F64ToU64Sat, vec![f64b(0xBFF8000000000000)], Want::Exact(Value::U64(0x0000000000000000))),
        (Opcode::F64ToU64Sat, vec![f64b(0x43F0000000000000)], Want::Exact(Value::U64(0xFFFFFFFFFFFFFFFF))),
        (Opcode::F32ToI32, vec![f32b(0x4F000000)], Want::Trap(Trap::IntCastOverflow, I32)),
        (Opcode::F32ToI32, vec![f32b(0xCF000000)], Want::Exact(Value::I32(i32::MIN))),
        (Opcode::F32ToI32, vec![f32b(0x7F800000)], Want::Trap(Trap::FloatToIntInvalid, I32)),
        (Opcode::F32ToU32, vec![f32b(0xBF000000)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F32ToU32, vec![f32b(0xBF800000)], Want::Trap(Trap::IntCastOverflow, U32)),
        (Opcode::F32ToI32Sat, vec![f32b(0x4EFFFFFF)], Want::Exact(Value::I32(2147483520))),
        (Opcode::F32ToI32Sat, vec![f32b(0x4F000000)], Want::Exact(Value::I32(2147483647))),
        (Opcode::F32ToI32Sat, vec![f32b(0x7FC00000)], Want::Exact(Value::I32(0))),
        (Opcode::F32ToU32Sat, vec![f32b(0x4F7FFFFF)], Want::Exact(Value::U32(0xFFFFFF00))),
        (Opcode::F32ToU32Sat, vec![f32b(0x4F800000)], Want::Exact(Value::U32(0xFFFFFFFF))),
        (Opcode::F32ToU32Sat, vec![f32b(0xFF800000)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F64ToI32, vec![f64b(0x41DFFFFFFFF9999A)], Want::Exact(Value::I32(2147483647))),
        (Opcode::F64ToI32, vec![f64b(0xC1E00000001CCCCD)], Want::Exact(Value::I32(i32::MIN))),
        (Opcode::F64ToI32, vec![f64b(0x41E0000000000000)], Want::Trap(Trap::IntCastOverflow, I32)),
        (Opcode::F64ToU32, vec![f64b(0x41EFFFFFFFFCCCCD)], Want::Exact(Value::U32(0xFFFFFFFF))),
        (Opcode::F64ToI32Sat, vec![f64b(0xC202A05F20000000)], Want::Exact(Value::I32(i32::MIN))),
        (Opcode::F64ToU32Sat, vec![f64b(0x4202A05F20000000)], Want::Exact(Value::U32(0xFFFFFFFF))),
        // Float-to-integer edges: every conversion truncates toward zero first, then range-checks
        // (trapping forms) or clamps (`_sat` forms). -0.5 and -0.9999999 truncate to -0.0, which
        // is in range for unsigned targets; -1.0, 2^N and NaN/inf are the first values out of it.
        (Opcode::F32ToI32, vec![f32b(0x80000000)], Want::Exact(Value::I32(0))),
        (Opcode::F32ToI32, vec![f32b(0xBF000000)], Want::Exact(Value::I32(0))),
        (Opcode::F32ToI32, vec![f32b(0xBF7FFFFE)], Want::Exact(Value::I32(0))),
        (Opcode::F32ToI32, vec![f32b(0xBF800000)], Want::Exact(Value::I32(-1))),
        (Opcode::F32ToI32, vec![f32b(0x4EFFFFFF)], Want::Exact(Value::I32(2147483520))),
        (Opcode::F32ToI32, vec![f32b(0xCF000001)], Want::Trap(Trap::IntCastOverflow, I32)),
        (Opcode::F32ToI32, vec![f32b(0x7FC00000)], Want::Trap(Trap::FloatToIntInvalid, I32)),
        (Opcode::F32ToI32, vec![f32b(0xFF800000)], Want::Trap(Trap::FloatToIntInvalid, I32)),
        (Opcode::F32ToI32Sat, vec![f32b(0x80000000)], Want::Exact(Value::I32(0))),
        (Opcode::F32ToI32Sat, vec![f32b(0xBF000000)], Want::Exact(Value::I32(0))),
        (Opcode::F32ToI32Sat, vec![f32b(0xBF7FFFFE)], Want::Exact(Value::I32(0))),
        (Opcode::F32ToI32Sat, vec![f32b(0xBF800000)], Want::Exact(Value::I32(-1))),
        (Opcode::F32ToI32Sat, vec![f32b(0x4F000000)], Want::Exact(Value::I32(i32::MAX))),
        (Opcode::F32ToI32Sat, vec![f32b(0xCF000000)], Want::Exact(Value::I32(i32::MIN))),
        (Opcode::F32ToI32Sat, vec![f32b(0xCF000001)], Want::Exact(Value::I32(i32::MIN))),
        (Opcode::F32ToI32Sat, vec![f32b(0x7F800000)], Want::Exact(Value::I32(i32::MAX))),
        (Opcode::F32ToI32Sat, vec![f32b(0xFF800000)], Want::Exact(Value::I32(i32::MIN))),
        (Opcode::F32ToU32, vec![f32b(0x80000000)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F32ToU32, vec![f32b(0xBF7FFFFE)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F32ToU32, vec![f32b(0x4F7FFFFF)], Want::Exact(Value::U32(0xFFFFFF00))),
        (Opcode::F32ToU32, vec![f32b(0x4F800000)], Want::Trap(Trap::IntCastOverflow, U32)),
        (Opcode::F32ToU32, vec![f32b(0x7FC00000)], Want::Trap(Trap::FloatToIntInvalid, U32)),
        (Opcode::F32ToU32, vec![f32b(0x7F800000)], Want::Trap(Trap::FloatToIntInvalid, U32)),
        (Opcode::F32ToU32, vec![f32b(0xFF800000)], Want::Trap(Trap::FloatToIntInvalid, U32)),
        (Opcode::F32ToU32Sat, vec![f32b(0x80000000)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F32ToU32Sat, vec![f32b(0xBF000000)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F32ToU32Sat, vec![f32b(0xBF7FFFFE)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F32ToU32Sat, vec![f32b(0xBF800000)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F32ToU32Sat, vec![f32b(0x7FC00000)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F32ToU32Sat, vec![f32b(0x7F800000)], Want::Exact(Value::U32(0xFFFFFFFF))),
        (Opcode::F64ToI32, vec![f64b(0x8000000000000000)], Want::Exact(Value::I32(0))),
        (Opcode::F64ToI32, vec![f64b(0xBFE0000000000000)], Want::Exact(Value::I32(0))),
        (Opcode::F64ToI32, vec![f64b(0xBFEFFFFFCA501ACB)], Want::Exact(Value::I32(0))),
        (Opcode::F64ToI32, vec![f64b(0xBFF0000000000000)], Want::Exact(Value::I32(-1))),
        (Opcode::F64ToI32, vec![f64b(0x41DFFFFFFFFFFFFF)], Want::Exact(Value::I32(i32::MAX))),
        (Opcode::F64ToI32, vec![f64b(0xC1E0000000000000)], Want::Exact(Value::I32(i32::MIN))),
        (Opcode::F64ToI32, vec![f64b(0xC1E0000000000001)], Want::Exact(Value::I32(i32::MIN))),
        (Opcode::F64ToI32, vec![f64b(0x7FF8000000000000)], Want::Trap(Trap::FloatToIntInvalid, I32)),
        (Opcode::F64ToI32, vec![f64b(0x7FF0000000000000)], Want::Trap(Trap::FloatToIntInvalid, I32)),
        (Opcode::F64ToI32, vec![f64b(0xFFF0000000000000)], Want::Trap(Trap::FloatToIntInvalid, I32)),
        (Opcode::F64ToI32Sat, vec![f64b(0x8000000000000000)], Want::Exact(Value::I32(0))),
        (Opcode::F64ToI32Sat, vec![f64b(0xBFE0000000000000)], Want::Exact(Value::I32(0))),
        (Opcode::F64ToI32Sat, vec![f64b(0xBFEFFFFFCA501ACB)], Want::Exact(Value::I32(0))),
        (Opcode::F64ToI32Sat, vec![f64b(0xBFF0000000000000)], Want::Exact(Value::I32(-1))),
        (Opcode::F64ToI32Sat, vec![f64b(0x41DFFFFFFFFFFFFF)], Want::Exact(Value::I32(i32::MAX))),
        (Opcode::F64ToI32Sat, vec![f64b(0x41E0000000000000)], Want::Exact(Value::I32(i32::MAX))),
        (Opcode::F64ToI32Sat, vec![f64b(0xC1E0000000000000)], Want::Exact(Value::I32(i32::MIN))),
        (Opcode::F64ToI32Sat, vec![f64b(0xC1E0000000000001)], Want::Exact(Value::I32(i32::MIN))),
        (Opcode::F64ToI32Sat, vec![f64b(0x7FF8000000000000)], Want::Exact(Value::I32(0))),
        (Opcode::F64ToI32Sat, vec![f64b(0x7FF0000000000000)], Want::Exact(Value::I32(i32::MAX))),
        (Opcode::F64ToI32Sat, vec![f64b(0xFFF0000000000000)], Want::Exact(Value::I32(i32::MIN))),
        (Opcode::F64ToU32, vec![f64b(0x8000000000000000)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F64ToU32, vec![f64b(0xBFE0000000000000)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F64ToU32, vec![f64b(0xBFEFFFFFCA501ACB)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F64ToU32, vec![f64b(0xBFF0000000000000)], Want::Trap(Trap::IntCastOverflow, U32)),
        (Opcode::F64ToU32, vec![f64b(0x41EFFFFFFFFFFFFF)], Want::Exact(Value::U32(0xFFFFFFFF))),
        (Opcode::F64ToU32, vec![f64b(0x41F0000000000000)], Want::Trap(Trap::IntCastOverflow, U32)),
        (Opcode::F64ToU32, vec![f64b(0x7FF8000000000000)], Want::Trap(Trap::FloatToIntInvalid, U32)),
        (Opcode::F64ToU32, vec![f64b(0x7FF0000000000000)], Want::Trap(Trap::FloatToIntInvalid, U32)),
        (Opcode::F64ToU32, vec![f64b(0xFFF0000000000000)], Want::Trap(Trap::FloatToIntInvalid, U32)),
        (Opcode::F64ToU32Sat, vec![f64b(0x8000000000000000)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F64ToU32Sat, vec![f64b(0xBFE0000000000000)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F64ToU32Sat, vec![f64b(0xBFEFFFFFCA501ACB)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F64ToU32Sat, vec![f64b(0xBFF0000000000000)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F64ToU32Sat, vec![f64b(0x41EFFFFFFFFFFFFF)], Want::Exact(Value::U32(0xFFFFFFFF))),
        (Opcode::F64ToU32Sat, vec![f64b(0x41F0000000000000)], Want::Exact(Value::U32(0xFFFFFFFF))),
        (Opcode::F64ToU32Sat, vec![f64b(0x7FF8000000000000)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F64ToU32Sat, vec![f64b(0x7FF0000000000000)], Want::Exact(Value::U32(0xFFFFFFFF))),
        (Opcode::F64ToU32Sat, vec![f64b(0xFFF0000000000000)], Want::Exact(Value::U32(0x00000000))),
        (Opcode::F64ToI64, vec![f64b(0x8000000000000000)], Want::Exact(Value::I64(0))),
        (Opcode::F64ToI64, vec![f64b(0xBFE0000000000000)], Want::Exact(Value::I64(0))),
        (Opcode::F64ToI64, vec![f64b(0xBFEFFFFFCA501ACB)], Want::Exact(Value::I64(0))),
        (Opcode::F64ToI64, vec![f64b(0xBFF0000000000000)], Want::Exact(Value::I64(-1))),
        (Opcode::F64ToI64, vec![f64b(0xC3E0000000000001)], Want::Trap(Trap::IntCastOverflow, I64)),
        (Opcode::F64ToI64, vec![f64b(0x7FF0000000000000)], Want::Trap(Trap::FloatToIntInvalid, I64)),
        (Opcode::F64ToI64Sat, vec![f64b(0x8000000000000000)], Want::Exact(Value::I64(0))),
        (Opcode::F64ToI64Sat, vec![f64b(0xBFE0000000000000)], Want::Exact(Value::I64(0))),
        (Opcode::F64ToI64Sat, vec![f64b(0xBFEFFFFFCA501ACB)], Want::Exact(Value::I64(0))),
        (Opcode::F64ToI64Sat, vec![f64b(0xBFF0000000000000)], Want::Exact(Value::I64(-1))),
        (Opcode::F64ToI64Sat, vec![f64b(0x43DFFFFFFFFFFFFF)], Want::Exact(Value::I64(9223372036854774784))),
        (Opcode::F64ToI64Sat, vec![f64b(0x43E0000000000000)], Want::Exact(Value::I64(i64::MAX))),
        (Opcode::F64ToI64Sat, vec![f64b(0xC3E0000000000000)], Want::Exact(Value::I64(i64::MIN))),
        (Opcode::F64ToI64Sat, vec![f64b(0xC3E0000000000001)], Want::Exact(Value::I64(i64::MIN))),
        (Opcode::F64ToI64Sat, vec![f64b(0x7FF0000000000000)], Want::Exact(Value::I64(i64::MAX))),
        (Opcode::F64ToI64Sat, vec![f64b(0xFFF0000000000000)], Want::Exact(Value::I64(i64::MIN))),
        (Opcode::F64ToU64, vec![f64b(0x8000000000000000)], Want::Exact(Value::U64(0x0000000000000000))),
        (Opcode::F64ToU64, vec![f64b(0xBFE0000000000000)], Want::Exact(Value::U64(0x0000000000000000))),
        (Opcode::F64ToU64, vec![f64b(0xBFEFFFFFCA501ACB)], Want::Exact(Value::U64(0x0000000000000000))),
        (Opcode::F64ToU64, vec![f64b(0xBFF0000000000000)], Want::Trap(Trap::IntCastOverflow, U64)),
        (Opcode::F64ToU64, vec![f64b(0x7FF8000000000000)], Want::Trap(Trap::FloatToIntInvalid, U64)),
        (Opcode::F64ToU64, vec![f64b(0x7FF0000000000000)], Want::Trap(Trap::FloatToIntInvalid, U64)),
        (Opcode::F64ToU64, vec![f64b(0xFFF0000000000000)], Want::Trap(Trap::FloatToIntInvalid, U64)),
        (Opcode::F64ToU64Sat, vec![f64b(0x8000000000000000)], Want::Exact(Value::U64(0x0000000000000000))),
        (Opcode::F64ToU64Sat, vec![f64b(0xBFE0000000000000)], Want::Exact(Value::U64(0x0000000000000000))),
        (Opcode::F64ToU64Sat, vec![f64b(0xBFEFFFFFCA501ACB)], Want::Exact(Value::U64(0x0000000000000000))),
        (Opcode::F64ToU64Sat, vec![f64b(0xBFF0000000000000)], Want::Exact(Value::U64(0x0000000000000000))),
        (Opcode::F64ToU64Sat, vec![f64b(0x43EFFFFFFFFFFFFF)], Want::Exact(Value::U64(0xFFFFFFFFFFFFF800))),
        (Opcode::F64ToU64Sat, vec![f64b(0x7FF8000000000000)], Want::Exact(Value::U64(0x0000000000000000))),
        (Opcode::F64ToU64Sat, vec![f64b(0x7FF0000000000000)], Want::Exact(Value::U64(0xFFFFFFFFFFFFFFFF))),
        (Opcode::F64ToU64Sat, vec![f64b(0xFFF0000000000000)], Want::Exact(Value::U64(0x0000000000000000))),
    ]
}

/// Builds `f(ins...) -> out` whose body is the one instruction, round-tripped through the wire
/// format.
fn program(op: Opcode, ins: &[ValueType], out: ValueType) -> Arc<VerifiedProgram> {
    let args: Vec<u32> = (1..).take(ins.len()).collect();
    let dst = u32::try_from(ins.len()).unwrap() + 1;
    let operands: Vec<Operand<'_>> = std::iter::once(dst)
        .chain(args.iter().copied())
        .map(Operand::Reg)
        .collect();
    let mut bytecode = Vec::new();
    encode_instruction(op, &operands, &mut bytecode).unwrap();
    encode_instruction(
        Opcode::Ret,
        &[Operand::Reg(0), Operand::RegList(&[dst])],
        &mut bytecode,
    )
    .unwrap();
    let mut register_types = vec![ValueType::Unit];
    register_types.extend(ins);
    register_types.push(out);
    let program = Program::new(
        vec![],
        vec![],
        vec![],
        TypeTableDef::default(),
        vec![FunctionDef {
            arg_types: ins.to_vec(),
            ret_types: vec![out],
            register_types,
            bytecode,
            spans: vec![],
        }],
    );
    let program = Program::decode(&program.encode()).unwrap();
    Arc::new(verify_program_owned(program, &VerifyConfig::default()).unwrap())
}

fn float_bits(value: &Value) -> Option<(ValueType, u64)> {
    match *value {
        Value::F32(x) => Some((F32, u64::from(x.to_bits()))),
        Value::F64(x) => Some((F64, x.to_bits())),
        _ => None,
    }
}

fn is_nan(value: &Value) -> bool {
    match *value {
        Value::F32(x) => x.is_nan(),
        Value::F64(x) => x.is_nan(),
        _ => false,
    }
}

#[test]
fn float_operations_produce_the_specified_bits() {
    let mut failures = Vec::new();
    let cases = cases();
    for (op, input, want) in &cases {
        let ins: Vec<ValueType> = input.iter().map(Value::value_type).collect();
        let out = match want {
            Want::Exact(value) => value.value_type(),
            Want::Nan => ins[0],
            Want::Trap(_, ty) => *ty,
        };
        let program = program(*op, &ins, out);
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(program).unwrap();
        let got = vm.run(FuncRef::new(unit, FuncId(0)), input, TraceMask::NONE, None);
        let ok = match (&got, want) {
            (Ok(got), Want::Exact(want)) => match (float_bits(&got[0]), float_bits(want)) {
                (Some(got), Some(want)) => got == want,
                _ => got[0] == *want,
            },
            (Ok(got), Want::Nan) => is_nan(&got[0]) && got[0].value_type() == out,
            (Err(got), Want::Trap(want, _)) => format!("{:?}", got.trap) == format!("{want:?}"),
            _ => false,
        };
        if !ok {
            let shown = |v: &Value| match float_bits(v) {
                Some((_, bits)) => format!("{v:?} = {bits:#x}"),
                None => format!("{v:?}"),
            };
            let got = match &got {
                Ok(values) => shown(&values[0]),
                Err(trap) => format!("{:?}", trap.trap),
            };
            let want = match want {
                Want::Exact(value) => shown(value),
                Want::Nan => "NaN".to_owned(),
                Want::Trap(trap, _) => format!("{trap:?}"),
            };
            let input: Vec<String> = input.iter().map(shown).collect();
            failures.push(format!(
                "{} {}: got {got}, want {want}",
                op.mnemonic(),
                input.join(", ")
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(cases.len() > 250, "{}", cases.len());
}

/// `fn(a, b, c) = a * b + c` as two instructions, for `ty`.
fn multiply_add(ty: ValueType) -> Arc<VerifiedProgram> {
    let mut asm = Asm::new();
    asm.set_register_types(vec![ValueType::Unit, ty, ty, ty, ty, ty]);
    if ty == F32 {
        asm.f32_mul(4, 1, 2).f32_add(5, 4, 3);
    } else {
        asm.f64_mul(4, 1, 2).f64_add(5, 4, 3);
    }
    asm.ret(0, &[5]);
    let mut builder = ProgramBuilder::new();
    builder
        .push_function_checked(
            asm,
            FunctionSig {
                arg_types: vec![ty; 3],
                ret_types: vec![ty],
            },
        )
        .unwrap();
    Arc::new(builder.build_verified().unwrap())
}

/// A product is rounded before the next instruction adds to it: nothing is fused.
///
/// `a * a` is `1 + 2^-k + 2^-2k` exactly, which rounds to `1 + 2^-k`; adding `-(1 + 2^-k)` then
/// gives `+0`. A fused multiply-add would keep the `2^-2k` term.
#[test]
fn products_are_rounded_before_they_are_added() {
    for (ty, a, c) in [
        // a = 1 + 2^-12, c = -(1 + 2^-11): fused, the result would be 2^-24.
        (F32, f32b(0x3F80_0800), f32b(0xBF80_1000)),
        // a = 1 + 2^-30, c = -(1 + 2^-29): fused, the result would be 2^-60.
        (
            F64,
            f64b(0x3FF0_0000_0040_0000),
            f64b(0xBFF0_0000_0080_0000),
        ),
    ] {
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(multiply_add(ty)).unwrap();
        let out = vm
            .run(
                FuncRef::new(unit, FuncId(0)),
                &[a.clone(), a, c],
                TraceMask::NONE,
                None,
            )
            .unwrap();
        assert_eq!(float_bits(&out[0]), Some((ty, 0)), "{ty:?}: {out:?}");
    }
}

/// Moves, constants, entry arguments, returns and recordings keep float bits exactly, signaling
/// NaNs included.
#[test]
fn moves_constants_and_recordings_keep_float_bits() {
    const SNAN32: u32 = 0x7FA0_0001;
    const SNAN64: u64 = 0xFFF0_0000_0000_0002;
    let mut asm = Asm::new();
    asm.set_register_types(vec![ValueType::Unit, F32, F64, F32, F64, F32, F64]);
    asm.mov(3, 1)
        .mov(4, 2)
        .const_f32_bits(5, SNAN32)
        .const_f64_bits(6, SNAN64)
        .ret(0, &[3, 4, 5, 6]);
    let mut builder = ProgramBuilder::new();
    builder
        .push_function_checked(
            asm,
            FunctionSig {
                arg_types: vec![F32, F64],
                ret_types: vec![F32, F64, F32, F64],
            },
        )
        .unwrap();
    let program = Program::decode(&builder.build().encode()).unwrap();
    let program = Arc::new(verify_program_owned(program, &VerifyConfig::default()).unwrap());
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let out = vm
        .run(
            FuncRef::new(unit, FuncId(0)),
            &[f32b(SNAN32 | 1 << 31), f64b(SNAN64 & !(1 << 63))],
            recorder.mask(),
            Some(&mut recorder),
        )
        .unwrap();
    let want = [
        (F32, u64::from(SNAN32 | 1 << 31)),
        (F64, SNAN64 & !(1 << 63)),
        (F32, u64::from(SNAN32)),
        (F64, SNAN64),
    ];
    let got: Vec<_> = out.iter().map(|v| float_bits(v).unwrap()).collect();
    assert_eq!(got, want);
    let record = recorder.finish();
    let Some(Outcome::Returned(values)) = record.outcome() else {
        panic!("expected a recorded return");
    };
    let recorded: Vec<_> = values
        .iter()
        .map(|v| match *v {
            RecordedValue::F32(bits) => (F32, u64::from(bits)),
            RecordedValue::F64(bits) => (F64, bits),
            ref other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(recorded, want);
}
