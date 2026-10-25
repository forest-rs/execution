// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Application-level evidence: edit transactions, execution bodies, graph publications, and
//! explicitly registered ownership roots. Footfall durations are wall time, not CPU time.

use super::{Captured, Compiled, Graph, Mesh, SOURCE, SceneError, SceneValue};
use execution_graph::{GraphError, NodeId, RunDetailReport};
use footfall::analysis::{Comparison, GroupBy, Population, Selection, Trace};
use footfall::runtime::Runtime;
use footfall::session::capture::{CaptureReader, CaptureRecorder};
use footfall::{
    Args, ContextHandle, EventDesc, FlowId, LabelKey, RegisteredContext, ResourceId,
    ResourceKindDesc, ScopeDesc, ScopeGuard,
};
use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{self, Write as _},
    fs::{self, File},
    io::BufReader,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

static WORKLOAD: LabelKey = LabelKey::new("workload");
static PROGRAM: LabelKey = LabelKey::new("program");
static EDIT: ScopeDesc = ScopeDesc::new("procedural.edit");
static TAPE: ScopeDesc = ScopeDesc::new("procedural.tape");
static GRID: ScopeDesc = ScopeDesc::new("procedural.grid");
static DEFORM: ScopeDesc = ScopeDesc::new("procedural.deform");
static BOUNDS: ScopeDesc = ScopeDesc::new("procedural.bounds");
static HOST: ScopeDesc = ScopeDesc::new("procedural.quality");
static BODY: EventDesc = EventDesc::new("procedural.body_outcome");
static PUBLISHED: EventDesc = EventDesc::new("procedural.published");
static CAUSE: EventDesc = EventDesc::new("procedural.cause");
static CUTOFF: EventDesc = EventDesc::new("procedural.cutoff");
static FAILED: EventDesc = EventDesc::new("procedural.failed");
static ROOT: EventDesc = EventDesc::new("procedural.root");
static CHECKPOINT: EventDesc = EventDesc::new("procedural.storage");
static RECORDING: EventDesc = EventDesc::new("procedural.recording");
static POINTS: ResourceKindDesc = ResourceKindDesc::new("procedural.points");
static TOPOLOGY: ResourceKindDesc = ResourceKindDesc::new("procedural.topology");

// Each closed edit is flushed separately. Keep the window small enough that the integration
// test also exercises exporting history that the runtime no longer retains.
pub(super) const EVENT_CAPACITY: usize = 256;

struct Inner {
    context: RegisteredContext,
    edit: AtomicU64,
    next_edit: AtomicU64,
    next_buffer: AtomicU64,
    live: AtomicU64,
    peak: AtomicU64,
}
impl Inner {
    fn context(&self) -> ContextHandle {
        self.context.handle()
    }
}
#[derive(Clone, Default)]
pub(super) struct Probe(Option<Arc<Inner>>);

