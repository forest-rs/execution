// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::program::CallSigId;

/// An encoded local control clause in one function.
///
/// The target is a typed block whose parameters receive request values followed by a one-use
/// continuation. Symbolic authoring uses [`crate::asm::HandlerArm`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandlerClause {
    /// Captures the computation inside the nearest matching resume delimiter.
    Suspend {
        /// Register containing the exact control operation identity.
        tag: u32,
        /// Request and response protocol interpreted by the same program.
        signature: CallSigId,
        /// Typed block header byte offset in the resumer's function.
        target: u32,
    },
}
impl HandlerClause {
    pub(crate) fn register(self, index: usize) -> Option<u32> {
        match (self, index) {
            (Self::Suspend { tag, .. }, 0) => Some(tag),
            _ => None,
        }
    }
    pub(crate) fn target(self) -> u32 {
        match self {
            Self::Suspend { target, .. } => target,
        }
    }
    pub(crate) fn signature(self) -> CallSigId {
        match self {
            Self::Suspend { signature, .. } => signature,
        }
    }
}
