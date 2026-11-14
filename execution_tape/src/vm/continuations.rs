// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! One-shot continuations: guest control captured from, and reinstated onto, the stack.
//!
//! The interpreter keeps one contiguous stack of frames and register banks per invocation. A
//! continuation is a *segment* of it: the frames from a resumed root up to a `suspend`, with
//! their registers rebased to zero, owned by an entry of the invocation's continuation table.
//! `suspend` moves that segment off the stack and `resume` moves it back on top of the
//! resumer, so the active stack stays contiguous and every depth-keyed mechanism (unwinding,
//! callbacks, profiling scopes) sees one ordinary stack. `cont.new` builds a segment holding
//! one unstarted frame.
//!
//! Registers name continuations by [`ContRef`]: a table slot and a generation. Consuming a
//! continuation bumps its slot's generation, so every copy of the reference goes stale and
//! one-shot use is a single comparison. The table is invocation-scoped: unresumed
//! continuations are released by `cont.drop` or when the invocation ends. Their frames stay
//! charged to the invocation's frame budget meanwhile, so abandoned continuations cannot grow
//! without bound.
//!
//! The switch points are explicit instructions (`resume`, `resume_throw`, `suspend`), and the
//! handler search only reads frames and their prepared region tables. A native tier can keep
//! the same table, references, typing and handler search while backing each segment with its
//! own machine stack and switching stack pointers instead of moving frames.

use super::*;
use crate::program::{CallSigId, TagId};
use crate::typed::ExecCatch;
use crate::value::TagRef;
use inspection::FrameRegisters;
use units::ListId;

/// A continuation register value: an invocation table slot and its generation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContRef {
    slot: u32,
    generation: u32,
}

impl ContRef {
    /// The value of a continuation register before its first write; it names no continuation.
    pub(crate) const NONE: Self = Self {
        slot: u32::MAX,
        generation: 0,
    };
}

/// Where a continuation receives the values that resume it, in its top frame.
#[derive(Copy, Clone, Debug)]
enum Inputs {
    /// An unstarted continuation receives its entry function's arguments.
    Entry,
    /// A suspended continuation receives its `suspend` instruction's results.
    Results(VRegSlice),
}

/// Frames moved off the stack, with their registers rebased to start at zero.
#[derive(Debug, Default)]
struct Segment {
    frames: Vec<Frame>,
    activations: Vec<inspection::Activation>,
    bools: Vec<bool>,
    i64s: Vec<i64>,
    u64s: Vec<u64>,
    f64s: Vec<f64>,
    w32s: Vec<u32>,
    decimals: Vec<Decimal>,
    bytes: Vec<BytesHandle>,
    strs: Vec<StrHandle>,
    objs: Vec<Obj>,
    aggs: Vec<AggHandle>,
    funcs: Vec<FuncRef>,
    closures: Vec<Closure>,
    exns: Vec<Option<Arc<Exception>>>,
    conts: Vec<ContRef>,
}

#[derive(Debug)]
struct Slot {
    generation: u32,
    /// Whether the slot holds a continuation that can still be consumed.
    live: bool,
    segment: Segment,
    inputs: Inputs,
    /// Leading inputs already written by `cont.new` (a closure environment) or `cont.bind`.
    bound: usize,
    /// The values that resume this continuation and those it finally returns.
    ty: (ListId, ListId),
}

/// An invocation's suspended and unstarted continuations.
///
/// Slots are reused, keeping their segment buffers, so steady suspend/resume cycles do not
/// allocate.
#[derive(Debug, Default)]
pub(super) struct Continuations {
    slots: Vec<Slot>,
    free: Vec<u32>,
    /// Frames held by live continuations; [`Budget::frames`] counts them too.
    frames: usize,
}

macro_rules! for_each_bank {
    ($m:ident) => {
        $m!(
            bools, i64s, u64s, f64s, w32s, decimals, bytes, strs, objs, aggs, funcs, closures,
            exns, conts
        )
    };
}