pub(super) struct Scope {
    scope: Option<ScopeGuard>,
    transaction: Option<(Probe, u64)>,
}
impl Drop for Scope {
    fn drop(&mut self) {
        if let Some((probe, edit)) = self.transaction.take() {
            let inner = probe.0.as_ref().unwrap();
            footfall::flow_end(FlowId::new(edit), inner.context());
            drop(self.scope.take());
            footfall::transaction_end(inner.context());
            inner.edit.store(0, Ordering::Relaxed);
        } else {
            drop(self.scope.take());
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub(super) enum Buffer {
    Points,
    Topology,
}
impl Buffer {
    fn desc(self) -> &'static ResourceKindDesc {
        match self {
            Self::Points => &POINTS,
            Self::Topology => &TOPOLOGY,
        }
    }
}
pub(super) struct Lease {
    probe: Probe,
    id: u64,
    bytes: u64,
    kind: Buffer,
}
impl fmt::Debug for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BufferLease")
            .field("id", &self.id)
            .field("bytes", &self.bytes)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}
impl Lease {
    pub(super) fn id(&self) -> u64 {
        self.id
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(inner) = &self.probe.0 {
            footfall::resource_drop(self.kind.desc(), ResourceId::new(self.id), inner.context());
            inner.live.fetch_sub(self.bytes, Ordering::Relaxed);
        }
    }
}
impl Probe {
    pub(super) fn enabled(&self) -> bool {
        self.0.is_some()
    }
    pub(super) fn edit(&self, kind: &'static str) -> Scope {
        let Some(inner) = &self.0 else {
            return Scope {
                scope: None,
                transaction: None,
            };
        };
        assert_eq!(
            inner.edit.load(Ordering::Relaxed),
            0,
            "example edits are sequential"
        );
        let edit = inner.next_edit.fetch_add(1, Ordering::Relaxed);
        inner.edit.store(edit, Ordering::Relaxed);
        footfall::transaction_begin(inner.context());
        let scope = Some(footfall::scope_with(
            &EDIT,
            inner.context(),
            &Args::none().with("edit", edit).with("kind", kind),
        ));
        footfall::flow_begin(FlowId::new(edit), inner.context());
        Scope {
            scope,
            transaction: Some((self.clone(), edit)),
        }
    }
    pub(super) fn node(
        &self,
        name: &'static str,
        node: NodeId,
        attempt: u64,
        items: usize,
        recorded: bool,
    ) -> Scope {
        let Some(inner) = &self.0 else {
            return Scope {
                scope: None,
                transaction: None,
            };
        };
        let desc = match name {
            "tape" => &TAPE,
            "grid" => &GRID,
            "deform" => &DEFORM,
            "bounds" => &BOUNDS,
            _ => unreachable!("example node kind"),
        };
        let args = Args::none()
            .with("edit", inner.edit.load(Ordering::Relaxed))
            .with("node", node.as_u64())
            .with("attempt", attempt);
        let args = if name == "tape" {
            args.with("recorded", recorded)
        } else {
            args.with("items", items as u64)
        };
        let scope = Some(footfall::scope_with(desc, inner.context(), &args));
        footfall::flow_step(
            FlowId::new(inner.edit.load(Ordering::Relaxed)),
            inner.context(),
        );
        Scope {
            scope,
            transaction: None,
        }
    }
    pub(super) fn host(&self) -> Scope {
        Scope {
            scope: self
                .0
                .as_ref()
                .map(|inner| footfall::scope(&HOST, inner.context())),
            transaction: None,
        }
    }
    pub(super) fn outcome(&self, node: NodeId, attempt: u64, ok: bool) {
        if let Some(inner) = &self.0 {
            footfall::message_with(
                &BODY,
                inner.context(),
                if ok { "complete" } else { "failed" },
                &Args::none()
                    .with("node", node.as_u64())
                    .with("attempt", attempt)
                    .with("edit", inner.edit.load(Ordering::Relaxed)),
            );
        }
    }
    pub(super) fn buffer(&self, kind: Buffer, bytes: usize) -> Lease {
        let mut lease = Lease {
            probe: self.clone(),
            id: 0,
            bytes: bytes as u64,
            kind,
        };
        if let Some(inner) = &self.0 {
            lease.id = inner.next_buffer.fetch_add(1, Ordering::Relaxed);
            let live = inner.live.fetch_add(lease.bytes, Ordering::Relaxed) + lease.bytes;
            inner.peak.fetch_max(live, Ordering::Relaxed);
            footfall::resource_create(
                kind.desc(),
                ResourceId::new(lease.id),
                inner.context(),
                lease.bytes,
            );
        }
        lease
    }
    pub(super) fn root(&self, owner: &'static str, key: u64, mesh: Option<&Mesh>) {
        if let Some(inner) = &self.0 {
            footfall::message_with(
                &ROOT,
                inner.context(),
                owner,
                &Args::none()
                    .with("owner", key)
                    .with("points", mesh.map_or(0, |mesh| mesh._lease.id()))
                    .with(
                        "topology",
                        mesh.map_or(0, |mesh| mesh.triangles._lease.id()),
                    )
                    .with("edit", inner.edit.load(Ordering::Relaxed)),
            );
        }
    }
    pub(super) fn completed(
        &self,
        graph: &Graph,
        result: &Result<RunDetailReport, GraphError<SceneError>>,
    ) {
        let Some(inner) = &self.0 else {
            return;
        };
        let report = match result {
            Ok(report) => Some(report),
            Err(error) => error.partial_report(),
        };
        if let Some(report) = report {
            for row in &report.executed {
                if let Some(cause) = &row.because_of {
                    let path = row.why_path.as_ref().map_or_else(
                        || format!("{cause:?}"),
                        |path| {
                            path.iter()
                                .map(|key| format!("{key:?}"))
                                .collect::<Vec<_>>()
                                .join(" -> ")
                        },
                    );
                    footfall::message_with(
                        &CAUSE,
                        inner.context(),
                        &path,
                        &Args::none()
                            .with("node", row.node.as_u64())
                            .with("edit", inner.edit.load(Ordering::Relaxed))
                            .with("traced", row.why_path_traced == Some(true)),
                    );
                }
                footfall::message_with(
                    &PUBLISHED,
                    inner.context(),
                    row.node_label.as_deref().unwrap_or("node"),
                    &Args::none()
                        .with("node", row.node.as_u64())
                        .with("run", graph.node_run_count(row.node).unwrap_or(0))
                        .with("edit", inner.edit.load(Ordering::Relaxed)),
                );
                if let Some(outputs) = graph.node_outputs(row.node) {
                    for (_, value) in outputs.iter() {
                        if let SceneValue::Mesh(mesh) = value {
                            self.root("cache", row.node.as_u64(), Some(mesh));
                        }
                    }
                }
            }
            for row in &report.cut_off {
                footfall::message_with(
                    &CUTOFF,
                    inner.context(),
                    row.node_label.as_deref().unwrap_or("node"),
                    &Args::none()
                        .with("node", row.node.as_u64())
                        .with("edit", inner.edit.load(Ordering::Relaxed)),
                );
            }
        }
        if let Err(error) = result {
            footfall::message_with(
                &FAILED,
                inner.context(),
                &error.to_string(),
                &Args::none().with("edit", inner.edit.load(Ordering::Relaxed)),
            );
        }
    }
    pub(super) fn checkpoint(&self, phase: &'static str) {
        if let Some(inner) = &self.0 {
            footfall::message_with(
                &CHECKPOINT,
                inner.context(),
                phase,
                &Args::none()
                    .with("live", inner.live.load(Ordering::Relaxed))
                    .with("peak", inner.peak.load(Ordering::Relaxed))
                    .with("edit", inner.edit.load(Ordering::Relaxed)),
            );
        }
    }
    pub(super) fn recording(&self, node: NodeId, attempt: u64) {
        if let Some(inner) = &self.0 {
            footfall::message_with(
                &RECORDING,
                inner.context(),
                "VM recording selected",
                &Args::none()
                    .with("node", node.as_u64())
                    .with("attempt", attempt)
                    .with("edit", inner.edit.load(Ordering::Relaxed)),
            );
        }
    }
}

pub(super) struct Capture {
    probe: Probe,
    directory: Option<PathBuf>,
    recorder: Option<CaptureRecorder<'static, File>>,
}
impl Capture {
    pub(super) fn start(path: Option<&Path>) -> Result<Self, Box<dyn Error>> {
        let Some(path) = path else {
            return Ok(Self {
                probe: Probe::default(),
                directory: None,
                recorder: None,
            });
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir(path)?;
        let runtime = Runtime::builder()
            .process_name("procedural execution")
            .session_name("whole edits and ownership")
            .event_capacity(EVENT_CAPACITY)
            .install_global()?;
        let context = RegisteredContext::new(&footfall::context!(
            WORKLOAD = "procedural.surface",
            PROGRAM = 1_u64
        ));
        let probe = Probe(Some(Arc::new(Inner {
            context,
            edit: AtomicU64::new(0),
            next_edit: AtomicU64::new(1),
            next_buffer: AtomicU64::new(1),
            live: AtomicU64::new(0),
            peak: AtomicU64::new(0),
        })));
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(path.join("edits.ffcap"))?;
        Ok(Self {
            probe,
            directory: Some(path.into()),
            recorder: Some(CaptureRecorder::new(runtime, file)?),
        })
    }
    pub(super) fn probe(&self) -> Probe {
        self.probe.clone()
    }
    pub(super) fn flush(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(recorder) = &mut self.recorder {
            let window = recorder.write()?;
            if window.missed() != 0 {
                return Err(
                    "capture lost events; increase event capacity or flush more often".into(),
                );
            }
        }
        Ok(())
    }
    pub(super) fn finish(
        mut self,
        compiled: &Compiled,
        captures: &[Captured],
    ) -> Result<(), Box<dyn Error>> {
        let Some(directory) = self.directory.take() else {
            return Ok(());
        };
        let finished = self.recorder.take().unwrap().finish()?;
        if finished.missed() != 0 {
            return Err("capture lost events".into());
        }
        let captured = Trace::from_capture(CaptureReader::new(BufReader::new(File::open(
            directory.join("edits.ffcap"),
        )?))?)?;
        if captured.truncated().is_some() || captured.skipped() != 0 {
            return Err("capture was truncated or mixed runtime instances".into());
        }
        let trace = captured.trace();
        let report = analyze(trace)?;
        let mut recording_edits = BTreeMap::new();
        for message in trace
            .messages()
            .iter()
            .filter(|message| message.name() == "procedural.recording")
        {
            recording_edits.insert(
                (
                    unsigned(message, "node").ok_or("recording node absent")?,
                    unsigned(message, "attempt").ok_or("recording attempt absent")?,
                ),
                unsigned(message, "edit").ok_or("recording edit absent")?,
            );
        }
        // Analysis owns its events. Release them before materializing the export snapshot.
        drop(captured);
        fs::write(directory.join("report.txt"), &report)?;
        let joined =
            CaptureReader::new(BufReader::new(File::open(directory.join("edits.ffcap"))?))?
                .join()?;
        footfall::export::write_perfetto_trace_file(
            &joined.into_snapshot(),
            directory.join("timeline.pftrace"),
        )?;
        fs::write(
            directory.join("program.extape"),
            compiled.program.program().encode(),
        )?;
        fs::write(directory.join("parameters.expr"), SOURCE)?;
        let mut mappings = String::new();
        for (index, (pc, range)) in compiled.ranges.iter().enumerate() {
            if index > 0 {
                mappings.push(',');
            }
            write!(
                mappings,
                "{{\"func\":{},\"pc\":{pc},\"start\":{},\"end\":{}}}",
                compiled.entry.0, range.start, range.end
            )?;
        }
        let mut recordings = String::new();
        for (index, capture) in captures.iter().enumerate() {
            let edit = recording_edits
                .get(&(capture.node.as_u64(), capture.attempt))
                .ok_or("VM recording has no matching edit observation")?;
            let file = format!("vm-{}-{}.txt", capture.node.as_u64(), capture.attempt);
            fs::write(
                directory.join(&file),
                super::explanation(compiled, capture)?,
            )?;
            if index > 0 {
                recordings.push(',');
            }
            write!(
                recordings,
                "{{\"node\":{},\"attempt\":{},\"edit\":{edit},\"explanation\":\"{file}\"}}",
                capture.node.as_u64(),
                capture.attempt
            )?;
        }
        fs::write(
            directory.join("manifest.json"),
            format!(
                "{{\n\"schema\":1,\n\"capture\":\"edits.ffcap\",\n\"program\":{{\"key\":1,\"file\":\"program.extape\",\"source\":\"parameters.expr\",\"mappings\":[{mappings}]}},\n\"vm_explanations\":[{recordings}],\n\"native_graph_replay\":false,\n\"presentation_observed\":false\n}}\n"
            ),
        )?;
        println!(
            "\n{}\nCapture and source-linked VM explanations: {}",
            report,
            directory.display()
        );
        Ok(())
    }
}

fn unsigned(message: &footfall::analysis::Message, key: &str) -> Option<u64> {
    message
        .args()
        .iter()
        .find_map(|arg| match (arg.key(), arg.value()) {
            (actual, footfall::analysis::OwnedLabelValue::Unsigned(value)) if actual == key => {
                Some(*value)
            }
            _ => None,
        })
}

fn analyze(trace: &Trace) -> Result<String, Box<dyn Error>> {
    if !trace
        .recorded(footfall::analysis::Signal::Resources)
        .is_yes()
    {
        return Err("native resource events were not recorded".into());
    }
    let table = trace.spans().group_by(GroupBy::Name).table();
    if !table.basis().is_complete() {
        return Err(format!("incomplete scope evidence: {}", table.basis()).into());
    }
    let mut text = String::from(
        "Whole-edit capture: edit -> execution -> publication; no renderer/presentation evidence.\nDurations are wall time, including observation and capture setup; this is not a VM benchmark.\nPublication/cutoff/cause messages are post-query report notifications, not exact publication timestamps.\n",
    );
    writeln!(text, "Scope evidence: {}", table.basis())?;
    for row in table.rows() {
        writeln!(
            text,
            "  {}: {} spans, {:?} total, {:?} self",
            row.describe(),
            row.stats().count(),
            row.stats().total(),
            row.stats().self_time()
        )?;
    }
    let publications = trace
        .messages()
        .iter()
        .filter(|m| m.name() == "procedural.published")
        .count();
    let cutoffs = trace
        .messages()
        .iter()
        .filter(|m| m.name() == "procedural.cutoff")
        .count();
    let failures = trace
        .messages()
        .iter()
        .filter(|m| m.name() == "procedural.failed")
        .count();
    writeln!(
        text,
        "Graph evidence: {publications} successful node publications; {cutoffs} scheduled-node cutoffs; {failures} failed queries."
    )?;
    let mut by_edit = BTreeMap::<u64, Vec<_>>::new();
    let mut roots = BTreeMap::<(String, u64), (u64, u64)>::new();
    for message in trace.messages() {
        if let Some(edit) = unsigned(message, "edit") {
            by_edit.entry(edit).or_default().push(message);
        }
        match message.name() {
            "procedural.root" => {
                let owner = (
                    message.text().to_owned(),
                    unsigned(message, "owner").ok_or("root owner absent")?,
                );
                let buffers = (
                    unsigned(message, "points").ok_or("point root absent")?,
                    unsigned(message, "topology").ok_or("topology root absent")?,
                );
                if buffers == (0, 0) {
                    roots.remove(&owner);
                } else {
                    roots.insert(owner, buffers);
                }
            }
            "procedural.storage" => {
                writeln!(
                    text,
                    "  {}: {} native-buffer bytes; roots:",
                    message.text(),
                    unsigned(message, "live").ok_or("storage total absent")?
                )?;
                for ((owner, key), (points, topology)) in &roots {
                    writeln!(
                        text,
                        "    {owner}[{key}] -> points#{points}, topology#{topology}"
                    )?;
                }
                if roots.is_empty() {
                    writeln!(text, "    none")?;
                }
            }
            "procedural.failed" => {
                writeln!(text, "  rejected: {}", message.text())?;
            }
            _ => {}
        }
    }
    writeln!(
        text,
        "Edit outcomes (body completion and graph publication are distinct):"
    )?;
    for selected in trace.spans().named("procedural.edit").iter() {
        let span = selected.span();
        let Some(footfall::analysis::OwnedLabelValue::Unsigned(id)) = span.arg("edit") else {
            return Err("edit ID absent".into());
        };
        let messages = by_edit.get(id).ok_or("edit observations absent")?;
        let names = |name| {
            messages
                .iter()
                .filter(|message| message.name() == name)
                .map(|message| message.text())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let kind = match span.arg("kind") {
            Some(footfall::analysis::OwnedLabelValue::String(kind)) => kind.as_str(),
            _ => "unknown",
        };
        writeln!(
            text,
            "  edit#{id} {kind}: published [{}]; cutoff [{}]",
            names("procedural.published"),
            names("procedural.cutoff")
        )?;
        for message in messages
            .iter()
            .filter(|message| message.name() == "procedural.cause")
        {
            writeln!(
                text,
                "    node {} one scheduling path (traced={}): {}",
                unsigned(message, "node").ok_or("cause node absent")?,
                message.args().iter().any(|arg| arg.key() == "traced"
                    && matches!(arg.value(), footfall::analysis::OwnedLabelValue::Bool(true))),
                message.text()
            )?;
        }
    }
    let resources = trace.resources();
    let unclosed = resources.iter().filter(|r| r.dropped().is_none()).count();
    writeln!(
        text,
        "Native buffer evidence: {} point/topology lifetimes; {unclosed} still live at capture end. Sizes are Vec capacities, excluding allocator overhead and VM storage.",
        resources.len()
    )?;
    if unclosed != 0 {
        return Err("native buffers remained live after cache and undo release".into());
    }
    // Transactions already work as comparison units; no fabricated rendering frames are needed.
    let edits: Vec<_> = trace
        .spans()
        .named("procedural.edit")
        .iter()
        .into_iter()
        .map(|selected| selected.span())
        .collect();
    let ranges = |kind| {
        edits
            .iter()
            .filter(|span| matches!(span.arg("kind"), Some(footfall::analysis::OwnedLabelValue::String(value)) if value.as_str() == kind))
            .map(|span| span.range())
            .collect::<Vec<_>>()
    };
    let amplitude = ranges("amplitude");
    let quality = ranges("quality");
    if !amplitude.is_empty() && !quality.is_empty() {
        let a = Population::ranges(trace, amplitude);
        let b = Population::ranges(trace, quality);
        let comparison = Comparison::between(&a, &b)
            .selecting(Selection::Inside)
            .group_by(GroupBy::Name)
            .group_by(GroupBy::Arg("recorded".into()))
            .group_by(GroupBy::Arg("items".into()))
            .run();
        writeln!(
            text,
            "Amplitude versus topology-edit populations (different work; recording mode and item count separated; not a speedup comparison):"
        )?;
        for row in comparison.rows() {
            writeln!(
                text,
                "  {}: {:+.1} us/edit ({:+.1} volume, {:+.1} per-span cost)",
                row.describe(),
                row.change_micros(),
                row.volume_micros(),
                row.cost_micros()
            )?;
        }
        for caveat in comparison.caveats() {
            writeln!(text, "  caveat: {caveat:?}")?;
        }
    }
    Ok(text)
}
