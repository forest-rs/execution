// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Report public value layout on this target; these sizes are not a stable ABI.

use execution_tape::{
    host::ValueRef,
    program::{Const, ConstEntry},
    value::{AggHandle, Closure, Decimal, FuncId, Obj, Value},
};

#[allow(
    dead_code,
    reason = "layout-only models; no arithmetic or ownership behavior is implemented"
)]
mod models {
    use super::{AggHandle, Closure, FuncId, Obj};

    pub(super) struct WideNative {
        mantissa: i128,
        scale: u8,
    }
    pub(super) struct WideLimbs {
        low: u64,
        high: i64,
        scale: u8,
    }
    pub(super) enum Owned<D> {
        Unit,
        Bool(bool),
        I64(i64),
        U64(u64),
        F64(f64),
        Decimal(D),
        Bytes(Vec<u8>),
        Str(String),
        Obj(Obj),
        Agg(AggHandle),
        Func(FuncId),
        Closure(Closure),
    }
    pub(super) enum Borrowed<'a, D> {
        Unit,
        Bool(bool),
        I64(i64),
        U64(u64),
        F64(f64),
        Decimal(D),
        Bytes(&'a [u8]),
        Str(&'a str),
        Obj(Obj),
        Agg(AggHandle),
        Func(FuncId),
        Closure(Closure),
    }
}

fn row<T>(name: &str) {
    println!(
        "{name:16} {:>5} bytes  alignment {}",
        size_of::<T>(),
        align_of::<T>()
    );
}

fn main() {
    println!(
        "Public value layout (target {}, pointer width {}):",
        std::env::consts::ARCH,
        usize::BITS
    );
    row::<Decimal>("Decimal");
    row::<Value>("Value");
    row::<ValueRef<'_>>("ValueRef");
    row::<Const>("Const");
    row::<ConstEntry>("ConstEntry");
    assert_eq!(size_of::<Value>(), size_of::<models::Owned<Decimal>>());
    assert_eq!(
        size_of::<ValueRef<'_>>(),
        size_of::<models::Borrowed<'_, Decimal>>()
    );
    println!("\nCandidate layouts only (no arithmetic, allocator, or throughput measurement):");
    row::<models::WideNative>("native i128");
    row::<models::Owned<models::WideNative>>("owned/native");
    row::<models::Borrowed<'_, models::WideNative>>("view/native copy");
    row::<models::WideLimbs>("two u64 limbs");
    row::<models::Owned<models::WideLimbs>>("owned/limbs");
    row::<models::Borrowed<'_, models::WideLimbs>>("view/limbs copy");
    row::<models::Borrowed<'_, &models::WideLimbs>>("view/limbs ref");
}
