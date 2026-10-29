// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use crate::{ExecutionGraph, TapeExecutor};
use execution_tape::asm::{Asm, FunctionAbi, FunctionSig, ProgramBuilder};
use execution_tape::contract::Contract;
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::{StructTypeDef, ValueType};
use execution_tape::value::{Closure, FuncId};
use execution_tape::vm::{InvocationOptions, Limits, RunStatus, Vm};

#[test]
fn exported_graph_closure_survives_output_removal_and_executor_destruction() {
    let mut builder = ProgramBuilder::new();
    let integer = builder.contract(Contract::Kind(ValueType::I64)).unwrap();
    let environment = builder.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let body = builder
        .declare_function(FunctionAbi {
            args: vec![environment.into(), integer.into()],
            rets: vec![integer.into()],
        })
        .unwrap();
    let mut a = builder.function_asm(body).unwrap();
    let captured = a.alloc_register(ValueType::I64);
    let answer = a.alloc_register(ValueType::I64);
    a.tuple_get(captured, 1, 0)
        .i64_add(answer, captured, 2)
        .ret(0, &[answer]);
    builder.define_function(body, a).unwrap();
    let maker = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::Closure],
        })
        .unwrap();
    let mut a = builder.function_asm(maker).unwrap();
    let env = a.alloc_register(ValueType::Agg);
    let target = a.alloc_register(ValueType::Func);
    let result = a.alloc_register(ValueType::Closure);
    a.tuple_new(env, &[1])
        .const_func(target, body)
        .closure_new(result, target, env)
        .ret(0, &[result]);
    builder.define_function(maker, a).unwrap();
    let program = Arc::new(builder.build_verified().unwrap());
    let weak = Arc::downgrade(&program);
    let mut graph = ExecutionGraph::new(TapeExecutor::new(HostProbe::default(), Limits::default()));
    let node = graph
        .add_tape_node(program, maker, vec!["x".into()])
        .unwrap();
    graph
        .set_input_value(node, "x", Value::I64(40).into())
        .unwrap();
    graph.run_node(node).unwrap();
    let output = graph.node_outputs(node).unwrap().get("ret0").unwrap();
    let Repr::Shared { bundle, .. } = &output.repr else {
        panic!("owned output")
    };
    let weak_bundle = Arc::downgrade(bundle);
    let callable = output.retain_callable().unwrap();
    drop(graph.remove_node(node).unwrap());
    drop(graph);
    assert!(
        weak_bundle.upgrade().is_none(),
        "export does not retain unrelated publication roots"
    );
    assert!(weak.upgrade().is_some(), "export owns its code");
    let mut vm = Vm::new(HostProbe::default(), Limits::default());
    let mut call = callable
        .start(&mut vm, vec![Value::I64(2)], InvocationOptions::default())
        .unwrap();
    drop(callable);
    while call.resume(1).unwrap() == RunStatus::Suspended {}
    assert_eq!(call.take_outputs(), Some(vec![Value::I64(42)]));
    drop(call);
    assert!(weak.upgrade().is_none());
    assert_eq!(vm.aggregates().len_u32(), 0);
    assert_eq!(vm.host().calls, 0);
}

#[test]
fn exported_function_uses_original_program_and_raw_values_explain_missing_ownership() {
    let (program, entries) = program(11);
    let mut graph = ExecutionGraph::new(TapeExecutor::new(HostProbe::default(), Limits::default()));
    let node = graph
        .add_tape_node(program, entries.function, vec![])
        .unwrap();
    graph.run_node(node).unwrap();
    let callable = graph
        .node_outputs(node)
        .unwrap()
        .get("ret0")
        .unwrap()
        .retain_callable()
        .unwrap();
    drop(graph);
    let mut vm = Vm::new(HostProbe::default(), Limits::default());
    let mut call = callable
        .start(&mut vm, vec![], InvocationOptions::default())
        .unwrap();
    call.resume(u64::MAX).unwrap();
    assert_eq!(call.take_outputs(), Some(vec![Value::I64(11)]));
    assert_eq!(
        TapeValue::from(Value::I64(42))
            .retain_callable()
            .unwrap_err(),
        CallableError::NotCallable {
            actual: ValueType::I64
        }
    );
    for value in [
        Value::Func(entries.value),
        Value::Closure(Closure {
            func: entries.value,
            env: AggHandle(0),
        }),
    ] {
        assert_eq!(
            TapeValue::from(value).retain_callable().unwrap_err(),
            CallableError::MissingCodeOwner
        );
    }
}

