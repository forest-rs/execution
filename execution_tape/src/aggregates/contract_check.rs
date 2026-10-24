// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Iterative, budgeted checks of immutable aggregate graphs.

use super::*;
use crate::contract::{Contract, ContractId, ContractMismatch, ContractViolation};
use alloc::collections::BTreeSet;

#[derive(Debug)]
pub(crate) enum CheckError {
    Mismatch(ContractViolation),
    Limit,
}
#[derive(Clone, Debug)]
struct Frame {
    handle: AggHandle,
    contract: ContractId,
    next: usize,
    entered: bool,
    index: Option<usize>,
}
#[derive(Debug, Default)]
pub(crate) struct ContractScratch {
    frames: Vec<Frame>,
    proven: BTreeSet<(u32, u32)>,
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
        child: Option<usize>,
        mismatch: ContractMismatch,
    ) -> CheckError {
        let mut path: Vec<usize> = self.frames.iter().filter_map(|frame| frame.index).collect();
        if let Some(index) = child {
            path.push(index);
        }
        CheckError::Mismatch(ContractViolation {
            contract,
            path,
            mismatch,
        })
    }
    pub(crate) fn check(
        &mut self,
        program: &Program,
        heap: &AggHeap,
        handle: AggHandle,
        contract: ContractId,
        limit: u64,
    ) -> Result<(), CheckError> {
        self.frames.clear();
        self.frames.push(Frame {
            handle,
            contract,
            next: 0,
            entered: false,
            index: None,
        });
        while let Some(frame) = self.frames.last().cloned() {
            let contract = program
                .contracts
                .get(frame.contract)
                .expect("verified contract");
            if !frame.entered {
                self.charge(limit)?;
                if self.proven.contains(&(frame.handle.0, frame.contract.0)) {
                    self.frames.pop();
                    continue;
                }
            }
            let node = heap.node(frame.handle).map_err(|error| {
                self.failure(frame.contract, None, ContractMismatch::Heap(error))
            })?;
            let values = node.values();
            let expected = match contract {
                Contract::Kind(_) => None,
                Contract::Tuple(members) => Some(AggType::Tuple {
                    arity: u32::try_from(members.len()).expect("contract tuple address limit"),
                }),
                Contract::Struct { type_id, .. } => Some(AggType::Struct { type_id }),
                Contract::Array { elem_type_id, .. } => Some(AggType::Array { elem_type_id }),
            };
            if !frame.entered {
                if let Some(expected) = expected {
                    let actual = heap.agg_type(frame.handle).expect("resolved node");
                    if expected != actual {
                        return Err(self.failure(
                            frame.contract,
                            None,
                            ContractMismatch::Aggregate { expected, actual },
                        ));
                    }
                }
                let fixed_len = match contract {
                    Contract::Tuple(members) => Some(members.len()),
                    Contract::Struct { fields, .. } => Some(fields.len()),
                    _ => None,
                };
                if let Some(expected) = fixed_len
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
                self.frames.last_mut().expect("current frame").entered = true;
            }
            if expected.is_none() || frame.next == values.len() {
                // No unfinished obligation may enter the proof cache.
                self.proven.insert((frame.handle.0, frame.contract.0));
                self.frames.pop();
                continue;
            }
            let index = frame.next;
            self.frames.last_mut().expect("current frame").next += 1;
            self.charge(limit)?; // Count edges, including unconstrained tuple slots.
            let child = match contract {
                Contract::Tuple(members) => members[index],
                Contract::Struct { fields, .. } => Some(fields[index]),
                Contract::Array { element, .. } => Some(element),
                Contract::Kind(_) => None,
            };
            if let Some(child) = child {
                let expected = program.contracts.kind(child).expect("verified child");
                let actual = values[index].value_type();
                if expected != actual {
                    return Err(self.failure(
                        child,
                        Some(index),
                        ContractMismatch::Kind { expected, actual },
                    ));
                }
                match &values[index] {
                    Value::Agg(handle) => self.frames.push(Frame {
                        handle: *handle,
                        contract: child,
                        next: 0,
                        entered: false,
                        index: Some(index),
                    }),
                    _ => {
                        self.charge(limit)?;
                    }
                }
            }
        }
        Ok(())
    }
}
