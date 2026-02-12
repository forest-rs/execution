// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Internal dispatch interfaces for executing [`RunPlan`] values.
//!
//! This module intentionally stays internal. It provides a stable seam between planning ("what to
//! run") and execution strategy ("how to run"), so future scheduler work can swap dispatch
//! implementations without reshaping `ExecutionGraph` public APIs.

use alloc::vec::Vec;
use execution_tape::host::Host;

use crate::access::NodeId;
use crate::graph::{ExecutionGraph, GraphError};
use crate::plan::{PlanScope, RunPlan};
use crate::report::RunDetailReport;

/// Internal dispatcher contract.
///
/// Dispatchers execute nodes in a precomputed [`RunPlan`] and may optionally assemble traced
/// reporting if the plan carries trace payload.
pub(crate) trait Dispatcher<H: Host> {
    /// Executes `plan` without producing traced reporting.
    ///
    /// Returns the drained scheduling buffer so callers can reuse its capacity.
    fn dispatch(
        &mut self,
        graph: &mut ExecutionGraph<H>,
        plan: RunPlan,
    ) -> Result<Vec<NodeId>, GraphError>;

    /// Executes `plan` and returns traced reporting if available.
    ///
    /// Returns both the drained scheduling buffer (for capacity reuse) and the assembled report.
    fn dispatch_with_report(
        &mut self,
        graph: &mut ExecutionGraph<H>,
        plan: RunPlan,
    ) -> Result<(Vec<NodeId>, RunDetailReport), GraphError>;
}

/// Serial in-thread dispatcher used by default.
///
/// Nodes are executed in the order provided by the [`RunPlan`], preserving deterministic behavior
/// and fail-fast error semantics.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct InlineDispatcher;

impl<H: Host> Dispatcher<H> for InlineDispatcher {
    #[inline]
    fn dispatch(
        &mut self,
        graph: &mut ExecutionGraph<H>,
        mut plan: RunPlan,
    ) -> Result<Vec<NodeId>, GraphError> {
        // Keep scope as part of the dispatch contract even before scope-specific strategies exist.
        match plan.scope() {
            PlanScope::All | PlanScope::WithinDependenciesOf(_) => {}
        }

        let mut to_run: Vec<NodeId> = plan.take_nodes();
        for node in to_run.drain(..) {
            graph.execute_scheduled_node(node)?;
        }
        Ok(to_run)
    }

