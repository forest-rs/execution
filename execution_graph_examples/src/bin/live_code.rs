// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Shared mutable captures and live lookup updates, without patching active code.

use execution_graph::{ExecutionGraph, TapeExecutor};
use execution_tape::{
    asm::{FunctionAbi, ProgramBuilder},
    callable::{Callable, CodeSlot, CodeUpdate, CodeVersion},
    contract::Contract,
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{HostTypeId, ValueType},
    recording::{RecordLimits, Recorder, Recording, ReplayOutcome},
    source::{SourceDocument, SourceError, SourceId, SourceLimits, SourceMap, SourceMapping},
    trace::{TraceLocation, TraceSink},
    value::{FuncId, Obj, ObjHandle, Value},
    vm::{Invocation, InvocationOptions, Limits, RunStatus, Vm},
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const CELL: HostTypeId = HostTypeId(1);
fn receiver() -> Obj {
    Obj {
        host_type: CELL,
        handle: ObjHandle(42),
    }
}

#[derive(Default)]
struct Store {
    cells: BTreeMap<u64, i64>,
    calls: usize,
}
#[derive(Clone)]
struct Cells(Rc<RefCell<Store>>);
impl Cells {
    fn new() -> Self {
        Self(Rc::new(RefCell::new(Store {
            cells: [(receiver().handle.0, 0), (99, 999)].into(),
            calls: 0,
        })))
    }
    fn value(&self) -> i64 {
        self.0.borrow().cells[&receiver().handle.0]
    }
}
impl Host for Cells {
    fn call(
        &mut self,
        symbol: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> std::result::Result<u64, HostError> {
        let [ValueRef::Obj(object), ValueRef::I64(delta)] = args else {
            return Err(HostError::Failed);
        };
        if symbol != "CounterCell.add" || object.host_type != CELL {
            return Err(HostError::Failed);
        }
        let mut store = self.0.borrow_mut();
        let value = store
            .cells
            .get_mut(&object.handle.0)
            .ok_or(HostError::Failed)?;
        *value = value.checked_add(*delta).ok_or(HostError::Failed)?;
        rets[0] = Value::I64(*value);
        store.calls += 1;
        Ok(0)
    }
}

struct Revision {
    factory: Callable,
    sources: SourceMap,
}
fn mapping(source: &str, function: FuncId, pc: u32, fragment: &str) -> SourceMapping {
    let start = source.find(fragment).expect("authored demo fragment");
    SourceMapping {
        location: TraceLocation { func: function, pc },
        document: SourceId(0),
        range: start..start + fragment.len(),
    }
}
fn revision(increment: i64) -> Result<Revision> {
    // This tiny example assembles its source directly. A language frontend would supply
    // these contracts and exact instruction-to-source sidecars from its compiler.
    let source = format!(
        "fn make_counter(receiver) {{\n    return || {{\n        let cell = receiver;\n        return CounterCell.add(cell, {increment});\n    }};\n}}\n"
    );
    let mut b = ProgramBuilder::new();
    b.set_program_name(&format!("Counter callback · increment {increment}"));
    let integer = b.contract(Contract::Kind(ValueType::I64))?;
    let object = b.contract(Contract::Kind(ValueType::Obj(CELL)))?;
    let env = b.contract(Contract::Tuple(&[Some(object)]))?;
    let closure = b.contract(Contract::Closure {
        args: &[],
        rets: &[integer],
    })?;
    let add = b.host_sig_for(
        "CounterCell.add",
        HostSig {
            args: vec![ValueType::Obj(CELL), ValueType::I64],
            rets: vec![ValueType::I64],
        },
    );
    let body = b.declare_function(FunctionAbi {
        args: vec![env.into()],
        rets: vec![integer.into()],
    })?;
    let mut a = b.function_asm(body)?;
    let cell = a.alloc_register(ValueType::Obj(CELL));
    let delta = a.alloc_register(ValueType::I64);
    let answer = a.alloc_register(ValueType::I64);
    let mut mappings = vec![mapping(&source, body, a.pc(), "let cell = receiver;")];
    a.tuple_get(cell, 1, 0);
    mappings.push(mapping(&source, body, a.pc(), &increment.to_string()));
    a.const_i64(delta, increment);
    mappings.push(mapping(
        &source,
        body,
        a.pc(),
        &format!("CounterCell.add(cell, {increment})"),
    ));
    a.host_call_sig_id(0, add, 0, &[cell, delta], &[answer]);
    mappings.push(mapping(
        &source,
        body,
        a.pc(),
        &format!("return CounterCell.add(cell, {increment});"),
    ));
    a.ret(0, &[answer]);
    b.define_function(body, a)?;
    b.set_function_name(body, "counter_block")?;
    let factory = b.declare_function(FunctionAbi {
        args: vec![object.into()],
        rets: vec![closure.into()],
    })?;
    let mut a = b.function_asm(factory)?;
    let env = a.alloc_register(ValueType::Agg);
    let function = a.alloc_register(ValueType::Func);
    let block = a.alloc_register(ValueType::Closure);
    for step in 0..4 {
        mappings.push(mapping(&source, factory, a.pc(), "return || {"));
        match step {
            0 => {
                a.tuple_new(env, &[1]);
            }
            1 => {
                a.const_func(function, body);
            }
            2 => {
                a.closure_new(block, function, env);
            }
            _ => {
                a.ret(0, &[block]);
            }
        }
    }
    b.define_function(factory, a)?;
    b.set_function_name(factory, "make_counter")?;
    let program = Arc::new(b.build_verified()?);
    let sources = SourceMap::new(
        program.program(),
        vec![SourceDocument {
            name: "counter.expr".into(),
            text: source,
        }],
        mappings,
        SourceLimits::default(),
    )?;
    Ok(Revision {
        factory: Callable::function(program, factory)?,
        sources,
    })
}

// Publication identity is embedding provenance; Callable itself retains executable code and
// captures. Graph output identity is unchanged by exporting a callable.
struct Callback {
    code: CodeVersion,
    target: Callable,
}
fn graph_callback(code: CodeVersion, host: &Cells) -> Result<Callback> {
    let mut graph = ExecutionGraph::new(TapeExecutor::new(host.clone(), Limits::default()));
    let node = graph.add_tape_node(
        Arc::clone(code.callable().program()),
        code.callable().function_id(),
        vec!["receiver".into()],
    )?;
    graph.set_input_value(node, "receiver", Value::Obj(receiver()).into())?;
    graph.run_node(node)?;
    let target = graph
        .node_outputs(node)
        .unwrap()
        .get("ret0")
        .unwrap()
        .retain_callable()?;
    drop(graph.remove_node(node)?);
    drop(graph);
    Ok(Callback { code, target })
}
fn returned(values: &[Value]) -> Result<i64> {
    match values {
        [Value::I64(n)] => Ok(*n),
        _ => Err("expected one integer result".into()),
    }
}
fn checked_recording(recording: Recording, expected: i64, host: &Cells) -> Result<()> {
    let before = (host.value(), host.0.borrow().calls);
    let ReplayOutcome::Returned(values) = recording.replay()? else {
        return Err("checked replay trapped".into());
    };
    if returned(values.values())? != expected {
        return Err("checked replay returned another value".into());
    }
    if before != (host.value(), host.0.borrow().calls) {
        return Err("replay mutated the live cell".into());
    }
    Ok(())
}
fn invoke(callback: &Callback, host: &Cells) -> Result<i64> {
    let mut vm = Vm::new(host.clone(), Limits::default());
    let mut recorder = Recorder::new(
        Arc::clone(callback.target.program()),
        RecordLimits::default(),
    );
    let mut call = callback.target.start(
        &mut vm,
        vec![],
        InvocationOptions::default().trace(recorder.mask(), &mut recorder),
    )?;
    while call.resume(1)? == RunStatus::Suspended {}
    let answer = returned(call.result().unwrap().map_err(Clone::clone)?)?;
    drop(call);
    checked_recording(recorder.finish(), answer, host)?;
    Ok(answer)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    None,
    Snapshot,
    Interactive,
}

#[cfg(feature = "observatory")]
fn finish_paused<'a>(
    mut call: Invocation<'a, Cells>,
    sources: &'a SourceMap,
    view: View,
) -> Result<(Option<i64>, Option<String>)> {
    use execution_tape_observatory::{LiveExplorer, ObjectView};
    if view == View::None {
        call.resume(u64::MAX)?;
        return Ok((
            Some(returned(call.result().unwrap().map_err(Clone::clone)?)?),
            None,
        ));
    }
    let mut explorer = LiveExplorer::new(call).sources(sources)?.object_inspector(
        |host: &Cells, object: Obj, _limit| {
            if object.host_type != CELL {
                return None;
            }
            let value = *host.0.borrow().cells.get(&object.handle.0)?;
            Some(ObjectView {
                name: "CounterCell".into(),
                fields: vec![("value".into(), Value::I64(value))],
                total_fields: Some(1),
            })
        },
    );
    explorer.inspect_object(receiver());
    let snapshot = explorer.render_text(140, 42)?;
    if view == View::Interactive {
        explorer.run()?;
    } else {
        print!("{snapshot}");
        explorer.resume(u64::MAX)?;
    }
    let answer = explorer
        .result()
        .map(|r| r.map_err(Clone::clone))
        .transpose()?
        .map(returned)
        .transpose()?;
    Ok((answer, Some(snapshot)))
}
#[cfg(not(feature = "observatory"))]
fn finish_paused(
    mut call: Invocation<'_, Cells>,
    _: &SourceMap,
    view: View,
) -> Result<(Option<i64>, Option<String>)> {
    if view != View::None {
        return Err("--snapshot/--interactive requires --features observatory".into());
    }
    call.resume(u64::MAX)?;
    Ok((
        Some(returned(call.result().unwrap().map_err(Clone::clone)?)?),
        None,
    ))
}

struct Story {
    fresh: i64,
    paused: Option<i64>,
    peer: i64,
    collected: usize,
    calls: usize,
    snapshot: Option<String>,
}
impl Story {
    fn json(&self) -> String {
        format!(
            "{{\n  \"schema\": \"execution-live-code-example/1\",\n  \"published_revision\": 1,\n  \"paused_revision\": 0,\n  \"receiver\": {{\"host_type\": 1, \"handle\": 42}},\n  \"fresh_result\": {},\n  \"paused_result\": {},\n  \"old_peer_result\": {},\n  \"final_cell\": {},\n  \"host_calls\": {},\n  \"collected_unreachable_cells\": {},\n  \"source_mismatch_rejected\": true,\n  \"checked_replays\": {},\n  \"replays_mutated_host\": false,\n  \"programs\": [\"old.tape\", \"new.tape\"],\n  \"source_sidecars\": [\"old.sources\", \"new.sources\"]\n}}\n",
            self.fresh,
            self.paused.map_or_else(|| "null".into(), |n| n.to_string()),
            self.peer,
            self.peer,
            self.calls,
            self.collected,
            if self.paused.is_some() { 3 } else { 2 }
        )
    }
}
fn run(view: View, old: &Revision, new: &Revision) -> Result<Story> {
    let host = Cells::new();
    let mut slot = CodeSlot::new(old.factory.clone());
    let first = graph_callback(slot.current(), &host)?;
    let peer = graph_callback(slot.current(), &host)?;
    let mut vm = Vm::new(host.clone(), Limits::default());
    let mut recorder = Recorder::new(Arc::clone(first.target.program()), RecordLimits::default());
    let mut paused = first.target.start(
        &mut vm,
        vec![],
        InvocationOptions::default().trace(recorder.mask(), &mut recorder),
    )?;
    assert_eq!(
        paused.resume(2)?,
        RunStatus::Suspended,
        "old callback must pause before its host effect"
    ); // receiver + delta, before host effect
    assert_eq!(
        host.value(),
        0,
        "preparing and pausing a closure must not mutate its cell"
    );
    let old_location = paused.inspect().frames().next().unwrap().location;
    assert!(
        old.sources
            .bind(paused.program().program())?
            .lookup(old_location)
            .is_some(),
        "paused old code must resolve to its authored source"
    );
    assert!(
        matches!(
            new.sources.bind(paused.program().program()),
            Err(SourceError::ProgramMismatch)
        ),
        "new source must not bind to paused old code"
    );
    let CodeUpdate::Replaced { previous, current } = slot.replace(new.factory.clone())? else {
        return Err("expected a new code publication".into());
    };
    assert_eq!(
        first.code, previous,
        "exported callback must retain its original publication"
    );
    assert_eq!(
        current.revision(),
        1,
        "replacement must allocate the next slot revision"
    );
    assert!(
        first.code.same_slot(&current),
        "both revisions must belong to one lookup slot"
    );
    let fresh = graph_callback(slot.current(), &host)?;
    let fresh_result = invoke(&fresh, &host)?;
    assert_eq!(fresh_result, 10, "a fresh lookup must execute the +10 body");
    // This is an example-owned collector. Real languages must also visit their heap edges,
    // globals and external roots. VM roots are conservative, not a minimal liveness set.
    let mut roots = BTreeSet::new();
    for callback in [&first, &peer, &fresh] {
        callback.target.visit_objects(|o| {
            roots.insert(o.handle.0);
        });
    }
    paused.inspect().visit_objects(|root| {
        roots.insert(root.object.handle.0);
    });
    let before = host.0.borrow().cells.len();
    host.0
        .borrow_mut()
        .cells
        .retain(|handle, _| roots.contains(handle));
    let collected = before - host.0.borrow().cells.len();
    assert_eq!(
        collected, 1,
        "only the unreachable second cell should be collected"
    );
    assert_eq!(
        host.value(),
        10,
        "collection must preserve the shared live cell"
    );
    if view != View::None {
        eprintln!(
            "Lookup revision 1 uses +10; the paused callback keeps revision 0 (+1). Shared cell: 10."
        );
    }
    let (paused_result, snapshot) = finish_paused(paused, &old.sources, view)?;
    if let Some(result) = paused_result {
        assert_eq!(
            result, 11,
            "paused old code must add 1 to the updated shared cell"
        );
        checked_recording(recorder.finish(), result, &host)?;
    } else {
        // Leaving the interactive view early cancels the old invocation. No return/replay
        // is fabricated for it, and its host effect has not been rolled back.
        drop(recorder);
    }
    let peer_result = invoke(&peer, &host)?;
    assert_eq!(
        peer_result,
        host.value(),
        "the second old closure must observe the same cell"
    );
    let calls = host.0.borrow().calls;
    Ok(Story {
        fresh: fresh_result,
        paused: paused_result,
        peer: peer_result,
        collected,
        calls,
        snapshot,
    })
}
fn capture(directory: &Path, story: &Story, old: &Revision, new: &Revision) -> Result<()> {
    std::fs::create_dir(directory)?; // A new directory only; never overwrite evidence.
    for (name, data) in [
        ("old.tape", old.factory.program().program().encode()),
        ("new.tape", new.factory.program().program().encode()),
        ("old.sources", old.sources.encode()),
        ("new.sources", new.sources.encode()),
        ("story.json", story.json().into_bytes()),
    ] {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(name))?
            .write_all(&data)?;
    }
    if let Some(text) = &story.snapshot {
        std::fs::write(directory.join("paused.txt"), text)?;
    }
    Ok(())
}
fn main() -> Result<()> {
    let mut view = View::None;
    let mut directory = None::<PathBuf>;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--snapshot" => view = View::Snapshot,
            "--interactive" => view = View::Interactive,
            "--capture" => {
                directory = Some(
                    args.next()
                        .ok_or("--capture requires a new directory")?
                        .into(),
                );
            }
            "--help" | "-h" => {
                println!(
                    "live_code [--snapshot | --interactive] [--capture NEW_DIR]\n\nA graph exports two +1 closures sharing a host cell. Pause one, publish +10,\nrun the new closure, then resume the old code. Default: run and print JSON.\nUI options require --features observatory. --snapshot renders before resuming.\n--interactive executes real host effects; q cancels unfinished work.\nCaptures include exact tapes, source sidecars and results, not a portable heap or replay archive."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown option {arg}; use --help").into()),
        }
    }
    #[cfg(not(feature = "observatory"))]
    if view != View::None {
        return Err("--snapshot/--interactive requires --features observatory".into());
    }
    let old = revision(1)?;
    let new = revision(10)?;
    let story = run(view, &old, &new)?;
    println!("{}", story.json());
    if let Some(directory) = directory {
        capture(&directory, &story, &old, &new)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn graph_export_update_pause_shared_cell_collection_and_replay_compose() {
        let story = run(View::None, &revision(1).unwrap(), &revision(10).unwrap()).unwrap();
        assert_eq!(
            (
                story.fresh,
                story.paused,
                story.peer,
                story.calls,
                story.collected
            ),
            (10, Some(11), 12, 3, 1)
        );
    }
    #[cfg(feature = "observatory")]
    #[test]
    fn live_snapshot_shows_old_source_and_current_shared_cell_after_new_code_ran() {
        let story = run(
            View::Snapshot,
            &revision(1).unwrap(),
            &revision(10).unwrap(),
        )
        .unwrap();
        let text = story.snapshot.unwrap();
        assert!(text.contains("increment 1"));
        assert!(!text.contains("increment 10"));
        assert!(text.contains("value = 10"));
        assert!(text.contains("CounterCell.add(cell, 1)"));
        assert!(text.contains("CounterCell.add"));
        assert!(text.contains("HOST OBJECT · read-only"));
        assert_eq!(story.paused, Some(11));
    }
}
