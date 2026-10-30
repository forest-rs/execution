// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

extern crate std;

use crate::*;
use alloc::{boxed::Box, rc::Rc, vec, vec::Vec};
use core::{
    cell::{Cell, RefCell},
    task::{Context, Poll, Waker},
};

type Graph = ExecutionGraph<FnExecutor<i64, &'static str>>;
const ASSET: HostOpId = HostOpId::new(77);

#[derive(Debug, Default)]
struct Reply {
    value: Option<i64>,
    waker: Option<Waker>,
    dropped: usize,
}
#[derive(Debug)]
struct Request {
    reply: Rc<RefCell<Reply>>,
    input: i64,
    polls: usize,
    watch: bool,
}
impl Drop for Request {
    fn drop(&mut self) {
        self.reply.borrow_mut().dropped += 1;
    }
}
impl NodeTask<i64, &'static str> for Request {
    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        outputs: &mut Vec<i64>,
        access: &mut NodeAccess<'_, i64>,
    ) -> Poll<Result<(), &'static str>> {
        self.polls += 1;
        if self.watch {
            access.read_host_state(ASSET, 0);
        }
        let mut reply = self.reply.borrow_mut();
        if let Some(value) = reply.value.take() {
            outputs.clear();
            outputs.push(self.input + value);
            Poll::Ready(Ok(()))
        } else {
            if self.polls == 1 {
                outputs.push(-999);
            }
            reply.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
    fn description(&self) -> Option<&str> {
        Some("worker asset reply")
    }
}
fn deferred(
    replies: Rc<RefCell<Vec<Rc<RefCell<Reply>>>>>,
    watch: bool,
) -> FnNode<i64, &'static str> {
    FnNode::deferred(move |inputs, _| {
        let reply = Rc::new(RefCell::new(Reply::default()));
        replies.borrow_mut().push(Rc::clone(&reply));
        Ok(Box::new(Request {
            reply,
            input: inputs.first().copied().unwrap_or(0),
            polls: 0,
            watch,
        }))
    })
}
fn answer(reply: &Rc<RefCell<Reply>>, value: i64) {
    let waker = {
        let mut reply = reply.borrow_mut();
        reply.value = Some(value);
        reply.waker.take()
    };
    if let Some(waker) = waker {
        waker.wake();
    }
}

#[test]
fn pending_task_does_not_block_independent_work_or_publish_tentative_values() {
    let replies = Rc::new(RefCell::new(Vec::new()));
    let mut graph = Graph::new(FnExecutor::new());
    graph.set_collect_task_events(true);
    let task = graph
        .add_node(
            deferred(Rc::clone(&replies), false),
            vec![],
            vec!["asset".into()],
        )
        .unwrap();
    let downstream = graph
        .add_node(
            FnNode::new(|input, output, _| {
                output.push(input[0] * 2);
                Ok(())
            }),
            vec!["asset".into()],
            vec!["mesh".into()],
        )
        .unwrap();
    graph.connect(task, "asset", downstream, "asset").unwrap();
    let independent = graph
        .add_node(
            FnNode::new(|_, output, _| {
                output.push(42);
                Ok(())
            }),
            vec![],
            vec!["value".into()],
        )
        .unwrap();
    let report = graph.run_all_with_report(ReportDetailMask::FULL).unwrap();
    assert_eq!(report.waiting, [task]);
    assert_eq!(report.blocked, [downstream]);
    assert_eq!(report.executed.len(), 1);
    assert_eq!(
        graph.node_outputs(independent).unwrap().get("value"),
        Some(&42)
    );
    assert!(graph.node_outputs(task).unwrap().get("asset").is_none());
    assert_eq!(graph.node_attempt_count(downstream), Some(0));
    assert_eq!(graph.node_status(task), Some(NodeStatus::Waiting));
    assert_eq!(
        graph.task_info(task).unwrap().description,
        Some("worker asset reply")
    );
    graph.run_all().unwrap();
    assert_eq!(
        replies.borrow().len(),
        1,
        "waiting does not restart the body"
    );
    assert_eq!(graph.task_info(task).unwrap().polls, 2);
    assert_eq!(graph.node_attempt_count(task), Some(1));
    answer(&replies.borrow()[0], 7);
    let summary = graph.run_all().unwrap();
    assert_eq!(summary.executed_nodes, 2);
    assert_eq!(summary.task_polls, 1);
    assert_eq!(
        graph.node_outputs(downstream).unwrap().get("mesh"),
        Some(&14)
    );
    assert_eq!(graph.node_status(task), Some(NodeStatus::Current));
    assert!(graph.task_info(task).is_none());
    assert_eq!(replies.borrow()[0].borrow().dropped, 1);
    let events = graph.take_task_events();
    assert_eq!(
        events.iter().map(|e| e.id.attempt).collect::<Vec<_>>(),
        [1, 1, 1, 1]
    );
    assert_eq!(events.last().unwrap().kind, TaskEventKind::Published);
}

