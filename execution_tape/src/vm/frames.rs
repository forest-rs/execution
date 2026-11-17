// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The frame stack, which counts every change to which frames it holds.

use super::*;

/// The frame stack, which counts every change to which frames it holds.
///
/// The interpreter keeps the top frame's function, unit and register base between instructions
/// and looks them up again only when [`Self::epoch`] moves. Outside this module, frames are read
/// through the slice and changed only through these methods: the ones that add, remove or move
/// frames move the epoch, and the others change only where a frame is or where it returns to,
/// never its unit, function or base. A caller cannot replace a frame in place or mutate one
/// through a reference.
#[derive(Debug, Default)]
pub(super) struct Frames {
    frames: Vec<Frame>,
    epoch: u64,
}

impl core::ops::Deref for Frames {
    type Target = [Frame];

    #[inline(always)]
    fn deref(&self) -> &[Frame] {
        &self.frames
    }
}

impl Frames {
    /// Counts the changes to which frames the stack holds.
    #[inline(always)]
    pub(super) fn epoch(&self) -> u64 {
        self.epoch
    }

    #[inline(always)]
    pub(super) fn changed(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    #[inline(always)]
    pub(super) fn push(&mut self, frame: Frame) {
        self.changed();
        self.frames.push(frame);
    }

    #[inline(always)]
    pub(super) fn pop(&mut self) -> Option<Frame> {
        self.changed();
        self.frames.pop()
    }

    #[inline]
    pub(super) fn truncate(&mut self, len: usize) {
        self.changed();
        self.frames.truncate(len);
    }

    #[inline(always)]
    pub(super) fn clear(&mut self) {
        self.changed();
        self.frames.clear();
    }

    /// Moves the frames from `depth` up onto the end of `into`, their register bases made
    /// relative to `base`, the base of the first of them.
    pub(super) fn capture(&mut self, depth: usize, base: RegBase, into: &mut Vec<Frame>) {
        self.changed();
        into.extend(self.frames.drain(depth..).map(|mut frame| {
            frame.base = frame.base.offset_by(base, false);
            frame
        }));
    }

    /// Moves `frames`, captured by [`Self::capture`], back on top, their register bases placed
    /// at `base`.
    pub(super) fn reinstate(&mut self, frames: &mut Vec<Frame>, base: RegBase) {
        self.changed();
        self.frames.extend(frames.drain(..).map(|mut frame| {
            frame.base = frame.base.offset_by(base, true);
            frame
        }));
    }

    /// Moves the frame at `index` to the instruction at `instr_ix`, whose offset is `pc`.
    #[inline(always)]
    pub(super) fn set_position(&mut self, index: usize, pc: u32, instr_ix: usize) {
        let frame = &mut self.frames[index];
        frame.pc = pc;
        frame.instr_ix = instr_ix;
    }

    /// Sets where the frame at `index` returns to.
    pub(super) fn set_return_to(&mut self, index: usize, return_to: Option<ReturnTo>) {
        self.frames[index].return_to = return_to;
    }

    /// Bytes of frame capacity held.
    pub(super) fn retained_bytes(&self) -> usize {
        capacity_bytes(&self.frames)
    }
}
