// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Flow-sensitive aggregate proofs. Source contracts stay borrowed; derived facts are interned.

pub(super) mod inspection;

use super::*;
use crate::contract::{Contract, ContractHasher, ContractId, ContractMismatch, ContractViolation};
use crate::value::AggType;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::hash::{Hash, Hasher};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Fact {
    Kind(ValueType),
    Shape(u32),
}

impl Fact {
    fn register(state: &ShapeState, types: RegisterTypes<'_>, reg: u32) -> Self {
        let kind = types.get(reg).expect("checked register");
        if kind == ValueType::Agg {
            state
                .agg(reg as usize)
                .copied()
                .map(Self::Shape)
                .unwrap_or(Self::Kind(kind))
        } else {
            Self::Kind(kind)
        }
    }
}

#[derive(Debug)]
enum Node {
    Source(ContractId),
    Tuple(Vec<Option<Fact>>),
    Struct(TypeId, Vec<Option<Fact>>),
    Array(ElemTypeId, Fact),
}

#[derive(Clone, Copy)]
enum Members<'a> {
    Facts(&'a [Option<Fact>]),
    Tuple(&'a [Option<ContractId>], &'a BTreeMap<ContractId, Fact>),
    Struct(&'a [ContractId], &'a BTreeMap<ContractId, Fact>),
    One(Fact),
    Registers(&'a [u32], RegisterTypes<'a>, &'a ShapeState),
}
impl Members<'_> {
    fn len(self) -> usize {
        match self {
            Self::Facts(v) => v.len(),
            Self::Tuple(v, _) => v.len(),
            Self::Struct(v, _) => v.len(),
            Self::One(_) => 1,
            Self::Registers(regs, _, _) => regs.len(),
        }
    }
    fn get(self, index: usize) -> Option<Fact> {
        match self {
            Self::Facts(v) => v.get(index).copied().flatten(),
            Self::Tuple(v, map) => v.get(index).copied().flatten().map(|id| map[&id]),
            Self::Struct(v, map) => v.get(index).map(|id| map[id]),
            Self::One(fact) => (index == 0).then_some(fact),
            Self::Registers(regs, types, state) => regs
                .get(index)
                .map(|&reg| Fact::register(state, types, reg)),
        }
    }
}
#[derive(Clone, Copy)]
struct View<'a> {
    descriptor: AggType,
    members: Members<'a>,
}
impl View<'_> {
    fn hash(self) -> u64 {
        let mut hash = ContractHasher(0xcbf2_9ce4_8422_2325);
        self.descriptor.hash(&mut hash);
        self.members.len().hash(&mut hash);
        for index in 0..self.members.len() {
            self.members.get(index).hash(&mut hash);
        }
        hash.finish()
    }
    fn same(self, other: Self) -> bool {
        self.descriptor == other.descriptor
            && self.members.len() == other.members.len()
            && (0..self.members.len()).all(|i| self.members.get(i) == other.members.get(i))
    }
    fn child(self, index: usize) -> Option<Fact> {
        self.members
            .get(if matches!(self.descriptor, AggType::Array { .. }) {
                0
            } else {
                index
            })
    }
}

struct Check {
    expected: ContractId,
    actual: Fact,
    next: usize,
    entered: bool,
}

