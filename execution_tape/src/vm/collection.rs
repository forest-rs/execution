// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Explicit, planned, moving collection of a VM's value storage.
//!
//! A collection runs only when the embedding asks for one: [`Vm::collect`] between runs, or
//! [`Turn::collect`] while a turn waits. It has two phases.
//!
//! **Planning** reads everything that can hold a value and mutates nothing. It finds the roots:
//! registered groups and, for a turn, its inputs, outputs, callback results and the
//! reference registers of its active frames and of the frames its unresumed continuations hold.
//! A register counts only where the verifier's initialization facts establish it at the point
//! its frame stopped: a register not yet written there retains nothing, whatever stale handle
//! its slot holds. A written register keeps its value alive until it is overwritten, even if it
//! is never read again; liveness is not analyzed. Planning then traces the aggregate heap
//! from those roots, and finds which string and byte descriptors live registers name. Every
//! handle is validated on the way; anything it cannot account for refuses the collection.
//!
//! **Applying** the plan cannot fail. It slides surviving aggregates down in their original
//! order, which preserves sharing and distinct identity and keeps a retained prefix free of
//! references into a later suffix; drops dead descriptors and their owned payloads; and rewrites
//! every root it planned. Registers that hold no value are left alone: they are written before
//! they are read.
//!
//! The root walk is per stack ([`continuation::Continuation`]), so a VM that owns several saved
//! stacks walks each the same way.

use super::*;
use crate::aggregates::{AggRemap, Compaction, PlanError, aggregate_handle};
use crate::arena::{ArenaPlan, DanglingPayload};
use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use inspection::{FrameFacts, FrameRegisters};

/// Why a collection was refused. A refused collection moves and reclaims nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CollectError {
    /// A root, or a field reachable from one, names an aggregate the heap does not hold.
    ///
    /// The heap was changed behind the VM, for example through [`Vm::aggregates_mut`], or a
    /// value carried a handle from another heap.
    DanglingHandle {
        /// The missing aggregate.
        handle: AggHandle,
    },
    /// A live string or byte register names a payload that no longer resolves.
    DanglingPayload,
    /// A frame stopped where verification recorded no register initialization, so which of its
    /// registers hold values is unknown.
    UnknownRegisters {
        /// Where the frame stopped.
        location: TraceLocation,
    },
    /// The waiting host request holds aggregates its host constructed in staging, which only
    /// the host's handles reach.
    ///
    /// Complete the request before collecting.
    HostStaging,
    /// A waiting host request holds handles to aggregates it built and then promoted by calling
    /// back, and the collection would move aggregates.
    ///
    /// Only the host knows those handles, so the VM can neither update them nor tell them apart
    /// from stale ones; it keeps the aggregates alive and refuses to move them. A collection
    /// that reclaims nothing moves nothing and still succeeds. To collect, complete the request
    /// first, or build the values the host will return after its callbacks rather than before.
    /// (A callback-driven host that must collect with values in flight should hold them the
    /// explicit way, as registered values it resolves again, which is the intended direction
    /// rather than any implicit translation.)
    HostHandles,
    /// A saved computation waits at a host request whose host built aggregates and called back,
    /// and the collection would move aggregates.
    ///
    /// This is [`Self::HostHandles`] for a computation [`Turn::finish`] kept: it refuses
    /// every collection that would move anything until [`Vm::resume`] continues it and its
    /// request completes.
    SavedHostHandles {
        /// The saved computation.
        computation: ComputationId,
    },
    /// A saved computation reaches an aggregate the heap does not hold.
    ///
    /// This is [`Self::DanglingHandle`] for a computation [`Turn::finish`] kept: the heap
    /// was changed behind the VM while it was saved.
    SavedDanglingHandle {
        /// The saved computation.
        computation: ComputationId,
        /// The missing aggregate.
        handle: AggHandle,
    },
    /// Delivering a host outcome or a suspension answer was interrupted, for example by a
    /// panicking observer. The turn can only report its trap or be dropped.
    Abandoned,
    /// The surviving aggregates cannot be addressed.
    Aggregate(AggError),
}

