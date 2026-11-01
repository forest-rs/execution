// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{HostContext, HostError, HostSig, SigHash},
    program::ValueType,
    source::{SourceDocument, SourceId, SourceLimits},
    value::FuncId,
    vm::{InvocationOptions, Limits, Vm},
};
use ratatui::crossterm::event::KeyModifiers;

struct NoHost;
impl Host for NoHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        panic!("the debugger does not dispatch these requests")
    }
}

fn code(name: &str) -> (VerifiedProgram, SourceMap) {
    let text = format!("{name} calls its host\n");
    let mut builder = ProgramBuilder::new();
    builder.set_program_name(name);
    let operation = builder.host_sig_for(
        name,
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    builder.set_function_name(entry, name).unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    body.source(SourceId(0), 0..text.len());
    let [answer] = builder.emit_call(&mut body, operation, &[]).unwrap();
    body.ret(0, &[answer]);
    let mappings = body.source_mappings(entry).collect();
    builder.define_function(entry, body).unwrap();
    let program = builder.build_verified().unwrap();
    let sources = SourceMap::new(
        program.program(),
        vec![SourceDocument {
            name: format!("{name}.guest"),
            text,
        }],
        mappings,
        SourceLimits::default(),
    )
    .unwrap();
    (program, sources)
}

#[test]
fn caller_and_child_with_equal_local_ids_keep_their_own_code_sources_and_breakpoints() {
    let (caller, caller_source) = code("send_method");
    let (child, child_source) = code("read_cell");
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = vm.instantiate(&child).unwrap();
    let mut run = vm
        .start(
            &caller,
            FuncId(0),
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    run.resume(100).unwrap();
    run.pending_host_call()
        .unwrap()
        .call_instance(
            &instance,
            FuncId(0),
            vec![],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    run.resume(100).unwrap();
    let mut view = LiveExplorer::new(run)
        .sources(&child_source)
        .unwrap()
        .sources_for(&caller, &caller_source)
        .unwrap();
    assert!(
        view.code.iter().any(|(_, line)| line.contains("read_cell")),
        "the leaf uses its actual imported host name"
    );
    assert!(
        view.source_lines
            .join("\n")
            .contains("read_cell calls its host"),
        "the leaf selects its exact source definition"
    );
    view.selected_frame = 1;
    view.refresh();
    assert!(
        view.code
            .iter()
            .any(|(_, line)| line.contains("send_method")),
        "selecting f0 in another definition invalidates the disassembly cache"
    );
    assert!(
        view.source_lines
            .join("\n")
            .contains("send_method calls its host"),
        "the suspended caller uses its own source map"
    );
    view.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
    assert!(
        view.has_breakpoint(&caller, view.frames[1].location),
        "the selected caller owns the breakpoint"
    );
    assert!(
        !view.has_breakpoint(&child, view.frames[0].location),
        "equal local IDs never create a breakpoint in another definition"
    );
}
