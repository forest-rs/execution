// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! One argument boundary accepts borrowed inputs, owned inputs, or registered values.

use super::*;
use alloc::borrow::Cow;
use core::ops::{Bound, Range, RangeBounds};

/// Inputs for a guest start or resumption.
///
/// Slices stay borrowed until durable publication needs ownership; vectors and arrays move.
/// Registered inputs preserve their actual nominal owner and are materialized inside the turn.
/// Only selected immediate payloads are copied, after charging the work allowance. Aggregate
/// graphs remain shared in this VM. Conversion never executes or collects guest storage.
pub struct Arguments<'a>(Input<'a>);
enum Input<'a> {
    Values {
        values: Cow<'a, [Value]>,
        owner: Option<Instance>,
    },
    Rooted {
        values: RootedValues,
        selection: Selection,
    },
}
#[derive(Debug)]
struct Selection {
    start: Bound<usize>,
    end: Bound<usize>,
}
impl Selection {
    fn range(&self, len: usize) -> Result<Range<usize>, RootError> {
        let invalid = || RootError::InvalidRange {
            start: self.start,
            end: self.end,
            len,
        };
        let start = match self.start {
            Bound::Included(index) => index,
            Bound::Excluded(index) => index.checked_add(1).ok_or_else(invalid)?,
            Bound::Unbounded => 0,
        };
        let end = match self.end {
            Bound::Included(index) => index.checked_add(1).ok_or_else(invalid)?,
            Bound::Excluded(index) => index,
            Bound::Unbounded => len,
        };
        if start > end || end > len {
            return Err(invalid());
        }
        Ok(start..end)
    }
}
impl From<Vec<Value>> for Arguments<'_> {
    fn from(values: Vec<Value>) -> Self {
        Self(Input::Values {
            values: Cow::Owned(values),
            owner: None,
        })
    }
}
impl<const N: usize> From<[Value; N]> for Arguments<'_> {
    fn from(values: [Value; N]) -> Self {
        Self::from(Vec::from(values))
    }
}
impl<'a> From<&'a [Value]> for Arguments<'a> {
    fn from(values: &'a [Value]) -> Self {
        Self(Input::Values {
            values: Cow::Borrowed(values),
            owner: None,
        })
    }
}
impl<'a, const N: usize> From<&'a [Value; N]> for Arguments<'a> {
    fn from(values: &'a [Value; N]) -> Self {
        Self::from(values.as_slice())
    }
}
impl<'a> From<&'a Vec<Value>> for Arguments<'a> {
    fn from(values: &'a Vec<Value>) -> Self {
        Self::from(values.as_slice())
    }
}
impl From<RootedValues> for Arguments<'_> {
    fn from(values: RootedValues) -> Self {
        Self(Input::Rooted {
            values,
            selection: Selection {
                start: Bound::Unbounded,
                end: Bound::Unbounded,
            },
        })
    }
}
impl From<&RootedValues> for Arguments<'_> {
    fn from(values: &RootedValues) -> Self {
        Self::from(values.clone())
    }
}
impl fmt::Debug for Arguments<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Input::Values { values, owner } => f
                .debug_struct("Arguments")
                .field("count", &values.len())
                .field("borrowed", &matches!(values, Cow::Borrowed(_)))
                .field("owner", owner)
                .finish(),
            Input::Rooted { values, selection } => f
                .debug_struct("Arguments")
                .field("values", values)
                .field("selection", selection)
                .finish(),
        }
    }
}
impl<'a> Arguments<'a> {
    /// Gives raw values their actual nominal code owner. Registered inputs retain their existing
    /// owner; this cannot reinterpret a registration. The originating VM checks store identity.
    #[must_use]
    pub fn in_instance(instance: &Instance, arguments: impl Into<Self>) -> Self {
        let mut arguments = arguments.into();
        if let Input::Values { owner, .. } = &mut arguments.0 {
            *owner = Some(instance.clone());
        }
        arguments
    }
    /// Selects registered inputs without borrowing the VM or copying their payloads.
    /// Range and ownership checks occur inside the receiving turn. This can be prepared before
    /// acquiring its exclusive lease, unlike a slice borrowed from [`Vm::resolve`].
    #[must_use]
    pub fn select(values: &RootedValues, range: impl RangeBounds<usize>) -> Arguments<'static> {
        Arguments(Input::Rooted {
            values: values.clone(),
            selection: Selection {
                start: range.start_bound().cloned(),
                end: range.end_bound().cloned(),
            },
        })
    }
    pub(super) fn validate<H: Host>(&self, vm: &Vm<H>) -> Result<(), RootError> {
        match &self.0 {
            Input::Rooted { values, selection } => {
                selection.range(vm.resolve(values)?.values().len())?;
            }
            Input::Values {
                owner: Some(owner), ..
            } if !vm.owns_instance(owner) => return Err(RootError::ForeignStore),
            _ => {}
        }
        Ok(())
    }
    pub(super) fn materialize<H: Host>(
        self,
        vm: &Vm<H>,
        budget: &mut Budget,
    ) -> Result<(Cow<'a, [Value]>, Option<Instance>), ControlError> {
        self.validate(vm).map_err(ControlError::Retention)?;
        match self.0 {
            Input::Values { values, owner } => Ok((values, owner)),
            Input::Rooted { values, selection } => {
                let view = vm.resolve(&values).map_err(ControlError::Retention)?;
                let values = &view.values()[selection
                    .range(view.values().len())
                    .map_err(ControlError::Retention)?];
                let bytes = values
                    .iter()
                    .try_fold(0_usize, |total, value| {
                        let size = match value {
                            Value::Str(text) => text.len(),
                            Value::Bytes(bytes) => bytes.len(),
                            _ => 0,
                        };
                        total.checked_add(size).ok_or(Trap::PayloadSizeOverflow)
                    })
                    .map_err(ControlError::Input)?;
                budget
                    .data
                    .charge_payload_bytes(bytes, 1)
                    .map_err(ControlError::Input)?;
                Ok((Cow::Owned(values.to_vec()), Some(view.instance().clone())))
            }
        }
    }
}