#[derive(Default)]
struct HostProbe {
    calls: usize,
    heap_nodes: Vec<u32>,
}
impl Host for HostProbe {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        outputs: &mut [Value],
        cx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        self.calls += 1;
        self.heap_nodes.push(cx.aggregates().len_u32());
        let [ValueRef::Agg(first), ValueRef::Agg(wrapper)] = args else {
            panic!("aggregate arguments")
        };
        outputs[0] = Value::Bool(cx.tuple_get(*wrapper, 0).unwrap() == Value::Agg(*first));
        Ok(0)
    }
}

struct Entries {
    value: FuncId,
    function: FuncId,
    invoke: FuncId,
    tuple: FuncId,
    wrap: FuncId,
    identity: FuncId,
    same: FuncId,
    discard_wrap: FuncId,
}

fn program(number: i64) -> (Arc<VerifiedProgram>, Entries) {
    let mut pb = ProgramBuilder::new();
    pb.struct_type(StructTypeDef {
        field_names: vec!["value".into()],
        field_types: vec![ValueType::I64],
    });
    pb.array_elem(ValueType::I64);
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::I64]);
    body.const_i64(1, number).ret(0, &[1]);
    let value = pb
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::Func]);
    body.const_func(1, value).ret(0, &[1]);
    let function = pb
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::Func],
            },
        )
        .unwrap();
    let sig = pb
        .call_sig(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::Func, ValueType::I64]);
    body.call_indirect(0, sig, 1, 0, &[], &[2]).ret(0, &[2]);
    let invoke = pb
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![ValueType::Func],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::Agg]);
    body.tuple_new(2, &[1]).ret(0, &[2]);
    let tuple = pb
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::Agg],
            },
        )
        .unwrap();
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::Agg, ValueType::Agg]);
    body.tuple_new(2, &[1]).ret(0, &[2]);
    let wrap = pb
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![ValueType::Agg],
                ret_types: vec![ValueType::Agg],
            },
        )
        .unwrap();
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::Agg]);
    body.ret(0, &[1]);
    let identity = pb
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![ValueType::Agg],
                ret_types: vec![ValueType::Agg],
            },
        )
        .unwrap();
    let host = pb.host_sig_for(
        "same_child",
        HostSig {
            args: vec![ValueType::Agg, ValueType::Agg],
            rets: vec![ValueType::Bool],
        },
    );
    let mut body = Asm::new();
    body.set_register_types(vec![
        ValueType::Unit,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::Bool,
    ]);
    body.host_call_sig_id(0, host, 0, &[1, 2], &[3])
        .ret(0, &[3]);
    let same = pb
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![ValueType::Agg, ValueType::Agg],
                ret_types: vec![ValueType::Bool],
            },
        )
        .unwrap();
    let mut body = Asm::new();
    body.set_register_types(vec![
        ValueType::Unit,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::Agg,
    ]);
    body.tuple_new(3, &[2]).ret(0, &[3]);
    let discard_wrap = pb
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![ValueType::Agg, ValueType::Agg],
                ret_types: vec![ValueType::Agg],
            },
        )
        .unwrap();
    (
        Arc::new(pb.build_verified().unwrap()),
        Entries {
            value,
            function,
            invoke,
            tuple,
            wrap,
            identity,
            same,
            discard_wrap,
        },
    )
}

