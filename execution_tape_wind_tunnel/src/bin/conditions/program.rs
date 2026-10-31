// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A tiny expression frontend for the language protocol. It compiles an authored Rust AST and
//! supplies its formatted expression source as a sidecar; it does not parse a language or map
//! guest operations to this compiler's Rust implementation source.

use super::language::{Class, Policy, RESTART_TOKEN, RestartMode};
use execution_tape::{
    asm::{Asm, CallTarget, ClosureLayout, ContractRef, FunctionAbi, ProgramBuilder},
    contract::{Contract, ContractId},
    host::HostSig,
    program::{HostSigId, ValueType},
    source::{SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping},
    value::FuncId,
    verifier::VerifiedProgram,
};
use std::{collections::BTreeMap, ops::Range, sync::Arc};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Debug)]
pub(super) enum Term {
    Constant(i64),
    Capture,
    Payload,
    Argument,
    Add(Box<Term>, Box<Term>),
}
impl Term {
    pub(super) fn plus(self, other: Self) -> Self {
        Self::Add(Box::new(self), Box::new(other))
    }
}
impl std::fmt::Display for Term {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Constant(value) => value.fmt(f),
            Self::Capture => f.write_str("captured"),
            Self::Payload => f.write_str("condition.payload"),
            Self::Argument => f.write_str("argument"),
            Self::Add(left, right) => write!(f, "({left} + {right})"),
        }
    }
}
#[derive(Clone, Debug)]
pub(super) enum Expr {
    Value(Term, bool),
    Signal(Class, Term),
    IfClass {
        class: Class,
        yes: Box<Self>,
        no: Box<Self>,
    },
    Handle {
        class: Class,
        policy: Policy,
        accepted: bool,
        handler: Box<Self>,
        body: Box<Self>,
    },
    Restart {
        name: i64,
        mode: RestartMode,
        class: Class,
        function: Box<Self>,
        body: Box<Self>,
    },
    Protect {
        cleanup: Box<Self>,
        body: Box<Self>,
    },
    Invoke(i64, Term),
    Remember(i64),
    InvokeSaved(Term),
    Sequence(Vec<Self>),
    Call(&'static str, Box<Self>),
    Emit(i64, Box<Self>),
    Wait(Box<Self>),
}
impl Expr {
    pub(super) fn value(value: i64) -> Self {
        Self::Value(Term::Constant(value), true)
    }
    pub(super) fn handle(self, class: Class, policy: Policy, handler: Self) -> Self {
        Self::Handle {
            class,
            policy,
            accepted: true,
            handler: Box::new(handler),
            body: Box::new(self),
        }
    }
    pub(super) fn protect(self, cleanup: Self) -> Self {
        Self::Protect {
            cleanup: Box::new(cleanup),
            body: Box::new(self),
        }
    }
    pub(super) fn restart(self, name: i64, mode: RestartMode, function: Self) -> Self {
        Self::Restart {
            name,
            mode,
            class: Class::Condition,
            function: Box::new(function),
            body: Box::new(self),
        }
    }
    pub(super) fn wait(self) -> Self {
        Self::Wait(Box::new(self))
    }
    pub(super) fn emit(self, value: i64) -> Self {
        Self::Emit(value, Box::new(self))
    }
    pub(super) fn call(self, name: &'static str) -> Self {
        Self::Call(name, Box::new(self))
    }
    fn label(&self) -> String {
        match self {
            Self::Value(term, flag) => format!("({term}, {flag})"),
            Self::Signal(class, term) => format!("signal {class:?}({term})"),
            Self::IfClass { class, .. } => format!("if current condition is {class:?}"),
            Self::Handle {
                class,
                policy,
                accepted,
                ..
            } => format!("handle {class:?} [{policy:?}, accept={accepted}]"),
            Self::Restart {
                name, mode, class, ..
            } => format!("restart {} [{mode:?}, {class:?}]", restart_name(*name)),
            Self::Protect { .. } => {
                "protect body; cleanup on normal or non-local completion".into()
            }
            Self::Invoke(name, argument) => format!("invoke {}({argument})", restart_name(*name)),
            Self::Remember(name) => format!("retain token for {}", restart_name(*name)),
            Self::InvokeSaved(argument) => format!("invoke retained token({argument})"),
            Self::Sequence(_) => "sequence".into(),
            Self::Call(name, _) => format!("call {name}"),
            Self::Emit(value, _) => format!("emit {value}"),
            Self::Wait(_) => "await host work".into(),
        }
    }
}
pub(super) fn restart_name(name: i64) -> String {
    match name {
        1 => "use-value".into(),
        2 => "retry".into(),
        3 => "normal-function".into(),
        4 => "local-exit".into(),
        _ => format!("restart-{name}"),
    }
}
pub(super) struct Fixture {
    pub(super) program: Arc<VerifiedProgram>,
    pub(super) entry: FuncId,
    pub(super) sources: SourceMap,
}
#[derive(Clone, Copy)]
enum Kind {
    Entry,
    Body,
    Handler { accepted: bool },
    Restart,
    Cleanup,
}
#[derive(Clone, Copy)]
struct Context {
    capture: u32,
    class: u32,
    payload: u32,
    argument: u32,
}
#[derive(Clone, Copy)]
struct Services {
    handle: HostSigId,
    restart: HostSigId,
    protect: HostSigId,
    signal: HostSigId,
    find: HostSigId,
    invoke: HostSigId,
    emit: HostSigId,
    wait: HostSigId,
    save: HostSigId,
    load: HostSigId,
}
struct Compiler {
    builder: ProgramBuilder,
    closures: BTreeMap<u32, ClosureLayout>,
    integer: ContractId,
    boolean: ContractId,
    services: Services,
    source: String,
    mappings: Vec<SourceMapping>,
    capture: i64,
}
impl Compiler {
    fn new(capture: i64) -> Result<Self> {
        let mut builder = ProgramBuilder::new();
        builder.set_program_name("Language conditions and scoped recovery");
        let integer = builder.contract(Contract::Kind(ValueType::I64))?;
        let boolean = builder.contract(Contract::Kind(ValueType::Bool))?;
        let pair = [ValueType::I64, ValueType::Bool];
        let mut service = |name: &str, args: &[ValueType], rets: &[ValueType]| {
            builder.host_sig_for(
                name,
                HostSig {
                    args: args.to_vec(),
                    rets: rets.to_vec(),
                },
            )
        };
        let services = Services {
            handle: service(
                "condition.handle",
                &[
                    ValueType::Closure,
                    ValueType::Closure,
                    ValueType::I64,
                    ValueType::U64,
                ],
                &pair,
            ),
            restart: service(
                "condition.restart_bind",
                &[
                    ValueType::Closure,
                    ValueType::Closure,
                    ValueType::I64,
                    ValueType::U64,
                    ValueType::I64,
                ],
                &pair,
            ),
            protect: service(
                "condition.protect",
                &[ValueType::Closure, ValueType::Closure],
                &pair,
            ),
            signal: service("condition.signal", &[ValueType::I64, ValueType::I64], &pair),
            find: service(
                "condition.find",
                &[ValueType::I64, ValueType::I64, ValueType::I64],
                &[ValueType::Bool, ValueType::Obj(RESTART_TOKEN)],
            ),
            invoke: service(
                "condition.invoke",
                &[ValueType::Obj(RESTART_TOKEN), ValueType::I64],
                &pair,
            ),
            emit: service("demo.emit", &[ValueType::I64], &[]),
            wait: service("demo.wait", &[], &[]),
            save: service("demo.save_restart", &[ValueType::Obj(RESTART_TOKEN)], &[]),
            load: service("demo.load_restart", &[], &[ValueType::Obj(RESTART_TOKEN)]),
        };
        Ok(Self {
            builder,
            closures: BTreeMap::new(),
            integer,
            boolean,
            services,
            source: String::new(),
            mappings: Vec::new(),
            capture,
        })
    }
    fn line(&mut self, indent: usize, text: &str) -> Range<usize> {
        self.source.push_str(&"  ".repeat(indent));
        let start = self.source.len();
        self.source.push_str(text);
        let end = self.source.len();
        self.source.push('\n');
        start..end
    }
    fn mark(&self, body: &mut Asm, range: &Range<usize>) {
        body.source(SourceId(0), range.clone());
    }
    fn integer(a: &mut Asm, value: i64) -> u32 {
        let register = a.alloc_register(ValueType::I64);
        a.const_i64(register, value);
        register
    }
    fn context(&mut self, function: FuncId, kind: Kind, body: &mut Asm) -> Result<Context> {
        if matches!(kind, Kind::Entry) {
            return Ok(Context {
                capture: Self::integer(body, self.capture),
                class: Self::integer(body, -1),
                payload: Self::integer(body, 0),
                argument: Self::integer(body, 0),
            });
        }
        let closure = &self.closures[&function.0];
        let [capture, mut class, mut payload, mut argument] = closure.load_captures(body)?;
        if matches!(kind, Kind::Handler { .. }) {
            class = closure.argument(0)?;
            payload = closure.argument(1)?;
        }
        if matches!(kind, Kind::Restart) {
            argument = closure.argument(0)?;
        }
        Ok(Context {
            capture,
            class,
            payload,
            argument,
        })
    }
    fn environment(a: &mut Asm, context: Context) -> u32 {
        let env = a.alloc_register(ValueType::Agg);
        a.tuple_new(
            env,
            &[
                context.capture,
                context.class,
                context.payload,
                context.argument,
            ],
        );
        env
    }
    fn closure(&self, body: &mut Asm, context: Context, function: FuncId) -> Result<u32> {
        Ok(self.closures[&function.0].instantiate(
            body,
            &[
                context.capture,
                context.class,
                context.payload,
                context.argument,
            ],
        )?)
    }
    fn function(&mut self, role: &str, kind: Kind, expr: &Expr, indent: usize) -> Result<FuncId> {
        let mut args: Vec<ContractRef> = vec![];
        if matches!(kind, Kind::Handler { .. }) {
            args.push(self.integer.into());
            args.push(self.integer.into());
        }
        if matches!(kind, Kind::Restart) {
            args.push(self.integer.into());
        }
        let rets = match kind {
            Kind::Handler { .. } => vec![
                self.boolean.into(),
                self.integer.into(),
                self.boolean.into(),
            ],
            Kind::Cleanup => vec![],
            _ => vec![self.integer.into(), self.boolean.into()],
        };
        let abi = FunctionAbi { args, rets };
        let function = if matches!(kind, Kind::Entry) {
            self.builder.declare_function(abi)?
        } else {
            let closure = self
                .builder
                .declare_closure(&[self.integer.into(); 4], abi)?;
            let function = closure.function();
            self.closures.insert(function.0, closure);
            function
        };
        let name = if matches!(kind, Kind::Entry) {
            role.to_string()
        } else {
            format!("{role}#{}", function.0)
        };
        self.builder.set_function_name(function, &name)?;
        let header = self.line(indent, &format!("fn {name}:"));
        let mut a = self.builder.function_asm(function)?;
        a.source(SourceId(0), header);
        let context = self.context(function, kind, &mut a)?;
        let body_start = self.source.len() + 2 * (indent + 1);
        let body_range = body_start..body_start + expr.label().len();
        let (value, flag) = self.expression(&mut a, context, expr, indent + 1)?;
        self.mark(&mut a, &body_range);
        match kind {
            Kind::Handler { accepted } => {
                let accept = a.alloc_register(ValueType::Bool);
                a.const_bool(accept, accepted)
                    .ret(0, &[accept, value, flag]);
            }
            Kind::Cleanup => {
                a.ret(0, &[]);
            }
            _ => {
                a.ret(0, &[value, flag]);
            }
        }
        self.mappings.extend(a.source_mappings(function));
        self.builder.define_function(function, a)?;
        Ok(function)
    }
    fn term(a: &mut Asm, context: Context, term: &Term) -> u32 {
        match term {
            Term::Constant(value) => Self::integer(a, *value),
            Term::Capture => context.capture,
            Term::Payload => context.payload,
            Term::Argument => context.argument,
            Term::Add(left, right) => {
                let left = Self::term(a, context, left);
                let right = Self::term(a, context, right);
                let result = a.alloc_register(ValueType::I64);
                a.i64_add(result, left, right);
                result
            }
        }
    }
    fn lookup(
        &self,
        body: &mut Asm,
        context: Context,
        name: i64,
        range: &Range<usize>,
    ) -> Result<u32> {
        let name = Self::integer(body, name);
        self.mark(body, range);
        let [_found, token] = self.builder.emit_call(
            body,
            self.services.find,
            &[name, context.class, context.payload],
        )?;
        Ok(token)
    }
    fn pair_call(
        &self,
        body: &mut Asm,
        target: impl Into<CallTarget>,
        args: &[u32],
        range: &Range<usize>,
    ) -> Result<(u32, u32)> {
        self.mark(body, range);
        let [value, flag] = self.builder.emit_call(body, target, args)?;
        Ok((value, flag))
    }
    fn value_pair(body: &mut Asm, value: u32, flag: bool) -> (u32, u32) {
        let result_flag = body.alloc_register(ValueType::Bool);
        body.const_bool(result_flag, flag);
        (value, result_flag)
    }
    fn expression(
        &mut self,
        body: &mut Asm,
        context: Context,
        expr: &Expr,
        indent: usize,
    ) -> Result<(u32, u32)> {
        let range = self.line(indent, &expr.label());
        self.mark(body, &range);
        match expr {
            Expr::Value(term, flag) => {
                let value = Self::term(body, context, term);
                Ok(Self::value_pair(body, value, *flag))
            }
            Expr::Signal(class, payload) => {
                let class = Self::integer(body, class.code());
                let payload = Self::term(body, context, payload);
                self.pair_call(body, self.services.signal, &[class, payload], &range)
            }
            Expr::IfClass { class, yes, no } => {
                let expected = Self::integer(body, class.code());
                let condition = body.alloc_register(ValueType::Bool);
                let yes_label = body.label();
                let no_label = body.label();
                let answer = body.alloc_register(ValueType::I64);
                let flag = body.alloc_register(ValueType::Bool);
                let join = body.block(&[answer, flag]);
                self.mark(body, &range);
                body.i64_eq(condition, context.class, expected)
                    .br(condition, yes_label, no_label);
                body.place(yes_label)?;
                let (value, result_flag) = self.expression(body, context, yes, indent + 1)?;
                body.jmp(join.with_args(&[value, result_flag]));
                body.place(no_label)?;
                let (value, result_flag) = self.expression(body, context, no, indent + 1)?;
                body.jmp(join.with_args(&[value, result_flag]));
                self.mark(body, &range);
                body.place(join)?;
                Ok((answer, flag))
            }
            Expr::Handle {
                class,
                policy,
                accepted,
                handler,
                body: protected,
            } => {
                let handler = self.function(
                    &format!("{class:?}_handler"),
                    Kind::Handler {
                        accepted: *accepted,
                    },
                    handler,
                    indent + 1,
                )?;
                let protected = self.function("handler_body", Kind::Body, protected, indent + 1)?;
                let handler = self.closure(body, context, handler)?;
                let protected = self.closure(body, context, protected)?;
                let class = Self::integer(body, class.code());
                let policy_register = body.alloc_register(ValueType::U64);
                body.const_u64(policy_register, *policy as u64);
                self.pair_call(
                    body,
                    self.services.handle,
                    &[protected, handler, class, policy_register],
                    &range,
                )
            }
            Expr::Restart {
                name,
                mode,
                class,
                function,
                body: protected,
            } => {
                let function =
                    self.function(&restart_name(*name), Kind::Restart, function, indent + 1)?;
                let protected = self.function("restart_body", Kind::Body, protected, indent + 1)?;
                let function = self.closure(body, context, function)?;
                let protected = self.closure(body, context, protected)?;
                let name = Self::integer(body, *name);
                let mode_register = body.alloc_register(ValueType::U64);
                body.const_u64(mode_register, *mode as u64);
                let class = Self::integer(body, class.code());
                self.pair_call(
                    body,
                    self.services.restart,
                    &[protected, function, name, mode_register, class],
                    &range,
                )
            }
            Expr::Protect {
                cleanup,
                body: protected,
            } => {
                let cleanup = self.function("cleanup", Kind::Cleanup, cleanup, indent + 1)?;
                let protected =
                    self.function("protected_body", Kind::Body, protected, indent + 1)?;
                let cleanup = self.closure(body, context, cleanup)?;
                let protected = self.closure(body, context, protected)?;
                self.pair_call(body, self.services.protect, &[protected, cleanup], &range)
            }
            Expr::Invoke(name, argument) => {
                let token = self.lookup(body, context, *name, &range)?;
                let argument = Self::term(body, context, argument);
                self.pair_call(body, self.services.invoke, &[token, argument], &range)
            }
            Expr::Remember(name) => {
                let token = self.lookup(body, context, *name, &range)?;
                self.mark(body, &range);
                let [] = self.builder.emit_call(body, self.services.save, &[token])?;
                let value = Self::integer(body, 0);
                Ok(Self::value_pair(body, value, true))
            }
            Expr::InvokeSaved(argument) => {
                let [token] = self.builder.emit_call(body, self.services.load, &[])?;
                let argument = Self::term(body, context, argument);
                self.pair_call(body, self.services.invoke, &[token, argument], &range)
            }
            Expr::Sequence(expressions) => {
                let mut result = None;
                for expression in expressions {
                    result = Some(self.expression(body, context, expression, indent + 1)?);
                }
                if let Some(result) = result {
                    return Ok(result);
                }
                let value = Self::integer(body, 0);
                Ok(Self::value_pair(body, value, true))
            }
            Expr::Call(name, expression) => {
                let callee = self.function(name, Kind::Body, expression, indent + 1)?;
                let environment = Self::environment(body, context);
                self.pair_call(body, callee, &[environment], &range)
            }
            Expr::Emit(value, expression) => {
                let value = Self::integer(body, *value);
                self.mark(body, &range);
                let [] = self.builder.emit_call(body, self.services.emit, &[value])?;
                self.expression(body, context, expression, indent + 1)
            }
            Expr::Wait(expression) => {
                let [] = self.builder.emit_call(body, self.services.wait, &[])?;
                self.expression(body, context, expression, indent + 1)
            }
        }
    }
}

pub(super) fn compile(expression: Expr, capture: i64) -> Result<Fixture> {
    let mut compiler = Compiler::new(capture)?;
    let entry = compiler.function("main", Kind::Entry, &expression, 0)?;
    let program = Arc::new(compiler.builder.build_verified()?);
    let sources = SourceMap::new(
        program.program(),
        vec![SourceDocument {
            name: "conditions.expr (assembled AST)".into(),
            text: compiler.source,
        }],
        compiler.mappings,
        SourceLimits::default(),
    )?;
    Ok(Fixture {
        program,
        entry,
        sources,
    })
}
pub(super) fn demo() -> Result<Fixture> {
    scenario("recovery")
}

pub(super) const CASES: &[&str] = &[
    "recovery",
    "decline",
    "cl-decline",
    "nested",
    "nested-bind",
    "functions",
    "restart",
    "cleanup",
    "normal-cleanup",
    "cleanup-replaces",
    "cleanup-local-exit",
    "cleanup-signals",
    "unhandled",
    "normal-debugger",
    "expired",
    "rebind",
    "missing",
    "applicability",
];

pub(super) fn scenario(name: &str) -> Result<Fixture> {
    let division = || {
        Expr::Signal(Class::DivisionByZero, Term::Constant(5))
            .call("divide")
            .call("compute")
    };
    let identity = || Expr::Value(Term::Argument, true);
    let increment = || Expr::Value(Term::Argument.plus(Term::Constant(1)), true);
    let exit_handler = || Expr::Invoke(1, Term::Constant(41));
    let expression = match name {
        "recovery" => division().handle(
            Class::ArithmeticError,
            Policy::Dylan,
            Expr::Value(Term::Capture.plus(Term::Constant(2)), true).wait(),
        ),
        "decline" | "cl-decline" => Expr::Handle {
            class: Class::Condition,
            policy: if name == "decline" {
                Policy::Dylan
            } else {
                Policy::CommonLisp
            },
            accepted: name != "decline",
            handler: Box::new(Expr::value(999).emit(11)),
            body: Box::new(division()),
        }
        .handle(
            Class::Error,
            Policy::Dylan,
            Expr::Value(Term::Payload.plus(Term::Constant(10)), false).emit(12),
        ),
        "nested" => division()
            .handle(
                Class::Condition,
                Policy::Dylan,
                Expr::Signal(Class::Warning, Term::Constant(7)).emit(11),
            )
            .handle(
                Class::Condition,
                Policy::Dylan,
                Expr::Value(Term::Payload.plus(Term::Constant(100)), true).emit(12),
            ),
        "nested-bind" => division().handle(
            Class::Condition,
            Policy::Dylan,
            Expr::Signal(Class::Warning, Term::Constant(7)).handle(
                Class::Warning,
                Policy::Dylan,
                Expr::value(123).emit(13),
            ),
        ),
        "functions" => Expr::Sequence(vec![
            Expr::Invoke(3, Term::Constant(1)),
            Expr::Invoke(3, Term::Constant(2)),
        ])
        .restart(
            3,
            RestartMode::Function,
            Expr::Value(Term::Argument.plus(Term::Capture), true).emit(30),
        ),
        "restart" => division()
            .handle(Class::ArithmeticError, Policy::CommonLisp, exit_handler())
            .restart(1, RestartMode::Exit, increment()),
        "cleanup" => division()
            .protect(Expr::value(0).wait().emit(20))
            .protect(Expr::value(0).emit(10))
            .handle(Class::Condition, Policy::CommonLisp, exit_handler())
            .restart(1, RestartMode::Exit, increment()),
        "normal-cleanup" => Expr::value(7)
            .protect(Expr::value(0).wait().emit(20))
            .protect(Expr::value(0).emit(10)),
        "cleanup-replaces" => division()
            .handle(Class::Condition, Policy::CommonLisp, exit_handler())
            .protect(Expr::Invoke(1, Term::Constant(98)).emit(20))
            .restart(1, RestartMode::Exit, increment()),
        "cleanup-local-exit" => division()
            .handle(Class::Condition, Policy::CommonLisp, exit_handler())
            .protect(
                Expr::Sequence(vec![
                    Expr::Invoke(4, Term::Constant(5)).restart(4, RestartMode::Exit, identity()),
                    Expr::value(0).emit(21),
                ])
                .emit(20),
            )
            .restart(1, RestartMode::Exit, increment()),
        "cleanup-signals" => division()
            .protect(Expr::Signal(Class::Warning, Term::Constant(10)).emit(20))
            .handle(
                Class::Condition,
                Policy::Dylan,
                Expr::IfClass {
                    class: Class::Warning,
                    yes: Box::new(Expr::value(11).emit(30)),
                    no: Box::new(exit_handler().emit(11)),
                },
            )
            .restart(1, RestartMode::Exit, increment()),
        "unhandled" => {
            division()
                .protect(Expr::value(0).emit(20))
                .restart(1, RestartMode::Exit, identity())
        }
        "normal-debugger" => division().restart(3, RestartMode::Function, increment()),
        "expired" => Expr::Sequence(vec![
            Expr::Remember(1).restart(1, RestartMode::Function, identity()),
            Expr::InvokeSaved(Term::Constant(9)),
        ])
        .handle(Class::InvalidRestart, Policy::Dylan, Expr::value(77)),
        "rebind" => Expr::Sequence(vec![
            Expr::Remember(1).restart(1, RestartMode::Function, identity()),
            Expr::InvokeSaved(Term::Constant(9)).restart(
                1,
                RestartMode::Function,
                Expr::Value(Term::Argument.plus(Term::Constant(100)), true),
            ),
        ])
        .handle(Class::InvalidRestart, Policy::Dylan, Expr::value(77)),
        "missing" => Expr::Invoke(99, Term::Constant(9)).handle(
            Class::InvalidRestart,
            Policy::Dylan,
            Expr::value(88),
        ),
        "applicability" => Expr::Restart {
            name: 1,
            mode: RestartMode::Exit,
            class: Class::ArithmeticError,
            function: Box::new(identity()),
            body: Box::new(Expr::Signal(Class::Warning, Term::Constant(1)).handle(
                Class::Condition,
                Policy::Dylan,
                exit_handler(),
            )),
        },
        _ => return Err(format!("unknown condition example {name:?}").into()),
    };
    compile(expression, 40)
}

#[cfg(test)]
mod tests {
    use super::*;
    use execution_tape::{
        disasm::disassemble_function, program::Program, source::SourceError, trace::TraceLocation,
    };

