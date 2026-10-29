// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! An embedding-owned method table and shared object store.

use super::model::{CELL, Compiled, Failure, Result, receiver};
use execution_tape::{
    callable::{Callable, CodeSlot, CodeUpdate, CodeVersion},
    host::{
        AccessSink, Host, HostContext, HostError, ResourceKeyRef, SigHash, ValueRef,
        sig_hash_slices,
    },
    value::{Obj, Value},
};
use std::{cell::RefCell, error::Error, fmt, rc::Rc, sync::Arc};

pub(super) const METHODS: SigHash = SigHash(0x4D45_5448_4F44);
const CELLS: SigHash = SigHash(0x4345_4C4C);
#[derive(Default)]
pub(super) struct Counter {
    pub(super) value: i64,
    pub(super) native_calls: usize,
}
#[derive(Clone, Default)]
pub(super) struct Cells(pub(super) Rc<RefCell<Counter>>);
impl Host for Cells {
    fn call(
        &mut self,
        symbol: &str,
        signature: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        mut ctx: HostContext<'_, '_>,
    ) -> std::result::Result<u64, HostError> {
        if symbol != "CounterCell.add" {
            return Err(HostError::UnknownSymbol);
        }
        if signature
            != sig_hash_slices(
                &[
                    execution_tape::program::ValueType::Obj(CELL),
                    execution_tape::program::ValueType::I64,
                ],
                &[execution_tape::program::ValueType::I64],
            )
            || rets.len() != 1
        {
            return Err(HostError::SignatureMismatch);
        }
        let [ValueRef::Obj(object), ValueRef::I64(delta)] = args else {
            return Err(HostError::SignatureMismatch);
        };
        if *object != receiver() {
            return Err(HostError::custom(DispatchError::UnknownReceiver(*object)));
        }
        ctx.record_read(ResourceKeyRef::HostState {
            op: CELLS,
            key: object.handle.0,
        });
        let mut state = self.0.borrow_mut();
        let value = state
            .value
            .checked_add(*delta)
            .ok_or_else(|| HostError::custom(DispatchError::Overflow))?;
        state.value = value;
        state.native_calls += 1;
        ctx.record_write(ResourceKeyRef::HostState {
            op: CELLS,
            key: object.handle.0,
        });
        rets[0] = Value::I64(value);
        Ok(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Access {
    pub(super) write: bool,
    pub(super) namespace: SigHash,
    pub(super) key: u64,
}
#[derive(Clone, Default)]
pub(super) struct Ledger(pub(super) Rc<RefCell<Vec<Access>>>);
impl AccessSink for Ledger {
    fn read(&mut self, key: ResourceKeyRef<'_>) {
        if let ResourceKeyRef::HostState { op, key } = key {
            self.0.borrow_mut().push(Access {
                write: false,
                namespace: op,
                key,
            });
        }
    }
    fn write(&mut self, key: ResourceKeyRef<'_>) {
        if let ResourceKeyRef::HostState { op, key } = key {
            self.0.borrow_mut().push(Access {
                write: true,
                namespace: op,
                key,
            });
        }
    }
}

#[derive(Debug)]
pub(super) enum DispatchError {
    NoSuchMethod { receiver: Obj, selector: String },
    UnknownReceiver(Obj),
    Overflow,
    Cancelled,
    UnexpectedReply,
}
impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchMethod { receiver, selector } => write!(
                f,
                "Counter does not understand {selector:?} (receiver #{})",
                receiver.handle.0
            ),
            Self::UnknownReceiver(object) => {
                write!(f, "Counter receiver #{} is unavailable", object.handle.0)
            }
            Self::Overflow => f.write_str("Counter increment exceeds i64"),
            Self::Cancelled => f.write_str(
                "method left the live view without returning; its performed effects remain",
            ),
            Self::UnexpectedReply => {
                f.write_str("method did not return its declared integer result")
            }
        }
    }
}
impl Error for DispatchError {}
#[derive(Debug)]
pub(super) struct MethodFailure {
    pub(super) revision: u64,
    pub(super) cause: Failure,
}
impl fmt::Display for MethodFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Counter method revision {} failed: {}",
            self.revision, self.cause
        )
    }
}
impl Error for MethodFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

pub(super) struct Methods {
    pub(super) increment: CodeSlot,
}
impl Methods {
    pub(super) fn new(code: &Compiled) -> Result<Self> {
        Ok(Self {
            increment: CodeSlot::new(Callable::function(Arc::clone(&code.program), code.entry)?),
        })
    }
    pub(super) fn lookup(&self, object: Obj, selector: &str) -> Result<CodeVersion> {
        if object != receiver() {
            return Err(Box::new(DispatchError::UnknownReceiver(object)));
        }
        if selector != "increment" {
            return Err(Box::new(DispatchError::NoSuchMethod {
                receiver: object,
                selector: selector.into(),
            }));
        }
        Ok(self.increment.current())
    }
    pub(super) fn publish(&mut self, code: &Compiled) -> Result<()> {
        let update = self
            .increment
            .replace(Callable::function(Arc::clone(&code.program), code.entry)?)?;
        if !matches!(update, CodeUpdate::Replaced { .. }) {
            return Err("expected another method publication".into());
        }
        Ok(())
    }
}
