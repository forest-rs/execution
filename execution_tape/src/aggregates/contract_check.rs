// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Iterative, budgeted checks of immutable values and captured environments.

use super::*;
use crate::contract::{Contract, ContractId, ContractMismatch, ContractPath, ContractViolation};
use crate::program::ValueType;
use crate::value::FuncId;
use alloc::collections::BTreeSet;

#[derive(Debug)]
pub(crate) enum CheckError {
    Mismatch(ContractViolation),
    Limit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Input {
    Aggregate(AggHandle),
    Function(FuncId),
    Closure(FuncId, AggHandle),
}
impl Input {
    fn new(value: &Value) -> Option<Self> {
        match value {
            Value::Agg(handle) => Some(Self::Aggregate(*handle)),
            Value::Func(function) => Some(Self::Function(*function)),
            Value::Closure(closure) => Some(Self::Closure(closure.func, closure.env)),
            _ => None,
        }
    }
    fn key(self, contract: ContractId) -> (u8, u32, u32, ContractId) {
        match self {
            Self::Aggregate(handle) => (0, handle.0, 0, contract),
            Self::Function(func) => (1, func.0, 0, contract),
            Self::Closure(func, env) => (2, func.0, env.0, contract),
        }
    }
    fn kind(self) -> ValueType {
        match self {
            Self::Aggregate(_) => ValueType::Agg,
            Self::Function(_) => ValueType::Func,
            Self::Closure(..) => ValueType::Closure,
        }
    }
}
#[derive(Clone, Debug)]
struct Frame {
    value: Input,
    contract: ContractId,
    next: usize,
    entered: bool,
    path: Option<ContractPath>,
}
#[derive(Debug, Default)]
pub(crate) struct ContractScratch {
    frames: Vec<Frame>,
    proven: BTreeSet<(u8, u32, u32, ContractId)>,
    work: u64,
}
impl ContractScratch {
    pub(crate) fn reset(&mut self) {
        self.clear_proofs();
        self.work = 0;
    }
    pub(crate) fn clear_proofs(&mut self) {
        self.frames.clear();
        self.proven.clear();
    }
    fn charge(&mut self, limit: u64) -> Result<(), CheckError> {
        if self.work >= limit {
            return Err(CheckError::Limit);
        }
        self.work += 1;
        Ok(())
    }
    fn failure(
        &self,
        contract: ContractId,
        child: Option<ContractPath>,
        mismatch: ContractMismatch,
    ) -> CheckError {
        let mut path: Vec<_> = self.frames.iter().filter_map(|frame| frame.path).collect();
        path.extend(child);
        CheckError::Mismatch(ContractViolation {
            contract,
            path,
            mismatch,
        })
    }
    #[cfg(test)]
    pub(crate) fn check(
        &mut self,
        program: &Program,
        heap: &AggHeap,
        handle: AggHandle,
        contract: ContractId,
        limit: u64,
    ) -> Result<(), CheckError> {
        self.check_value(program, heap, &Value::Agg(handle), contract, limit)
    }
    pub(crate) fn check_value(
        &mut self,
        program: &Program,
        heap: &AggHeap,
        value: &Value,
        contract: ContractId,
        limit: u64,
    ) -> Result<(), CheckError> {
        self.frames.clear();
        let Some(input) = Input::new(value) else {
            self.charge(limit)?;
            let expected = program.contracts.kind(contract).expect("verified contract");
            return if expected == value.value_type() {
                Ok(())
            } else {
                Err(self.failure(
                    contract,
                    None,
                    ContractMismatch::Kind {
                        expected,
                        actual: value.value_type(),
                    },
                ))
            };
        };
        self.frames.push(Frame {
            value: input,
            contract,
            next: 0,
            entered: false,
            path: None,
        });
        while let Some(frame) = self.frames.last().cloned() {
            let contract = program
                .contracts
                .get(frame.contract)
                .expect("verified contract");
            if !frame.entered {
                self.charge(limit)?;
                if self.proven.contains(&frame.value.key(frame.contract)) {
                    self.frames.pop();
                    continue;
                }
                if contract.kind() != frame.value.kind() {
                    return Err(self.failure(
                        frame.contract,
                        None,
                        ContractMismatch::Kind {
                            expected: contract.kind(),
                            actual: frame.value.kind(),
                        },
                    ));
                }
                self.frames.last_mut().unwrap().entered = true;
            }
            let mut child = None;
            match contract {
                Contract::Kind(ValueType::Agg) => {
                    let Input::Aggregate(handle) = frame.value else {
                        unreachable!("checked kind")
                    };
                    heap.node(handle).map_err(|error| {
                        self.failure(frame.contract, None, ContractMismatch::Heap(error))
                    })?;
                }
                Contract::Kind(_) => {}
                Contract::Function { args, rets } | Contract::Closure { args, rets } => {
                    let (function, environment) = match frame.value {
                        Input::Function(function) => (function, None),
                        Input::Closure(function, env) => (function, Some(env)),
                        _ => unreachable!("checked kind"),
                    };
                    let actual = program.functions.get(function.0 as usize).ok_or_else(|| {
                        self.failure(
                            frame.contract,
                            None,
                            ContractMismatch::Callable {
                                function: Some(function),
                            },
                        )
                    })?;
                    let actual_args = program.verified_contracts(actual.arg_types);
                    let actual_rets = program.verified_contracts(actual.ret_types);
                    let (env_contract, actual_args) = if environment.is_some() {
                        let Some((&env, rest)) = actual_args.split_first() else {
                            return Err(self.failure(
                                frame.contract,
                                None,
                                ContractMismatch::Callable {
                                    function: Some(function),
                                },
                            ));
                        };
                        if program.contracts.kind(env) != Some(ValueType::Agg) {
                            return Err(self.failure(
                                frame.contract,
                                None,
                                ContractMismatch::Callable {
                                    function: Some(function),
                                },
                            ));
                        }
                        (Some(env), rest)
                    } else {
                        (None, actual_args)
                    };
                    let mut matches =
                        actual_args.len() == args.len() && actual_rets.len() == rets.len();
                    if !frame.entered && matches {
                        for (&actual, &expected) in actual_args
                            .iter()
                            .chain(actual_rets)
                            .zip(args.iter().chain(rets))
                        {
                            self.charge(limit)?;
                            if actual != expected {
                                matches = false;
                                break;
                            }
                        }
                    }
                    if !matches {
                        return Err(self.failure(
                            frame.contract,
                            None,
                            ContractMismatch::Callable {
                                function: Some(function),
                            },
                        ));
                    }
                    if frame.next == 0
                        && let Some(env) = environment
                    {
                        self.frames.last_mut().unwrap().next = 1;
                        self.charge(limit)?;
                        child = Some((
                            Input::Aggregate(env),
                            env_contract.unwrap(),
                            ContractPath::Environment,
                        ));
                    }
                }
                Contract::Tuple(_) | Contract::Struct { .. } | Contract::Array { .. } => {
                    let Input::Aggregate(handle) = frame.value else {
                        unreachable!("checked kind")
                    };
                    let node = heap.node(handle).map_err(|error| {
                        self.failure(frame.contract, None, ContractMismatch::Heap(error))
                    })?;
                    let values = node.values();
                    let (expected, fixed) = match contract {
                        Contract::Tuple(members) => (
                            AggType::Tuple {
                                arity: u32::try_from(members.len())
                                    .expect("verified address space"),
                            },
                            Some(members.len()),
                        ),
                        Contract::Struct { type_id, fields } => {
                            (AggType::Struct { type_id }, Some(fields.len()))
                        }
                        Contract::Array { elem_type_id, .. } => {
                            (AggType::Array { elem_type_id }, None)
                        }
                        _ => unreachable!(),
                    };
                    if !frame.entered {
                        let actual = heap.agg_type(handle).expect("resolved aggregate");
                        if expected != actual {
                            return Err(self.failure(
                                frame.contract,
                                None,
                                ContractMismatch::Aggregate { expected, actual },
                            ));
                        }
                        if let Some(expected) = fixed
                            && expected != values.len()
                        {
                            return Err(self.failure(
                                frame.contract,
                                None,
                                ContractMismatch::Arity {
                                    expected,
                                    actual: values.len(),
                                },
                            ));
                        }
                    }
                    if frame.next < values.len() {
                        let index = frame.next;
                        self.frames.last_mut().unwrap().next += 1;
                        self.charge(limit)?;
                        let expected = match contract {
                            Contract::Tuple(members) => members[index],
                            Contract::Struct { fields, .. } => Some(fields[index]),
                            Contract::Array { element, .. } => Some(element),
                            _ => unreachable!(),
                        };
                        if let Some(expected) = expected {
                            if let Some(input) = Input::new(&values[index]) {
                                child = Some((input, expected, ContractPath::Member(index)));
                            } else {
                                self.charge(limit)?;
                                let kind =
                                    program.contracts.kind(expected).expect("verified child");
                                if kind != values[index].value_type() {
                                    return Err(self.failure(
                                        expected,
                                        Some(ContractPath::Member(index)),
                                        ContractMismatch::Kind {
                                            expected: kind,
                                            actual: values[index].value_type(),
                                        },
                                    ));
                                }
                                continue;
                            }
                        } else {
                            continue;
                        }
                    }
                }
            }
            if let Some((value, contract, path)) = child {
                self.frames.push(Frame {
                    value,
                    contract,
                    next: 0,
                    entered: false,
                    path: Some(path),
                });
            } else {
                self.proven.insert(frame.value.key(frame.contract));
                self.frames.pop();
            }
        }
        Ok(())
    }
}