#[test]
fn callable_program_domain_is_enforced_before_execution() {
    let (a, entries) = program(11);
    let (b, other) = program(99);
    let mut graph = ExecutionGraph::new(TapeExecutor::new(HostProbe::default(), Limits::default()));
    let producer = graph
        .add_tape_node(Arc::clone(&a), entries.function, vec![])
        .unwrap();
    let same = graph
        .add_tape_node(a, entries.invoke, vec!["f".into()])
        .unwrap();
    let foreign = graph
        .add_tape_node(b, other.invoke, vec!["f".into()])
        .unwrap();
    graph.connect(producer, "ret0", same, "f").unwrap();
    graph.connect(producer, "ret0", foreign, "f").unwrap();
    graph.run_node(same).unwrap();
    assert_eq!(
        graph
            .node_outputs(same)
            .unwrap()
            .get("ret0")
            .unwrap()
            .value(),
        &Value::I64(11)
    );
    assert!(matches!(
        graph.run_node(foreign),
        Err(crate::GraphError::Node {
            source: TapeError::ProgramMismatch { input: 0 },
            ..
        })
    ));
    graph
        .set_input_value(same, "f", Value::Func(entries.value).into())
        .unwrap();
    graph.invalidate_input("f");
    assert!(matches!(
        graph.run_node(same),
        Err(crate::GraphError::Node {
            source: TapeError::UnownedInput { input: 0 },
            ..
        })
    ));
    assert_eq!(graph.executor().host().calls, 0);
}

#[test]
fn nested_scoped_ids_require_program_and_reject_foreign_owners() {
    let (a, _) = program(11);
    let (b, entries) = program(99);
    for kind in 0..4 {
        let mut heap = AggHeap::new();
        let leaf = match kind {
            0 => Value::Func(FuncId(0)),
            1 => {
                let env = heap.tuple_new(vec![]).unwrap();
                Value::Closure(Closure {
                    func: FuncId(0),
                    env,
                })
            }
            2 => Value::Agg(heap.struct_new(TypeId(0), vec![Value::I64(7)]).unwrap()),
            _ => Value::Agg(heap.array_new(ElemTypeId(0), vec![Value::I64(7)]).unwrap()),
        };
        let root = heap.tuple_new(vec![leaf]).unwrap();
        let (snapshot, _) = heap.into_snapshot(vec![Value::Agg(root)]).unwrap();
        assert!(matches!(
            TapeValue::from_snapshot(snapshot.clone(), None),
            Err(TapeError::MissingProgram { root: 0 })
        ));
        let values = TapeValue::from_snapshot(snapshot, Some(Arc::clone(&a))).unwrap();
        assert!(values[0].validate_input(3, &a).is_ok());
        assert!(matches!(
            values[0].validate_input(3, &b),
            Err(TapeError::ProgramMismatch { input: 3 })
        ));
        let mut portable = AggHeap::new();
        let child = portable.tuple_new(vec![Value::I64(42)]).unwrap();
        let wrapper = portable.tuple_new(vec![Value::Agg(child)]).unwrap();
        let (snapshot, _) = portable
            .into_snapshot(vec![Value::Agg(child), Value::Agg(wrapper)])
            .unwrap();
        let portable = TapeValue::from_snapshot(snapshot, None).unwrap();
        let mut graph =
            ExecutionGraph::new(TapeExecutor::new(HostProbe::default(), Limits::default()));
        let node = graph
            .add_tape_node(
                Arc::clone(&b),
                entries.same,
                vec!["first".into(), "wrapper".into()],
            )
            .unwrap();
        graph
            .set_input_value(node, "first", portable[0].clone())
            .unwrap();
        graph
            .set_input_value(node, "wrapper", portable[1].clone())
            .unwrap();
        graph.run_node(node).unwrap();
        graph
            .set_input_value(node, "first", values[0].clone())
            .unwrap();
        graph.invalidate_input("first");
        assert!(matches!(
            graph.run_node(node),
            Err(crate::GraphError::Node {
                source: TapeError::ProgramMismatch { input: 0 },
                ..
            })
        ));
        assert_eq!(graph.executor().host().calls, 1);
        assert_eq!(
            graph
                .node_outputs(node)
                .unwrap()
                .get("ret0")
                .unwrap()
                .value(),
            &Value::Bool(true)
        );
    }
}

