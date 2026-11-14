// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Optional live inspection. Normal prepared programs retain no debugger indexes.

use super::*;
use crate::value::Obj;
use crate::verifier::{FunctionAnalysis, ProgramAnalysis, ValueFact};
use alloc::{collections::BTreeMap, sync::Arc};
use core::hash::{Hash, Hasher};

/// An activation identity valid only in its originating invocation.
///
/// Cloning retains the opaque identity domain, preventing address reuse from making an old ID
/// refer to a later invocation. Inspection rejects IDs from other invocations, even when their
/// local ordinals match. IDs are process-local handles, not persistent archive identities.
#[derive(Clone)]
pub struct ActivationId {
    owner: Arc<()>,
    ordinal: u64,
}
impl ActivationId {
    /// Local display ordinal; it is unique within one invocation, not across invocations.
    #[must_use]
    pub const fn ordinal(&self) -> u64 {
        self.ordinal
    }
}
impl PartialEq for ActivationId {
    fn eq(&self, other: &Self) -> bool {
        self.ordinal == other.ordinal && Arc::ptr_eq(&self.owner, &other.owner)
    }
}
impl Eq for ActivationId {}
impl Hash for ActivationId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.owner).hash(state);
        self.ordinal.hash(state);
    }
}
impl fmt::Debug for ActivationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActivationId")
            .field("invocation", &Arc::as_ptr(&self.owner))
            .field("ordinal", &self.ordinal)
            .finish()
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Activation {
    ordinal: u64,
    caller: Option<TraceLocation>,
}

/// State touched only by the resumable dispatch specialization.
#[derive(Default)]
pub(super) struct ExecutionControl {
    pub(super) remaining: u64,
    pub(super) yield_host_calls: bool,
    pub(super) yield_suspensions: bool,
    pub(super) pending_host: Option<host_call::PendingHost>,
    pub(super) pending_suspension: Option<invocation::PendingSuspend>,
    pub(super) callbacks: Option<Box<invocation::callback::Callbacks>>,
    owner: Option<Arc<()>>,
    frames: Vec<Activation>,
    last: u64,
}
impl ExecutionControl {
    pub(super) fn owner(&self) -> &Arc<()> {
        self.owner.as_ref().expect("resumable control identity")
    }

    pub(super) fn start(yield_host_calls: bool, yield_suspensions: bool) -> Self {
        Self {
            remaining: 0,
            yield_host_calls,
            yield_suspensions,
            pending_host: None,
            pending_suspension: None,
            callbacks: None,
            owner: Some(Arc::new(())),
            frames: vec![Activation {
                ordinal: 0,
                caller: None,
            }],
            last: 0,
        }
    }
    pub(super) fn paused_location(&self, ctx: &Registers, index: usize) -> TraceLocation {
        if index + 1 < ctx.frames.len() {
            self.frames[index + 1]
                .caller
                .expect("nested activation has caller")
        } else if let Some(pending) = self
            .pending_host
            .filter(|pending| pending.depth == ctx.frames.len())
        {
            pending.location
        } else if let Some(pending) = self
            .pending_suspension
            .filter(|pending| pending.depth == ctx.frames.len())
        {
            pending.location
        } else {
            let frame = &ctx.frames[index];
            TraceLocation {
                unit: frame.unit,
                func: frame.func,
                pc: frame.pc,
            }
        }
    }
    pub(super) fn enter(&mut self, caller: TraceLocation) {
        // Every call consumes hard fuel, which cannot be replenished in an invocation.
        // Even u64::MAX fuel therefore cannot exhaust ordinals before it is exhausted.
        self.last += 1;
        self.frames.push(Activation {
            ordinal: self.last,
            caller: Some(caller),
        });
    }
    pub(super) fn truncate(&mut self, count: usize) {
        self.frames.truncate(count);
    }
    /// Moves the activations from `depth` up into a suspended continuation's `saved` list.
    pub(super) fn suspend(&mut self, depth: usize, saved: &mut Vec<Activation>) {
        saved.clear();
        if self.frames.len() > depth {
            saved.extend(self.frames.drain(depth..));
        }
    }
    /// Restores a continuation's activations above `resumer`, or enters its unstarted root.
    ///
    /// A resumed activation keeps its identity; its root's caller becomes the resumer.
    pub(super) fn reinstate(&mut self, mut saved: Vec<Activation>, resumer: TraceLocation) {
        if let Some(root) = saved.first_mut() {
            root.caller = Some(resumer);
            self.frames.append(&mut saved);
        } else {
            self.enter(resumer);
        }
    }
    pub(super) fn leave(&mut self) {
        self.frames.pop().expect("active guest activation");
    }
}