#[test]
fn rebind_cancels_pending_work_and_late_reply_cannot_replace_the_old_cache() {
    let replies = Rc::new(RefCell::new(Vec::new()));
    let mut graph = Graph::new(FnExecutor::new());
    let node = graph
        .add_node(
            deferred(Rc::clone(&replies), false),
            vec!["version".into()],
            vec!["asset".into()],
        )
        .unwrap();
    graph.set_input_value(node, "version", 1).unwrap();
    graph.run_all().unwrap();
    answer(&replies.borrow()[0], 10);
    graph.run_all().unwrap();
    assert_eq!(graph.node_outputs(node).unwrap().get("asset"), Some(&11));
    graph.invalidate_input("version");
    graph.run_all().unwrap();
    let old = Rc::clone(&replies.borrow()[1]);
    graph.set_input_value(node, "version", 2).unwrap();
    assert_eq!(old.borrow().dropped, 1);
    answer(&old, 9999);
    graph.run_all().unwrap();
    assert_eq!(graph.node_outputs(node).unwrap().get("asset"), Some(&11));
    assert_eq!(graph.node_status(node), Some(NodeStatus::Waiting));
    answer(&replies.borrow()[2], 20);
    graph.run_all().unwrap();
    assert_eq!(graph.node_outputs(node).unwrap().get("asset"), Some(&22));
    assert_eq!(graph.node_run_count(node), Some(2));
}

#[test]
fn provisional_reads_survive_intern_reclamation_and_cancel_before_first_publication() {
    let replies = Rc::new(RefCell::new(Vec::new()));
    let mut graph = Graph::new(FnExecutor::new());
    graph.set_collect_task_events(true);
    let node = graph
        .add_node(
            deferred(Rc::clone(&replies), true),
            vec![],
            vec!["asset".into()],
        )
        .unwrap();
    graph.run_all().unwrap();
    assert_eq!(graph.storage_stats().resources, 2);
    assert_eq!(graph.node_dependencies(node).unwrap().count(), 0);
    assert_eq!(
        graph
            .task_dependencies(node)
            .unwrap()
            .cloned()
            .collect::<Vec<_>>(),
        [ResourceKey::host_state(ASSET, 0)]
    );
    graph.invalidate(ResourceKey::host_state(ASSET, 0));
    assert!(graph.task_info(node).is_none());
    assert_eq!(replies.borrow()[0].borrow().dropped, 1);
    assert!(graph.take_task_events().iter().any(|e| matches!(&e.kind, TaskEventKind::Cancelled(TaskCancelReason::Changed(key)) if *key == ResourceKey::host_state(ASSET, 0))));
    assert_eq!(graph.storage_stats().resources, 1);
    graph.run_all().unwrap();
    graph.remove_node(node).unwrap();
    assert_eq!(replies.borrow()[1].borrow().dropped, 1);
    assert_eq!(graph.storage_stats().resources, 0);
}

