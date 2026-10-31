// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Real pending asset I/O, owned guest/native values and one source-linked change explanation.

#[path = "deferred_assets/asset_io.rs"]
mod asset_io;
#[path = "deferred_assets/evidence.rs"]
mod evidence;
#[path = "deferred_assets/guest.rs"]
mod guest;
#[path = "deferred_assets/program.rs"]
mod program;
use evidence::Evidence;
use execution_graph::{
    ExecutionGraph, FnExecutor, FnNode, GraphError, NodeId, NodeStatus, ReportDetailMask,
    RunDetailReport, TapeError, TapeValue,
};
use execution_tape::{aggregates::AggError, vm::TrapInfo};
use std::{
    cell::RefCell,
    error::Error,
    fmt,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Wake, Waker},
    time::Instant,
};

struct Cloud {
    request: usize,
    reply: asset_io::Reply,
    metadata: TapeValue,
}
impl fmt::Debug for Cloud {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cloud")
            .field("request", &self.request)
            .field("vertices", &(self.reply.coordinates.len() / 3))
            .field("metadata", &self.metadata)
            .finish()
    }
}
#[derive(Clone, Debug)]
enum SceneValue {
    Asset(String),
    Number(i64),
    Cloud(Arc<Cloud>),
    Bounds([i64; 6]),
    Text(String),
}
#[derive(Debug)]
enum SceneError {
    Expected(&'static str),
    Vm(TrapInfo),
    Aggregate(AggError),
    Tape(TapeError),
    Io(asset_io::LoadError),
    #[cfg(feature = "observatory")]
    Observation(String),
}
impl fmt::Display for SceneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expected(expected) => write!(f, "expected {expected}"),
            Self::Vm(error) => error.fmt(f),
            Self::Aggregate(error) => error.fmt(f),
            Self::Tape(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
            #[cfg(feature = "observatory")]
            Self::Observation(error) => error.fmt(f),
        }
    }
}
impl Error for SceneError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Vm(e) => Some(e),
            Self::Aggregate(e) => Some(e),
            Self::Tape(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}
