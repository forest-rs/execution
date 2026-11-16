// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Optional live inspection.
//!
//! Which registers a paused frame may show comes from the initialization table the verifier
//! retains for every function; inspection never repeats a function analysis.

use super::*;
use crate::analysis::bitset::BitSet;
use crate::program::Reg;
use crate::value::Obj;
use alloc::{collections::BTreeMap, sync::Arc};
use core::hash::{Hash, Hasher};

/// Identifies one computation for its whole life: the same in every invocation that runs it,
/// whichever [`ComputationId`] each of its saves receives.
///
/// It names the computation's stack and its VM. Like a [`ComputationId`],
/// it is a process-local handle, and an identity that outlives its VM could match a later VM
/// that reuses the address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StackId {
    vm: usize,
    stack: u64,
}

impl StackId {
    pub(super) fn new(vm: &Arc<()>, control: &ExecutionControl) -> Self {
        Self {
            vm: Arc::as_ptr(vm).addr(),
            stack: control.owner(),
        }
    }
}

/// An activation identity valid only in its originating invocation.
///
/// It names its VM, which cloning retains so address reuse cannot make an old ID refer to
/// another VM, and the invocation within that VM. Inspection rejects IDs from other
/// invocations, even when their local ordinals match. IDs are process-local handles, not
/// persistent archive identities.
#[derive(Clone)]
pub struct ActivationId {
    vm: Arc<()>,
    stack: u64,
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
        self.ordinal == other.ordinal
            && self.stack == other.stack
            && Arc::ptr_eq(&self.vm, &other.vm)
    }
}
impl Eq for ActivationId {}
impl Hash for ActivationId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.vm).hash(state);
        self.stack.hash(state);
        self.ordinal.hash(state);
    }
}
impl fmt::Debug for ActivationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActivationId")
            .field("vm", &Arc::as_ptr(&self.vm))
            .field("invocation", &self.stack)
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
    /// The VM-assigned identity of the stack this control drives; zero for synchronous runs,
    /// which hand out no activation identities and hold no proofs across stacks.
    owner: u64,
    frames: Vec<Activation>,
    last: u64,
}
impl ExecutionControl {
    /// Bytes of activation capacity held.
    pub(super) fn retained_bytes(&self) -> usize {
        capacity_bytes(&self.frames)
    }

    /// The caller location of the activation at `index`; `None` for a root.
    pub(super) fn caller(&self, index: usize) -> Option<TraceLocation> {
        self.frames.get(index).and_then(|frame| frame.caller)
    }

    pub(super) fn owner(&self) -> u64 {
        debug_assert_ne!(self.owner, 0, "resumable control identity");
        self.owner
    }

    /// Resets this control, keeping its buffers, to start a stack identified by `owner`.
    pub(super) fn start(&mut self, owner: u64, yield_host_calls: bool, yield_suspensions: bool) {
        self.restart(owner);
        self.yield_host_calls = yield_host_calls;
        self.yield_suspensions = yield_suspensions;
        self.frames.push(Activation {
            ordinal: 0,
            caller: None,
        });
    }

    /// Sets how a resumed lease handles admitted host calls and unhandled suspensions.
    ///
    /// A request or suspension already waiting stays waiting either way.
    pub(super) fn set_yields(&mut self, yield_host_calls: bool, yield_suspensions: bool) {
        self.yield_host_calls = yield_host_calls;
        self.yield_suspensions = yield_suspensions;
    }

    /// Resets this control, keeping its buffers, for a synchronous run.
    pub(super) fn start_run(&mut self) {
        self.restart(0);
    }

    fn restart(&mut self, owner: u64) {
        self.remaining = 0;
        self.yield_host_calls = false;
        self.yield_suspensions = false;
        self.pending_host = None;
        self.pending_suspension = None;
        self.callbacks = None;
        self.owner = owner;
        self.frames.clear();
        self.last = 0;
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
                pc: Pc(frame.pc),
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
    pub register: Reg,
    /// Fixed declared kind, including for unavailable values.
    pub storage_kind: ValueType,
    /// Borrowed value only when definite initialization is established at this boundary.
    /// `None` does not expose a bank default or a value left by an earlier activation.
    /// A path-specific value that is not definitely initialized is intentionally unavailable.
    pub value: Option<ValueRef<'a>>,
}

/// Where a conservatively retained host-object reference occurs.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObjectRootOrigin {
    /// An initialized register of an active frame.
    ///
    /// An exception reference register contributes the objects in its exception's payload.
    Register {
        /// Activation containing the register.
        activation: ActivationId,
        /// Source register number.
        register: Reg,
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
        register: Reg,
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
    /// The frame's register base within the banks below.
    pub(super) base: RegBase,
    /// Object registers.
    pub(super) objs: &'a [Obj],
    /// Exception reference registers.
    pub(super) exns: &'a [Option<Arc<Exception>>],
    /// Aggregate registers.
    pub(super) aggs: &'a [AggHandle],
    /// Closure registers.
    pub(super) closures: &'a [Closure],
    /// Byte string registers, naming payload descriptors.
    pub(super) bytes: &'a [BytesHandle],
    /// String registers, naming payload descriptors.
    pub(super) strs: &'a [StrHandle],
}

impl<'a> FrameRegisters<'a> {
    /// The banks of the active stack, for the frame stopped at `location` with `base`.
    pub(super) fn active(ctx: &'a Registers, location: TraceLocation, base: RegBase) -> Self {
        Self {
            location,
            started: true,
            bound: &[],
            base,
            objs: &ctx.objs,
            exns: &ctx.exns,
            aggs: &ctx.aggs,
            closures: &ctx.closures,
            bytes: &ctx.bytes,
            strs: &ctx.strs,
        }
    }
}

/// Lazily built register maps and initialization facts, keyed by code location.
///
/// Inspection and collection share them: a register holds a value exactly when the verifier's
/// facts establish its initialization where the frame stopped, or when it was bound since.
/// Registers outside that set may hold anything, and are never read before they are written.
pub(super) struct FrameFacts<'a> {
    units: &'a UnitTable,
    /// Source-to-bank register maps, keyed by unit and function: each frame is read with its
    /// own unit's program.
    functions: BTreeMap<(u32, u32), Vec<VReg>>,
    /// Registers definitely initialized at a location, keyed by unit, function and PC; `None`
    /// when the PC is not an instruction boundary.
    locations: BTreeMap<(u32, u32, u32), Option<BitSet>>,
}

