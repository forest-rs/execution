// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Independently compiled definitions with exact authored source, all deliberately using f0.

use execution_tape::{
    asm::{Asm, FunctionSig, ProgramBuilder},
    host::HostSig,
    program::ValueType,
    source::{SourceDocument, SourceId, SourceLimits, SourceMap},
    value::FuncId,
    verifier::VerifiedProgram,
};

pub(super) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(super) struct Compiled {
    pub(super) code: VerifiedProgram,
    pub(super) entry: FuncId,
    pub(super) sources: SourceMap,
}

pub(super) struct Programs {
    pub(super) main: Compiled,
    pub(super) divide: Compiled,
    pub(super) replacement: Compiled,
    pub(super) handler: Compiled,
    pub(super) recovery: Compiled,
    pub(super) restart: Compiled,
    pub(super) cleanup: Compiled,
}

fn source(body: &mut Asm, text: &str, expression: &str) {
    let start = text.find(expression).expect("authored expression");
    body.source(SourceId(0), start..start + expression.len());
}

fn declare(
    name: &str,
    args: Vec<ValueType>,
    rets: Vec<ValueType>,
) -> Result<(ProgramBuilder, FuncId, Asm)> {
    let mut builder = ProgramBuilder::new();
    builder.set_program_name(name);
    let entry = builder.declare_function(FunctionSig {
        arg_types: args,
        ret_types: rets,
    })?;
    builder.set_function_name(entry, name)?;
    let body = builder.function_asm(entry)?;
    Ok((builder, entry, body))
}

fn finish(
    mut builder: ProgramBuilder,
    entry: FuncId,
    body: Asm,
    name: &str,
    text: &str,
) -> Result<Compiled> {
    let mappings = body.source_mappings(entry).collect();
    builder.define_function(entry, body)?;
    let code = builder.build_verified()?;
    let sources = SourceMap::new(
        code.program(),
        vec![SourceDocument {
            name: format!("{name}.guest"),
            text: text.into(),
        }],
        mappings,
        SourceLimits::default(),
    )?;
    Ok(Compiled {
        code,
        entry,
        sources,
    })
}

fn main_program() -> Result<Compiled> {
    let text = "main() {\n    let first = divide(84, 0);\n    let second = divide(84, 0);\n    return first + second;\n}\n";
    let (mut builder, entry, mut body) = declare("main", vec![], vec![ValueType::I64])?;
    let send = builder.host_sig_for(
        "Object.send",
        HostSig {
            args: vec![ValueType::I64, ValueType::I64],
            rets: vec![ValueType::I64],
        },
    );
    let numerator = body.alloc_register(ValueType::I64);
    let zero = body.alloc_register(ValueType::I64);
    source(&mut body, text, "84");
    body.const_i64(numerator, 84);
    source(&mut body, text, "0");
    body.const_i64(zero, 0);
    source(&mut body, text, "let first = divide(84, 0);");
    let [first] = builder.emit_call(&mut body, send, &[numerator, zero])?;
    source(&mut body, text, "let second = divide(84, 0);");
    let [second] = builder.emit_call(&mut body, send, &[numerator, zero])?;
    let sum = body.alloc_register(ValueType::I64);
    source(&mut body, text, "first + second");
    body.i64_add(sum, first, second).ret(0, &[sum]);
    finish(builder, entry, body, "main", text)
}

