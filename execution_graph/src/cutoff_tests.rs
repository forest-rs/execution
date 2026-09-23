// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Early cutoff: re-run nodes whose outputs are unchanged stop propagation.

extern crate std;

use alloc::vec;
use core::convert::Infallible;

use super::*;
use crate::native::{FnExecutor, FnNode};

type Graph = ExecutionGraph<FnExecutor<i64, Infallible>>;

fn cutoff_graph() -> Graph {
    ExecutionGraph::new(FnExecutor::new().with_value_eq(|a, b| a == b))
}

/// Clamps its input to `0..=10`.
fn clamp_node() -> FnNode<i64, Infallible> {
    FnNode::new(|inputs: &[i64], outputs, _access| {
        outputs.push(inputs[0].clamp(0, 10));
        Ok(())
    })
}

fn sum_node() -> FnNode<i64, Infallible> {
    FnNode::new(|inputs, outputs, _access| {
        outputs.push(inputs.iter().sum());
        Ok(())
    })
}

/// `clamp(x)` feeding `b = clamp + 1` and `c = clamp + 2`, both feeding `d = b + c`.
fn diamond(g: &mut Graph) -> [NodeId; 4] {
    let a = g
        .add_node(clamp_node(), vec!["x".into()], vec!["value".into()])
        .unwrap();
    let b = g
        .add_node(
            sum_node(),
            vec!["value".into(), "one".into()],
            vec!["value".into()],
        )
        .unwrap();
    let c = g
        .add_node(
            sum_node(),
            vec!["value".into(), "two".into()],
            vec!["value".into()],
        )
        .unwrap();
    let d = g
        .add_node(
            sum_node(),
            vec!["lhs".into(), "rhs".into()],
            vec!["value".into()],
        )
        .unwrap();
    g.set_input_value(a, "x", 12).unwrap();
    g.set_input_value(b, "one", 1).unwrap();
    g.set_input_value(c, "two", 2).unwrap();
    g.connect(a, "value", b, "value").unwrap();
    g.connect(a, "value", c, "value").unwrap();
    g.connect(b, "value", d, "lhs").unwrap();
    g.connect(c, "value", d, "rhs").unwrap();
    [a, b, c, d]
}

fn value(g: &Graph, node: NodeId) -> i64 {
    *g.node_outputs(node).unwrap().get("value").unwrap()
}

#[test]
fn an_unchanged_output_cuts_off_its_dependents() {
    let mut g = cutoff_graph();
    let [a, b, c, d] = diamond(&mut g);
    assert_eq!(g.run_all().unwrap().executed_nodes, 4);
    assert_eq!(value(&g, d), 23);

    // 12 -> 15 still clamps to 10: only the clamp re-runs.
    g.set_input_value(a, "x", 15).unwrap();
    g.invalidate_input("x");
    let summary = g.run_all().unwrap();
    assert_eq!(summary.executed_nodes, 1);
    assert_eq!(summary.cut_off_nodes, 3);
    for (node, runs) in [(a, 2), (b, 1), (c, 1), (d, 1)] {
        assert_eq!(g.node_run_count(node), Some(runs));
    }
    assert_eq!(g.run_all().unwrap(), RunSummary::default());

    // A real change still propagates through the whole diamond.
    g.set_input_value(a, "x", 3).unwrap();
    g.invalidate_input("x");
    let summary = g.run_all().unwrap();
    assert_eq!((summary.executed_nodes, summary.cut_off_nodes), (4, 0));
    assert_eq!(value(&g, d), 9);
}

#[test]
fn a_node_still_runs_when_another_read_changed() {
    let mut g = cutoff_graph();
    let [a, b, c, d] = diamond(&mut g);
    g.run_all().unwrap();

    // The clamp is unchanged, but `c` also reads `two`, which changed.
    g.set_input_value(a, "x", 11).unwrap();
    g.invalidate_input("x");
    g.set_input_value(c, "two", 5).unwrap();
    g.invalidate_input("two");
    let summary = g.run_all().unwrap();
    assert_eq!((summary.executed_nodes, summary.cut_off_nodes), (3, 1));
    for (node, runs) in [(a, 2), (b, 1), (c, 2), (d, 2)] {
        assert_eq!(g.node_run_count(node), Some(runs));
    }
    assert_eq!(value(&g, d), 26);
}

