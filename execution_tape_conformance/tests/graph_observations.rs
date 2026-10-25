// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Selected graph/tape observations, completion evidence and checked replay.

use execution_graph::{
    ExecutionGraph, GraphError, HostOpId, NodeId, ResourceKey, TapeError, TapeExecutor,
    TapeInvocation, TapeNode, TapeObserver, TapeValue,
};
use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{
    Host, HostContext, HostError, HostSig, ResourceKeyRef, SigHash, ValueRef,
};
use execution_tape::program::ValueType;
use execution_tape::recording::{
    InstructionCapture, InstructionStatus, RecordLimits, Recorder, Recording, RegisterWatch,
    ReplayOutcome,
};
use execution_tape::trace::{TraceLocation, TraceMask, TraceSink};
use execution_tape::value::{FuncId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::Limits;
use std::sync::Arc;

struct State {
    offset: i64,
    report: bool,
    fail: bool,
}
impl Host for State {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        outputs: &mut [Value],
        mut cx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        if self.report {
            cx.record_read(ResourceKeyRef::HostState {
                op: SigHash(0),
                key: 0,
            });
        }
        if self.fail {
            return Err(HostError::UnknownSymbol);
        }
        outputs[0] = Value::I64(self.offset);
        Ok(0)
    }
}

struct Capture {
    node: NodeId,
    attempt: u64,
    succeeded: bool,
    recording: Recording,
}
#[derive(Default)]
struct Observe {
    selected: Option<NodeId>,
    all: bool,
    begun: usize,
    captures: Vec<Capture>,
}
impl TapeObserver for Observe {
    type Sink = Recorder;
    fn begin(&mut self, invocation: TapeInvocation<'_>) -> Option<Recorder> {
        self.begun += 1;
        if !self.all && self.selected != Some(invocation.node) {
            return None;
        }
        Some(
            Recorder::new(Arc::clone(invocation.program), RecordLimits::default())
                .with_instructions(InstructionCapture {
                    max_steps: 64,
                    watches: vec![RegisterWatch {
                        location: TraceLocation {
                            func: invocation.entry,
                            pc: 0,
                        },
                        registers: vec![1, 2],
                    }],
                }),
        )
    }
    fn finish(
        &mut self,
        invocation: TapeInvocation<'_>,
        recorder: Recorder,
        result: Result<(), &TapeError>,
    ) {
        self.captures.push(Capture {
            node: invocation.node,
            attempt: invocation.attempt,
            succeeded: result.is_ok(),
            recording: recorder.finish(),
        });
    }
}

