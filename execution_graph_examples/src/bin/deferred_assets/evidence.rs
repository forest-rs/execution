// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Evidence retains metadata/recordings, never the native point buffers it describes.

use super::{Graph, program::Compiled};
use execution_graph::{NodeAttempt, NodeId, RunDetailReport, TaskEvent};
use execution_tape::{
    recording::{Outcome, Recorder, Recording, ReplayError, ReplayOutcome},
    trace::TraceLocation,
    vm::DataWork,
};
use std::{
    cell::RefCell,
    error::Error,
    fmt::Write as _,
    io::Write as _,
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

pub(super) struct Attempt {
    pub id: NodeAttempt,
    pub asset: String,
    pub request: Option<usize>,
    pub location: Option<TraceLocation>,
    pub fuel_waiting: Option<u64>,
    pub fuel_latest: Option<u64>,
    pub replay: Option<String>,
    pub started: Instant,
    pub ended: Option<Instant>,
    pub polls: usize,
    pub poll_wall: Duration,
    pub work: DataWork,
    pub vertices: Option<usize>,
    pub driver_error: Option<String>,
    capture: Option<Rc<RefCell<Recorder>>>,
    pub recording: Option<Recording>,
}
#[derive(Default)]
pub(super) struct Evidence {
    pub attempts: Vec<Attempt>,
    pub events: Vec<TaskEvent>,
    pub snapshots: Vec<(usize, String)>,
    pub checkpoints: Vec<Checkpoint>,
    queries: Vec<Query>,
}
impl Evidence {
    pub(super) fn start(
        &mut self,
        id: NodeAttempt,
        asset: String,
        capture: Rc<RefCell<Recorder>>,
    ) -> usize {
        let index = self.attempts.len();
        self.attempts.push(Attempt {
            id,
            asset,
            request: None,
            location: None,
            fuel_waiting: None,
            fuel_latest: None,
            replay: None,
            started: Instant::now(),
            ended: None,
            polls: 0,
            poll_wall: Duration::ZERO,
            work: DataWork::default(),
            vertices: None,
            driver_error: None,
            capture: Some(capture),
            recording: None,
        });
        index
    }
    pub(super) fn requested(
        &mut self,
        index: usize,
        request: usize,
        location: TraceLocation,
        fuel: u64,
    ) {
        let attempt = &mut self.attempts[index];
        attempt.request = Some(request);
        attempt.location = Some(location);
        attempt.fuel_waiting = Some(fuel);
        attempt.fuel_latest = Some(fuel);
    }
    pub(super) fn returned(&mut self, index: usize, work: DataWork, fuel: u64, vertices: usize) {
        self.attempts[index].work = work;
        self.attempts[index].vertices = Some(vertices);
        self.attempts[index].fuel_latest = Some(fuel);
    }
    pub(super) fn polled(
        &mut self,
        index: usize,
        elapsed: Duration,
        work: Option<DataWork>,
        fuel: Option<u64>,
        error: Option<String>,
    ) {
        let attempt = &mut self.attempts[index];
        attempt.polls += 1;
        attempt.poll_wall += elapsed;
        if let Some(work) = work {
            attempt.work = work;
        }
        if fuel.is_some() {
            attempt.fuel_latest = fuel;
        }
        if error.is_some() {
            attempt.driver_error = error;
        }
    }
    pub(super) fn retired(&mut self, index: usize) {
        let attempt = &mut self.attempts[index];
        attempt.ended = Some(Instant::now());
        if let Some(capture) = attempt.capture.take() {
            let recorder = Rc::try_unwrap(capture)
                .expect("VM and its observer retired")
                .into_inner();
            attempt.recording = Some(recorder.finish());
        }
    }
}

impl Attempt {
    pub(super) fn check_replay(&mut self) -> Result<(), Box<dyn Error>> {
        let recording = self
            .recording
            .as_ref()
            .ok_or("attempt did not retire its recorder")?;
        self.replay = Some(
            match (recording.status(), recording.outcome(), recording.replay()) {
                (Err(_), _, Err(ReplayError::Incomplete(_))) => "incomplete: replay refused",
                (Ok(()), Some(Outcome::Returned(_)), Ok(ReplayOutcome::Returned(_))) => {
                    "returned metadata"
                }
                (Ok(()), Some(Outcome::Trapped(_)), Ok(ReplayOutcome::Trapped(_))) => {
                    "reproduced trap"
                }
                (_, _, Err(error)) => return Err(error.into()),
                _ => return Err("replay did not match the recorded guest outcome".into()),
            }
            .into(),
        );
        Ok(())
    }
}
pub(super) struct Checkpoint {
    label: String,
    roots: Vec<String>,
    pub bytes: usize,
}
struct Query {
    label: String,
    wall: Duration,
    report: RunDetailReport,
    error: Option<String>,
    nodes: Vec<NodeView>,
}
struct NodeView {
    node: NodeId,
    status: String,
    published: u64,
    committed_reads: Vec<String>,
    provisional_reads: Vec<String>,
    waiting: Option<String>,
}
impl Evidence {
    pub(super) fn checkpoint(&mut self, label: &str, roots: &[&str], bytes: usize) {
        self.checkpoints.push(Checkpoint {
            label: label.into(),
            roots: roots.iter().map(|s| (*s).into()).collect(),
            bytes,
        });
    }
    pub(super) fn queried(
        &mut self,
        label: &str,
        wall: Duration,
        report: RunDetailReport,
        error: Option<String>,
        graph: &Graph,
        nodes: &[NodeId],
    ) {
        self.queries.push(Query {
            label: label.into(),
            wall,
            report,
            error,
            nodes: nodes
                .iter()
                .map(|&node| {
                    let mut committed_reads: Vec<_> = graph
                        .node_dependencies(node)
                        .into_iter()
                        .flatten()
                        .map(|key| format!("{key:?}"))
                        .collect();
                    let mut provisional_reads: Vec<_> = graph
                        .task_dependencies(node)
                        .into_iter()
                        .flatten()
                        .map(|key| format!("{key:?}"))
                        .collect();
                    committed_reads.sort_unstable();
                    provisional_reads.sort_unstable();
                    NodeView {
                        node,
                        status: format!("{:?}", graph.node_status(node).expect("live node")),
                        published: graph.node_run_count(node).unwrap_or(0),
                        committed_reads,
                        provisional_reads,
                        waiting: graph
                            .task_info(node)
                            .and_then(|info| info.description.map(String::from)),
                    }
                })
                .collect(),
        });
    }
    pub(super) fn explain(&self, compiled: &Compiled) -> Result<String, Box<dyn Error>> {
        let sources = compiled.sources.bind(compiled.target.program().program())?;
        let mut text =
            String::from("Deferred asset scene: real I/O, guest metadata and native geometry\n\n");
        for query in &self.queries {
            writeln!(
                text,
                "{} ({:.3} ms query wall)",
                query.label,
                query.wall.as_secs_f64() * 1000.0
            )?;
            writeln!(
                text,
                "  published {:?}; waiting {:?}; blocked {:?}; cutoff {:?}",
                query
                    .report
                    .executed
                    .iter()
                    .map(|r| r.node.as_u64())
                    .collect::<Vec<_>>(),
                node_ids(&query.report.waiting),
                node_ids(&query.report.blocked),
                query
                    .report
                    .cut_off
                    .iter()
                    .map(|r| r.node.as_u64())
                    .collect::<Vec<_>>()
            )?;
            for row in query.report.executed.iter().chain(&query.report.cut_off) {
                writeln!(
                    text,
                    "  n{} {}: cause {:?}, path {:?} (traced {:?})",
                    row.node.as_u64(),
                    row.node_label.as_deref().unwrap_or("unlabelled"),
                    row.because_of,
                    row.why_path,
                    row.why_path_traced
                )?;
            }
            for node in &query.nodes {
                if let Some(waiting) = &node.waiting {
                    writeln!(
                        text,
                        "  n{} {}: {waiting}; provisional reads {:?}",
                        node.node.as_u64(),
                        node.status,
                        node.provisional_reads
                    )?;
                }
            }
            if let Some(error) = &query.error {
                writeln!(text, "  FAILED: {error}")?;
            }
        }
        text.push_str("\nGuest attempts (recorded return is distinct from graph publication):\n");
        for attempt in &self.attempts {
            let recording = attempt.recording.as_ref().expect("retired attempt");
            let outcome = match recording.outcome() {
                Some(Outcome::Returned(_)) => "returned metadata".into(),
                Some(Outcome::Trapped(error)) => format!("trapped: {error}"),
                None => "no final outcome".into(),
            };
            writeln!(
                text,
                "  n{} attempt {} / request {:?} / {}",
                attempt.id.node.as_u64(),
                attempt.id.attempt,
                attempt.request,
                attempt.asset
            )?;
            if let Some(location) = attempt.location {
                let source = sources
                    .lookup(location)
                    .ok_or("host call has no source mapping")?;
                writeln!(
                    text,
                    "    cook_asset f{}:{:06} -> asset.load_points at {}:{}:{}",
                    location.func.0,
                    location.pc,
                    source.document().name,
                    source.start().line,
                    source.start().byte_column
                )?;
                writeln!(text, "    {}", source.text())?;
            }
            writeln!(
                text,
                "    {outcome}; boundary capture {:?}; {}",
                recording.status(),
                attempt.replay.as_deref().unwrap_or("replay unchecked")
            )?;
            writeln!(
                text,
                "    {} polls, {:.3} ms poll wall / {:.3} ms lifetime; fuel {:?} -> {:?}; data work {:?}",
                attempt.polls,
                attempt.poll_wall.as_secs_f64() * 1000.0,
                attempt
                    .ended
                    .unwrap()
                    .duration_since(attempt.started)
                    .as_secs_f64()
                    * 1000.0,
                attempt.fuel_waiting,
                attempt.fuel_latest,
                attempt.work
            )?;
            for event in self.events.iter().filter(|event| event.id == attempt.id) {
                writeln!(text, "    graph {:?}", event.kind)?;
            }
        }
        let native = native_source();
        writeln!(
            text,
            "\nnative bounds source: {}:{} (native kernel, separate from guest frames)\n{}",
            native.name, native.line, native.text
        )?;
        text.push_str("\nNative coordinate buffers (Vec capacity bytes; explicit roots):\n");
        for checkpoint in &self.checkpoints {
            writeln!(
                text,
                "  {}: {} bytes; roots {:?}",
                checkpoint.label, checkpoint.bytes, checkpoint.roots
            )?;
        }
        text.push_str("\nReplay checks guest metadata and opaque object tokens; it does not reconstruct native geometry, I/O or graph publication.\nQuery/poll/lifetime wall times overlap. Lifetime includes waiting and caller scheduling, not just I/O.\nBuffer bytes exclude VM metadata, allocator overhead and RSS; recordings retain no native point buffers.\n");
        Ok(text)
    }
    pub(super) fn json(&self, compiled: &Compiled) -> Result<String, Box<dyn Error>> {
        let sources = compiled.sources.bind(compiled.target.program().program())?;
        let mut attempts = Vec::new();
        for a in &self.attempts {
            let record = a.recording.as_ref().expect("retired attempt");
            let location = a.location.expect("scripted request");
            let source = sources.lookup(location).ok_or("source mapping missing")?;
            let outcome = match record.outcome() {
                Some(Outcome::Returned(_)) => "returned",
                Some(Outcome::Trapped(_)) => "trapped",
                None => "missing",
            };
            attempts.push(format!(
                "{{\"node\":{},\"attempt\":{},\"request\":{},\"asset\":{},\"function\":{},\"pc\":{},\"import\":\"asset.load_points\",\"source\":{{\"document\":{},\"start\":{},\"end\":{},\"line\":{},\"byte_column\":{},\"text\":{}}},\"polls\":{},\"poll_wall_ns\":{},\"lifetime_wall_ns\":{},\"fuel_waiting\":{},\"fuel_latest\":{},\"data_work\":{{\"aggregate\":{},\"payload\":{}}},\"vertices\":{},\"driver_error\":{},\"capture_complete\":{},\"capture_fault\":{},\"guest_outcome\":{},\"replay\":{}}}",
                a.id.node.as_u64(), a.id.attempt, a.request.unwrap(), quoted(&a.asset), location.func.0, location.pc,
                quoted(&source.document().name), source.range().start, source.range().end, source.start().line,
                source.start().byte_column, quoted(source.text()), a.polls, a.poll_wall.as_nanos(),
                a.ended.unwrap().duration_since(a.started).as_nanos(), optional_number(a.fuel_waiting),
                optional_number(a.fuel_latest), a.work.aggregate, a.work.payload,
                a.vertices.map_or_else(|| "null".into(), |n| n.to_string()), optional_text(a.driver_error.as_deref()),
                record.status().is_ok(), optional_text(record.status().err().map(|e| format!("{e:?}")).as_deref()),
                quoted(outcome), optional_text(a.replay.as_deref())));
        }
        let queries: Vec<_> = self.queries.iter().map(|q| {
            let nodes: Vec<_> = q.nodes.iter().map(|n| format!(
                "{{\"node\":{},\"status\":{},\"publications\":{},\"committed_reads\":{},\"provisional_reads\":{},\"waiting\":{}}}",
                n.node.as_u64(), quoted(&n.status), n.published, strings(&n.committed_reads), strings(&n.provisional_reads), optional_text(n.waiting.as_deref()))).collect();
            let rows = |rows: &[execution_graph::NodeRunDetail]| rows.iter().map(|r| format!(
                "{{\"node\":{},\"label\":{},\"because_of\":{},\"why_path\":{},\"why_path_traced\":{}}}",
                r.node.as_u64(), optional_text(r.node_label.as_deref()),
                optional_text(r.because_of.as_ref().map(|k| format!("{k:?}")).as_deref()),
                r.why_path.as_ref().map_or_else(|| "null".into(), |path| strings(&path.iter().map(|k| format!("{k:?}")).collect::<Vec<_>>())),
                r.why_path_traced.map_or_else(|| "null".into(), |b| b.to_string())
            )).collect::<Vec<_>>().join(",");
            format!("{{\"label\":{},\"wall_ns\":{},\"error\":{},\"published\":[{}],\"cutoff\":[{}],\"waiting\":{:?},\"blocked\":{:?},\"task_polls\":{},\"nodes\":[{}]}}",
                quoted(&q.label), q.wall.as_nanos(), optional_text(q.error.as_deref()), rows(&q.report.executed), rows(&q.report.cut_off),
                node_ids(&q.report.waiting), node_ids(&q.report.blocked), q.report.task_polls, nodes.join(","))
        }).collect();
        let events: Vec<_> = self
            .events
            .iter()
            .map(|event| {
                format!(
                    "{{\"node\":{},\"attempt\":{},\"kind\":{}}}",
                    event.id.node.as_u64(),
                    event.id.attempt,
                    quoted(&format!("{:?}", event.kind))
                )
            })
            .collect();
        let checkpoints: Vec<_> = self.checkpoints.iter().map(|c| format!("{{\"label\":{},\"native_coordinate_capacity_bytes\":{},\"explicit_roots\":{}}}",
            quoted(&c.label), c.bytes, strings(&c.roots))).collect();
        let native = native_source();
        Ok(format!(
            "{{\"schema\":\"execution-deferred-assets-example/1\",\"program\":\"asset.tape\",\"sources\":\"asset.sources\",\"attempts\":[{}],\"queries\":[{}],\"task_events\":[{}],\"ownership\":[{}],\"native_bounds_source\":{{\"document\":{},\"start\":{},\"end\":{},\"line\":{},\"text\":{}}},\"replay_scope\":\"guest metadata and opaque tokens; excludes native geometry, I/O and graph publication\",\"timing_scope\":\"overlapping query, poll and lifetime wall times; not CPU or isolated I/O latency\"}}\n",
            attempts.join(","),
            queries.join(","),
            events.join(","),
            checkpoints.join(","),
            quoted(native.name),
            native.start,
            native.end,
            native.line,
            quoted(native.text)
        ))
    }
    pub(super) fn write(
        &self,
        directory: &Path,
        compiled: &Compiled,
    ) -> Result<(), Box<dyn Error>> {
        std::fs::create_dir(directory)?;
        let mut files = vec![
            (
                "asset.tape".into(),
                compiled.target.program().program().encode(),
            ),
            ("asset.sources".into(), compiled.sources.encode()),
            (
                "points.scene".into(),
                super::program::SOURCE.as_bytes().to_vec(),
            ),
            ("native.rs".into(), NATIVE_SOURCE.as_bytes().to_vec()),
            ("report.txt".into(), self.explain(compiled)?.into_bytes()),
            ("report.json".into(), self.json(compiled)?.into_bytes()),
        ];
        for (index, view) in &self.snapshots {
            files.push((format!("pending-{index}.txt"), view.as_bytes().to_vec()));
        }
        for (name, bytes) in files {
            std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(directory.join(name))?
                .write_all(&bytes)?;
        }
        Ok(())
    }
}
fn node_ids(nodes: &[NodeId]) -> Vec<u64> {
    nodes.iter().map(|n| n.as_u64()).collect()
}
fn optional_number(n: Option<u64>) -> String {
    n.map_or_else(|| "null".into(), |n| n.to_string())
}
fn optional_text(s: Option<&str>) -> String {
    s.map_or_else(|| "null".into(), quoted)
}
fn strings(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|s| quoted(s))
            .collect::<Vec<_>>()
            .join(",")
    )
}
fn quoted(text: &str) -> String {
    let mut output = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch < ' ' => write!(output, "\\u{:04x}", u32::from(ch)).expect("string write"),
            ch => output.push(ch),
        }
    }
    output.push('"');
    output
}
const NATIVE_SOURCE: &str = include_str!("../deferred_assets.rs");
struct NativeSource {
    name: &'static str,
    start: usize,
    end: usize,
    line: usize,
    text: &'static str,
}
fn native_source() -> NativeSource {
    let start = NATIVE_SOURCE
        .find("fn bounds_of(")
        .expect("native kernel source");
    let end = start
        + NATIVE_SOURCE[start..]
            .find("\nfn cached_cloud")
            .expect("next function");
    NativeSource {
        name: "native.rs",
        start,
        end,
        line: NATIVE_SOURCE[..start]
            .bytes()
            .filter(|&b| b == b'\n')
            .count()
            + 1,
        text: &NATIVE_SOURCE[start..end],
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_strings_escape_control_characters_and_preserve_unicode() {
        assert_eq!(quoted("a\n\"\\\u{0000}é"), "\"a\\n\\\"\\\\\\u0000é\"");
    }
}
