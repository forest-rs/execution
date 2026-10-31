// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Three independently prepared code domains for one language-owned task.

use execution_tape::{
    asm::{Asm, FunctionSig, ProgramBuilder},
    host::HostSig,
    program::{HostSigId, ValueType},
    source::{SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping},
    value::FuncId,
    verifier::VerifiedProgram,
};
use std::sync::Arc;

pub(super) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(super) struct Compiled {
    pub(super) program: Arc<VerifiedProgram>,
    pub(super) entry: FuncId,
    pub(super) sources: SourceMap,
}

struct Author {
    builder: ProgramBuilder,
    text: String,
    mappings: Vec<SourceMapping>,
}

impl Author {
    fn new(name: &str, text: &str) -> Self {
        let mut builder = ProgramBuilder::new();
        builder.set_program_name(name);
        Self {
            builder,
            text: text.into(),
            mappings: Vec::new(),
        }
    }

    fn import(&mut self, name: &str, args: &[ValueType], rets: &[ValueType]) -> HostSigId {
        self.builder.host_sig_for(
            name,
            HostSig {
                args: args.into(),
                rets: rets.into(),
            },
        )
    }

    fn at(&self, body: &mut Asm, expression: &str) {
        let start = self
            .text
            .find(expression)
            .expect("expression in authored fixture");
        body.source(SourceId(0), start..start + expression.len());
    }

    fn define(&mut self, function: FuncId, name: &str, body: Asm) -> Result<()> {
        self.mappings.extend(body.source_mappings(function));
        self.builder.set_function_name(function, name)?;
        self.builder.define_function(function, body)?;
        Ok(())
    }

    fn finish(self, entry: FuncId, filename: &str) -> Result<Arc<Compiled>> {
        let program = Arc::new(self.builder.build_verified()?);
        let sources = SourceMap::new(
            program.program(),
            vec![SourceDocument {
                name: filename.into(),
                text: self.text,
            }],
            self.mappings,
            SourceLimits::default(),
        )?;
        Ok(Arc::new(Compiled {
            program,
            entry,
            sources,
        }))
    }
}

/// The factory has returned before its block runs, but `run` is still the lexical home.
pub(super) fn home(non_local: bool) -> Result<Arc<Compiled>> {
    let return_expression = if non_local {
        "Runtime return: answer to: home"
    } else {
        "answer"
    };
    let text = format!(
        "run\n    | home block answer |\n    home := Runtime home.\n    block := self makeBlockFor: home.\n    answer := Runtime send: (Runtime retain: block).\n    Runtime afterSend.\n    ^ answer\n\nmakeBlockFor: home\n    ^ [ | answer |\n        answer := Runtime signal: #NeedValue.\n        {return_expression} ]\n"
    );
    let mut author = Author::new("Lexical home and block factory", &text);
    let home = author.import("Runtime.home", &[], &[ValueType::U64]);
    let retain = author.import("Runtime.retain", &[ValueType::Closure], &[ValueType::U64]);
    let send = author.import("Runtime.send", &[ValueType::U64], &[ValueType::I64]);
    let after = author.import("Runtime.afterSend", &[], &[]);
    let signal = author.import("Runtime.signal", &[], &[ValueType::I64]);
    let return_home = author.import(
        "Runtime.return",
        &[ValueType::U64, ValueType::I64],
        &[ValueType::I64],
    );
    let entry = author.builder.declare_function(FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    })?;
    let factory = author.builder.declare_function(FunctionSig {
        arg_types: vec![ValueType::U64],
        ret_types: vec![ValueType::Closure],
    })?;
    let block = author.builder.declare_closure(
        &[ValueType::U64.into()],
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        },
    )?;

    let mut body = author.builder.function_asm(block.function())?;
    author.at(&mut body, "[ | answer |");
    let [lexical_home] = block.load_captures(&mut body)?;
    author.at(&mut body, "Runtime signal: #NeedValue");
    let [answer] = author.builder.emit_call(&mut body, signal, &[])?;
    if non_local {
        author.at(&mut body, return_expression);
        let [returned] =
            author
                .builder
                .emit_call(&mut body, return_home, &[lexical_home, answer])?;
        body.ret(0, &[returned]);
    } else {
        body.ret(0, &[answer]);
    }
    author.define(block.function(), "makeBlockFor: / escaped block", body)?;

    let mut body = author.builder.function_asm(factory)?;
    let lexical_home = author.builder.argument(factory, 0)?;
    author.at(&mut body, "^ [ | answer |");
    let value = block.instantiate(&mut body, &[lexical_home])?;
    body.ret(0, &[value]);
    author.define(factory, "makeBlockFor:", body)?;

    let mut body = author.builder.function_asm(entry)?;
    author.at(&mut body, "Runtime home");
    let [lexical_home] = author.builder.emit_call(&mut body, home, &[])?;
    author.at(&mut body, "self makeBlockFor: home");
    let [block] = author
        .builder
        .emit_call(&mut body, factory, &[lexical_home])?;
    author.at(&mut body, "Runtime retain: block");
    let [token] = author.builder.emit_call(&mut body, retain, &[block])?;
    author.at(&mut body, "Runtime send: (Runtime retain: block)");
    let [answer] = author.builder.emit_call(&mut body, send, &[token])?;
    author.at(&mut body, "Runtime afterSend");
    let [] = author.builder.emit_call(&mut body, after, &[])?;
    author.at(&mut body, "^ answer");
    body.ret(0, &[answer]);
    author.define(entry, "run", body)?;
    author.finish(entry, "home.st")
}

