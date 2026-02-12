// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

use execution_graph::{ExecutionGraph, HostOpId, ResourceKey};
use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{
    AccessSink, Host, HostError, HostSig, ResourceKeyRef, SigHash, ValueRef, sig_hash,
};
use execution_tape::program::ValueType;
use execution_tape::value::{FuncId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::Limits;
use std::cell::RefCell;
use std::rc::Rc;

/// Entry point for `execution_graph` wind-tunnel benchmarks.
///
/// This function registers a collection of scenarios that are meant to highlight how
/// invalidations propagate through different graph shapes (chains, fanout, shared upstreams,
/// layered DAG “cones”).
fn bench_graph(c: &mut Criterion) {
    bench_single_node_rerun(c);
    bench_chain_rerun(c);
    bench_chain_noop(c);
    bench_stable_deps_many_reads(c);
    bench_stable_deps_host_order_flap(c);
    bench_fanout_rerun(c);
    bench_disjoint_chains_host_key(c);
    bench_shared_upstream_one_tenant(c);
    bench_shared_upstream_shared_key(c);
    bench_layered_dag_cone(c);
}

#[derive(Debug, Default)]
struct NopHost;

impl Host for NopHost {
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

fn build_identity_program(output_name: &str) -> (VerifiedProgram, FuncId) {
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
        .unwrap();
    pb.set_function_output_name(f, 0, output_name).unwrap();
    (pb.build_verified().unwrap(), f)
}

fn build_chain_graph(len: usize) -> (ExecutionGraph<NopHost>, execution_graph::NodeId) {
    let (prog, entry) = build_identity_program("value");
    let mut g = ExecutionGraph::new(NopHost, Limits::default());

    let n0 = g.add_node(prog.clone(), entry, vec!["in".into()]);
    g.set_input_value(n0, "in", Value::I64(1));

    let mut prev = n0;
    for _ in 1..len {
        let n = g.add_node(prog.clone(), entry, vec!["x".into()]);
        g.connect(prev, "value", n, "x");
        prev = n;
    }

    g.run_all().unwrap();
    (g, n0)
}

fn build_wide_input_program(input_count: usize, output_name: &str) -> (VerifiedProgram, FuncId) {
    // fn wide(in0, in1, ..., inN) -> i64 { in0 }
    let mut pb = ProgramBuilder::new();
    let mut a = Asm::new();
    a.ret(0, &[1]);

    let f = pb
        .push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![ValueType::I64; input_count],
                ret_types: vec![ValueType::I64],
                reg_count: u32::try_from(input_count.saturating_add(1)).unwrap_or(u32::MAX),
            },
        )
        .unwrap();
    pb.set_function_output_name(f, 0, output_name).unwrap();
    (pb.build_verified().unwrap(), f)
}

fn build_stable_deps_graph(input_count: usize) -> ExecutionGraph<NopHost> {
    let (prog, entry) = build_wide_input_program(input_count, "value");
    let mut g = ExecutionGraph::new(NopHost, Limits::default());

    let input_names: Vec<Box<str>> = (0..input_count)
        .map(|i| format!("in{i}").into_boxed_str())
        .collect();
    let node = g.add_node(prog, entry, input_names.clone());

    for (i, name) in input_names.iter().enumerate() {
        let value = i64::try_from(i).unwrap_or(i64::MAX);
        g.set_input_value(node, name.as_ref(), Value::I64(value));
    }

    g.run_all().unwrap();
    g
}

/// Single identity node that is invalidated and rerun each iteration.
///
/// Graph shape:
/// ```text
/// input("in")
///    |
///   [n0] -> value
/// ```
///
/// Measures the tightest per-execution overhead: 1 `invalidate_input` + 1 `plan_all` +
/// 1 `run_node_internal`.
fn bench_single_node_rerun(c: &mut Criterion) {
    let (prog, entry) = build_identity_program("value");
    let mut g = ExecutionGraph::new(NopHost, Limits::default());
    let n0 = g.add_node(prog, entry, vec!["in".into()]);
    g.set_input_value(n0, "in", Value::I64(0));
    g.run_all().unwrap();

    c.bench_function("single_node_rerun", |b| {
        let mut v = 0_i64;
        b.iter(|| {
            v = v.wrapping_add(1);
            g.set_input_value(n0, "in", Value::I64(black_box(v)));
            g.invalidate_input("in");
            g.run_all().unwrap();
        });
    });
}