type Graph = ExecutionGraph<FnExecutor<SceneValue, SceneError>>;
fn values_equal(a: &SceneValue, b: &SceneValue) -> bool {
    match (a, b) {
        (SceneValue::Asset(a), SceneValue::Asset(b))
        | (SceneValue::Text(a), SceneValue::Text(b)) => a == b,
        (SceneValue::Number(a), SceneValue::Number(b)) => a == b,
        (SceneValue::Bounds(a), SceneValue::Bounds(b)) => a == b,
        (SceneValue::Cloud(a), SceneValue::Cloud(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}
fn bounds_of(cloud: &Cloud) -> [i64; 6] {
    let mut bounds = [i64::MAX, i64::MAX, i64::MAX, i64::MIN, i64::MIN, i64::MIN];
    for point in cloud.reply.coordinates.as_chunks::<3>().0 {
        for axis in 0..3 {
            bounds[axis] = bounds[axis].min(point[axis]);
            bounds[axis + 3] = bounds[axis + 3].max(point[axis]);
        }
    }
    bounds
}
fn cached_cloud(graph: &Graph, node: NodeId) -> Arc<Cloud> {
    let SceneValue::Cloud(cloud) = graph
        .node_outputs(node)
        .expect("live asset node")
        .get("points")
        .expect("published points")
    else {
        panic!("point cloud")
    };
    Arc::clone(cloud)
}
#[derive(Default)]
struct WakeCount(AtomicUsize);
impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
struct Scene {
    graph: Graph,
    points: NodeId,
    bounds: NodeId,
    caption: NodeId,
    gain: NodeId,
    service: Rc<RefCell<asset_io::Service>>,
    evidence: Rc<RefCell<Evidence>>,
    wakes: Arc<WakeCount>,
}
impl Scene {
    fn new(compiled: &program::Compiled, snapshot: bool) -> Result<Self, Box<dyn Error>> {
        let service = Rc::new(RefCell::new(asset_io::Service::new()));
        let evidence = Rc::new(RefCell::new(Evidence::default()));
        let mut graph = Graph::new(FnExecutor::new().with_value_eq(values_equal));
        graph.set_collect_task_events(true);
        let wakes = Arc::new(WakeCount::default());
        graph.set_task_waker(Waker::from(wakes.clone()));
        let points = graph.add_node(
            FnNode::deferred({
                let compiled = compiled.clone();
                let service = service.clone();
                let evidence = evidence.clone();
                move |inputs, access| {
                    let [SceneValue::Asset(path)] = inputs else {
                        return Err(SceneError::Expected("asset path"));
                    };
                    let snapshot = snapshot && evidence.borrow().attempts.is_empty();
                    Ok(Box::new(guest::Guest::start(
                        path.clone(),
                        compiled.clone(),
                        service.clone(),
                        evidence.clone(),
                        access,
                        snapshot,
                    )?))
                }
            }),
            vec!["asset".into()],
            vec!["points".into()],
        )?;
        graph.set_node_label(points, "load points (guest + native I/O)")?;
        let bounds = graph.add_node(
            FnNode::named("native bounds", |inputs, outputs, _| {
                let [SceneValue::Cloud(cloud)] = inputs else {
                    return Err(SceneError::Expected("native point cloud"));
                };
                outputs.push(SceneValue::Bounds(bounds_of(cloud)));
                Ok(())
            }),
            vec!["points".into()],
            vec!["bounds".into()],
        )?;
        graph.set_node_label(bounds, "native bounds")?;
        graph.connect(points, "points", bounds, "points")?;
        let caption = graph.add_node(
            FnNode::named("bounds caption", |inputs, outputs, _| {
                let [SceneValue::Bounds(bounds)] = inputs else {
                    return Err(SceneError::Expected("bounds"));
                };
                outputs.push(SceneValue::Text(format!("bounds {bounds:?}")));
                Ok(())
            }),
            vec!["bounds".into()],
            vec!["caption".into()],
        )?;
        graph.set_node_label(caption, "bounds caption")?;
        graph.connect(bounds, "bounds", caption, "bounds")?;
        let gain = graph.add_node(
            FnNode::named("independent gain", |inputs, outputs, _| {
                let [SceneValue::Number(value)] = inputs else {
                    return Err(SceneError::Expected("numeric gain"));
                };
                outputs.push(SceneValue::Number(value * 2));
                Ok(())
            }),
            vec!["gain".into()],
            vec!["gain".into()],
        )?;
        graph.set_node_label(gain, "independent gain")?;
        graph.set_input_value(gain, "gain", SceneValue::Number(3))?;
        Ok(Self {
            graph,
            points,
            bounds,
            caption,
            gain,
            service,
            evidence,
            wakes,
        })
    }
    fn bind(&mut self, path: &str) -> Result<(), GraphError<SceneError>> {
        self.graph
            .set_input_value(self.points, "asset", SceneValue::Asset(path.into()))?;
        self.graph.invalidate_input("asset");
        Ok(())
    }
    fn query(&mut self, label: &str) -> Result<RunDetailReport, GraphError<SceneError>> {
        let start = Instant::now();
        let result = self.graph.run_all_with_report(ReportDetailMask::FULL);
        let wall = start.elapsed();
        let report = match &result {
            Ok(report) => report.clone(),
            Err(error) => error.partial_report().cloned().unwrap_or_default(),
        };
        let mut evidence = self.evidence.borrow_mut();
        evidence.events.extend(self.graph.take_task_events());
        evidence.queried(
            label,
            wall,
            report,
            result.as_ref().err().map(ToString::to_string),
            &self.graph,
            &[self.points, self.bounds, self.caption, self.gain],
        );
        result
    }
    fn checkpoint(&self, label: &str, roots: &[&str]) {
        self.evidence.borrow_mut().checkpoint(
            label,
            roots,
            self.service
                .borrow()
                .storage
                .live_positions
                .load(Ordering::Relaxed),
        );
    }
    fn reply(&self, request: usize, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
        let before = self.wakes.0.load(Ordering::Relaxed);
        self.service.borrow_mut().reply(request, bytes)?;
        assert!(
            self.wakes.0.load(Ordering::Relaxed) > before,
            "the real worker must wake its pending graph task"
        );
        Ok(())
    }
}

fn grid() -> Vec<u8> {
    use fmt::Write as _;
    let mut text = String::new();
    for y in 0..32 {
        for x in 0..32 {
            writeln!(text, "{x} {y} {}", (x + y) % 7).expect("string write");
        }
    }
    text.into_bytes()
}

/// A deterministic edit history; synchronization uses real replies and joins, never timed sleeps.
fn run(compiled: &program::Compiled, snapshot: bool) -> Result<Evidence, Box<dyn Error>> {
    let mut scene = Scene::new(compiled, snapshot)?;
    scene.bind("surface-v1.points")?;
    let initial = scene.query("start first asset; independent gain publishes")?;
    assert!(
        initial.waiting.contains(&scene.points),
        "asset task must wait for real I/O"
    );
    assert!(
        initial.blocked.contains(&scene.bounds),
        "native consumer must wait for publication"
    );
    assert!(
        initial.executed.iter().any(|row| row.node == scene.gain),
        "independent work must progress"
    );
    assert_eq!(
        scene.graph.node_status(scene.points),
        Some(NodeStatus::Waiting),
        "pending task must remain inspectable"
    );
    let fuel = scene.evidence.borrow().attempts[0].fuel_waiting;

    scene
        .graph
        .set_input_value(scene.gain, "gain", SceneValue::Number(5))?;
    scene.graph.invalidate_input("gain");
    scene.query("edit gain while the same guest waits")?;
    assert_eq!(
        scene.service.borrow().len(),
        1,
        "waiting must not restart guest I/O"
    );
    assert!(
        matches!(
            scene.graph.node_outputs(scene.gain).unwrap().get("gain"),
            Some(SceneValue::Number(10))
        ),
        "independent edit must publish while I/O waits"
    );
    assert_eq!(
        scene.evidence.borrow().attempts[0].fuel_latest,
        fuel,
        "polling an unresolved host request must spend no additional guest fuel"
    );

    scene.reply(0, b"0 0 0\n1 2 3\n")?;
    assert!(
        scene
            .graph
            .task_info(scene.points)
            .unwrap()
            .description
            .unwrap()
            .contains("queued"),
        "inspection must distinguish a queued reply from guest admission"
    );
    scene.checkpoint(
        "old response queued, before guest admission",
        &["request 0 mailbox"],
    );
    scene.bind("surface-v2.points")?;
    assert!(
        scene.service.borrow().is_retired(0),
        "rebind must cancel the old request"
    );
    assert_eq!(
        scene
            .service
            .borrow()
            .storage
            .live_positions
            .load(Ordering::Relaxed),
        0,
        "a queued obsolete reply must be reclaimed before another query"
    );
    scene.checkpoint("rebind discards old response", &[]);
    scene.query("start replacement; old response cannot publish")?;
    let bytes = grid();
    scene.reply(1, &bytes)?;
    scene.query("publish replacement geometry and native bounds")?;
    let undo = cached_cloud(&scene.graph, scene.points);
    let viewport = Arc::clone(&undo);
    let old = Arc::downgrade(&undo);
    assert_eq!(
        undo.reply.coordinates.len() / 3,
        1024,
        "decode the whole 32 by 32 grid"
    );
    assert_eq!(
        bounds_of(&undo),
        [0, 0, 0, 31, 31, 6],
        "native bounds must match the supplied grid"
    );
    scene.checkpoint(
        "first published geometry",
        &["graph cache", "viewport", "undo"],
    );
    let caption_runs = scene.graph.node_run_count(scene.caption);

    scene.bind("broken.points")?;
    scene.query("start invalid asset; previous output remains cached")?;
    scene.reply(2, b"0 invalid 0\n")?;
    let error = scene
        .query("host decoding fails; preserve previous publication")
        .expect_err("malformed coordinates must fail");
    assert!(
        error.to_string().contains("coordinate 1"),
        "typed host diagnostic must survive graph failure"
    );
    assert!(
        matches!(
            find_load_error(&error),
            Some(asset_io::LoadError::Coordinate { index: 1 })
        ),
        "native callers must recover the typed failure through the graph/VM error chain"
    );
    assert_eq!(
        scene.graph.node_status(scene.points),
        Some(NodeStatus::Pending),
        "failed replacement must leave cached output pending"
    );
    assert!(
        Arc::ptr_eq(&cached_cloud(&scene.graph, scene.points), &undo),
        "failure must preserve the exact previous publication"
    );
    scene.checkpoint(
        "failed replacement; cached output is pending",
        &["graph cache", "viewport", "undo"],
    );

    scene.bind("surface-recovered.points")?;
    scene.query("start recovery")?;
    scene.reply(3, &bytes)?;
    let recovered =
        scene.query("publish a new cloud; unchanged bounds stop caption propagation")?;
    let latest = cached_cloud(&scene.graph, scene.points);
    let new = Arc::downgrade(&latest);
    assert!(
        !Arc::ptr_eq(&latest, &undo),
        "geometry identity must change even when bounds match"
    );
    assert_eq!(
        scene.graph.node_run_count(scene.caption),
        caption_runs,
        "unchanged bounds must cut off caption execution"
    );
    assert!(
        recovered
            .cut_off
            .iter()
            .any(|row| row.node == scene.caption),
        "report must explain the actual caption cutoff"
    );
    drop(latest);
    scene.checkpoint(
        "new graph result; viewport and undo retain old geometry",
        &["graph cache (new)", "viewport (old)", "undo (old)"],
    );

    drop(scene.graph.remove_node(scene.points)?);
    assert!(
        new.upgrade().is_none(),
        "removing and dropping the graph output must retire its buffer"
    );
    assert!(
        old.upgrade().is_some(),
        "explicit external roots must remain valid"
    );
    scene.checkpoint("removed asset node", &["viewport (old)", "undo (old)"]);
    drop(viewport);
    scene.checkpoint("closed viewport", &["undo (old)"]);
    drop(undo);
    assert!(
        old.upgrade().is_none(),
        "last explicit root must retire old geometry"
    );
    scene.checkpoint("discarded undo; recordings remain", &[]);
    assert_eq!(
        scene
            .service
            .borrow()
            .storage
            .live_positions
            .load(Ordering::Relaxed),
        0,
        "recordings must retain no native point buffers"
    );
    assert_eq!(
        scene.graph.node_attempt_count(scene.bounds),
        Some(2),
        "native kernel runs only for two published clouds"
    );
    assert_eq!(
        scene.service.borrow().len(),
        4,
        "waiting polls must not duplicate I/O"
    );
    {
        let mut evidence = scene.evidence.borrow_mut();
        for attempt in &mut evidence.attempts {
            let request = attempt
                .request
                .expect("every scripted attempt admitted I/O");
            assert_eq!(
                scene.service.borrow().label(request),
                attempt.asset,
                "request provenance must match the actual worker"
            );
            attempt.check_replay()?;
        }
        assert_eq!(
            evidence.attempts[0].replay.as_deref(),
            Some("incomplete: replay refused"),
            "cancelled guest must remain incomplete"
        );
        assert_eq!(
            evidence.attempts[1].replay.as_deref(),
            Some("returned metadata"),
            "successful guest metadata must replay"
        );
        assert_eq!(
            evidence.attempts[2].replay.as_deref(),
            Some("reproduced trap"),
            "failed host outcome must replay as a guest trap"
        );
        assert_eq!(
            evidence.attempts[3].replay.as_deref(),
            Some("returned metadata"),
            "successful guest metadata must replay"
        );
    }
    let evidence = scene.evidence.clone();
    drop(scene);
    Ok(Rc::try_unwrap(evidence)
        .unwrap_or_else(|_| panic!("tasks and scene retired"))
        .into_inner())
}
fn find_load_error<'a>(mut error: &'a (dyn Error + 'static)) -> Option<&'a asset_io::LoadError> {
    loop {
        if let Some(error) = error.downcast_ref() {
            return Some(error);
        }
        error = error.source()?;
    }
}
fn main() -> Result<(), Box<dyn Error>> {
    let mut snapshot = false;
    let mut interactive = false;
    let mut json = false;
    let mut directory = None::<PathBuf>;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--snapshot" => snapshot = true,
            "--observatory" => interactive = true,
            "--json" => json = true,
            "--capture" => {
                directory = Some(args.next().ok_or("--capture needs a new directory")?.into());
            }
            "--help" | "-h" => {
                println!(
                    "deferred_assets [--json] [--snapshot] [--observatory] [--capture NEW_DIR]\n\nReal local socket reads, owned guest tasks, native geometry, cancelled/failed\nreplacements, explicit viewport/undo roots, source links and checked metadata replay.\nUI options require --features observatory. --observatory opens the completed\nguest recording; native I/O/geometry and graph publication are not replayed."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown option {arg}; use --help").into()),
        }
    }
    #[cfg(not(feature = "observatory"))]
    if snapshot || interactive {
        return Err("--snapshot/--observatory requires --features observatory".into());
    }
    let compiled = program::compile()?;
    let evidence = run(&compiled, snapshot)?;
    if json {
        println!("{}", evidence.json(&compiled)?);
    } else {
        println!("{}", evidence.explain(&compiled)?);
        for (_, view) in &evidence.snapshots {
            println!("{view}");
        }
    }
    if let Some(directory) = directory {
        evidence.write(&directory, &compiled)?;
    }
    #[cfg(feature = "observatory")]
    if interactive {
        let recording = evidence
            .attempts
            .last()
            .unwrap()
            .recording
            .as_ref()
            .unwrap();
        execution_tape_observatory::RecordingExplorer::new(recording)
            .sources(&compiled.sources)?
            .instructions()
            .run()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_guest_publication_failure_cutoff_and_native_roots_compose() {
        let compiled = program::compile().unwrap();
        let evidence = run(&compiled, false).unwrap();
        assert_eq!(evidence.attempts.len(), 4);
        assert_eq!(evidence.checkpoints.last().unwrap().bytes, 0);
        let explanation = evidence.explain(&compiled).unwrap();
        assert!(explanation.contains("points.scene:1"));
        assert!(explanation.contains("native bounds source"));
        assert!(explanation.contains("coordinate 1"));
        assert!(explanation.contains("reproduced trap"));
    }
    #[cfg(feature = "observatory")]
    #[test]
    fn pending_guest_view_has_exact_authored_source_without_resuming_io() {
        let evidence = run(&program::compile().unwrap(), true).unwrap();
        assert_eq!(evidence.snapshots.len(), 1);
        let view = &evidence.snapshots[0].1;
        assert!(view.contains("asset.load_points"));
        assert!(view.contains("points.scene"));
        assert!(view.contains("AWAITING HOST"));
    }
}
