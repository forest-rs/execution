// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration test that drives a real scope stream through the profiling adapter.
//!
//! Run with:
//! `cargo test -p execution_tape_tracy`

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef, sig_hash};
use execution_tape::program::ValueType;
use execution_tape::trace::TraceSink;
use execution_tape::value::{FuncId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, Vm};

use execution_tape_tracy::{LabelResolver, ProfilingTraceSink};

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
        rets: &mut [Value],
        _ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
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
        rets[0] = Value::I64(*arg);
        Ok(0)
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

    let callee = pb
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let main = pb
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();

    let mut a_callee = Asm::new();
    a_callee.set_register_types(vec![ValueType::Unit, ValueType::I64]);
    a_callee.ret(0, &[1]);
    pb.define_function(callee, a_callee).unwrap();

    let mut a_main = Asm::new();
    a_main.set_register_types(vec![
        ValueType::Unit,
        ValueType::I64,
        ValueType::I64,
        ValueType::I64,
    ]);
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

#[test]
fn real_guest_and_host_scopes_resolve_the_callback_function_and_pc() {
    use execution_tape::{
        program::Program,
        source::{SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping},
        trace::{ScopeKind, TraceLocation, TraceMask, TraceOutcome},
    };
    struct Observe<'a> {
        sink: ProfilingTraceSink<'a>,
        locations: Vec<(FuncId, String, usize)>,
    }
    impl TraceSink for Observe<'_> {
        fn mask(&self) -> TraceMask {
            self.sink.mask()
        }
        fn scope_enter(
            &mut self,
            program: &Program,
            kind: ScopeKind,
            depth: usize,
            func: FuncId,
            pc: u32,
            span: Option<u64>,
        ) {
            let source = self
                .sink
                .source_location(program, TraceLocation { func, pc })
                .expect("scope maps to its own function/PC");
            self.locations
                .push((func, source.document().name.clone(), source.start().line));
            self.sink.scope_enter(program, kind, depth, func, pc, span);
        }
        fn scope_exit(
            &mut self,
            program: &Program,
            kind: ScopeKind,
            depth: usize,
            func: FuncId,
            pc: u32,
            span: Option<u64>,
            outcome: TraceOutcome<'_>,
        ) {
            self.sink
                .scope_exit(program, kind, depth, func, pc, span, outcome);
        }
    }
    let (program, entry, expected_sig) = build_program();
    let mut mappings = Vec::new();
    for function in [FuncId(0), entry] {
        mappings.extend(
            program
                .function(function)
                .unwrap()
                .instructions()
                .map(|instruction| SourceMapping {
                    location: TraceLocation {
                        func: function,
                        pc: instruction.pc,
                    },
                    document: SourceId(function.0),
                    range: if function.0 == 0 { 0..6 } else { 6..10 },
                }),
        );
    }
    let sources = SourceMap::new(
        program.program(),
        vec![
            SourceDocument {
                name: "helper.expr".into(),
                text: "helper".into(),
            },
            SourceDocument {
                name: "main.expr".into(),
                text: "entry\nmain".into(),
            },
        ],
        mappings,
        SourceLimits::default(),
    )
    .unwrap();
    let host = EchoHost {
        expected_sig,
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let mut vm = Vm::new(host, Limits::default());
    let mut observe = Observe {
        sink: ProfilingTraceSink::new()
            .sources(program.program(), &sources)
            .unwrap(),
        locations: Vec::new(),
    };
    assert_eq!(
        vm.run(&program, entry, &[], observe.mask(), Some(&mut observe))
            .unwrap(),
        [Value::I64(42)]
    );
    assert_eq!(
        observe.locations,
        [
            (entry, "main.expr".into(), 2),
            (FuncId(0), "helper.expr".into(), 1),
            (entry, "main.expr".into(), 2),
        ]
    );
}

#[test]
fn host_panic_unwinds_active_scopes_and_a_fresh_sink_can_run_again() {
    struct PanicHost;
    impl Host for PanicHost {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[ValueRef<'_>],
            _: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            panic!("intentional host panic");
        }
    }
    let _client = tracy_client::Client::start();
    let (program, entry, expected_sig) = build_program();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut vm = Vm::new(PanicHost, Limits::default());
        let mut sink = ProfilingTraceSink::new();
        let _ = vm.run(&program, entry, &[], sink.mask(), Some(&mut sink));
    }));
    assert!(panic.is_err());
    let mut vm = Vm::new(
        EchoHost {
            expected_sig,
            calls: Arc::new(AtomicUsize::new(0)),
        },
        Limits::default(),
    );
    let mut sink = ProfilingTraceSink::new();
    assert_eq!(
        vm.run(&program, entry, &[], sink.mask(), Some(&mut sink))
            .unwrap(),
        [Value::I64(42)]
    );
}
