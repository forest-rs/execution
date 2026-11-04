// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! One exclusive observer lease belongs to the drive, never to a saved guest stack.

use super::{BorrowedOrOwned, InvocationOptions};
use crate::{host::AccessSink, trace::TraceSink};

#[derive(Default)]
pub(super) struct Observers<'a> {
    pub(super) trace: Option<BorrowedOrOwned<'a, dyn TraceSink + 'a>>,
    pub(super) access: Option<BorrowedOrOwned<'a, dyn AccessSink + 'a>>,
}
impl<'a> Observers<'a> {
    pub(super) fn take(options: &mut InvocationOptions<'a>) -> Self {
        Self {
            trace: options.trace.take(),
            access: options.access.take(),
        }
    }
}
