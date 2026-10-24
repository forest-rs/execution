// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use crate::{render_program, render_screen};
use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::program::{SpanId, ValueType};

fn fixture() -> Program {
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("branch and call");
    let mut helper = Asm::new();
    helper.set_register_types(vec![ValueType::Unit]);
    helper.ret(0, &[]);
    let helper = builder
        .push_function_checked(
            helper,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            },
        )
        .unwrap();
    builder.set_function_name(helper, "helper").unwrap();
    let mut root = Asm::new();
    root.set_register_types(vec![ValueType::Unit, ValueType::Bool]);
    let yes = root.label();
    let no = root.label();
    root.span(SpanId::try_from(42).unwrap());
    root.const_bool(1, true).br(1, yes, no);
    root.place(yes).unwrap();
    root.call(0, helper, 0, &[], &[]).ret(0, &[]);
    root.place(no).unwrap();
    root.ret(0, &[]);
    let root = builder
        .push_function_checked(
            root,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            },
        )
        .unwrap();
    builder.set_function_name(root, "root").unwrap();
    builder.build_checked().unwrap()
}

fn press(view: &mut Browser<'_>, key: KeyCode) {
    assert!(view.key(KeyEvent::new(key, KeyModifiers::NONE)));
}

#[test]
fn encoded_program_can_be_browsed_without_a_recording_or_host() {
    let program = Program::decode(&fixture().encode()).unwrap();
    let screen = render_program(&program, 120, 36).unwrap();
    assert!(screen.contains("PROGRAM · VERIFIED"));
    assert!(screen.contains("branch and call"));
    assert!(screen.contains("2 functions · 0 imports"));
    assert!(screen.contains("helper"));
    assert!(screen.contains("return"));
    assert!(screen.contains("no execution"));
    assert!(!screen.contains("CAPTURE COMPLETE"));
    let small = render_program(&program, 79, 24).unwrap();
    assert!(small.contains("current size: 79 × 24"));
}

#[test]
fn follow_branches_and_calls_then_return_to_exact_instruction() {
    let program = fixture();
    let mut view = Browser::new(&program);
    press(&mut view, KeyCode::Down); // root
    press(&mut view, KeyCode::Enter); // instructions
    press(&mut view, KeyCode::Down); // branch
    let branch = view.instruction;
    press(&mut view, KeyCode::Char('f'));
    assert!(matches!(
        view.source
            .as_ref()
            .unwrap()
            .instrs()
            .nth(view.instruction)
            .unwrap()
            .operands(),
        Operands::Ret { .. }
    ));
    press(&mut view, KeyCode::Backspace);
    assert_eq!(view.instruction, branch);
    press(&mut view, KeyCode::Enter); // true arm / direct call
    let call = view.instruction;
    let history = view.history.clone();
    press(&mut view, KeyCode::Char('f')); // calls do not have a false arm
    assert_eq!(view.history, history);
    assert_eq!(view.instruction, call);
    assert_eq!(view.section, Section::Functions);
    assert!(view.message.contains("no false branch"));
    press(&mut view, KeyCode::Enter);
    assert_eq!(view.message, CONTEXT); // helper
    assert_eq!(view.source.as_ref().unwrap().func(), FuncId(0));
    press(&mut view, KeyCode::Backspace);
    assert_eq!(view.source.as_ref().unwrap().func(), FuncId(1));
    assert_eq!(view.instruction, call);
    let shown = render_screen(|| view, 120, 36).unwrap();
    assert!(shown.contains("Source span ID: 42"));
}

