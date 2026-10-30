// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Live UI policy over the VM's borrow-scoped scheduling and inspection contract.

use super::{Screen, describe, flow::Flow, palette::*, render, source_context, syntax};
use execution_tape::{
    disasm::disassemble_function,
    host::{Host, ValueRef},
    source::{BoundSources, SourceError, SourceMap},
    trace::TraceLocation,
    value::{AggHandle, AggType, Obj, Value},
    vm::{ActivationId, Invocation, ObjectRootOrigin, PausedFrame, RunStatus, TrapInfo},
};
mod objects;
mod origin;
use origin::RequestOrigin;
mod view;
pub use objects::{ObjectInspector, ObjectView};

use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{collections::BTreeSet, fmt, io};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Stack,
    Code,
    Values,
    Source,
}
impl Focus {
    fn next(self) -> Self {
        match self {
            Self::Stack => Self::Code,
            Self::Code => Self::Values,
            Self::Values => Self::Source,
            Self::Source => Self::Stack,
        }
    }
}

enum Motion {
    Continue,
    Over(ActivationId),
    Out(ActivationId),
}

#[derive(Clone, Copy)]
enum ValueTarget {
    Aggregate(AggHandle),
    Object(Obj),
}

/// A live debugger for an explicitly supplied invocation and host.
///
/// The VM owns execution, resource limits and rollback. This view owns stepping policy,
/// instruction breakpoints and presentation. It never fabricates a host for an arbitrary tape.
/// Default host calls execute synchronously, including their real effects, when stepped or
/// continued. Yielded calls wait for explicit `h` dispatch or embedding-supplied completion.
/// Quitting pauses the invocation; dropping the explorer cancels unfinished execution.
///
/// Paused values are read-only and shown only when definitely initialized. Selecting a caller
/// displays its pending call, not hypothetical return values. Source snapshots are optional and
/// must match the complete program identity. This is live execution, not recording replay.
///
/// ```no_run
/// # use execution_tape::{host::Host, source::SourceMap, value::FuncId,
/// #     verifier::VerifiedProgram, vm::{Vm, InvocationOptions}};
/// # use execution_tape_observatory::LiveExplorer;
/// # fn debug<H: Host>(vm: &mut Vm<H>, program: &VerifiedProgram, entry: FuncId,
/// #     sources: &SourceMap) -> Result<(), Box<dyn std::error::Error>> {
/// let invocation = vm.start(program, entry, &[], InvocationOptions::default())?;
/// let mut debugger = LiveExplorer::new(invocation).sources(sources)?;
/// debugger.run()?;
/// if let Some(result) = debugger.result() {
///     println!("{result:?}");
/// }
/// // Leaving the UI preserves the handle. Dropping it cancels unfinished guest execution.
/// # Ok(()) }
/// ```
pub struct LiveExplorer<'view, 'vm, H: Host> {
    invocation: Invocation<'vm, H>,
    sources: Option<BoundSources<'view>>,
    origin: Option<RequestOrigin<'view>>,
    origin_selected: bool,
    showing_origin: bool,
    frames: Vec<PausedFrame>,
    selected_frame: usize,
    focus: Focus,
    code: Vec<(u32, String)>,
    code_func: Option<execution_tape::value::FuncId>,
    code_selected: usize,
    flow: Flow,
    values: Vec<String>,
    value_targets: Vec<Option<ValueTarget>>,
    value_path: Vec<ValueTarget>,
    object_inspector: Option<Box<dyn ObjectInspector<H> + 'view>>,
    values_offset: usize,
    source_lines: Vec<String>,
    source_offset: usize,
    roots: bool,
    breakpoints: BTreeSet<(u32, u32)>,
    motion: Option<Motion>,
    skip_breakpoint: bool,
    status: String,
}
impl<H: Host> fmt::Debug for LiveExplorer<'_, '_, H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveExplorer")
            .field("invocation", &self.invocation)
            .field("frames", &self.frames.len())
            .field("breakpoints", &self.breakpoints)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl<'view, 'vm, H: Host> LiveExplorer<'view, 'vm, H> {
    /// Opens at the invocation's current safe boundary without executing an instruction.
    #[must_use]
    pub fn new(invocation: Invocation<'vm, H>) -> Self {
        let mut view = Self {
            invocation,
            sources: None,
            origin: None,
            origin_selected: false,
            showing_origin: false,
            frames: Vec::new(),
            selected_frame: 0,
            focus: Focus::Code,
            code: Vec::new(),
            code_func: None,
            code_selected: 0,
            flow: Flow::default(),
            values: Vec::new(),
            value_targets: Vec::new(),
            value_path: Vec::new(),
            object_inspector: None,
            values_offset: 0,
            source_lines: Vec::new(),
            source_offset: 0,
            roots: false,
            breakpoints: BTreeSet::new(),
            motion: None,
            skip_breakpoint: false,
            status: "PAUSED · before next instruction".into(),
        };
        view.refresh();
        view
    }

    /// Attaches exact frontend source snapshots, rejecting stale maps before terminal entry.
    pub fn sources(mut self, map: &'view SourceMap) -> Result<Self, SourceError> {
        self.sources = Some(map.bind(self.invocation.program().program())?);
        self.refresh_source();
        Ok(self)
    }

    /// Adds read-only, on-demand inspection of host objects from the actual invocation host.
    ///
    /// `Enter` follows an object in the Values pane; `Backspace` returns. The inspector runs
    /// only while an object view is open and receives a 256-field limit. Displaying fields
    /// never executes guest instructions or automatically calls host methods. See
    /// [`ObjectInspector`] for lifetime, effect and bounded-materialization requirements.
    #[must_use]
    pub fn object_inspector(mut self, inspector: impl ObjectInspector<H> + 'view) -> Self {
        self.object_inspector = Some(Box::new(inspector));
        self
    }

    /// Opens an embedding-selected host object without executing guest code or retaining it.
    ///
    /// This is also available through `Enter` on an object-valued row. A missing inspector or
    /// unavailable projection is displayed explicitly. `Backspace` returns to paused values.
    pub fn inspect_object(&mut self, object: Obj) {
        self.origin_selected = false;
        self.value_path.clear();
        self.value_path.push(ValueTarget::Object(object));
        self.focus = Focus::Values;
        self.refresh();
    }

    /// Opens the terminal UI. On return, the invocation remains available to inspect or resume.
    ///
    /// `s`: step into, `n`: step over, `u`: step out, `c`: continue, Space: pause,
    /// `b`: toggle the selected instruction's breakpoint, `g`: current instruction,
    /// `h`: dispatch a pending host request through the actual host,
    /// `o`: toggle conservative host-object references, Tab: next pane, `q`: leave.
    /// Continued execution polls input between bounded quanta. A synchronous host call can still
    /// block the UI; no host effects are rolled back by pausing or cancellation.
    pub fn run(&mut self) -> io::Result<()> {
        super::interact(self)
    }

    /// Renders the current view without advancing execution or changing terminal modes.
    /// Dimensions follow [`super::render_text`]. Inspection allocates only on view refresh.
    pub fn render_text(&mut self, width: u16, height: u16) -> io::Result<String> {
        super::render_current(self, width, height)
    }

    /// Executes one guest instruction, entering callees and treating host calls as indivisible.
    pub fn step(&mut self) -> Result<RunStatus, TrapInfo> {
        self.resume(1)
    }

    /// Executes a bounded quantum directly, ignoring UI breakpoints and stepping policy.
    /// Hard budgets remain cumulative. Use the terminal's continue command for breakpoints.
    pub fn resume(&mut self, quantum: u64) -> Result<RunStatus, TrapInfo> {
        self.motion = None;
        self.origin_selected = false;
        if self.showing_origin
            && let Some(frame) = self.frames.first()
        {
            self.show_location(frame.location);
        }
        self.value_path.clear();
        let result = self.invocation.resume(quantum);
        self.status = "PAUSED · before next instruction".into();
        self.selected_frame = 0;
        self.refresh();
        result
    }

    /// Borrows committed outputs or the actual terminal trap; `None` means still paused/running.
    #[must_use]
    pub fn result(&self) -> Option<Result<&[Value], &TrapInfo>> {
        self.invocation.result()
    }

    /// Takes successful owned outputs. Aggregate handles belong to the original borrowed VM.
    pub fn take_outputs(&mut self) -> Option<Vec<Value>> {
        let values = self.invocation.take_outputs();
        self.refresh();
        values
    }

    fn location(&self) -> Option<TraceLocation> {
        self.code_func.and_then(|func| {
            self.code
                .get(self.code_selected)
                .map(|&(pc, _)| TraceLocation { func, pc })
        })
    }

    fn refresh_source(&mut self) {
        self.source_offset = 0;
        let sources = if self.showing_origin {
            self.origin.as_ref().and_then(|origin| origin.sources)
        } else {
            self.sources
        };
        self.source_lines = match (sources, self.location()) {
            (Some(map), Some(loc)) => source_context::context_lines(map, loc, 6, 100),
            _ => vec![
                "No frontend source attached.".into(),
                "Attach an exact SourceMap; span IDs alone are not source locations.".into(),
            ],
        };
    }

    fn show_location(&mut self, location: TraceLocation) {
        self.show_location_in(location, false);
    }
    fn show_location_in(&mut self, location: TraceLocation, origin: bool) {
        let changed = self.code_func != Some(location.func) || self.showing_origin != origin;
        self.showing_origin = origin;
        if changed {
            let (flow, code) = {
                let program = self.code_program();
                match disassemble_function(program, location.func) {
                    Ok(source) => (
                        Flow::new(&source),
                        source
                            .instrs()
                            .map(|i| (i.pc(), describe::instruction(program, i)))
                            .collect(),
                    ),
                    Err(_) => (Flow::default(), Vec::new()),
                }
            };
            self.flow = flow;
            self.code = code;
            self.code_func = Some(location.func);
        }
        self.code_selected = self
            .code
            .iter()
            .position(|&(pc, _)| pc == location.pc)
            .unwrap_or(0);
        self.refresh_source();
    }

    fn refresh(&mut self) {
        self.frames = self.invocation.inspect().frames().collect();
        self.selected_frame = self.selected_frame.min(self.frames.len().saturating_sub(1));
        self.values.clear();
        self.value_targets.clear();
        self.values_offset = 0;
        if let Some(result) = self.invocation.result() {
            self.status = match result {
                Ok(_) => "RETURNED · committed VM outputs".into(),
                Err(error) => format!("TRAPPED · {}", render::trap_text(&error.trap)),
            };
        }
        if let Some(request) = self.invocation.pending_host_call() {
            self.status = format!(
                "AWAITING HOST · {} · h dispatch",
                describe::preview(request.symbol(), 60)
            );
        }
        if self.origin_selected {
            let origin = self.origin.as_mut().expect("selected caller");
            let location = origin.location;
            self.values = origin.values();
            self.value_targets = vec![None; self.values.len()];
            self.show_location_in(location, true);
            return;
        }
        if let Some(result) = self.invocation.result() {
            match result {
                Ok(outputs) => {
                    self.status = "RETURNED · committed VM outputs".into();
                    self.values = outputs
                        .iter()
                        .enumerate()
                        .take(256)
                        .map(|(i, v)| {
                            format!("Output[{i}] = {}", value_text(ValueRef::from_value(v)))
                        })
                        .collect();
                    if outputs.is_empty() {
                        self.values.push("Returned without values.".into());
                    }
                    self.value_targets = outputs
                        .iter()
                        .take(256)
                        .map(|v| value_target(ValueRef::from_value(v)))
                        .collect();
                    if outputs.len() > 256 {
                        self.values.push("… further outputs omitted".into());
                    }
                }
                Err(trap) => {
                    self.status = format!("TRAPPED · {}", render::trap_text(&trap.trap));
                    let location = TraceLocation {
                        func: trap.func,
                        pc: trap.pc,
                    };
                    self.values.push(
                        "No live frames after failure; uncommitted values were discarded.".into(),
                    );
                    self.show_location(location);
                }
            }
        } else if let Some(frame) = self.frames.get(self.selected_frame).cloned() {
            self.show_location(frame.location);
            let mut inspection = self.invocation.inspect();
            if !self.roots
                && let Some(registers) = inspection.registers(&frame.activation)
            {
                for reg in registers.take(256) {
                    self.value_targets.push(reg.value.and_then(value_target));
                    self.values.push(format!(
                        "r{} · {} = {}",
                        reg.register,
                        describe::type_name(reg.storage_kind),
                        reg.value.map_or_else(
                            || "unavailable (not definitely initialized)".into(),
                            value_text
                        )
                    ));
                }
                if frame.register_count > 256 {
                    self.values.push("… further registers omitted".into());
                }
            }
        }
        if self.roots {
            self.values.clear();
            self.value_targets.clear();
            let mut inspection = self.invocation.inspect();
            let mut count = 0_usize;
            inspection.visit_objects(|root| {
                count += 1;
                if self.values.len() < 256 {
                    let origin = match root.origin {
                        ObjectRootOrigin::Register {
                            activation,
                            register,
                        } => format!("frame #{} r{register}", activation.ordinal()),
                        ObjectRootOrigin::Argument { index } => format!("argument[{index}]"),
                        ObjectRootOrigin::Output { index } => format!("output[{index}]"),
                        ObjectRootOrigin::CallbackResult { index } => {
                            format!("callback result[{index}]")
                        }
                        ObjectRootOrigin::Aggregate { aggregate, index } => {
                            format!("aggregate @{} field[{index}]", aggregate.0)
                        }
                    };
                    self.value_targets
                        .push(Some(ValueTarget::Object(root.object)));
                    self.values.push(format!(
                        "{} ← {origin}",
                        value_text(ValueRef::Obj(root.object))
                    ));
                }
            });
            self.value_targets.insert(0, None);
            self.values.insert(
                0,
                format!("{count} references · conservative, duplicates possible"),
            );
            if count > 256 {
                self.values.push("… further references omitted".into());
            }
            self.values
                .push("Add host heap/globals/external roots before collection.".into());
        }
        if let Some(&ValueTarget::Aggregate(handle)) = self.value_path.last() {
            let heap = self.invocation.aggregates();
            self.values.clear();
            self.value_targets.clear();
            let kind = match heap.agg_type(handle) {
                Ok(kind) => kind,
                Err(error) => {
                    self.values
                        .push(format!("Cannot inspect aggregate @{}: {error}", handle.0));
                    return;
                }
            };
            let count = match kind {
                AggType::Tuple { .. } => heap.tuple_len(handle),
                AggType::Struct { .. } => heap.struct_field_count(handle),
                AggType::Array { .. } => heap.array_len(handle),
            }
            .expect("validated aggregate kind");
            for index in 0..count.min(256) {
                let value = match kind {
                    AggType::Tuple { .. } => heap.tuple_get_ref(handle, index),
                    AggType::Struct { .. } => heap.struct_get_ref(handle, index),
                    _ => heap.array_get_ref(handle, index),
                }
                .expect("validated aggregate field");
                self.value_targets
                    .push(value_target(ValueRef::from_value(value)));
                self.values.push(format!(
                    "Item[{index}] = {}",
                    value_text(ValueRef::from_value(value))
                ));
            }
            if count == 0 {
                self.values.push("Empty aggregate.".into());
            }
            if count > 256 {
                self.values.push("… further items omitted".into());
            }
        }
        self.refresh_object();
    }

    fn refresh_object(&mut self) {
        let Some(&ValueTarget::Object(object)) = self.value_path.last() else {
            return;
        };
        self.values.clear();
        self.value_targets.clear();
        let Some(inspector) = self.object_inspector.as_mut() else {
            self.values
                .push("Object state unavailable: no host ObjectInspector attached.".into());
            return;
        };
        let Some(view) = inspector.inspect(self.invocation.host(), object, 256) else {
            self.values
                .push("Object state unavailable from the host inspector.".into());
            return;
        };
        self.values.push(format!(
            "{} · object #{} · host type {}",
            describe::preview(&view.name, 80),
            object.handle.0,
            object.host_type.0
        ));
        self.value_targets.push(None);
        let supplied = view.fields.len();
        for (name, value) in view.fields.into_iter().take(256) {
            let local = matches!(value, Value::Agg(_) | Value::Func(_) | Value::Closure(_));
            self.value_targets.push(if local {
                None
            } else {
                value_target(ValueRef::from_value(&value))
            });
            self.values.push(format!(
                "{} = {}",
                describe::preview(&name, 80),
                if local {
                    "unavailable (VM-local value has no ownership context)".into()
                } else {
                    value_text(ValueRef::from_value(&value))
                }
            ));
        }
        if supplied == 0 && view.total_fields == Some(0) {
            self.values.push("No fields.".into());
        }
        if view
            .total_fields
            .is_some_and(|total| total > supplied.min(256))
            || supplied > 256
        {
            self.values.push(format!(
                "… further fields omitted ({} shown)",
                supplied.min(256)
            ));
        }
        self.values_offset = self.values_offset.min(self.values.len().saturating_sub(1));
    }

    fn start_motion(&mut self, kind: KeyCode) {
        if self.invocation.pending_host_call().is_some() {
            self.refresh();
            return;
        }
        if self.invocation.result().is_some() {
            return;
        }
        self.origin_selected = false;
        if self.showing_origin
            && let Some(frame) = self.frames.first()
        {
            self.show_location(frame.location);
        }
        self.value_path.clear();
        let origin = self
            .frames
            .first()
            .expect("suspended leaf")
            .activation
            .clone();
        self.motion = Some(match kind {
            KeyCode::Char('n') => Motion::Over(origin),
            KeyCode::Char('u') => Motion::Out(origin),
            _ => Motion::Continue,
        });
        self.skip_breakpoint = true;
        self.status = "RUNNING · Space pauses between instructions".into();
    }

    fn advance(&mut self) {
        if self.motion.is_none() {
            return;
        }
        let precise =
            !self.breakpoints.is_empty() || !matches!(self.motion, Some(Motion::Continue));
        for _ in 0..if precise { 512 } else { 1 } {
            let leaf = self.invocation.inspect().frames().next();
            let Some(leaf) = leaf else {
                self.motion = None;
                break;
            };
            if !self.skip_breakpoint
                && self
                    .breakpoints
                    .contains(&(leaf.location.func.0, leaf.location.pc))
            {
                self.motion = None;
                self.status = "PAUSED · instruction breakpoint (before execution)".into();
                break;
            }
            self.skip_breakpoint = false;
            if !matches!(
                self.invocation.resume(if precise { 1 } else { 512 }),
                Ok(RunStatus::Suspended)
            ) {
                self.motion = None;
                break;
            }
            let now = self.invocation.inspect();
            let stop = match &self.motion {
                Some(Motion::Over(id)) => {
                    now.frames().next().is_some_and(|f| &f.activation == id)
                        || !now.frames().any(|f| &f.activation == id)
                }
                Some(Motion::Out(id)) => !now.frames().any(|f| &f.activation == id),
                _ => false,
            };
            if stop {
                self.motion = None;
                self.status = "PAUSED · step completed".into();
                break;
            }
        }
        self.selected_frame = 0;
        self.origin_selected = false;
        self.refresh();
    }

    fn move_selection(&mut self, down: bool, amount: usize) {
        let move_by = |value: &mut usize, count: usize| {
            *value = if down {
                value.saturating_add(amount).min(count.saturating_sub(1))
            } else {
                value.saturating_sub(amount)
            };
        };
        match self.focus {
            Focus::Stack => {
                self.value_path.clear();
                let mut selected = if self.origin_selected {
                    self.frames.len()
                } else {
                    self.selected_frame
                };
                move_by(
                    &mut selected,
                    self.frames.len() + usize::from(self.origin.is_some()),
                );
                self.origin_selected = self.origin.is_some() && selected == self.frames.len();
                if !self.origin_selected {
                    self.selected_frame = selected;
                }
                self.refresh();
            }
            Focus::Code => {
                move_by(&mut self.code_selected, self.code.len());
                self.refresh_source();
            }
            Focus::Values => move_by(&mut self.values_offset, self.values.len()),
            Focus::Source => move_by(&mut self.source_offset, self.source_lines.len()),
        }
    }
}