/// Linear chain of `len` nodes where every node depends on the previous node's output.
///
/// Graph shape (`len = 5` example):
/// ```text
/// input("in")
///    |
///   [n0] -> [n1] -> [n2] -> [n3] -> [n4]
/// ```
///
/// Measures the cost of a single root input invalidation that forces the entire chain to rerun.
fn bench_chain_rerun(c: &mut Criterion) {
    let mut group = c.benchmark_group("chain_rerun");
    for &len in &[10_usize, 100, 1_000] {
        let (mut g, n0) = build_chain_graph(len);
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |b, _| {
            let mut v = 0_i64;
            b.iter(|| {
                v = v.wrapping_add(1);
                g.set_input_value(n0, "in", Value::I64(black_box(v)));
                g.invalidate_input("in");
                g.run_all().unwrap();
            });
        });
    }
    group.finish();
}

/// Steady-state overhead of calling `run_all()` when nothing is dirty.
///
/// Graph shape (same chain used by `bench_chain_rerun`):
/// ```text
/// input("in")
///    |
///   [n0] -> [n1] -> ... -> [n(len-1)]
/// ```
///
/// This should be near-constant (does not scale with graph size) and acts as a “baseline tax”.
fn bench_chain_noop(c: &mut Criterion) {
    let mut group = c.benchmark_group("chain_noop_run_all");
    for &len in &[10_usize, 100, 1_000] {
        let (mut g, _n0) = build_chain_graph(len);
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |b, _| {
            b.iter(|| {
                g.run_all().unwrap();
            });
        });
    }
    group.finish();
}

/// Single node with many external input reads; invalidation key set stays stable each iteration.
///
/// Graph shape (`input_count = 5` example):
/// ```text
/// in0 --->\
/// in1 ---->\
/// in2 -----> [wide] -> value
/// in3 ---->/
/// in4 --->/
/// ```
///
/// Benchmark loop:
/// - invalidate `in0`
/// - run all dirty work
///
/// This isolates steady-state rerun cost where dependency keys are unchanged between runs.
fn bench_stable_deps_many_reads(c: &mut Criterion) {
    let mut group = c.benchmark_group("stable_deps_many_reads_rerun");
    for &input_count in &[8_usize, 32, 128, 512, 1024, 2048, 4096] {
        let mut g = build_stable_deps_graph(input_count);
        group.bench_with_input(
            BenchmarkId::from_parameter(input_count),
            &input_count,
            |b, _| {
                b.iter(|| {
                    g.invalidate_input(black_box("in0"));
                    g.run_all().unwrap();
                });
            },
        );
    }
    group.finish();
}

#[derive(Debug, Clone)]
struct FlappingOrderHost {
    read_count: usize,
    flip: Rc<RefCell<bool>>,
}