impl fmt::Display for CollectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DanglingHandle { handle } => write!(
                f,
                "cannot collect: a root reaches aggregate {} which the heap does not hold",
                handle.0
            ),
            Self::DanglingPayload => {
                f.write_str("cannot collect: a live register names a missing payload")
            }
            Self::UnknownRegisters { location } => write!(
                f,
                "cannot collect: no register initialization is known at unit {} function #{} pc {}",
                location.unit, location.func, location.pc.0
            ),
            Self::HostStaging => {
                f.write_str("cannot collect while a host request holds aggregates only in staging")
            }
            Self::HostHandles => f.write_str(
                "cannot collect: aggregates a waiting host built would move over its handles",
            ),
            Self::SavedHostHandles { computation } => write!(
                f,
                "cannot collect: aggregates the host of saved computation {computation:?} built \
                 would move over its handles; resume it and complete its request first",
            ),
            Self::SavedDanglingHandle {
                computation,
                handle,
            } => write!(
                f,
                "cannot collect: saved computation {computation:?} reaches aggregate {} which \
                 the heap does not hold",
                handle.0
            ),
            Self::Abandoned => f.write_str("cannot collect an abandoned turn"),
            Self::Aggregate(error) => write!(f, "cannot collect: {error}"),
        }
    }
}

impl core::error::Error for CollectError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Aggregate(error) => Some(error),
            _ => None,
        }
    }
}

impl From<PlanError> for CollectError {
    fn from(error: PlanError) -> Self {
        match error {
            PlanError::Dangling(handle) => Self::DanglingHandle { handle },
            PlanError::Aggregate(error) => Self::Aggregate(error),
        }
    }
}

impl From<DanglingPayload> for CollectError {
    fn from(DanglingPayload: DanglingPayload) -> Self {
        Self::DanglingPayload
    }
}

/// What survived a collection and what it reclaimed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Retention {
    /// Kept.
    pub live: u64,
    /// Reclaimed.
    pub reclaimed: u64,
}

impl Retention {
    fn new((live, reclaimed): (u64, u64)) -> Self {
        Self { live, reclaimed }
    }

    fn add(&mut self, (live, reclaimed): (u64, u64)) {
        self.live += live;
        self.reclaimed += reclaimed;
    }
}

/// What one collection did.
///
/// Byte counts are logical: aggregate node headers, field slots and byte/string contents. They
/// exclude allocator overhead and spare capacity, and freed capacity may stay with the VM for
/// reuse rather than return to the allocator.
///
/// The report carries no duration, since the core has no clock. Time a collection around the
/// call; its cost is proportional to the aggregates before collection (`aggregates.live +
/// aggregates.reclaimed`), the fields of the live ones and [`Self::roots`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CollectionReport {
    /// Aggregate nodes.
    pub aggregates: Retention,
    /// Logical bytes of aggregate nodes.
    pub aggregate_bytes: Retention,
    /// Surviving aggregates whose handle changed.
    pub moved: u64,
    /// String and byte descriptors of the registers of the waiting turn, if any, and of
    /// saved computations.
    pub payloads: Retention,
    /// Owned bytes of those descriptors. Borrowed constants, arguments and aggregate fields
    /// own nothing here.
    pub payload_bytes: Retention,
    /// Registered groups still held by a handle.
    pub root_groups: u64,
    /// Groups whose handle was dropped, unregistered by this collection.
    pub released_groups: u64,
    /// Root references traced: registered values, turn inputs, outputs and callback
    /// results, and live reference registers.
    pub roots: u64,
}

/// A VM's value storage, for deciding when to collect.
///
/// Every field is read without walking the heap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct HeapStats {
    /// Aggregate nodes the heap holds, reachable or not.
    pub aggregates: u32,
    /// Retained aggregate nodes that survived the most recent collection, or zero before the
    /// first.
    ///
    /// Between runs that is every survivor. A collection while a turn waits counts only
    /// survivors older than the turn, which its commit or rollback leaves in place; its own
    /// aggregates count as growth. So `aggregates.saturating_sub(live_after_collection)` is what
    /// was allocated since, less what runs discarded, until the heap is cleared directly.
    pub live_after_collection: u32,
    /// Collections run so far.
    pub collections: u64,
    /// Registered groups, including ones whose handle was dropped since the last collection.
    pub root_groups: usize,
    /// Bytes of buffer capacity the VM keeps between runs for its execution state: register
    /// banks, frames, held continuation segments and payload descriptors.
    ///
    /// Runs reuse these buffers rather than allocating them again, so they stay at the largest
    /// size a run needed. This counts capacities, not allocator overhead, and is zero while an
    /// turn has the state on loan. [`Vm::shrink_scratch`] releases them.
    pub scratch_bytes: usize,
    /// Computations saved with [`crate::vm::Turn::finish`] and not yet resumed.
    pub saved_computations: usize,
    /// Bytes of register, frame and argument capacity those computations hold, beyond the
    /// aggregates they reach. Counts capacities, not payloads or allocator overhead.
    pub saved_bytes: usize,
    /// Frames those computations hold, counted against [`crate::vm::Limits::max_frames`].
    pub saved_frames: usize,
}