struct Arena<'a> {
    program: &'a Program,
    nodes: Vec<Node>,
    previous: Vec<u32>,
    heads: BTreeMap<u64, u32>,
    imported: BTreeMap<ContractId, Fact>,
    common_cache: BTreeMap<(Fact, Fact), Option<Fact>>,
    replacements: Vec<(u32, Fact)>,
    check_frames: Vec<Check>,
    check_path: Vec<usize>,
    check_proven: BTreeSet<(ContractId, Fact)>,
    import_pending: Vec<(ContractId, bool)>,
    common_pending: Vec<((Fact, Fact), bool)>,
}
impl<'a> Arena<'a> {
    fn new(program: &'a Program) -> Self {
        Self {
            program,
            nodes: Vec::new(),
            previous: Vec::new(),
            heads: BTreeMap::new(),
            imported: BTreeMap::new(),
            common_cache: BTreeMap::new(),
            replacements: Vec::new(),
            check_frames: Vec::new(),
            check_path: Vec::new(),
            check_proven: BTreeSet::new(),
            import_pending: Vec::new(),
            common_pending: Vec::new(),
        }
    }
    fn view_node<'b>(&'b self, node: &'b Node) -> View<'b> {
        match node {
            Node::Source(id) => match self.program.contracts.get(*id).expect("verified contract") {
                Contract::Tuple(v) => View {
                    descriptor: AggType::Tuple {
                        arity: u32::try_from(v.len()).expect("shape member address limit"),
                    },
                    members: Members::Tuple(v, &self.imported),
                },
                Contract::Struct { type_id, fields } => View {
                    descriptor: AggType::Struct { type_id },
                    members: Members::Struct(fields, &self.imported),
                },
                Contract::Array {
                    elem_type_id,
                    element,
                } => View {
                    descriptor: AggType::Array { elem_type_id },
                    members: Members::One(self.imported[&element]),
                },
                Contract::Kind(_) => unreachable!("kind is not a shape"),
            },
            Node::Tuple(v) => View {
                descriptor: AggType::Tuple {
                    arity: u32::try_from(v.len()).expect("shape member address limit"),
                },
                members: Members::Facts(v),
            },
            Node::Struct(type_id, v) => View {
                descriptor: AggType::Struct { type_id: *type_id },
                members: Members::Facts(v),
            },
            Node::Array(elem_type_id, fact) => View {
                descriptor: AggType::Array {
                    elem_type_id: *elem_type_id,
                },
                members: Members::One(*fact),
            },
        }
    }
    fn view(&self, fact: Fact) -> Option<View<'_>> {
        match fact {
            Fact::Shape(id) => Some(self.view_node(&self.nodes[id as usize])),
            Fact::Kind(_) => None,
        }
    }
    fn kind(fact: Fact) -> ValueType {
        match fact {
            Fact::Kind(kind) => kind,
            Fact::Shape(_) => ValueType::Agg,
        }
    }
    fn find(&self, view: View<'_>) -> Option<Fact> {
        let hash = view.hash();
        let mut candidate = self.heads.get(&hash).copied().unwrap_or(u32::MAX);
        while candidate != u32::MAX {
            if view.same(self.view_node(&self.nodes[candidate as usize])) {
                return Some(Fact::Shape(candidate));
            }
            candidate = self.previous[candidate as usize];
        }
        None
    }
    fn intern(&mut self, node: Node) -> Fact {
        let view = self.view_node(&node);
        if let Some(fact) = self.find(view) {
            return fact;
        }
        let hash = view.hash();
        let previous = self.heads.get(&hash).copied().unwrap_or(u32::MAX);
        let id = u32::try_from(self.nodes.len()).expect("shape fact address space");
        self.nodes.push(node);
        self.previous.push(previous);
        self.heads.insert(hash, id);
        Fact::Shape(id)
    }
    fn construction(
        &mut self,
        descriptor: AggType,
        regs: &[u32],
        types: RegisterTypes<'_>,
        state: &ShapeState,
    ) -> Fact {
        let view = View {
            descriptor,
            members: Members::Registers(regs, types, state),
        };
        if let Some(fact) = self.find(view) {
            return fact;
        }
        let fields = (0..regs.len())
            .map(|index| view.members.get(index))
            .collect();
        self.intern(match descriptor {
            AggType::Tuple { .. } => Node::Tuple(fields),
            AggType::Struct { type_id } => Node::Struct(type_id, fields),
            _ => unreachable!(),
        })
    }
    fn import(&mut self, root: ContractId) -> Fact {
        if let Some(&fact) = self.imported.get(&root) {
            return fact;
        }
        let mut pending = core::mem::take(&mut self.import_pending);
        pending.push((root, false));
        while let Some((id, ready)) = pending.pop() {
            if self.imported.contains_key(&id) {
                continue;
            }
            let contract = self.program.contracts.get(id).expect("verified contract");
            if let Contract::Kind(kind) = contract {
                self.imported.insert(id, Fact::Kind(kind));
                continue;
            }
            if ready {
                let fact = self.intern(Node::Source(id));
                self.imported.insert(id, fact);
                continue;
            }
            pending.push((id, true));
            match contract {
                Contract::Tuple(members) => {
                    pending.extend(members.iter().flatten().map(|&id| (id, false)));
                }
                Contract::Struct { fields, .. } => {
                    pending.extend(fields.iter().map(|&id| (id, false)));
                }
                Contract::Array { element, .. } => pending.push((element, false)),
                Contract::Kind(_) => unreachable!(),
            }
        }
        self.import_pending = pending;
        self.imported[&root]
    }
    fn common(&mut self, a: Fact, b: Fact) -> Option<Fact> {
        if a == b {
            return Some(a);
        }
        if Self::kind(a) != Self::kind(b) {
            return None;
        }
        fn key(a: Fact, b: Fact) -> (Fact, Fact) {
            if a <= b { (a, b) } else { (b, a) }
        }
        let root = key(a, b);
        if let Some(&common) = self.common_cache.get(&root) {
            return common;
        }
        let mut pending = core::mem::take(&mut self.common_pending);
        pending.push((root, false));
        while let Some(((a, b), ready)) = pending.pop() {
            let pair = (a, b);
            if self.common_cache.contains_key(&pair) {
                continue;
            }
            if a == b {
                self.common_cache.insert(pair, Some(a));
                continue;
            }
            if Self::kind(a) != Self::kind(b) {
                self.common_cache.insert(pair, None);
                continue;
            }
            let views = self
                .view(a)
                .zip(self.view(b))
                .filter(|(a, b)| a.descriptor == b.descriptor);
            let Some((left, right)) = views else {
                self.common_cache
                    .insert(pair, Some(Fact::Kind(Self::kind(a))));
                continue;
            };
            if !ready {
                pending.push((pair, true));
                for index in 0..left.members.len() {
                    if let (Some(a), Some(b)) = (left.members.get(index), right.members.get(index))
                    {
                        pending.push((key(a, b), false));
                    }
                }
                continue;
            }
            let fields: Vec<_> = (0..left.members.len())
                .map(|index| {
                    left.members
                        .get(index)
                        .zip(right.members.get(index))
                        .and_then(|(a, b)| self.common_cache[&key(a, b)])
                })
                .collect();
            let node = match left.descriptor {
                AggType::Tuple { .. } => Node::Tuple(fields),
                AggType::Struct { type_id } => Node::Struct(type_id, fields),
                AggType::Array { elem_type_id } => Node::Array(
                    elem_type_id,
                    fields[0].expect("nominal array element kinds agree"),
                ),
            };
            let fact = self.intern(node);
            self.common_cache.insert(pair, Some(fact));
        }
        self.common_pending = pending;
        self.common_cache[&root]
    }
    fn fact(&self, state: &ShapeState, types: RegisterTypes<'_>, reg: u32) -> Fact {
        Fact::register(state, types, reg)
    }
    fn check(&mut self, expected: ContractId, actual: Fact) -> Result<(), ContractViolation> {
        if let Some(Contract::Kind(kind)) = self.program.contracts.get(expected) {
            return if kind == Self::kind(actual) {
                Ok(())
            } else {
                Err(ContractViolation {
                    contract: expected,
                    path: Vec::new(),
                    mismatch: ContractMismatch::Kind {
                        expected: kind,
                        actual: Self::kind(actual),
                    },
                })
            };
        }
        let mut pending = core::mem::take(&mut self.check_frames);
        let mut path = core::mem::take(&mut self.check_path);
        let result = (|| {
            pending.push(Check {
                expected,
                actual,
                next: 0,
                entered: false,
            });
            while let Some(frame) = pending.last_mut() {
                let contract = self
                    .program
                    .contracts
                    .get(frame.expected)
                    .expect("verified contract");
                let failure = if contract.kind() != Self::kind(frame.actual) {
                    Some(ContractMismatch::Kind {
                        expected: contract.kind(),
                        actual: Self::kind(frame.actual),
                    })
                } else {
                    None
                };
                if let Some(mismatch) = failure {
                    return Err(ContractViolation {
                        contract: frame.expected,
                        path: path.clone(),
                        mismatch,
                    });
                }
                // Missing shape facts are dynamic obligations, not static contradictions.
                let actual = self.view(frame.actual);
                let expected_view = match contract {
                    Contract::Kind(_) => None,
                    Contract::Tuple(members) => Some((
                        AggType::Tuple {
                            arity: u32::try_from(members.len())
                                .expect("contract tuple address limit"),
                        },
                        members.len(),
                    )),
                    Contract::Struct { type_id, fields } => {
                        Some((AggType::Struct { type_id }, fields.len()))
                    }
                    Contract::Array { elem_type_id, .. } => {
                        Some((AggType::Array { elem_type_id }, 1))
                    }
                };
                if let (Some(actual), Some((expected_descriptor, count))) = (actual, expected_view)
                {
                    if actual.descriptor != expected_descriptor {
                        return Err(ContractViolation {
                            contract: frame.expected,
                            path: path.clone(),
                            mismatch: ContractMismatch::Aggregate {
                                expected: expected_descriptor,
                                actual: actual.descriptor,
                            },
                        });
                    }
                    if !frame.entered && self.check_proven.contains(&(frame.expected, frame.actual))
                    {
                        frame.next = count;
                    }
                    frame.entered = true;
                    if frame.next < count {
                        let index = frame.next;
                        frame.next += 1;
                        let child = match contract {
                            Contract::Tuple(members) => members[index],
                            Contract::Struct { fields, .. } => Some(fields[index]),
                            Contract::Array { element, .. } => Some(element),
                            _ => None,
                        };
                        if let (Some(expected), Some(actual)) = (child, actual.child(index)) {
                            path.push(index);
                            pending.push(Check {
                                expected,
                                actual,
                                next: 0,
                                entered: false,
                            });
                        }
                        continue;
                    }
                }
                self.check_proven.insert((frame.expected, frame.actual));
                pending.pop();
                if !pending.is_empty() {
                    path.pop();
                }
            }
            Ok(())
        })();
        pending.clear();
        path.clear();
        self.check_frames = pending;
        self.check_path = path;
        result
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ShapeState {
    pub(super) top: bool,
    pub(super) aggs: Vec<(usize, u32)>,
}
impl Clone for ShapeState {
    fn clone(&self) -> Self {
        Self {
            top: self.top,
            aggs: self.aggs.clone(),
        }
    }
    fn clone_from(&mut self, source: &Self) {
        self.top = source.top;
        self.aggs.clone_from(&source.aggs);
    }
}
impl ShapeState {
    pub(super) fn unknown() -> Self {
        Self {
            top: false,
            aggs: Vec::new(),
        }
    }
    pub(super) fn agg(&self, reg: usize) -> Option<&u32> {
        match self.aggs.last() {
            Some((last, fact)) if *last == reg => Some(fact),
            Some((last, _)) if *last > reg => self
                .aggs
                .binary_search_by_key(&reg, |(reg, _)| *reg)
                .ok()
                .map(|index| &self.aggs[index].1),
            _ => None,
        }
    }
    pub(super) fn set_agg(&mut self, reg: usize, shape: Option<u32>) {
        if self.aggs.last().is_none_or(|(last, _)| *last < reg) {
            if let Some(shape) = shape {
                self.aggs.push((reg, shape));
            }
            return;
        }
        match (self.aggs.binary_search_by_key(&reg, |(reg, _)| *reg), shape) {
            (Ok(i), Some(shape)) => self.aggs[i].1 = shape,
            (Ok(i), None) => {
                self.aggs.remove(i);
            }
            (Err(i), Some(shape)) => self.aggs.insert(i, (reg, shape)),
            (Err(_), None) => {}
        }
    }
}

fn transfer(
    arena: &mut Arena<'_>,
    instr: &Instr,
    types: RegisterTypes<'_>,
    state: &mut ShapeState,
) {
    // Block declarations receive facts atomically along incoming edges; they do not write again.
    if matches!(instr, Instr::Block { .. }) {
        return;
    }

    if state.top
        || !instr
            .writes()
            .any(|reg| types.get(reg) == Some(ValueType::Agg))
    {
        return;
    }
    // Read every source before invalidating aliased destinations, including projection sources.
    let mut replacements = core::mem::take(&mut arena.replacements);
    match instr {
        Instr::Mov { dst, src } => replacements.push((*dst, arena.fact(state, types, *src))),
        Instr::Select { dst, a, b, .. } => {
            let a = arena.fact(state, types, *a);
            let b = arena.fact(state, types, *b);
            if let Some(common) = arena.common(a, b) {
                replacements.push((*dst, common));
            }
        }
        Instr::TupleNew { dst, values } => {
            let fact = arena.construction(
                AggType::Tuple {
                    arity: u32::try_from(values.len()).expect("verified operand count"),
                },
                values,
                types,
                state,
            );
            replacements.push((*dst, fact));
        }
        Instr::StructNew {
            dst,
            type_id,
            values,
        } => {
            let fact =
                arena.construction(AggType::Struct { type_id: *type_id }, values, types, state);
            replacements.push((*dst, fact));
        }
        Instr::ArrayNew {
            dst,
            elem_type_id,
            values,
            ..
        } => {
            let kind = arena.program.types.array_elems[elem_type_id.0 as usize];
            let mut element = values
                .first()
                .map_or(Fact::Kind(kind), |&reg| arena.fact(state, types, reg));
            for &reg in values.iter().skip(1) {
                element = arena
                    .common(element, arena.fact(state, types, reg))
                    .unwrap_or(Fact::Kind(kind));
            }
            replacements.push((*dst, arena.intern(Node::Array(*elem_type_id, element))));
        }
        Instr::TupleGet { dst, tuple, index } => {
            if let Some(child) = arena
                .view(arena.fact(state, types, *tuple))
                .and_then(|view| view.child(*index as usize))
            {
                replacements.push((*dst, child));
            }
        }
        Instr::StructGet {
            dst,
            st,
            field_index,
        } => {
            if let Some(child) = arena
                .view(arena.fact(state, types, *st))
                .and_then(|view| view.child(*field_index as usize))
            {
                replacements.push((*dst, child));
            }
        }
        Instr::ArrayGet { dst, arr, .. } | Instr::ArrayGetImm { dst, arr, .. } => {
            if let Some(child) = arena
                .view(arena.fact(state, types, *arr))
                .and_then(|view| view.child(0))
            {
                replacements.push((*dst, child));
            }
        }
        Instr::Call { func_id, rets, .. } => {
            for (&reg, &id) in rets.iter().zip(
                arena.program.functions[func_id.0 as usize]
                    .ret_contracts(arena.program)
                    .expect("checked signature"),
            ) {
                replacements.push((reg, arena.import(id)));
            }
        }
        _ => {}
    }
    for reg in instr.writes() {
        if types.get(reg) == Some(ValueType::Agg) {
            state.set_agg(reg as usize, None);
        }
    }
    for &(reg, fact) in &replacements {
        if types.get(reg) == Some(ValueType::Agg) {
            state.set_agg(
                reg as usize,
                match fact {
                    Fact::Shape(id) => Some(id),
                    Fact::Kind(_) => None,
                },
            );
        }
    }
    replacements.clear();
    arena.replacements = replacements;
}

fn validate(
    arena: &mut Arena<'_>,
    func_id: u32,
    pc: u32,
    instr: &Instr,
    types: RegisterTypes<'_>,
    state: &ShapeState,
) -> Result<(), VerifyError> {
    let projection = match instr {
        Instr::TupleGet { dst, tuple, index } => Some((*dst, *tuple, *index, AggKind::Tuple)),
        Instr::StructGet {
            dst,
            st,
            field_index,
        } => Some((*dst, *st, *field_index, AggKind::Struct)),
        Instr::ArrayGet { dst, arr, .. } | Instr::ArrayGetImm { dst, arr, .. } => {
            Some((*dst, *arr, 0, AggKind::Array))
        }
        _ => None,
    };
    let access = projection
        .map(|(_, reg, index, kind)| (reg, Some(index), kind))
        .or(match instr {
            Instr::TupleLen { tuple, .. } => Some((*tuple, None, AggKind::Tuple)),
            Instr::StructFieldCount { st, .. } => Some((*st, None, AggKind::Struct)),
            Instr::ArrayLen { arr, .. } => Some((*arr, None, AggKind::Array)),
            _ => None,
        });
    if let Some((reg, index, expected)) = access
        && let Some(view) = arena.view(arena.fact(state, types, reg))
    {
        let actual = match view.descriptor {
            AggType::Tuple { .. } => AggKind::Tuple,
            AggType::Struct { .. } => AggKind::Struct,
            AggType::Array { .. } => AggKind::Array,
        };
        if actual != expected {
            return Err(VerifyError::AggKindMismatch {
                func: func_id,
                pc,
                expected,
                actual,
            });
        }
        if let Some(index) = index {
            match view.descriptor {
                AggType::Tuple { arity } if index >= arity => {
                    return Err(VerifyError::TupleIndexOutOfBounds {
                        func: func_id,
                        pc,
                        arity,
                        index,
                    });
                }
                AggType::Struct { type_id } if index as usize >= view.members.len() => {
                    return Err(VerifyError::StructFieldIndexOutOfBounds {
                        func: func_id,
                        pc,
                        type_id: type_id.0,
                        field_index: index,
                    });
                }
                _ => {}
            }
        }
    }
    if let Some((dst, reg, index, _)) = projection {
        let fact = arena
            .view(arena.fact(state, types, reg))
            .and_then(|view| view.child(index as usize))
            .ok_or(VerifyError::UnknownAggregateShape {
                func: func_id,
                pc,
                reg,
            })?;
        check_expected(func_id, pc, dst, types.get(dst), Arena::kind(fact))?;
    }
    match instr {
        Instr::Call {
            func_id: callee,
            args,
            ..
        } => {
            for (index, (&reg, &contract)) in args
                .iter()
                .zip(
                    arena.program.functions[callee.0 as usize]
                        .arg_contracts(arena.program)
                        .expect("checked signature"),
                )
                .enumerate()
            {
                arena
                    .check(contract, arena.fact(state, types, reg))
                    .map_err(|error| VerifyError::ContractMismatch {
                        func: func_id,
                        pc,
                        position: ContractPosition::Argument {
                            callee: *callee,
                            index: u32::try_from(index)
                                .expect("verified signature or schema arity"),
                        },
                        error,
                    })?;
            }
        }
        Instr::Ret { rets, .. } => {
            for (index, (&reg, &contract)) in rets
                .iter()
                .zip(
                    arena.program.functions[func_id as usize]
                        .ret_contracts(arena.program)
                        .expect("checked signature"),
                )
                .enumerate()
            {
                arena
                    .check(contract, arena.fact(state, types, reg))
                    .map_err(|error| VerifyError::ContractMismatch {
                        func: func_id,
                        pc,
                        position: ContractPosition::Result {
                            index: u32::try_from(index)
                                .expect("verified signature or schema arity"),
                        },
                        error,
                    })?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn verify_flow(
    program: &Program,
    func_id: u32,
    blocks: &[BasicBlock],
    reachable: &[bool],
    types: RegisterTypes<'_>,
    decoded: &[DecodedInstr],
) -> Result<(), VerifyError> {
    if !types.iter().any(|kind| kind == ValueType::Agg) {
        return Ok(());
    }
    let (mut arena, inputs, outputs) =
        solve_flow(program, func_id, blocks, reachable, types, decoded);
    for (index, (block, mut state)) in blocks.iter().zip(inputs).enumerate() {
        if !reachable[index] {
            continue;
        }
        for di in &decoded[block.instr_start..block.instr_end] {
            validate(&mut arena, func_id, di.offset, &di.instr, types, &state)?;
            transfer(&mut arena, &di.instr, types, &mut state);
        }
        debug_assert_eq!(state, outputs[index], "shape OUT mismatch");
    }
    Ok(())
}

fn solve_flow<'a>(
    program: &'a Program,
    func_id: u32,
    blocks: &[BasicBlock],
    reachable: &[bool],
    types: RegisterTypes<'_>,
    decoded: &[DecodedInstr],
) -> (Arena<'a>, Vec<ShapeState>, Vec<ShapeState>) {
    let mut arena = Arena::new(program);
    let mut entry = ShapeState::unknown();
    for (index, &id) in program.functions[func_id as usize]
        .arg_contracts(program)
        .expect("checked signature")
        .iter()
        .enumerate()
    {
        if let Fact::Shape(shape) = arena.import(id) {
            entry.set_agg(index + 1, Some(shape));
        }
    }
    let arena = core::cell::RefCell::new(arena);
    let mut edge_scratch: Option<ShapeState> = None;
    let (inputs, outputs) = dataflow::solve_forward(
        blocks,
        reachable,
        entry,
        ShapeState {
            top: true,
            aggs: Vec::new(),
        },
        |pred, arm, _, acc, incoming| {
            let branch = &decoded[blocks[pred].instr_end - 1].instr;
            let incoming = if let Some((target, args)) =
                branch.edge(arm).filter(|(_, args)| !args.is_empty())
            {
                let params = blocks::declaration(decoded, target).expect("validated target");
                let scratch = edge_scratch.get_or_insert_with(|| incoming.clone());
                blocks::transfer_edge(incoming, scratch, params, args);
                &*scratch
            } else {
                incoming
            };
            if incoming.top {
                return false;
            }
            if acc.top {
                acc.clone_from(incoming);
                return true;
            }
            let mut arena = arena.borrow_mut();
            let mut changed = false;
            acc.aggs.retain_mut(|(reg, fact)| {
                let common = incoming
                    .agg(*reg)
                    .and_then(|&other| arena.common(Fact::Shape(*fact), Fact::Shape(other)));
                match common {
                    Some(Fact::Shape(common)) => {
                        changed |= *fact != common;
                        *fact = common;
                        true
                    }
                    _ => {
                        changed = true;
                        false
                    }
                }
            });
            changed
        },
        |_, block, state| {
            let mut arena = arena.borrow_mut();
            for di in &decoded[block.instr_start..block.instr_end] {
                transfer(&mut arena, &di.instr, types, state);
            }
        },
    );
    (arena.into_inner(), inputs, outputs)
}

pub(super) fn verify_definitions(program: &Program) -> Result<(), VerifyError> {
    use crate::contract::ContractDefinitionError as E;
    for (id, contract) in program.contracts.iter() {
        let fail = |error| VerifyError::InvalidContract {
            contract: id,
            error,
        };
        match contract {
            Contract::Struct { type_id, fields } => {
                let schema = program
                    .types
                    .structs
                    .get(type_id.0 as usize)
                    .ok_or_else(|| fail(E::UnknownStruct(type_id)))?;
                let kinds = program
                    .types
                    .struct_field_types(schema)
                    .expect("validated nominal schema");
                if kinds.len() != fields.len() {
                    return Err(fail(E::FieldCount {
                        expected: kinds.len(),
                        actual: fields.len(),
                    }));
                }
                for (index, (&expected, &child)) in kinds.iter().zip(fields).enumerate() {
                    let actual = program.contracts.kind(child).expect("backward child");
                    if expected != actual {
                        return Err(fail(E::MemberKind {
                            index: u32::try_from(index)
                                .expect("verified signature or schema arity"),
                            expected,
                            actual,
                        }));
                    }
                }
            }
            Contract::Array {
                elem_type_id,
                element,
            } => {
                let expected = *program
                    .types
                    .array_elems
                    .get(elem_type_id.0 as usize)
                    .ok_or_else(|| fail(E::UnknownArray(elem_type_id)))?;
                let actual = program.contracts.kind(element).expect("backward child");
                if expected != actual {
                    return Err(fail(E::MemberKind {
                        index: 0,
                        expected,
                        actual,
                    }));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_fact_clone_reuses_capacity_and_keeps_unreachable_distinct() {
        let mut state = ShapeState::unknown();
        state.set_agg(4, Some(0));
        let allocation = state.aggs.as_ptr();
        let mut incoming = ShapeState::unknown();
        incoming.set_agg(3, Some(1));
        state.clone_from(&incoming);
        assert_eq!(state, incoming);
        assert_eq!(state.aggs.as_ptr(), allocation);
        incoming.aggs.clear();
        incoming.top = true;
        state.clone_from(&incoming);
        assert!(state.top);
        assert!(state.aggs.is_empty());
        assert_ne!(state, ShapeState::unknown());
    }
}
