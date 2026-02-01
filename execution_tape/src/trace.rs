// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tracing hooks for `execution_tape`.
//!
//! Tracing is optional and is designed to be `no_std` friendly.
//! The VM only emits events requested by a [`TraceMask`].
//!
//! To enable tracing, pass a [`TraceMask`] and [`TraceSink`] to [`Vm::run`].

#[cfg(doc)]
use crate::vm::Vm;

use crate::host::SigHash;
use crate::program::Program;
use crate::program::{HostSigId, SymbolId};
use crate::value::FuncId;
use crate::vm::TrapInfo;

/// A set of trace events requested by a [`TraceSink`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TraceMask(u32);

impl core::ops::BitOr for TraceMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for TraceMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl TraceMask {
    /// No tracing.
    pub const NONE: Self = Self(0);
    /// Emit [`TraceEvent::RunStart`] and [`TraceEvent::RunEnd`].
    pub const RUN: Self = Self(1 << 0);
    /// Emit [`TraceEvent::Instr`] for each executed instruction.
    pub const INSTR: Self = Self(1 << 1);
    /// Emit [`TraceEvent::ScopeEnter`] and [`TraceEvent::ScopeExit`] for call frames.
    pub const CALL: Self = Self(1 << 2);
    /// Emit [`TraceEvent::ScopeEnter`] and [`TraceEvent::ScopeExit`] for host calls.
    pub const HOST: Self = Self(1 << 3);

    /// Returns `true` if this mask includes all bits in `other`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

/// The kind of scope being entered/exited.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    /// A VM call frame (function activation).
    CallFrame {
        /// Function id for the frame being entered/exited.
        func: FuncId,
    },
    /// A VM host call.
    HostCall {
        /// Host signature identifier.
        host_sig: HostSigId,
        /// Host symbol identifier.
        symbol: SymbolId,
        /// Host signature hash carried in bytecode/program.
        sig_hash: SigHash,
    },
}

/// A trace event emitted by the VM.
#[derive(Clone, Debug)]
pub enum TraceEvent<'a> {
    /// Start of a VM run.
    RunStart {
        /// Entry function id.
        entry: FuncId,
        /// Number of value arguments passed (not including the effect token).
        arg_count: usize,
    },
    /// A single instruction step.
    Instr {
        /// Current function id.
        func: FuncId,
        /// Current instruction pc (byte offset).
        pc: u32,
        /// Next instruction pc (byte offset).
        next_pc: u32,
        /// Best-effort span id for tracing/source mapping.
        span_id: Option<u64>,
        /// Opcode byte.
        opcode: u8,
    },
    /// Enter a profiling scope.
    ///
    /// Scope events are emitted for:
    /// - call frames (when [`TraceMask::CALL`] is requested)
    /// - host calls (when [`TraceMask::HOST`] is requested)
    ScopeEnter {
        /// Scope kind.
        kind: ScopeKind,
        /// Current stack depth (frames) after entering the scope.
        depth: usize,
        /// Function id at the scope boundary.
        func: FuncId,
        /// Program counter at the scope boundary.
        pc: u32,
        /// Best-effort span id at the scope boundary.
        span_id: Option<u64>,
    },
    /// Exit a profiling scope.
    ScopeExit {
        /// Scope kind.
        kind: ScopeKind,
        /// Current stack depth (frames) before exiting the scope.
        depth: usize,
        /// Function id at the scope boundary.
        func: FuncId,
        /// Program counter at the scope boundary.
        pc: u32,
        /// Best-effort span id at the scope boundary.
        span_id: Option<u64>,
    },
    /// End of a VM run.
    RunEnd {
        /// Run outcome.
        outcome: TraceOutcome<'a>,
    },
}

/// Run outcome for tracing.
#[derive(Clone, Debug)]
pub enum TraceOutcome<'a> {
    /// Successful run.
    Ok,
    /// Trapped.
    Trap(&'a TrapInfo),
}

/// A trace sink that can receive VM events.
pub trait TraceSink {
    /// Returns the set of events the sink wants.
    fn mask(&self) -> TraceMask {
        TraceMask::NONE
    }

    /// Receives a trace event.
    fn event(&mut self, program: &Program, event: TraceEvent<'_>);
}
