// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Hand-lowering of the accompanying Smalltalk-style story, with exact source snapshots.
//! This is a control-flow fixture, not a Smalltalk parser or general object representation.

use execution_tape::{
    asm::{Asm, CallTarget, ClosureLayout, FunctionAbi, FunctionSig, ProgramBuilder},
    contract::Contract,
    host::HostSig,
    program::{HostSigId, HostTypeId, ValueType},
    source::{SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping},
    value::FuncId,
    verifier::VerifiedProgram,
};
use std::{ops::Range, sync::Arc};

pub(super) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub(super) const CELL: HostTypeId = HostTypeId(1);
pub(super) const HOME: HostTypeId = HostTypeId(2);
const STORY: &str = include_str!("story.st");

pub(super) struct Fixture {
    pub(super) program: Arc<VerifiedProgram>,
    pub(super) entry: FuncId,
    pub(super) sources: SourceMap,
}
struct Imports {
    new_cell: HostSigId,
    require_block: HostSigId,
    read: HostSigId,
    write: HostSigId,
    cleaned: HostSigId,
    wait: HostSigId,
    after_ensure: HostSigId,
    home: HostSigId,
    ensure: HostSigId,
    return_home: HostSigId,
}
struct Author {
    program: ProgramBuilder,
    source: String,
    mappings: Vec<SourceMapping>,
}
impl Author {
    fn at(&self, body: &mut Asm, method: &str, expression: &str) {
        let start = self
            .source
            .find(&format!("\n{method}\n"))
            .expect("authored method")
            + 1;
        let method_end = self.source[start..]
            .find("\n\n")
            .map_or(self.source.len(), |end| start + end);
        let offset = self.source[start..method_end]
            .find(expression)
            .expect("authored expression");
        let range: Range<usize> = start + offset..start + offset + expression.len();
        body.source(SourceId(0), range);
    }
    fn define(&mut self, function: FuncId, name: &str, body: Asm) -> Result<()> {
        self.mappings.extend(body.source_mappings(function));
        self.program.set_function_name(function, name)?;
        self.program.define_function(function, body)?;
        Ok(())
    }
    fn integer(body: &mut Asm, value: i64) -> u32 {
        let register = body.alloc_register(ValueType::I64);
        body.const_i64(register, value);
        register
    }
    fn return_block(
        &mut self,
        imports: &Imports,
        method: &str,
        non_local: bool,
    ) -> Result<ClosureLayout> {
        let block = self.program.declare_closure(
            &[ValueType::Obj(HOME).into(), ValueType::Obj(CELL).into()],
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )?;
        let mut body = self.program.function_asm(block.function())?;
        let literal = if non_local {
            "[ ^ counter value ]"
        } else {
            "[ counter value ]"
        };
        self.at(&mut body, method, literal);
        let [home, counter] = block.load_captures(&mut body)?;
        let [value] = self
            .program
            .emit_call(&mut body, imports.read, &[counter])?;
        let answer = if non_local {
            let [answer] =
                self.program
                    .emit_call(&mut body, imports.return_home, &[home, value])?;
            answer
        } else {
            value
        };
        body.ret(0, &[answer]);
        self.define(
            block.function(),
            &format!(
                "{} / return block",
                method.split_whitespace().next().expect("method selector")
            ),
            body,
        )?;
        Ok(block)
    }
    fn cleanup(&mut self, imports: &Imports, tag: i64, waits: bool) -> Result<ClosureLayout> {
        let cleanup = self.program.declare_closure(
            &[ValueType::Obj(CELL).into()],
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            },
        )?;
        let mut body = self.program.function_asm(cleanup.function())?;
        self.at(
            &mut body,
            "answerUsing: counter",
            &format!("counter cleaned: {tag}"),
        );
        let [counter] = cleanup.load_captures(&mut body)?;
        if waits {
            self.at(&mut body, "answerUsing: counter", "Demo wait");
            let [] = self.program.emit_call(&mut body, imports.wait, &[])?;
        }
        self.at(
            &mut body,
            "answerUsing: counter",
            &format!("counter cleaned: {tag}"),
        );
        let tag_value = Self::integer(&mut body, tag);
        let [] = self
            .program
            .emit_call(&mut body, imports.cleaned, &[counter, tag_value])?;
        body.ret(0, &[]);
        self.define(cleanup.function(), &format!("ensure / cleanup {tag}"), body)?;
        Ok(cleanup)
    }
}

