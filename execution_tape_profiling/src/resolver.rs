// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use execution_tape::program::{HostSigId, Program};
use execution_tape::value::FuncId;
use std::collections::HashMap;
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
#[derive(Default, Debug)]
pub struct ProgramSymbolResolver {
    call_frame_cache: HashMap<FuncId, String>,
    host_call_cache: HashMap<HostSigId, String>,
}

impl LabelResolver for ProgramSymbolResolver {
    fn call_frame_label(&mut self, func: FuncId, program: &Program) -> Option<String> {
        if let Some(label) = self.call_frame_cache.get(&func) {
            return Some(label.clone());
        }
        let name = program.function_name(func.0)?;
        let label = format!("func:{name}");
        self.call_frame_cache.insert(func, label.clone());
        Some(label)
    }

    fn host_call_label(&mut self, host_sig: HostSigId, program: &Program) -> Option<String> {
        if let Some(label) = self.host_call_cache.get(&host_sig) {
            return Some(label.clone());
        }
        let entry = program.host_sig(host_sig)?;
        let symbol_str = program.symbol_str(entry.symbol).ok()?;
        let label = format!("host:{symbol_str}@{:016x}", entry.sig_hash.0);
        self.host_call_cache.insert(host_sig, label.clone());
        Some(label)
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