fn value_text(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Str(text) => format!(
            "\"{}\" · {} UTF-8 bytes",
            describe::preview(text, 80),
            text.len()
        ),
        ValueRef::Bytes(bytes) => format!(
            "{:02x?}{} · {} bytes",
            &bytes[..bytes.len().min(16)],
            if bytes.len() > 16 { "…" } else { "" },
            bytes.len()
        ),
        ValueRef::Unit => "()".into(),
        ValueRef::Bool(value) => value.to_string(),
        ValueRef::I64(value) => value.to_string(),
        ValueRef::U64(value) => value.to_string(),
        ValueRef::F64(value) => value.to_string(),
        ValueRef::Decimal(value) => format!("{}e-{}", value.mantissa(), value.scale()),
        ValueRef::Obj(object) => format!(
            "object #{} · host type {} · Enter to inspect",
            object.handle.0, object.host_type.0
        ),
        ValueRef::Agg(handle) => format!("aggregate @{} · Enter to inspect", handle.0),
        ValueRef::Func(func) => format!("function f{}", func.0),
        ValueRef::Closure(closure) => format!(
            "closure f{} · environment @{}",
            closure.func.0, closure.env.0
        ),
    }
}

fn value_target(value: ValueRef<'_>) -> Option<ValueTarget> {
    match value {
        ValueRef::Agg(handle) => Some(ValueTarget::Aggregate(handle)),
        ValueRef::Closure(c) => Some(ValueTarget::Aggregate(c.env)),
        ValueRef::Obj(object) => Some(ValueTarget::Object(object)),
        _ => None,
    }
}