impl Continuations {
    /// Releases every continuation and its registers, keeping buffer capacity.
    ///
    /// Continuation references never outlive their invocation, so generations restart.
    pub(super) fn clear(&mut self) {
        self.free.clear();
        for (index, slot) in self.slots.iter_mut().enumerate() {
            slot.live = false;
            slot.generation = 0;
            let segment = &mut slot.segment;
            segment.frames.clear();
            segment.activations.clear();
            macro_rules! empty {
                ($($bank:ident),*) => {$( segment.$bank.clear(); )*};
            }
            for_each_bank!(empty);
            self.free
                .push(u32::try_from(index).expect("slot count fits u32"));
        }
        self.frames = 0;
    }

    /// Drops the segment of a consumed slot, returning how many frames it released.
    fn discard(&mut self, slot: u32) -> usize {
        let segment = &mut self.slots[slot as usize].segment;
        let released = segment.frames.len();
        segment.frames.clear();
        segment.activations.clear();
        macro_rules! empty {
            ($($bank:ident),*) => {$( segment.$bank.clear(); )*};
        }
        for_each_bank!(empty);
        self.frames -= released;
        self.release(slot);
        released
    }

    /// Returns a consumed slot for reuse, unless its generations are exhausted.
    ///
    /// A slot whose generation reached `u32::MAX` is retired for the rest of the invocation, so
    /// a stale reference can never match a later continuation in it.
    fn release(&mut self, slot: u32) {
        if self.slots[slot as usize].generation != u32::MAX {
            self.free.push(slot);
        }
    }

    /// Frames held by live continuations.
    pub(super) fn frames(&self) -> usize {
        self.frames
    }

    /// Every frame held by a live continuation, root first within each continuation, with its
    /// activation; an unstarted frame gets one only when first resumed.
    ///
    /// Consumed and dropped continuations hold none: their frames were reinstated or released.
    pub(super) fn held<'a>(
        &'a self,
        units: &'a UnitTable,
    ) -> impl Iterator<Item = (Option<inspection::Activation>, FrameRegisters<'a>)> {
        self.slots
            .iter()
            .filter(|slot| slot.live)
            .flat_map(move |slot| {
                let segment = &slot.segment;
                let top = segment.frames.len() - 1;
                segment
                    .frames
                    .iter()
                    .enumerate()
                    .map(move |(index, frame)| {
                        let body = units
                            .unit(frame.unit)
                            .program
                            .verified(frame.func)
                            .expect("held verified function");
                        let started = !matches!(slot.inputs, Inputs::Entry);
                        // A started frame waits after its `suspend` or pending call.
                        let pc = if started {
                            body.pc_at_ix(frame.instr_ix - 1)
                                .expect("a started frame waits after an instruction")
                        } else {
                            frame.pc
                        };
                        let bound = if index == top {
                            let inputs = match slot.inputs {
                                Inputs::Entry => &body.reg_layout.arg_regs[..],
                                Inputs::Results(results) => body.vregs(results),
                            };
                            &inputs[..slot.bound]
                        } else {
                            &[]
                        };
                        let registers = FrameRegisters {
                            location: TraceLocation {
                                unit: frame.unit,
                                func: frame.func,
                                pc,
                            },
                            started,
                            bound,
                            base: frame.base,
                            objs: &segment.objs,
                            exns: &segment.exns,
                        };
                        (segment.activations.get(index).copied(), registers)
                    })
            })
    }

    /// The live slot `cont` names, or the trap for a consumed or never-created continuation.
    fn live(&self, cont: ContRef) -> Result<&Slot, Trap> {
        self.slots
            .get(cont.slot as usize)
            .filter(|slot| slot.live && slot.generation == cont.generation)
            .ok_or(Trap::ContinuationConsumed)
    }

    /// Marks the live slot `cont` names consumed, so every copy of `cont` goes stale.
    ///
    /// Its segment stays stored until reinstated or discarded.
    fn retire(&mut self, cont: ContRef) {
        let slot = &mut self.slots[cont.slot as usize];
        debug_assert!(
            slot.live && slot.generation == cont.generation,
            "only a live continuation is retired"
        );
        slot.live = false;
        slot.generation = slot.generation.saturating_add(1);
    }

    /// A slot ready to receive a segment.
    fn vacant(&mut self) -> u32 {
        if let Some(slot) = self.free.pop() {
            return slot;
        }
        let slot = u32::try_from(self.slots.len()).expect("frame budget bounds continuations");
        self.slots.push(Slot {
            generation: 0,
            live: false,
            segment: Segment::default(),
            inputs: Inputs::Entry,
            bound: 0,
            ty: (ListId::NONE, ListId::NONE),
        });
        slot
    }
}

