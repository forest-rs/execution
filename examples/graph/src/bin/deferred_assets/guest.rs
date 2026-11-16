// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A task owns its guest and request; publications own native data and typed guest metadata.

use super::{
    Cloud, Evidence, SceneError, SceneValue,
    asset_io::{Reply, Request, Service},
    program::{Compiled, POINTS_TYPE},
};
use execution_graph::{NodeAccess, NodeAttempt, NodeTask, TapeValue};
use execution_tape::trace::CodeLocation;
use execution_tape::{
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    recording::{InstructionCapture, RecordLimits, Recorder, RegisterWatch},
    trace::TraceSink,
    value::{Obj, ObjHandle, Value},
    vm::{ComputationId, ExecutionWork, Invocation, InvocationOptions, Limits, RunStatus, Vm},
};
use std::{
    cell::RefCell,
    fmt,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};

pub(super) struct NoHost;
impl Host for NoHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        // External requests are completed by the task; accidentally dispatching is an error.
        Err(HostError::UnknownSymbol)
    }
}
/// An asset task: it owns its VM and keeps its guest computation there between polls.
pub(super) struct Guest {
    vm: Option<Vm<NoHost>>,
    /// The guest, saved between polls; `None` once it has returned.
    computation: Option<ComputationId>,
    /// The recorder every invocation of the guest shares; taken when the task retires.
    capture: Option<Rc<RefCell<Recorder>>>,
    /// Work the guest's invocations admitted so far: each poll's has a fresh allowance.
    spent: ExecutionWork,
    request: Option<Request>,
    compiled: Compiled,
    path: String,
    service: Rc<RefCell<Service>>,
    evidence: Rc<RefCell<Evidence>>,
    index: usize,
    snapshot: bool,
}
impl fmt::Debug for Guest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Guest")
            .field("asset", &self.path)
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}
impl Guest {
    pub(super) fn start(
        path: String,
        compiled: Compiled,
        service: Rc<RefCell<Service>>,
        evidence: Rc<RefCell<Evidence>>,
        access: &NodeAccess<'_, SceneValue>,
        snapshot: bool,
    ) -> Result<Self, SceneError> {
        let body = execution_tape::disasm::disassemble_function(
            compiled.target.program().program(),
            compiled.target.function_id(),
        )
        .expect("verified guest");
        let watches = body
            .instrs()
            .map(|instruction| RegisterWatch {
                location: CodeLocation {
                    func: compiled.target.function_id(),
                    pc: instruction.pc(),
                },
                registers: instruction.reads().chain(instruction.writes()).collect(),
            })
            .collect();
        let capture = Rc::new(RefCell::new(
            Recorder::new(compiled.target.program().clone(), RecordLimits::default())
                .with_instructions(InstructionCapture {
                    max_steps: 8,
                    watches,
                }),
        ));
        let mask = capture.mask();
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(Arc::clone(compiled.target.program())).unwrap();
        let computation = compiled
            .target
            .start(
                &mut vm,
                unit,
                vec![Value::Str(path.clone())],
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace_owned(mask, capture.clone()),
            )
            .map_err(|error| SceneError::Vm(Box::new(error)))?
            .save()
            .expect("a guest that has not run yet waits");
        let index = evidence.borrow_mut().start(
            NodeAttempt {
                node: access.node(),
                attempt: access.attempt(),
            },
            path.clone(),
            Rc::clone(&capture),
        );
        Ok(Self {
            vm: Some(vm),
            computation: Some(computation),
            capture: Some(capture),
            spent: ExecutionWork::default(),
            request: None,
            compiled,
            path,
            service,
            evidence,
            index,
            snapshot,
        })
    }
    fn advance(
        &mut self,
        cx: &Context<'_>,
        outputs: &mut Vec<SceneValue>,
        access: &mut NodeAccess<'_, SceneValue>,
    ) -> Result<Poll<()>, SceneError> {
        // Each poll is one invocation of the saved guest, with the same recorder.
        let capture = self.capture.as_ref().expect("active task");
        let mask = capture.mask();
        // The ID stays with the task until the computation is resumed under it.
        let computation = self.computation.expect("saved guest");
        let mut invocation = self
            .vm
            .as_mut()
            .expect("active task")
            .resume(
                computation,
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace_owned(mask, Rc::clone(capture)),
            )
            .map_err(|_| SceneError::Expected("the guest saved by the previous poll"))?;
        self.computation = None;
        // Work spent before this poll; `self.spent` adds this invocation's as it goes, so a
        // poll that fails still reports what it ran.
        let before = self.spent;
        let status = invocation.resume(u64::MAX);
        self.spent = spent(before, &invocation);
        match status.map_err(|error| SceneError::Vm(Box::new(error)))? {
            RunStatus::AwaitingHost => {}
            RunStatus::Suspended => unreachable!("unbounded quantum"),
            RunStatus::CallbackReturned => unreachable!("asset loading starts no callbacks"),
            RunStatus::AwaitingHandler => unreachable!("asset loading yields no suspensions"),
            RunStatus::Complete => return Err(SceneError::Expected("asset request before result")),
            _ => return Err(SceneError::Expected("a run status this loader handles")),
        }
        if self.request.is_none() {
            let request = invocation
                .pending_host_call()
                .expect("yielded host request");
            if request.symbol() != "asset.load_points" {
                return Err(SceneError::Expected("asset.load_points import"));
            }
            let location = request.location();
            drop(request);
            // This task performs the actual I/O and records its read in the current poll.
            // Neither a graph access borrow nor a borrowed argument escapes into the worker.
            access.read_input(&format!("asset:{}", self.path));
            let request = self
                .service
                .borrow_mut()
                .start(&self.path)
                .map_err(SceneError::Io)?;
            self.evidence
                .borrow_mut()
                .requested(self.index, request.id, location, self.spent.fuel);
            self.request = Some(request);
            if self.snapshot {
                invocation =
                    capture_snapshot(invocation, &self.compiled, &self.evidence, self.index)?;
                self.snapshot = false;
            }
        }
        let reply = match self.request.as_mut().expect("I/O admitted").poll(cx) {
            Poll::Pending => {
                self.computation = Some(invocation.save().expect("waiting at its request"));
                return Ok(Poll::Pending);
            }
            Poll::Ready(reply) => reply,
        };
        let request_id = self.request.as_ref().expect("request").id;
        let mut request = invocation.pending_host_call().expect("guest still waiting");
        let reply = match reply {
            Err(error) => {
                let completed = request.complete(&mut [], Err(HostError::custom(error)));
                self.spent = spent(before, &invocation);
                completed.map_err(|error| SceneError::Vm(Box::new(error)))?;
                return Err(SceneError::Expected("failed host completion must trap"));
            }
            Ok(reply) => reply,
        };
        let produced = (|| {
            let mut context = request.context();
            let preview = context.array_new(
                self.compiled.preview,
                reply.coordinates[..3]
                    .iter()
                    .copied()
                    .map(Value::I64)
                    .collect(),
            )?;
            let root = context.struct_new(
                self.compiled.schema,
                vec![
                    Value::Obj(Obj {
                        host_type: POINTS_TYPE,
                        handle: ObjHandle(request_id as u64),
                    }),
                    Value::U64((reply.coordinates.len() / 3) as u64),
                    Value::Agg(preview),
                    Value::Str(self.path.clone()),
                ],
            )?;
            Ok::<_, HostError>(Value::Agg(root))
        })();
        let completed = match produced {
            Ok(root) => request.complete(&mut [root], Ok(0)),
            Err(error) => request.complete(&mut [], Err(error)),
        };
        let status = completed.and_then(|()| invocation.resume(u64::MAX));
        self.spent = spent(before, &invocation);
        if status.map_err(|error| SceneError::Vm(Box::new(error)))? != RunStatus::Complete {
            return Err(SceneError::Expected("completed asset guest"));
        }
        let (work, fuel) = (self.spent.data, self.spent.fuel);
        let owner = Some(Arc::clone(invocation.program()));
        let returned = invocation.take_outputs().expect("successful guest");
        drop(invocation);
        // The task is done with its VM: move the result out of it without copying payloads.
        let snapshot = self
            .vm
            .take()
            .expect("completed guest")
            .into_snapshot(returned)
            .map_err(SceneError::Aggregate)?;
        let mut values = TapeValue::from_snapshot(snapshot, owner).map_err(SceneError::Tape)?;
        if values.len() != 1 {
            return Err(SceneError::Expected("one retained metadata root"));
        }
        let metadata = values.pop().expect("one root");
        validate_metadata(&metadata, request_id, &reply, &self.path)?;
        self.evidence
            .borrow_mut()
            .returned(self.index, work, fuel, reply.coordinates.len() / 3);
        outputs.push(SceneValue::Cloud(Arc::new(Cloud {
            request: request_id,
            reply,
            metadata,
        })));
        Ok(Poll::Ready(()))
    }
}

