// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A fresh observer imports present state, never invented historical starts.

use super::*;
use crate::trace::{FrameSnapshot, ScopeSnapshot, StackSnapshot, TraceContext};

pub(super) fn join(
    stack: &mut Continuation<'_>,
    observers: &mut Observers<'_>,
    mask: TraceMask,
    parked: bool,
    yielding: bool,
) {
    stack.control.yield_host_calls = yielding;
    stack.trace = TraceState::new(mask);
    stack.register_state = observations::RegisterState::default();
    if mask == TraceMask::NONE {
        return;
    }
    stack.control.enable_identity();
    stack.trace.stack = Some(stack.control.root_activation());
    stack.trace.run_open = mask.contains(TraceMask::RUN);
    let frames: Vec<_> = stack
        .ctx
        .frames
        .iter()
        .enumerate()
        .map(|(index, _)| FrameSnapshot {
            activation: stack.control.activation(&stack.ctx, index),
            location: stack.control.paused_location(&stack.ctx, index),
        })
        .collect();
    let mut scopes = Vec::new();
    if mask.contains(TraceMask::CALL) {
        for (index, frame) in stack.ctx.frames.iter().enumerate() {
            let location = TraceLocation {
                func: frame.func,
                pc: 0,
            };
            scopes.push(ScopeSnapshot {
                kind: ScopeKind::CallFrame {
                    func: frame.func,
                    caller: ExecutionControl::caller(&stack.ctx, index),
                },
                depth: index + 1,
                location,
                span: stack
                    .ctx
                    .span_at(stack.program.program(), location.func, location.pc),
            });
        }
    }
    if mask.contains(TraceMask::HOST)
        && let Some(pending) = stack.control.waiting.host()
    {
        let (ExecInstr::HostCall { host_sig, .. }, _, _) = stack
            .program
            .verified(pending.location.func)
            .expect("admitted definition")
            .fetch_at_ix(pending.instruction)
            .expect("admitted host")
        else {
            unreachable!()
        };
        let signature = stack
            .program
            .program()
            .host_sig(*host_sig)
            .expect("verified import");
        scopes.push(ScopeSnapshot {
            kind: ScopeKind::HostCall {
                host_sig: *host_sig,
                symbol: signature.symbol,
                sig_hash: signature.sig_hash,
            },
            depth: pending.depth,
            location: pending.location,
            span: stack.ctx.span_at(
                stack.program.program(),
                pending.location.func,
                pending.location.pc,
            ),
        });
    }
    stack.trace.scopes = scopes
        .iter()
        .map(|scope| (scope.kind, scope.depth))
        .collect();
    let context = TraceContext::new(stack.program.program(), stack.trace.stack.as_ref());
    if let Some(sink) = &mut observers.trace {
        sink.run_mask(context, mask);
        sink.stack_context(
            context,
            StackSnapshot {
                mask,
                entry: stack.entry,
                arg_count: stack.args.len(),
                parent: stack.parent.as_ref().map(|(id, _)| id),
                frames: &frames,
                scopes: &scopes,
                pending: stack.control.waiting.site().map(|(location, _)| location),
                parked,
            },
        );
    }
}

fn release(stack: &mut Continuation<'_>, observers: &mut Observers<'_>) {
    let mask = stack.trace.mask;
    // Observer bookkeeping is retired before the callback, so unwinding cannot replay it.
    stack.trace.mask = TraceMask::NONE;
    stack.trace.run_open = false;
    stack.trace.pending = None;
    stack.trace.scopes.clear();
    stack.register_state = observations::RegisterState::default();
    if mask != TraceMask::NONE
        && let Some(sink) = &mut observers.trace
    {
        sink.stack_release(TraceContext::new(
            stack.program.program(),
            stack.trace.stack.as_ref(),
        ));
    }
}

pub(super) fn join_domain(
    domain: &mut Domain,
    observers: &mut Observers<'_>,
    mask: TraceMask,
    yielding: bool,
) {
    // Parked inventory creates no physical guard; active chain imports root first.
    for saved in domain.saved.values_mut() {
        if let invocation::delimited::Saved::Suspended(stacks) = saved {
            for stack in stacks {
                join(stack, observers, mask, true, yielding);
            }
        }
    }
}

pub(super) fn release_domain(domain: &mut Domain, observers: &mut Observers<'_>) {
    if let Some(chain) = &mut domain.paused {
        release(&mut chain.continuation, observers);
        for stack in chain.parents.iter_mut().rev() {
            release(stack, observers);
        }
    }
    for saved in domain.saved.values_mut() {
        if let invocation::delimited::Saved::Suspended(stacks) = saved {
            for stack in stacks.iter_mut().rev() {
                release(stack, observers);
            }
        }
    }
}