#[test]
fn cutoff_is_per_output() {
    let mut g = cutoff_graph();
    // `split` emits `x / 10` and `x % 10`; readers of each output cut off independently.
    let split = g
        .add_node(
            FnNode::new(|inputs, outputs, _access| {
                outputs.push(inputs[0] / 10);
                outputs.push(inputs[0] % 10);
                Ok(())
            }),
            vec!["x".into()],
            vec!["tens".into(), "ones".into()],
        )
        .unwrap();
    let tens = g
        .add_node(sum_node(), vec!["v".into()], vec!["value".into()])
        .unwrap();
    let ones = g
        .add_node(sum_node(), vec!["v".into()], vec!["value".into()])
        .unwrap();
    g.set_input_value(split, "x", 42).unwrap();
    g.connect(split, "tens", tens, "v").unwrap();
    g.connect(split, "ones", ones, "v").unwrap();
    g.run_all().unwrap();

    g.set_input_value(split, "x", 47).unwrap();
    g.invalidate_input("x");
    let report = g.run_all_with_report(ReportDetailMask::FULL).unwrap();
    let executed: Vec<NodeId> = report.executed.iter().map(|r| r.node).collect();
    let cut_off: Vec<NodeId> = report.cut_off.iter().map(|r| r.node).collect();
    assert_eq!(executed, vec![split, ones]);
    assert_eq!(cut_off, vec![tens]);
    assert_eq!(
        report.cut_off[0].because_of,
        Some(ResourceKey::node_output(tens, "value"))
    );
    assert!(report.cut_off[0].why_path.is_some());
    assert_eq!(value(&g, ones), 7);
    assert_eq!(value(&g, tens), 4);
}

// A follow-up `run_all` is not asserted here: on the base branch a scoped drain drops the
// pending work of dependents outside the target's closure (fixed separately in
// forest-rs/execution#99), so `c` and `d` would not be revisited either way.
#[test]
fn targeted_runs_cut_off_within_the_closure() {
    let mut g = cutoff_graph();
    let [a, b, c, d] = diamond(&mut g);
    g.run_all().unwrap();

    g.set_input_value(a, "x", 20).unwrap();
    g.invalidate_input("x");
    let summary = g.run_node(b).unwrap();
    assert_eq!((summary.executed_nodes, summary.cut_off_nodes), (1, 1));
    assert_eq!(g.node_run_count(a), Some(2));
    assert_eq!(g.node_run_count(b), Some(1));
    assert_eq!(g.node_run_count(c), Some(1));
    assert_eq!(g.node_run_count(d), Some(1));
}

#[test]
fn writes_count_as_changes_within_a_run() {
    let op = HostOpId::new(7);
    let mut g = cutoff_graph();
    // The writer's output never changes, but it writes host state a later reader consults.
    let writer = g
        .add_node(
            FnNode::new(move |_inputs, outputs, access| {
                access.write_host_state(op, 1);
                outputs.push(0);
                Ok(())
            }),
            vec!["x".into()],
            vec!["value".into()],
        )
        .unwrap();
    let reader = g
        .add_node(
            FnNode::new(move |inputs, outputs, access| {
                access.read_host_state(op, 1);
                outputs.push(inputs[0]);
                Ok(())
            }),
            vec!["v".into()],
            vec!["value".into()],
        )
        .unwrap();
    g.set_input_value(writer, "x", 1).unwrap();
    g.connect(writer, "value", reader, "v").unwrap();
    g.run_all().unwrap();
    // The first run's write is still pending for the reader; drain it.
    g.run_all().unwrap();
    assert_eq!(g.node_run_count(reader), Some(2));

    g.invalidate_input("x");
    let summary = g.run_all().unwrap();
    assert_eq!((summary.executed_nodes, summary.cut_off_nodes), (2, 0));
    assert_eq!(g.node_run_count(reader), Some(3));
}

#[test]
fn read_modify_write_nodes_still_reach_a_fixpoint() {
    let op = HostOpId::new(9);
    let mut g = cutoff_graph();
    let rmw = g
        .add_node(
            FnNode::new(move |_inputs, outputs, access| {
                access.read_opaque_host(op);
                access.write_opaque_host(op);
                outputs.push(1);
                Ok(())
            }),
            vec![],
            vec!["value".into()],
        )
        .unwrap();
    let reader = g
        .add_node(
            FnNode::new(move |_inputs, outputs, access| {
                access.read_opaque_host(op);
                outputs.push(2);
                Ok(())
            }),
            vec![],
            vec!["value".into()],
        )
        .unwrap();

    g.run_all().unwrap();
    // The write dirtied the opaque key: the reader re-runs, the writer does not re-trigger
    // itself, and then the graph is quiet.
    let summary = g.run_all().unwrap();
    assert_eq!((summary.executed_nodes, summary.cut_off_nodes), (1, 0));
    assert_eq!(g.node_run_count(rmw), Some(1));
    assert_eq!(g.node_run_count(reader), Some(2));
    assert_eq!(g.run_all().unwrap(), RunSummary::default());
}

