// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use execution_tape::program::{HostSigId, Program};
use execution_tape::value::FuncId;
use std::string::String;

/// Optional label resolver for profiling scopes.
///
/// Return `None` to fall back to the default id-based labels.
pub trait LabelResolver {
    /// Resolve a label for a call-frame scope.
    fn call_frame_label(&mut self, _func: FuncId, _program: &Program) -> Option<String> {
        None
    }

    /// Resolve a label for a host-call scope.
    fn host_call_label(&mut self, _host_sig: HostSigId, _program: &Program) -> Option<String> {
        None
    }
}

/// Default resolver that keeps stable id-based labels.
#[derive(Default, Debug)]
pub struct DefaultLabelResolver;

impl LabelResolver for DefaultLabelResolver {}

/// Resolver that uses `Program`-provided names when available.
///
/// Names are resolved from the supplied program on every call. Function and host-import IDs
/// are local to a program, so a resolver can be reused across programs without stale labels.
#[derive(Default, Debug)]
pub struct ProgramSymbolResolver {}

impl LabelResolver for ProgramSymbolResolver {
    fn call_frame_label(&mut self, func: FuncId, program: &Program) -> Option<String> {
        let name = program.function_name(func.0)?;
        Some(format!("func:{name}"))
    }

    fn host_call_label(&mut self, host_sig: HostSigId, program: &Program) -> Option<String> {
        let entry = program.host_sig(host_sig)?;
        let symbol_str = program.symbol_str(entry.symbol).ok()?;
        Some(format!("host:{symbol_str}@{:016x}", entry.sig_hash.0))
    }
}

pub(crate) fn default_call_frame_label(func: FuncId) -> String {
    format!("func:{}", func.0)
}

pub(crate) fn default_host_call_label(host_sig: HostSigId, program: &Program) -> String {
    if let Some(entry) = program.host_sig(host_sig) {
        return format!(
            "host:sig={} sym={} hash={:016x}",
            host_sig.0, entry.symbol.0, entry.sig_hash.0
        );
    }
    format!("host:sig={}", host_sig.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
    use execution_tape::host::HostSig;
    use execution_tape::program::ValueType;

    fn named_program(name: Option<&str>, host: &str, result: ValueType) -> Program {
        let mut builder = ProgramBuilder::new();
        let mut body = Asm::new();
        body.set_register_types(vec![ValueType::Unit]);
        body.ret(0, &[]);
        let func = builder
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![],
                },
            )
            .unwrap();
        if let Some(name) = name {
            builder.set_function_name(func, name).unwrap();
        }
        builder.host_sig_for(
            host,
            HostSig {
                args: vec![],
                rets: vec![result],
            },
        );
        builder.build()
    }

    #[test]
    fn overlapping_local_ids_resolve_against_each_program() {
        let first = named_program(Some("first"), "host_first", ValueType::I64);
        let second = named_program(Some("second"), "host_second", ValueType::Bool);
        let mut resolver = ProgramSymbolResolver::default();
        for program in [&first, &second, &first] {
            assert_eq!(
                resolver.call_frame_label(FuncId(0), program),
                Some(format!("func:{}", program.function_name(0).unwrap()))
            );
            let import = program.host_sig(HostSigId(0)).unwrap();
            assert_eq!(
                resolver.host_call_label(HostSigId(0), program),
                Some(format!(
                    "host:{}@{:016x}",
                    program.symbol_str(import.symbol).unwrap(),
                    import.sig_hash.0
                ))
            );
        }
    }

    #[test]
    fn previous_labels_do_not_supply_missing_symbols() {
        let named = named_program(Some("named"), "host", ValueType::I64);
        let unnamed = named_program(None, "host", ValueType::I64);
        let empty = ProgramBuilder::new().build();
        let mut resolver = ProgramSymbolResolver::default();
        assert!(resolver.call_frame_label(FuncId(0), &named).is_some());
        assert_eq!(resolver.call_frame_label(FuncId(0), &unnamed), None);
        assert!(resolver.host_call_label(HostSigId(0), &named).is_some());
        assert_eq!(resolver.host_call_label(HostSigId(0), &empty), None);
    }
}