pub(super) fn compile(non_local: bool) -> Result<Fixture> {
    let source = if non_local {
        STORY.to_string()
    } else {
        STORY.replacen("[ ^ counter value ]", "[ counter value ]", 1)
    };
    let mut author = Author {
        program: ProgramBuilder::new(),
        source,
        mappings: Vec::new(),
    };
    author
        .program
        .set_program_name("Blocks, lexical homes and shared captures");
    let mut import = |name: &str, args: &[ValueType], rets: &[ValueType]| {
        author.program.host_sig_for(
            name,
            HostSig {
                args: args.to_vec(),
                rets: rets.to_vec(),
            },
        )
    };
    let imports = Imports {
        require_block: import(
            "Object.requireBlock",
            &[ValueType::Agg],
            &[ValueType::Closure],
        ),
        new_cell: import("Cell.new", &[ValueType::I64], &[ValueType::Obj(CELL)]),
        read: import("Cell.read", &[ValueType::Obj(CELL)], &[ValueType::I64]),
        write: import(
            "Cell.write",
            &[ValueType::Obj(CELL), ValueType::I64],
            &[ValueType::I64],
        ),
        cleaned: import("Cell.cleaned", &[ValueType::Obj(CELL), ValueType::I64], &[]),
        wait: import("Demo.wait", &[], &[]),
        after_ensure: import("Demo.afterEnsure", &[], &[]),
        home: import("Block.home", &[], &[ValueType::Obj(HOME)]),
        ensure: import(
            "Block.ensure",
            &[ValueType::Closure, ValueType::Closure],
            &[ValueType::I64],
        ),
        return_home: import(
            "Block.return",
            &[ValueType::Obj(HOME), ValueType::I64],
            &[ValueType::I64],
        ),
    };
    let integer = author.program.contract(Contract::Kind(ValueType::I64))?;
    let scalar_box = author.program.contract(Contract::Tuple(&[Some(integer)]))?;
    let object_box = author.program.contract(Contract::Tuple(&[None]))?;
    let answers = author
        .program
        .contract(Contract::Tuple(&[Some(integer); 4]))?;

    // A closure factory runs twice. Distinct environments retain the same mutable cell identity.
    let increment = author.program.declare_closure(
        &[ValueType::Obj(CELL).into()],
        FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        },
    )?;
    let mut body = author.program.function_asm(increment.function())?;
    author.at(
        &mut body,
        "makeIncrement: counter",
        "[ :amount | counter value: (counter value + amount) ]",
    );
    let [counter] = increment.load_captures(&mut body)?;
    let amount = increment.argument(0)?;
    let [previous] = author
        .program
        .emit_call(&mut body, imports.read, &[counter])?;
    let total = body.alloc_register(ValueType::I64);
    body.i64_add(total, previous, amount);
    let [answer] = author
        .program
        .emit_call(&mut body, imports.write, &[counter, total])?;
    body.ret(0, &[answer]);
    author.define(
        increment.function(),
        "makeIncrement: / increment block",
        body,
    )?;
    let factory = author.program.declare_function(FunctionSig {
        arg_types: vec![ValueType::Obj(CELL)],
        ret_types: vec![ValueType::Closure],
    })?;
    let mut body = author.program.function_asm(factory)?;
    author.at(
        &mut body,
        "makeIncrement: counter",
        "^ [ :amount | counter value: (counter value + amount) ]",
    );
    let counter = author.program.argument(factory, 0)?;
    let block = increment.instantiate(&mut body, &[counter])?;
    body.ret(0, &[block]);
    author.define(factory, "makeIncrement:", body)?;

    // Both normal and non-local completion must pass through the same nested ensure protocol.
    let return_block = author.return_block(&imports, "answerUsing: counter", non_local)?;
    let inner_cleanup = author.cleanup(&imports, 2, true)?;
    let outer_cleanup = author.cleanup(&imports, 1, false)?;
    let outer_body = author.program.declare_closure(
        &[ValueType::Closure.into(); 2],
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        },
    )?;
    let mut body = author.program.function_asm(outer_body.function())?;
    author.at(
        &mut body,
        "answerUsing: counter",
        "ensure: [ Demo wait. counter cleaned: 2 ]",
    );
    let [protected_block, cleanup] = outer_body.load_captures(&mut body)?;
    let [answer] =
        author
            .program
            .emit_call(&mut body, imports.ensure, &[protected_block, cleanup])?;
    body.ret(0, &[answer]);
    author.define(outer_body.function(), "ensure / outer body", body)?;

    // Method result boxes give both scalar and block objects the same aggregate storage kind.
    // makeEscaped: can answer either a block normally or an integer via its lexical return.
    let answer_method = author.program.declare_function(FunctionAbi {
        args: vec![ValueType::Obj(CELL).into()],
        rets: vec![scalar_box.into()],
    })?;
    let mut body = author.program.function_asm(answer_method)?;
    author.at(&mut body, "answerUsing: counter", "answerUsing: counter");
    let counter = author.program.argument(answer_method, 0)?;
    let [home] = author.program.emit_call(&mut body, imports.home, &[])?;
    let protected_block = return_block.instantiate(&mut body, &[home, counter])?;
    let inner_cleanup = inner_cleanup.instantiate(&mut body, &[counter])?;
    let outer_cleanup = outer_cleanup.instantiate(&mut body, &[counter])?;
    let protected = outer_body.instantiate(&mut body, &[protected_block, inner_cleanup])?;
    author.at(
        &mut body,
        "answerUsing: counter",
        "ensure: [ counter cleaned: 1 ]",
    );
    let [answer] =
        author
            .program
            .emit_call(&mut body, imports.ensure, &[protected, outer_cleanup])?;
    author.at(&mut body, "answerUsing: counter", "Demo afterEnsure");
    let [] = author
        .program
        .emit_call(&mut body, imports.after_ensure, &[])?;
    author.at(&mut body, "answerUsing: counter", "^ answer");
    let boxed = body.alloc_register(ValueType::Agg);
    body.tuple_new(boxed, &[answer]).ret(0, &[boxed]);
    author.define(answer_method, "answerUsing:", body)?;

    let escaped = author.return_block(&imports, "makeEscaped: counter", true)?;
    let make_escaped = author.program.declare_function(FunctionAbi {
        args: vec![ValueType::Obj(CELL).into()],
        rets: vec![object_box.into()],
    })?;
    let mut body = author.program.function_asm(make_escaped)?;
    author.at(&mut body, "makeEscaped: counter", "makeEscaped: counter");
    let counter = author.program.argument(make_escaped, 0)?;
    let [home] = author.program.emit_call(&mut body, imports.home, &[])?;
    author.at(&mut body, "makeEscaped: counter", "^ [ ^ counter value ]");
    let block = escaped.instantiate(&mut body, &[home, counter])?;
    let boxed = body.alloc_register(ValueType::Agg);
    body.tuple_new(boxed, &[block]).ret(0, &[boxed]);
    author.define(make_escaped, "makeEscaped:", body)?;

    let entry = author.program.declare_function(FunctionAbi {
        args: vec![],
        rets: vec![answers.into()],
    })?;
    let mut body = author.program.function_asm(entry)?;
    author.at(&mut body, "run", "Cell with: 40");
    let forty = Author::integer(&mut body, 40);
    let [counter] = author
        .program
        .emit_call(&mut body, imports.new_cell, &[forty])?;
    author.at(&mut body, "run", "left := self makeIncrement: counter");
    let [left] = author.program.emit_call(&mut body, factory, &[counter])?;
    author.at(&mut body, "run", "right := self makeIncrement: counter");
    let [right] = author.program.emit_call(&mut body, factory, &[counter])?;
    author.at(&mut body, "run", "left value: 1");
    let one = Author::integer(&mut body, 1);
    let [first] = author.program.emit_call(
        &mut body,
        CallTarget::Indirect {
            signature: increment.call_signature(),
            callee: left,
        },
        &[one],
    )?;
    author.at(&mut body, "run", "right value: 1");
    let [second] = author.program.emit_call(
        &mut body,
        CallTarget::Indirect {
            signature: increment.call_signature(),
            callee: right,
        },
        &[one],
    )?;
    author.at(&mut body, "run", "self answerUsing: counter");
    let [boxed_answer] = author
        .program
        .emit_call(&mut body, answer_method, &[counter])?;
    let answer = body.alloc_register(ValueType::I64);
    body.tuple_get(answer, boxed_answer, 0);
    author.at(&mut body, "run", "self makeEscaped: counter");
    let [boxed_block] = author
        .program
        .emit_call(&mut body, make_escaped, &[counter])?;
    let [escaped_block] =
        author
            .program
            .emit_call(&mut body, imports.require_block, &[boxed_block])?;
    author.at(&mut body, "run", "escaped value");
    let [late] = author.program.emit_call(
        &mut body,
        CallTarget::Indirect {
            signature: escaped.call_signature(),
            callee: escaped_block,
        },
        &[],
    )?;
    author.at(&mut body, "run", "^ { first. second. answer. late }");
    let results = body.alloc_register(ValueType::Agg);
    body.tuple_new(results, &[first, second, answer, late])
        .ret(0, &[results]);
    author.define(entry, "run", body)?;
    let program = Arc::new(author.program.build_verified()?);
    let sources = SourceMap::new(
        program.program(),
        vec![SourceDocument {
            name: "story.st (assembled fixture)".into(),
            text: author.source,
        }],
        author.mappings,
        SourceLimits::default(),
    )?;
    Ok(Fixture {
        program,
        entry,
        sources,
    })
}