impl Host for FlappingOrderHost {
    fn call(
        &mut self,
        symbol: &str,
        sig_hash: SigHash,
        _args: &[ValueRef<'_>],
        mut access: Option<&mut dyn AccessSink>,
    ) -> Result<(Vec<Value>, u64), HostError> {
        if symbol != "flap_reads_i64" {
            return Err(HostError::UnknownSymbol);
        }

        let mut flip = self.flip.borrow_mut();
        let reverse = *flip;
        *flip = !*flip;

        if reverse {
            for i in (0..self.read_count).rev() {
                let key = u64::try_from(i).map_err(|_| HostError::Failed)?;
                if let Some(sink) = access.as_mut() {
                    sink.read(ResourceKeyRef::HostState { op: sig_hash, key });
                }
            }
        } else {
            for i in 0..self.read_count {
                let key = u64::try_from(i).map_err(|_| HostError::Failed)?;
                if let Some(sink) = access.as_mut() {
                    sink.read(ResourceKeyRef::HostState { op: sig_hash, key });
                }
            }
        }

        Ok((vec![Value::I64(0)], 0))
    }
}

fn build_flap_reads_program() -> (VerifiedProgram, FuncId, HostOpId) {
    let host_sig = HostSig {
        args: vec![],
        rets: vec![ValueType::I64],
    };
    let op = HostOpId::new(sig_hash(&host_sig).0);

    let mut pb = ProgramBuilder::new();
    let host_sig_id = pb.host_sig_for("flap_reads_i64", host_sig);

    let mut a = Asm::new();
    a.host_call(0, host_sig_id, 0, &[], &[1]);
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
        .unwrap();
    pb.set_function_output_name(f, 0, "value").unwrap();

    (pb.build_verified().unwrap(), f, op)
}

fn build_stable_deps_flapping_order_graph(
    read_count: usize,
) -> (ExecutionGraph<FlappingOrderHost>, HostOpId) {
    let (prog, entry, op) = build_flap_reads_program();
    let host = FlappingOrderHost {
        read_count,
        flip: Rc::new(RefCell::new(false)),
    };
    let mut g = ExecutionGraph::new(host, Limits::default());
    g.add_node(prog, entry, vec![]);
    g.run_all().unwrap();
    (g, op)
}

/// Single node with host-read dependency set stable, but read emission order alternates each run.
///
/// This isolates order-normalization overhead and checks that stable sets do not thrash dependency
/// replacement when host read order is nondeterministic.
fn bench_stable_deps_host_order_flap(c: &mut Criterion) {
    let mut group = c.benchmark_group("stable_deps_host_order_flap_rerun");
    for &read_count in &[32_usize, 128, 512, 1024] {
        let (mut g, op) = build_stable_deps_flapping_order_graph(read_count);
        group.bench_with_input(
            BenchmarkId::from_parameter(read_count),
            &read_count,
            |b, _| {
                b.iter(|| {
                    g.invalidate(ResourceKey::host_state(op, black_box(0)));
                    g.run_all().unwrap();
                });
            },
        );
    }
    group.finish();
}

fn build_fanout_graph(fanout: usize) -> (ExecutionGraph<NopHost>, execution_graph::NodeId) {
    let (prog, entry) = build_identity_program("value");
    let mut g = ExecutionGraph::new(NopHost, Limits::default());

    let root = g.add_node(prog.clone(), entry, vec!["in".into()]);
    g.set_input_value(root, "in", Value::I64(1));

    for _ in 0..fanout {
        let leaf = g.add_node(prog.clone(), entry, vec!["x".into()]);
        g.connect(root, "value", leaf, "x");
    }

    g.run_all().unwrap();
    (g, root)
}

/// Star/fanout graph where a single root feeds `fanout` independent leaves.
///
/// Graph shape (`fanout = 5` example):
/// ```text
///                +--> [leaf0]
///                +--> [leaf1]
/// input("in")    +--> [leaf2]
///    |           +--> [leaf3]
///  [root] ------ +--> [leaf4]
/// ```
///
/// Measures the cost of a root input invalidation that reruns all leaves.
fn bench_fanout_rerun(c: &mut Criterion) {
    let mut group = c.benchmark_group("fanout_rerun");
    for &fanout in &[10_usize, 100, 1_000] {
        let (mut g, root) = build_fanout_graph(fanout);
        group.bench_with_input(BenchmarkId::from_parameter(fanout), &fanout, |b, _| {
            let mut v = 0_i64;
            b.iter(|| {
                v = v.wrapping_add(1);
                g.set_input_value(root, "in", Value::I64(black_box(v)));
                g.invalidate_input("in");
                g.run_all().unwrap();
            });
        });
    }
    group.finish();
}

#[derive(Debug, Clone)]
struct ParamHost {
    params: Rc<RefCell<Vec<i64>>>,
}

impl Host for ParamHost {
    fn call(
        &mut self,
        symbol: &str,
        sig_hash: SigHash,
        args: &[ValueRef<'_>],
        mut access: Option<&mut dyn AccessSink>,
    ) -> Result<(Vec<Value>, u64), HostError> {
        if symbol != "param_i64" {
            return Err(HostError::UnknownSymbol);
        }
        let [ValueRef::U64(k)] = args else {
            return Err(HostError::SignatureMismatch);
        };
        let k = *k;
        if let Some(sink) = access.as_mut() {
            sink.read(ResourceKeyRef::HostState {
                op: sig_hash,
                key: k,
            });
        }

        let i = usize::try_from(k).map_err(|_| HostError::Failed)?;
        let v = self
            .params
            .borrow()
            .get(i)
            .copied()
            .ok_or(HostError::Failed)?;
        Ok((vec![Value::I64(v)], 0))
    }
}

fn build_param_program() -> (VerifiedProgram, FuncId, HostOpId) {
    let host_sig = HostSig {
        args: vec![ValueType::U64],
        rets: vec![ValueType::I64],
    };
    let op = HostOpId::new(sig_hash(&host_sig).0);

    let mut pb = ProgramBuilder::new();
    let host_sig_id = pb.host_sig_for("param_i64", host_sig);

    let mut a = Asm::new();
    a.host_call(0, host_sig_id, 0, &[1], &[2]);
    a.ret(0, &[2]);

    let f = pb
        .push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![ValueType::U64],
                ret_types: vec![ValueType::I64],
                reg_count: 3,
            },
        )
        .unwrap();
    pb.set_function_output_name(f, 0, "value").unwrap();

