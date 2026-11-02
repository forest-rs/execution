// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Executable value identity is independent of the program currently interpreting it.

use super::FuncId;
use crate::instance::Instance;
use core::fmt;

/// A function in one exact code instance.
///
/// `FuncId` remains a program-local instruction operand. A runtime function value also retains
/// its instance, so moving the value cannot reinterpret that operand in another definition.
/// Cloning shares code and instance identity without allocating a new function object. The
/// originating store must still own any host state used when invoking the function.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct FunctionRef {
    instance: Instance,
    function: FuncId,
}

impl FunctionRef {
    pub(crate) fn from_verified(instance: Instance, function: FuncId) -> Self {
        Self { instance, function }
    }

    /// Exact code and store namespace interpreting the target.
    #[must_use]
    pub fn instance(&self) -> &Instance {
        &self.instance
    }

    /// Local target in [`Self::instance`]'s definition.
    #[must_use]
    pub fn id(&self) -> FuncId {
        self.function
    }
}

impl fmt::Debug for FunctionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FunctionRef")
            .field("instance", &self.instance)
            .field("function", &self.function)
            .finish()
    }
}
