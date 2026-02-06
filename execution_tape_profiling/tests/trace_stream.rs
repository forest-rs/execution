// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration test that drives a real scope stream through the profiling adapter.
//!
//! Run with:
//! `cargo test -p execution_tape_profiling`

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostError, HostSig, SigHash, ValueRef, sig_hash};
use execution_tape::program::ValueType;
use execution_tape::trace::TraceSink;
use execution_tape::value::{FuncId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, Vm};

use execution_tape_profiling::{LabelResolver, ProfilingTraceSink};

#[derive(Clone)]
struct CountingResolver {
    call_frames: Arc<AtomicUsize>,
    host_calls: Arc<AtomicUsize>,
}

impl LabelResolver for CountingResolver {
    fn call_frame_label(
        &mut self,
        func: FuncId,
        _program: &execution_tape::program::Program,
    ) -> Option<String> {
        self.call_frames.fetch_add(1, Ordering::Relaxed);
        Some(format!("call_frame:{}", func.0))
    }

    fn host_call_label(
        &mut self,
        host_sig: execution_tape::program::HostSigId,
        program: &execution_tape::program::Program,
    ) -> Option<String> {
        self.host_calls.fetch_add(1, Ordering::Relaxed);
        let entry = program.host_sig(host_sig)?;
        Some(format!(
            "host_call:sig={} sym={} hash={:016x}",
            host_sig.0, entry.symbol.0, entry.sig_hash.0
        ))
    }
}

struct EchoHost {
    expected_sig: SigHash,
    calls: Arc<AtomicUsize>,
}

impl Host for EchoHost {
    fn call(
        &mut self,
        symbol: &str,
        sig_hash: SigHash,
        args: &[ValueRef<'_>],
    ) -> Result<(Vec<Value>, u64), HostError> {
        if symbol != "trace.echo" {
            return Err(HostError::UnknownSymbol);
        }
        if sig_hash != self.expected_sig {
            return Err(HostError::SignatureMismatch);
        }
        let [ValueRef::I64(arg)] = args else {
            return Err(HostError::Failed);
        };
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok((vec![Value::I64(*arg)], 0))
    }
}

fn build_program() -> (VerifiedProgram, FuncId, SigHash) {
    let mut pb = ProgramBuilder::new();
    let host_sig = HostSig {
        args: vec![ValueType::I64],
        rets: vec![ValueType::I64],
    };
    let expected_sig = sig_hash(&host_sig);
    let host_sig_id = pb.host_sig_for("trace.echo", host_sig);

    let callee = pb.declare_function(FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
        reg_count: 2,
    });
    let main = pb.declare_function(FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
        reg_count: 4,
    });

    let mut a_callee = Asm::new();
    a_callee.ret(0, &[1]);
    pb.define_function(callee, a_callee).unwrap();

    let mut a_main = Asm::new();
    a_main.const_i64(1, 42);
    a_main.call(0, callee, 0, &[1], &[2]);
    a_main.host_call_sig_id(0, host_sig_id, 0, &[2], &[3]);
    a_main.ret(0, &[3]);
    pb.define_function(main, a_main).unwrap();

    let program = pb.build_verified().unwrap();
    (program, main, expected_sig)
}

#[test]
fn profiling_sink_handles_real_scope_stream() {
    let _tracy = tracy_client::Client::start();
    let (program, entry, expected_sig) = build_program();

    let host_calls = Arc::new(AtomicUsize::new(0));
    let call_frames = Arc::new(AtomicUsize::new(0));
    let host_scopes = Arc::new(AtomicUsize::new(0));

    let host = EchoHost {
        expected_sig,
        calls: host_calls.clone(),
    };
    let mut vm = Vm::new(host, Limits::default());

    let resolver = CountingResolver {
        call_frames: call_frames.clone(),
        host_calls: host_scopes.clone(),
    };
    let mut sink = ProfilingTraceSink::with_resolver(resolver);

    let out = vm
        .run(&program, entry, &[], sink.mask(), Some(&mut sink))
        .unwrap();

    assert_eq!(out, vec![Value::I64(42)]);
    assert_eq!(host_calls.load(Ordering::Relaxed), 1);
    assert_eq!(call_frames.load(Ordering::Relaxed), 2);
    assert_eq!(host_scopes.load(Ordering::Relaxed), 1);
}
