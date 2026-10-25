// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A procedural surface: authored parameters → tape → native grid/deformation → bounds.
//! Old viewport/undo values remain owned across edits, failures, removal and graph destruction.

use execution_graph::{
    ExecutionGraph, Executor, GraphError, HostOpId, NodeAccess, NodeId, NodeOutcome, OutputId,
    ReportDetailMask, ResourceKey, TapeError, TapeExecutor, TapeInvocation, TapeNode, TapeObserver,
    TapeValue,
};
use execution_tape::{
    asm::{Asm, FunctionAbi, ProgramBuilder},
    contract::Contract,
    host::{Host, HostContext, HostError, HostSig, ResourceKeyRef, SigHash, ValueRef},
    program::ValueType,
    trace::TraceLocation,
    value::{FuncId, Value},
    verifier::VerifiedProgram,
    vm::Limits,
};
use execution_tape_recording::{
    InstructionCapture, RecordLimits, Recorder, Recording, RegisterWatch, ReplayOutcome,
};
use std::{
    error::Error,
    fmt,
    ops::Range,
    path::PathBuf,
    sync::{Arc, Weak},
    time::Instant,
};

const SOURCE: &str = "segments = density * levels + quality()\ndeformation = (amplitude,)\nreturn segments, deformation\n";
const QUALITY_OP: u64 = 7;

struct Compiled {
    program: Arc<VerifiedProgram>,
    entry: FuncId,
    ranges: Vec<(u32, Range<usize>)>,
}

fn source_range(text: &str) -> Range<usize> {
    let start = SOURCE.find(text).expect("compiler-owned source fragment");
    start..start + text.len()
}

fn compile() -> Result<Compiled, Box<dyn Error>> {
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("Procedural surface parameters");
    let quality = builder.host_sig_for(
        "quality",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let mut body = Asm::new();
    body.set_register_types(vec![
        ValueType::Unit,
        ValueType::I64,
        ValueType::I64,
        ValueType::F64,
        ValueType::I64,
        ValueType::I64,
        ValueType::I64,
        ValueType::Agg,
    ]);
    let mut ranges = Vec::new();
    ranges.push((body.pc(), source_range("density * levels")));
    body.i64_mul(4, 1, 2);
    ranges.push((body.pc(), source_range("quality()")));
    body.host_call_sig_id(0, quality, 0, &[], &[5]);
    ranges.push((body.pc(), source_range("density * levels + quality()")));
    body.i64_add(6, 4, 5);
    ranges.push((body.pc(), source_range("(amplitude,)")));
    body.tuple_new(7, &[3]);
    ranges.push((body.pc(), source_range("return segments, deformation")));
    body.ret(0, &[6, 7]);
    let floating = builder.contract(Contract::Kind(ValueType::F64))?;
    let deformation = builder.contract(Contract::Tuple(&[Some(floating)]))?;
    let entry = builder.push_function_checked(
        body,
        FunctionAbi {
            args: vec![
                ValueType::I64.into(),
                ValueType::I64.into(),
                ValueType::F64.into(),
            ],
            rets: vec![ValueType::I64.into(), deformation.into()],
        },
    )?;
    builder.set_function_name(entry, "cook_parameters")?;
    builder.set_function_output_name(entry, 0, "segments")?;
    builder.set_function_output_name(entry, 1, "deformation")?;
    Ok(Compiled {
        program: Arc::new(builder.build_verified()?),
        entry,
        ranges,
    })
}

struct QualityHost {
    quality: i64,
}
impl Host for QualityHost {
    fn call(
        &mut self,
        symbol: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        outputs: &mut [Value],
        mut cx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        if symbol != "quality" {
            return Err(HostError::UnknownSymbol);
        }
        cx.record_read(ResourceKeyRef::HostState {
            op: SigHash(QUALITY_OP),
            key: 0,
        });
        outputs[0] = Value::I64(self.quality);
        Ok(0)
    }
}

struct Captured {
    node: NodeId,
    attempt: u64,
    elapsed_ns: u128,
    recording: Recording,
}
#[derive(Default)]
struct Observe {
    next: bool,
    started: Option<Instant>,
    captures: Vec<Captured>,
}
impl TapeObserver for Observe {
    type Sink = Recorder;
    fn begin(&mut self, invocation: TapeInvocation<'_>) -> Option<Recorder> {
        if !std::mem::take(&mut self.next) {
            return None;
        }
        self.started = Some(Instant::now());
        let source = execution_tape::disasm::disassemble_function(
            invocation.program.program(),
            invocation.entry,
        )
        .expect("verified function");
        let watches = source
            .instrs()
            .map(|instruction| RegisterWatch {
                location: TraceLocation {
                    func: invocation.entry,
                    pc: instruction.pc(),
                },
                registers: instruction.reads().chain(instruction.writes()).collect(),
            })
            .collect();
        Some(
            Recorder::new(Arc::clone(invocation.program), RecordLimits::default())
                .with_instructions(InstructionCapture {
                    max_steps: 64,
                    watches,
                }),
        )
    }
    fn finish(
        &mut self,
        invocation: TapeInvocation<'_>,
        sink: Recorder,
        _: Result<(), &TapeError>,
    ) {
        let elapsed_ns = self
            .started
            .take()
            .expect("selected capture start")
            .elapsed()
            .as_nanos();
        // Retention is an application policy; this demo keeps at most two selected invocations.
        if self.captures.len() == 2 {
            self.captures.remove(0);
        }
        self.captures.push(Captured {
            node: invocation.node,
            attempt: invocation.attempt,
            elapsed_ns,
            recording: sink.finish(),
        });
    }
}

struct Mesh {
    points: Vec<[f64; 3]>,
    triangles: Arc<Vec<[u32; 3]>>,
}
impl fmt::Debug for Mesh {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mesh")
            .field("points", &self.points.len())
            .field("triangles", &self.triangles.len())
            .finish()
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
struct Bounds {
    min: [f64; 3],
    max: [f64; 3],
}
#[derive(Clone, Debug)]
enum SceneValue {
    Tape(TapeValue),
    Mesh(Arc<Mesh>),
    Bounds(Bounds),
}
#[derive(Debug)]
enum SceneNode {
    Tape(TapeNode),
    Grid,
    Deform,
    Bounds,
}
#[derive(Debug)]
enum SceneError {
    Tape(TapeError),
    Expected(&'static str),
    Segments(i64),
    Amplitude(f64),
}
impl fmt::Display for SceneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tape(error) => error.fmt(f),
            Self::Expected(kind) => write!(f, "expected {kind}"),
            Self::Segments(count) => write!(f, "surface needs 1..=512 segments, got {count}"),
            Self::Amplitude(value) => write!(
                f,
                "deformation amplitude must be finite and within -100..=100, got {value}"
            ),
        }
    }
}
impl Error for SceneError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Tape(error) => Some(error),
            _ => None,
        }
    }
}