impl RegBase {
    fn offset_by(mut self, delta: Self, add: bool) -> Self {
        macro_rules! shift {
            ($($bank:ident),*) => {$(
                self.$bank = if add { self.$bank + delta.$bank } else { self.$bank - delta.$bank };
            )*};
        }
        for_each_bank!(shift);
        self
    }
}

impl Registers {
    /// The register base the next allocated frame would get.
    fn next_base(&self) -> RegBase {
        macro_rules! lens {
            ($($bank:ident),*) => { RegBase { $($bank: self.$bank.len()),* } };
        }
        for_each_bank!(lens)
    }

    /// Where the top frame raises: the instruction it is suspended at or executing.
    ///
    /// After `resume_throw` reinstates a continuation this is its `suspend`; when an unstarted
    /// continuation was discarded instead, it is the `resume_throw` itself.
    pub(super) fn raising_location(&self, units: &UnitTable) -> TraceLocation {
        let frame = self.frames.last().expect("active frame");
        let body = units
            .unit(frame.unit)
            .program
            .verified(frame.func)
            .expect("active verified function");
        TraceLocation {
            unit: frame.unit,
            func: frame.func,
            pc: body
                .pc_at_ix(frame.instr_ix - 1)
                .expect("a frame resumes after its raising instruction"),
        }
    }

    /// Frames retained by this invocation: the active stack and every live continuation.
    pub(super) fn held_frames(&self) -> usize {
        self.frames.len() + self.continuations.frames()
    }

    /// Moves frames `depth..` and their registers into a new continuation in `slot`.
    fn capture(&mut self, depth: usize, slot: u32) {
        let base = self.frames[depth].base;
        let continuations = &mut self.continuations;
        let segment = &mut continuations.slots[slot as usize].segment;
        macro_rules! take {
            ($($bank:ident),*) => {$(
                segment.$bank.clear();
                segment.$bank.extend(self.$bank.drain(base.$bank..));
            )*};
        }
        for_each_bank!(take);
        segment.frames.clear();
        segment
            .frames
            .extend(self.frames.drain(depth..).map(|mut frame| {
                frame.base = frame.base.offset_by(base, false);
                frame
            }));
        continuations.frames += segment.frames.len();
        let slot = &mut continuations.slots[slot as usize];
        slot.live = true;
    }

    /// Moves the segment of `slot` back on top of the stack, returning its root's depth index.
    ///
    /// The slot must already be consumed; its buffers keep their capacity for reuse.
    fn reinstate(&mut self, slot: u32) -> usize {
        let base = self.next_base();
        let root = self.frames.len();
        let continuations = &mut self.continuations;
        let segment = &mut continuations.slots[slot as usize].segment;
        macro_rules! put {
            ($($bank:ident),*) => {$(
                self.$bank.extend(segment.$bank.drain(..));
            )*};
        }
        for_each_bank!(put);
        continuations.frames -= segment.frames.len();
        self.frames
            .extend(segment.frames.drain(..).map(|mut frame| {
                frame.base = frame.base.offset_by(base, true);
                frame
            }));
        continuations.release(slot);
        root
    }

    /// Consumes the continuation `cont`, checking its type against `ty`.
    ///
    /// Returns its slot, whose segment is still stored there, with its inputs and bound count.
    fn consume(
        &mut self,
        cont: ContRef,
        ty: (ListId, ListId),
        signature: CallSigId,
    ) -> Result<(u32, Inputs, usize), Trap> {
        let slot = self.continuations.live(cont)?;
        if slot.ty != ty {
            return Err(Trap::ContinuationSignatureMismatch { signature });
        }
        let (inputs, bound) = (slot.inputs, slot.bound);
        self.continuations.retire(cont);
        Ok((cont.slot, inputs, bound))
    }

