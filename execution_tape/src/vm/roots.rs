// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Values the embedding registers with a VM to keep them alive across runs and collections.
//!
//! A raw [`AggHandle`] is a heap address: [`Vm::collect`] moves aggregates and invalidates every
//! address copied out before it. A [`RootedValues`] handle is not an address. The VM keeps the
//! registered values, collection updates them where they move, and [`Vm::resolve`] reads them
//! back as they are now.

use super::*;
use alloc::sync::Weak;

/// A group of values registered with one VM, which keeps them alive until the handle goes.
///
/// [`Vm::root`] registers the values and returns this handle. Collection treats them as roots:
/// everything they reach survives, and [`Vm::collect`] updates their aggregate handles and
/// closure environments where those move. Read the current values with [`Vm::resolve`] after
/// every collection; a raw handle copied out of an earlier resolution is invalid once a
/// collection has run.
///
/// [`Vm::release`] unregisters the group at once and returns its values. Dropping the handle
/// unregisters it too, lazily: the next collection reclaims what only it kept alive. Dropping
/// never collects.
///
/// The handle does not borrow or retain its VM, so it can be stored anywhere the embedding
/// keeps state, and another VM rejects it with [`RootError::ForeignVm`].
#[must_use = "dropping the handle unregisters its values at the next collection"]
pub struct RootedValues {
    token: Arc<Token>,
    slot: u32,
}

/// Shared between a handle and its group: the group sees the handle drop through a weak count.
struct Token {
    /// The registering VM's identity.
    owner: Weak<()>,
}

impl fmt::Debug for RootedValues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootedValues")
            .field("slot", &self.slot)
            .finish_non_exhaustive()
    }
}

/// Why a VM refused a root registration or lookup. Nothing was registered or changed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RootError {
    /// The handle was registered with another VM.
    ForeignVm,
    /// A value names an aggregate (or closure environment) this VM's heap does not hold.
    DanglingHandle {
        /// Position of the value in the registered group.
        index: usize,
        /// The missing aggregate.
        handle: AggHandle,
    },
    /// A function or closure value names a unit this VM did not install.
    UnknownUnit {
        /// Position of the value in the registered group.
        index: usize,
        /// The unknown unit.
        unit: UnitId,
    },
    /// A function or closure value names a function its unit's program does not define.
    UnknownFunction {
        /// Position of the value in the registered group.
        index: usize,
        /// The missing function.
        function: FuncRef,
    },
}

impl fmt::Display for RootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignVm => f.write_str("rooted values belong to another VM"),
            Self::DanglingHandle { index, handle } => write!(
                f,
                "value #{index} names aggregate {} which this VM does not hold",
                handle.0
            ),
            Self::UnknownFunction { index, function } => write!(
                f,
                "value #{index} names function #{} which unit {} does not define",
                function.func, function.unit
            ),
            Self::UnknownUnit { index, unit } => {
                write!(
                    f,
                    "value #{index} names unit {unit} which this VM did not install"
                )
            }
        }
    }
}

impl core::error::Error for RootError {}

struct Group {
    token: Weak<Token>,
    values: Vec<Value>,
}

impl Group {
    /// Whether a handle still owns the group.
    fn held(&self) -> bool {
        self.token.strong_count() != 0
    }
}

/// The registered groups of one VM.
#[derive(Default)]
pub(super) struct RootTable {
    groups: Vec<Option<Group>>,
    free: Vec<u32>,
}

impl RootTable {
    /// Registers `values` for the VM whose identity is `owner`.
    fn insert(&mut self, owner: &Arc<()>, values: Vec<Value>) -> RootedValues {
        let token = Arc::new(Token {
            owner: Arc::downgrade(owner),
        });
        let group = Group {
            token: Arc::downgrade(&token),
            values,
        };
        let slot = if let Some(slot) = self.free.pop() {
            self.groups[slot as usize] = Some(group);
            slot
        } else {
            let slot = u32::try_from(self.groups.len()).expect("root groups fit u32");
            self.groups.push(Some(group));
            slot
        };
        RootedValues { token, slot }
    }