/// Positions of the reference registers that hold values in one register file.
#[derive(Debug, Default)]
pub(super) struct LiveSlots {
    aggs: Vec<usize>,
    closures: Vec<usize>,
    bytes: Vec<usize>,
    strs: Vec<usize>,
    exns: Vec<usize>,
}

/// A register file's reference banks, borrowed for relocation.
pub(super) struct ReferenceBanks<'a> {
    pub(super) aggs: &'a mut [AggHandle],
    pub(super) closures: &'a mut [Closure],
    pub(super) bytes: &'a mut [BytesHandle],
    pub(super) strs: &'a mut [StrHandle],
    pub(super) exns: &'a mut [Option<Arc<Exception>>],
}

/// Exceptions already relocated, by the original's address, so registers that shared one still
/// share its copy. The original is kept alive until relocation ends so no address is reused.
type Relocated = BTreeMap<usize, (Arc<Exception>, Arc<Exception>)>;

impl ReferenceBanks<'_> {
    fn relocate(
        self,
        live: &LiveSlots,
        remap: &AggRemap,
        arena: &ArenaPlan,
        exceptions: &mut Relocated,
    ) {
        let moved = |handle: AggHandle| {
            remap
                .remap_handle(handle)
                .expect("planning rooted every live register")
        };
        for &index in &live.aggs {
            self.aggs[index] = moved(self.aggs[index]);
        }
        for &index in &live.closures {
            self.closures[index].env = moved(self.closures[index].env);
        }
        for &index in &live.bytes {
            self.bytes[index] = arena.bytes(self.bytes[index]);
        }
        for &index in &live.strs {
            self.strs[index] = arena.str(self.strs[index]);
        }
        for &index in &live.exns {
            let Some(exception) = &mut self.exns[index] else {
                continue;
            };
            // A register sharing an exception already relocated takes that copy. Only then is
            // an unshared exception relocated in place: once one sharer moved to a copy, the
            // original may look unshared to the rest.
            let address = Arc::as_ptr(exception) as usize;
            if let Some((_, relocated)) = exceptions.get(&address) {
                *exception = Arc::clone(relocated);
                continue;
            }
            if let Some(unique) = Arc::get_mut(exception) {
                unique.relocate(remap);
                continue;
            }
            let relocated = Arc::new(exception.relocated(remap));
            let original = core::mem::replace(exception, Arc::clone(&relocated));
            exceptions.insert(address, (original, relocated));
        }
    }
}

impl Registers {
    fn banks_mut(&mut self) -> ReferenceBanks<'_> {
        ReferenceBanks {
            aggs: &mut self.aggs,
            closures: &mut self.closures,
            bytes: &mut self.bytes,
            strs: &mut self.strs,
            exns: &mut self.exns,
        }
    }
}

/// Everything a planned collection of one stack rewrites.
#[derive(Debug, Default)]
pub(super) struct StackPlan {
    active: LiveSlots,
    /// Live registers of each held continuation, by table slot.
    held: Vec<(u32, LiveSlots)>,
    arena: ArenaPlan,
    /// Root references this stack contributed.
    roots: u64,
}

/// Roots found while planning.
struct Roots<'a> {
    handles: &'a mut Vec<AggHandle>,
    /// Descriptors live string and byte registers name.
    bytes: Vec<BytesHandle>,
    strs: Vec<StrHandle>,
    count: u64,
}

impl Roots<'_> {
    fn values(&mut self, values: &[Value]) {
        for value in values {
            self.count += 1;
            self.handles.extend(aggregate_handle(value));
        }
    }
}