#[test]
fn explicitly_invalidated_outputs_always_run() {
    let mut g = cutoff_graph();
    let [_, b, _, d] = diamond(&mut g);
    g.run_all().unwrap();

    g.invalidate(ResourceKey::node_output(b, "value"));
    let summary = g.run_all().unwrap();
    // `b` re-runs because it was marked directly; its unchanged output cuts `d` off.
    assert_eq!((summary.executed_nodes, summary.cut_off_nodes), (1, 1));
    assert_eq!(g.node_run_count(b), Some(2));
    assert_eq!(g.node_run_count(d), Some(1));
}

#[test]
fn rewiring_after_a_run_is_never_cut_off() {
    let mut g = cutoff_graph();
    let [a, b, c, d] = diamond(&mut g);
    g.run_all().unwrap();

    // Swap `d`'s inputs: nothing upstream changed, but `d` must run with its new wiring.
    g.connect(c, "value", d, "lhs").unwrap();
    g.connect(b, "value", d, "rhs").unwrap();
    let summary = g.run_all().unwrap();
    assert_eq!(summary.executed_nodes, 1);
    assert_eq!(g.node_run_count(d), Some(2));
    assert_eq!(g.node_run_count(a), Some(1));
}

#[test]
fn rebinding_an_input_after_a_run_is_never_cut_off() {
    // `n` first reads `a.value`, then is rebound to an external value. A later upstream change
    // that leaves `a.value` equal must not cut `n` off on the strength of its old read set.
    let mut g = cutoff_graph();
    let a = g
        .add_node(clamp_node(), vec!["x".into()], vec!["value".into()])
        .unwrap();
    let n = g
        .add_node(sum_node(), vec!["v".into()], vec!["value".into()])
        .unwrap();
    g.set_input_value(a, "x", 12).unwrap();
    g.connect(a, "value", n, "v").unwrap();
    g.run_all().unwrap();
    assert_eq!(value(&g, n), 10);

    g.set_input_value(n, "v", 99).unwrap();
    g.set_input_value(a, "x", 15).unwrap();
    g.invalidate_input("x");
    g.run_all().unwrap();
    assert_eq!(value(&g, n), 99);
}

#[test]
fn invalidating_a_newly_bound_input_reaches_the_node() {
    // Rebinding adds a conservative edge, so the new input's invalidation schedules the node
    // before it has ever read that key.
    let mut g = cutoff_graph();
    let a = g
        .add_node(clamp_node(), vec!["x".into()], vec!["value".into()])
        .unwrap();
    let n = g
        .add_node(sum_node(), vec!["v".into()], vec!["value".into()])
        .unwrap();
    g.set_input_value(a, "x", 1).unwrap();
    g.connect(a, "value", n, "v").unwrap();
    g.run_all().unwrap();

    g.set_input_value(n, "v", 7).unwrap();
    g.invalidate_input("v");
    let summary = g.run_all().unwrap();
    assert_eq!(summary.executed_nodes, 1);
    assert_eq!(value(&g, n), 7);
}

#[test]
fn rebinding_to_a_different_key_schedules_the_node_like_connect() {
    let mut g = cutoff_graph();
    let a = g
        .add_node(clamp_node(), vec!["x".into()], vec!["value".into()])
        .unwrap();
    let n = g
        .add_node(sum_node(), vec!["v".into()], vec!["value".into()])
        .unwrap();
    g.set_input_value(a, "x", 3).unwrap();
    g.connect(a, "value", n, "v").unwrap();
    g.run_all().unwrap();
    assert_eq!(value(&g, n), 3);

    // `FromNode` -> `External`: the node runs with the new binding without an invalidation.
    g.set_input_value(n, "v", 42).unwrap();
    let summary = g.run_all().unwrap();
    assert_eq!((summary.executed_nodes, summary.cut_off_nodes), (1, 0));
    assert_eq!(value(&g, n), 42);

    // Same key, new value: not scheduled until the input is invalidated.
    g.set_input_value(n, "v", 43).unwrap();
    assert_eq!(g.run_all().unwrap().executed_nodes, 0);
    assert_eq!(value(&g, n), 42);
    g.invalidate_input("v");
    assert_eq!(g.run_all().unwrap().executed_nodes, 1);
    assert_eq!(value(&g, n), 43);
}

#[test]
fn executors_without_equality_never_cut_off() {
    let mut g: Graph = ExecutionGraph::new(FnExecutor::new());
    let [a, b, c, d] = diamond(&mut g);
    g.run_all().unwrap();

    g.set_input_value(a, "x", 15).unwrap();
    g.invalidate_input("x");
    let summary = g.run_all().unwrap();
    assert_eq!((summary.executed_nodes, summary.cut_off_nodes), (4, 0));
    for node in [a, b, c, d] {
        assert_eq!(g.node_run_count(node), Some(2));
    }
}

