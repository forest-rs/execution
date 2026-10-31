// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Checked replay of this finite language policy, above the independent VM recordings.
//!
//! Retain the original code owners and accepted driver inputs. Reexecute the policy and guests
//! in fresh VMs, then compare typed observations. This is not a portable session format or a
//! general language runtime: unsupported evidence fails explicitly instead of weakening the check.

use super::*;
use execution_tape::{
    recording::{
        AggregateId, ControlAction, HostOutcome, InstructionOutcome, Outcome, RecordedValue,
        Recording,
    },
    value::{AggType, Decimal, FuncId, Obj},
    vm::DataWork,
};
use std::collections::BTreeMap;

#[derive(Debug)]
pub(crate) enum ReplayFailure {
    IncompleteTask,
    Input { index: usize, status: Status },
    ExtraInputs,
    Outcome,
    Policy { index: usize },
    Invocation { index: usize },
    UnsupportedEvidence,
}
impl fmt::Display for ReplayFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IncompleteTask => {
                f.write_str("task replay requires a completed or explicitly stopped task")
            }
            Self::Input { index, status } => write!(
                f,
                "task replay input {index} is missing or invalid at {status:?}"
            ),
            Self::ExtraInputs => {
                f.write_str("task replay completed before consuming all recorded choices")
            }
            Self::Outcome => {
                f.write_str("task replay changed the answer, publication revision or work")
            }
            Self::Policy { index } => write!(
                f,
                "task replay changed language decision {index} or its source owner"
            ),
            Self::Invocation { index } => write!(
                f,
                "task replay changed invocation {index} or its guest evidence"
            ),
            Self::UnsupportedEvidence => {
                f.write_str("task replay encountered evidence outside this fixture's protocol")
            }
        }
    }
}
impl Error for ReplayFailure {}

pub(crate) struct ReplayReport {
    pub(crate) stopped: bool,
    pub(crate) invocations: usize,
    pub(crate) decisions: usize,
    pub(crate) work: ExecutionWork,
}

impl Task {
    /// Reexecutes this fixture's native policy and guests under its retained external choices.
    /// Original VMs are already retired. No method lookup or external producer is substituted
    /// with a saved guest answer. Hard-cancelled/unfinished tasks remain inspection-only.
    pub(crate) fn check_replay(&self) -> Result<ReplayReport> {
        if !matches!(self.phase, Phase::Complete | Phase::Stopped) {
            return Err(ReplayFailure::IncompleteTask.into());
        }
        let mut fresh = Self::with_code(self.code.clone(), self.initial_fuel)?;
        let mut next = 0;
        loop {
            let status = fresh.poll(1)?;
            match status {
                Status::Running => {}
                Status::Complete | Status::Stopped => break,
                Status::NeedValue | Status::CleanupWaiting => {
                    match (status, self.inputs.get(next)) {
                        (Status::NeedValue, Some(Input::Publish)) => fresh.publish_replacement()?,
                        (Status::NeedValue, Some(Input::Recover(value))) => {
                            fresh.recover(*value)?
                        }
                        (Status::CleanupWaiting, Some(Input::CleanupReady)) => {
                            fresh.finish_cleanup_wait()?
                        }
                        (_, Some(Input::Stop)) => fresh.stop()?,
                        _ => {
                            return Err(ReplayFailure::Input {
                                index: next,
                                status,
                            }
                            .into());
                        }
                    }
                    next += 1;
                }
                Status::Cancelled => return Err(ReplayFailure::IncompleteTask.into()),
            }
        }
        if next != self.inputs.len() {
            return Err(ReplayFailure::ExtraInputs.into());
        }
        if self.phase != fresh.phase
            || self.answer != fresh.answer
            || self.revision() != fresh.revision()
            || self.work()? != fresh.work()?
            || self.remaining_fuel != fresh.remaining_fuel
        {
            return Err(ReplayFailure::Outcome.into());
        }
        if self.events.len() != fresh.events.len() {
            return Err(ReplayFailure::Policy {
                index: self.events.len().min(fresh.events.len()),
            }
            .into());
        }
        for (index, (expected, actual)) in self.events.iter().zip(&fresh.events).enumerate() {
            if expected.action != actual.action
                || expected.site.location != actual.site.location
                || !Arc::ptr_eq(&expected.site.code, &actual.site.code)
            {
                return Err(ReplayFailure::Policy { index }.into());
            }
        }
        if self.evidence.len() != fresh.evidence.len() {
            return Err(ReplayFailure::Invocation {
                index: self.evidence.len().min(fresh.evidence.len()),
            }
            .into());
        }
        for (index, (expected, actual)) in self.evidence.iter().zip(&fresh.evidence).enumerate() {
            // Independent VM replay checks prefix/control integrity as well as the native-policy
            // reexecution below. Neither check is used as a substitute for the other.
            expected.recording.replay()?;
            actual.recording.replay()?;
            if expected.label != actual.label
                || expected.termination != actual.termination
                || expected.work != actual.work
                || !Arc::ptr_eq(&expected.code, &actual.code)
                || survey(&expected.recording)? != survey(&actual.recording)?
            {
                return Err(ReplayFailure::Invocation { index }.into());
            }
        }
        Ok(ReplayReport {
            stopped: self.stopped(),
            invocations: self.evidence.len(),
            decisions: self.events.len(),
            work: self.work()?,
        })
    }
}