/// Records the live reference registers of one frame, and the aggregates they hold.
fn trace_frame(
    facts: &mut FrameFacts<'_>,
    frame: &FrameRegisters<'_>,
    live: &mut LiveSlots,
    roots: &mut Roots<'_>,
) -> Result<(), CollectError> {
    let base = frame.base;
    let known = facts.initialized(frame, |_, vreg| match vreg {
        VReg::Agg(r) => {
            let index = base.aggs + r.0 as usize;
            live.aggs.push(index);
            roots.handles.push(frame.aggs[index]);
            roots.count += 1;
        }
        VReg::Closure(r) => {
            let index = base.closures + r.0 as usize;
            live.closures.push(index);
            roots.handles.push(frame.closures[index].env);
            roots.count += 1;
        }
        VReg::Bytes(r) => {
            let index = base.bytes + r.0 as usize;
            live.bytes.push(index);
            roots.bytes.push(frame.bytes[index]);
            roots.count += 1;
        }
        VReg::Str(r) => {
            let index = base.strs + r.0 as usize;
            live.strs.push(index);
            roots.strs.push(frame.strs[index]);
            roots.count += 1;
        }
        VReg::Exn(r) => {
            let index = base.exns + r.0 as usize;
            live.exns.push(index);
            if let Some(exception) = &frame.exns[index] {
                roots.values(exception.payload());
            }
        }
        _ => {}
    });
    if known {
        Ok(())
    } else {
        Err(CollectError::UnknownRegisters {
            location: frame.location,
        })
    }
}

impl continuation::Stack {
    /// Finds this stack's roots and plans its own relocation, mutating nothing.
    ///
    /// Aggregates the stack keeps alive are appended to `handles`.
    pub(super) fn plan_collection(
        &self,
        args: &[Value],
        units: &UnitTable,
        heap: &AggHeap,
        handles: &mut Vec<AggHandle>,
    ) -> Result<StackPlan, CollectError> {
        let mut roots = Roots {
            handles,
            bytes: Vec::new(),
            strs: Vec::new(),
            count: 0,
        };
        let mut plan = StackPlan::default();
        roots.values(&self.outputs);
        if self.outcome.is_some() {
            // A finished stack keeps only its committed outputs.
            plan.roots = roots.count;
            return Ok(plan);
        }
        let control = &self.control;
        if control
            .pending_host
            .is_some_and(|pending| pending.abandoned)
            || control
                .pending_suspension
                .is_some_and(|pending| pending.abandoned)
        {
            return Err(CollectError::Abandoned);
        }
        if self
            .host_results
            .as_ref()
            .is_some_and(crate::host::HostResults::is_staging)
        {
            return Err(CollectError::HostStaging);
        }
        roots.values(args);
        if let Some(callbacks) = &control.callbacks {
            if let Some(returned) = &callbacks.returned {
                roots.values(returned);
            }
            // Only the host holds handles to aggregates it built before calling back; its
            // request may still return them, so every one stays until the request completes.
            for (_, range) in &callbacks.promoted {
                roots.count += u64::from(range.end - range.start);
                roots.handles.extend(range.clone().map(AggHandle));
            }
        }
        let ctx = &self.ctx;
        let mut facts = FrameFacts::new(units);
        for index in 0..ctx.frames.len() {
            let location = control.paused_location(ctx, index);
            let frame = FrameRegisters::active(ctx, location, ctx.frames[index].base);
            trace_frame(&mut facts, &frame, &mut plan.active, &mut roots)?;
        }
        for (slot, _, frame) in ctx.continuations.held(units) {
            if plan.held.last().is_none_or(|(last, _)| *last != slot) {
                plan.held.push((slot, LiveSlots::default()));
            }
            let live = &mut plan.held.last_mut().expect("pushed above").1;
            trace_frame(&mut facts, &frame, live, &mut roots)?;
        }
        let sources = ValueSources::new(units, args, heap);
        plan.arena = ctx.arena.plan_compaction(
            roots.bytes.drain(..),
            roots.strs.drain(..),
            sources,
            roots.handles,
        )?;
        plan.roots = roots.count;
        Ok(plan)
    }

    /// Refuses a heap plan that would move anything while a waiting host holds handles to
    /// aggregates it built and promoted: only the host knows those handles, and moving them
    /// would leave the host returning or passing a handle that names nothing.
    pub(super) fn check_host_handles(&self, plan: &HeapPlan) -> Result<(), CollectError> {
        let holds = self
            .control
            .callbacks
            .as_ref()
            .is_some_and(|callbacks| !callbacks.promoted.is_empty());
        if holds && plan.compaction().moved() != 0 {
            return Err(CollectError::HostHandles);
        }
        Ok(())
    }