#[test]
fn unrelated_edits_preserve_owned_continuations_and_explicit_cancel_allows_fresh_demand() {
    let replies = Rc::new(RefCell::new(Vec::new()));
    let mut graph = Graph::new(FnExecutor::new());
    let node = graph
        .add_node(
            deferred(Rc::clone(&replies), true),
            vec![],
            vec!["asset".into()],
        )
        .unwrap();
    let unrelated = graph
        .add_node(
            FnNode::new(|args, out, _| {
                out.push(args[0]);
                Ok(())
            }),
            vec!["other".into()],
            vec!["out".into()],
        )
        .unwrap();
    graph.set_input_value(unrelated, "other", 1).unwrap();
    graph.run_all().unwrap();
    let id = graph.task_info(node).unwrap().id;
    graph.set_input_value(unrelated, "other", 2).unwrap();
    graph.invalidate_input("other");
    graph.run_node(unrelated).unwrap();
    assert_eq!(graph.task_info(node).unwrap().id, id);
    assert_eq!(
        graph.task_info(node).unwrap().polls,
        1,
        "scoped query does not poll unrelated work"
    );
    assert!(graph.cancel_task(node).unwrap());
    assert!(!graph.cancel_task(node).unwrap());
    graph.run_node(node).unwrap();
    assert_eq!(graph.task_info(node).unwrap().id.attempt, 2);
    assert_eq!(replies.borrow()[0].borrow().dropped, 1);
    drop(graph);
    assert_eq!(replies.borrow()[1].borrow().dropped, 1);
}

#[test]
fn embedding_waker_is_given_to_the_pending_task() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::task::Wake;
    struct Notify(AtomicUsize);
    impl Wake for Notify {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let notify = Arc::new(Notify(AtomicUsize::new(0)));
    let replies = Rc::new(RefCell::new(Vec::new()));
    let mut graph = Graph::new(FnExecutor::new());
    graph.set_task_waker(Waker::from(Arc::clone(&notify)));
    let node = graph
        .add_node(
            deferred(Rc::clone(&replies), false),
            vec![],
            vec!["asset".into()],
        )
        .unwrap();
    graph.run_node(node).unwrap();
    answer(&replies.borrow()[0], 3);
    assert_eq!(notify.0.load(Ordering::SeqCst), 1);
    assert_eq!(graph.node_status(node), Some(NodeStatus::Waiting));
    graph.run_node(node).unwrap();
    assert_eq!(graph.node_status(node), Some(NodeStatus::Current));
    assert!(graph.take_task_events().is_empty(), "observation is opt-in");
}

#[derive(Debug)]
struct ReadThenWait {
    producer: NodeId,
    request: Request,
    captured: bool,
}
impl NodeTask<i64, &'static str> for ReadThenWait {
    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        outputs: &mut Vec<i64>,
        access: &mut NodeAccess<'_, i64>,
    ) -> Poll<Result<(), &'static str>> {
        if !self.captured {
            match access.read_node_output(self.producer, OutputId::new(0)) {
                Ok(Some(value)) => {
                    self.request.input = value;
                    self.captured = true;
                }
                Ok(None) => return Poll::Pending,
                Err(_) => return Poll::Ready(Err("missing producer")),
            }
        }
        self.request.poll(cx, outputs, access)
    }
}