    /// Consumes `cont` without resuming it, releasing its frames and registers.
    ///
    /// Any continuation can be dropped, whatever its type; the slot's buffers keep their
    /// capacity for reuse, and its generation moves on so every copy of `cont` goes stale.
    pub(super) fn drop_continuation(
        &mut self,
        budget: &mut Budget,
        cont: ContRef,
    ) -> Result<(), Trap> {
        self.continuations.live(cont)?;
        self.continuations.retire(cont);
        budget.frames -= self.continuations.discard(cont.slot);
        Ok(())
    }

    /// Publishes the continuation just captured into `slot` and returns its reference.
    fn publish(
        &mut self,
        slot: u32,
        inputs: Inputs,
        bound: usize,
        ty: (ListId, ListId),
    ) -> ContRef {
        let entry = &mut self.continuations.slots[slot as usize];
        entry.inputs = inputs;
        entry.bound = bound;
        entry.ty = ty;
        ContRef {
            slot,
            generation: entry.generation,
        }
    }

    /// The registers of the frame at `index` that receive resumption values.
    fn inputs<'p>(&self, units: &'p UnitTable, index: usize, inputs: Inputs) -> &'p [VReg] {
        let frame = &self.frames[index];
        let body = units
            .unit(frame.unit)
            .program
            .verified(frame.func)
            .expect("active verified function");
        match inputs {
            Inputs::Entry => &body.reg_layout.arg_regs,
            Inputs::Results(results) => body.vregs(results),
        }
    }
}

/// A selected `on` clause: the resumer's depth index, the clause, and the resume's result type.
struct Handler {
    resumer: usize,
    clause: ExecCatch,
    results: ListId,
}

impl<H: Host> Vm<H> {
    /// Creates an unstarted continuation of `target`, checked against `call_sig` of `unit`.
    ///
    /// The entry frame is allocated now, so it counts against the frame budget from creation.
    /// A closure's environment is written as its first, already bound, argument.
    pub(super) fn cont_new(
        &self,
        ctx: &mut Registers,
        budget: &mut Budget,
        unit: UnitId,
        call_sig: CallSigId,
        target: FuncRef,
        env: Option<AggHandle>,
    ) -> Result<ContRef, Trap> {
        let (_, _, body) = indirect_target(&self.units, unit, call_sig, target, env.is_some())?;
        if budget.frames >= self.limits.max_call_depth {
            return Err(Trap::CallDepthExceeded);
        }
        let depth = ctx.frames.len();
        let base = ctx.alloc_frame(body);
        if let Some(env) = env {
            ctx.write_inline_value(base, body.reg_layout.arg_regs[0], &Value::Agg(env))?;
        }
        ctx.frames.push(Frame {
            unit: target.unit,
            func: target.func,
            pc: body.pc_at_ix(0).unwrap_or(0),
            instr_ix: 0,
            byte_len: body.byte_len,
            base,
            return_to: None,
        });
        budget.frames += 1;
        let slot = ctx.continuations.vacant();
        ctx.capture(depth, slot);
        let ty = self.units.unit(unit).call_sig_lists(call_sig);
        Ok(ctx.publish(slot, Inputs::Entry, usize::from(env.is_some()), ty))
    }

