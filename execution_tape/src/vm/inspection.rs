// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Optional live inspection. Normal prepared programs retain no debugger indexes.

use super::*;
use crate::value::Obj;
use crate::verifier::{FunctionAnalysis, ProgramAnalysis, ValueFact};
use alloc::{collections::BTreeMap, sync::Arc};
use core::hash::{Hash, Hasher};

/// An activation identity valid only in its originating execution.
///
/// Cloning retains the opaque identity domain, preventing address reuse from making an old ID
/// refer to a later invocation. Inspection rejects IDs from other invocations, even when their
/// local ordinals match. IDs are process-local handles, not persistent archive identities.
#[derive(Clone)]
pub struct ActivationId {
    owner: Arc<()>,
    execution: Arc<()>,
    ordinal: u64,
    segment: u64,
}
impl ActivationId {
    /// Display identity of the code segment within this execution. Segment ordinals are never
    /// reused during that execution; the opaque execution identity remains required across runs.
    #[must_use]
    pub const fn segment_ordinal(&self) -> u64 {
        self.segment
    }

    /// Display ordinal local to one code segment. Compare opaque identities across segments;
    /// equal ordinals alone never identify the same activation.
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
            .field("control", &Arc::as_ptr(&self.owner))
            .field("execution", &Arc::as_ptr(&self.execution))
            .field("segment", &self.segment)
            .field("ordinal", &self.ordinal)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Activation {
    ordinal: u64,
    caller: Option<TraceLocation>,
}

/// Exactly one boundary can own the next result of a control stack.
#[derive(Clone, Copy, Debug, Default)]
pub(super) enum Waiting {
    #[default]
    Ready,
    /// Installed owner whose control transition has not completed observer delivery.
    Transition(TraceLocation),
    Host(host_call::PendingHost),
    Guest(PendingGuest),
    Control(PendingGuest),
}
impl Waiting {
    pub(super) fn host(self) -> Option<host_call::PendingHost> {
        match self {
            Self::Host(site) => Some(site),
            _ => None,
        }
    }
    pub(super) fn host_mut(&mut self) -> Option<&mut host_call::PendingHost> {
        match self {
            Self::Host(site) => Some(site),
            _ => None,
        }
    }
    pub(super) fn guest(self) -> Option<PendingGuest> {
        match self {
            Self::Guest(site) => Some(site),
            _ => None,
        }
    }
    pub(super) fn guest_mut(&mut self) -> Option<&mut PendingGuest> {
        match self {
            Self::Guest(site) => Some(site),
            _ => None,
        }
    }
    pub(super) fn control(self) -> Option<PendingGuest> {
        match self {
            Self::Control(site) => Some(site),
            _ => None,
        }
    }
    pub(super) fn control_mut(&mut self) -> Option<&mut PendingGuest> {
        match self {
            Self::Control(site) => Some(site),
            _ => None,
        }
    }
    pub(super) fn transfer(self) -> Option<PendingGuest> {
        match self {
            Self::Guest(site) | Self::Control(site) => Some(site),
            _ => None,
        }
    }
    pub(super) fn site(self) -> Option<(TraceLocation, usize)> {
        match self {
            Self::Host(site) => Some((site.location, site.depth)),
            Self::Guest(site) | Self::Control(site) => Some((site.location, site.depth)),
            Self::Transition(location) => Some((location, 0)),
            Self::Ready => None,
        }
    }
    pub(super) fn poison(&mut self) {
        match self {
            Self::Host(site) => site.abandoned = true,
            Self::Guest(site) | Self::Control(site) => site.abandoned = true,
            Self::Ready | Self::Transition(_) => {}
        }
    }
    pub(super) fn interrupted(self) -> Option<TraceLocation> {
        match self {
            Self::Transition(location) => Some(location),
            Self::Guest(site) | Self::Control(site) if site.abandoned => Some(site.location),
            _ => None,
        }
    }
}

/// State touched only by the resumable dispatch specialization.
#[derive(Default)]
pub(super) struct ExecutionControl {
    pub(super) remaining: u64,
    pub(super) yield_host_calls: bool,
    pub(super) waiting: Waiting,
    pub(super) callbacks: Option<Box<invocation::callback::Callbacks>>,
    owner: Option<Arc<()>>,
    execution: Option<Arc<()>>,
    last: u64,
    pub(super) segment: u64,
}
impl ExecutionControl {
    pub(super) fn execution_owner(&self) -> &Arc<()> {
        self.execution.as_ref().expect("execution identity")
    }

    pub(super) fn owner(&self) -> &Arc<()> {
        self.owner.as_ref().expect("resumable control identity")
    }