#[test]
fn search_focus_and_empty_tables_have_no_fabricated_selection() {
    let program = fixture();
    let mut view = Browser::new(&program);
    press(&mut view, KeyCode::Char('/'));
    for ch in "root".chars() {
        press(&mut view, KeyCode::Char(ch));
    }
    assert_eq!(view.entries, [1]);
    press(&mut view, KeyCode::Enter);
    press(&mut view, KeyCode::Tab);
    assert_eq!(view.focus, Focus::Code);
    press(&mut view, KeyCode::BackTab);
    assert_eq!(view.focus, Focus::Navigator);
    press(&mut view, KeyCode::Char(']')); // no imports
    assert!(view.entries.is_empty());
    press(&mut view, KeyCode::End);
    press(&mut view, KeyCode::Enter);
    assert_eq!(view.selected, 0);
    press(&mut view, KeyCode::Char('/'));
    press(&mut view, KeyCode::Char('q')); // query, not quit
    assert_eq!(view.filter, "q");
    press(&mut view, KeyCode::Esc);
    assert!(view.filter.is_empty());
}

#[test]
fn verifier_failure_opens_exact_instruction_and_remains_navigable() {
    let mut program = fixture();
    // The bool argument of the branch is declared i64 instead.
    let reg = program.functions[1].register_types.offset as usize + 1;
    program.register_type_ids[reg] = u32::try_from(program.value_types.len()).unwrap();
    program.value_types.push(ValueType::I64);
    let mut view = Browser::new(&program);
    assert_eq!(view.error.as_ref().unwrap().function(), Some(FuncId(1)));
    assert_eq!(view.source.as_ref().unwrap().func(), FuncId(1));
    assert_eq!(
        Some(
            view.source
                .as_ref()
                .unwrap()
                .instrs()
                .nth(view.instruction)
                .unwrap()
                .pc()
        ),
        view.error.as_ref().unwrap().pc()
    );
    press(&mut view, KeyCode::Home); // helper
    press(&mut view, KeyCode::Char('d'));
    assert_eq!(view.source.as_ref().unwrap().func(), FuncId(1));
    let screen = render_screen(|| view, 120, 36).unwrap();
    assert!(screen.contains("PROGRAM · REJECTED"));
    assert!(screen.contains("declared I64, required Bool"), "{screen}");
}

#[test]
fn malformed_bytecode_and_metadata_remain_inspectable() {
    let mut program = fixture();
    program.bytecode_data[0] = 255;
    let screen = render_program(&program, 120, 36).unwrap();
    assert!(screen.contains("REJECTED"));
    assert!(screen.contains("Cannot decode function"));
    program.symbols.clear();
    program.functions[0].register_types.len = u32::MAX;
    let screen = render_program(&program, 120, 36).unwrap();
    assert!(screen.contains("REJECTED"));
    assert!(screen.contains("function #0"));
}

#[test]
fn imported_calls_constants_and_aggregate_types_are_inspectable_without_bindings() {
    use execution_tape::host::HostSig;
    use execution_tape::program::StructTypeDef;
    let mut builder = ProgramBuilder::new();
    let import = builder.host_sig_for(
        "external_echo",
        HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        },
    );
    builder.const_str("hello\u{1b}[31m");
    builder.struct_type(StructTypeDef {
        field_names: vec!["length".into()],
        field_types: vec![ValueType::I64],
    });
    builder.types_mut().array_elems.push(ValueType::Str);
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::I64]);
    body.const_i64(1, 7)
        .host_call_sig_id(0, import, 0, &[1], &[1])
        .ret(0, &[1]);
    builder
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    let program = Program::decode(&builder.build_checked().unwrap().encode()).unwrap();
    let mut view = Browser::new(&program);
    press(&mut view, KeyCode::Enter);
    press(&mut view, KeyCode::Down);
    let call = view.instruction;
    let history = view.history.clone();
    press(&mut view, KeyCode::Char('f')); // calls do not have a false arm
    assert_eq!(view.history, history);
    assert_eq!(view.instruction, call);
    assert_eq!(view.section, Section::Functions);
    assert!(view.message.contains("no false branch"));
    press(&mut view, KeyCode::Enter);
    assert_eq!(view.message, CONTEXT);
    assert_eq!(view.section, Section::Imports);
    assert!(
        render::details(&view)
            .iter()
            .any(|line| line.contains("external_echo(i64) → (i64)"))
    );
    press(&mut view, KeyCode::Tab); // instruction details
    press(&mut view, KeyCode::Tab); // focus details, keep that context
    assert_eq!(view.focus, Focus::Details);
    assert!(
        render::details(&view)
            .iter()
            .any(|line| line.contains("r1: i64"))
    );
    press(&mut view, KeyCode::Backspace);
    assert_eq!(view.instruction, call);
    assert_eq!(view.section, Section::Functions);
    press(&mut view, KeyCode::Char(']'));
    press(&mut view, KeyCode::Char(']'));
    assert_eq!(view.section, Section::Constants);
    let screen = render_screen(|| view, 120, 36).unwrap();
    assert!(screen.contains("hello\\u{1b}[31m"));
    assert!(!screen.contains('\u{1b}'));
    let mut view = Browser::new(&program);
    view.section = Section::Types;
    view.refilter();
    assert!(
        render::details(&view)
            .iter()
            .any(|line| line.contains("length: i64"))
    );
    press(&mut view, KeyCode::Down);
    assert!(
        render::details(&view)
            .iter()
            .any(|line| line.contains("Array element type #0: text"))
    );
}