    /// Binds the leading inputs of `cont` to `args` of the current frame.
    ///
    /// Values bound to a suspended continuation are checked now against `from_sig`, whose
    /// leading arguments are the tag's results; an unstarted continuation checks all its
    /// arguments, bound or not, when it is first resumed.
    pub(super) fn cont_bind(
        &self,
        ctx: &mut Registers,
        budget: &mut Budget,
        location: TraceLocation,
        (from_sig, to_sig): (CallSigId, CallSigId),
        cont: ContRef,
        base: RegBase,
        args: &[VReg],
    ) -> Result<ContRef, Trap> {
        let current = self.units.unit(location.unit);
        let (slot, inputs, bound) =
            ctx.consume(cont, current.call_sig_lists(from_sig), from_sig)?;
        if let Inputs::Results(_) = inputs {
            self.check_signature_args(ctx, budget, location, from_sig, base, args)?;
        }
        // Bind in place: reinstate the segment, write the values, then capture it again.
        let activations =
            core::mem::take(&mut ctx.continuations.slots[slot as usize].segment.activations);
        let root = ctx.reinstate(slot);
        let top = ctx.frames.len() - 1;
        let targets = ctx.inputs(&self.units, top, inputs);
        let top_base = ctx.frames[top].base;
        for (&src, &dst) in args.iter().zip(&targets[bound..]) {
            ctx.copy_vreg(base, src, top_base, dst)?;
        }
        let slot = ctx.continuations.vacant();
        ctx.capture(root, slot);
        ctx.continuations.slots[slot as usize].segment.activations = activations;
        Ok(ctx.publish(
            slot,
            inputs,
            bound + args.len(),
            current.call_sig_lists(to_sig),
        ))
    }