#[test]
fn dynamic_reads_can_wait_for_a_deferred_producer_without_spinning_or_restarting() {
    let replies = Rc::new(RefCell::new(Vec::new()));
    let producer_id = Rc::new(Cell::new(NodeId::new(0)));
    let reply = Rc::new(RefCell::new(Reply::default()));
    let mut graph = Graph::new(FnExecutor::new());
    let root = graph
        .add_node(
            FnNode::deferred({
                let producer_id = Rc::clone(&producer_id);
                let reply = Rc::clone(&reply);
                move |_, _| {
                    Ok(Box::new(ReadThenWait {
                        producer: producer_id.get(),
                        request: Request {
                            reply: Rc::clone(&reply),
                            input: 0,
                            polls: 0,
                            watch: false,
                        },
                        captured: false,
                    }))
                }
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    let producer = graph
        .add_node(
            deferred(Rc::clone(&replies), false),
            vec![],
            vec!["asset".into()],
        )
        .unwrap();
    producer_id.set(producer);
    let summary = graph.run_node(root).unwrap();
    assert_eq!(summary.waiting_nodes, 1);
    assert_eq!(summary.blocked_nodes, 1);
    assert_eq!(graph.task_info(root).unwrap().polls, 1);
    graph.run_node(root).unwrap();
    assert_eq!(graph.node_attempt_count(root), Some(1));
    assert_eq!(
        graph.task_info(root).unwrap().polls,
        1,
        "producer must be current before another poll"
    );
    answer(&replies.borrow()[0], 7);
    graph.run_node(root).unwrap();
    assert_eq!(graph.task_info(root).unwrap().polls, 2);
    answer(&reply, 10);
    graph.run_node(root).unwrap();
    assert_eq!(graph.node_outputs(root).unwrap().get("out"), Some(&17));
}

#[test]
fn provisional_upstream_reads_are_verified_before_poll_and_cutoff_preserves_the_task() {
    let producer_id = Rc::new(Cell::new(NodeId::new(0)));
    let reply = Rc::new(RefCell::new(Reply::default()));
    let mut graph: Graph = Graph::new(FnExecutor::new().with_value_eq(|a, b| a == b));
    let root = graph
        .add_node(
            FnNode::deferred({
                let producer_id = Rc::clone(&producer_id);
                let reply = Rc::clone(&reply);
                move |_, _| {
                    Ok(Box::new(ReadThenWait {
                        producer: producer_id.get(),
                        request: Request {
                            reply: Rc::clone(&reply),
                            input: 0,
                            polls: 0,
                            watch: false,
                        },
                        captured: false,
                    }))
                }
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    let producer = graph
        .add_node(
            FnNode::new(|args: &[i64], out, _| {
                out.push(args[0].min(1));
                Ok(())
            }),
            vec!["x".into()],
            vec!["value".into()],
        )
        .unwrap();
    producer_id.set(producer);
    graph.set_input_value(producer, "x", 2).unwrap();
    graph.run_node(root).unwrap();
    let id = graph.task_info(root).unwrap().id;
    graph.set_input_value(producer, "x", 3).unwrap();
    graph.invalidate_input("x");
    answer(&reply, 99);
    let summary = graph.run_node(root).unwrap();
    assert_eq!(summary.executed_nodes, 2);
    assert_eq!(graph.node_attempt_count(root), Some(id.attempt));
    assert_eq!(graph.node_outputs(root).unwrap().get("out"), Some(&100));
}

#[cfg(panic = "unwind")]
#[test]
fn a_panicking_task_is_never_polled_again_after_unwinding() {
    #[derive(Debug)]
    struct Panics(Rc<Cell<usize>>);
    impl NodeTask<i64, &'static str> for Panics {
        fn poll(
            &mut self,
            _: &mut Context<'_>,
            _: &mut Vec<i64>,
            _: &mut NodeAccess<'_, i64>,
        ) -> Poll<Result<(), &'static str>> {
            self.0.set(self.0.get() + 1);
            panic!("injected task panic after an external effect");
        }
    }
    let effects = Rc::new(Cell::new(0));
    let mut graph = Graph::new(FnExecutor::new());
    let node = graph
        .add_node(
            FnNode::deferred({
                let effects = Rc::clone(&effects);
                move |_, _| Ok(Box::new(Panics(Rc::clone(&effects))))
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| graph.run_all())).is_err());
    assert_eq!(effects.get(), 1);
    assert!(graph.task_info(node).unwrap().abandoned);
    assert!(matches!(graph.run_all(), Err(GraphError::TaskAbandoned { id }) if id.node == node));
    assert_eq!(effects.get(), 1);
    assert!(graph.task_info(node).is_none());
}

#[test]
fn pending_writes_remain_effective_after_cancellation_and_do_not_retrigger_the_writer() {
    #[derive(Debug)]
    struct Writes {
        state: Rc<Cell<i64>>,
        started: bool,
    }
    impl NodeTask<i64, &'static str> for Writes {
        fn poll(
            &mut self,
            _: &mut Context<'_>,
            _: &mut Vec<i64>,
            access: &mut NodeAccess<'_, i64>,
        ) -> Poll<Result<(), &'static str>> {
            if !self.started {
                self.started = true;
                access.read_host_state(ASSET, 0);
                self.state.set(self.state.get() + 1);
                access.write_host_state(ASSET, 0);
            }
            Poll::Pending
        }
    }
    let state = Rc::new(Cell::new(0));
    let mut graph = Graph::new(FnExecutor::new());
    let reader = graph
        .add_node(
            FnNode::new({
                let state = Rc::clone(&state);
                move |_, out, access| {
                    access.read_host_state(ASSET, 0);
                    out.push(state.get());
                    Ok(())
                }
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    graph.run_node(reader).unwrap();
    let writer = graph
        .add_node(
            FnNode::deferred({
                let state = Rc::clone(&state);
                move |_, _| {
                    Ok(Box::new(Writes {
                        state: Rc::clone(&state),
                        started: false,
                    }))
                }
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    graph.run_node(writer).unwrap();
    assert_eq!(graph.node_status(reader), Some(NodeStatus::Pending));
    graph.run_node(writer).unwrap();
    assert_eq!(state.get(), 1, "a later poll must not repeat the write");
    graph.cancel_task(writer).unwrap();
    graph.run_node(reader).unwrap();
    assert_eq!(graph.node_outputs(reader).unwrap().get("out"), Some(&1));
}

#[test]
fn cancellation_releases_tentative_payloads_but_published_values_can_outlive_the_graph() {
    #[derive(Debug)]
    struct Payload {
        ready: Rc<Cell<bool>>,
        value: Rc<Vec<u8>>,
    }
    impl NodeTask<Rc<Vec<u8>>, &'static str> for Payload {
        fn poll(
            &mut self,
            _: &mut Context<'_>,
            out: &mut Vec<Rc<Vec<u8>>>,
            _: &mut NodeAccess<'_, Rc<Vec<u8>>>,
        ) -> Poll<Result<(), &'static str>> {
            if out.is_empty() {
                out.push(Rc::clone(&self.value));
            }
            if self.ready.get() {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
    }
    let ready = Rc::new(Cell::new(false));
    let allocations = Rc::new(RefCell::new(Vec::new()));
    let mut graph = ExecutionGraph::new(FnExecutor::new());
    let node = graph
        .add_node(
            FnNode::deferred({
                let ready = Rc::clone(&ready);
                let allocations = Rc::clone(&allocations);
                move |_, _| {
                    let value = Rc::new(vec![7; 1024 * 1024]);
                    allocations.borrow_mut().push(Rc::downgrade(&value));
                    Ok(Box::new(Payload {
                        ready: Rc::clone(&ready),
                        value,
                    }))
                }
            }),
            vec![],
            vec!["payload".into()],
        )
        .unwrap();
    graph.run_all().unwrap();
    assert!(allocations.borrow()[0].upgrade().is_some());
    graph.cancel_task(node).unwrap();
    assert!(allocations.borrow()[0].upgrade().is_none());
    ready.set(true);
    graph.run_all().unwrap();
    let undo = Rc::clone(graph.node_outputs(node).unwrap().get("payload").unwrap());
    drop(graph.remove_node(node).unwrap());
    drop(graph);
    assert_eq!(undo.len(), 1024 * 1024);
    assert!(allocations.borrow()[1].upgrade().is_some());
    drop(undo);
    assert!(allocations.borrow()[1].upgrade().is_none());
}

#[test]
fn generated_pending_edit_histories_match_a_fresh_publication_model() {
    for seed in 1..=64_u64 {
        let mut random = seed;
        let replies = Rc::new(RefCell::new(Vec::new()));
        let mut graph = Graph::new(FnExecutor::new());
        let mut node = graph
            .add_node(
                deferred(Rc::clone(&replies), true),
                vec!["version".into()],
                vec!["out".into()],
            )
            .unwrap();
        let mut input = 0;
        graph.set_input_value(node, "version", input).unwrap();
        let mut pending = true;
        let mut active: Option<(usize, i64)> = None;
        let mut published = None;
        for step in 0..256 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            match (random >> 32) % 8 {
                0 | 1 => {
                    let before = replies.borrow().len();
                    if pending {
                        if let Some((index, captured)) = active {
                            if let Some(value) = replies.borrow()[index].borrow().value {
                                published = Some(captured + value);
                                active = None;
                                pending = false;
                            }
                        } else {
                            active = Some((before, input));
                        }
                    }
                    graph.run_all().unwrap();
                }
                2 => {
                    input = (random % 100) as i64;
                    graph.set_input_value(node, "version", input).unwrap();
                    graph.invalidate_input("version");
                    active = None;
                    pending = true;
                }
                3 => {
                    if !replies.borrow().is_empty() {
                        let count = u64::try_from(replies.borrow().len()).unwrap();
                        let index = usize::try_from(random % count).unwrap();
                        answer(&replies.borrow()[index], (random % 1000) as i64);
                    }
                }
                4 => {
                    graph.invalidate(ResourceKey::host_state(ASSET, 0));
                    if active.is_some() || published.is_some() {
                        pending = true;
                    }
                    active = None;
                }
                5 => {
                    graph.cancel_task(node).unwrap();
                    active = None;
                }
                6 => {
                    graph.invalidate(ResourceKey::InputId(999));
                }
                _ => {
                    let old = node;
                    drop(graph.remove_node(old).unwrap());
                    node = graph
                        .add_node(
                            deferred(Rc::clone(&replies), true),
                            vec!["version".into()],
                            vec!["out".into()],
                        )
                        .unwrap();
                    assert!(node.as_u64() > old.as_u64());
                    graph.set_input_value(node, "version", input).unwrap();
                    published = None;
                    active = None;
                    pending = true;
                }
            }
            assert_eq!(
                graph.node_outputs(node).unwrap().get("out").copied(),
                published,
                "seed={seed} step={step}"
            );
            assert_eq!(
                graph.task_info(node).is_some(),
                active.is_some(),
                "seed={seed} step={step}"
            );
            for (index, reply) in replies.borrow().iter().enumerate() {
                assert_eq!(
                    reply.borrow().dropped,
                    usize::from(active.is_none_or(|(current, _)| current != index)),
                    "seed={seed} step={step} request={index}"
                );
            }
        }
    }
}

#[test]
fn ready_tasks_cross_the_same_arity_failure_and_atomic_publication_boundary() {
    #[derive(Debug)]
    struct Finish {
        mode: u8,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for Finish {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }
    impl NodeTask<i64, &'static str> for Finish {
        fn poll(
            &mut self,
            _: &mut Context<'_>,
            out: &mut Vec<i64>,
            access: &mut NodeAccess<'_, i64>,
        ) -> Poll<Result<(), &'static str>> {
            access.read_host_state(ASSET, 3);
            if self.mode == 2 {
                return Poll::Ready(Err("worker failed"));
            }
            if self.mode == 0 {
                out.push(7);
            }
            Poll::Ready(Ok(()))
        }
    }
    let mode = Rc::new(Cell::new(0));
    let drops = Rc::new(Cell::new(0));
    let mut graph = Graph::new(FnExecutor::new());
    graph.set_collect_task_events(true);
    let node = graph
        .add_node(
            FnNode::deferred({
                let mode = Rc::clone(&mode);
                let drops = Rc::clone(&drops);
                move |_, _| {
                    Ok(Box::new(Finish {
                        mode: mode.get(),
                        drops: Rc::clone(&drops),
                    }))
                }
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    graph.run_all().unwrap();
    mode.set(1);
    graph.invalidate(ResourceKey::node_output(node, OutputId::new(0)));
    assert!(
        matches!(graph.run_all(), Err(GraphError::BadOutputArity { node: failed }) if failed == node)
    );
    assert_eq!(graph.node_outputs(node).unwrap().get("out"), Some(&7));
    assert_eq!(graph.node_run_count(node), Some(1));
    assert!(graph.task_info(node).is_none());
    mode.set(2);
    assert!(
        matches!(graph.run_all(), Err(GraphError::Node { node: failed, source: "worker failed" }) if failed == node)
    );
    assert_eq!(graph.node_outputs(node).unwrap().get("out"), Some(&7));
    assert_eq!(drops.get(), 3);
    let events = graph.take_task_events();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == TaskEventKind::Published)
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == TaskEventKind::Cancelled(TaskCancelReason::Failed))
            .count(),
        2
    );
}

#[test]
fn invalidation_during_dependency_resolution_does_not_restart_effects_in_the_same_query() {
    #[derive(Debug)]
    struct Ready(i64);
    impl NodeTask<i64, &'static str> for Ready {
        fn poll(
            &mut self,
            _: &mut Context<'_>,
            out: &mut Vec<i64>,
            _: &mut NodeAccess<'_, i64>,
        ) -> Poll<Result<(), &'static str>> {
            out.push(self.0);
            Poll::Ready(Ok(()))
        }
    }
    let source = Rc::new(Cell::new(0));
    let starts = Rc::new(Cell::new(0));
    let producer_id = Rc::new(Cell::new(NodeId::new(0)));
    let mut graph = Graph::new(FnExecutor::new());
    graph.set_collect_task_events(true);
    let reader = graph
        .add_node(
            FnNode::restartable({
                let source = Rc::clone(&source);
                let starts = Rc::clone(&starts);
                let producer_id = Rc::clone(&producer_id);
                move |_, _, access| {
                    let Some(value) = access
                        .read_node_output(producer_id.get(), OutputId::new(0))
                        .unwrap()
                    else {
                        return Ok(NodeOutcome::Pending);
                    };
                    starts.set(starts.get() + 1);
                    assert!(
                        starts.get() <= 2,
                        "query automatically repeated effects after supersession"
                    );
                    source.set(source.get() + 1);
                    access.write_host_state(ASSET, 0);
                    Ok(NodeOutcome::deferred(Ready(value)))
                }
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    let producer = graph
        .add_node(
            FnNode::new({
                let source = Rc::clone(&source);
                move |_, out, access| {
                    access.read_host_state(ASSET, 0);
                    out.push(source.get());
                    Ok(())
                }
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    producer_id.set(producer);
    let report = graph
        .run_node_with_report(reader, ReportDetailMask::FULL)
        .unwrap();
    assert_eq!(starts.get(), 1);
    assert_eq!(source.get(), 1, "performed effects remain real");
    assert_eq!(report.blocked, [reader]);
    assert!(graph.node_outputs(reader).unwrap().get("out").is_none());
    assert!(graph.task_info(reader).is_none());
    assert!(matches!(
        graph.take_task_events().last().unwrap().kind,
        TaskEventKind::Cancelled(TaskCancelReason::Changed(_))
    ));
}

#[test]
fn a_dynamic_cycle_retires_the_implicated_continuation_and_its_provisional_resources() {
    #[derive(Debug)]
    struct Cycle {
        target: Rc<Cell<NodeId>>,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for Cycle {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }
    impl NodeTask<i64, &'static str> for Cycle {
        fn poll(
            &mut self,
            _: &mut Context<'_>,
            _: &mut Vec<i64>,
            access: &mut NodeAccess<'_, i64>,
        ) -> Poll<Result<(), &'static str>> {
            access.read_host_state(ASSET, 0);
            assert!(
                access
                    .read_node_output(self.target.get(), OutputId::new(0))
                    .unwrap()
                    .is_none()
            );
            Poll::Pending
        }
    }
    let target = Rc::new(Cell::new(NodeId::new(0)));
    let drops = Rc::new(Cell::new(0));
    let mut graph = Graph::new(FnExecutor::new());
    let node = graph
        .add_node(
            FnNode::deferred({
                let target = Rc::clone(&target);
                let drops = Rc::clone(&drops);
                move |_, _| {
                    Ok(Box::new(Cycle {
                        target: Rc::clone(&target),
                        drops: Rc::clone(&drops),
                    }))
                }
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    target.set(node);
    assert!(matches!(
        graph.run_node(node),
        Err(GraphError::DynamicDependencyCycle { .. })
    ));
    assert_eq!(drops.get(), 1);
    assert!(graph.task_info(node).is_none());
    assert_eq!(graph.storage_stats().resources, 1);
}
