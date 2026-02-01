// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host ABI for `execution_tape`.
//!
//! The VM delegates effectful operations to an embedder-provided [`Host`].
//! Host calls are identified by a stable symbol string plus a signature hash.

use alloc::vec::Vec;
use core::fmt;

use crate::program::ValueType;
use crate::value::AggHandle;
use crate::value::Decimal;
use crate::value::FuncId;
use crate::value::Obj;
use crate::value::Value;

#[cfg(doc)]
use crate::program::Program;

/// A host-call signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSig {
    /// Argument value types.
    pub args: Vec<ValueType>,
    /// Return value types.
    pub rets: Vec<ValueType>,
}

/// A stable 64-bit signature hash.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SigHash(pub u64);

/// Computes a stable signature hash for `sig`.
///
/// v1 uses a fixed FNV-1a 64-bit hash over a canonical byte encoding.
#[must_use]
pub fn sig_hash(sig: &HostSig) -> SigHash {
    sig_hash_slices(&sig.args, &sig.rets)
}

/// Computes a stable signature hash from argument/return type slices.
#[must_use]
pub fn sig_hash_slices(args: &[ValueType], rets: &[ValueType]) -> SigHash {
    const PREFIX: &[u8] = b"execution_tape:v1\0";
    let mut h = Fnv1a64::new();
    h.update(PREFIX);

    h.update(&encode_u32(u32::try_from(args.len()).unwrap_or(u32::MAX)));
    for t in args {
        hash_value_type(&mut h, *t);
    }
    h.update(&encode_u32(u32::try_from(rets.len()).unwrap_or(u32::MAX)));
    for t in rets {
        hash_value_type(&mut h, *t);
    }

    SigHash(h.finish())
}

/// Errors a host call can return.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostError {
    /// The symbol is unknown to the host.
    UnknownSymbol,
    /// The signature hash does not match what the host expects for the symbol.
    SignatureMismatch,
    /// The host failed during execution.
    Failed,
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSymbol => write!(f, "unknown host symbol"),
            Self::SignatureMismatch => write!(f, "host signature mismatch"),
            Self::Failed => write!(f, "host call failed"),
        }
    }
}

impl core::error::Error for HostError {}

/// A borrowed host-call argument value.
///
/// This is a view into VM registers to avoid cloning alloc-backed values (e.g. bytes/strings) just
/// to pass them to the host.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ValueRef<'a> {
    /// `()`.
    Unit,
    /// Boolean.
    Bool(bool),
    /// Signed 64-bit integer.
    I64(i64),
    /// Unsigned 64-bit integer.
    U64(u64),
    /// 64-bit float.
    F64(f64),
    /// Decimal.
    Decimal(Decimal),
    /// Byte string.
    Bytes(&'a [u8]),
    /// UTF-8 string.
    Str(&'a str),
    /// Host object.
    Obj(Obj),
    /// Aggregate handle (tuple/struct/array).
    Agg(AggHandle),
    /// Function reference.
    Func(FuncId),
}

impl<'a> ValueRef<'a> {
    /// Converts this borrowed view into an owned [`Value`].
    ///
    /// This allocates for bytes/strings and is mainly intended for tests and simple hosts.
    #[must_use]
    pub fn to_value(self) -> Value {
        match self {
            Self::Unit => Value::Unit,
            Self::Bool(b) => Value::Bool(b),
            Self::I64(i) => Value::I64(i),
            Self::U64(u) => Value::U64(u),
            Self::F64(f) => Value::F64(f),
            Self::Decimal(d) => Value::Decimal(d),
            Self::Bytes(b) => Value::Bytes(b.to_vec()),
            Self::Str(s) => Value::Str(s.into()),
            Self::Obj(o) => Value::Obj(o),
            Self::Agg(h) => Value::Agg(h),
            Self::Func(f) => Value::Func(f),
        }
    }

    /// Returns the corresponding value type tag.
    #[must_use]
    pub fn value_type(self) -> ValueType {
        match self {
            Self::Unit => ValueType::Unit,
            Self::Bool(_) => ValueType::Bool,
            Self::I64(_) => ValueType::I64,
            Self::U64(_) => ValueType::U64,
            Self::F64(_) => ValueType::F64,
            Self::Decimal(_) => ValueType::Decimal,
            Self::Bytes(_) => ValueType::Bytes,
            Self::Str(_) => ValueType::Str,
            Self::Obj(o) => ValueType::Obj(o.host_type),
            Self::Agg(_) => ValueType::Agg,
            Self::Func(_) => ValueType::Func,
        }
    }
}

/// Host call interface.
///
/// The interpreter provides:
/// - the resolved `symbol` string (via
///   [`Program::symbol_str`])
/// - the signature hash (a [`SigHash`]) carried in bytecode
/// - argument values
///
/// The host returns:
/// - a list of return values (must match the declared return count)
/// - an optional additional fuel charge (charged by the VM)
pub trait Host {
    /// Performs a host call.
    fn call(
        &mut self,
        symbol: &str,
        sig_hash: SigHash,
        args: &[ValueRef<'_>],
    ) -> Result<(Vec<Value>, u64), HostError>;
}

#[derive(Copy, Clone, Debug)]
struct Fnv1a64(u64);

impl Fnv1a64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn finish(self) -> u64 {
        self.0
    }
}

fn encode_u32(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

fn encode_value_type_tag(t: ValueType) -> u8 {
    match t {
        ValueType::Any => 11,
        ValueType::Unit => 0,
        ValueType::Bool => 1,
        ValueType::I64 => 2,
        ValueType::U64 => 3,
        ValueType::F64 => 4,
        ValueType::Decimal => 5,
        ValueType::Bytes => 6,
        ValueType::Str => 7,
        ValueType::Obj(_) => 8,
        ValueType::Agg => 9,
        ValueType::Func => 10,
    }
}

fn hash_value_type(h: &mut Fnv1a64, t: ValueType) {
    h.update(&[encode_value_type_tag(t)]);
    if let ValueType::Obj(host_type) = t {
        h.update(&host_type.0.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn sig_hash_is_stable_for_same_sig() {
        let s1 = HostSig {
            args: vec![ValueType::I64, ValueType::Bool],
            rets: vec![ValueType::U64],
        };
        let s2 = s1.clone();
        assert_eq!(sig_hash(&s1), sig_hash(&s2));
    }

    #[test]
    fn sig_hash_changes_when_sig_changes() {
        let a = HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::U64],
        };
        let b = HostSig {
            args: vec![ValueType::I64, ValueType::Bool],
            rets: vec![ValueType::U64],
        };
        assert_ne!(sig_hash(&a), sig_hash(&b));
    }
}