pub(super) fn method(cleanup_marker: i64) -> Result<Arc<Compiled>> {
    let text = format!(
        "through: block\n    | answer |\n    answer := Runtime protect: block\n        ensure: [ Runtime waitForCleanup. Runtime cleaned: {cleanup_marker} ].\n    Runtime afterMethod.\n    ^ answer\n"
    );
    let mut author = Author::new(
        &format!("Protected method / cleanup {cleanup_marker}"),
        &text,
    );
    let protect = author.import(
        "Runtime.protect",
        &[ValueType::U64, ValueType::Closure],
        &[ValueType::I64],
    );
    let wait = author.import("Runtime.waitForCleanup", &[], &[]);
    let cleaned = author.import("Runtime.cleaned", &[ValueType::I64], &[]);
    let after = author.import("Runtime.afterMethod", &[], &[]);
    let entry = author.builder.declare_function(FunctionSig {
        arg_types: vec![ValueType::U64],
        ret_types: vec![ValueType::I64],
    })?;
    let cleanup = author.builder.declare_closure(
        &[],
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![],
        },
    )?;
    let mut body = author.builder.function_asm(cleanup.function())?;
    author.at(&mut body, "Runtime waitForCleanup");
    let [] = author.builder.emit_call(&mut body, wait, &[])?;
    let marker = body.alloc_register(ValueType::I64);
    let expression = format!("Runtime cleaned: {cleanup_marker}");
    author.at(&mut body, &expression);
    body.const_i64(marker, cleanup_marker);
    let [] = author.builder.emit_call(&mut body, cleaned, &[marker])?;
    body.ret(0, &[]);
    author.define(cleanup.function(), "through: / ensure block", body)?;

    let mut body = author.builder.function_asm(entry)?;
    let block = author.builder.argument(entry, 0)?;
    author.at(&mut body, "[ Runtime waitForCleanup.");
    let cleanup = cleanup.instantiate(&mut body, &[])?;
    author.at(&mut body, "Runtime protect: block");
    let [answer] = author
        .builder
        .emit_call(&mut body, protect, &[block, cleanup])?;
    author.at(&mut body, "Runtime afterMethod");
    let [] = author.builder.emit_call(&mut body, after, &[])?;
    author.at(&mut body, "^ answer");
    body.ret(0, &[answer]);
    author.define(entry, "through:", body)?;
    author.finish(entry, "through.st")
}

pub(super) fn handler() -> Result<Arc<Compiled>> {
    let mut author = Author::new(
        "NeedValue handler",
        "handleNeedValue\n    ^ Runtime chooseRecovery\n",
    );
    let recover = author.import("Runtime.chooseRecovery", &[], &[ValueType::I64]);
    let entry = author.builder.declare_function(FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    })?;
    let mut body = author.builder.function_asm(entry)?;
    author.at(&mut body, "Runtime chooseRecovery");
    let [answer] = author.builder.emit_call(&mut body, recover, &[])?;
    body.ret(0, &[answer]);
    author.define(entry, "handleNeedValue", body)?;
    author.finish(entry, "handler.st")
}