#[test]
fn wrapped_details_scroll_to_the_last_display_line() {
    use ratatui::{Terminal, backend::TestBackend};
    let mut program = fixture();
    program.functions[1].register_types.len = 0;
    let mut view = Browser::new(&program);
    view.focus = Focus::Details;
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    terminal.draw(|frame| view.draw(frame)).unwrap();
    assert!(view.details_end > 0);
    press(&mut view, KeyCode::End);
    assert_eq!(view.details_offset, view.details_end);
    terminal.draw(|frame| view.draw(frame)).unwrap();
    let shown = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(shown.contains("r0: unit · effect token"), "{shown}");
}

#[test]
fn malformed_import_ranges_never_look_like_empty_signatures() {
    use execution_tape::host::HostSig;
    for arguments in [true, false] {
        let mut builder = ProgramBuilder::new();
        builder.host_sig_for(
            "external_echo",
            HostSig {
                args: vec![ValueType::I64],
                rets: vec![ValueType::I64],
            },
        );
        let mut program = builder.build_checked().unwrap();
        let range = if arguments {
            &mut program.host_sigs[0].args
        } else {
            &mut program.host_sigs[0].rets
        };
        range.offset = u32::MAX;
        let mut view = Browser::new(&program);
        press(&mut view, KeyCode::Char(']'));
        let details = render::details(&view).join("\n");
        assert!(details.contains(if arguments {
            "<malformed argument types>"
        } else {
            "<malformed return types>"
        }));
        assert!(!details.contains("external_echo()"));
        assert!(!details.contains("→ ()"));
    }
}

#[test]
fn diagnostic_formatting_bounds_large_contract_paths() {
    use execution_tape::contract::{
        ContractId, ContractMismatch, ContractPosition, ContractViolation,
    };
    let error = VerifyError::ContractMismatch {
        func: 0,
        pc: 0,
        position: ContractPosition::Result { index: 0 },
        error: ContractViolation {
            contract: ContractId(0),
            path: vec![0; 20_000],
            mismatch: ContractMismatch::Kind {
                expected: ValueType::I64,
                actual: ValueType::Bool,
            },
        },
    };
    let text = describe::diagnostic(&error);
    assert!(text.ends_with('…'));
    assert!(text.chars().count() <= 601);
}

#[test]
fn invalid_snapshot_dimensions_are_rejected_before_preparing_the_program() {
    let program = fixture();
    let constructions = std::cell::Cell::new(0);
    let result = render_screen(
        || {
            constructions.set(constructions.get() + 1);
            Browser::new(&program)
        },
        0,
        0,
    );
    assert!(result.is_err());
    assert_eq!(constructions.get(), 0);
}