struct Scene {
    tape: TapeExecutor<QualityHost, Observe>,
    args: Vec<TapeValue>,
    results: Vec<TapeValue>,
}

impl Executor for Scene {
    type Value = SceneValue;
    type Node = SceneNode;
    type Error = SceneError;
    fn execute(
        &mut self,
        node: &mut SceneNode,
        inputs: &[SceneValue],
        outputs: &mut Vec<SceneValue>,
        access: &mut NodeAccess<'_, SceneValue>,
    ) -> Result<NodeOutcome, SceneError> {
        match node {
            SceneNode::Tape(node) => {
                let result = (|| {
                    for input in inputs {
                        let SceneValue::Tape(input) = input else {
                            return Err(SceneError::Expected("tape parameters"));
                        };
                        self.args.push(input.clone());
                    }
                    self.tape
                        .execute_node(node, &self.args, &mut self.results, access)
                        .map_err(SceneError::Tape)?;
                    outputs.extend(self.results.drain(..).map(SceneValue::Tape));
                    Ok(())
                })();
                self.args.clear();
                self.results.clear();
                result?;
            }
            SceneNode::Grid => {
                let [SceneValue::Tape(count)] = inputs else {
                    return Err(SceneError::Expected("segment count"));
                };
                let Value::I64(count) = count.value() else {
                    return Err(SceneError::Expected("integer segment count"));
                };
                if !(1..=512).contains(count) {
                    return Err(SceneError::Segments(*count));
                }
                let count = u32::try_from(*count).expect("bounded segment count");
                let width = count + 1;
                let mut points = Vec::with_capacity((width * width) as usize);
                for row in 0..width {
                    for column in 0..width {
                        points.push([
                            2.0 * f64::from(column) / f64::from(count) - 1.0,
                            0.0,
                            2.0 * f64::from(row) / f64::from(count) - 1.0,
                        ]);
                    }
                }
                let mut triangles = Vec::with_capacity((count * count * 2) as usize);
                for row in 0..count {
                    for column in 0..count {
                        let corner = row * width + column;
                        triangles.extend([
                            [corner, corner + width, corner + 1],
                            [corner + 1, corner + width, corner + width + 1],
                        ]);
                    }
                }
                outputs.push(SceneValue::Mesh(Arc::new(Mesh {
                    points,
                    triangles: Arc::new(triangles),
                })));
            }
            SceneNode::Deform => {
                let [SceneValue::Mesh(mesh), SceneValue::Tape(parameters)] = inputs else {
                    return Err(SceneError::Expected("mesh and deformation tuple"));
                };
                let Value::Agg(tuple) = parameters.value() else {
                    return Err(SceneError::Expected("deformation tuple"));
                };
                let Value::F64(amplitude) = parameters
                    .aggregates()
                    .expect("owned aggregate")
                    .tuple_get(*tuple, 0)
                    .map_err(|error| SceneError::Tape(TapeError::Aggregate(error)))?
                else {
                    return Err(SceneError::Expected("floating amplitude"));
                };
                if !amplitude.is_finite() || amplitude.abs() > 100.0 {
                    return Err(SceneError::Amplitude(amplitude));
                }
                let points = mesh
                    .points
                    .iter()
                    .map(|&[x, _, z]| {
                        [
                            x,
                            amplitude
                                * (x * std::f64::consts::PI).sin()
                                * (z * std::f64::consts::PI).cos(),
                            z,
                        ]
                    })
                    .collect();
                outputs.push(SceneValue::Mesh(Arc::new(Mesh {
                    points,
                    triangles: Arc::clone(&mesh.triangles),
                })));
            }
            SceneNode::Bounds => {
                let [SceneValue::Mesh(mesh)] = inputs else {
                    return Err(SceneError::Expected("mesh"));
                };
                let mut bounds = Bounds {
                    min: [f64::INFINITY; 3],
                    max: [f64::NEG_INFINITY; 3],
                };
                for point in &mesh.points {
                    for (axis, coordinate) in point.iter().enumerate() {
                        bounds.min[axis] = bounds.min[axis].min(*coordinate);
                        bounds.max[axis] = bounds.max[axis].max(*coordinate);
                    }
                }
                outputs.push(SceneValue::Bounds(bounds));
            }
        }
        Ok(NodeOutcome::Complete)
    }
    fn values_equal(&self, previous: &SceneValue, next: &SceneValue) -> bool {
        match (previous, next) {
            (SceneValue::Tape(a), SceneValue::Tape(b)) => self.tape.values_equal(a, b),
            (SceneValue::Mesh(a), SceneValue::Mesh(b)) => Arc::ptr_eq(a, b),
            (SceneValue::Bounds(a), SceneValue::Bounds(b)) => a == b,
            _ => false,
        }
    }
    fn describe(&self, node: &SceneNode) -> Option<String> {
        Some(
            match node {
                SceneNode::Tape(_) => "compiled parameters",
                SceneNode::Grid => "native grid",
                SceneNode::Deform => "native deformation",
                SceneNode::Bounds => "native bounds",
            }
            .into(),
        )
    }
}