#[test]
fn aliases_survive_separate_publications_and_equal_nodes_remain_distinct() {
    let (program, entries) = program(11);
    let mut graph = ExecutionGraph::new(TapeExecutor::new(HostProbe::default(), Limits::default()));
    let a = graph
        .add_tape_node(Arc::clone(&program), entries.tuple, vec!["x".into()])
        .unwrap();
    let equal = graph
        .add_tape_node(Arc::clone(&program), entries.tuple, vec!["x".into()])
        .unwrap();
    let b = graph
        .add_tape_node(Arc::clone(&program), entries.wrap, vec!["a".into()])
        .unwrap();
    let c = graph
        .add_tape_node(program, entries.same, vec!["a".into(), "b".into()])
        .unwrap();
    graph
        .set_input_value(a, "x", Value::I64(42).into())
        .unwrap();
    graph
        .set_input_value(equal, "x", Value::I64(42).into())
        .unwrap();
    graph.connect(a, "ret0", b, "a").unwrap();
    graph.connect(a, "ret0", c, "a").unwrap();
    graph.connect(b, "ret0", c, "b").unwrap();
    graph.run_node(c).unwrap();
    assert_eq!(
        graph.node_outputs(c).unwrap().get("ret0").unwrap().value(),
        &Value::Bool(true)
    );
    graph.connect(equal, "ret0", c, "a").unwrap();
    graph.run_node(c).unwrap();
    assert_eq!(
        graph.node_outputs(c).unwrap().get("ret0").unwrap().value(),
        &Value::Bool(false)
    );
}

#[test]
fn old_results_are_reclaimed_by_last_owner_and_outlive_executor() {
    let (program, entries) = program(11);
    let mut graph = ExecutionGraph::new(TapeExecutor::new(HostProbe::default(), Limits::default()));
    let node = graph
        .add_tape_node(program, entries.tuple, vec!["x".into()])
        .unwrap();
    let mut previous = None;
    let mut retained = None;
    for value in 0..1000 {
        graph
            .set_input_value(node, "x", Value::I64(value).into())
            .unwrap();
        graph.invalidate_input("x");
        graph.run_node(node).unwrap();
        if let Some(weak) = previous.take() {
            assert!(alloc::sync::Weak::<Bundle>::upgrade(&weak).is_none());
        }
        let output = graph.node_outputs(node).unwrap().get("ret0").unwrap();
        if value == 0 {
            retained = Some(output.clone());
        } else if let Repr::Shared { bundle, .. } = &output.repr {
            previous = Some(Arc::downgrade(bundle));
        }
    }
    drop(graph.remove_node(node).unwrap());
    assert!(previous.unwrap().upgrade().is_none());
    drop(graph);
    let retained = retained.unwrap();
    let Value::Agg(root) = retained.value() else {
        panic!("tuple")
    };
    assert_eq!(
        retained.aggregates().unwrap().tuple_get(*root, 0).unwrap(),
        Value::I64(0)
    );
    let Repr::Shared { bundle, .. } = &retained.repr else {
        panic!("owned bundle")
    };
    let weak = Arc::downgrade(bundle);
    drop(retained);
    assert!(weak.upgrade().is_none());
}

#[test]
fn portable_native_tuple_and_payload_clones_have_independent_owners() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<TapeValue>();
    let mut heap = AggHeap::new();
    let root = heap.tuple_new(vec![Value::I64(42)]).unwrap();
    let (snapshot, _) = heap.into_snapshot(vec![Value::Agg(root)]).unwrap();
    let values = TapeValue::from_snapshot(snapshot, None).unwrap();
    let (program, entries) = program(11);
    let mut graph = ExecutionGraph::new(TapeExecutor::new(HostProbe::default(), Limits::default()));
    let node = graph
        .add_tape_node(program, entries.identity, vec!["tuple".into()])
        .unwrap();
    graph
        .set_input_value(node, "tuple", values[0].clone())
        .unwrap();
    graph.run_all().unwrap();
    assert_eq!(
        graph.node_outputs(node).unwrap().get("ret0"),
        Some(&values[0])
    );
    let bytes: TapeValue = Value::Bytes(vec![42; 8192]).into();
    let clone = bytes.clone();
    let (Value::Bytes(a), Value::Bytes(b)) = (bytes.value(), clone.value()) else {
        panic!("bytes")
    };
    assert_eq!(a.as_ptr(), b.as_ptr());
}

