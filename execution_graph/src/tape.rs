// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! [`Executor`] over verified `execution_tape` programs.
//!
//! A [`TapeNode`] is a `(VerifiedProgram, entry FuncId)` pair. The [`TapeExecutor`] owns one VM
//! and one reusable execution context, runs each node's entry function with the bound argument
//! values, and translates the host's [`AccessSink`] events into graph dependency keys. Edges
//! carry owned [`TapeValue`]s; the VM heap is invocation-local rather than a lifetime-long cache.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::Cell;
use core::fmt;

use execution_tape::aggregates::AggError;
use execution_tape::host::{AccessSink, Host, ResourceKeyRef, SigHash};
use execution_tape::trace::{ScopeKind, TraceMask, TraceSink};
use execution_tape::value::{FuncId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{ExecutionContext, Limits, TrapInfo, Vm};

use crate::access::{HostOpId, NodeId, ResourceKey};
use crate::executor::Executor;
use crate::graph::{ExecutionGraph, GraphError};
use crate::node_access::NodeAccess;
use crate::tape_value::{Importer, TapeValue};

/// Failures specific to tape nodes.
#[derive(Clone, Debug)]
pub enum TapeError {
    /// A retained root contains program-local IDs without their owning program.
    MissingProgram {
        /// Root index in the retained snapshot.
        root: usize,
    },
    /// A raw input handle has no ownership or program domain.
    UnownedInput {
        /// Positional node input.
        input: usize,
    },
    /// An input's scoped IDs belong to a different prepared program.
    ProgramMismatch {
        /// Positional node input, including scoped IDs nested inside aggregates.
        input: usize,
    },
    /// Retaining or importing an aggregate graph failed.
    Aggregate(AggError),
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
            Self::MissingProgram { root } => write!(
                f,
                "snapshot root {root} contains program-local IDs; supply its original prepared program"
            ),
            Self::UnownedInput { input } => write!(
                f,
                "input {input} contains an unowned aggregate or callable; adopt a retained snapshot with TapeValue::from_snapshot"
            ),
            Self::ProgramMismatch { input } => write!(
                f,
                "input {input} contains IDs belonging to a different prepared program; use its original program or explicitly translate the value"
            ),
            Self::Aggregate(error) => write!(f, "aggregate transfer failed: {error}"),
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

impl core::error::Error for TapeError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Trap(trap) => Some(trap),
            Self::Aggregate(error) => Some(error),
            _ => None,
        }
    }
}

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

/// Executor that runs [`TapeNode`]s on one `execution_tape` VM with owned [`TapeValue`] edges.
///
/// Result owners are independent of this executor. Inputs are admitted before execution,
/// imported with sharing preserved, then discarded with transient context data after each
/// attempt. Cached and externally cloned results keep only their immutable publication storage.
pub struct TapeExecutor<H: Host> {
    vm: Vm<H>,
    ctx: ExecutionContext,
    strict_deps: bool,
    early_cutoff: bool,
    args: Vec<Value>,
    results: Vec<Value>,
    importer: Importer,
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
            args: Vec::new(),
            results: Vec::new(),
            importer: Importer::default(),
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

    /// Borrows host state between runs.
    pub fn host(&self) -> &H {
        self.vm.host()
    }

    /// Updates host state between runs. Report affected keys through the graph's
    /// `invalidate`/`invalidate_many` methods before querying dependent outputs.
    pub fn host_mut(&mut self) -> &mut H {
        self.vm.host_mut()
    }

    fn clear_workspace(&mut self) {
        self.ctx.clear();
        self.args.clear();
        self.results.clear();
        self.importer.clear();
        self.vm.aggregates_mut().clear();
    }
}

impl<H: Host> Executor for TapeExecutor<H> {
    type Value = TapeValue;
    type Node = TapeNode;
    type Error = TapeError;

    fn execute(
        &mut self,
        node: &mut Self::Node,
        inputs: &[Self::Value],
        outputs: &mut Vec<Self::Value>,
        access: &mut NodeAccess<'_, Self::Value>,
    ) -> Result<crate::NodeOutcome, Self::Error> {
        self.execute_node(node, inputs, outputs, access)
            .map(|()| crate::NodeOutcome::Complete)
    }