type Graph = ExecutionGraph<Scene>;
struct Nodes {
    parameters: NodeId,
    grid: NodeId,
    deform: NodeId,
    bounds: NodeId,
}

fn graph(compiled: &Compiled, density: i64) -> Result<(Graph, Nodes), GraphError<SceneError>> {
    let mut tape = TapeExecutor::new(QualityHost { quality: 0 }, Limits::default())
        .with_observer(Observe::default());
    tape.set_strict_deps(true);
    tape.set_early_cutoff(true);
    let mut graph = ExecutionGraph::new(Scene {
        tape,
        args: Vec::new(),
        results: Vec::new(),
    });
    let parameters = graph.add_node(
        SceneNode::Tape(
            TapeNode::new(Arc::clone(&compiled.program), compiled.entry)
                .map_err(|error| GraphError::InvalidNode(SceneError::Tape(error)))?,
        ),
        vec!["density".into(), "levels".into(), "amplitude".into()],
        vec!["segments".into(), "deformation".into()],
    )?;
    let grid = graph.add_node(
        SceneNode::Grid,
        vec!["segments".into()],
        vec!["mesh".into()],
    )?;
    let deform = graph.add_node(
        SceneNode::Deform,
        vec!["mesh".into(), "deformation".into()],
        vec!["mesh".into()],
    )?;
    let bounds = graph.add_node(
        SceneNode::Bounds,
        vec!["mesh".into()],
        vec!["bounds".into()],
    )?;
    for (node, label) in [
        (parameters, "parameters"),
        (grid, "grid"),
        (deform, "deform"),
        (bounds, "bounds"),
    ] {
        graph.set_node_label(node, label)?;
    }
    graph.set_input_value(
        parameters,
        "density",
        SceneValue::Tape(Value::I64(density).into()),
    )?;
    graph.set_input_value(parameters, "levels", SceneValue::Tape(Value::I64(4).into()))?;
    graph.set_input_value(
        parameters,
        "amplitude",
        SceneValue::Tape(Value::F64(1.0).into()),
    )?;
    graph.connect(parameters, "segments", grid, "segments")?;
    graph.connect(grid, "mesh", deform, "mesh")?;
    graph.connect(parameters, "deformation", deform, "deformation")?;
    graph.connect(deform, "mesh", bounds, "mesh")?;
    Ok((
        graph,
        Nodes {
            parameters,
            grid,
            deform,
            bounds,
        },
    ))
}