impl<H: Host> Screen for LiveExplorer<'_, '_, H> {
    fn running(&self) -> bool {
        self.motion.is_some()
    }
    fn tick(&mut self) {
        self.advance();
    }
    fn key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.motion = None;
                if self.invocation.result().is_none() {
                    self.status = if self.invocation.pending_host_call().is_some() {
                        "AWAITING HOST · left terminal view; completion required".into()
                    } else {
                        "PAUSED · left terminal view".into()
                    };
                }
                return false;
            }
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => self.focus = self.focus.next().next().next(),
            KeyCode::Char('s') => {
                let _ = self.step();
            }
            KeyCode::Char('c' | 'n' | 'u') => self.start_motion(key.code),
            KeyCode::Char(' ') => {
                self.motion = None;
                if self.invocation.result().is_none() {
                    self.status = if self.invocation.pending_host_call().is_some() {
                        "AWAITING HOST · completion required; h dispatch".into()
                    } else {
                        "PAUSED · requested by user".into()
                    };
                }
            }
            KeyCode::Char('o') => {
                self.origin_selected = false;
                self.value_path.clear();
                self.roots = !self.roots;
                self.refresh();
            }
            KeyCode::Enter if self.focus == Focus::Values => {
                if let Some(Some(handle)) = self.value_targets.get(self.values_offset) {
                    self.value_path.push(*handle);
                    self.refresh();
                }
            }
            KeyCode::Backspace if self.focus == Focus::Values => {
                self.value_path.pop();
                self.refresh();
            }
            KeyCode::Char('b') => {
                if self.showing_origin {
                    self.status =
                        "External caller belongs to another invocation; g returns to this code."
                            .into();
                    return true;
                }
                if let Some(loc) = self.location() {
                    if !self
                        .invocation
                        .program()
                        .function(loc.func)
                        .is_some_and(|f| f.instructions().any(|i| i.pc == loc.pc))
                    {
                        self.motion = None;
                        if self.invocation.result().is_none() {
                            self.status = "PAUSED · join declaration has no execution step; choose next instruction".into();
                        }
                        return true;
                    }
                    let point = (loc.func.0, loc.pc);
                    if !self.breakpoints.remove(&point) {
                        self.breakpoints.insert(point);
                    }
                }
            }
            KeyCode::Char('g') => {
                self.origin_selected = false;
                self.selected_frame = 0;
                if let Some(frame) = self.frames.first() {
                    self.show_location(frame.location);
                } else if let Some(Err(trap)) = self.invocation.result() {
                    self.show_location(TraceLocation {
                        func: trap.func,
                        pc: trap.pc,
                    });
                }
                self.refresh();
            }
            KeyCode::Char('h') => {
                if let Some(request) = self.invocation.pending_host_call() {
                    let _ = request.dispatch();
                } else {
                    return true;
                }
                self.status = "PAUSED · host instruction completed".into();
                self.motion = None;
                self.origin_selected = false;
                self.value_path.clear();
                self.refresh();
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(true, 1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(false, 1),
            KeyCode::PageDown => self.move_selection(true, 10),
            KeyCode::PageUp => self.move_selection(false, 10),
            _ => {}
        }
        true
    }
    fn draw(&mut self, frame: &mut Frame<'_>) {
        self.draw_view(frame);
    }
}

#[cfg(test)]
mod request_tests;
#[cfg(test)]
mod tests;
