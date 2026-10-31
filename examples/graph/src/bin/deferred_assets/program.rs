// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The example's tiny frontend translates these two authored operations directly to tape.

use execution_tape::{
    asm::{Asm, FunctionAbi, ProgramBuilder},
    callable::Callable,
    contract::Contract,
    host::HostSig,
    program::{ElemTypeId, HostTypeId, StructTypeDef, TypeId, ValueType},
    source::{SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping},
    trace::TraceLocation,
};
use std::{error::Error, sync::Arc};

pub(super) const SOURCE: &str = "points = asset.load_points(asset)\nreturn points\n";
pub(super) const POINTS_TYPE: HostTypeId = HostTypeId(1);
#[derive(Clone)]
pub(super) struct Compiled {
    pub target: Callable,
    pub sources: Arc<SourceMap>,
    pub schema: TypeId,
    pub preview: ElemTypeId,
}
pub(super) fn compile() -> Result<Compiled, Box<dyn Error>> {
    let mut b = ProgramBuilder::new();
    b.set_program_name("Deferred point-cloud asset");
    let schema = b.struct_type(StructTypeDef {
        field_names: vec![
            "object".into(),
            "vertex_count".into(),
            "preview_xyz".into(),
            "asset".into(),
        ],
        field_types: vec![
            ValueType::Obj(POINTS_TYPE),
            ValueType::U64,
            ValueType::Agg,
            ValueType::Str,
        ],
    });
    let preview = b.array_elem(ValueType::I64);
    let integer = b.contract(Contract::Kind(ValueType::I64))?;
    let preview_contract = b.contract(Contract::Array {
        elem_type_id: preview,
        element: integer,
    })?;
    let object = b.contract(Contract::Kind(ValueType::Obj(POINTS_TYPE)))?;
    let count = b.contract(Contract::Kind(ValueType::U64))?;
    let text = b.contract(Contract::Kind(ValueType::Str))?;
    let result = b.contract(Contract::Struct {
        type_id: schema,
        fields: &[object, count, preview_contract, text],
    })?;
    let import = b.host_sig_for(
        "asset.load_points",
        HostSig {
            args: vec![ValueType::Str],
            rets: vec![ValueType::Agg],
        },
    );
    let mut a = Asm::new();
    a.set_register_types(vec![ValueType::Unit, ValueType::Str, ValueType::Agg]);
    let host_pc = a.pc();
    a.host_call_sig_id(0, import, 0, &[1], &[2]);
    let return_pc = a.pc();
    a.ret(0, &[2]);
    let entry = b.push_function_checked(
        a,
        FunctionAbi {
            args: vec![ValueType::Str.into()],
            rets: vec![result.into()],
        },
    )?;
    b.set_function_name(entry, "cook_asset")?;
    b.set_function_output_name(entry, 0, "points")?;
    let program = Arc::new(b.build_verified()?);
    let split = SOURCE.find('\n').expect("source newline");
    let sources = Arc::new(SourceMap::new(
        program.program(),
        vec![SourceDocument {
            name: "points.scene".into(),
            text: SOURCE.into(),
        }],
        vec![
            SourceMapping {
                location: TraceLocation {
                    func: entry,
                    pc: host_pc,
                },
                document: SourceId(0),
                range: 0..split,
            },
            SourceMapping {
                location: TraceLocation {
                    func: entry,
                    pc: return_pc,
                },
                document: SourceId(0),
                range: split + 1..SOURCE.len() - 1,
            },
        ],
        SourceLimits::default(),
    )?);
    Ok(Compiled {
        target: Callable::function(program, entry)?,
        sources,
        schema,
        preview,
    })
}
