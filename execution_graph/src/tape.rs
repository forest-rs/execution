// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! [`Executor`] over verified `execution_tape` programs.
//!
//! A [`TapeNode`] is a `(VerifiedProgram, entry FuncId)` pair. The [`TapeExecutor`] owns one VM
//! and one reusable execution context, runs each node's entry function with the bound argument
//! values, and translates the host's [`AccessSink`] events into graph dependency keys.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::Cell;
use core::fmt;

use execution_tape::host::{AccessSink, Host, ResourceKeyRef, SigHash};
use execution_tape::trace::{ScopeKind, TraceMask, TraceSink};
use execution_tape::value::{FuncId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{ExecutionContext, Limits, TrapInfo, Vm};

use crate::access::{HostOpId, NodeId, ResourceKey};
use crate::executor::Executor;
use crate::graph::{ExecutionGraph, GraphError};
use crate::node_access::NodeAccess;

/// Failures specific to tape nodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TapeError {
    /// A function id was not present in the verified program supplied for a node.
    BadEntryFunc {
        /// Invalid entry function id.
        func: FuncId,
    },
    /// A node's declared graph inputs did not match its tape function arity.
    BadInputArity {
        /// Entry function id for the node being added.
        func: FuncId,
        /// Expected graph input count from the tape function signature.
        expected: usize,
        /// Actual input count supplied by the caller.
        actual: usize,
    },
    /// Strict deps mode error: a host op recorded no access keys.
    StrictDepsViolation {
        /// Host call symbol.
        symbol: Box<str>,
        /// Signature hash carried in bytecode/program.
        sig_hash: SigHash,
    },
    /// VM execution trapped.
    Trap(TrapInfo),
}

impl fmt::Display for TapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadEntryFunc { func } => {
                write!(
                    f,
                    "bad entry function: f{} is not in the node program",
                    func.0
                )
            }
            Self::BadInputArity {
                func,
                expected,
                actual,
            } => write!(
                f,
                "bad node input arity: entry=f{} expected {expected} inputs, got {actual}",
                func.0
            ),
            Self::StrictDepsViolation { symbol, sig_hash } => write!(
                f,
                "strict deps violation: host_call={symbol} sig_hash={}; host call recorded no access keys, so strict dependency tracking cannot know what invalidates it",
                sig_hash.0
            ),
            Self::Trap(trap) => write!(f, "vm trapped: {trap}"),
        }
    }
}

impl core::error::Error for TapeError {}

/// A graph node that runs one entry function of a verified tape program.
#[derive(Clone, Debug)]
pub struct TapeNode {
    program: Arc<VerifiedProgram>,
    entry: FuncId,
}

impl TapeNode {
    /// Wraps `entry` of `program` as a node body.
    ///
    /// Returns [`TapeError::BadEntryFunc`] if `entry` is not present in `program`.
    pub fn new(program: Arc<VerifiedProgram>, entry: FuncId) -> Result<Self, TapeError> {
        if program.program().functions.get(entry.0 as usize).is_none() {
            return Err(TapeError::BadEntryFunc { func: entry });
        }
        Ok(Self { program, entry })
    }

    /// Returns the program.
    #[must_use]
    #[inline]
    pub fn program(&self) -> &Arc<VerifiedProgram> {
        &self.program
    }

    /// Returns the entry function.
    #[must_use]
    #[inline]
    pub const fn entry(&self) -> FuncId {
        self.entry
    }

    /// Returns the number of graph inputs the entry function expects.
    #[must_use]
    pub fn input_count(&self) -> usize {
        self.program.program().functions[self.entry.0 as usize].arg_count as usize
    }

    /// Returns the output names declared by the entry function.
    ///
    /// Output names are optional in the tape format. Unnamed returns get the predictable
    /// fallback `ret{index}` so tooling can still wire them; callers that need stable wiring
    /// should name outputs explicitly with `ProgramBuilder::set_function_output_name`.
    #[must_use]
    pub fn output_names(&self) -> Vec<Box<str>> {
        let program = self.program.program();
        let ret_count = program.functions[self.entry.0 as usize].ret_count as usize;
        let mut names: Vec<Box<str>> = Vec::with_capacity(ret_count);
        for i in 0..ret_count {
            let ret = u32::try_from(i).unwrap_or(u32::MAX);
            match program.function_output_name(self.entry.0, ret) {
                Some(name) if name != "ret" => names.push(name.into()),
                _ => names.push(format!("ret{i}").into_boxed_str()),
            }
        }
        names
    }
}

/// Compares values whose equality means readers see the same thing; handles never compare.
fn plain_values_equal(previous: &Value, next: &Value) -> bool {
    match (previous, next) {
        (Value::Unit, Value::Unit) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::I64(a), Value::I64(b)) => a == b,
        (Value::U64(a), Value::U64(b)) => a == b,
        (Value::F64(a), Value::F64(b)) => a.to_bits() == b.to_bits(),
        (Value::Decimal(a), Value::Decimal(b)) => a == b,
        (Value::Bytes(a), Value::Bytes(b)) => a == b,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Func(a), Value::Func(b)) => a == b,
        _ => false,
    }
}