    /// Rewrites every root the plan found, after the heap was compacted with `remap`.
    pub(super) fn relocate(
        &mut self,
        args: &mut Cow<'_, [Value]>,
        plan: &StackPlan,
        compaction: &Compaction,
    ) {
        let remap = compaction.remap();
        let moved = |values: &mut [Value]| {
            remap
                .remap_values_in_place(values)
                .expect("planning rooted every stack value");
        };
        moved(&mut self.outputs);
        if self.outcome.is_some() {
            return;
        }
        if args.iter().any(|value| aggregate_handle(value).is_some()) {
            // Borrowed inputs stay the embedding's; the stack keeps its own relocated copy.
            moved(args.to_mut());
        }
        if let Some(callbacks) = &mut self.control.callbacks {
            // Promoted ranges never move: a plan that would move them was refused.
            if let Some(returned) = &mut callbacks.returned {
                moved(returned);
            }
        }
        let mut exceptions = Relocated::new();
        self.ctx
            .banks_mut()
            .relocate(&plan.active, remap, &plan.arena, &mut exceptions);
        for (slot, live) in &plan.held {
            self.ctx.continuations.banks_mut(*slot).relocate(
                live,
                remap,
                &plan.arena,
                &mut exceptions,
            );
        }
        self.ctx.arena.apply_compaction(&plan.arena, remap);
    }
}

/// A planned collection of a whole VM: its heap compaction and how many roots it found.
pub(super) struct HeapPlan {
    compaction: Compaction,
    roots: u64,
}

impl HeapPlan {
    pub(super) fn compaction(&self) -> &Compaction {
        &self.compaction
    }
}

impl<H: Host> Vm<H> {
    /// Reclaims every aggregate no registered group reaches, moving the survivors together.
    ///
    /// Between runs, registered groups ([`Self::root`]) and saved computations
    /// ([`Turn::finish`]) are the roots: a saved computation keeps what a turn waiting
    /// at the same point would (see [`Turn::collect`]) and is relocated with it. Group
    /// values are updated where aggregates move; read them with [`Self::resolve`]. Every other aggregate
    /// handle copied out of the VM before the call, including a previous run's outputs and an
    /// earlier resolution, is invalid afterwards. Groups whose handle was dropped are
    /// unregistered first, so what only they kept alive is reclaimed.
    ///
    /// Nothing collects implicitly: runs only append aggregates and keep what their outputs
    /// reach. Call this when [`Self::heap_stats`] shows enough growth. The work is explicit
    /// embedding work outside every guest budget, proportional to the heap; it has no latency
    /// bound. To collect while a turn waits, use [`Turn::collect`].
    ///
    /// A saved computation waiting at a host request whose host built aggregates and called back
    /// pins them: every collection that would move anything is refused with
    /// [`CollectError::SavedHostHandles`] until it is resumed and its request completes.
    ///
    /// A refused collection moves and reclaims nothing, not even dropped groups. Separately, and
    /// whether or not the collection then succeeds, it first discards what a forgotten
    /// turn or a run interrupted by a host panic allocated, as any run would.
    pub fn collect(&mut self) -> Result<CollectionReport, CollectError> {
        self.settle();
        let mut handles = Vec::new();
        let saved = self.plan_saved(&mut handles)?;
        let roots = saved.iter().map(StackPlan::roots).sum();
        let plan = self.plan_collection(&mut handles, roots)?;
        self.check_saved(&plan)?;
        // Planning succeeded: nothing below can fail.
        let mut report = self.apply_collection(&plan, None);
        self.relocate_saved(&saved, plan.compaction(), &mut report);
        Ok(report)
    }

    /// Plans the relocation of every saved computation, mutating nothing. What they keep alive
    /// is appended to `handles`.
    pub(super) fn plan_saved(
        &self,
        handles: &mut Vec<AggHandle>,
    ) -> Result<Vec<StackPlan>, CollectError> {
        self.check_pinned();
        let heap = self.memory.aggs();
        self.saved
            .iter()
            .map(|(computation, saved)| {
                let start = handles.len();
                let plan = saved
                    .stack
                    .plan_collection(&saved.args, &self.units, heap, handles)?;
                // Name the computation a dangling root belongs to; heap planning cannot.
                if let Some(&handle) = handles[start..]
                    .iter()
                    .find(|&&handle| heap.agg_type(handle).is_err())
                {
                    return Err(CollectError::SavedDanglingHandle {
                        computation,
                        handle,
                    });
                }
                Ok(plan)
            })
            .collect()
    }

