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
    /// Trace run boundaries.
    ///
    /// Enables:
    /// - [`TraceSink::run_start`]
    /// - [`TraceSink::run_end`]
    pub const RUN: Self = Self(1 << 0);
    /// Trace each executed instruction.
    ///
    /// Enables:
    /// - [`TraceSink::instr`]
    pub const INSTR: Self = Self(1 << 1);
    /// Trace call frames.
    ///
    /// Enables (for [`ScopeKind::CallFrame`]):
    /// - [`TraceSink::scope_enter`]
    /// - [`TraceSink::scope_exit`]
    pub const CALL: Self = Self(1 << 2);
    /// Trace host calls.
    ///
    /// Enables (for [`ScopeKind::HostCall`]):
    /// - [`TraceSink::scope_enter`]
    /// - [`TraceSink::scope_exit`]
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

    /// Called at the start of a VM run.
    ///
    /// Called only if `mask()` includes [`TraceMask::RUN`].
    ///
    /// - `program`: program being executed (unverified container data)
    /// - `entry`: entry function id
    /// - `arg_count`: number of value arguments passed (not including the effect token)
    fn run_start(&mut self, _program: &Program, _entry: FuncId, _arg_count: usize) {}

    /// Called for each executed instruction.
    ///
    /// Called only if `mask()` includes [`TraceMask::INSTR`].
    ///
    /// - `program`: program being executed (unverified container data)
    /// - `func`: current function id
    /// - `pc`: current instruction pc (byte offset)
    /// - `next_pc`: next instruction pc (byte offset)
    /// - `span_id`: best-effort span id for tracing/source mapping
    /// - `opcode`: opcode byte
    fn instr(
        &mut self,
        _program: &Program,
        _func: FuncId,
        _pc: u32,
        _next_pc: u32,
        _span_id: Option<u64>,
        _opcode: u8,
    ) {
    }

    /// Called when entering a profiling scope.
    ///
    /// Called only if `mask()` includes:
    /// - [`TraceMask::CALL`] (for [`ScopeKind::CallFrame`])
    /// - [`TraceMask::HOST`] (for [`ScopeKind::HostCall`])
    ///
    /// - `program`: program being executed (unverified container data)
    /// - `kind`: the kind of scope being entered
    /// - `depth`: stack depth (frames) after entering the scope
    /// - `func`: function id at the scope boundary
    /// - `pc`: program counter at the scope boundary
    /// - `span_id`: best-effort span id at the scope boundary
    fn scope_enter(
        &mut self,
        _program: &Program,
        _kind: ScopeKind,
        _depth: usize,
        _func: FuncId,
        _pc: u32,
        _span_id: Option<u64>,
    ) {
    }

    /// Called when exiting a profiling scope.
    ///
    /// Called only if `mask()` includes:
    /// - [`TraceMask::CALL`] (for [`ScopeKind::CallFrame`])
    /// - [`TraceMask::HOST`] (for [`ScopeKind::HostCall`])
    ///
    /// - `program`: program being executed (unverified container data)
    /// - `kind`: the kind of scope being exited
    /// - `depth`: stack depth (frames) before exiting the scope
    /// - `func`: function id at the scope boundary
    /// - `pc`: program counter at the scope boundary
    /// - `span_id`: best-effort span id at the scope boundary
    fn scope_exit(
        &mut self,
        _program: &Program,
        _kind: ScopeKind,
        _depth: usize,
        _func: FuncId,
        _pc: u32,
        _span_id: Option<u64>,
    ) {
    }

    /// Called at the end of a VM run.
    ///
    /// Called only if `mask()` includes [`TraceMask::RUN`].
    ///
    /// - `program`: program being executed (unverified container data)
    /// - `outcome`: whether the run ended successfully or trapped
    fn run_end(&mut self, _program: &Program, _outcome: TraceOutcome<'_>) {}
}
