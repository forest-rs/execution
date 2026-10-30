// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Causal context from the actual host request that initiated another invocation.

use super::*;
use execution_tape::{
    verifier::VerifiedProgram,
    vm::{InvocationInspection, PendingHostCall},
};

pub(super) struct RequestOrigin<'a> {
    pub(super) program: &'a VerifiedProgram,
    pub(super) location: TraceLocation,
    pub(super) symbol: String,
    pub(super) arguments: Vec<String>,
    inspection: InvocationInspection<'a>,
    pub(super) sources: Option<BoundSources<'a>>,
}
impl<'a> RequestOrigin<'a> {
    pub(super) fn new<P: Host>(
        request: &'a PendingHostCall<'_, '_, P>,
        sources: Option<&'a SourceMap>,
    ) -> Result<Self, SourceError> {
        let program = request.program();
        let mut arguments: Vec<_> = request
            .arguments()
            .take(32)
            .enumerate()
            .map(|(index, value)| {
                let value = caller_value(value);
                format!("Argument[{index}] = {value}")
            })
            .collect();
        if request.arguments().len() > 32 {
            arguments.push("… further caller arguments omitted".into());
        }
        Ok(Self {
            program,
            location: request.location(),
            symbol: describe::preview(request.symbol(), 80),
            arguments,
            inspection: request.inspect(),
            sources: sources.map(|map| map.bind(program.program())).transpose()?,
        })
    }

    pub(super) fn values(&mut self) -> Vec<String> {
        let mut values = vec![
            format!(
                "External caller: {} · byte {}",
                describe::function(self.program.program(), self.location.func),
                self.location.pc
            ),
            format!("Awaiting host {}", self.symbol),
            "Values belong to the caller's heap and host.".into(),
        ];
        values.extend(self.arguments.iter().cloned());
        values.push("Caller registers · before host result:".into());
        let frame = self.inspection.frames().next();
        if let Some(frame) = frame {
            if let Some(registers) = self.inspection.registers(&frame.activation) {
                for register in registers.take(256) {
                    values.push(format!(
                        "r{} · {} = {}",
                        register.register,
                        describe::type_name(register.storage_kind),
                        register.value.map_or_else(
                            || "unavailable (not definitely initialized)".into(),
                            caller_value
                        )
                    ));
                }
            }
            if frame.register_count > 256 {
                values.push("… further caller registers omitted".into());
            }
        }
        values
    }
}

fn caller_value(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Agg(_) | ValueRef::Closure(_) => {
            "caller-owned aggregate/capture; inspect in its invocation".into()
        }
        ValueRef::Obj(object) => format!(
            "object #{} · caller host type {}",
            object.handle.0, object.host_type.0
        ),
        value => value_text(value),
    }
}

impl<'view, 'vm, H: Host> LiveExplorer<'view, 'vm, H> {
    /// Links this invocation to the actual yielded request that initiated it.
    ///
    /// The live stack gains a selectable external-caller row with its exact instruction/source
    /// and bounded argument previews. It is causal context across independent invocations,
    /// without inventing a guest activation or interpreting foreign handles in this VM/host.
    /// The borrowed request cannot complete while this view is using its context. Optional
    /// caller source maps must match that request's exact program, independently of this code.
    pub fn requested_by<P: Host>(
        mut self,
        request: &'view PendingHostCall<'_, '_, P>,
        sources: Option<&'view SourceMap>,
    ) -> Result<Self, SourceError> {
        self.origin = Some(RequestOrigin::new(request, sources)?);
        self.refresh();
        Ok(self)
    }

    /// Selects an attached external caller's instruction, source and read-only values.
    ///
    /// Returns false when no requesting call was attached. This is the programmatic equivalent
    /// of selecting its external-caller stack row; execution controls still address this VM.
    pub fn inspect_caller(&mut self) -> bool {
        if self.origin.is_none() {
            return false;
        }
        self.origin_selected = true;
        self.focus = Focus::Stack;
        self.value_path.clear();
        self.refresh();
        true
    }

    /// Leaves presentation and returns the invocation, preserving its current pending state.
    ///
    /// Embeddings can inspect a yielded call, leave the UI, then complete it or hand it back
    /// to their scheduler. Unlike dropping the explorer, this does not cancel unfinished work.
    #[must_use]
    pub fn into_invocation(self) -> Invocation<'vm, H> {
        self.invocation
    }

    pub(super) fn code_program(&self) -> &execution_tape::program::Program {
        if self.showing_origin {
            self.origin.as_ref().expect("caller code").program.program()
        } else {
            self.invocation.program().program()
        }
    }
}