    pub(super) fn start(yield_host_calls: bool) -> Self {
        let owner = Arc::new(());
        Self {
            remaining: 0,
            yield_host_calls,
            waiting: Waiting::Ready,
            callbacks: None,
            owner: Some(Arc::clone(&owner)),
            execution: Some(owner),
            last: 0,
            segment: 0,
        }
    }
    pub(super) fn activation(&self, ctx: &Registers, index: usize) -> ActivationId {
        ActivationId {
            owner: Arc::clone(self.owner()),
            execution: Arc::clone(self.execution.as_ref().expect("execution identity")),
            ordinal: ctx.frames[index].activation.ordinal,
            segment: self.segment,
        }
    }

    pub(super) fn root_activation(&self) -> ActivationId {
        ActivationId {
            owner: Arc::clone(self.owner()),
            execution: Arc::clone(self.execution.as_ref().expect("execution identity")),
            ordinal: 0,
            segment: self.segment,
        }
    }

    pub(super) fn inherit_execution(&mut self, parent: &Self) {
        self.execution = parent.execution.clone();
    }

    pub(super) fn belongs_to_execution(&self, id: &ActivationId) -> bool {
        self.execution
            .as_ref()
            .is_some_and(|owner| Arc::ptr_eq(owner, &id.execution))
    }

    pub(super) fn paused_location(&self, ctx: &Registers, index: usize) -> TraceLocation {
        if index + 1 < ctx.frames.len() {
            ctx.frames[index + 1]
                .activation
                .caller
                .expect("nested activation has caller")
        } else if let Some((location, _)) = self
            .waiting
            .site()
            .filter(|(_, depth)| *depth == ctx.frames.len())
        {
            location
        } else {
            let frame = &ctx.frames[index];
            TraceLocation {
                func: frame.func,
                pc: frame.pc,
            }
        }
    }
    pub(super) fn target_index(
        &self,
        ctx: &Registers,
        id: &ActivationId,
    ) -> Result<usize, ControlError> {
        if !self
            .owner
            .as_ref()
            .is_some_and(|owner| Arc::ptr_eq(owner, &id.owner))
        {
            return Err(ControlError::ForeignInvocation);
        }
        ctx.frames
            .iter()
            .position(|frame| frame.activation.ordinal == id.ordinal)
            .ok_or(ControlError::ExpiredActivation)
    }
    pub(super) fn enter(&mut self, ctx: &mut Registers, caller: TraceLocation) {
        self.last += 1;
        ctx.frames.last_mut().expect("entered frame").activation = Activation {
            ordinal: self.last,
            caller: Some(caller),
        };
    }
    pub(super) fn enable_identity(&mut self) {
        if self.owner.is_none() {
            let owner = Arc::new(());
            self.owner = Some(Arc::clone(&owner));
            self.execution = Some(owner);
        }
    }
    pub(super) fn proof_owner(&self) -> Option<&Arc<()>> {
        self.owner.as_ref()
    }
}

/// Metadata for one paused activation; the frame order is innermost first.
#[derive(Clone, Debug)]
pub struct PausedFrame {
    /// Exact code definition interpreting this frame's local location and registers.
    pub program: VerifiedProgram,
    /// Store namespace, when the frame belongs to an explicitly selected or admitted instance.
    pub instance: Option<Instance>,
    /// Actual caller activation, including a caller in another code instance.
    pub parent: Option<ActivationId>,
    /// Identity retained across resumes until this activation returns.
    pub activation: ActivationId,
    /// Next instruction for the active leaf; pending call instruction for a waiting caller.
    pub location: TraceLocation,
    /// Instruction that entered this activation; `None` for the root.
    pub caller: Option<TraceLocation>,
    /// Whether a guest callee or yielded host operation has not returned yet.
    pub waiting_for_return: bool,
    /// Number of source register declarations, including r0 and unused registers.
    pub register_count: u32,
}

impl PartialEq for PausedFrame {
    fn eq(&self, other: &Self) -> bool {
        self.program.shares_definition_with(&other.program)
            && self.instance == other.instance
            && self.parent == other.parent
            && self.activation == other.activation
            && self.location == other.location
            && self.caller == other.caller
            && self.waiting_for_return == other.waiting_for_return
            && self.register_count == other.register_count
    }
}
impl Eq for PausedFrame {}

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
    /// An initialized paused register.
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

/// A computation parked outside the active call chain, still owned by this execution.
/// Query its exact frames before diagnosing retained values or collecting host objects.
pub struct SavedContinuation<'a> {
    id: crate::control::ContinuationId,
    stacks: Vec<InvocationInspection<'a>>,
}
impl SavedContinuation<'_> {
    /// One-use authority identity; this inspection view cannot resume it.
    #[must_use]
    pub fn id(&self) -> &crate::control::ContinuationId {
        &self.id
    }
    /// Whether guest execution has begun. Unstarted computations have no frames.
    #[must_use]
    pub fn is_started(&self) -> bool {
        !self.stacks.is_empty()
    }
    /// Parked frames, innermost first, retaining their exact code and activation identity.
    pub fn frames(&self) -> impl ExactSizeIterator<Item = PausedFrame> + DoubleEndedIterator + '_ {
        self.stacks
            .iter()
            .rev()
            .flat_map(|stack| stack.frames())
            .collect::<Vec<_>>()
            .into_iter()
    }
}
impl fmt::Debug for SavedContinuation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SavedContinuation")
            .field("id", &self.id)
            .field("started", &self.is_started())
            .field("stacks", &self.stacks.len())
            .finish()
    }
}