    /// Refuses a heap plan that would move aggregates a saved computation's waiting host built.
    pub(super) fn check_saved(&self, plan: &HeapPlan) -> Result<(), CollectError> {
        self.saved.iter().try_for_each(|(computation, saved)| {
            saved
                .stack
                .check_host_handles(plan)
                .map_err(|error| match error {
                    CollectError::HostHandles => CollectError::SavedHostHandles { computation },
                    error => error,
                })
        })
    }

    /// Rewrites every saved computation's roots after the heap was compacted.
    pub(super) fn relocate_saved(
        &mut self,
        plans: &[StackPlan],
        compaction: &Compaction,
        report: &mut CollectionReport,
    ) {
        for (saved, plan) in self.saved.iter_mut().zip(plans) {
            saved.stack.relocate(&mut saved.args, plan, compaction);
            plan.report(report);
        }
    }

    /// Plans a collection rooted at registered groups and `handles`, which a stack's planning
    /// contributed with `roots` references. Mutates nothing.
    pub(super) fn plan_collection(
        &self,
        handles: &mut Vec<AggHandle>,
        mut roots: u64,
    ) -> Result<HeapPlan, CollectError> {
        for values in self.roots.held() {
            roots += u64::try_from(values.len()).unwrap_or(u64::MAX);
            handles.extend(values.iter().filter_map(aggregate_handle));
        }
        let compaction = self
            .memory
            .aggs()
            .plan_collection(handles.iter().copied(), self.memory.window_limit())?;
        Ok(HeapPlan { compaction, roots })
    }

    /// Applies a heap plan and relocates registered groups. It cannot fail.
    ///
    /// Stacks planned with the heap relocate themselves afterwards.
    ///
    /// `checkpoint` is where a waiting turn's own aggregates begin, if one is collected.
    pub(super) fn apply_collection(
        &mut self,
        plan: &HeapPlan,
        checkpoint: Option<Checkpoint>,
    ) -> CollectionReport {
        let compaction = &plan.compaction;
        let bytes = self.memory.aggs().measure(compaction);
        self.memory.apply_collection(compaction, checkpoint);
        for values in self.roots.held_mut() {
            compaction
                .remap()
                .remap_values_in_place(values)
                .expect("planning traced every registered value");
        }
        let released_groups = self.roots.prune();
        CollectionReport {
            aggregates: Retention::new((
                u64::from(compaction.live()),
                u64::from(compaction.reclaimed()),
            )),
            aggregate_bytes: Retention::new(bytes),
            moved: u64::from(compaction.moved()),
            payloads: Retention::default(),
            payload_bytes: Retention::default(),
            root_groups: u64::try_from(self.roots.held().count()).unwrap_or(u64::MAX),
            released_groups: u64::try_from(released_groups).unwrap_or(u64::MAX),
            roots: plan.roots,
        }
    }

    /// Reports the heap's size and collection history without walking it.
    ///
    /// Aggregates a forgotten turn or a run interrupted by a host panic allocated are
    /// counted until the VM next runs, starts, collects, registers or releases a group, or hands
    /// out [`Self::aggregates_mut`], which discards them first.
    #[must_use]
    pub fn heap_stats(&self) -> HeapStats {
        HeapStats {
            scratch_bytes: self.exec.as_ref().map_or(0, |exec| exec.retained_bytes()),
            saved_computations: self.saved.len(),
            saved_bytes: self.saved.retained_bytes(),
            saved_frames: self.saved.frames(),
            ..self.memory.stats(self.roots.len())
        }
    }
}

impl StackPlan {
    /// Root references the stack contributed.
    pub(super) fn roots(&self) -> u64 {
        self.roots
    }

    /// Adds the stack's payload descriptors to a report.
    pub(super) fn report(&self, report: &mut CollectionReport) {
        report.payloads.add(self.arena.descriptors);
        report.payload_bytes.add(self.arena.owned_bytes);
    }
}

#[cfg(test)]
mod tests;