impl<'a> FrameFacts<'a> {
    pub(super) fn new(units: &'a UnitTable) -> Self {
        Self {
            units,
            functions: BTreeMap::new(),
            locations: BTreeMap::new(),
        }
    }

    /// The register mapping of `location`'s function and, for a frame that has run, the
    /// registers definitely initialized before `location`, both built on first use.
    ///
    /// The initialization is `None` for an unstarted frame and for a PC that is not an
    /// instruction boundary.
    fn facts(&mut self, location: TraceLocation, started: bool) -> (&[VReg], Option<&BitSet>) {
        let program: &'a VerifiedProgram = &self.units.unit(location.unit).program;
        let TraceLocation { unit, func, pc } = location;
        let registers = self.functions.entry((unit.0, func.0)).or_insert_with(|| {
            observations::register_map(program.program(), func).expect("verified ledger")
        });
        if !started {
            return (registers, None);
        }
        let initialized = self
            .locations
            .entry((unit.0, func.0, pc.0))
            .or_insert_with(|| {
                let source = program.program();
                let definition = &source.functions[func.0 as usize];
                program.init_table(func).expect("verified function").before(
                    definition.bytecode(source).expect("verified bytecode"),
                    definition.reg_count as usize,
                    pc.0,
                )
            });
        (registers, initialized.as_ref())
    }

    /// Visits the bank register behind each source register of `frame` that holds a value,
    /// with its source register number.
    ///
    /// Returns `false`, having visited only bound registers, when the frame has run but its
    /// location has no initialization facts.
    pub(super) fn initialized(
        &mut self,
        frame: &FrameRegisters<'_>,
        mut visit: impl FnMut(u32, VReg),
    ) -> bool {
        let (registers, facts) = self.facts(frame.location, frame.started);
        for (register, &vreg) in (0_u32..).zip(registers.iter()) {
            if frame.bound.contains(&vreg)
                || facts.is_some_and(|facts| facts.get(register as usize))
            {
                visit(register, vreg);
            }
        }
        !frame.started || facts.is_some()
    }
}

/// The live register banks and the value sources their handles resolve against.
#[derive(Clone, Copy)]
struct LiveRegisters<'a> {
    ctx: &'a Registers,
    sources: ValueSources<'a>,
}
impl<'a> LiveRegisters<'a> {
    /// Reads a register of the frame with `base`.
    fn read(self, base: RegBase, register: VReg) -> Option<ValueRef<'a>> {
        let ctx = self.ctx;
        read_value_ref_at(
            &ctx.arena,
            self.sources,
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
            base,
            register,
        )
        .ok()
    }
}

/// Borrowed live state plus lazily built register indexes.
///
/// Only queried functions and locations allocate mapping data; the initialization facts behind
/// it are retained by verification. Holding this view prevents resumption or mutation of the
/// host. Reuse it for multiple queries at one suspension; discard it before resuming. Terminal
/// invocations expose no live frames, but retained heaps and outputs can still contribute object
/// roots. Source connections use each frame's exact `location`.
pub struct InvocationInspection<'a> {
    vm: &'a Arc<()>,
    units: &'a UnitTable,
    ctx: &'a Registers,
    args: &'a [Value],
    outputs: &'a [Value],
    aggregates: &'a AggHeap,
    control: &'a ExecutionControl,
    active: bool,
    facts: FrameFacts<'a>,
}
impl fmt::Debug for InvocationInspection<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InvocationInspection")
            .field("active", &self.active)
            .field("indexed_functions", &self.facts.functions.len())
            .field("indexed_locations", &self.facts.locations.len())
            .finish_non_exhaustive()
    }
}