fn fixture() -> (Arc<VerifiedProgram>, FuncId) {
    let mut builder = ProgramBuilder::new();
    let host = builder.host_sig_for(
        "offset",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let mut body = Asm::new();
    body.set_register_types(vec![
        ValueType::Unit,
        ValueType::I64,
        ValueType::I64,
        ValueType::I64,
        ValueType::I64,
    ]);
    body.i64_add(2, 1, 1)
        .host_call_sig_id(0, host, 0, &[], &[3])
        .i64_add(4, 2, 3)
        .ret(0, &[4]);
    let entry = builder
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    (Arc::new(builder.build_verified().unwrap()), entry)
}

fn state() -> State {
    State {
        offset: 1,
        report: true,
        fail: false,
    }
}

#[test]
fn selected_recordings_follow_actual_attempts_and_survive_graph_drop() {
    let (program, entry) = fixture();
    let mut executor =
        TapeExecutor::new(state(), Limits::default()).with_observer(Observe::default());
    executor.set_strict_deps(true);
    let mut graph = ExecutionGraph::new(executor);
    let selected = graph
        .add_tape_node(Arc::clone(&program), entry, vec!["input".into()])
        .unwrap();
    let other = graph
        .add_tape_node(program, entry, vec!["input".into()])
        .unwrap();
    graph
        .set_input_value(selected, "input", Value::I64(41).into())
        .unwrap();
    graph.connect(selected, "ret0", other, "input").unwrap();
    graph.executor_mut().observer_mut().selected = Some(selected);
    assert_eq!(graph.run_all().unwrap().executed_nodes, 2);
    assert_eq!(graph.run_all().unwrap().executed_nodes, 0);
    assert_eq!(graph.executor().observer().captures.len(), 1);
    graph.executor_mut().host_mut().offset = 2;
    graph.invalidate(ResourceKey::host_state(HostOpId::new(0), 0));
    assert_eq!(graph.run_all().unwrap().executed_nodes, 2);
    let captures = std::mem::take(&mut graph.executor_mut().observer_mut().captures);
    drop(graph);
    for (index, capture) in captures.iter().enumerate() {
        assert_eq!(capture.node, selected);
        assert_eq!(capture.attempt, index as u64 + 1);
        assert!(capture.succeeded);
        assert_eq!(
            capture.recording.instruction_status(),
            Some(InstructionStatus::Complete)
        );
        assert_eq!(capture.recording.instructions().len(), 4);
        let first = capture.recording.instruction(0).unwrap();
        assert_eq!(first.inputs.len(), 2);
        assert_eq!(first.outputs.len(), 1);
        let ReplayOutcome::Returned(snapshot) = capture.recording.replay().unwrap() else {
            panic!("recorded return")
        };
        assert_eq!(snapshot.values(), &[Value::I64(83 + index as i64)]);
    }
}

#[test]
fn vm_traps_and_strict_rejections_finish_recordings_without_replacing_cache() {
    let (program, entry) = fixture();
    let mut graph = ExecutionGraph::new(
        TapeExecutor::new(state(), Limits::default()).with_observer(Observe::default()),
    );
    let node = graph
        .add_tape_node(program, entry, vec!["input".into()])
        .unwrap();
    graph
        .set_input_value(node, "input", Value::I64(1).into())
        .unwrap();
    graph.executor_mut().observer_mut().selected = Some(node);
    graph.run_all().unwrap();
    for strict in [false, true] {
        graph.executor_mut().host_mut().fail = !strict;
        graph.executor_mut().host_mut().report = false;
        graph.executor_mut().set_strict_deps(strict);
        graph.invalidate_input("input");
        assert!(graph.run_node(node).is_err());
        assert_eq!(
            graph
                .node_outputs(node)
                .unwrap()
                .get("ret0")
                .map(TapeValue::value),
            Some(&Value::I64(3))
        );
        let capture = graph.executor_mut().observer_mut().captures.pop().unwrap();
        assert!(!capture.succeeded);
        assert!(matches!(
            (strict, capture.recording.replay().unwrap()),
            (false, ReplayOutcome::Trapped(_)) | (true, ReplayOutcome::Returned(_))
        ));
    }
}

#[test]
fn invocation_success_does_not_claim_graph_publication() {
    let (program, entry) = fixture();
    let mut graph = ExecutionGraph::new(
        TapeExecutor::new(state(), Limits::default()).with_observer(Observe::default()),
    );
    let node = graph
        .add_node(
            TapeNode::new(program, entry).unwrap(),
            vec!["input".into()],
            vec!["one".into(), "two".into()],
        )
        .unwrap();
    graph
        .set_input_value(node, "input", Value::I64(1).into())
        .unwrap();
    graph.executor_mut().observer_mut().selected = Some(node);
    assert!(matches!(
        graph.run_node(node),
        Err(GraphError::BadOutputArity { .. })
    ));
    let capture = graph.executor_mut().observer_mut().captures.pop().unwrap();
    assert!(capture.succeeded);
    assert!(matches!(
        capture.recording.replay().unwrap(),
        ReplayOutcome::Returned(_)
    ));
    assert!(graph.node_outputs(node).unwrap().get("one").is_none());
}

#[derive(Default)]
struct RunOnly {
    disabled: bool,
    masks: Vec<TraceMask>,
    starts: usize,
    ends: usize,
}
impl TraceSink for RunOnly {
    fn mask(&self) -> TraceMask {
        if self.disabled {
            TraceMask::NONE
        } else {
            TraceMask::RUN
        }
    }
    fn run_mask(&mut self, _: &execution_tape::program::Program, mask: TraceMask) {
        self.masks.push(mask);
    }
    fn run_start(&mut self, _: &execution_tape::program::Program, _: FuncId, _: usize) {
        self.starts += 1;
    }
    fn run_end(
        &mut self,
        _: &execution_tape::program::Program,
        _: execution_tape::trace::TraceOutcome<'_>,
    ) {
        self.ends += 1;
    }
    fn scope_enter(
        &mut self,
        _: &execution_tape::program::Program,
        _: execution_tape::trace::ScopeKind,
        _: usize,
        _: FuncId,
        _: u32,
        _: Option<u64>,
    ) {
        panic!("internal strict-deps events must not leak");
    }
}
#[derive(Default)]
struct Runs {
    disabled: bool,
    completed: Vec<RunOnly>,
}
impl TapeObserver for Runs {
    type Sink = RunOnly;
    fn begin(&mut self, _: TapeInvocation<'_>) -> Option<RunOnly> {
        Some(RunOnly {
            disabled: self.disabled,
            ..RunOnly::default()
        })
    }
    fn finish(&mut self, _: TapeInvocation<'_>, sink: RunOnly, _: Result<(), &TapeError>) {
        self.completed.push(sink);
    }
}
#[test]
fn strict_events_do_not_expand_the_observers_delivery_mask() {
    let (program, entry) = fixture();
    let mut executor = TapeExecutor::new(state(), Limits::default()).with_observer(Runs::default());
    executor.set_strict_deps(true);
    let mut graph = ExecutionGraph::new(executor);
    let node = graph
        .add_tape_node(program, entry, vec!["input".into()])
        .unwrap();
    graph
        .set_input_value(node, "input", Value::I64(1).into())
        .unwrap();
    graph.run_all().unwrap();
    let observed = &graph.executor().observer().completed[0];
    assert_eq!(observed.masks, [TraceMask::RUN]);
    assert_eq!((observed.starts, observed.ends), (1, 1));
}

#[test]
fn executed_unchanged_results_are_observed_but_cutoff_dependents_are_not() {
    let (program, entry) = fixture();
    let mut executor = TapeExecutor::new(state(), Limits::default()).with_observer(Observe {
        all: true,
        ..Observe::default()
    });
    executor.set_early_cutoff(true);
    let mut graph = ExecutionGraph::new(executor);
    let first = graph
        .add_tape_node(Arc::clone(&program), entry, vec!["input".into()])
        .unwrap();
    let second = graph
        .add_tape_node(program, entry, vec!["input".into()])
        .unwrap();
    graph
        .set_input_value(first, "input", Value::I64(1).into())
        .unwrap();
    graph.connect(first, "ret0", second, "input").unwrap();
    graph.run_all().unwrap();
    graph.invalidate_input("input");
    assert_eq!(graph.run_all().unwrap().executed_nodes, 1);
    let captures = &graph.executor().observer().captures;
    assert_eq!(captures.len(), 3);
    assert_eq!((captures[2].node, captures[2].attempt), (first, 2));
    assert_eq!(graph.node_run_count(second), Some(1));
}

#[test]
fn selected_zero_mask_has_completion_but_no_trace_callbacks() {
    let (program, entry) = fixture();
    let mut host = state();
    host.report = false;
    let mut executor = TapeExecutor::new(host, Limits::default()).with_observer(Runs {
        disabled: true,
        ..Runs::default()
    });
    executor.set_strict_deps(true);
    let mut graph = ExecutionGraph::new(executor);
    let node = graph
        .add_tape_node(program, entry, vec!["input".into()])
        .unwrap();
    graph
        .set_input_value(node, "input", Value::I64(1).into())
        .unwrap();
    assert!(matches!(
        graph.run_all(),
        Err(GraphError::Node {
            source: TapeError::StrictDepsViolation { .. },
            ..
        })
    ));
    let completed = &graph.executor().observer().completed;
    assert_eq!(completed.len(), 1);
    assert!(completed[0].masks.is_empty());
    assert_eq!((completed[0].starts, completed[0].ends), (0, 0));
}

#[test]
fn ownership_rejection_precedes_observer_admission() {
    let (program, entry) = fixture();
    let mut graph = ExecutionGraph::new(
        TapeExecutor::new(state(), Limits::default()).with_observer(Observe {
            all: true,
            ..Observe::default()
        }),
    );
    let node = graph
        .add_tape_node(program, entry, vec!["input".into()])
        .unwrap();
    graph
        .set_input_value(node, "input", Value::Func(entry).into())
        .unwrap();
    assert!(matches!(
        graph.run_all(),
        Err(GraphError::Node {
            source: TapeError::UnownedInput { input: 0 },
            ..
        })
    ));
    assert_eq!(graph.executor().observer().begun, 0);
    assert!(graph.executor().observer().captures.is_empty());
}
