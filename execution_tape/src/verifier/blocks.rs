// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Source block contracts, per-edge verification, and prepared parallel-copy schedules.
//! Mutable execution state belongs to the VM.

use super::*;

/// A malformed typed block declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockDeclarationError {
    /// Function entry parameters must come from the function signature.
    EntryParameters,
    /// A parameter names r0 or a register outside the function.
    InvalidRegister {
        /// Parameter index.
        index: usize,
        /// Register number.
        reg: u32,
    },
    /// A register is declared more than once.
    DuplicateRegister {
        /// Parameter index.
        index: usize,
        /// Register number.
        reg: u32,
    },
}

/// A failed control-flow edge contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockEdgeError {
    /// The encoded branch argument split exceeds the combined list.
    InvalidArgumentSplit,
    /// Argument count does not match the target declaration (zero for an untyped target).
    Arity {
        /// Target parameter count.
        expected: usize,
        /// Supplied argument count.
        actual: usize,
    },
    /// An edge enters a typed block after its declaration.
    BypassesHeader {
        /// Required target header PC.
        header: u32,
    },
    /// An argument has the wrong type, or no definite type.
    Type {
        /// Argument index within this edge.
        index: usize,
        /// Source register.
        reg: u32,
        /// Declared parameter type.
        expected: ValueType,
        /// Definite source type, if known.
        actual: Option<ValueType>,
    },
}

pub(super) fn declaration(decoded: &[DecodedInstr], target: u32) -> Option<&[u32]> {
    let ix = decoded.binary_search_by_key(&target, |di| di.offset).ok()?;
    match &decoded[ix].instr {
        Instr::Block { params } => Some(params),
        _ => None,
    }
}

pub(super) fn validate_structure(
    func: u32,
    reg_count: u32,
    decoded: &[DecodedInstr],
) -> Result<(), VerifyError> {
    if !decoded.iter().any(|di| {
        matches!(
            di.instr,
            Instr::Block { .. } | Instr::JmpArgs { .. } | Instr::BrArgs { .. }
        )
    }) {
        return Ok(());
    }
    let mut owner = None;
    let mut owners = Vec::with_capacity(decoded.len());
    for di in decoded {
        if let Instr::Block { params } = &di.instr {
            let invalid = if di.offset == 0 && !params.is_empty() {
                Some(BlockDeclarationError::EntryParameters)
            } else {
                let mut sorted = params.iter().copied().enumerate().collect::<Vec<_>>();
                sorted.sort_unstable_by_key(|&(_, reg)| reg);
                let bad = params
                    .iter()
                    .enumerate()
                    .find(|&(_, &reg)| reg == 0 || reg >= reg_count);
                if let Some((index, &reg)) = bad {
                    Some(BlockDeclarationError::InvalidRegister { index, reg })
                } else {
                    sorted
                        .windows(2)
                        .find(|pair| pair[0].1 == pair[1].1)
                        .map(|pair| BlockDeclarationError::DuplicateRegister {
                            index: pair[1].0,
                            reg: pair[1].1,
                        })
                }
            };
            if let Some(error) = invalid {
                return Err(VerifyError::BlockDeclaration {
                    func,
                    pc: di.offset,
                    error,
                });
            }
            owner = Some(di.offset);
        }
        owners.push(owner);
        if Opcode::from_u8(di.opcode).is_some_and(Opcode::is_terminator) {
            owner = None;
        }
    }
    for di in decoded {
        if let Instr::BrArgs {
            true_arg_count,
            args,
            pc_true,
            ..
        } = &di.instr
            && *true_arg_count as usize > args.len()
        {
            return Err(VerifyError::BlockEdge {
                func,
                pc: di.offset,
                arm: 0,
                target: *pc_true,
                error: BlockEdgeError::InvalidArgumentSplit,
            });
        }
        for arm in 0..2 {
            let Some((target, args)) = di.instr.edge(arm) else {
                continue;
            };
            let Ok(ix) = decoded.binary_search_by_key(&target, |di| di.offset) else {
                continue;
            };
            let error = if let Some(header) = owners[ix].filter(|&header| header != target) {
                Some(BlockEdgeError::BypassesHeader { header })
            } else {
                let expected = declaration(decoded, target).map_or(0, |params| params.len());
                (expected != args.len()).then_some(BlockEdgeError::Arity {
                    expected,
                    actual: args.len(),
                })
            };
            if let Some(error) = error {
                return Err(VerifyError::BlockEdge {
                    func,
                    pc: di.offset,
                    arm,
                    target,
                    error,
                });
            }
        }
    }
    Ok(())
}