/// Metadata for one paused activation; the frame order is innermost first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PausedFrame {
    /// Identity retained across resumes until this activation returns.
    pub activation: ActivationId,
    /// Next instruction for the active leaf; pending call instruction for a waiting caller, or
    /// the `suspend` of a leaf whose suspension waits for the embedding.
    pub location: TraceLocation,
    /// Instruction that entered this activation; `None` for the root.
    pub caller: Option<TraceLocation>,
    /// Whether a guest callee or yielded host operation has not returned yet, or a yielded
    /// suspension has not been resumed yet.
    pub waiting_for_return: bool,
    /// Number of source register declarations, including r0 and unused registers.
    pub register_count: u32,
}

/// A source register's storage kind and safely inspectable value at a suspension.
#[derive(Clone, Copy, Debug)]
pub struct RegisterValue<'a> {
    /// Function-local virtual register number.
    pub register: u32,
    /// Fixed declared kind, including for unavailable values.
    pub storage_kind: ValueType,
    /// Borrowed value only when definite initialization is established at this boundary.
    /// `None` does not expose a bank default or a value left by an earlier activation.
    /// A path-specific value that is not definitely initialized is intentionally unavailable.
    pub value: Option<ValueRef<'a>>,
}

/// Where a conservatively retained host-object reference occurs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectRootOrigin {
    /// An initialized register of an active frame.
    ///
    /// An exception reference register contributes the objects in its exception's payload.
    Register {
        /// Activation containing the register.
        activation: ActivationId,
        /// Source register number.
        register: u32,
    },
    /// One of the invocation's borrowed entry arguments.
    Argument {
        /// Argument position.
        index: usize,
    },
    /// A committed root return slot.
    Output {
        /// Result position.
        index: usize,
    },
    /// A callback result still owned by the pending host operation.
    CallbackResult {
        /// Callback result position, independent of the host return signature.
        index: usize,
    },
    /// An initialized register of a frame held by an unresumed in-guest continuation.
    ///
    /// `cont.new`, `cont.bind` and a `suspend` handled by a guest `on` clause move frames off the
    /// active stack, so [`InvocationInspection::frames`] does not list them, but resuming the
    /// continuation can still read these registers. An unstarted frame's only initialized
    /// registers are the arguments bound to it. As for [`Self::Register`], an exception reference
    /// register contributes the objects in its exception's payload.
    Continuation {
        /// The frame's activation, which keeps its identity when resumed; `None` for an
        /// unstarted frame, whose activation begins when the continuation is first resumed.
        activation: Option<ActivationId>,
        /// Where the frame waits: its `suspend` or pending call, or an unstarted frame's entry.
        location: TraceLocation,
        /// Source register number.
        register: u32,
    },
    /// An immediate field of a retained or temporary VM aggregate.
    Aggregate {
        /// Heap-local node identity.
        aggregate: AggHandle,
        /// Immediate field/element position.
        index: usize,
    },
}

/// A host-owned object handle referenced by invocation or VM storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectRoot {
    /// Opaque object token; its host type and heap validity remain the embedder's responsibility.
    pub object: Obj,
    /// The source of this reference. Repeated references are reported separately.
    pub origin: ObjectRootOrigin,
}