    #[inline]
    fn dispatch_with_report(
        &mut self,
        graph: &mut ExecutionGraph<H>,
        mut plan: RunPlan,
    ) -> Result<(Vec<NodeId>, RunDetailReport), GraphError> {
        // Keep scope as part of the dispatch contract even before scope-specific strategies exist.
        match plan.scope() {
            PlanScope::All | PlanScope::WithinDependenciesOf(_) => {}
        }

        let mut trace = plan.take_trace();
        let mut report = RunDetailReport::default();
        let mut to_run: Vec<NodeId> = plan.take_nodes();

        for node in to_run.drain(..) {
            graph.execute_scheduled_node(node)?;
            if let Some(t) = trace.as_mut()
                && let Some(r) = t.take_report_for(node)
            {
                report.executed.push(r);
            }
        }

        Ok((to_run, report))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{Dispatcher, InlineDispatcher};
    use crate::access::ResourceKey;
    use crate::graph::{ExecutionGraph, GraphError};
    use crate::plan::{RunPlan, RunPlanTrace};
    use crate::report::NodeRunDetail;
    use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
    use execution_tape::host::{AccessSink, Host, HostError, SigHash, ValueRef};
    use execution_tape::program::ValueType;
    use execution_tape::value::{FuncId, Value};
    use execution_tape::verifier::VerifiedProgram;
    use execution_tape::vm::Limits;

    #[derive(Debug, Default)]
    struct HostNoop;

    impl Host for HostNoop {
        fn call(
            &mut self,
            _symbol: &str,
            _sig_hash: SigHash,
            _args: &[ValueRef<'_>],
            _access: Option<&mut dyn AccessSink>,
        ) -> Result<(Vec<Value>, u64), HostError> {
            Err(HostError::UnknownSymbol)
        }
    }

    fn make_identity_program(output_name: &str) -> (Arc<VerifiedProgram>, FuncId) {
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        a.ret(0, &[1]);
        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::I64],
                    reg_count: 2,
                },
            )
            .expect("identity function should be valid");
        pb.set_function_output_name(f, 0, output_name)
            .expect("name assignment should succeed");
        (
            Arc::new(pb.build_verified().expect("program should verify")),
            f,
        )
    }

    fn make_const_program(output_name: &str, value: i64) -> (Arc<VerifiedProgram>, FuncId) {
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        a.const_i64(1, value);
        a.ret(0, &[1]);
        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                    reg_count: 2,
                },
            )
            .expect("const function should be valid");
        pb.set_function_output_name(f, 0, output_name)
            .expect("name assignment should succeed");
        (
            Arc::new(pb.build_verified().expect("program should verify")),
            f,
        )
    }

    #[test]
    fn inline_dispatcher_fail_fast_matches_graph_error_semantics() {
        let (needs_input_prog, needs_input_entry) = make_identity_program("value");
        let (const_prog, const_entry) = make_const_program("value", 7);

        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let n_err = g.add_node(needs_input_prog, needs_input_entry, vec!["in".into()]);
        let n_ok = g.add_node(const_prog, const_entry, vec![]);
        let plan = RunPlan::all(vec![n_err, n_ok]);
        let mut dispatcher = InlineDispatcher;

        assert_eq!(
            dispatcher.dispatch(&mut g, plan),
            Err(GraphError::MissingInput {
                node: n_err,
                name: "in".into()
            })
        );

        assert_eq!(g.node_run_count(n_err), Some(0));
        assert_eq!(g.node_run_count(n_ok), Some(0));
    }

    #[test]
    fn inline_dispatcher_with_report_keeps_execution_order() {
        let (prog, entry) = make_const_program("value", 11);

        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let n0 = g.add_node(prog.clone(), entry, vec![]);
        let n1 = g.add_node(prog, entry, vec![]);

        let r0 = NodeRunDetail {
            node: n0,
            because_of: Some(ResourceKey::node_output(n0, "value")),
            why_path: Some(vec![ResourceKey::input("seed")]),
        };
        let r1 = NodeRunDetail {
            node: n1,
            because_of: Some(ResourceKey::node_output(n1, "value")),
            why_path: Some(vec![ResourceKey::input("seed")]),
        };

        let mut node_reports = vec![None; 2];
        node_reports[0] = Some(r0.clone());
        node_reports[1] = Some(r1.clone());

        let plan =
            RunPlan::all(vec![n1, n0]).with_trace(RunPlanTrace::from_node_reports(node_reports));
        let mut dispatcher = InlineDispatcher;
        let (_buf, report) = dispatcher
            .dispatch_with_report(&mut g, plan)
            .expect("dispatch should succeed");

        assert_eq!(report.executed.len(), 2);
        assert_eq!(report.executed[0], r1);
        assert_eq!(report.executed[1], r0);
    }

    #[test]
    fn inline_dispatcher_with_report_handles_short_trace_vectors() {
        let (prog, entry) = make_const_program("value", 5);
        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let node = g.add_node(prog, entry, vec![]);

        // Empty trace payload: execution should still succeed and simply produce no traced rows.
        let trace = RunPlanTrace::from_node_reports(vec![]);

        let mut dispatcher = InlineDispatcher;
        let (_buf, out) = dispatcher
            .dispatch_with_report(&mut g, RunPlan::all(vec![node]).with_trace(trace))
            .expect("dispatch should succeed");

        assert_eq!(g.node_run_count(node), Some(1));
        assert!(out.executed.is_empty());
    }
}