pub(super) fn validate_initialized(
    func: u32,
    pc: u32,
    instr: &Instr,
    state: &BitSet,
    types: RegisterTypes<'_>,
    decoded: &[DecodedInstr],
) -> Result<(), VerifyError> {
    for arm in 0..2 {
        let Some((target, args)) = instr.edge(arm) else {
            continue;
        };
        let Some(params) = declaration(decoded, target) else {
            continue;
        };
        for (index, (&reg, &param)) in args.iter().zip(params).enumerate() {
            let expected = types.get(param).expect("validated parameter register");
            if !state.get(reg as usize) {
                return Err(VerifyError::BlockEdge {
                    func,
                    pc,
                    arm,
                    target,
                    error: BlockEdgeError::Type {
                        index,
                        reg,
                        expected,
                        actual: None,
                    },
                });
            }
        }
    }
    Ok(())
}

pub(super) fn validate_types(
    func: u32,
    pc: u32,
    instr: &Instr,
    types: RegisterTypes<'_>,
    decoded: &[DecodedInstr],
) -> Result<(), VerifyError> {
    for arm in 0..2 {
        let Some((target, args)) = instr.edge(arm) else {
            continue;
        };
        let Some(params) = declaration(decoded, target) else {
            continue;
        };
        for (index, (&reg, &param)) in args.iter().zip(params).enumerate() {
            let expected = types.get(param).expect("validated parameter register");
            let actual = types.get(reg);
            if actual != Some(expected) {
                return Err(VerifyError::BlockEdge {
                    func,
                    pc,
                    arm,
                    target,
                    error: BlockEdgeError::Type {
                        index,
                        reg,
                        expected,
                        actual,
                    },
                });
            }
        }
    }
    Ok(())
}

/// Copies facts from the original predecessor into a reusable scratch state before meeting.
pub(super) fn transfer_edge(
    incoming: &RefinementState,
    scratch: &mut RefinementState,
    params: &[u32],
    args: &[u32],
) {
    scratch.clone_from(incoming);
    for (&dst, &src) in params.iter().zip(args) {
        scratch.set_refinement(dst as usize, incoming.refinement(src as usize).cloned());
    }
}

