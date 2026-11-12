// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Runtime value model for `execution_tape`.
//!
//! This is the representation used by the interpreter and host ABI. It is intentionally small and
//! `no_std + alloc` friendly.

use alloc::string::String;
use alloc::vec::Vec;

use crate::program::HostTypeId;

/// An opaque handle to a host-owned object.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObjHandle(pub u64);

/// A host object value.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Obj {
    /// Host-defined type id.
    pub host_type: HostTypeId,
    /// Host-defined handle.
    pub handle: ObjHandle,
}

/// A function identifier, local to one program.
///
/// Bytecode operands and program tables use this local index. Executable values qualify it with
/// the [`UnitId`] that interprets it; see [`FuncRef`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FuncId(pub u32);

/// One verified program installed in a VM store.
///
/// [`crate::vm::Vm::install`] assigns these in installation order. An ID belongs to the VM that
/// issued it: it does not retain that VM, and another VM may assign the same number to unrelated
/// code. Units are never removed, so an ID keeps its meaning for the lifetime of its VM.
///
/// The default is the first unit a VM installs.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnitId(pub u32);

/// A function in one installed unit: the store-qualified identity of executable code.
///
/// Calls, function values and closures carry this pair, so a value moving between independently
/// compiled units keeps its meaning. Like an [`AggHandle`], it is a plain store address: it
/// belongs to the VM that installed [`Self::unit`], and the VM validates it when it is used.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FuncRef {
    /// Installed unit whose program interprets [`Self::func`].
    pub unit: UnitId,
    /// Function index in that unit's program.
    pub func: FuncId,
}

/// An exception tag of one VM: the identity that `throw` raises and catch clauses compare.
///
/// A program's [`crate::program::TagId`]s name tags locally; installing the program binds each
/// to a `TagRef`. A tag the program defines is new for every install, while an imported tag
/// binds to a tag defined in the VM ([`crate::vm::Vm::define_tag`]), so every unit importing it
/// throws and catches the same exception. Like a [`UnitId`], it belongs to the VM that issued it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TagRef(pub u32);

impl FuncRef {
    /// Qualifies `func` with the unit whose program defines it.
    #[must_use]
    pub const fn new(unit: UnitId, func: FuncId) -> Self {
        Self { unit, func }
    }
}

/// A handle to an aggregate heap value.
///
/// Aggregates are immutable and structural; v1 stores them out-of-line in an aggregate heap.
/// A handle belongs to its originating heap. Copying it does not retain data or transfer it to
/// another heap. Host-call handles are borrowed; [`crate::host::HostContext::retain`] creates
/// an independent snapshot. Returned VM handles stay valid until the heap is explicitly cleared.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AggHandle(pub u32);

/// A closure value (function + captured environment).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Closure {
    /// The closure body, qualified with the unit that defines it.
    pub func: FuncRef,
    /// Captured immutable environment.
    pub env: AggHandle,
}

/// Exact decimal with a signed 128-bit mantissa and per-value scale.
///
/// Its numeric value is `mantissa × 10^(-scale)`. Construction preserves both parts:
/// equality and hashing distinguish `1.0` from `1.00`; the `dec.cmp` opcode compares numeric
/// values instead. Addition and subtraction are exact at the larger operand scale;
/// multiplication adds scales. Only `dec.rescale` and `dec.div` round, at an explicit scale and
/// [`DecimalRounding`] mode.
///
/// The mantissa uses byte-aligned storage to avoid imposing 16-byte alignment on
/// [`Value`] and inspection records. This is an implementation detail, not a stable ABI.
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct Decimal {
    mantissa: [u8; 16],
    scale: u8,
}

impl Decimal {
    /// Constructs a decimal without normalization or rounding.
    #[must_use]
    pub const fn new(mantissa: i128, scale: u8) -> Self {
        Self {
            mantissa: mantissa.to_le_bytes(),
            scale,
        }
    }

    /// Returns the signed integer mantissa.
    #[must_use]
    pub const fn mantissa(self) -> i128 {
        i128::from_le_bytes(self.mantissa)
    }

