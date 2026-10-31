// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A branch, typed join and nested host call shared by the capture tool and its boundary tests.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::{SpanId, ValueType};
use execution_tape::value::{FuncId, Value};
use execution_tape::verifier::VerifiedProgram;
use footfall::{ContextHandle, ScopeDesc};

/// Main entry: `(condition, input) -> output`.
pub const ENTRY: FuncId = FuncId(1);
/// Entry that deliberately divides by zero.
pub const GUEST_TRAP: FuncId = FuncId(2);

static HOST_WORK: ScopeDesc = ScopeDesc::new("demo.host_work");

/// Outcome chosen for the demonstration's host operation.
#[derive(Clone, Copy, Debug)]
pub enum Behavior {
    /// Return the input plus one.
    Success,
    /// Return a host error.
    Failure,
    /// Return a value that violates the declared host signature.
    InvalidReturn,
}

/// Host that instruments its own work in the VM's semantic context.
#[derive(Debug)]
pub struct DemoHost {
    /// Registered context shared with the VM trace sink.
    pub context: ContextHandle,
    /// Host behavior for the next run.
    pub behavior: Behavior,
}

impl Host for DemoHost {
    fn call(
        &mut self,
        _symbol: &str,
        _sig_hash: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let _scope = footfall::scope(&HOST_WORK, self.context);
        let [ValueRef::I64(value)] = args else {
            return Err(HostError::Failed);
        };
        match self.behavior {
            Behavior::Success => rets[0] = Value::I64(value + 1),
            Behavior::Failure => return Err(HostError::Failed),
            Behavior::InvalidReturn => rets[0] = Value::Bool(true),
        }
        Ok(0)
    }
}

/// Builds the exact program saved with the capture.
pub fn program() -> VerifiedProgram {
    use ValueType::{Bool, I64, Unit};
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("footfall_demo");
    let import = builder.host_sig_for(
        "demo.increment",
        HostSig {
            args: vec![I64],
            rets: vec![I64],
        },
    );
    let mut callee = Asm::new();
    callee.set_register_types(vec![Unit, I64, I64]);
    callee.host_call_sig_id(0, import, 0, &[1], &[2]);
    callee.ret(0, &[2]);
    let callee = builder
        .push_function_checked(
            callee,
            FunctionSig {
                arg_types: vec![I64],
                ret_types: vec![I64],
            },
        )
        .expect("valid host wrapper");
    builder
        .set_function_name(callee, "increment")
        .expect("defined function");

    let mut body = Asm::new();
    body.set_register_types(vec![Unit, Bool, I64, I64, I64, I64]);
    body.span(SpanId::try_from(100).expect("nonzero source span"));
    let yes = body.label_named("yes");
    let no = body.label_named("no");
    let join = body.block(&[5]);
    body.br(1, yes, no);
    body.place(yes).expect("fresh label");
    body.call(0, callee, 0, &[2], &[3]);
    body.jmp(join.with_args(&[3]));
    body.place(no).expect("fresh label");
    body.const_i64(4, 0);
    body.jmp(join.with_args(&[4]));
    body.place(join).expect("fresh join");
    body.ret(0, &[5]);
    assert_eq!(
        builder
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![Bool, I64],
                    ret_types: vec![I64],
                },
            )
            .expect("valid entry"),
        ENTRY
    );
    builder
        .set_function_name(ENTRY, "main")
        .expect("defined function");

    let mut trap = Asm::new();
    trap.set_register_types(vec![Unit, I64, I64, I64]);
    trap.const_i64(1, 1);
    trap.const_i64(2, 0);
    trap.i64_div(3, 1, 2);
    trap.ret(0, &[3]);
    assert_eq!(
        builder
            .push_function_checked(
                trap,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![I64],
                },
            )
            .expect("valid guest trap"),
        GUEST_TRAP
    );
    builder
        .set_function_name(GUEST_TRAP, "divide_by_zero")
        .expect("defined function");
    builder.build_verified().expect("valid demo")
}