#[derive(Default)]
pub(super) struct CopyScheduler {
    nodes: Vec<CopyNode>,
    ready: Vec<usize>,
}
struct CopyNode {
    dst: VReg,
    src: VReg,
    users: usize,
    source: Option<usize>,
    first_user: Option<usize>,
    next_user: Option<usize>,
    pending: bool,
}
impl CopyScheduler {
    pub(super) fn clear(&mut self) {
        self.nodes.clear();
        self.ready.clear();
    }
    pub(super) fn push(&mut self, dst: VReg, src: VReg) {
        if dst != src {
            self.nodes.push(CopyNode {
                dst,
                src,
                users: 0,
                source: None,
                first_user: None,
                next_user: None,
                pending: true,
            });
        }
    }
    /// Lower simultaneous assignments in O(n log n), using reusable flat dependency storage.
    /// Only cycles need scratch; all edges in a function share at most one scratch per bank.
    pub(super) fn lower(
        &mut self,
        operands: &mut Vec<VReg>,
        counts: &mut RegCounts,
        scratch_regs: &mut Vec<VReg>,
    ) -> VRegSlice {
        let start = operands.len();
        self.nodes.sort_unstable_by_key(|node| node.dst.key());
        for ix in 0..self.nodes.len() {
            let key = self.nodes[ix].src.key();
            if let Ok(source) = self.nodes.binary_search_by_key(&key, |node| node.dst.key()) {
                self.nodes[ix].source = Some(source);
                self.nodes[ix].next_user = self.nodes[source].first_user;
                self.nodes[source].first_user = Some(ix);
                self.nodes[source].users += 1;
            }
        }
        self.ready.extend(
            self.nodes
                .iter()
                .enumerate()
                .filter_map(|(ix, node)| (node.users == 0).then_some(ix)),
        );
        let mut remaining = self.nodes.len();
        let mut cursor = 0;
        while remaining != 0 {
            if let Some(ix) = self.ready.pop() {
                let node = &mut self.nodes[ix];
                debug_assert!(node.pending, "ready copies are scheduled exactly once");
                node.pending = false;
                operands.extend([node.dst, node.src]);
                remaining -= 1;
                if let Some(source) = node.source {
                    self.nodes[source].users -= 1;
                    if self.nodes[source].users == 0 {
                        self.ready.push(source);
                    }
                }
                continue;
            }
            // The remaining graph contains a cycle. Preserve one destination's old value,
            // redirect its still-pending users to scratch, then resume ordinary ready copies.
            while !self.nodes[cursor].pending {
                cursor += 1;
            }
            let saved = self.nodes[cursor].dst;
            let existing = scratch_regs
                .iter()
                .copied()
                .find(|reg| core::mem::discriminant(reg) == core::mem::discriminant(&saved));
            let scratch = if let Some(reg) = existing {
                reg
            } else {
                let reg = match saved {
                    VReg::Unit(_) => {
                        let r = VReg::Unit(UnitReg(
                            u32::try_from(counts.unit)
                                .expect("source register count bounds scratch index"),
                        ));
                        counts.unit += 1;
                        r
                    }
                    VReg::Bool(_) => {
                        let r = VReg::Bool(BoolReg(
                            u32::try_from(counts.bools)
                                .expect("source register count bounds scratch index"),
                        ));
                        counts.bools += 1;
                        r
                    }
                    VReg::I64(_) => {
                        let r = VReg::I64(I64Reg(
                            u32::try_from(counts.i64s)
                                .expect("source register count bounds scratch index"),
                        ));
                        counts.i64s += 1;
                        r
                    }
                    VReg::U64(_) => {
                        let r = VReg::U64(U64Reg(
                            u32::try_from(counts.u64s)
                                .expect("source register count bounds scratch index"),
                        ));
                        counts.u64s += 1;
                        r
                    }
                    VReg::F64(_) => {
                        let r = VReg::F64(F64Reg(
                            u32::try_from(counts.f64s)
                                .expect("source register count bounds scratch index"),
                        ));
                        counts.f64s += 1;
                        r
                    }
                    VReg::Decimal(_) => {
                        let r = VReg::Decimal(DecimalReg(
                            u32::try_from(counts.decimals)
                                .expect("source register count bounds scratch index"),
                        ));
                        counts.decimals += 1;
                        r
                    }
                    VReg::Bytes(_) => {
                        let r = VReg::Bytes(BytesReg(
                            u32::try_from(counts.bytes)
                                .expect("source register count bounds scratch index"),
                        ));
                        counts.bytes += 1;
                        r
                    }
                    VReg::Str(_) => {
                        let r = VReg::Str(StrReg(
                            u32::try_from(counts.strs)
                                .expect("source register count bounds scratch index"),
                        ));
                        counts.strs += 1;
                        r
                    }
                    VReg::Obj(_) => {
                        let r = VReg::Obj(ObjReg(
                            u32::try_from(counts.objs)
                                .expect("source register count bounds scratch index"),
                        ));
                        counts.objs += 1;
                        r
                    }
                    VReg::Agg(_) => {
                        let r = VReg::Agg(AggReg(
                            u32::try_from(counts.aggs)
                                .expect("source register count bounds scratch index"),
                        ));
                        counts.aggs += 1;
                        r
                    }
                    VReg::Func(_) => {
                        let r = VReg::Func(FuncReg(
                            u32::try_from(counts.funcs)
                                .expect("source register count bounds scratch index"),
                        ));
                        counts.funcs += 1;
                        r
                    }
                    VReg::Closure(_) => {
                        let r = VReg::Closure(ClosureReg(
                            u32::try_from(counts.closures)
                                .expect("source register count bounds scratch index"),
                        ));
                        counts.closures += 1;
                        r
                    }
                };
                scratch_regs.push(reg);
                reg
            };
            operands.extend([scratch, saved]);
            let mut user = self.nodes[cursor].first_user;
            while let Some(ix) = user {
                let node = &mut self.nodes[ix];
                user = node.next_user;
                if node.pending {
                    node.src = scratch;
                    node.source = None;
                    self.nodes[cursor].users -= 1;
                }
            }
            debug_assert_eq!(
                self.nodes[cursor].users, 0,
                "all remaining cycle users now read scratch"
            );
            self.ready.push(cursor);
        }
        VRegSlice {
            start,
            len: operands.len() - start,
        }
    }
}

#[cfg(test)]
mod tests;
