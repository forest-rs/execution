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

/// Sink for recording resource accesses during execution.
///
/// This is an optional integration point intended for incremental execution systems (e.g.
/// `execution_graph`). A sink is provided to host calls via [`Host::call`].
///
/// ## Semantics
///
/// An [`AccessSink`] observes a *best-effort* stream of resource accesses during a run:
/// - [`AccessSink::read`] records a dependency (“this call’s result depended on that resource”).
/// - [`AccessSink::write`] records an invalidation source (“this call mutated that resource”).
///
/// Incremental executors use these events to decide what to re-run after external state changes:
/// a run that previously *read* a given key should be considered potentially stale after some run
/// *writes* the same key.
///
/// The core soundness contract is:
/// - If a host call’s output depends on some external state, it should record a `read(...)` for a
///   key that will also be used for future `write(...)` events when that external state changes.
///
/// If you record too few reads/writes, incremental execution may incorrectly reuse stale results
/// (unsound). If you record extra reads/writes, incremental execution may re-run more than
/// necessary (conservative but correct).
///
/// ## Choosing keys
///
/// [`ResourceKeyRef`] is intentionally small and allocation-free. If your sink needs ownership,
/// clone strings or intern structured keys on the sink side.
///
/// Good keys are:
/// - stable across program rebuilds that don’t change the underlying resource identity
/// - collision-resistant (especially for `u64`-based keys)
/// - scoped to a well-defined namespace so unrelated resources don’t alias
///
/// Avoid keys derived from VM register numbers, argument indices, or other incidental details:
/// those identities tend to shift as code changes, which makes caching/debugging brittle.
///
/// ## Example: recording host reads/writes
///
/// This example records a host call's external reads/writes without using `execution_graph` yet.
///
/// ```no_run
/// extern crate alloc;
///
/// use alloc::collections::BTreeMap;
/// use alloc::vec;
/// use alloc::vec::Vec;
///
/// use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
/// use execution_tape::host::{
///     sig_hash, AccessSink, Host, HostError, HostSig, ResourceKeyRef, SigHash, ValueRef,
/// };
/// use execution_tape::program::ValueType;
/// use execution_tape::trace::TraceMask;
/// use execution_tape::value::{FuncId, Value};
/// use execution_tape::vm::{ExecutionContext, Limits, Vm};
///
/// #[derive(Debug, Default)]
/// struct RecordingAccessSink {
///     reads: Vec<(SigHash, u64)>,
///     writes: Vec<(SigHash, u64)>,
/// }
///
/// impl AccessSink for RecordingAccessSink {
///     fn read(&mut self, key: ResourceKeyRef<'_>) {
///         if let ResourceKeyRef::HostState { op, key } = key {
///             self.reads.push((op, key));
///         }
///     }
///
///     fn write(&mut self, key: ResourceKeyRef<'_>) {
///         if let ResourceKeyRef::HostState { op, key } = key {
///             self.writes.push((op, key));
///         }
///     }
/// }
///
/// struct KvHost {
///     kv: BTreeMap<u64, i64>,
///     get_sig: SigHash,
///     set_sig: SigHash,
/// }
///
/// impl Host for KvHost {
///     fn call(
///         &mut self,
///         symbol: &str,
///         sig_hash: SigHash,
///         args: &[ValueRef<'_>],
///         rets: &mut [Value],
///         access: Option<&mut dyn AccessSink>,
///     ) -> Result<u64, HostError> {
///         match symbol {
///             "kv.get" => {
///                 if sig_hash != self.get_sig {
///                     return Err(HostError::SignatureMismatch);
///                 }
///                 let [ValueRef::U64(key)] = args else {
///                     return Err(HostError::Failed);
///                 };
///                 if let Some(a) = access {
///                     a.read(ResourceKeyRef::HostState {
///                         op: sig_hash,
///                         key: *key,
///                     });
///                 }
///                 let v = *self.kv.get(key).unwrap_or(&0);
///                 rets[0] = Value::I64(v);
///                 Ok(0)
///             }
///             "kv.set" => {
///                 if sig_hash != self.set_sig {
///                     return Err(HostError::SignatureMismatch);
///                 }
///                 let [ValueRef::U64(key), ValueRef::I64(value)] = args else {
///                     return Err(HostError::Failed);
///                 };
///                 if let Some(a) = access {
///                     a.write(ResourceKeyRef::HostState {
///                         op: sig_hash,
///                         key: *key,
///                     });
///                 }
///                 self.kv.insert(*key, *value);
///                 rets[0] = Value::Unit;
///                 Ok(0)
///             }
///             _ => Err(HostError::UnknownSymbol),
///         }
///     }
/// }
///
/// // Program: v = kv.get(42); kv.set(42, v + 1); return v + 1
/// let get_sig = HostSig {
///     args: vec![ValueType::U64],
///     rets: vec![ValueType::I64],
/// };
/// let set_sig = HostSig {
///     args: vec![ValueType::U64, ValueType::I64],
///     rets: vec![ValueType::Unit],
/// };
/// let get_hash = sig_hash(&get_sig);
/// let set_hash = sig_hash(&set_sig);
///
/// let mut pb = ProgramBuilder::new();
/// let get_host = pb.host_sig_for("kv.get", get_sig);
/// let set_host = pb.host_sig_for("kv.set", set_sig);
///
/// let mut a = Asm::new();
/// a.const_u64(1, 42); // r1 = 42
/// a.host_call(0, get_host, 0, &[1], &[2]); // r2 = kv.get(r1)
/// a.const_i64(3, 1); // r3 = 1
/// a.i64_add(4, 2, 3); // r4 = r2 + r3
/// a.host_call(0, set_host, 0, &[1, 4], &[5]); // r5 = kv.set(r1, r4)
/// a.ret(0, &[4]); // return r4
///
/// pb.push_function_checked(
///     a,
///     FunctionSig {
///         arg_types: vec![],
///         ret_types: vec![ValueType::I64],
///     },
/// )?;
/// let program = pb.build_verified()?;
///
/// let mut kv = BTreeMap::new();
/// kv.insert(42, 7);
/// let host = KvHost {
///     kv,
///     get_sig: get_hash,
///     set_sig: set_hash,
/// };
///
/// let mut vm = Vm::new(host, Limits::default());
/// let mut ctx = ExecutionContext::new();
/// let mut access = RecordingAccessSink::default();
///
/// let out = vm
///     .run_with_ctx(
///         &mut ctx,
///         &program,
///         FuncId(0),
///         &[],
///         TraceMask::NONE,
///         None,
///         Some(&mut access),
///     )
///     .unwrap();
///
/// assert_eq!(out, vec![Value::I64(8)]);
/// assert_eq!(access.reads.len(), 1);
/// assert_eq!(access.writes.len(), 1);
/// # Ok::<(), execution_tape::asm::BuildError>(())
/// ```
pub trait AccessSink {
    /// Records a read of `key` (a dependency edge).
    fn read(&mut self, key: ResourceKeyRef<'_>);
    /// Records a write of `key` (an invalidation source).
    fn write(&mut self, key: ResourceKeyRef<'_>);
}

/// A borrowed resource key used to model dependencies for incremental execution.
///
/// This type is intentionally small and allocation-free; sinks that need ownership should clone
/// the referenced strings.
///
/// ## Key kinds
///
/// - [`ResourceKeyRef::Input`] is for embedder-defined “semantic inputs”: values that are supplied
///   *from outside* the VM/host boundary (configuration, environment, request parameters, etc.).
///   The string is an embedder-chosen stable name.
///
/// - [`ResourceKeyRef::HostState`] is the main “precise” form for host-managed state.
///   It is explicitly namespaced by the host operation’s [`SigHash`], so different host ops can
///   reuse the same numeric `key` without colliding. The `key: u64` should identify *which*
///   piece of state was consulted/mutated for that operation (often a stable hash of a structured
///   key, or an intern id managed by the embedder).
///
/// - [`ResourceKeyRef::OpaqueHost`] is a conservative escape hatch for operations that depend on
///   (or mutate) host state but cannot (or choose not to) produce a more precise key.
///   Use it when the best you can say is “this call depends on *something* behind op X”.
///
///   The intended pattern is:
///   - record `read(OpaqueHost { op })` for calls whose outputs depend on opaque host state
///   - record `write(OpaqueHost { op })` for calls that may invalidate that opaque state
///
///   This is always safe (it may cause extra re-runs), and it provides a predictable stepping
///   stone until a host op can be keyed more precisely.
///
/// ## Read/write matching
///
/// Incremental systems treat keys as equal by simple structural equality. That means the
/// *caller* of [`AccessSink`] is responsible for consistency:
/// - If a later mutation should invalidate a prior dependency, they must use the same
///   `ResourceKeyRef` (same variant + same payload values).
/// - If you choose a `u64` hash key, collisions are “aliasing”: unrelated resources can spuriously
///   invalidate each other (conservative but may be costly). Prefer stable, collision-resistant
///   hashing or interning when it matters.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ResourceKeyRef<'a> {
    /// An external input by name (e.g. environment, provided parameter, or named input binding).
    Input(&'a str),
    /// Host state consulted by an operation, with a key namespace local to the host op.
    HostState {
        /// Host operation identifier.
        op: SigHash,
        /// Opaque per-op key identifying the consulted state.
        key: u64,
    },
    /// Conservative dependency for opaque host operations.
    OpaqueHost {
        /// Host operation identifier.
        op: SigHash,
    },
}

/// A host-call signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSig {
    /// Argument value types.
    pub args: Vec<ValueType>,
    /// Return value types.
    pub rets: Vec<ValueType>,
}

/// A stable 64-bit signature hash.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
/// - argument values as `args`
/// - a pre-sized `rets` slice (one slot per declared return value, pre-filled with
///   [`Value::Unit`])
///
/// The host writes return values into `rets` and returns an optional additional fuel charge
/// (charged by the VM). Every slot in `rets` whose declared type is not `Unit` **must** be
/// written; unwritten slots will fail the VM's post-call type check.
pub trait Host {
    /// Performs a host call.
    ///
    /// `rets` is pre-sized by the VM to match the declared return count and pre-filled with
    /// [`Value::Unit`]. The host must overwrite each slot with the correct return value.
    ///
    /// If `access` is present, the host should record any external reads and writes that are
    /// relevant for incremental execution.
    fn call(
        &mut self,
        symbol: &str,
        sig_hash: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        access: Option<&mut dyn AccessSink>,
    ) -> Result<u64, HostError>;
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
