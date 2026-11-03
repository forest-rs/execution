// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

/// A dynamically installed control clause. Clause bodies are ordinary qualified callables.
///
/// Suspension clauses receive the operation's request values followed by a one-use
/// continuation. Their results are the results of the enclosing `cont.resume`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandlerArm {
    /// Captures the computation inside the nearest matching resume delimiter.
    Suspend {
        /// Register containing the exact control operation identity.
        tag: u32,
        /// Function or closure register containing the handler body.
        handler: u32,
    },
}
impl HandlerArm {
    pub(crate) fn register(self, index: usize) -> Option<u32> {
        match (self, index) {
            (Self::Suspend { tag, .. }, 0) => Some(tag),
            (Self::Suspend { handler, .. }, 1) => Some(handler),
            _ => None,
        }
    }
}
