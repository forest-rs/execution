// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A tiny expression frontend for the language protocol. It compiles an authored Rust AST and
//! supplies its formatted expression source as a sidecar; it does not parse a language or map
//! guest operations to this compiler's Rust implementation source.

use super::language::{Class, Policy, RESTART_TOKEN, RestartMode};
use execution_tape::{
    asm::{Asm, ContractRef, FunctionAbi, ProgramBuilder},
    contract::{Contract, ContractId},
    disasm::disassemble_function,
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
            Self::Value(term, flag) => format!("({term:?}, {flag})"),
            Self::Signal(class, term) => format!("signal {class:?}({term:?})"),
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
            Self::Invoke(name, argument) => format!("invoke {}({argument:?})", restart_name(*name)),
            Self::Remember(name) => format!("retain token for {}", restart_name(*name)),
            Self::InvokeSaved(argument) => format!("invoke retained token({argument:?})"),
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
    function: FuncId,
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
    environment: ContractId,
    integer: ContractId,
    boolean: ContractId,
    services: Services,
    source: String,
    anchors: BTreeMap<(u32, u32), Range<usize>>,
    capture: i64,
}
impl Compiler {
    fn new(capture: i64) -> Result<Self> {
        let mut builder = ProgramBuilder::new();
        builder.set_program_name("Language conditions and scoped recovery");
        let integer = builder.contract(Contract::Kind(ValueType::I64))?;
        let boolean = builder.contract(Contract::Kind(ValueType::Bool))?;
        let environment = builder.contract(Contract::Tuple(&[Some(integer); 4]))?;
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
            environment,
            integer,
            boolean,
            services,
            source: String::new(),
            anchors: BTreeMap::new(),
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
    fn mark(&mut self, context: Context, a: &Asm, range: &Range<usize>) {
        self.anchors
            .insert((context.function.0, a.pc()), range.clone());
    }
    fn integer(a: &mut Asm, value: i64) -> u32 {
        let register = a.alloc_register(ValueType::I64);
        a.const_i64(register, value);
        register
    }
    fn context(&mut self, function: FuncId, kind: Kind, a: &mut Asm) -> Context {
        if matches!(kind, Kind::Entry) {
            Context {
                function,
                capture: Self::integer(a, self.capture),
                class: Self::integer(a, -1),
                payload: Self::integer(a, 0),
                argument: Self::integer(a, 0),
            }
        } else {
            let capture = a.alloc_register(ValueType::I64);
            a.tuple_get(capture, 1, 0);
            let field = |a: &mut Asm, index| {
                let reg = a.alloc_register(ValueType::I64);
                a.tuple_get(reg, 1, index);
                reg
            };
            let class = if matches!(kind, Kind::Handler { .. }) {
                2
            } else {
                field(a, 1)
            };
            let payload = if matches!(kind, Kind::Handler { .. }) {
                3
            } else {
                field(a, 2)
            };
            let argument = if matches!(kind, Kind::Restart) {
                2
            } else {
                field(a, 3)
            };
            Context {
                function,
                capture,
                class,
                payload,
                argument,
            }
        }
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
    fn closure(a: &mut Asm, context: Context, function: FuncId) -> u32 {
        let env = Self::environment(a, context);
        let callee = a.alloc_register(ValueType::Func);
        let closure = a.alloc_register(ValueType::Closure);
        a.const_func(callee, function)
            .closure_new(closure, callee, env);
        closure
    }
    fn function(&mut self, role: &str, kind: Kind, expr: &Expr, indent: usize) -> Result<FuncId> {
        let mut args: Vec<ContractRef> = if matches!(kind, Kind::Entry) {
            vec![]
        } else {
            vec![self.environment.into()]
        };
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
        let function = self.builder.declare_function(FunctionAbi { args, rets })?;
        let name = if matches!(kind, Kind::Entry) {
            role.to_string()
        } else {
            format!("{role}#{}", function.0)
        };
        self.builder.set_function_name(function, &name)?;
        let header = self.line(indent, &format!("fn {name}:"));
        let mut a = self.builder.function_asm(function)?;
        self.anchors.insert((function.0, 0), header.clone());
        let context = self.context(function, kind, &mut a);
        let body_start = self.source.len() + 2 * (indent + 1);
        let body_range = body_start..body_start + expr.label().len();
        let (value, flag) = self.expression(&mut a, context, expr, indent + 1)?;
        self.mark(context, &a, &body_range);
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
    fn lookup(&mut self, a: &mut Asm, context: Context, name: i64, range: &Range<usize>) -> u32 {
        let name = Self::integer(a, name);
        let found = a.alloc_register(ValueType::Bool);
        let token = a.alloc_register(ValueType::Obj(RESTART_TOKEN));
        self.mark(context, a, range);
        a.host_call_sig_id(
            0,
            self.services.find,
            0,
            &[name, context.class, context.payload],
            &[found, token],
        );
        token
    }
    fn expression(
        &mut self,
        a: &mut Asm,
        context: Context,
        expr: &Expr,
        indent: usize,
    ) -> Result<(u32, u32)> {
        let range = self.line(indent, &expr.label());
        self.mark(context, a, &range);
        let answer = a.alloc_register(ValueType::I64);
        let flag = a.alloc_register(ValueType::Bool);
        match expr {
            Expr::Value(term, value) => {
                let result = Self::term(a, context, term);
                a.const_bool(flag, *value);
                return Ok((result, flag));
            }
            Expr::Signal(class, payload) => {
                let class = Self::integer(a, class.code());
                let payload = Self::term(a, context, payload);
                self.mark(context, a, &range);
                a.host_call_sig_id(
                    0,
                    self.services.signal,
                    0,
                    &[class, payload],
                    &[answer, flag],
                );
            }
            Expr::IfClass { class, yes, no } => {
                let expected = Self::integer(a, class.code());
                let condition = a.alloc_register(ValueType::Bool);
                let yes_label = a.label();
                let no_label = a.label();
                let join = a.block(&[answer, flag]);
                self.mark(context, a, &range);
                a.i64_eq(condition, context.class, expected)
                    .br(condition, yes_label, no_label);
                a.place(yes_label)?;
                let (value, result_flag) = self.expression(a, context, yes, indent + 1)?;
                a.jmp(join.with_args(&[value, result_flag]));
                a.place(no_label)?;
                let (value, result_flag) = self.expression(a, context, no, indent + 1)?;
                a.jmp(join.with_args(&[value, result_flag]));
                a.place(join)?;
                self.mark(context, a, &range);
            }
            Expr::Handle {
                class,
                policy,
                accepted,
                handler,
                body,
            } => {
                let handler = self.function(
                    &format!("{class:?}_handler"),
                    Kind::Handler {
                        accepted: *accepted,
                    },
                    handler,
                    indent + 1,
                )?;
                let body = self.function("handler_body", Kind::Body, body, indent + 1)?;
                let handler = Self::closure(a, context, handler);
                let body = Self::closure(a, context, body);
                let class = Self::integer(a, class.code());
                let policy_reg = a.alloc_register(ValueType::U64);
                a.const_u64(policy_reg, *policy as u64);
                self.mark(context, a, &range);
                a.host_call_sig_id(
                    0,
                    self.services.handle,
                    0,
                    &[body, handler, class, policy_reg],
                    &[answer, flag],
                );
            }
            Expr::Restart {
                name,
                mode,
                class,
                function,
                body,
            } => {
                let function =
                    self.function(&restart_name(*name), Kind::Restart, function, indent + 1)?;
                let body = self.function("restart_body", Kind::Body, body, indent + 1)?;
                let function = Self::closure(a, context, function);
                let body = Self::closure(a, context, body);
                let name = Self::integer(a, *name);
                let mode_reg = a.alloc_register(ValueType::U64);
                a.const_u64(mode_reg, *mode as u64);
                let class = Self::integer(a, class.code());
                self.mark(context, a, &range);
                a.host_call_sig_id(
                    0,
                    self.services.restart,
                    0,
                    &[body, function, name, mode_reg, class],
                    &[answer, flag],
                );
            }
            Expr::Protect { cleanup, body } => {
                let cleanup = self.function("cleanup", Kind::Cleanup, cleanup, indent + 1)?;
                let body = self.function("protected_body", Kind::Body, body, indent + 1)?;
                let cleanup = Self::closure(a, context, cleanup);
                let body = Self::closure(a, context, body);
                self.mark(context, a, &range);
                a.host_call_sig_id(
                    0,
                    self.services.protect,
                    0,
                    &[body, cleanup],
                    &[answer, flag],
                );
            }
            Expr::Invoke(name, argument) => {
                let token = self.lookup(a, context, *name, &range);
                let argument = Self::term(a, context, argument);
                self.mark(context, a, &range);
                a.host_call_sig_id(
                    0,
                    self.services.invoke,
                    0,
                    &[token, argument],
                    &[answer, flag],
                );
            }
            Expr::Remember(name) => {
                let token = self.lookup(a, context, *name, &range);
                self.mark(context, a, &range);
                a.host_call_sig_id(0, self.services.save, 0, &[token], &[])
                    .const_i64(answer, 0)
                    .const_bool(flag, true);
            }
            Expr::InvokeSaved(argument) => {
                let token = a.alloc_register(ValueType::Obj(RESTART_TOKEN));
                a.host_call_sig_id(0, self.services.load, 0, &[], &[token]);
                let argument = Self::term(a, context, argument);
                self.mark(context, a, &range);
                a.host_call_sig_id(
                    0,
                    self.services.invoke,
                    0,
                    &[token, argument],
                    &[answer, flag],
                );
            }
            Expr::Sequence(expressions) => {
                let mut result = None;
                for expression in expressions {
                    result = Some(self.expression(a, context, expression, indent + 1)?);
                }
                if let Some(result) = result {
                    return Ok(result);
                }
                a.const_i64(answer, 0).const_bool(flag, true);
            }
            Expr::Call(name, body) => {
                let callee = self.function(name, Kind::Body, body, indent + 1)?;
                let env = Self::environment(a, context);
                self.mark(context, a, &range);
                a.call(0, callee, 0, &[env], &[answer, flag]);
            }
            Expr::Emit(value, body) => {
                let value = Self::integer(a, *value);
                self.mark(context, a, &range);
                a.host_call_sig_id(0, self.services.emit, 0, &[value], &[]);
                return self.expression(a, context, body, indent + 1);
            }
            Expr::Wait(body) => {
                a.host_call_sig_id(0, self.services.wait, 0, &[], &[]);
                return self.expression(a, context, body, indent + 1);
            }
        }
        Ok((answer, flag))
    }
}

pub(super) fn compile(expression: Expr, capture: i64) -> Result<Fixture> {
    let mut compiler = Compiler::new(capture)?;
    let entry = compiler.function("main", Kind::Entry, &expression, 0)?;
    let program = Arc::new(compiler.builder.build_verified()?);
    // Expand frontend interval anchors into exact instruction mappings outside VM preparation.
    // Source inspection never points into the Rust compiler implementation or guesses a filename.
    let mut mappings = Vec::new();
    for function in program.functions() {
        let source = disassemble_function(program.program(), function.id()).map_err(|error| {
            format!(
                "cannot map compiled function {}: {error:?}",
                function.id().0
            )
        })?;
        for instruction in source.instrs() {
            let (_, range) = compiler
                .anchors
                .range((function.id().0, 0)..=(function.id().0, instruction.pc()))
                .next_back()
                .ok_or("missing expression anchor")?;
            mappings.push(SourceMapping {
                location: execution_tape::trace::TraceLocation {
                    func: function.id(),
                    pc: instruction.pc(),
                },
                document: SourceId(0),
                range: range.clone(),
            });
        }
    }
    let sources = SourceMap::new(
        program.program(),
        vec![SourceDocument {
            name: "conditions.expr (assembled AST)".into(),
            text: compiler.source,
        }],
        mappings,
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
    use execution_tape::{program::Program, source::SourceError, trace::TraceLocation};

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