    (pb.build_verified().unwrap(), f, op)
}

fn build_disjoint_chains(
    chains: usize,
    chain_len: usize,
) -> (ExecutionGraph<ParamHost>, Rc<RefCell<Vec<i64>>>, HostOpId) {
    let params = Rc::new(RefCell::new(vec![0_i64; chains]));
    let host = ParamHost {
        params: params.clone(),
    };
    let mut g = ExecutionGraph::new(host, Limits::default());

    let (param_prog, param_entry, op) = build_param_program();
    let (id_prog, id_entry) = build_identity_program("value");

    for i in 0..chains {
        let root = g.add_node(param_prog.clone(), param_entry, vec!["key".into()]);
        g.set_input_value(
            root,
            "key",
            Value::U64(u64::try_from(i).unwrap_or(u64::MAX)),
        );

        let mut prev = root;
        for _ in 1..chain_len {
            let n = g.add_node(id_prog.clone(), id_entry, vec!["x".into()]);
            g.connect(prev, "value", n, "x");
            prev = n;
        }
    }

    g.run_all().unwrap();
    (g, params, op)
}

#[inline]
fn invalidate_host_state<H: Host>(g: &mut ExecutionGraph<H>, op: HostOpId, key: u64) {
    g.invalidate(ResourceKey::host_state(op, key));
}

/// Many disjoint chains (no shared upstreams), where each chain’s root reads a distinct host key.
///
/// Graph shape (`chains = 3`, `chain_len = 4` example):
/// ```text
/// key=0 -> [c0_0] -> [c0_1] -> [c0_2] -> [c0_3]
/// key=1 -> [c1_0] -> [c1_1] -> [c1_2] -> [c1_3]
/// key=2 -> [c2_0] -> [c2_1] -> [c2_2] -> [c2_3]
///
/// (no edges between chains)
/// ```
///
/// Measures the cost of invalidating exactly one host-state key and rerunning only the affected
/// chain, even as the total node count grows.
fn bench_disjoint_chains_host_key(c: &mut Criterion) {
    let mut group = c.benchmark_group("disjoint_chains_invalidate_one_host_key");
    // Keep the total node count roughly comparable across sizes.
    let chain_len = 32;
    for &chains in &[10_usize, 100, 1_000] {
        let (mut g, params, op) = build_disjoint_chains(chains, chain_len);
        group.bench_with_input(
            BenchmarkId::from_parameter(chains),
            &chains,
            |b, &chains| {
                let mut tick = 0_i64;
                let mut idx = 0_usize;
                b.iter(|| {
                    tick = tick.wrapping_add(1);
                    idx = (idx + 1) % chains;

                    params.borrow_mut()[idx] = black_box(tick);
                    invalidate_host_state(&mut g, op, u64::try_from(idx).unwrap_or(u64::MAX));
                    g.run_all().unwrap();
                });
            },
        );
    }
    group.finish();
}

fn build_add2_program(output_name: &str) -> (VerifiedProgram, FuncId) {
    // fn add2(a: i64, b: i64) -> i64 { a + b }
    let mut pb = ProgramBuilder::new();
    let mut a = Asm::new();
    a.i64_add(3, 1, 2);
    a.ret(0, &[3]);

    let f = pb
        .push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![ValueType::I64, ValueType::I64],
                ret_types: vec![ValueType::I64],
                reg_count: 4,
            },
        )
        .unwrap();
    pb.set_function_output_name(f, 0, output_name).unwrap();
    (pb.build_verified().unwrap(), f)
}

