// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Store-bound namespaces for shared verified code.
//!
//! A definition describes immutable code. An instance gives that definition an identity in
//! one VM store. Hosts can use the instance as a key for state or import routing; the VM still
//! owns mutable host state and values. Instance identity does not make raw value handles portable.

use crate::{verifier::VerifiedProgram, vm::TrapInfo};
use alloc::sync::Arc;
use core::{
    fmt,
    hash::{Hash, Hasher},
};

/// Verified code instantiated in one VM store.
///
/// Clones retain the same instance. Instantiating the same definition twice creates distinct
/// identities. Equality and hashing compare instance identity, independently of code equality.
/// Keeping this handle retains code and identity, not the VM, its host state or its heap.
/// Use [`Self::start`] in the originating VM; a foreign VM rejects entry before executing code.
#[derive(Clone)]
pub struct Instance(pub(crate) Arc<InstanceData>);

pub(crate) struct InstanceData {
    pub(crate) program: VerifiedProgram,
    pub(crate) store: Arc<()>,
    pub(crate) slot: u32,
    pub(crate) is_default: bool,
}

impl Instance {
    pub(crate) fn new(
        program: VerifiedProgram,
        store: Arc<()>,
        slot: u32,
        is_default: bool,
    ) -> Self {
        Self(Arc::new(InstanceData {
            program,
            store,
            slot,
            is_default,
        }))
    }

    /// The exact immutable definition retained by this instance.
    #[must_use]
    pub fn program(&self) -> &VerifiedProgram {
        &self.0.program
    }
}

impl PartialEq for Instance {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for Instance {}
impl Hash for Instance {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}
impl fmt::Debug for Instance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Instance")
            .field("program", &self.program().program().name())
            .field("store_slot", &self.0.slot)
            .finish_non_exhaustive()
    }
}

/// Why an instance could not be created or entered.
#[derive(Clone, Debug)]
pub enum InstanceError {
    /// The instance belongs to another VM. Entry spends no guest work or host effects.
    ForeignStore,
    /// The VM cannot represent another live instance.
    CapacityExceeded,
    /// Entry was admitted, but its arguments, contracts or execution limits were rejected.
    Trapped(TrapInfo),
}
impl fmt::Display for InstanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignStore => f.write_str("instance belongs to another VM store"),
            Self::CapacityExceeded => f.write_str("VM instance capacity exhausted"),
            Self::Trapped(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for InstanceError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Trapped(error) => Some(error),
            _ => None,
        }
    }
}