fn saved_inspections<'a>(
    saved: &'a crate::control::arena::Resumptions<invocation::delimited::Saved<'_>>,
    heap: &'a AggHeap,
) -> Vec<SavedContinuation<'a>> {
    saved
        .iter()
        .map(|(id, saved)| SavedContinuation {
            id,
            stacks: match saved {
                invocation::delimited::Saved::New { .. } => Vec::new(),
                invocation::delimited::Saved::Suspended(stacks) => {
                    stacks.iter().map(|stack| inspection(stack, heap)).collect()
                }
            },
        })
        .collect()
}

struct FunctionInspection<'a> {
    facts: FunctionAnalysis<'a>,
    registers: Vec<VReg>,
}

/// Borrowed live state plus lazily computed debugger indexes.
///
/// Only queried functions allocate analysis/mapping data. Holding this view prevents resumption
/// or mutation of the host. Reuse it for multiple queries at one suspension; discard it before
/// resuming. Terminal invocations expose no live frames, but retained heaps and outputs can
/// still contribute object roots. Source connections use each frame's exact `location`.
pub struct InvocationInspection<'a> {
    program: &'a VerifiedProgram,
    ctx: &'a Registers,
    args: &'a [Value],
    outputs: &'a [Value],
    aggregates: &'a AggHeap,
    control: &'a ExecutionControl,
    active: bool,
    functions: BTreeMap<u32, FunctionInspection<'a>>,
    ancestors: Vec<Self>,
    saved: Vec<SavedContinuation<'a>>,
    parent: Option<(ActivationId, TraceLocation)>,
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
        let mut view = inspection(&self.continuation, self.vm.memory.aggs());
        view.ancestors = self
            .parents
            .iter()
            .rev()
            .map(|parent| inspection(parent, self.vm.memory.aggs()))
            .collect();
        view.saved = saved_inspections(&self.resumptions, self.vm.memory.aggs());
        view
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
        let mut view = inspection(self.continuation, self.vm.memory.aggs());
        view.ancestors = self
            .parents
            .iter()
            .rev()
            .map(|parent| inspection(parent, self.vm.memory.aggs()))
            .collect();
        view.saved = saved_inspections(self.resumptions, self.vm.memory.aggs());
        view
    }
}

fn inspection<'a>(
    continuation: &'a continuation::Continuation<'_>,
    aggregates: &'a AggHeap,
) -> InvocationInspection<'a> {
    InvocationInspection {
        program: &continuation.program,
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
        ancestors: Vec::new(),
        saved: Vec::new(),
        parent: continuation.parent.clone(),
    }
}