/// Renders the waiting guest into the evidence, when the observatory is built in.
#[cfg_attr(
    not(feature = "observatory"),
    expect(unused_variables, reason = "rendering needs the observatory")
)]
fn capture_snapshot<'vm>(
    invocation: Invocation<'vm, NoHost>,
    compiled: &Compiled,
    evidence: &Rc<RefCell<Evidence>>,
    index: usize,
) -> Result<Invocation<'vm, NoHost>, SceneError> {
    #[cfg(feature = "observatory")]
    {
        let mut view = execution_tape_observatory::LiveExplorer::new(invocation)
            .sources(&compiled.sources)
            .map_err(|error| SceneError::Observation(error.to_string()))?;
        let result = view.render_text(120, 38);
        let invocation = view.into_invocation();
        evidence.borrow_mut().snapshots.push((
            index,
            result.map_err(|error| SceneError::Observation(error.to_string()))?,
        ));
        Ok(invocation)
    }
    #[cfg(not(feature = "observatory"))]
    Err(SceneError::Expected(
        "--snapshot needs --features observatory",
    ))
}

/// Work admitted before `invocation` plus its own.
fn spent(before: ExecutionWork, invocation: &Invocation<'_, NoHost>) -> ExecutionWork {
    before
        .checked_add(invocation.work())
        .expect("guest work fits u64")
}