#[test]
fn deep_shared_input_import_and_release_are_iterative() {
    let mut heap = AggHeap::new();
    let mut root = heap.tuple_new(vec![Value::I64(42)]).unwrap();
    for _ in 0..10_000 {
        root = heap
            .tuple_new(vec![Value::Agg(root), Value::Agg(root)])
            .unwrap();
    }
    let (snapshot, _) = heap.into_snapshot(vec![Value::Agg(root)]).unwrap();
    let values = TapeValue::from_snapshot(snapshot, None).unwrap();
    let mut importer = Importer::default();
    let mut target = AggHeap::new();
    let a = importer.import(&values[0], &mut target).unwrap();
    let b = importer.import(&values[0], &mut target).unwrap();
    assert_eq!(a, b);
    assert_eq!(target.len_u32(), 10_001);
    assert_eq!(importer.origins.domains.len(), 1);
    let (program, entries) = program(11);
    let mut graph = ExecutionGraph::new(TapeExecutor::new(HostProbe::default(), Limits::default()));
    let node = graph
        .add_tape_node(program, entries.identity, vec!["dag".into()])
        .unwrap();
    graph
        .set_input_value(node, "dag", values[0].clone())
        .unwrap();
    graph.run_node(node).unwrap();
    assert_eq!(
        graph.node_outputs(node).unwrap().get("ret0"),
        Some(&values[0])
    );
    drop(graph);
    drop(values);
    importer.clear();
}

#[test]
fn scalar_payload_output_does_not_pin_unrelated_aggregate_storage() {
    let (program, _) = program(11);
    let mut heap = AggHeap::new();
    let root = heap.tuple_new(vec![Value::Bytes(vec![42; 65536])]).unwrap();
    let mut outputs = Vec::new();
    TapeValue::publish(
        heap,
        vec![Value::Agg(root), Value::Str("label".into())],
        program,
        Origins::default(),
        &mut outputs,
    )
    .unwrap();
    let Repr::Shared { bundle, .. } = &outputs[0].repr else {
        panic!("aggregate owner")
    };
    let weak = Arc::downgrade(bundle);
    drop(outputs.remove(0));
    assert!(weak.upgrade().is_none());
    assert_eq!(outputs[0].value(), &Value::Str("label".into()));
}

#[test]
fn alias_origins_follow_compaction_after_a_dead_imported_prefix() {
    let (program, entries) = program(11);
    let mut graph = ExecutionGraph::new(TapeExecutor::new(HostProbe::default(), Limits::default()));
    let garbage = graph
        .add_tape_node(Arc::clone(&program), entries.tuple, vec!["x".into()])
        .unwrap();
    let shared = graph
        .add_tape_node(Arc::clone(&program), entries.tuple, vec!["x".into()])
        .unwrap();
    let wrapped = graph
        .add_tape_node(
            Arc::clone(&program),
            entries.discard_wrap,
            vec!["garbage".into(), "shared".into()],
        )
        .unwrap();
    let reader = graph
        .add_tape_node(
            program,
            entries.same,
            vec!["shared".into(), "wrapped".into()],
        )
        .unwrap();
    graph
        .set_input_value(garbage, "x", Value::I64(-1).into())
        .unwrap();
    graph
        .set_input_value(shared, "x", Value::I64(42).into())
        .unwrap();
    graph.connect(garbage, "ret0", wrapped, "garbage").unwrap();
    graph.connect(shared, "ret0", wrapped, "shared").unwrap();
    graph.connect(shared, "ret0", reader, "shared").unwrap();
    graph.connect(wrapped, "ret0", reader, "wrapped").unwrap();
    graph.run_node(reader).unwrap();
    assert_eq!(
        graph
            .node_outputs(wrapped)
            .unwrap()
            .get("ret0")
            .unwrap()
            .aggregates()
            .unwrap()
            .len_u32(),
        2
    );
    assert_eq!(
        graph
            .node_outputs(reader)
            .unwrap()
            .get("ret0")
            .unwrap()
            .value(),
        &Value::Bool(true)
    );
}