    fn values_equal(&self, previous: &Self::Value, next: &Self::Value) -> bool {
        self.early_cutoff
            && plain_values_equal(previous.value(), next.value())
            && match (previous.value(), next.value()) {
                (Value::Func(_), Value::Func(_)) => previous == next,
                _ => true,
            }
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

impl<H: Host> TapeExecutor<H> {
    /// Executes one tape body as part of a custom graph executor with edge type `V`.
    ///
    /// Convert selected inputs to `TapeValue`, then wrap appended outputs in the custom edge
    /// representation. `access` forwards host resource keys regardless of that representation.
    /// This does not schedule graph work or publish outputs; the graph validates the custom
    /// executor's attempt afterward. The built-in [`Executor`] implementation uses this same
    /// ownership and cleanup path. Tape calls complete or fail; they do not suspend or read
    /// graph outputs dynamically through host calls.
    pub fn execute_node<V>(
        &mut self,
        node: &TapeNode,
        inputs: &[TapeValue],
        outputs: &mut Vec<TapeValue>,
        access: &mut NodeAccess<'_, V>,
    ) -> Result<(), TapeError> {
        // Admission is complete before copying any input or allowing host effects.
        for (index, input) in inputs.iter().enumerate() {
            input.validate_input(index, &node.program)?;
        }
        let result = self.execute_owned(node, inputs, outputs, access);
        self.clear_workspace();
        result
    }

    fn execute_owned<V>(
        &mut self,
        node: &TapeNode,
        inputs: &[TapeValue],
        outputs: &mut Vec<TapeValue>,
        access: &mut NodeAccess<'_, V>,
    ) -> Result<(), TapeError> {
        for input in inputs {
            self.args.push(
                self.importer
                    .import(input, self.vm.aggregates_mut())
                    .map_err(TapeError::Aggregate)?,
            );
        }
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
        self.vm
            .run_with_ctx(
                &mut self.ctx,
                &node.program,
                node.entry,
                &self.args,
                &mut self.results,
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

        if !self
            .results
            .iter()
            .any(|value| matches!(value, Value::Agg(_) | Value::Closure(_)))
        {
            outputs.extend(
                self.results
                    .drain(..)
                    .map(|value| TapeValue::plain_result(value, &node.program)),
            );
            return Ok(());
        }
        let values = core::mem::take(&mut self.results);
        let heap = core::mem::take(self.vm.aggregates_mut());
        let origins = core::mem::take(&mut self.importer.origins);
        TapeValue::publish(heap, values, Arc::clone(&node.program), origins, outputs)?;
        Ok(())
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
    /// argument count, both wrapped in [`GraphError::InvalidNode`]. An entry without returns
    /// produces [`GraphError::NoOutputs`]; return unit to represent completion.
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

#[cfg(test)]
mod memory_tests {
    use super::*;
    use alloc::vec;
    use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
    use execution_tape::program::ValueType;

    struct NoHost;
    impl Host for NoHost {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[execution_tape::host::ValueRef<'_>],
            _: &mut [Value],
            _: execution_tape::host::HostContext<'_, '_>,
        ) -> Result<u64, execution_tape::host::HostError> {
            unreachable!("the test program has no host calls")
        }
    }

    #[test]
    fn tape_nodes_require_a_result_or_explicit_unit_completion() {
        let mut builder = ProgramBuilder::new();
        let mut body = Asm::new();
        body.set_register_types(vec![ValueType::Unit]);
        body.ret(0, &[]);
        let empty = builder
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![],
                },
            )
            .unwrap();
        let mut body = Asm::new();
        body.set_register_types(vec![ValueType::Unit]);
        body.ret(0, &[0]);
        let unit = builder
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::Unit],
                },
            )
            .unwrap();
        let program = Arc::new(builder.build_verified().unwrap());
        let mut graph = ExecutionGraph::new(TapeExecutor::new(NoHost, Limits::default()));
        assert!(matches!(
            graph.add_tape_node(Arc::clone(&program), empty, vec![]),
            Err(GraphError::NoOutputs)
        ));
        let node = graph.add_tape_node(program, unit, vec![]).unwrap();
        assert_eq!(graph.run_node(node).unwrap().executed_nodes, 1);
        assert_eq!(
            graph.node_outputs(node).unwrap().get("ret0"),
            Some(&Value::Unit.into())
        );
    }