#[test]
fn compositional_signature_previews_are_nested_and_bounded() {
    use execution_tape::contract::Contract;
    use execution_tape::program::StructTypeDef;
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let inner = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let outer = b.contract(Contract::Tuple(&[Some(inner), None])).unwrap();
    let schema = b.struct_type(StructTypeDef {
        field_names: (0..1000).map(|i| format!("f{i}")).collect(),
        field_types: vec![ValueType::I64; 1000],
    });
    let fields = vec![integer; 1000];
    let structure = b
        .contract(Contract::Struct {
            type_id: schema,
            fields: &fields,
        })
        .unwrap();
    let mut deep = outer;
    for _ in 0..2000 {
        deep = b
            .contract(Contract::Tuple(&[Some(deep), Some(deep)]))
            .unwrap();
    }
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::Agg]);
    body.ret(0, &[1]);
    b.push_function_checked(
        body,
        execution_tape::asm::FunctionAbi {
            args: vec![outer.into()],
            rets: vec![outer.into()],
        },
    )
    .unwrap();
    let program = b.build_checked().unwrap();
    let view = Browser::new(&program);
    let details = render::details(&view).join("\n");
    assert!(details.contains("tuple(tuple(i64), ?)"), "{details}");
    let nested = describe::contracts(&program, &[outer]);
    assert!(nested.contains("tuple(tuple(i64), ?)"), "{nested}");
    let structure = describe::contracts(&program, &[structure]);
    assert!(structure.contains("… +996"));
    assert!(structure.len() < 150);
    let deep = describe::contracts(&program, &[deep]);
    assert!(deep.contains("aggregate #"));
    assert!(deep.len() < 200);
}

fn nested_fixture(caller: bool) -> (Program, FuncId, execution_tape::contract::ContractId) {
    use execution_tape::asm::FunctionAbi;
    use execution_tape::contract::Contract;
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let boolean = b.contract(Contract::Kind(ValueType::Bool)).unwrap();
    let inner = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let outer = b
        .contract(Contract::Tuple(&[Some(inner), Some(boolean)]))
        .unwrap();
    let wrong_inner = b.contract(Contract::Tuple(&[Some(boolean)])).unwrap();
    let wrong_outer = b
        .contract(Contract::Tuple(&[Some(wrong_inner), Some(boolean)]))
        .unwrap();
    let entry = caller.then(|| {
        b.declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap()
    });
    let mut make = Asm::new();
    make.set_register_types(vec![
        ValueType::Unit,
        ValueType::I64,
        ValueType::Bool,
        ValueType::Agg,
        ValueType::Agg,
    ]);
    make.const_i64(1, 42)
        .const_bool(2, true)
        .tuple_new(3, &[1])
        .tuple_new(4, &[3, 2])
        .ret(0, &[4]);
    let make = b
        .push_function_checked(
            make,
            FunctionAbi {
                args: vec![],
                rets: vec![outer.into()],
            },
        )
        .unwrap();
    b.set_function_name(make, "make_nested").unwrap();
    if let Some(entry) = entry {
        let mut main = Asm::new();
        main.set_register_types(vec![ValueType::Unit, ValueType::Agg, ValueType::I64]);
        main.call(0, make, 0, &[], &[1])
            .tuple_get(1, 1, 0)
            .tuple_get(2, 1, 0)
            .ret(0, &[2]);
        b.define_function(entry, main).unwrap();
        b.set_function_name(entry, "main").unwrap();
    }
    (b.build_checked().unwrap(), make, wrong_outer)
}

#[test]
fn calls_and_returns_distinguish_nested_contracts_from_storage_kinds() {
    let (program, make, _) = nested_fixture(true);
    let mut view = Browser::new(&program);
    let details = render::details(&view).join("\n");
    assert!(
        details.contains("Results on successful return:\n  r1: tuple(tuple(i64), bool)"),
        "{details}"
    );
    assert!(details.contains("Storage kinds (fixed register declarations):"));
    assert!(details.contains("r1: aggregate"));
    press(&mut view, KeyCode::Enter);
    press(&mut view, KeyCode::Down);
    let projection = render::details(&view).join("\n");
    assert!(!projection.contains("Results on successful return:"));
    view.navigate(make, 4);
    let returned = render::details(&view).join("\n");
    assert!(
        returned.contains("Required returned values:\n  r4: tuple(tuple(i64), bool)"),
        "{returned}"
    );
}