fn build_shared_upstream(
    tenants: usize,
    chain_len: usize,
) -> (ExecutionGraph<ParamHost>, Rc<RefCell<Vec<i64>>>, HostOpId) {
    // One shared "global config" value at key=0, plus per-tenant value at key=(i+1).
    // Each tenant computes: base = add2(global, tenant), then a pass-through chain.
    let params = Rc::new(RefCell::new(vec![0_i64; tenants + 1]));
    let host = ParamHost {
        params: params.clone(),
    };
    let mut g = ExecutionGraph::new(host, Limits::default());

    let (param_prog, param_entry, op) = build_param_program();
    let (add_prog, add_entry) = build_add2_program("value");
    let (id_prog, id_entry) = build_identity_program("value");

    let global = g.add_node(param_prog.clone(), param_entry, vec!["key".into()]);
    g.set_input_value(global, "key", Value::U64(0));

    for i in 0..tenants {
        let per = g.add_node(param_prog.clone(), param_entry, vec!["key".into()]);
        g.set_input_value(
            per,
            "key",
            Value::U64(u64::try_from(i + 1).unwrap_or(u64::MAX)),
        );

        let base = g.add_node(add_prog.clone(), add_entry, vec!["a".into(), "b".into()]);
        g.connect(global, "value", base, "a");
        g.connect(per, "value", base, "b");

        let mut prev = base;
        for _ in 1..chain_len {
            let n = g.add_node(id_prog.clone(), id_entry, vec!["x".into()]);
            g.connect(prev, "value", n, "x");
            prev = n;
        }
    }

    g.run_all().unwrap();
    (g, params, op)
}

/// Many “tenants” share one global upstream value, but each tenant also has its own key.
///
/// Graph shape (`tenants = 3`, `chain_len = 4` example):
/// ```text
///                     [global key=0]
///                       /     |     \
///                      v      v      v
/// [tenant key=1] -> [base0] -> [t0_1] -> [t0_2] -> [t0_3]
/// [tenant key=2] -> [base1] -> [t1_1] -> [t1_2] -> [t1_3]
/// [tenant key=3] -> [base2] -> [t2_1] -> [t2_2] -> [t2_3]
///
/// where each `[baseN]` is `add2(global, tenantN)`.
/// ```
///
/// Measures invalidation of a single tenant’s key. This should remain close to constant as
/// tenant count grows (only one tenant’s subgraph should rerun).
fn bench_shared_upstream_one_tenant(c: &mut Criterion) {
    let mut group = c.benchmark_group("shared_upstream_invalidate_one_tenant");
    let chain_len = 16;
    for &tenants in &[10_usize, 100, 1_000] {
        let (mut g, params, op) = build_shared_upstream(tenants, chain_len);
        group.bench_with_input(
            BenchmarkId::from_parameter(tenants),
            &tenants,
            |b, &tenants| {
                let mut tick = 0_i64;
                let mut idx = 0_usize;
                b.iter(|| {
                    tick = tick.wrapping_add(1);
                    idx = (idx + 1) % tenants;

                    // Per-tenant keys start at 1.
                    params.borrow_mut()[idx + 1] = black_box(tick);
                    invalidate_host_state(&mut g, op, u64::try_from(idx + 1).unwrap_or(u64::MAX));
                    g.run_all().unwrap();
                });
            },
        );
    }
    group.finish();
}