impl<'a> InvocationInspection<'a> {
    /// Computations retained outside the active call chain. These still contribute object roots.
    pub fn saved_continuations(&self) -> impl ExactSizeIterator<Item = &SavedContinuation<'a>> {
        self.saved.iter()
    }

    fn frame_at(&self, index: usize) -> PausedFrame {
        let frame = &self.ctx.frames[index];
        let activation = frame.activation;
        let child = index + 1 < self.ctx.frames.len();
        let pending = self
            .control
            .waiting
            .host()
            .filter(|pending| !child && pending.depth == self.ctx.frames.len());
        let waiting = child
            || pending.is_some()
            || self
                .control
                .waiting
                .transfer()
                .is_some_and(|pending| pending.depth == self.ctx.frames.len());
        let location = self.control.paused_location(self.ctx, index);
        PausedFrame {
            program: self.program.clone(),
            instance: self.ctx.instance.clone(),
            parent: if index > 0 {
                Some(self.control.activation(self.ctx, index - 1))
            } else {
                self.parent.as_ref().map(|(id, _)| id.clone())
            },
            activation: ActivationId {
                owner: Arc::clone(self.control.owner.as_ref().expect("invocation identity")),
                execution: Arc::clone(self.control.execution.as_ref().expect("execution identity")),
                ordinal: activation.ordinal,
                segment: self.control.segment,
            },
            location,
            caller: activation
                .caller
                .or_else(|| self.parent.as_ref().map(|(_, location)| *location)),
            waiting_for_return: waiting,
            register_count: self.program.program().functions[frame.func.0 as usize].reg_count,
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
        self.ctx
            .frames
            .iter()
            .position(|frame| frame.activation.ordinal == id.ordinal)
    }

    /// Visits active and suspended parent frames, innermost first. Frame metadata is materialized
    /// on demand; this query constructs no verifier or register indexes.
    pub fn frames(&self) -> impl ExactSizeIterator<Item = PausedFrame> + DoubleEndedIterator + '_ {
        let mut frames = Vec::new();
        self.append_frames(&mut frames);
        frames.into_iter()
    }

    fn append_frames(&self, frames: &mut Vec<PausedFrame>) {
        if self.active {
            frames.extend(
                (0..self.ctx.frames.len())
                    .rev()
                    .map(|index| self.frame_at(index)),
            );
        }
        for ancestor in &self.ancestors {
            ancestor.append_frames(frames);
        }
    }

    /// Queries one register. Unknown, retired or foreign activations and absent registers return
    /// `None`. A known declaration without definite initialization returns a value with `None`.
    /// Waiting callers use facts before their pending call, never hypothetical returned values.
    pub fn register(&mut self, id: &ActivationId, register: u32) -> Option<RegisterValue<'a>> {
        let Some(index) = self.index(id) else {
            return self
                .ancestors
                .iter_mut()
                .chain(self.saved.iter_mut().flat_map(|saved| &mut saved.stacks))
                .find_map(|stack| stack.register(id, register));
        };
        let frame = self.frame_at(index);
        let function = self
            .functions
            .entry(frame.location.func.0)
            .or_insert_with(|| FunctionInspection {
                facts: ProgramAnalysis::from(self.program)
                    .function(frame.location.func)
                    .expect("verified function"),
                registers: observations::register_map(self.program.program(), frame.location.func)
                    .expect("verified ledger"),
            });
        let fact = function
            .facts
            .instruction(frame.location.pc)?
            .register(register)?;
        let value = if matches!(fact.before, ValueFact::Contract(_)) {
            let ctx = self.ctx;
            read_value_ref_at(
                &ctx.arena,
                ValueSources::new(self.program.program(), self.args, self.aggregates),
                &ctx.bools,
                &ctx.i64s,
                &ctx.u64s,
                &ctx.f64s,
                &ctx.decimals,
                &ctx.bytes,
                &ctx.strs,
                &ctx.objs,
                &ctx.aggs,
                &ctx.funcs,
                &ctx.closures,
                &ctx.tags,
                &ctx.continuations,
                self.ctx.frames[index].base,
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
        let count = self
            .frames()
            .chain(self.saved.iter().flat_map(|saved| saved.frames()))
            .find(|frame| &frame.activation == id)?
            .register_count;
        let id = id.clone();
        Some((0..count).filter_map(move |register| self.register(&id, register)))
    }

    /// Enumerates conservative host-object references without materializing a root vector.
    ///
    /// Includes initialized active registers, borrowed arguments, pending callback results,
    /// committed outputs and every
    /// VM aggregate field (including retained results and uncommitted temporaries). Duplicates
    /// and no-longer-needed values may occur; this is not a minimal liveness set. No uninitialized
    /// register slots are read. The collector must additionally trace its own heap, globals,
    /// snapshots and other external roots. Object identity, relocation and reclamation belong to
    /// that collector. This method performs no collection or host-state mutation.
    pub fn visit_objects(&mut self, mut visit: impl FnMut(ObjectRoot)) {
        self.visit_objects_dyn(&mut visit, true);
    }

    fn visit_objects_dyn(&mut self, visit: &mut dyn FnMut(ObjectRoot), shared_heap: bool) {
        for stack in self
            .ancestors
            .iter_mut()
            .chain(self.saved.iter_mut().flat_map(|saved| &mut saved.stacks))
        {
            stack.visit_objects_dyn(visit, false);
        }

        for index in (0..if self.active {
            self.ctx.frames.len()
        } else {
            0
        })
            .rev()
        {
            let frame = self.frame_at(index);
            for register in self.registers(&frame.activation).expect("active frame") {
                if let Some(ValueRef::Obj(object)) = register.value {
                    visit(ObjectRoot {
                        object,
                        origin: ObjectRootOrigin::Register {
                            activation: frame.activation.clone(),
                            register: register.register,
                        },
                    });
                }
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
        if shared_heap {
            for (aggregate, index, object) in self.aggregates.object_references() {
                visit(ObjectRoot {
                    object,
                    origin: ObjectRootOrigin::Aggregate { aggregate, index },
                });
            }
        }
    }
}