fn division() -> Result<Compiled> {
    let text = "divide(lhs, rhs) {\n    let divisor = if rhs == 0 {\n        signal ZeroDivisor(lhs, rhs)\n    } else { rhs };\n    return lhs / divisor;\n}\n";
    let (mut builder, entry, mut body) = declare(
        "divide/v1",
        vec![ValueType::I64, ValueType::I64],
        vec![ValueType::I64],
    )?;
    let signal = builder.host_sig_for(
        "Condition.signal.ZeroDivisor",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::I64],
        },
    );
    let lhs = builder.argument(entry, 0)?;
    let rhs = builder.argument(entry, 1)?;
    let zero = body.alloc_register(ValueType::I64);
    let is_zero = body.alloc_register(ValueType::Bool);
    let divisor = body.alloc_register(ValueType::I64);
    let signalling = body.label_named("signal_zero_divisor");
    let calculate = body.block(&[divisor]);
    body.name_label(calculate, "calculate_with_divisor")?;
    source(&mut body, text, "rhs == 0");
    body.const_i64(zero, 0).i64_eq(is_zero, rhs, zero);
    body.br(is_zero, signalling, calculate.with_args(&[rhs]));
    body.place(signalling)?;
    source(&mut body, text, "signal ZeroDivisor(lhs, rhs)");
    let condition = body.alloc_register(ValueType::Agg);
    body.tuple_new(condition, &[lhs, rhs]);
    let [replacement] = builder.emit_call(&mut body, signal, &[condition])?;
    body.jmp(calculate.with_args(&[replacement]));
    body.place(calculate)?;
    let answer = body.alloc_register(ValueType::I64);
    source(&mut body, text, "lhs / divisor");
    body.i64_div(answer, lhs, divisor).ret(0, &[answer]);
    finish(builder, entry, body, "divide-v1", text)
}

fn constant(name: &str, args: Vec<ValueType>, value: i64, text: &str) -> Result<Compiled> {
    let (builder, entry, mut body) = declare(name, args, vec![ValueType::I64])?;
    let answer = body.alloc_register(ValueType::I64);
    source(&mut body, text, &value.to_string());
    body.const_i64(answer, value).ret(0, &[answer]);
    finish(builder, entry, body, name, text)
}

fn handler() -> Result<Compiled> {
    let text =
        "handle ZeroDivisor(condition) {\n    return invoke_restart use_divisor(condition);\n}\n";
    let (mut builder, entry, mut body) = declare(
        "zero_divisor_handler",
        vec![ValueType::Agg],
        vec![ValueType::I64],
    )?;
    let invoke = builder.host_sig_for(
        "Restart.invoke.use_divisor",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::I64],
        },
    );
    let condition = builder.argument(entry, 0)?;
    source(&mut body, text, "invoke_restart use_divisor(condition)");
    let [answer] = builder.emit_call(&mut body, invoke, &[condition])?;
    body.ret(0, &[answer]);
    finish(builder, entry, body, "handler", text)
}

fn cleanup() -> Result<Compiled> {
    let text = "ensure_cleanup() {\n    native_cleanup();\n}\n";
    let (mut builder, entry, mut body) = declare("ensure_cleanup", vec![], vec![])?;
    let cleanup = builder.host_sig_for(
        "native.cleanup",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    source(&mut body, text, "native_cleanup()");
    let [] = builder.emit_call(&mut body, cleanup, &[])?;
    body.ret(0, &[]);
    finish(builder, entry, body, "cleanup", text)
}

impl Programs {
    pub(super) fn build() -> Result<Self> {
        Ok(Self {
            main: main_program()?,
            divide: division()?,
            replacement: constant(
                "divide/v2",
                vec![ValueType::I64, ValueType::I64],
                99,
                "divide(lhs, rhs) { return 99; }\n",
            )?,
            handler: handler()?,
            recovery: constant(
                "recover_zero_divisor",
                vec![ValueType::Agg],
                2,
                "handle ZeroDivisor(condition) { resume 2; }\n",
            )?,
            restart: constant(
                "use_divisor",
                vec![ValueType::Agg],
                2,
                "restart use_divisor(condition) { return 2; }\n",
            )?,
            cleanup: cleanup()?,
        })
    }

    pub(super) fn all(&self) -> [&Compiled; 7] {
        [
            &self.main,
            &self.divide,
            &self.replacement,
            &self.handler,
            &self.recovery,
            &self.restart,
            &self.cleanup,
        ]
    }
}