fn mesh(graph: &Graph, node: NodeId) -> Arc<Mesh> {
    let SceneValue::Mesh(mesh) = graph
        .node_outputs(node)
        .expect("live node")
        .get_by_id(OutputId::new(0))
        .expect("published output")
    else {
        panic!("mesh output")
    };
    Arc::clone(mesh)
}

fn amplitude(graph: &mut Graph, nodes: &Nodes, value: f64) -> Result<(), GraphError<SceneError>> {
    graph.set_input_value(
        nodes.parameters,
        "amplitude",
        SceneValue::Tape(Value::F64(value).into()),
    )?;
    graph.invalidate_input("amplitude");
    Ok(())
}

fn verify_bounds(graph: &Graph, nodes: &Nodes, density: i64, quality: i64, amplitude: f64) {
    let SceneValue::Bounds(actual) = graph
        .node_outputs(nodes.bounds)
        .expect("bounds node")
        .get("bounds")
        .expect("current bounds")
    else {
        panic!("bounds output")
    };
    // Independent closed form: the grid's nearest quarter-period sample sets the sine maximum;
    // endpoints of the other axis contain a cosine extremum. No evaluator/mesh builder is reused.
    let segments = f64::from(i32::try_from(density * 4 + quality).expect("bounded segments"));
    let nearest = (segments / 4.0).round();
    let height = amplitude.abs()
        * ((2.0 * nearest / segments - 1.0) * std::f64::consts::PI)
            .sin()
            .abs();
    let expected = Bounds {
        min: [-1.0, -height, -1.0],
        max: [1.0, height, 1.0],
    };
    for (actual, expected) in actual
        .min
        .into_iter()
        .chain(actual.max)
        .zip(expected.min.into_iter().chain(expected.max))
    {
        assert!(
            (actual - expected).abs() < 1e-12,
            "incremental bounds {actual} differ from independent surface bound {expected}"
        );
    }
}