    /// The group `root` names, if it belongs to the VM whose identity is `owner`.
    fn group(&self, owner: &Arc<()>, root: &RootedValues) -> Result<&Group, RootError> {
        if !core::ptr::eq(root.token.owner.as_ptr(), Arc::as_ptr(owner)) {
            return Err(RootError::ForeignVm);
        }
        // A handle of this VM always names its own live group: the handle keeps the token
        // allocation alive, so no later group can share its address.
        let group = self.groups[root.slot as usize]
            .as_ref()
            .expect("a live handle's group stays registered");
        debug_assert!(
            core::ptr::eq(group.token.as_ptr(), Arc::as_ptr(&root.token)),
            "the slot holds this handle's group"
        );
        Ok(group)
    }

    fn remove(&mut self, slot: u32) -> Vec<Value> {
        let group = self.groups[slot as usize]
            .take()
            .expect("a live handle's group stays registered");
        self.free.push(slot);
        group.values
    }

    /// Registered groups, including ones whose handle was dropped since the last prune.
    pub(super) fn len(&self) -> usize {
        self.groups.len() - self.free.len()
    }

    /// Groups still held by a handle.
    pub(super) fn held(&self) -> impl Iterator<Item = &[Value]> {
        self.groups
            .iter()
            .flatten()
            .filter(|group| group.held())
            .map(|group| group.values.as_slice())
    }

    /// Values of every held group, mutably, for relocation.
    pub(super) fn held_mut(&mut self) -> impl Iterator<Item = &mut Vec<Value>> {
        self.groups
            .iter_mut()
            .flatten()
            .filter(|group| group.held())
            .map(|group| &mut group.values)
    }

    /// Unregisters every group whose handle was dropped, returning how many.
    pub(super) fn prune(&mut self) -> usize {
        let mut released = 0;
        for (slot, entry) in self.groups.iter_mut().enumerate() {
            if entry.as_ref().is_some_and(|group| !group.held()) {
                *entry = None;
                self.free
                    .push(u32::try_from(slot).expect("root groups fit u32"));
                released += 1;
            }
        }
        released
    }
}

impl<H: Host> Vm<H> {
    /// Registers `values` to keep them, and every aggregate they reach, alive across runs and
    /// collections.
    ///
    /// The values move into the VM; byte and string payloads are not copied. Aggregate handles
    /// and closure environments must name aggregates of this VM's heap, and function and
    /// closure values functions of its installed units. A refused registration changes nothing.
    ///
    /// Register values between runs, for example a run's outputs, which are valid until the
    /// next collection. Read them back with [`Self::resolve`].
    pub fn root(&mut self, values: Vec<Value>) -> Result<RootedValues, RootError> {
        // A forgotten invocation's aggregates go first, so no group can keep them.
        self.settle();
        for (index, value) in values.iter().enumerate() {
            if let Some(handle) = crate::aggregates::aggregate_handle(value)
                && self.memory.aggs().agg_type(handle).is_err()
            {
                return Err(RootError::DanglingHandle { index, handle });
            }
            let function = match value {
                Value::Func(function) => *function,
                Value::Closure(closure) => closure.func,
                _ => continue,
            };
            let Some(unit) = self.units.get(function.unit) else {
                return Err(RootError::UnknownUnit {
                    index,
                    unit: function.unit,
                });
            };
            if unit.program.program().function(function.func).is_none() {
                return Err(RootError::UnknownFunction { index, function });
            }
        }
        Ok(self.roots.insert(&self.identity, values))
    }

    /// Borrows the current values of a registered group.
    ///
    /// Aggregate handles in them are valid until the next collection, which may move them;
    /// resolve again afterwards rather than keeping copies. The borrow does not depend on the
    /// handle, only on the VM. A group never reaches what a forgotten invocation allocated:
    /// registering one discards those aggregates first.
    pub fn resolve(&self, root: &RootedValues) -> Result<&[Value], RootError> {
        Ok(&self.roots.group(&self.identity, root)?.values)
    }

    /// Unregisters a group and returns its current values.
    ///
    /// The returned handles stay valid until the next collection, which reclaims what only the
    /// group kept alive unless the values are registered again.
    pub fn release(&mut self, root: RootedValues) -> Result<Vec<Value>, RootError> {
        self.settle();
        self.roots.group(&self.identity, &root)?;
        Ok(self.roots.remove(root.slot))
    }
}