    #[test]
    fn failed_attempts_clear_imports_and_preserve_previous_owned_outputs() {
        use execution_tape::aggregates::AggHeap;
        use execution_tape::host::{HostContext, HostError, HostSig, ValueRef};

        #[derive(Default)]
        struct FallibleHost {
            fail: bool,
        }
        impl Host for FallibleHost {
            fn call(
                &mut self,
                _: &str,
                _: SigHash,
                _: &[ValueRef<'_>],
                _: &mut [Value],
                _: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                if self.fail {
                    Err(HostError::UnknownSymbol)
                } else {
                    Ok(0)
                }
            }
        }
        let mut builder = ProgramBuilder::new();
        let host = builder.host_sig_for(
            "check",
            HostSig {
                args: vec![],
                rets: vec![],
            },
        );
        let mut body = Asm::new();
        body.set_register_types(vec![ValueType::Unit, ValueType::Agg]);
        body.host_call_sig_id(0, host, 0, &[], &[]).ret(0, &[1]);
        let entry = builder
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![ValueType::Agg],
                    ret_types: vec![ValueType::Agg],
                },
            )
            .unwrap();
        let program = Arc::new(builder.build_verified().unwrap());
        let mut heap = AggHeap::new();
        let root = heap.tuple_new(vec![Value::Bytes(vec![7; 65536])]).unwrap();
        let (snapshot, _) = heap.into_snapshot(vec![Value::Agg(root)]).unwrap();
        let input = TapeValue::from_snapshot(snapshot, None).unwrap().remove(0);
        let mut graph = ExecutionGraph::new(TapeExecutor::new(
            FallibleHost::default(),
            Limits::default(),
        ));
        let node = graph
            .add_tape_node(program, entry, vec!["input".into()])
            .unwrap();
        graph.set_input_value(node, "input", input).unwrap();
        graph.run_all().unwrap();
        let previous = graph
            .node_outputs(node)
            .unwrap()
            .get("ret0")
            .unwrap()
            .clone();
        for strict in [false, true] {
            graph.executor_mut().host_mut().fail = !strict;
            graph.executor_mut().set_strict_deps(strict);
            graph.invalidate_input("input");
            let error = graph.run_all().unwrap_err();
            assert!(matches!(
                (strict, error),
                (
                    false,
                    GraphError::Node {
                        source: TapeError::Trap(_),
                        ..
                    }
                ) | (
                    true,
                    GraphError::Node {
                        source: TapeError::StrictDepsViolation { .. },
                        ..
                    }
                )
            ));
            assert_eq!(
                graph.node_outputs(node).unwrap().get("ret0"),
                Some(&previous)
            );
            let executor = graph.executor();
            assert_eq!(executor.vm.aggregates().len_u32(), 0);
            assert!(executor.args.is_empty());
            assert!(executor.results.is_empty());
        }
    }

    #[test]
    fn graph_errors_preserve_typed_host_failure_sources() {
        use core::error::Error;
        use execution_tape::host::{HostContext, HostError, HostSig, ValueRef};

        #[derive(Debug)]
        struct OperationFailure {
            code: u32,
        }
        impl fmt::Display for OperationFailure {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "operation error {}", self.code)
            }
        }
        impl Error for OperationFailure {}
        struct FailingHost;
        impl Host for FailingHost {
            fn call(
                &mut self,
                _: &str,
                _: SigHash,
                _: &[ValueRef<'_>],
                _: &mut [Value],
                _: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                Err(HostError::custom(OperationFailure { code: 23 }))
            }
        }
        let mut pb = ProgramBuilder::new();
        let import = pb.host_sig_for(
            "fallible",
            HostSig {
                args: vec![],
                rets: vec![ValueType::I64],
            },
        );
        let mut body = Asm::new();
        body.set_register_types(vec![ValueType::Unit, ValueType::I64]);
        body.host_call_sig_id(0, import, 0, &[], &[1]);
        body.ret(0, &[1]);
        let entry = pb
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        let p = Arc::new(pb.build_verified().unwrap());
        let mut graph = ExecutionGraph::new(TapeExecutor::new(FailingHost, Limits::default()));
        let node = graph.add_tape_node(p, entry, vec![]).unwrap();
        let error = graph.run_all().unwrap_err();
        assert!(matches!(&error, GraphError::Node { node: failed, .. } if *failed == node));
        let mut source: &(dyn Error + 'static) = &error;
        loop {
            if let Some(failure) = source.downcast_ref::<OperationFailure>() {
                assert_eq!(failure.code, 23);
                break;
            }
            source = source
                .source()
                .expect("typed host failure in graph error chain");
        }
    }

    #[test]
    fn cached_aggregate_output_survives_later_node_reclamation() {
        let mut pb = ProgramBuilder::new();
        let mut producer = Asm::new();
        producer.set_register_types(vec![
            ValueType::Unit,
            ValueType::I64,
            ValueType::Agg,
            ValueType::Agg,
        ]);
        producer.tuple_new(2, &[1]);
        producer.tuple_new(3, &[2]); // Unreachable wrapper.
        producer.ret(0, &[2]);
        let producer = pb
            .push_function_checked(
                producer,
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::Agg],
                },
            )
            .unwrap();
        let mut reader = Asm::new();
        reader.set_register_types(vec![
            ValueType::Unit,
            ValueType::Agg,
            ValueType::Agg,
            ValueType::I64,
        ]);
        reader.tuple_new(2, &[1]); // Unreachable wrapper around a retained input.
        reader.tuple_get(3, 1, 0);
        reader.ret(0, &[3]);
        let integer = pb
            .contract(execution_tape::contract::Contract::Kind(ValueType::I64))
            .unwrap();
        let tuple = pb
            .contract(execution_tape::contract::Contract::Tuple(&[Some(integer)]))
            .unwrap();
        let reader = pb
            .push_function_checked(
                reader,
                execution_tape::asm::FunctionAbi {
                    args: vec![tuple.into()],
                    rets: vec![ValueType::I64.into()],
                },
            )
            .unwrap();
        let program = Arc::new(pb.build_verified().unwrap());
        let mut graph = ExecutionGraph::new(TapeExecutor::new(NoHost, Limits::default()));
        let a = graph
            .add_tape_node(Arc::clone(&program), producer, vec!["x".into()])
            .unwrap();
        let b = graph
            .add_tape_node(program, reader, vec!["tuple".into()])
            .unwrap();
        graph
            .set_input_value(a, "x", Value::I64(42).into())
            .unwrap();
        graph.connect(a, "ret0", b, "tuple").unwrap();
        assert_eq!(graph.run_all().unwrap().executed_nodes, 2);
        let retained = graph.node_outputs(a).unwrap().get("ret0").cloned();
        for _ in 0..3 {
            graph.invalidate(ResourceKey::node_output(b, crate::OutputId::new(0)));
            assert_eq!(graph.run_all().unwrap().executed_nodes, 1);
            assert_eq!(
                graph.node_outputs(a).unwrap().get("ret0"),
                retained.as_ref()
            );
            assert_eq!(
                graph.node_outputs(b).unwrap().get("ret0"),
                Some(&Value::I64(42).into())
            );
            assert_eq!(graph.executor().vm.aggregates().len_u32(), 0);
            assert_eq!(
                retained.as_ref().unwrap().aggregates().unwrap().len_u32(),
                1
            );
        }
    }
}

/// Translates tape host access events into graph dependency recording.
struct TapeAccessSink<'a, 'b, V> {
    access: &'a mut NodeAccess<'b, V>,
    /// Shared with [`StrictDepsTrace`] so strict-deps validation can verify that each host call
    /// reported at least one usable key.
    counter: &'a Cell<usize>,
}

impl<V> AccessSink for TapeAccessSink<'_, '_, V> {
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
        outcome: execution_tape::trace::TraceOutcome<'_>,
    ) {
        let ScopeKind::HostCall { .. } = kind else {
            return;
        };

        let Some((start, symbol, sig_hash)) = self.stack.pop() else {
            return;
        };

        if matches!(outcome, execution_tape::trace::TraceOutcome::Trap(_)) {
            return;
        }
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