/// Same graph shape as `shared_upstream_invalidate_one_tenant`, but invalidates the shared key.
///
/// Graph shape (`tenants = 3`, `chain_len = 4` example):
/// ```text
///                     [global key=0]
///                       /     |     \
///                      v      v      v
/// [tenant key=1] -> [base0] -> [t0_1] -> [t0_2] -> [t0_3]
/// [tenant key=2] -> [base1] -> [t1_1] -> [t1_2] -> [t1_3]
/// [tenant key=3] -> [base2] -> [t2_1] -> [t2_2] -> [t2_3]
///
/// where each `[baseN]` is `add2(global, tenantN)`.
/// ```
///
/// Measures the “blast radius” when a global configuration changes: all tenants’ subgraphs should
/// rerun, so this should scale roughly linearly with tenant count.
fn bench_shared_upstream_shared_key(c: &mut Criterion) {
    let mut group = c.benchmark_group("shared_upstream_invalidate_shared");
    let chain_len = 16;
    for &tenants in &[10_usize, 100, 1_000] {
        let (mut g, params, op) = build_shared_upstream(tenants, chain_len);
        group.bench_with_input(BenchmarkId::from_parameter(tenants), &tenants, |b, _| {
            let mut tick = 0_i64;
            b.iter(|| {
                tick = tick.wrapping_add(1);
                params.borrow_mut()[0] = black_box(tick);
                invalidate_host_state(&mut g, op, 0);
                g.run_all().unwrap();
            });
        });
    }
    group.finish();
}

fn build_layered_dag(
    width: usize,
    layers: usize,
) -> (ExecutionGraph<ParamHost>, Rc<RefCell<Vec<i64>>>, HostOpId) {
    // Root layer: width nodes each reads param_i64(key=i).
    // Each subsequent layer node i depends on (i) and (i+1 mod width) from previous layer via add2.
    let params = Rc::new(RefCell::new(vec![0_i64; width]));
    let host = ParamHost {
        params: params.clone(),
    };
    let mut g = ExecutionGraph::new(host, Limits::default());

    let (param_prog, param_entry, op) = build_param_program();
    let (add_prog, add_entry) = build_add2_program("value");

    let mut prev: Vec<execution_graph::NodeId> = Vec::with_capacity(width);
    for i in 0..width {
        let n = g.add_node(param_prog.clone(), param_entry, vec!["key".into()]);
        g.set_input_value(n, "key", Value::U64(u64::try_from(i).unwrap_or(u64::MAX)));
        prev.push(n);
    }

    for _ in 1..layers {
        let mut next: Vec<execution_graph::NodeId> = Vec::with_capacity(width);
        for i in 0..width {
            let n = g.add_node(add_prog.clone(), add_entry, vec!["a".into(), "b".into()]);
            let a0 = prev[i];
            let b0 = prev[(i + 1) % width];
            g.connect(a0, "value", n, "a");
            g.connect(b0, "value", n, "b");
            next.push(n);
        }
        prev = next;
    }

    g.run_all().unwrap();
    (g, params, op)
}

/// Layered DAG where each node depends on two upstream neighbors (“2-input stencil”).
///
/// Graph shape (`width = 5`, `layers = 4` example):
/// ```text
/// L0: [r0] [r1] [r2] [r3] [r4]
///      |\   |\   |\   |\   |\
///      | \  | \  | \  | \  | \
/// L1: [a0] [a1] [a2] [a3] [a4]
///      |\   |\   |\   |\   |\
///      | \  | \  | \  | \  | \
/// L2: [b0] [b1] [b2] [b3] [b4]
///      |\   |\   |\   |\   |\
///      | \  | \  | \  | \  | \
/// L3: [c0] [c1] [c2] [c3] [c4]
///
/// Dependency rule:
/// - `L{k+1}[i]` depends on `L{k}[i]` and `L{k}[(i + 1) % width]`.
/// ```
///
/// Measures the widening “cone” of recomputation from invalidating a single root input in the
/// first layer, across different widths/layer counts.
fn bench_layered_dag_cone(c: &mut Criterion) {
    let mut group = c.benchmark_group("layered_dag_cone_invalidate_one_root");
    for &(width, layers) in &[(64_usize, 8_usize), (256, 8), (256, 16)] {
        let (mut g, params, op) = build_layered_dag(width, layers);
        group.bench_with_input(
            BenchmarkId::new("w_l", format!("{width}x{layers}")),
            &(width, layers),
            |b, &(width, _layers)| {
                let mut tick = 0_i64;
                let mut idx = 0_usize;
                b.iter(|| {
                    tick = tick.wrapping_add(1);
                    idx = (idx + 1) % width;
                    params.borrow_mut()[idx] = black_box(tick);
                    invalidate_host_state(&mut g, op, u64::try_from(idx).unwrap_or(u64::MAX));
                    g.run_all().unwrap();
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_graph);
criterion_main!(benches);
