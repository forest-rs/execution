// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Qualified runtime values and retained snapshots enter through one admission boundary.

use super::*;
use crate::{callable::Callable, value::FunctionRef};
use alloc::borrow::Cow;

/// A callable input to [`Turn::start`].
///
/// Qualified functions and closures require their originating VM. A retained [`Callable`]
/// explicitly authorizes importing its immutable captures into the receiving turn's store,
/// under that turn's remaining allowance and publication checkpoint. This input has no
/// separate execution lifecycle. Borrowing a retained callable lasts only through admission;
/// the selection can be dropped immediately after starting its exact code revision.
pub struct Callee<'a>(Target<'a>);
enum Target<'a> {
    Registered { values: RootedValues, index: usize },
    Qualified(Value),
    Retained(Cow<'a, Callable>),
}
impl From<FunctionRef> for Callee<'_> {
    fn from(function: FunctionRef) -> Self {
        Self(Target::Qualified(Value::Func(function)))
    }
}
impl From<&FunctionRef> for Callee<'_> {
    fn from(function: &FunctionRef) -> Self {
        Self::from(function.clone())
    }
}
impl From<Closure> for Callee<'_> {
    fn from(closure: Closure) -> Self {
        Self(Target::Qualified(Value::Closure(closure)))
    }
}
impl From<Value> for Callee<'_> {
    fn from(value: Value) -> Self {
        Self(Target::Qualified(value))
    }
}
impl From<Callable> for Callee<'_> {
    fn from(callable: Callable) -> Self {
        Self(Target::Retained(Cow::Owned(callable)))
    }
}
impl<'a> From<&'a Callable> for Callee<'a> {
    fn from(callable: &'a Callable) -> Self {
        Self(Target::Retained(Cow::Borrowed(callable)))
    }
}
impl fmt::Debug for Callee<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Target::Registered { values, index } => f
                .debug_struct("Callee")
                .field("values", values)
                .field("index", index)
                .finish(),
            Target::Qualified(value) => f.debug_tuple("Callee").field(value).finish(),
            Target::Retained(callable) => f.debug_tuple("Callee").field(callable).finish(),
        }
    }
}
impl Callee<'_> {
    /// Selects a registered callable without borrowing or copying the VM's current storage.
    /// Resolution happens during admission, after any intervening moving collection. The
    /// selection retains its group until then; invalid indices/kinds are recoverable denials.
    #[must_use]
    pub fn registered(values: &RootedValues, index: usize) -> Callee<'static> {
        Callee(Target::Registered {
            values: values.clone(),
            index,
        })
    }

    pub(super) fn prepare<H: Host>(
        self,
        vm: &mut Vm<H>,
        budget: &mut Budget,
    ) -> Result<(FunctionRef, Option<AggHandle>), ControlError> {
        let value = match self.0 {
            Target::Qualified(value) => value,
            Target::Registered { values, index } => {
                let view = vm.resolve(&values).map_err(ControlError::Retention)?;
                let value = view.values().get(index).ok_or_else(|| {
                    ControlError::Retention(RootError::InvalidIndex {
                        index,
                        len: view.values().len(),
                    })
                })?;
                match value {
                    Value::Func(function) => Value::Func(function.clone()),
                    Value::Closure(closure) => Value::Closure(closure.clone()),
                    other => {
                        return Err(ControlError::NotCallable {
                            actual: other.value_type(),
                        });
                    }
                }
            }
            Target::Retained(callable) => {
                if let Some(capture) = callable.capture() {
                    let work = DataWork {
                        aggregate: capture.aggregate_work,
                        payload: capture.payload_work,
                    };
                    // Admission precedes capture copying; repeated entries share remaining work.
                    budget.charge_import(work).map_err(ControlError::Input)?;
                    let mut values = vm
                        .import_capture(&capture.snapshot)
                        .map_err(ControlError::Input)?;
                    values.pop().expect("retained closure root")
                } else {
                    Value::Func(
                        vm.function(callable.program(), callable.function_id())
                            .map_err(|_| ControlError::Input(Trap::InstanceLimitExceeded))?,
                    )
                }
            }
        };
        let (function, environment) = match value {
            Value::Func(function) => (function, None),
            Value::Closure(closure) => (closure.func, Some(closure.env)),
            other => {
                return Err(ControlError::NotCallable {
                    actual: other.value_type(),
                });
            }
        };
        if !vm.owns_instance(function.instance()) {
            return Err(ControlError::ForeignStore);
        }
        Ok((function, environment))
    }
}