    /// Consumes `cont` and reinstates it on top of the resuming frame.
    ///
    /// `args` of the resuming frame are written to its inputs, and `rets` receive its results
    /// when its root returns. `None` prepares it for an exception instead: the caller raises one
    /// at its suspension point, or at the resumer for an unstarted continuation, which is
    /// discarded because nothing in it can catch yet.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the interpreter state borrowed by other instruction helpers"
    )]
    pub(super) fn resume<P: TracePolicy, const YIELD: bool>(
        &self,
        ctx: &mut Registers,
        budget: &mut Budget,
        trace: &mut TraceCtx<'_>,
        control: &mut ExecutionControl,
        location: TraceLocation,
        call_sig: CallSigId,
        cont: ContRef,
        args: Option<&[VReg]>,
        rets: VRegSlice,
    ) -> Result<(), Trap> {
        let current = self.units.unit(location.unit);
        let (slot, inputs, bound) =
            ctx.consume(cont, current.call_sig_lists(call_sig), call_sig)?;
        let resumer = ctx.frames.len() - 1;
        let resumer_base = ctx.frames[resumer].base;
        if args.is_none() && matches!(inputs, Inputs::Entry) {
            // Nothing ran yet, so nothing can catch: the exception starts at the resumer.
            budget.frames -= ctx.continuations.discard(slot);
            return Ok(());
        }
        let activations =
            core::mem::take(&mut ctx.continuations.slots[slot as usize].segment.activations);
        let root = ctx.reinstate(slot);
        let top = ctx.frames.len() - 1;
        ctx.frames[root].return_to = Some(ReturnTo {
            dst_rets: rets,
            resumed: true,
        });
        if let Some(args) = args {
            let targets = ctx.inputs(&self.units, top, inputs);
            let top_base = ctx.frames[top].base;
            for (&src, &dst) in args.iter().zip(&targets[bound..]) {
                ctx.copy_vreg(resumer_base, src, top_base, dst)?;
            }
            match inputs {
                Inputs::Entry => {
                    let frame = &ctx.frames[top];
                    let (unit, func) = (frame.unit, frame.func);
                    let body = self
                        .units
                        .unit(unit)
                        .program
                        .verified(func)
                        .expect("active verified function");
                    self.check_arg_contracts(ctx, budget, unit, func, body, top_base)?;
                }
                Inputs::Results(_) => {
                    self.check_signature_args(ctx, budget, location, call_sig, resumer_base, args)?;
                }
            }
        }
        if YIELD {
            control.reinstate(activations, location);
        }
        // The root re-enters under this instruction.
        self.reenter_scopes(ctx, trace, root, Some(location));
        Ok(())
    }

    /// Re-enters the scopes of the activations from depth index `root` up, outermost first.
    ///
    /// Each re-enters under the instruction that entered it: the root under `resumer` (`None`
    /// for the invocation's root), the others under their caller's waiting call.
    pub(super) fn reenter_scopes(
        &self,
        ctx: &Registers,
        trace: &mut TraceCtx<'_>,
        root: usize,
        resumer: Option<TraceLocation>,
    ) {
        if !trace.enabled(TraceMask::CALL) {
            return;
        }
        let mut caller = resumer;
        for depth in root..ctx.frames.len() {
            let frame = &ctx.frames[depth];
            let at = TraceLocation {
                unit: frame.unit,
                func: frame.func,
                pc: frame.pc,
            };
            let body = self
                .units
                .unit(frame.unit)
                .program
                .verified(frame.func)
                .expect("active verified function");
            let program = self.units.unit(frame.unit).program.program();
            let span = ctx.span_at(program, at.func, at.pc);
            trace.call_scope_enter_if_enabled(program, depth + 1, at, span, caller);
            caller = Some(TraceLocation {
                pc: frame
                    .instr_ix
                    .checked_sub(1)
                    .and_then(|ix| body.pc_at_ix(ix))
                    .unwrap_or(frame.pc),
                ..at
            });
        }
    }

    /// Suspends to the nearest `on` clause for `tag` and continues at its handler.
    ///
    /// The payload of the current frame is written to the handler's parameters, the frames
    /// above the resumer become a new continuation, and the clause's last parameter receives it.
    /// Returns the handler's location, or `None` when no clause handles it and the invocation
    /// yields it to the embedding instead.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the interpreter state borrowed by other instruction helpers"
    )]
    pub(super) fn suspend<P: TracePolicy>(
        &self,
        ctx: &mut Registers,
        budget: &mut Budget,
        trace: &mut TraceCtx<'_>,
        control: &mut ExecutionControl,
        sources: ValueSources<'_>,
        location: TraceLocation,
        span_id: Option<u64>,
        tag: TagId,
        payload: VRegSlice,
        rets: VRegSlice,
    ) -> Result<Option<TraceLocation>, Trap> {
        let raised = self
            .units
            .unit(location.unit)
            .tag(tag)
            .expect("verified tag");
        let found = self.find_handler(ctx, raised)?;
        let top = ctx.frames.len() - 1;
        let base = ctx.frames[top].base;
        let suspending = self
            .units
            .unit(location.unit)
            .program
            .verified(location.func)
            .expect("active verified function");
        let args = suspending.vregs(payload);
        // The payload is checked whoever handles it, so its trap never depends on whether the
        // invocation yields suspensions.
        self.check_payload(ctx, budget, location, tag, base, args)?;
        let Some(handler) = found else {
            if !control.yield_suspensions {
                return Err(Trap::UnhandledSuspension { tag: raised });
            }
            self.yield_suspension::<P>(
                ctx,
                trace,
                control,
                sources,
                location,
                span_id,
                (tag, raised),
                payload,
                rets,
            );
            return Ok(None);
        };

        let resumer = &ctx.frames[handler.resumer];
        let (resumer_unit, resumer_func, resumer_base) = (resumer.unit, resumer.func, resumer.base);
        let body = self
            .units
            .unit(resumer_unit)
            .program
            .verified(resumer_func)
            .expect("active verified function");
        let params = body.vregs(handler.clause.handler.params);
        let (continuation, payload) = params
            .split_last()
            .expect("verified continuation parameter");
        for (&src, &dst) in args.iter().zip(payload) {
            ctx.copy_vreg(base, src, resumer_base, dst)?;
        }
        let target_ix = handler.clause.handler.target_ix as usize;
        let target = TraceLocation {
            unit: resumer_unit,
            func: resumer_func,
            pc: body.pc_at_ix(target_ix).expect("verified handler entry"),
        };

        // Close the suspended activations' scopes, innermost first, while they are intact.
        P::instr_end(
            trace,
            self.units.unit(location.unit).program.program(),
            Some(target),
        );
        if trace.enabled(TraceMask::CALL) {
            for depth in (handler.resumer + 1..ctx.frames.len()).rev() {
                let program = self.units.unit(ctx.frames[depth].unit).program.program();
                trace.suspend_frame(program, depth + 1, location, span_id);
            }
        }

        let slot = ctx.continuations.vacant();
        ctx.capture(handler.resumer + 1, slot);
        control.suspend(
            handler.resumer + 1,
            &mut ctx.continuations.slots[slot as usize].segment.activations,
        );
        let results = self.units.tag(raised).expect("bound tag").results_list;
        let cont = ctx.publish(slot, Inputs::Results(rets), 0, (results, handler.results));

        let VReg::Cont(param) = *continuation else {
            unreachable!("verified continuation parameter");
        };
        ctx.write_cont(resumer_base, param, cont);
        let resumer = &mut ctx.frames[handler.resumer];
        resumer.instr_ix = target_ix;
        resumer.pc = target.pc;
        Ok(Some(target))
    }

    /// Yields a suspension that reached the invocation's root to the embedding.
    ///
    /// The frames stay in place, waiting at the `suspend` like a frame waiting at a yielded host
    /// call, so inspection still sees them. Their scopes close as suspended, exactly as when a
    /// guest handler captures them, and re-enter when the embedding answers.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the interpreter state borrowed by other instruction helpers"
    )]
    fn yield_suspension<P: TracePolicy>(
        &self,
        ctx: &Registers,
        trace: &mut TraceCtx<'_>,
        control: &mut ExecutionControl,
        sources: ValueSources<'_>,
        location: TraceLocation,
        span_id: Option<u64>,
        (tag, raised): (TagId, TagRef),
        payload: VRegSlice,
        rets: VRegSlice,
    ) {
        // Until the observers below return, the suspension is abandoned: an observer that panics
        // must not leave the invocation resumable after a `suspend` with unwritten results.
        control.pending_suspension = Some(invocation::PendingSuspend {
            location,
            span: span_id,
            tag,
            payload,
            rets,
            depth: ctx.frames.len(),
            abandoned: true,
        });
        let program = self.units.unit(location.unit).program.program();
        // Control leaves the `suspend` for the embedding, which chooses what runs next.
        P::instr_end(trace, program, None);
        if P::values_enabled(trace) {
            let body = self
                .units
                .unit(location.unit)
                .program
                .verified(location.func)
                .expect("active verified function");
            let base = &ctx.frames[ctx.frames.len() - 1].base;
            let values: Vec<_> =
                invocation::read_registers(ctx, sources, base, body.vregs(payload)).collect();
            P::values(trace, program, || ValueEvent::SuspensionYielded {
                location,
                tag: raised,
                local: tag,
                payload: &values,
                aggregates: self.memory.aggs(),
            });
        }
        if trace.enabled(TraceMask::CALL) {
            for depth in (0..ctx.frames.len()).rev() {
                let program = self.units.unit(ctx.frames[depth].unit).program.program();
                trace.suspend_frame(program, depth + 1, location, span_id);
            }
        }
        if let Some(pending) = &mut control.pending_suspension {
            pending.abandoned = false;
        }
    }

    /// Finds the innermost `resume` whose `on` clauses handle `tag`.
    ///
    /// Only frames above a resumed root are suspendable. Reaching the stack's root ends the
    /// search with `None`; reaching the root of a guest callback traps.
    fn find_handler(&self, ctx: &Registers, tag: TagRef) -> Result<Option<Handler>, Trap> {
        for index in (1..ctx.frames.len()).rev() {
            let Some(return_to) = ctx.frames[index].return_to else {
                return Err(Trap::SuspensionAcrossHost { tag });
            };
            if !return_to.resumed {
                continue;
            }
            let resumer = &ctx.frames[index - 1];
            let unit = self.units.unit(resumer.unit);
            let body = unit
                .program
                .verified(resumer.func)
                .expect("active verified function");
            let raising = resumer.instr_ix - 1;
            let call_sig = match body.fetch_at_ix(raising) {
                Some((
                    ExecInstr::Resume { call_sig, .. } | ExecInstr::ResumeThrow { call_sig, .. },
                    _,
                    _,
                )) => *call_sig,
                _ => unreachable!("a resumed root's caller waits at resume"),
            };
            let clause = body.protection.as_deref().and_then(|protection| {
                protection.handle(raising, |local| unit.tag(local) == Some(tag))
            });
            if let Some(clause) = clause {
                return Ok(Some(Handler {
                    resumer: index - 1,
                    clause,
                    results: unit.call_sig_lists(call_sig).1,
                }));
            }
        }
        Ok(None)
    }

    /// Checks a suspension payload against its tag's refined payload contracts.
    fn check_payload(
        &self,
        ctx: &Registers,
        budget: &mut Budget,
        location: TraceLocation,
        tag: TagId,
        base: RegBase,
        args: &[VReg],
    ) -> Result<(), Trap> {
        let program = self.units.unit(location.unit).program.program();
        let entry = program.tag(tag).expect("verified tag");
        let contracts = program
            .tag_payload_contracts(entry)
            .expect("verified tag payload");
        for (index, (&register, &contract)) in args.iter().zip(contracts).enumerate() {
            if matches!(program.contracts.get(contract), Some(Contract::Kind(_))) {
                continue;
            }
            let value = Self::contract_value(ctx, base, register);
            budget
                .contract_scratch
                .check_value(
                    &self.units,
                    location.unit,
                    self.memory.aggs(),
                    &value,
                    contract,
                    self.limits.max_contract_work,
                )
                .map_err(|error| {
                    let position = ContractPosition::Payload {
                        tag,
                        index: u32::try_from(index).expect("verified payload arity"),
                    };
                    self.contract_trap(FuncRef::new(location.unit, location.func), position, error)
                })?;
        }
        Ok(())
    }

    /// Checks values resuming a suspension against the resume signature's refined arguments.
    fn check_signature_args(
        &self,
        ctx: &Registers,
        budget: &mut Budget,
        location: TraceLocation,
        call_sig: CallSigId,
        base: RegBase,
        args: &[VReg],
    ) -> Result<(), Trap> {
        let unit = location.unit;
        let program = self.units.unit(unit).program.program();
        let entry = program.call_sig(call_sig).expect("verified call signature");
        let contracts = program
            .call_sig_arg_contracts(entry)
            .expect("verified call signature");
        for (index, (&register, &contract)) in args.iter().zip(contracts).enumerate() {
            if matches!(program.contracts.get(contract), Some(Contract::Kind(_))) {
                continue;
            }
            let value = Self::contract_value(ctx, base, register);
            budget
                .contract_scratch
                .check_value(
                    &self.units,
                    unit,
                    self.memory.aggs(),
                    &value,
                    contract,
                    self.limits.max_contract_work,
                )
                .map_err(|error| {
                    let position = ContractPosition::IndirectArgument {
                        signature: call_sig,
                        index: u32::try_from(index).expect("verified signature arity"),
                    };
                    let function = FuncRef::new(unit, location.func);
                    self.contract_trap(function, position, error)
                })?;
        }
        Ok(())
    }
}

impl TraceCtx<'_> {
    /// Closes the scope of an activation whose continuation suspended.
    fn suspend_frame(
        &mut self,
        program: &Program,
        depth: usize,
        location: TraceLocation,
        span: Option<u64>,
    ) {
        if matches!(self.scopes.last(), Some((ScopeKind::CallFrame { .. }, _))) {
            let (kind, entered_depth) = self.scopes.pop().expect("entered guest scope");
            debug_assert_eq!(
                depth, entered_depth,
                "exit scopes follow actual guest activations"
            );
            if let Some(sink) = self.sink.as_mut() {
                sink.scope_exit(
                    program,
                    kind,
                    depth,
                    location,
                    span,
                    TraceOutcome::Suspended,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_slots_are_retired_instead_of_reused() {
        let mut table = Continuations::default();
        let fresh = table.vacant();
        let exhausted = table.vacant();
        table.slots[exhausted as usize].generation = u32::MAX;
        table.release(fresh);
        table.release(exhausted);
        assert_eq!(table.free, [fresh]);
        // A new invocation restarts generations, so the slot serves again.
        table.clear();
        assert_eq!(table.slots[exhausted as usize].generation, 0);
        assert_eq!(table.free.len(), 2);
    }
}
