// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::trace::{TraceLocation, TraceOutcome};
use crate::value::FuncRef;
use core::num::NonZeroU32;

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
    pub(crate) func: FuncRef,
    pub(crate) caller: Option<TraceLocation>,
    pub(crate) parent: Option<FrameId>,
    pub(crate) depth: u32,
    pub(crate) entered: usize,
    pub(crate) exit: Option<ActivationExit>,
}

impl Frame {
    pub(crate) fn view(&self, ordinal: usize) -> Activation {
        Activation {
            ordinal,
            func: self.func,
            caller: self.caller,
            parent: self.parent.map(FrameId::index),
            depth: self.depth as usize,
            entered: self.entered,
            exit: self.exit,
        }
    }
}

/// One entered guest activation, in entry order.
///
/// Recordings retain every activation the invocation entered, including ones that made no host
/// call, without instruction history. Sequences use the observation order shared by
/// [`HostCall::sequence`](super::HostCall::sequence), control commands and callback returns:
/// an activation consumes no sequence number, and observations numbered below its `entered`
/// happened before it was entered, while the rest happened after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Activation {
    /// Entry order, counting the root as zero. This is the live invocation's
    /// [`ActivationId::ordinal`](crate::vm::ActivationId::ordinal) for the same activation.
    pub ordinal: usize,
    /// Entered function and the unit that ran it.
    pub func: FuncRef,
    /// The instruction that entered this activation: a call, or the host request that called
    /// back into the guest. `None` for the root.
    pub caller: Option<TraceLocation>,
    /// Ordinal of the activation running [`Self::caller`]. `None` for the root.
    pub parent: Option<usize>,
    /// Stack depth while it ran; the root activation has depth one.
    pub depth: usize,
    /// Observation sequence when it was entered.
    pub entered: usize,
    /// How it ended, or `None` when no exit was observed because capture stopped first.
    pub exit: Option<ActivationExit>,
}

/// How and where a recorded activation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActivationExit {
    /// The instruction that ended it: its own return, or, when a trap, exception or
    /// cancellation unwound it, the trapping, raising or interrupted instruction. Every
    /// activation one unwinding passes through reports that same location.
    pub location: TraceLocation,
    /// Why it ended.
    pub outcome: ActivationOutcome,
    /// Observation sequence when it exited. Observations numbered below it happened before.
    pub sequence: usize,
}

/// Why a recorded activation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationOutcome {
    /// It returned to its caller, or a callback returned to its host request.
    Returned,
    /// An exception passed through it and was caught by an activation at `depth`.
    Caught {
        /// Stack depth of the catching activation; the root activation has depth one.
        depth: usize,
    },
    /// Its continuation suspended. Resuming it records a new activation of the same frame.
    Suspended,
    /// It was unwound by a trap. The trap itself is the recording's outcome.
    Trapped,
    /// The invocation was cancelled while it was active.
    Cancelled,
}

impl ActivationOutcome {
    pub(crate) fn of(outcome: TraceOutcome<'_>) -> Self {
        match outcome {
            TraceOutcome::Ok => Self::Returned,
            TraceOutcome::Caught { depth } => Self::Caught { depth },
            TraceOutcome::Suspended => Self::Suspended,
            TraceOutcome::Trap(_) => Self::Trapped,
            TraceOutcome::Cancelled => Self::Cancelled,
        }
    }
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