struct FunctionInspection<'a> {
    facts: FunctionAnalysis<'a>,
    registers: Vec<VReg>,
}

/// One frame's register storage, wherever it lives, and what establishes its initialization.
pub(super) struct FrameRegisters<'a> {
    /// Where the frame stopped: its next instruction, a pending call or `suspend`, or the entry
    /// of an unstarted frame.
    pub(super) location: TraceLocation,
    /// Whether the frame has run, so the facts before `location` establish initialization. An
    /// unstarted frame's only initialized registers are `bound`.
    pub(super) started: bool,
    /// Registers written since the frame stopped, which its facts cannot show initialized.
    pub(super) bound: &'a [VReg],
    /// The frame's register base within `objs` and `exns`.
    pub(super) base: RegBase,
    /// Object registers.
    pub(super) objs: &'a [Obj],
    /// Exception reference registers.
    pub(super) exns: &'a [Option<Arc<Exception>>],
}

/// Borrowed live state plus lazily computed debugger indexes.
///
/// Only queried functions allocate analysis/mapping data. Holding this view prevents resumption
/// or mutation of the host. Reuse it for multiple queries at one suspension; discard it before
/// resuming. Terminal invocations expose no live frames, but retained heaps and outputs can
/// still contribute object roots. Source connections use each frame's exact `location`.
pub struct InvocationInspection<'a> {
    units: &'a UnitTable,
    ctx: &'a Registers,
    args: &'a [Value],
    outputs: &'a [Value],
    aggregates: &'a AggHeap,
    control: &'a ExecutionControl,
    active: bool,
    /// Keyed by unit and function: each frame is analyzed in its own unit's program.
    functions: BTreeMap<(u32, u32), FunctionInspection<'a>>,
}
impl fmt::Debug for InvocationInspection<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InvocationInspection")
            .field("active", &self.active)
            .field("indexed_functions", &self.functions.len())
            .finish_non_exhaustive()
    }
}

impl<H: Host> Invocation<'_, H> {
    /// Borrows paused state. Expensive register facts are built only for queried functions.
    #[must_use]
    pub fn inspect(&self) -> InvocationInspection<'_> {
        inspection(&self.continuation, &self.vm.units, self.vm.memory.aggs())
    }

    /// Mutates host state between quanta. Preserve the validity of referenced object handles.
    /// For collection, enumerate `inspect().visit_objects(...)` first and add the language's
    /// external roots; this API does not choose a garbage-collection or object-lifetime policy.
    pub fn host_mut(&mut self) -> &mut H {
        &mut self.vm.host
    }
}

impl<H: Host> invocation::Running<'_, '_, H> {
    pub(super) fn inspect(&self) -> InvocationInspection<'_> {
        inspection(self.continuation, &self.vm.units, self.vm.memory.aggs())
    }
}

fn inspection<'a>(
    continuation: &'a continuation::Continuation<'_>,
    units: &'a UnitTable,
    aggregates: &'a AggHeap,
) -> InvocationInspection<'a> {
    InvocationInspection {
        units,
        ctx: &continuation.ctx,
        args: &continuation.args,
        outputs: continuation
            .outcome
            .as_ref()
            .and_then(|outcome| outcome.as_ref().ok())
            .map_or(&[][..], |()| continuation.outputs.as_slice()),
        aggregates,
        control: &continuation.control,
        active: continuation.outcome.is_none(),
        functions: BTreeMap::new(),
    }
}