    #[test]
    fn every_scenario_has_exact_source_for_each_verified_instruction() {
        for name in CASES {
            let fixture = scenario(name).unwrap();
            let sources = fixture.sources.bind(fixture.program.program()).unwrap();
            for function in fixture.program.functions() {
                let code = disassemble_function(fixture.program.program(), function.id()).unwrap();
                for instruction in code.instrs() {
                    let location = TraceLocation {
                        func: function.id(),
                        pc: instruction.pc(),
                    };
                    let source = sources
                        .lookup(location)
                        .expect("frontend supplied exact mapping");
                    assert!(!source.text().is_empty(), "case {name}, {location:?}");
                }
            }
            // The authored map also binds to an independently decoded copy of the exact tape.
            let encoded = fixture.program.program().encode();
            let decoded = Program::decode(&encoded).unwrap();
            fixture.sources.bind(&decoded).unwrap();
        }
    }

    #[test]
    fn captured_value_change_rejects_stale_source_even_with_identical_function_names() {
        let expression = Expr::Value(Term::Capture, true);
        let old = compile(expression.clone(), 40).unwrap();
        let current = compile(expression, 41).unwrap();
        assert!(matches!(
            old.sources.bind(current.program.program()),
            Err(SourceError::ProgramMismatch)
        ));
    }

    #[test]
    fn unknown_scenario_reports_the_requested_name() {
        let error = scenario("no-such-condition").err().unwrap();
        assert!(error.to_string().contains("no-such-condition"));
    }
}
