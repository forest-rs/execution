// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A frontend and headless consumer use source connections through the core API alone.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    program::{ConstEntry, Program, ValueType},
    source::{
        SourceDocument, SourceError, SourceId, SourceLimits, SourceMap, SourceMapping,
        SourcePosition,
    },
    trace::TraceLocation,
};

#[test]
fn headless_source_lookup_borrows_exact_text_and_byte_coordinates() {
    let mut builder = ProgramBuilder::new();
    let function = builder
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut body = builder.function_asm(function).unwrap();
    let result = body.alloc_register(ValueType::I64);
    let mapped = TraceLocation {
        func: function,
        pc: body.pc(),
    };
    body.const_i64(result, 42);
    let unmapped = TraceLocation {
        func: function,
        pc: body.pc(),
    };
    body.ret(0, &[result]);
    builder.define_function(function, body).unwrap();
    let program = builder.build_verified().unwrap();
    let snapshot = "heading\né界 = 42\n";
    let start = snapshot.find('界').unwrap();
    let map = SourceMap::new(
        program.program(),
        vec![SourceDocument {
            name: "logical/source.expr".into(),
            text: snapshot.into(),
        }],
        vec![SourceMapping {
            location: mapped,
            document: SourceId(0),
            range: start..start + "界".len(),
        }],
        SourceLimits::default(),
    )
    .unwrap();
    let encoded = map.encode();
    let decoded = SourceMap::decode(&encoded, SourceLimits::default()).unwrap();
    assert_eq!(decoded.encode(), encoded);
    let same_program = Program::decode(&program.program().encode()).unwrap();
    let bound = decoded.bind(&same_program).unwrap();
    let source = bound.lookup(mapped).unwrap();
    assert_eq!(source.text(), "界");
    assert_eq!(
        source.text().as_ptr(),
        source.document().text[start..].as_ptr()
    );
    assert_eq!(source.document().name, "logical/source.expr");
    assert_eq!(source.range(), start..start + 3);
    assert_eq!(
        source.start(),
        SourcePosition {
            line: 2,
            byte_column: 3
        }
    );
    assert_eq!(
        source.end(),
        SourcePosition {
            line: 2,
            byte_column: 6
        }
    );
    let context = bound.context(mapped, 1, 24).unwrap();
    assert_eq!(context.start, source.start());
    assert_eq!(context.end, source.end());
    assert_eq!(context.lines[0].selected, "界");
    assert!(
        bound.lookup(unmapped).is_none(),
        "unmapped instructions must not inherit source from their neighbors"
    );
    let mut changed = same_program;
    changed.const_pool.push(ConstEntry::I64(99));
    assert!(
        matches!(decoded.bind(&changed), Err(SourceError::ProgramMismatch)),
        "even a changed constant must reject stale source identity"
    );
}