#[cfg(feature = "tape")]
mod tape {
    use alloc::string::String;
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;

    use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
    use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
    use execution_tape::program::ValueType;
    use execution_tape::value::Value;
    use execution_tape::vm::Limits;

    use super::super::*;
    use crate::executor::Executor;
    use crate::native::{FnExecutor, FnNode};
    use crate::node_access::NodeAccess;
    use crate::tape::{TapeError, TapeExecutor, TapeNode};

    #[derive(Debug)]
    struct HostNoop;

    impl Host for HostNoop {
        fn call(
            &mut self,
            _symbol: &str,
            _sig_hash: SigHash,
            _args: &[ValueRef<'_>],
            _rets: &mut [Value],
            _ctx: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            Err(HostError::UnknownSymbol)
        }
    }

    #[derive(Debug)]
    enum Mixed {
        Tape(TapeNode),
        Native(FnNode<Value, TapeError>),
    }

    #[derive(Debug)]
    struct MixedExecutor {
        tape: TapeExecutor<HostNoop>,
        native: FnExecutor<Value, TapeError>,
    }

    impl Executor for MixedExecutor {
        type Value = Value;
        type Node = Mixed;
        type Error = TapeError;

        fn execute(
            &mut self,
            node: &mut Mixed,
            inputs: &[Value],
            outputs: &mut Vec<Value>,
            access: &mut NodeAccess<'_>,
        ) -> Result<(), TapeError> {
            match node {
                Mixed::Tape(n) => self.tape.execute(n, inputs, outputs, access),
                Mixed::Native(n) => self.native.execute(n, inputs, outputs, access),
            }
        }

        fn values_equal(&self, previous: &Value, next: &Value) -> bool {
            self.tape.values_equal(previous, next)
        }

        fn describe(&self, _node: &Mixed) -> Option<String> {
            None
        }
    }

    /// `fn constant(x: i64) -> i64 { 7 }`: re-runs on any input, output never changes.
    fn constant_program() -> TapeNode {
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        a.const_i64(2, 7);
        a.ret(0, &[2]);
        let entry = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        pb.set_function_output_name(entry, 0, "value").unwrap();
        TapeNode::new(Arc::new(pb.build_verified().unwrap()), entry).unwrap()
    }

    fn graph(early_cutoff: bool) -> (ExecutionGraph<MixedExecutor>, NodeId, NodeId) {
        let mut tape = TapeExecutor::new(HostNoop, Limits::default());
        tape.set_early_cutoff(early_cutoff);
        let mut g = ExecutionGraph::new(MixedExecutor {
            tape,
            native: FnExecutor::new(),
        });
        let node = constant_program();
        let outputs = node.output_names();
        let constant = g
            .add_node(Mixed::Tape(node), vec!["x".into()], outputs)
            .unwrap();
        let reader = g
            .add_node(
                Mixed::Native(FnNode::new(|inputs: &[Value], outputs, _access| {
                    outputs.push(inputs[0].clone());
                    Ok(())
                })),
                vec!["v".into()],
                vec!["value".into()],
            )
            .unwrap();
        g.set_input_value(constant, "x", Value::I64(1)).unwrap();
        g.connect(constant, "value", reader, "v").unwrap();
        g.run_all().unwrap();
        (g, constant, reader)
    }

    #[test]
    fn tape_outputs_cut_off_native_readers_when_enabled() {
        for (early_cutoff, reader_runs) in [(false, 2), (true, 1)] {
            let (mut g, constant, reader) = graph(early_cutoff);
            g.set_input_value(constant, "x", Value::I64(2)).unwrap();
            g.invalidate_input("x");
            g.run_all().unwrap();
            assert_eq!(g.node_run_count(constant), Some(2));
            assert_eq!(g.node_run_count(reader), Some(reader_runs));
        }
    }

    #[test]
    fn tape_equality_compares_plain_values_by_bits() {
        let mut tape = TapeExecutor::new(HostNoop, Limits::default());
        assert!(!tape.values_equal(&Value::I64(1), &Value::I64(1)));
        tape.set_early_cutoff(true);
        assert!(tape.values_equal(&Value::I64(1), &Value::I64(1)));
        assert!(!tape.values_equal(&Value::I64(1), &Value::U64(1)));
        assert!(tape.values_equal(&Value::F64(f64::NAN), &Value::F64(f64::NAN)));
        assert!(!tape.values_equal(&Value::F64(0.0), &Value::F64(-0.0)));
        assert!(tape.values_equal(&Value::Str("a".into()), &Value::Str("a".into())));
        assert!(tape.values_equal(&Value::Unit, &Value::Unit));
    }
}
