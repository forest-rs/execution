// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Source attribution is explicit, validated and isolated to its program instance.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    program::{ConstEntry, ValueType},
    source::{
        SourceDocument, SourceError, SourceId, SourceLimits, SourceMap, SourceMapping,
        SourcePosition,
    },
    trace::TraceLocation,
};
use execution_tape_tracy::ProfilingTraceSink;

#[test]
fn source_binding_is_exact_and_queryable_without_a_tracy_client() {
    let mut builder = ProgramBuilder::new();
    let function = builder
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut body = builder.function_asm(function).unwrap();
    let value = body.alloc_register(ValueType::I64);
    let location = TraceLocation {
        func: function,
        pc: body.pc(),
    };
    body.const_i64(value, 42);
    let unmapped = TraceLocation {
        func: function,
        pc: body.pc(),
    };
    body.ret(0, &[value]);
    builder.define_function(function, body).unwrap();
    let program = builder.build_verified().unwrap();
    let map = SourceMap::new(
        program.program(),
        vec![SourceDocument {
            name: "authored.expr".into(),
            text: "header\né = 42".into(),
        }],
        vec![SourceMapping {
            location,
            document: SourceId(0),
            range: 12..14,
        }],
        SourceLimits::default(),
    )
    .unwrap();
    let sink = ProfilingTraceSink::new()
        .sources(program.program(), &map)
        .unwrap();
    let source = sink.source_location(program.program(), location).unwrap();
    assert_eq!(source.document().name, "authored.expr");
    assert_eq!(
        source.start(),
        SourcePosition {
            line: 2,
            byte_column: 6
        }
    );
    assert_eq!(source.text(), "42");
    assert!(sink.source_location(program.program(), unmapped).is_none());
    let same_content = program.program().clone();
    assert!(
        sink.source_location(&same_content, location).is_none(),
        "another program instance must use bytecode fallback until explicitly bound"
    );
    let rebound = ProfilingTraceSink::new()
        .sources(&same_content, &map)
        .unwrap();
    assert_eq!(
        rebound
            .source_location(&same_content, location)
            .unwrap()
            .text(),
        "42"
    );
    let mut stale = same_content.clone();
    stale.const_pool.push(ConstEntry::I64(99));
    assert!(matches!(
        ProfilingTraceSink::new().sources(&stale, &map),
        Err(SourceError::ProgramMismatch)
    ));
}
