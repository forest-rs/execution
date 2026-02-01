// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Internal register value representation.
//!
//! Public APIs (VM entry args, VM returns, host returns) use [`crate::value::Value`]. Internally,
//! the interpreter stores alloc-backed bytes/strings as compact handles into a VM-owned arena.

use crate::arena::{BytesHandle, StrHandle};
use crate::program::ValueType;
use crate::value::{AggHandle, Decimal, FuncId, Obj};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RegValue {
    Unit,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    Decimal(Decimal),
    Bytes(BytesHandle),
    Str(StrHandle),
    Obj(Obj),
    Agg(AggHandle),
    Func(FuncId),
}

impl RegValue {
    pub(crate) fn value_type(&self) -> ValueType {
        match self {
            Self::Unit => ValueType::Unit,
            Self::Bool(_) => ValueType::Bool,
            Self::I64(_) => ValueType::I64,
            Self::U64(_) => ValueType::U64,
            Self::F64(_) => ValueType::F64,
            Self::Decimal(_) => ValueType::Decimal,
            Self::Bytes(_) => ValueType::Bytes,
            Self::Str(_) => ValueType::Str,
            Self::Obj(Obj { host_type, .. }) => ValueType::Obj(*host_type),
            Self::Agg(_) => ValueType::Agg,
            Self::Func(_) => ValueType::Func,
        }
    }
}