/// Executor that runs [`TapeNode`]s on one `execution_tape` VM.
pub struct TapeExecutor<H: Host> {
    vm: Vm<H>,
    ctx: ExecutionContext,
    strict_deps: bool,
    early_cutoff: bool,
}

impl<H: Host> fmt::Debug for TapeExecutor<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TapeExecutor")
            .field("vm", &self.vm)
            .field("strict_deps", &self.strict_deps)
            .field("early_cutoff", &self.early_cutoff)
            .finish_non_exhaustive()
    }
}

impl<H: Host> TapeExecutor<H> {
    /// Creates an executor whose VM calls into `host` under `limits`.
    #[must_use]
    pub fn new(host: H, limits: Limits) -> Self {
        Self {
            vm: Vm::new(host, limits),
            ctx: ExecutionContext::new(),
            strict_deps: false,
            early_cutoff: false,
        }
    }

    /// Enables or disables early cutoff (off by default).
    ///
    /// When enabled, a re-run output equal to its previous value counts as unchanged, so
    /// dependents scheduled only because of it are skipped. Only plain values compare: unit,
    /// booleans, integers, decimals, byte strings, strings, and function references by value,
    /// and floats by bit pattern (so `-0.0` differs from `0.0` and a NaN equals only itself).
    /// Host objects, aggregates, and closures are handles whose contents can change behind an
    /// equal handle, so they always count as changed.
    pub fn set_early_cutoff(&mut self, enabled: bool) {
        self.early_cutoff = enabled;
    }

    /// Returns whether early cutoff is enabled.
    #[must_use]
    #[inline]
    pub const fn early_cutoff(&self) -> bool {
        self.early_cutoff
    }

    /// Enables or disables strict dependency tracking for host calls.
    ///
    /// When enabled, each host call is required to record at least one access key via the access
    /// sink. This is a debugging mode intended to prevent silently unsound incremental execution
    /// caused by missing access reporting.
    pub fn set_strict_deps(&mut self, strict: bool) {
        self.strict_deps = strict;
    }

    /// Returns whether strict dependency tracking is enabled.
    #[must_use]
    #[inline]
    pub const fn strict_deps(&self) -> bool {
        self.strict_deps
    }
}

impl<H: Host> Executor for TapeExecutor<H> {
    type Value = Value;
    type Node = TapeNode;
    type Error = TapeError;

    fn execute(
        &mut self,
        node: &mut Self::Node,
        inputs: &[Self::Value],
        outputs: &mut Vec<Self::Value>,
        access: &mut NodeAccess<'_>,
    ) -> Result<(), Self::Error> {
        let access_count: Cell<usize> = Cell::new(0);
        let mut strict = StrictDepsTrace::new(&access_count);
        let (trace_mask, trace): (TraceMask, Option<&mut dyn TraceSink>) = if self.strict_deps {
            (TraceMask::HOST, Some(&mut strict as &mut dyn TraceSink))
        } else {
            (TraceMask::NONE, None)
        };
        let mut sink = TapeAccessSink {
            access,
            counter: &access_count,
        };
        let out = self
            .vm
            .run_with_ctx(
                &mut self.ctx,
                &node.program,
                node.entry,
                inputs,
                trace_mask,
                trace,
                Some(&mut sink),
            )
            .map_err(TapeError::Trap)?;

        if self.strict_deps
            && let Some(v) = strict.violation()
        {
            return Err(TapeError::StrictDepsViolation {
                symbol: v.symbol.clone(),
                sig_hash: v.sig_hash,
            });
        }

        outputs.extend(out);
        Ok(())
    }

    fn values_equal(&self, previous: &Self::Value, next: &Self::Value) -> bool {
        self.early_cutoff && plain_values_equal(previous, next)
    }

    fn describe(&self, node: &Self::Node) -> Option<String> {
        let program = node.program.program();
        let entry = match program.function_name(node.entry.0) {
            Some(name) => format!("entry=f{} ({name})", node.entry.0),
            None => format!("entry=f{}", node.entry.0),
        };
        Some(match program.name() {
            Some(name) => format!("program={name}\n{entry}"),
            None => entry,
        })
    }
}

impl<H: Host> ExecutionGraph<TapeExecutor<H>> {
    /// Adds a tape node and returns its [`NodeId`].
    ///
    /// `input_names` defines the mapping from per-node binding names to positional function args.
    /// Output names come from the program (see [`TapeNode::output_names`]).
    ///
    /// Returns [`TapeError::BadEntryFunc`] if `entry` is not present in `program`, or
    /// [`TapeError::BadInputArity`] if `input_names` does not match the entry function's
    /// argument count, both wrapped in [`GraphError::InvalidNode`].
    pub fn add_tape_node(
        &mut self,
        program: Arc<VerifiedProgram>,
        entry: FuncId,
        input_names: Vec<Box<str>>,
    ) -> Result<NodeId, GraphError<TapeError>> {
        let node = TapeNode::new(program, entry).map_err(GraphError::InvalidNode)?;
        let expected = node.input_count();
        if input_names.len() != expected {
            return Err(GraphError::InvalidNode(TapeError::BadInputArity {
                func: entry,
                expected,
                actual: input_names.len(),
            }));
        }
        let output_names = node.output_names();
        self.add_node(node, input_names, output_names)
    }

