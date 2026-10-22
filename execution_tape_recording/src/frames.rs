// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use core::num::NonZeroU32;
use execution_tape::trace::TraceLocation;
use execution_tape::value::FuncId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameId(NonZeroU32);

impl FrameId {
    pub(crate) fn new(index: usize) -> Option<Self> {
        u32::try_from(index)
            .ok()?
            .checked_add(1)
            .and_then(NonZeroU32::new)
            .map(Self)
    }
    pub(crate) fn index(self) -> usize {
        (self.0.get() - 1) as usize
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Frame {
    pub(crate) func: FuncId,
    pub(crate) caller: Option<TraceLocation>,
    pub(crate) parent: Option<FrameId>,
    pub(crate) depth: u32,
}

/// Borrowed guest instruction locations from the innermost activation to the root.
///
/// The first location is the selected host call, root return or trap. Each subsequent
/// location is the actual caller instruction suspended while its child executes. Recursion
/// keeps distinct activations, even when function IDs and call sites repeat. Iteration does
/// not allocate, expand values or reconstruct host/native frames.
#[derive(Clone)]
pub struct CallStack<'a> {
    frames: &'a [Frame],
    next: Option<(FrameId, TraceLocation)>,
    remaining: usize,
}

impl<'a> CallStack<'a> {
    pub(crate) fn new(frames: &'a [Frame], top: FrameId, location: TraceLocation) -> Self {
        Self {
            frames,
            next: Some((top, location)),
            remaining: frames[top.index()].depth as usize,
        }
    }
}

impl Iterator for CallStack<'_> {
    type Item = TraceLocation;

    fn next(&mut self) -> Option<Self::Item> {
        let (index, location) = self.next.take()?;
        let frame = &self.frames[index.index()];
        self.next = frame.parent.zip(frame.caller);
        self.remaining -= 1;
        Some(location)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}
impl ExactSizeIterator for CallStack<'_> {}
impl core::iter::FusedIterator for CallStack<'_> {}
impl core::fmt::Debug for CallStack<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}