/// Aggregate IDs are normalized by first observation; neither heap addresses nor recording-local
/// ordinals are compared as identities across recordings. Distinct equal nodes remain distinct.
#[derive(Debug, PartialEq, Eq)]
enum Atom {
    Unit,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(u64),
    Decimal(Decimal),
    Bytes(Arc<[u8]>),
    Str(Arc<str>),
    Obj(Obj),
    Func(FuncId),
    Aggregate(usize),
    Closure(FuncId, usize),
}

struct Graph<'a> {
    record: &'a Recording,
    ids: BTreeMap<AggregateId, usize>,
    queued: Vec<AggregateId>,
}
impl Graph<'_> {
    fn identity(&mut self, id: AggregateId) -> usize {
        if let Some(&index) = self.ids.get(&id) {
            return index;
        }
        let index = self.queued.len();
        self.ids.insert(id, index);
        self.queued.push(id);
        index
    }
    fn atom(&mut self, value: &RecordedValue) -> Atom {
        match value {
            RecordedValue::Unit => Atom::Unit,
            RecordedValue::Bool(value) => Atom::Bool(*value),
            RecordedValue::I64(value) => Atom::I64(*value),
            RecordedValue::U64(value) => Atom::U64(*value),
            RecordedValue::F64(value) => Atom::F64(*value),
            RecordedValue::Decimal(value) => Atom::Decimal(*value),
            RecordedValue::Bytes(value) => Atom::Bytes(Arc::clone(value)),
            RecordedValue::Str(value) => Atom::Str(Arc::clone(value)),
            RecordedValue::Obj(value) => Atom::Obj(*value),
            RecordedValue::Func(value) => Atom::Func(*value),
            RecordedValue::Agg(id) => Atom::Aggregate(self.identity(*id)),
            RecordedValue::Closure { func, env } => Atom::Closure(*func, self.identity(*env)),
        }
    }
    fn values(&mut self, values: &[RecordedValue]) -> Vec<Atom> {
        values.iter().map(|value| self.atom(value)).collect()
    }
    fn constructed(&mut self, nodes: &[AggregateId]) -> Vec<usize> {
        nodes.iter().map(|&id| self.identity(id)).collect()
    }
    fn finish(mut self, facts: &mut Vec<Fact>) -> Result<()> {
        let mut cursor = 0;
        while cursor < self.queued.len() {
            let node = self
                .record
                .aggregate(self.queued[cursor])
                .ok_or(ReplayFailure::UnsupportedEvidence)?;
            let fields = self.values(node.values);
            facts.push(Fact::Node {
                kind: node.kind,
                initial: node.initial,
                fields,
            });
            cursor += 1;
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Fact {
    Entry {
        function: FuncId,
        limits: ExecutionWork,
        call_depth: usize,
        preparation: DataWork,
        args: Vec<Atom>,
    },
    Host {
        sequence: usize,
        location: TraceLocation,
        import: execution_tape::program::HostSigId,
        work: DataWork,
        constructed: Vec<usize>,
        args: Vec<Atom>,
        values: Vec<Atom>,
        extra_fuel: u64,
        stack: Option<Vec<TraceLocation>>,
    },
    ReturnFrom {
        sequence: usize,
        location: TraceLocation,
        target: u64,
        target_location: TraceLocation,
        unwound_callbacks: usize,
        work: DataWork,
        constructed: Vec<usize>,
        values: Vec<Atom>,
        stack: Option<Vec<TraceLocation>>,
    },
    Abandon {
        sequence: usize,
        location: TraceLocation,
        work: DataWork,
        stack: Option<Vec<TraceLocation>>,
    },
    Instruction {
        location: TraceLocation,
        opcode: u8,
        next: Option<TraceLocation>,
        outcome: InstructionOutcome,
        parent: Option<usize>,
        stack: Option<Vec<TraceLocation>>,
        inputs: Vec<(u32, Atom)>,
        outputs: Vec<(u32, Atom)>,
    },
    Returned(Vec<Atom>),
    Stopped(TraceLocation),
    Node {
        kind: AggType,
        initial: bool,
        fields: Vec<Atom>,
    },
}

fn survey(record: &Recording) -> Result<Vec<Fact>> {
    record.status().map_err(Clone::clone)?;
    if record.instruction_status() != Some(execution_tape::recording::InstructionStatus::Complete) {
        return Err(ReplayFailure::UnsupportedEvidence.into());
    }
    let entry = record
        .invocation()
        .ok_or(ReplayFailure::UnsupportedEvidence)?;
    let mut graph = Graph {
        record,
        ids: BTreeMap::new(),
        queued: Vec::new(),
    };
    let limits = entry.limits;
    let mut facts = vec![Fact::Entry {
        function: entry.entry,
        limits: ExecutionWork {
            fuel: limits.fuel,
            host_calls: limits.max_host_calls,
            contract: limits.max_contract_work,
            data: DataWork {
                aggregate: limits.max_aggregate_work,
                payload: limits.max_payload_work,
            },
        },
        call_depth: limits.max_call_depth,
        preparation: entry.work,
        args: graph.values(entry.args),
    }];
    for host in record.host_calls() {
        let HostOutcome::Returned { values, extra_fuel } = host.outcome else {
            return Err(ReplayFailure::UnsupportedEvidence.into());
        };
        facts.push(Fact::Host {
            sequence: host.sequence,
            location: host.location,
            import: host.import,
            work: host.work,
            constructed: graph.constructed(host.constructed),
            args: graph.values(host.args),
            values: graph.values(values),
            extra_fuel,
            stack: host.stack.map(Iterator::collect),
        });
    }
    for command in record.control_commands() {
        let stack = command.stack.map(Iterator::collect);
        facts.push(match command.action {
            ControlAction::Abandon { work } => Fact::Abandon {
                sequence: command.sequence,
                location: command.location,
                work,
                stack,
            },
            ControlAction::ReturnFrom {
                target,
                target_location,
                unwound_callbacks,
                values,
                work,
                constructed,
            } => Fact::ReturnFrom {
                sequence: command.sequence,
                location: command.location,
                target,
                target_location,
                unwound_callbacks,
                work,
                constructed: graph.constructed(constructed),
                values: graph.values(values),
                stack,
            },
            ControlAction::CallGuest { .. } => {
                return Err(ReplayFailure::UnsupportedEvidence.into());
            }
        });
    }
    for instruction in record.instructions() {
        let inputs = instruction
            .inputs
            .iter()
            .map(|(register, value)| (register, graph.atom(value)))
            .collect();
        let outputs = instruction
            .outputs
            .iter()
            .map(|(register, value)| (register, graph.atom(value)))
            .collect();
        facts.push(Fact::Instruction {
            location: instruction.location,
            opcode: instruction.opcode,
            next: instruction.next,
            outcome: instruction.outcome,
            parent: instruction.parent_attempt,
            stack: instruction.stack.map(Iterator::collect),
            inputs,
            outputs,
        });
    }
    facts.push(match record.outcome() {
        Some(Outcome::Returned(values)) => Fact::Returned(graph.values(values)),
        Some(Outcome::Trapped(error)) if matches!(error.trap, Trap::Abandoned) => {
            Fact::Stopped(TraceLocation {
                func: error.func,
                pc: error.pc,
            })
        }
        _ => return Err(ReplayFailure::UnsupportedEvidence.into()),
    });
    graph.finish(&mut facts)?;
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use execution_tape::asm::{FunctionSig, ProgramBuilder};
    use execution_tape::program::ValueType;

    fn finished(non_local: bool, publish: bool, value: i64, quantum: u64) -> Task {
        let mut task = Task::new(non_local, 1_000).unwrap();
        for _ in 0..1_000 {
            match task.poll(quantum).unwrap() {
                Status::Running => {}
                Status::NeedValue => {
                    if publish {
                        task.publish_replacement().unwrap();
                    }
                    task.recover(value).unwrap();
                }
                Status::CleanupWaiting => task.finish_cleanup_wait().unwrap(),
                Status::Complete => return task,
                Status::Cancelled | Status::Stopped => panic!("finite successful history"),
            }
        }
        panic!("finite task did not finish");
    }

    #[test]
    fn composed_replay_checks_generated_driver_histories_in_fresh_vms() {
        for non_local in [false, true] {
            for publish in [false, true] {
                for value in [i64::MIN, 0, 42, i64::MAX] {
                    for quantum in [1, 64] {
                        let task = finished(non_local, publish, value, quantum);
                        assert!(
                            task.stack().is_empty(),
                            "original VMs retired before replay"
                        );
                        let report = task.check_replay().unwrap_or_else(|error| panic!("non_local={non_local}, publish={publish}, value={value}, quantum={quantum}: {error}"));
                        assert_eq!(report.invocations, 5);
                        assert_eq!(report.work, task.work().unwrap());
                        assert_eq!(task.answer, Some(value));
                    }
                }
            }
        }
    }

    #[test]
    fn changed_missing_reordered_and_extra_driver_inputs_are_rejected() {
        let mut task = finished(true, true, 42, 64);
        task.inputs[1] = Input::Recover(99);
        assert!(matches!(
            task.check_replay()
                .err()
                .unwrap()
                .downcast_ref::<ReplayFailure>(),
            Some(ReplayFailure::Outcome)
        ));
        task.inputs[1] = Input::Recover(42);
        task.inputs.push(Input::CleanupReady);
        assert!(matches!(
            task.check_replay()
                .err()
                .unwrap()
                .downcast_ref::<ReplayFailure>(),
            Some(ReplayFailure::ExtraInputs)
        ));
        task.inputs.pop();
        task.inputs.swap(0, 2);
        assert!(matches!(
            task.check_replay()
                .err()
                .unwrap()
                .downcast_ref::<ReplayFailure>(),
            Some(ReplayFailure::Input {
                index: 0,
                status: Status::NeedValue
            })
        ));
        task.inputs.swap(0, 2);
        task.inputs.pop();
        assert!(matches!(
            task.check_replay()
                .err()
                .unwrap()
                .downcast_ref::<ReplayFailure>(),
            Some(ReplayFailure::Input {
                index: 2,
                status: Status::CleanupWaiting
            })
        ));
    }

    #[test]
    fn changed_policy_source_work_and_invocation_ownership_are_rejected() {
        let mut task = finished(true, true, 42, 64);
        task.events[0].action = Action::Recovery(-1);
        assert!(matches!(
            task.check_replay()
                .err()
                .unwrap()
                .downcast_ref::<ReplayFailure>(),
            Some(ReplayFailure::Policy { index: 0 })
        ));
        task.events[0].action = Action::RetainedBlock;
        let original = Arc::clone(&task.events[0].site.code);
        task.events[0].site.code = Arc::clone(&task.code.replacement);
        assert!(matches!(
            task.check_replay()
                .err()
                .unwrap()
                .downcast_ref::<ReplayFailure>(),
            Some(ReplayFailure::Policy { index: 0 })
        ));
        task.events[0].site.code = original;
        task.remaining_fuel -= 1;
        assert!(matches!(
            task.check_replay()
                .err()
                .unwrap()
                .downcast_ref::<ReplayFailure>(),
            Some(ReplayFailure::Outcome)
        ));
        task.remaining_fuel += 1;
        task.evidence[0].code = Arc::clone(&task.code.method);
        assert!(matches!(
            task.check_replay()
                .err()
                .unwrap()
                .downcast_ref::<ReplayFailure>(),
            Some(ReplayFailure::Invocation { index: 0 })
        ));
    }

    #[test]
    fn incomplete_task_does_not_claim_composed_replay() {
        let mut task = Task::new(true, 1_000).unwrap();
        assert!(task.check_replay().is_err());
        task.cancel();
        assert!(matches!(
            task.check_replay()
                .err()
                .unwrap()
                .downcast_ref::<ReplayFailure>(),
            Some(ReplayFailure::IncompleteTask)
        ));
    }

    #[test]
    fn explicit_driver_stops_replay_without_fabricating_cleanup_or_an_answer() {
        for non_local in [false, true] {
            for stop_at in [Status::NeedValue, Status::CleanupWaiting] {
                for quantum in [1, 64] {
                    let mut task = Task::new(non_local, 1_000).unwrap();
                    assert!(
                        task.stop().is_err(),
                        "an arbitrary running frame is not a native wait"
                    );
                    for _ in 0..1_000 {
                        let status = task.poll(quantum).unwrap();
                        if status == Status::NeedValue {
                            task.publish_replacement().unwrap();
                        }
                        if status == stop_at {
                            task.stop().unwrap();
                            break;
                        }
                        if status == Status::NeedValue {
                            task.recover(42).unwrap();
                        }
                    }
                    assert!(task.stopped());
                    let work = task.work().unwrap();
                    assert_eq!(work.fuel, 1_000 - task.remaining_fuel);
                    assert_eq!(task.answer, None);
                    assert!(!task.events.iter().any(|event| matches!(
                        event.action,
                        Action::Cleaned(_) | Action::CleanupFinished | Action::HomeReturned
                    )));
                    assert!(task.retained_block.is_none());
                    assert!(task.retained_cleanup.is_none());
                    assert!(task.transfer.is_none());
                    assert!(task.stop().is_err());
                    task.cancel();
                    assert_eq!(task.poll(64).unwrap(), Status::Stopped);
                    assert_eq!(task.work().unwrap(), work);
                    let report = task.check_replay().unwrap();
                    assert!(report.stopped);
                    assert_eq!(
                        report.invocations,
                        if stop_at == Status::NeedValue { 4 } else { 5 }
                    );
                    assert!(
                        task.evidence
                            .iter()
                            .all(|evidence| evidence.termination != Termination::Cancelled)
                    );
                }
            }
        }
    }

    fn aggregate_record(
        program: &Arc<execution_tape::verifier::VerifiedProgram>,
        distinct: bool,
        unused: usize,
    ) -> Recording {
        let mut vm = Vm::new(Services, Limits::default());
        for _ in 0..unused {
            vm.aggregates_mut().tuple_new(vec![Value::I64(-1)]).unwrap();
        }
        let left = vm.aggregates_mut().tuple_new(vec![Value::I64(1)]).unwrap();
        let right = if distinct {
            vm.aggregates_mut().tuple_new(vec![Value::I64(1)]).unwrap()
        } else {
            left
        };
        let outer = vm
            .aggregates_mut()
            .tuple_new(vec![Value::Agg(left), Value::Agg(right)])
            .unwrap();
        let mut recorder = Recorder::new(Arc::clone(program), RecordLimits::default())
            .with_instructions(InstructionCapture::default());
        vm.run(
            program,
            FuncId(0),
            &[Value::Agg(outer)],
            recorder.mask(),
            Some(&mut recorder),
        )
        .unwrap();
        recorder.finish()
    }

    #[test]
    fn observation_comparison_normalizes_local_ids_and_preserves_sharing() {
        let mut builder = ProgramBuilder::new();
        let entry = builder
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::Agg],
                ret_types: vec![ValueType::Agg],
            })
            .unwrap();
        let mut body = builder.function_asm(entry).unwrap();
        body.ret(0, &[1]);
        builder.define_function(entry, body).unwrap();
        let program = Arc::new(builder.build_verified().unwrap());
        let shared = aggregate_record(&program, false, 0);
        let relocated = aggregate_record(&program, false, 7);
        let distinct = aggregate_record(&program, true, 0);
        assert_eq!(survey(&shared).unwrap(), survey(&relocated).unwrap());
        assert_ne!(
            survey(&shared).unwrap(),
            survey(&distinct).unwrap(),
            "equal contents do not establish shared identity"
        );
    }
}