    /// Marks a tape host key dirty.
    ///
    /// This accepts the borrowed key type used by `execution_tape` host access reporting and is
    /// equivalent to [`invalidate`](ExecutionGraph::invalidate) with the converted
    /// [`ResourceKey`].
    #[inline]
    pub fn invalidate_tape_key(&mut self, key: ResourceKeyRef<'_>) {
        self.invalidate(ResourceKey::from(key));
    }
}

impl From<ResourceKeyRef<'_>> for ResourceKey {
    fn from(key: ResourceKeyRef<'_>) -> Self {
        match key {
            ResourceKeyRef::Input(name) => Self::input(name),
            ResourceKeyRef::HostState { op, key } => Self::host_state(HostOpId::new(op.0), key),
            ResourceKeyRef::OpaqueHost { op } => Self::opaque_host(HostOpId::new(op.0)),
        }
    }
}

/// Translates tape host access events into graph dependency recording.
struct TapeAccessSink<'a, 'b> {
    access: &'a mut NodeAccess<'b>,
    /// Shared with [`StrictDepsTrace`] so strict-deps validation can verify that each host call
    /// reported at least one usable key.
    counter: &'a Cell<usize>,
}

impl AccessSink for TapeAccessSink<'_, '_> {
    fn read(&mut self, key: ResourceKeyRef<'_>) {
        self.counter.set(self.counter.get().saturating_add(1));
        match key {
            ResourceKeyRef::Input(name) => self.access.read_input(name),
            ResourceKeyRef::HostState { op, key } => {
                self.access.read_host_state(HostOpId::new(op.0), key);
            }
            ResourceKeyRef::OpaqueHost { op } => self.access.read_opaque_host(HostOpId::new(op.0)),
        }
    }

    fn write(&mut self, key: ResourceKeyRef<'_>) {
        match key {
            // Graph inputs are graph-owned: a host write to one is ignored rather than allowed to
            // alias a node's input-binding dependency (which the self-write filter would then
            // strip). It is not counted as a strict-deps access event either, because it reports
            // nothing usable.
            ResourceKeyRef::Input(_) => {}
            ResourceKeyRef::HostState { op, key } => {
                self.counter.set(self.counter.get().saturating_add(1));
                self.access.write_host_state(HostOpId::new(op.0), key);
            }
            ResourceKeyRef::OpaqueHost { op } => {
                self.counter.set(self.counter.get().saturating_add(1));
                self.access.write_opaque_host(HostOpId::new(op.0));
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StrictDepsViolation {
    symbol: Box<str>,
    sig_hash: SigHash,
}

/// Trace sink for strict dependency tracking: requires each host call to record at least one
/// access key.
#[derive(Debug)]
struct StrictDepsTrace<'a> {
    counter: &'a Cell<usize>,
    stack: Vec<(usize, execution_tape::program::SymbolId, SigHash)>,
    violation: Option<StrictDepsViolation>,
}

impl<'a> StrictDepsTrace<'a> {
    #[must_use]
    #[inline]
    const fn new(counter: &'a Cell<usize>) -> Self {
        Self {
            counter,
            stack: Vec::new(),
            violation: None,
        }
    }

    #[must_use]
    #[inline]
    fn violation(&self) -> Option<&StrictDepsViolation> {
        self.violation.as_ref()
    }
}

impl TraceSink for StrictDepsTrace<'_> {
    fn mask(&self) -> TraceMask {
        TraceMask::HOST
    }

    fn scope_enter(
        &mut self,
        _program: &execution_tape::program::Program,
        kind: ScopeKind,
        _depth: usize,
        _func: FuncId,
        _pc: u32,
        _span_id: Option<u64>,
    ) {
        let ScopeKind::HostCall {
            symbol, sig_hash, ..
        } = kind
        else {
            return;
        };
        self.stack.push((self.counter.get(), symbol, sig_hash));
    }

    fn scope_exit(
        &mut self,
        program: &execution_tape::program::Program,
        kind: ScopeKind,
        _depth: usize,
        _func: FuncId,
        _pc: u32,
        _span_id: Option<u64>,
    ) {
        let ScopeKind::HostCall { .. } = kind else {
            return;
        };

        let Some((start, symbol, sig_hash)) = self.stack.pop() else {
            return;
        };

        if self.violation.is_some() {
            return;
        }
        if self.counter.get() != start {
            return;
        }

        let sym = program
            .symbol_str(symbol)
            .unwrap_or("<invalid symbol>")
            .to_string()
            .into_boxed_str();

        self.violation = Some(StrictDepsViolation {
            symbol: sym,
            sig_hash,
        });
    }
}