#[test]
fn rejection_names_the_function_and_explains_the_nested_member() {
    let (mut program, make, wrong) = nested_fixture(false);
    let offset = program.functions[make.0 as usize].ret_types.offset as usize;
    program.contract_ids[offset] = wrong;
    let view = Browser::new(&program);
    let text = render::details(&view).join("\n");
    assert!(
        text.contains("make_nested: expected bool, found i64 at result[0][0][0] (byte 15)"),
        "{text}"
    );
    assert!(
        text.contains("Return value 1 → tuple element 1 → tuple element 1"),
        "{text}"
    );
    assert!(text.contains("Required returned values:\n  r4: tuple(tuple(bool), bool)"));
    let screen = render_program(&program, 120, 36).unwrap();
    assert!(screen.contains("make_nested: expected bool, found i64"));
    assert!(!screen.contains("Kind {"));
    assert!(!screen.contains("Result {"));
}

#[test]
fn contract_diagnostics_resolve_named_fields_and_bound_deep_paths() {
    use execution_tape::asm::FunctionAbi;
    use execution_tape::contract::{
        Contract, ContractId, ContractMismatch, ContractPosition, ContractViolation,
    };
    use execution_tape::program::StructTypeDef;
    let mut b = ProgramBuilder::new();
    let boolean = b.contract(Contract::Kind(ValueType::Bool)).unwrap();
    let schema = b.struct_type(StructTypeDef {
        field_names: vec!["enabled".into()],
        field_types: vec![ValueType::Bool],
    });
    let settings = b
        .contract(Contract::Struct {
            type_id: schema,
            fields: &[boolean],
        })
        .unwrap();
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::Agg]);
    body.ret(0, &[]);
    let callee = b
        .push_function_checked(
            body,
            FunctionAbi {
                args: vec![settings.into()],
                rets: vec![],
            },
        )
        .unwrap();
    b.set_function_name(callee, "accepts_settings").unwrap();
    let program = b.build_checked().unwrap();
    let error = |path| VerifyError::ContractMismatch {
        func: 0,
        pc: 0,
        position: ContractPosition::Argument { callee, index: 0 },
        error: ContractViolation {
            contract: ContractId(0),
            path,
            mismatch: ContractMismatch::Kind {
                expected: ValueType::Bool,
                actual: ValueType::I64,
            },
        },
    };
    let named = describe::verification(&program, &error(vec![0]));
    assert!(
        named
            .summary
            .contains("Call to accepts_settings: expected bool, found i64 at argument[0][0]")
    );
    assert_eq!(
        named.path.as_deref(),
        Some("Argument 1 to accepts_settings → field enabled")
    );
    let deep = describe::verification(&program, &error(vec![0; 20_000]));
    assert!(deep.summary.contains("expected bool, found i64"));
    assert!(deep.summary.len() < 300);
    let path = deep.path.unwrap();
    assert!(path.contains("19992 more members"));
    assert!(path.len() < 400);
}

#[test]
fn rejected_call_result_metadata_stays_explicit_and_inspectable() {
    use execution_tape::contract::ContractId;
    for mode in 0..3 {
        let (mut program, make, _) = nested_fixture(true);
        let range = &mut program.functions[make.0 as usize].ret_types;
        let expected = match mode {
            0 => {
                range.offset = u32::MAX;
                "<malformed result contract range>"
            }
            1 => {
                program.contract_ids[range.offset as usize] = ContractId(u32::MAX);
                "<invalid contract #4294967295>"
            }
            _ => {
                range.len = 0;
                "Result count mismatch: 1 registers, 0 contracts"
            }
        };
        let view = Browser::new(&program);
        assert!(view.error.is_some());
        let text = render::details(&view).join("\n");
        assert!(text.contains(expected), "{text}");
    }
}