fn report(graph: &mut Graph, label: &str) -> Result<(), GraphError<SceneError>> {
    let report = graph.run_all_with_report(ReportDetailMask::FULL)?;
    println!(
        "{label}: executed [{}]; cutoff [{}]",
        report
            .executed
            .iter()
            .filter_map(|row| row.node_label.as_deref())
            .collect::<Vec<_>>()
            .join(", "),
        report
            .cut_off
            .iter()
            .filter_map(|row| row.node_label.as_deref())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for row in report.executed {
        if let Some(cause) = row.because_of {
            println!(
                "  {}: {cause:?}",
                row.node_label.as_deref().unwrap_or("node")
            );
        }
    }
    Ok(())
}

struct Options {
    edits: u32,
    density: i64,
    record: bool,
    explain: bool,
    inspect: bool,
    snapshot: bool,
    write_mesh: Option<PathBuf>,
}
fn options() -> Result<Options, Box<dyn Error>> {
    let mut options = Options {
        edits: 64,
        density: 8,
        record: false,
        explain: false,
        inspect: false,
        snapshot: false,
        write_mesh: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--edits" => options.edits = args.next().ok_or("--edits needs a count")?.parse()?,
            "--density" => {
                options.density = args.next().ok_or("--density needs a count")?.parse()?;
            }
            "--record" => options.record = true,
            "--write-mesh" => {
                options.write_mesh = Some(args.next().ok_or("--write-mesh needs a path")?.into());
            }
            "--explain" => {
                options.explain = true;
                options.record = true;
            }
            "--inspect" => {
                options.inspect = true;
                options.record = true;
            }
            "--snapshot" => {
                options.snapshot = true;
                options.record = true;
            }
            "--help" | "-h" => {
                println!(
                    "procedural [--edits N] [--density N] [--record] [--explain] [--inspect | --snapshot] [--write-mesh PATH]\nDefault: 64 edits, 32 segments. --inspect/--snapshot require the observatory feature.\nModels native geometry ownership and selected VM evidence, not native graph replay."
                );
                std::process::exit(0);
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    if options.edits > 1_000_000 || !(1..=64).contains(&options.density) {
        return Err("use at most 1,000,000 edits and density 1..=64".into());
    }
    #[cfg(not(feature = "observatory"))]
    if options.inspect || options.snapshot {
        return Err(
            "terminal exploration requires --features observatory; --explain works in this build"
                .into(),
        );
    }
    Ok(options)
}

fn explain(compiled: &Compiled, capture: &Captured) -> Result<(), Box<dyn Error>> {
    println!(
        "\nTape evidence for node {} attempt {} ({} ns observed adapter work):",
        capture.node.as_u64(),
        capture.attempt,
        capture.elapsed_ns
    );
    for (index, step) in capture.recording.instructions().enumerate() {
        let range = &compiled
            .ranges
            .iter()
            .find(|(pc, _)| *pc == step.location.pc)
            .expect("compiler maps each instruction")
            .1;
        println!(
            "  #{index} parameters.expr:{}..{} {:?}",
            range.start,
            range.end,
            &SOURCE[range.clone()]
        );
        for (label, observations) in [("before", step.inputs), ("after", step.outputs)] {
            for (register, value) in observations.iter() {
                println!("    {label} r{register} = {value:?}");
            }
        }
    }
    capture.recording.replay()?;
    println!(
        "  Checked VM replay matched; native geometry and graph publication are separate evidence."
    );
    Ok(())
}

#[cfg(feature = "observatory")]
fn explore(compiled: &Compiled, capture: &Captured, snapshot: bool) -> Result<(), Box<dyn Error>> {
    use execution_tape_observatory::{
        RecordingExplorer,
        sources::{SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping},
    };
    let mappings = compiled
        .ranges
        .iter()
        .map(|(pc, range)| SourceMapping {
            location: TraceLocation {
                func: compiled.entry,
                pc: *pc,
            },
            document: SourceId(0),
            range: range.clone(),
        })
        .collect();
    let sources = SourceMap::new(
        compiled.program.program(),
        vec![SourceDocument {
            name: "parameters.expr".into(),
            text: SOURCE.into(),
        }],
        mappings,
        SourceLimits::default(),
    )?;
    let explorer = RecordingExplorer::new(&capture.recording)
        .sources(&sources)?
        .instructions();
    if snapshot {
        println!("{}", explorer.render_text(120, 40)?);
    } else {
        explorer.run()?;
    }
    Ok(())
}

fn run(options: Options) -> Result<(), Box<dyn Error>> {
    let compiled = compile()?;
    let (mut graph, nodes) = graph(&compiled, options.density)?;
    graph.executor_mut().tape.observer_mut().next = options.record;
    report(&mut graph, "Initial cook")?;
    verify_bounds(&graph, &nodes, options.density, 0, 1.0);
    let undo = mesh(&graph, nodes.deform);
    println!(
        "Surface: {} points, {} triangles; old viewport result retained for undo.",
        undo.points.len(),
        undo.triangles.len()
    );
    let original_grid_runs = graph.node_run_count(nodes.grid);
    let mut previous: Option<Weak<Mesh>> = None;
    let start = Instant::now();
    for edit in 0..options.edits {
        let value = 0.5 + f64::from(edit % 16) / 16.0;
        amplitude(&mut graph, &nodes, value)?;
        graph.run_node(nodes.bounds)?;
        verify_bounds(&graph, &nodes, options.density, 0, value);
        if let Some(previous) = previous.take() {
            assert!(
                previous.upgrade().is_none(),
                "obsolete deformation must release its payload"
            );
        }
        previous = Some(Arc::downgrade(&mesh(&graph, nodes.deform)));
    }
    let elapsed = start.elapsed();
    assert_eq!(
        graph.node_run_count(nodes.grid),
        original_grid_runs,
        "amplitude changes must cut off topology work"
    );
    println!(
        "{} amplitude edits: {} ns total; grid reused, obsolete deformation payloads released.",
        options.edits,
        elapsed.as_nanos()
    );

    graph.executor_mut().tape.host_mut().quality = 2;
    graph.invalidate(ResourceKey::host_state(HostOpId::new(QUALITY_OP), 0));
    graph.executor_mut().tape.observer_mut().next = options.record;
    report(&mut graph, "Quality edit")?;
    assert_eq!(
        graph.node_run_count(nodes.grid),
        original_grid_runs.map(|count| count + 1),
        "quality changes must rebuild topology"
    );
    let last_amplitude = if options.edits == 0 {
        1.0
    } else {
        0.5 + f64::from((options.edits - 1) % 16) / 16.0
    };
    verify_bounds(&graph, &nodes, options.density, 2, last_amplitude);
    let before_failure = mesh(&graph, nodes.deform);
    amplitude(&mut graph, &nodes, f64::NAN)?;
    let failure = graph
        .run_node(nodes.bounds)
        .expect_err("invalid amplitude fails native deformation");
    println!("Rejected edit: {failure}");
    assert!(
        Arc::ptr_eq(&before_failure, &mesh(&graph, nodes.deform)),
        "failed deformation must preserve the previous viewport result"
    );
    println!(
        "Previous viewport result is still readable; status: {:?}.",
        graph.node_status(nodes.deform)
    );
    amplitude(&mut graph, &nodes, 1.0)?;
    graph.run_node(nodes.bounds)?;
    verify_bounds(&graph, &nodes, options.density, 2, 1.0);
    drop(before_failure);
    if let Some(path) = &options.write_mesh {
        use std::io::Write;
        let mesh = mesh(&graph, nodes.deform);
        let mut file = std::io::BufWriter::new(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?,
        );
        writeln!(
            file,
            "# Procedural surface: {} points, {} triangles",
            mesh.points.len(),
            mesh.triangles.len()
        )?;
        for [x, y, z] in &mesh.points {
            writeln!(file, "v {x} {y} {z}")?;
        }
        for [a, b, c] in mesh.triangles.iter() {
            writeln!(file, "f {} {} {}", a + 1, b + 1, c + 1)?;
        }
        file.flush()?;
        println!("Wrote final native surface to {}.", path.display());
    }
    let current = Arc::downgrade(&mesh(&graph, nodes.deform));
    let captures = std::mem::take(&mut graph.executor_mut().tape.observer_mut().captures);
    for node in [nodes.bounds, nodes.deform, nodes.grid, nodes.parameters] {
        drop(graph.remove_node(node)?);
    }
    assert_eq!(
        graph.storage_stats().nodes,
        0,
        "all graph nodes were removed"
    );
    drop(graph);
    assert!(
        current.upgrade().is_none(),
        "removal releases the final current mesh"
    );
    println!(
        "Removed all nodes: current geometry released; retained undo still has {} points.",
        undo.points.len()
    );
    for capture in &captures {
        assert!(
            matches!(capture.recording.replay()?, ReplayOutcome::Returned(_)),
            "selected VM invocations replay their recorded successful returns"
        );
    }
    if options.record {
        println!(
            "{} selected tape recordings replayed after graph destruction.",
            captures.len()
        );
    }
    if let Some(capture) = captures.last() {
        if options.explain {
            explain(&compiled, capture)?;
        }
        #[cfg(feature = "observatory")]
        if options.inspect || options.snapshot {
            explore(&compiled, capture, options.snapshot)?;
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    run(options()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edit_history_retains_undo_and_releases_obsolete_geometry() {
        run(Options {
            edits: 32,
            density: 2,
            record: true,
            explain: false,
            inspect: false,
            snapshot: false,
            write_mesh: None,
        })
        .unwrap();
    }
}