impl<H: Host> Invocation<'_, H> {
    /// Borrows paused state. Register indexes are built only for queried functions.
    #[must_use]
    pub fn inspect(&self) -> InvocationInspection<'_> {
        inspection(&self.continuation, &self.vm)
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
        inspection(self.continuation, self.vm)
    }
}

fn inspection<'a, H: Host>(
    continuation: &'a continuation::Continuation<'_>,
    vm: &'a Vm<H>,
) -> InvocationInspection<'a> {
    let units = &vm.units;
    InvocationInspection {
        vm: &vm.identity,
        units,
        ctx: &continuation.stack.ctx,
        args: &continuation.args,
        outputs: continuation
            .stack
            .outcome
            .as_ref()
            .and_then(|outcome| outcome.as_ref().ok())
            .map_or(&[][..], |()| continuation.stack.outputs.as_slice()),
        aggregates: vm.memory.aggs(),
        control: &continuation.stack.control,
        active: continuation.stack.outcome.is_none(),
        facts: FrameFacts::new(units),
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
    /// The borrowed state register values are read from, apart from this view's indexes.
    fn live(&self) -> LiveRegisters<'a> {
        LiveRegisters {
            ctx: self.ctx,
            sources: ValueSources::new(self.units, self.args, self.aggregates),
        }
    }
    fn activation_id(&self, activation: Activation) -> ActivationId {
        ActivationId {
            vm: Arc::clone(self.vm),
            stack: self.control.owner(),
            ordinal: activation.ordinal,
        }
    }
    fn index(&self, id: &ActivationId) -> Option<usize> {
        if !self.active || self.control.owner != id.stack || !Arc::ptr_eq(self.vm, &id.vm) {
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
    pub fn register(&mut self, id: &ActivationId, register: Reg) -> Option<RegisterValue<'a>> {
        let index = self.index(id)?;
        let location = self.control.paused_location(self.ctx, index);
        let base = self.ctx.frames[index].base;
        let storage_kind = self.register_types(location).get(register)?;
        let (registers, initialized) = self.facts.facts(location, true);
        let initialized = initialized?.get(register.0 as usize);
        let vreg = *registers.get(register.0 as usize)?;
        Some(RegisterValue {
            register,
            storage_kind,
            value: if initialized {
                self.live().read(base, vreg)
            } else {
                None
            },
        })
    }

    /// Iterates declarations in source-register order, including explicit unavailable values.
    ///
    /// The frame is resolved once; each register then costs a bit test and a read. The first
    /// query of a function or location in this view builds its index.
    pub fn registers(
        &mut self,
        id: &ActivationId,
    ) -> Option<impl Iterator<Item = RegisterValue<'a>> + '_> {
        let index = self.index(id)?;
        let location = self.control.paused_location(self.ctx, index);
        let base = self.ctx.frames[index].base;
        let kinds = self.register_types(location);
        let live = self.live();
        let (registers, initialized) = self.facts.facts(location, true);
        let values = initialized.map(|initialized| {
            (0_u32..)
                .zip(registers)
                .map(move |(register, &vreg)| RegisterValue {
                    register: Reg(register),
                    storage_kind: kinds.get(Reg(register)).expect("verified register"),
                    value: if initialized.get(register as usize) {
                        live.read(base, vreg)
                    } else {
                        None
                    },
                })
        });
        Some(values.into_iter().flatten())
    }

    /// Declared register kinds of `location`'s function.
    fn register_types(&self, location: TraceLocation) -> crate::program::RegisterTypes<'a> {
        let program = self.units.unit(location.unit).program.program();
        program.functions[location.func.0 as usize].verified_register_types(program)
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
    ///
    /// The registers it reads are those [`Invocation::collect`] treats as roots. Aggregate fields
    /// are listed for every node, reachable or not; collecting first leaves only the nodes the
    /// VM's roots reach, which narrows this inventory to their objects.
    pub fn visit_objects(&mut self, mut visit: impl FnMut(ObjectRoot)) {
        let ctx = self.ctx;
        for index in (0..if self.active { ctx.frames.len() } else { 0 }).rev() {
            let frame = self.frame_at(index);
            let registers = FrameRegisters::active(ctx, frame.location, ctx.frames[index].base);
            self.visit_frame(
                registers,
                |register| ObjectRootOrigin::Register {
                    activation: frame.activation.clone(),
                    register: Reg(register),
                },
                &mut visit,
            );
        }
        if self.active {
            for (_, activation, registers) in ctx.continuations.held(self.units) {
                let activation = activation.map(|activation| self.activation_id(activation));
                let location = registers.location;
                self.visit_frame(
                    registers,
                    |register| ObjectRootOrigin::Continuation {
                        activation: activation.clone(),
                        location,
                        register: Reg(register),
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
        // Inventory is conservative but never reads an unestablished register: a frame
        // without facts contributes only its bound registers.
        self.facts.initialized(&frame, |register, vreg| match vreg {
            VReg::Obj(r) => visit(ObjectRoot {
                object: frame.objs[frame.base.objs + r.0 as usize],
                origin: origin(register),
            }),
            VReg::Exn(r) => {
                let Some(exception) = &frame.exns[frame.base.exns + r.0 as usize] else {
                    return;
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
        });
    }
}