impl NodeTask<SceneValue, SceneError> for Guest {
    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        outputs: &mut Vec<SceneValue>,
        access: &mut NodeAccess<'_, SceneValue>,
    ) -> Poll<Result<(), SceneError>> {
        let start = Instant::now();
        let result = self.advance(cx, outputs, access);
        self.evidence.borrow_mut().polled(
            self.index,
            start.elapsed(),
            Some(self.spent.data),
            Some(self.spent.fuel),
            result.as_ref().err().map(ToString::to_string),
        );
        match result {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(())) => Poll::Ready(Ok(())),
            Err(error) => Poll::Ready(Err(error)),
        }
    }
    fn description(&self) -> Option<&str> {
        Some(
            self.request
                .as_ref()
                .map_or("starting asset guest", Request::description),
        )
    }
}
impl Drop for Guest {
    fn drop(&mut self) {
        // Retire the VM and its saved guest first. A cancelled capture remains explicitly
        // incomplete.
        drop(self.vm.take());
        drop(self.capture.take());
        drop(self.request.take());
        self.evidence.borrow_mut().retired(self.index);
    }
}
fn validate_metadata(
    metadata: &TapeValue,
    id: usize,
    reply: &Reply,
    path: &str,
) -> Result<(), SceneError> {
    let Value::Agg(root) = metadata.value() else {
        return Err(SceneError::Expected("structured asset result"));
    };
    let heap = metadata
        .aggregates()
        .ok_or(SceneError::Expected("retained aggregate storage"))?;
    if heap.struct_get(*root, 0).map_err(SceneError::Aggregate)?
        != Value::Obj(Obj {
            host_type: POINTS_TYPE,
            handle: ObjHandle(id as u64),
        })
        || heap.struct_get(*root, 1).map_err(SceneError::Aggregate)?
            != Value::U64((reply.coordinates.len() / 3) as u64)
        || heap.struct_get(*root, 3).map_err(SceneError::Aggregate)? != Value::Str(path.into())
    {
        return Err(SceneError::Expected(
            "metadata matching its owned native object",
        ));
    }
    Ok(())
}