    /// Returns the base-10 scale (number of fractional digits).
    #[must_use]
    pub const fn scale(self) -> u8 {
        self.scale
    }

    pub(crate) fn packed(self) -> [u8; 17] {
        let mut bytes = [0; 17];
        bytes[..16].copy_from_slice(&self.mantissa);
        bytes[16] = self.scale;
        bytes
    }

    pub(crate) fn from_packed(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; 17] = bytes.try_into().ok()?;
        Some(Self {
            mantissa: bytes[..16].try_into().ok()?,
            scale: bytes[16],
        })
    }
}

impl core::fmt::Debug for Decimal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Decimal")
            .field("mantissa", &self.mantissa())
            .field("scale", &self.scale())
            .finish()
    }
}

/// How a decimal operation rounds when its result has fewer fractional digits than the exact
/// value (`dec.rescale` to a smaller scale, and `dec.div`).
///
/// Bytecode encodes the mode as a one-byte immediate ([`Self::byte`]); the verifier rejects
/// other byte values. Every mode rounds the exact value once: no intermediate result is rounded.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum DecimalRounding {
    /// To the nearest representable value; exact ties go to the even neighbor
    /// (`2.5 -> 2`, `3.5 -> 4`, `-2.5 -> -2`). IEEE 754 `roundTiesToEven`, "banker's rounding".
    HalfEven = 0,
    /// To the nearest representable value; exact ties go away from zero
    /// (`2.5 -> 3`, `-2.5 -> -3`). IEEE 754 `roundTiesToAway`, common commercial rounding.
    HalfAwayFromZero = 1,
    /// Toward zero, discarding the excess digits (`2.7 -> 2`, `-2.7 -> -2`).
    TowardZero = 2,
    /// Toward negative infinity (`2.7 -> 2`, `-2.1 -> -3`).
    Floor = 3,
    /// Toward positive infinity (`2.1 -> 3`, `-2.7 -> -2`).
    Ceiling = 4,
}

impl DecimalRounding {
    /// Every mode, in byte order.
    pub const ALL: [Self; 5] = [
        Self::HalfEven,
        Self::HalfAwayFromZero,
        Self::TowardZero,
        Self::Floor,
        Self::Ceiling,
    ];

    /// Returns the bytecode immediate for this mode.
    #[must_use]
    pub const fn byte(self) -> u8 {
        self as u8
    }

    /// Parses a bytecode immediate, returning `None` for bytes that name no mode.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            0 => Self::HalfEven,
            1 => Self::HalfAwayFromZero,
            2 => Self::TowardZero,
            3 => Self::Floor,
            4 => Self::Ceiling,
            _ => return None,
        })
    }

    /// Returns the mode's stable lowercase name, as used in disassembly (e.g. `half_even`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::HalfEven => "half_even",
            Self::HalfAwayFromZero => "half_away_from_zero",
            Self::TowardZero => "toward_zero",
            Self::Floor => "floor",
            Self::Ceiling => "ceiling",
        }
    }
}

/// A runtime value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
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
    Bytes(Vec<u8>),
    /// UTF-8 string.
    Str(String),
    /// Host object.
    Obj(Obj),
    /// Aggregate handle (tuple/struct/array).
    Agg(AggHandle),
    /// Function reference, qualified with the unit that defines it.
    Func(FuncRef),
    /// Closure reference (function + captured environment).
    Closure(Closure),
}

/// A struct or array shape interned in one aggregate heap.
///
/// Like an [`AggHandle`], it belongs to its heap: [`crate::aggregates::AggHeap::shape`] resolves
/// it to the structural [`crate::aggregates::AggShape`]. A VM interns each installed program's
/// struct and array types in its heap, so equal shapes from different units compare equal.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShapeId(pub u32);

/// Aggregate type descriptor for host signatures and reflection.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum AggType {
    /// Tuple with fixed arity.
    Tuple {
        /// Number of elements.
        arity: u32,
    },
    /// Struct of an interned shape.
    Struct {
        /// The struct's shape in its heap.
        shape: ShapeId,
    },
    /// Array of an interned shape.
    Array {
        /// The array's shape in its heap.
        shape: ShapeId,
    },
}