#[test]
fn custom_geometry_edges_can_delegate_tape_execution_and_keep_ownership() {
    use crate::{Executor, NodeAccess, NodeOutcome};
    use alloc::string::{String, ToString};

    #[derive(Clone, Debug)]
    enum SceneValue {
        Tape(TapeValue),
        Mesh(Arc<Vec<i64>>),
    }
    #[derive(Debug)]
    enum SceneNode {
        Tape(crate::TapeNode),
        Mesh,
    }
    struct Scene {
        tape: TapeExecutor<HostProbe>,
    }
    impl Executor for Scene {
        type Value = SceneValue;
        type Node = SceneNode;
        type Error = String;
        fn execute(
            &mut self,
            node: &mut SceneNode,
            inputs: &[SceneValue],
            outputs: &mut Vec<SceneValue>,
            access: &mut NodeAccess<'_, SceneValue>,
        ) -> Result<NodeOutcome, String> {
            match node {
                SceneNode::Tape(node) => {
                    let args = inputs
                        .iter()
                        .map(|input| match input {
                            SceneValue::Tape(value) => Ok(value.clone()),
                            SceneValue::Mesh(_) => Err(String::from("expected tape input")),
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let mut values = Vec::new();
                    self.tape
                        .execute_node(node, &args, &mut values, access)
                        .map_err(|error| error.to_string())?;
                    outputs.extend(values.into_iter().map(SceneValue::Tape));
                }
                SceneNode::Mesh => {
                    let SceneValue::Tape(parameters) = &inputs[0] else {
                        return Err(String::from("expected parameters"));
                    };
                    let Value::Agg(tuple) = parameters.value() else {
                        return Err(String::from("expected tuple"));
                    };
                    let Value::I64(size) = parameters
                        .aggregates()
                        .unwrap()
                        .tuple_get(*tuple, 0)
                        .unwrap()
                    else {
                        return Err(String::from("expected size"));
                    };
                    outputs.push(SceneValue::Mesh(Arc::new(vec![size, size + 1])));
                }
            }
            Ok(NodeOutcome::Complete)
        }
    }
    let (program, entries) = program(11);
    let mut graph = ExecutionGraph::new(Scene {
        tape: TapeExecutor::new(HostProbe::default(), Limits::default()),
    });
    let parameters = graph
        .add_node(
            SceneNode::Tape(crate::TapeNode::new(program, entries.tuple).unwrap()),
            vec!["size".into()],
            vec!["parameters".into()],
        )
        .unwrap();
    let mesh = graph
        .add_node(
            SceneNode::Mesh,
            vec!["parameters".into()],
            vec!["mesh".into()],
        )
        .unwrap();
    graph
        .set_input_value(parameters, "size", SceneValue::Tape(Value::I64(42).into()))
        .unwrap();
    graph
        .connect(parameters, "parameters", mesh, "parameters")
        .unwrap();
    graph.run_node(mesh).unwrap();
    let retained = graph
        .node_outputs(mesh)
        .unwrap()
        .get("mesh")
        .unwrap()
        .clone();
    drop(graph.remove_node(parameters).unwrap());
    drop(graph.remove_node(mesh).unwrap());
    drop(graph);
    let SceneValue::Mesh(mesh) = retained else {
        panic!("mesh output")
    };
    assert_eq!(mesh.as_ref(), &[42, 43]);
}

#[test]
fn selected_ports_do_not_import_unrelated_bundle_roots() {
    let mut heap = AggHeap::new();
    let child = heap.tuple_new(vec![Value::I64(42)]).unwrap();
    let wrapper = heap.tuple_new(vec![Value::Agg(child)]).unwrap();
    let mut large = heap.tuple_new(vec![Value::Bytes(vec![42; 65536])]).unwrap();
    for _ in 0..1000 {
        large = heap.tuple_new(vec![Value::Agg(large)]).unwrap();
    }
    let (snapshot, _) = heap
        .into_snapshot(vec![
            Value::Agg(child),
            Value::Agg(wrapper),
            Value::Agg(large),
        ])
        .unwrap();
    let values = TapeValue::from_snapshot(snapshot, None).unwrap();
    let (program, entries) = program(11);
    let mut graph = ExecutionGraph::new(TapeExecutor::new(HostProbe::default(), Limits::default()));
    let node = graph
        .add_tape_node(
            program,
            entries.same,
            vec!["first".into(), "wrapper".into()],
        )
        .unwrap();
    graph
        .set_input_value(node, "first", values[0].clone())
        .unwrap();
    graph
        .set_input_value(node, "wrapper", values[1].clone())
        .unwrap();
    graph.run_all().unwrap();
    assert_eq!(graph.executor().host().heap_nodes, [2]);
    assert_eq!(
        graph
            .node_outputs(node)
            .unwrap()
            .get("ret0")
            .unwrap()
            .value(),
        &Value::Bool(true)
    );
}