impl<'a> InvocationInspection<'a> {
    fn frame_at(&self, index: usize) -> PausedFrame {
        let frame = &self.ctx.frames[index];
        let activation = self.control.frames[index];
        let child = index + 1 < self.ctx.frames.len();
        let pending = self
            .control
            .pending_host
            .is_some_and(|pending| !child && pending.depth == self.ctx.frames.len());
        let suspended = self
            .control
            .pending_suspension
            .is_some_and(|pending| !child && pending.depth == self.ctx.frames.len());
        let waiting = child || pending || suspended;
        let location = self.control.paused_location(self.ctx, index);
        PausedFrame {
            activation: self.activation_id(activation),
            location,
            caller: activation.caller,
            waiting_for_return: waiting,
            register_count: self.units.unit(frame.unit).program.program().functions
                [frame.func.0 as usize]
                .reg_count,
        }
    }
    /// The facts and register mapping of `location`'s function, built on first use.
    fn function(&mut self, location: TraceLocation) -> &mut FunctionInspection<'a> {
        let program: &'a VerifiedProgram = &self.units.unit(location.unit).program;
        self.functions
            .entry((location.unit.0, location.func.0))
            .or_insert_with(|| FunctionInspection {
                facts: ProgramAnalysis::from(program)
                    .function(location.func)
                    .expect("verified function"),
                registers: observations::register_map(program.program(), location.func)
                    .expect("verified ledger"),
            })
    }
    fn activation_id(&self, activation: Activation) -> ActivationId {
        ActivationId {
            owner: Arc::clone(self.control.owner.as_ref().expect("invocation identity")),
            ordinal: activation.ordinal,
        }
    }
    fn index(&self, id: &ActivationId) -> Option<usize> {
        if !self.active
            || !self
                .control
                .owner
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &id.owner))
        {
            return None;
        }
        self.control
            .frames
            .iter()
            .position(|frame| frame.ordinal == id.ordinal)
    }

    /// Visits active frames, innermost first, without constructing register indexes.
    ///
    /// Frames held by unresumed continuations are not active and are not listed; their object
    /// references are reported by [`Self::visit_objects`].
    pub fn frames(&self) -> impl ExactSizeIterator<Item = PausedFrame> + DoubleEndedIterator + '_ {
        let count = if self.active {
            self.ctx.frames.len()
        } else {
            0
        };
        (0..count).rev().map(|index| self.frame_at(index))
    }

    /// Queries one register. Unknown, retired or foreign activations, activations suspended in a
    /// continuation and absent registers return `None`. A known declaration without definite
    /// initialization returns a value with `None`. Waiting callers use facts before their pending
    /// call, never hypothetical returned values.
    pub fn register(&mut self, id: &ActivationId, register: u32) -> Option<RegisterValue<'a>> {
        let index = self.index(id)?;
        let frame = self.frame_at(index);
        let (ctx, sources) = (
            self.ctx,
            ValueSources::new(self.units, self.args, self.aggregates),
        );
        let function = self.function(frame.location);
        let fact = function
            .facts
            .instruction(frame.location.pc)?
            .register(register)?;
        let value = if matches!(fact.before, ValueFact::Contract(_)) {
            read_value_ref_at(
                &ctx.arena,
                sources,
                &ctx.bools,
                &ctx.i64s,
                &ctx.u64s,
                &ctx.f64s,
                &ctx.w32s,
                &ctx.decimals,
                &ctx.bytes,
                &ctx.strs,
                &ctx.objs,
                &ctx.aggs,
                &ctx.funcs,
                &ctx.closures,
                ctx.frames[index].base,
                *function.registers.get(register as usize)?,
            )
            .ok()
        } else {
            None
        };
        Some(RegisterValue {
            register,
            storage_kind: fact.storage_kind,
            value,
        })
    }

    /// Iterates declarations in source-register order, including explicit unavailable values.
    /// Building/retaining only this selected function's facts is an on-demand inspection cost.
    pub fn registers(
        &mut self,
        id: &ActivationId,
    ) -> Option<impl Iterator<Item = RegisterValue<'a>> + '_> {
        let count = self.frame_at(self.index(id)?).register_count;
        let id = id.clone();
        Some((0..count).filter_map(move |register| self.register(&id, register)))
    }

    /// Enumerates conservative host-object references without materializing a root vector.
    ///
    /// Includes initialized registers of active frames and of frames held by unresumed
    /// continuations, the payloads of exceptions those registers reference, borrowed arguments,
    /// pending callback results, committed outputs and every VM aggregate field (including
    /// retained results and uncommitted temporaries). Duplicates and no-longer-needed values may
    /// occur; this is not a minimal liveness set. No uninitialized register slots are read. The
    /// collector must additionally trace its own heap, globals, snapshots and other external
    /// roots. Object identity, relocation and reclamation belong to that collector. This method
    /// performs no collection or host-state mutation.
    pub fn visit_objects(&mut self, mut visit: impl FnMut(ObjectRoot)) {
        let ctx = self.ctx;
        for index in (0..if self.active { ctx.frames.len() } else { 0 }).rev() {
            let frame = self.frame_at(index);
            let registers = FrameRegisters {
                location: frame.location,
                started: true,
                bound: &[],
                base: ctx.frames[index].base,
                objs: &ctx.objs,
                exns: &ctx.exns,
            };
            self.visit_frame(
                registers,
                |register| ObjectRootOrigin::Register {
                    activation: frame.activation.clone(),
                    register,
                },
                &mut visit,
            );
        }
        if self.active {
            for (activation, registers) in ctx.continuations.held(self.units) {
                let activation = activation.map(|activation| self.activation_id(activation));
                let location = registers.location;
                self.visit_frame(
                    registers,
                    |register| ObjectRootOrigin::Continuation {
                        activation: activation.clone(),
                        location,
                        register,
                    },
                    &mut visit,
                );
            }
        }
        for (index, value) in self.args.iter().enumerate() {
            if let Value::Obj(object) = value {
                visit(ObjectRoot {
                    object: *object,
                    origin: ObjectRootOrigin::Argument { index },
                });
            }
        }
        for (index, value) in self.outputs.iter().enumerate() {
            if let Value::Obj(object) = value {
                visit(ObjectRoot {
                    object: *object,
                    origin: ObjectRootOrigin::Output { index },
                });
            }
        }
        if let Some(values) = self
            .control
            .callbacks
            .as_ref()
            .and_then(|callbacks| callbacks.returned.as_deref())
        {
            for (index, value) in values.iter().enumerate() {
                if let Value::Obj(object) = value {
                    visit(ObjectRoot {
                        object: *object,
                        origin: ObjectRootOrigin::CallbackResult { index },
                    });
                }
            }
        }
        for (aggregate, index, object) in self.aggregates.object_references() {
            visit(ObjectRoot {
                object,
                origin: ObjectRootOrigin::Aggregate { aggregate, index },
            });
        }
    }

    /// Visits the objects in a frame's initialized object and exception reference registers.
    ///
    /// An exception reference contributes the objects of its payload, under its register's origin.
    fn visit_frame(
        &mut self,
        frame: FrameRegisters<'a>,
        origin: impl Fn(u32) -> ObjectRootOrigin,
        visit: &mut impl FnMut(ObjectRoot),
    ) {
        let FunctionInspection { facts, registers } = self.function(frame.location);
        let facts = if frame.started {
            facts.instruction(frame.location.pc)
        } else {
            None
        };
        for (register, &vreg) in (0_u32..).zip(registers.iter()) {
            if !matches!(vreg, VReg::Obj(_) | VReg::Exn(_)) {
                continue;
            }
            let initialized = frame.bound.contains(&vreg)
                || facts
                    .as_ref()
                    .and_then(|facts| facts.register(register))
                    .is_some_and(|fact| matches!(fact.before, ValueFact::Contract(_)));
            if !initialized {
                continue;
            }
            match vreg {
                VReg::Obj(r) => visit(ObjectRoot {
                    object: frame.objs[frame.base.objs + r.0 as usize],
                    origin: origin(register),
                }),
                VReg::Exn(r) => {
                    let Some(exception) = &frame.exns[frame.base.exns + r.0 as usize] else {
                        continue;
                    };
                    for value in exception.payload() {
                        if let Value::Obj(object) = value {
                            visit(ObjectRoot {
                                object: *object,
                                origin: origin(register),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }
}
