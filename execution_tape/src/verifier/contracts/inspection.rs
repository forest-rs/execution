// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Optional source-level facts. Execution artifacts never retain this data.

use super::*;
use crate::analysis::{bitset::BitSet, cfg};
use crate::contract::{ContractTable, ContractTableBuilder};
use crate::opcode::Opcode;
use crate::verifier::{
    VerifiedProgram, VerifyConfig, compute_must_init, initial_init, verify_program,
};
use core::ops::Range;

/// A borrowed, verified program from which optional function analyses can be requested.
///
/// Construction verifies once without cloning the `Program`. Only the selected function's optional
/// analysis is retained by [`Self::function`]. A prepared program can instead lend its existing
/// verification proof through `ProgramAnalysis::from(&prepared)`.
pub struct ProgramAnalysis<'a> {
    program: &'a Program,
}
impl<'a> ProgramAnalysis<'a> {
    /// Verifies all of `program`. Rejected programs produce no inferred facts.
    pub fn new(program: &'a Program, config: &VerifyConfig) -> Result<Self, VerifyError> {
        verify_program(program, config)?;
        Ok(Self { program })
    }

    /// Computes optional facts for one function, or returns `None` for an absent function.
    ///
    /// The result borrows the immutable program and owns decoded source instructions, block
    /// states, compact write effects, and an interned contract graph. Ordinary verification/preparation retains none of these.
    #[must_use]
    pub fn function(&self, function: FuncId) -> Option<FunctionAnalysis<'a>> {
        self.program.functions.get(function.0 as usize)?;
        Some(FunctionAnalysis::new(self.program, function))
    }
}
impl<'a> From<&'a VerifiedProgram> for ProgramAnalysis<'a> {
    fn from(program: &'a VerifiedProgram) -> Self {
        Self {
            program: program.program(),
        }
    }
}

/// A guaranteed value fact at a source instruction, distinct from its fixed storage kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueFact {
    /// The instruction is not reachable from the function entry.
    Unreachable,
    /// The register is not definitely initialized along every incoming path.
    Uninitialized,
    /// A contract in [`InstructionFacts::contracts`], not in the source program's contract table.
    /// `Kind(Agg)` is an initialized aggregate with no known nested structure.
    Contract(ContractId),
}

/// The local source of a register's current value. This does not claim ultimate provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactOrigin {
    /// The function's initial effect token.
    Effect,
    /// A value argument at function entry (zero-based, excluding the effect token).
    Argument {
        /// Argument position.
        index: u32,
    },
    /// The last defining instruction within the current basic block.
    Instruction {
        /// Source bytecode offset.
        pc: u32,
    },
    /// A fact admitted through incoming control flow. Inspect incoming edges for evidence.
    /// Block zero also meets the invocation's argument/effect seeds, which are not CFG edges.
    BlockEntry {
        /// Source offset of the current block's first instruction.
        pc: u32,
    },
}

/// Before/after facts for one virtual register at the selected source instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisterFact {
    /// Virtual register identifier.
    pub register: u32,
    /// Fixed declaration, including for uninitialized or unreachable registers.
    pub storage_kind: ValueType,
    /// Guaranteed value before the instruction.
    pub before: ValueFact,
    /// Guaranteed value after the local source transfer, before outgoing edge bindings.
    /// Call results assume successful callee return, not merely a trace dispatch-complete event.
    pub after: ValueFact,
    /// Local definition of the initialized incoming value, if any.
    pub origin_before: Option<FactOrigin>,
    /// Local definition of the initialized outgoing value, if any.
    pub origin_after: Option<FactOrigin>,
}

#[derive(Debug, Clone, Copy)]
struct Change {
    register: u32,
    contract: Option<ContractId>,
}
#[derive(Debug, Clone, Copy)]
struct Incoming {
    target: usize,
    source: usize,
    arm: usize,
}

#[derive(Debug, Default)]
struct Contracts(Vec<(usize, ContractId)>);
impl Clone for Contracts {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
    fn clone_from(&mut self, other: &Self) {
        self.0.clone_from(&other.0);
    }
}
impl Contracts {
    fn get(&self, reg: u32) -> Option<ContractId> {
        self.0
            .binary_search_by_key(&(reg as usize), |&(reg, _)| reg)
            .ok()
            .map(|i| self.0[i].1)
    }
    fn set(&mut self, reg: u32, id: Option<ContractId>) {
        match (
            self.0
                .binary_search_by_key(&(reg as usize), |&(reg, _)| reg),
            id,
        ) {
            (Ok(i), Some(id)) => self.0[i].1 = id,
            (Ok(i), None) => {
                self.0.remove(i);
            }
            (Err(i), Some(id)) => self.0.insert(i, (reg as usize, id)),
            (Err(_), None) => {}
        }
    }
}
#[derive(Debug)]
struct Cursor {
    block: usize,
    next: usize,
    contracts: Contracts,
    initialized: BitSet,
    definitions: BTreeMap<u32, u32>,
}

/// Optional selected-function analysis, separate from interpreter preparation.
///
/// Storage grows with source instructions, blocks, register writes and distinct contracts, not with every
/// register at every instruction. Sequential queries reuse a cursor; backwards queries reconstruct
/// from the containing block's entry. Contract definitions are shared and never unfolded as trees.
pub struct FunctionAnalysis<'a> {
    program: &'a Program,
    function: FuncId,
    decoded: Vec<DecodedInstr>,
    blocks: Vec<BasicBlock>,
    reachable: Vec<bool>,
    input_init: Vec<BitSet>,
    output_init: Vec<BitSet>,
    inputs: Vec<Contracts>,
    outputs: Vec<Contracts>,
    changes: Vec<Change>,
    change_offsets: Vec<usize>,
    contracts: ContractTable,
    kinds: BTreeMap<ValueType, ContractId>,
    incoming: Vec<Incoming>,
    incoming_ranges: Vec<Range<usize>>,
    cursor: Cursor,
}

impl<'a> FunctionAnalysis<'a> {
    fn new(program: &'a Program, function: FuncId) -> Self {
        let definition = &program.functions[function.0 as usize];
        let bytes = definition.bytecode(program).expect("verified bytecode");
        let decoded = decode_instructions(bytes).expect("verified instructions");
        let blocks = cfg::build_basic_blocks(
            u32::try_from(bytes.len()).expect("verified code size"),
            &decoded,
            &cfg::compute_boundaries(bytes.len(), &decoded),
        )
        .expect("verified control flow");
        let reachable = cfg::compute_reachable(&blocks);
        let types = definition.verified_register_types(program);
        let (input_init, output_init) = compute_must_init(
            &blocks,
            &reachable,
            definition.reg_count as usize,
            &initial_init(definition.reg_count as usize, definition.arg_count as usize),
            &decoded,
        )
        .expect("verified initialization");
        let (mut arena, inputs, outputs) =
            solve_flow(program, function.0, &blocks, &reachable, types, &decoded);
        let mut raw_changes = Vec::new();
        let mut change_offsets = Vec::with_capacity(decoded.len() + 1);
        for (b, block) in blocks.iter().enumerate() {
            let mut state = inputs[b].clone();
            for di in &decoded[block.instr_start..block.instr_end] {
                change_offsets.push(raw_changes.len());
                if !reachable[b] {
                    continue;
                }
                transfer(&mut arena, &di.instr, types, &mut state);
                for register in di
                    .instr
                    .writes()
                    .filter(|&reg| types.get(reg) == Some(ValueType::Agg))
                {
                    raw_changes.push((register, state.agg(register as usize).copied()));
                }
            }
        }
        change_offsets.push(raw_changes.len());
        let mut roots = Vec::new();
        for state in inputs.iter().chain(&outputs) {
            if !state.top {
                roots.extend(state.aggs.iter().map(|&(_, id)| id));
            }
        }
        roots.extend(raw_changes.iter().filter_map(|&(_, id)| id));
        let (contracts, shapes, kinds) = freeze(&arena, roots, types);
        let convert = |states: Vec<ShapeState>| {
            states
                .into_iter()
                .map(|state| {
                    Contracts(if state.top {
                        Vec::new()
                    } else {
                        state
                            .aggs
                            .into_iter()
                            .map(|(reg, id)| (reg, shapes[id as usize].expect("retained shape")))
                            .collect()
                    })
                })
                .collect::<Vec<_>>()
        };
        let inputs = convert(inputs);
        let outputs = convert(outputs);
        let changes = raw_changes
            .into_iter()
            .map(|(register, id)| Change {
                register,
                contract: id.map(|id| shapes[id as usize].expect("retained write shape")),
            })
            .collect();
        let mut incoming = Vec::new();
        for (source, block) in blocks.iter().enumerate() {
            if !reachable[source] {
                continue;
            }
            for (arm, target) in block.succs.iter().enumerate() {
                if let Some(target) = *target {
                    incoming.push(Incoming {
                        target,
                        source,
                        arm,
                    });
                }
            }
        }
        incoming.sort_unstable_by_key(|e| (e.target, e.source, e.arm));
        let incoming_ranges = (0..blocks.len())
            .map(|b| {
                incoming.partition_point(|e| e.target < b)
                    ..incoming.partition_point(|e| e.target <= b)
            })
            .collect();
        Self {
            program,
            function,
            decoded,
            blocks,
            reachable,
            input_init,
            output_init,
            inputs,
            outputs,
            changes,
            change_offsets,
            contracts,
            kinds,
            incoming,
            incoming_ranges,
            cursor: Cursor {
                block: usize::MAX,
                next: 0,
                contracts: Contracts::default(),
                initialized: BitSet::new_empty(definition.reg_count as usize),
                definitions: BTreeMap::new(),
            },
        }
    }

    /// Borrows the analysis-local shared definitions. IDs differ from source program IDs.
    #[must_use]
    pub fn contracts(&self) -> &ContractTable {
        &self.contracts
    }

    /// Queries a source instruction boundary, including zero-cost block declarations.
    /// Returns `None` for an absent PC. Facts describe static source dataflow, not observed
    /// execution: call outputs assume successful callee return, while branch outputs precede
    /// outgoing edge bindings. This differs from trace instruction-completion timing.
    /// Incoming block parameters are already admitted before the declaration itself.
    pub fn instruction(&mut self, pc: u32) -> Option<InstructionFacts<'_>> {
        let index = self
            .decoded
            .binary_search_by_key(&pc, |di| di.offset)
            .ok()?;
        let block = self
            .blocks
            .partition_point(|b| b.instr_start <= index)
            .checked_sub(1)?;
        if self.cursor.block != block || self.cursor.next > index {
            self.cursor.block = block;
            self.cursor.next = self.blocks[block].instr_start;
            self.cursor.contracts.clone_from(&self.inputs[block]);
            self.cursor.initialized.clone_from(&self.input_init[block]);
            self.cursor.definitions.clear();
            if let Instr::Block { params } = &self.decoded[self.cursor.next].instr {
                for &reg in params {
                    self.cursor.initialized.set(reg as usize);
                }
            }
        }
        while self.cursor.next < index {
            let ix = self.cursor.next;
            apply(
                &self.decoded[ix].instr,
                &self.changes[self.change_offsets[ix]..self.change_offsets[ix + 1]],
                &mut self.cursor.contracts,
                &mut self.cursor.initialized,
            );
            if !matches!(self.decoded[ix].instr, Instr::Block { .. }) {
                for reg in self.decoded[ix].instr.writes() {
                    self.cursor.definitions.insert(reg, self.decoded[ix].offset);
                }
            }
            self.cursor.next += 1;
        }
        Some(InstructionFacts {
            analysis: self,
            index,
            block,
        })
    }

    fn value(
        &self,
        register: u32,
        reachable: bool,
        state: &Contracts,
        initialized: &BitSet,
    ) -> Option<ValueFact> {
        let kind = self.program.functions[self.function.0 as usize]
            .register_type(self.program, register)
            .ok()?;
        Some(if !reachable {
            ValueFact::Unreachable
        } else if !initialized.get(register as usize) {
            ValueFact::Uninitialized
        } else {
            ValueFact::Contract(state.get(register).unwrap_or(self.kinds[&kind]))
        })
    }
}

fn apply(instr: &Instr, changes: &[Change], contracts: &mut Contracts, initialized: &mut BitSet) {
    for change in changes {
        contracts.set(change.register, change.contract);
    }
    for reg in instr.writes() {
        initialized.set(reg as usize);
    }
}

/// A borrowed before/after view at one source PC. Contract IDs belong to this view's table.
pub struct InstructionFacts<'a> {
    analysis: &'a FunctionAnalysis<'a>,
    index: usize,
    block: usize,
}
impl<'a> InstructionFacts<'a> {
    /// Containing function.
    #[must_use]
    pub fn function(&self) -> FuncId {
        self.analysis.function
    }
    /// Source bytecode offset.
    #[must_use]
    pub fn pc(&self) -> u32 {
        self.analysis.decoded[self.index].offset
    }
    /// Source opcode, including block declarations.
    #[must_use]
    pub fn opcode(&self) -> Opcode {
        Opcode::from_u8(self.analysis.decoded[self.index].opcode).expect("verified opcode")
    }
    /// Whether control flow from the function entry reaches this instruction.
    #[must_use]
    pub fn reachable(&self) -> bool {
        self.analysis.reachable[self.block]
    }
    /// Whether this is the containing basic block's first source instruction.
    #[must_use]
    pub fn is_block_entry(&self) -> bool {
        self.index == self.analysis.blocks[self.block].instr_start
    }
    /// The shared analysis-local contract graph.
    #[must_use]
    pub fn contracts(&self) -> &'a ContractTable {
        &self.analysis.contracts
    }
    /// Queries any declared virtual register; invalid register IDs return `None`.
    #[must_use]
    pub fn register(&self, register: u32) -> Option<RegisterFact> {
        let a = self.analysis;
        let storage_kind = a.program.functions[a.function.0 as usize]
            .register_type(a.program, register)
            .ok()?;
        let before = a.value(
            register,
            self.reachable(),
            &a.cursor.contracts,
            &a.cursor.initialized,
        )?;
        let writes = a.decoded[self.index]
            .instr
            .writes()
            .any(|reg| reg == register);
        let after = if !self.reachable() {
            ValueFact::Unreachable
        } else if writes {
            let contract = a.changes
                [a.change_offsets[self.index]..a.change_offsets[self.index + 1]]
                .iter()
                .find(|change| change.register == register)
                .and_then(|change| change.contract);
            ValueFact::Contract(contract.unwrap_or(a.kinds[&storage_kind]))
        } else {
            before
        };
        let origin_before = matches!(before, ValueFact::Contract(_)).then(|| {
            if let Some(&pc) = a.cursor.definitions.get(&register) {
                FactOrigin::Instruction { pc }
            } else if self.block == 0 && a.incoming_ranges[0].is_empty() {
                if register == 0 {
                    FactOrigin::Effect
                } else {
                    FactOrigin::Argument {
                        index: register - 1,
                    }
                }
            } else {
                FactOrigin::BlockEntry {
                    pc: a.blocks[self.block].start_pc,
                }
            }
        });
        let origin_after = if !matches!(after, ValueFact::Contract(_)) {
            None
        } else if !matches!(a.decoded[self.index].instr, Instr::Block { .. })
            && a.decoded[self.index]
                .instr
                .writes()
                .any(|reg| reg == register)
        {
            Some(FactOrigin::Instruction { pc: self.pc() })
        } else {
            origin_before
        };
        Some(RegisterFact {
            register,
            storage_kind,
            before,
            after,
            origin_before,
            origin_after,
        })
    }
    /// Reachable incoming control-flow arms for the containing block. Same-target arms remain
    /// separate. Binding facts read the predecessor before simultaneous parameter transfer.
    /// External invocation is not a CFG edge; block zero additionally meets argument/effect seeds.
    pub fn incoming_edges(&self) -> impl ExactSizeIterator<Item = IncomingEdge<'a>> + '_ {
        self.analysis.incoming[self.analysis.incoming_ranges[self.block].clone()]
            .iter()
            .map(|source| IncomingEdge {
                analysis: self.analysis,
                source: *source,
            })
    }
}

/// One reachable predecessor arm of an inspected instruction's containing basic block.
#[derive(Clone, Copy)]
pub struct IncomingEdge<'a> {
    analysis: &'a FunctionAnalysis<'a>,
    source: Incoming,
}
/// A simultaneous block-parameter binding and its guaranteed incoming value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeBinding {
    /// Destination parameter register at block entry.
    pub parameter: u32,
    /// Source register in the predecessor.
    pub argument: u32,
    /// Source value before the transfer, using the instruction view's contract table.
    pub value: ValueFact,
}
impl IncomingEdge<'_> {
    /// Source terminator's bytecode offset.
    #[must_use]
    pub fn source_pc(&self) -> u32 {
        self.instruction().offset
    }
    /// Source arm: 0 is jump/true, 1 is false.
    #[must_use]
    pub fn arm(&self) -> usize {
        self.source.arm
    }
    /// Destination block header PC.
    #[must_use]
    pub fn target_pc(&self) -> u32 {
        self.analysis.blocks[self.source.target].start_pc
    }
    fn instruction(&self) -> &DecodedInstr {
        &self.analysis.decoded[self.analysis.blocks[self.source.source].instr_end - 1]
    }
    /// Reads an unchanged predecessor register before the transfer, including for untyped joins.
    #[must_use]
    pub fn register(&self, register: u32) -> Option<ValueFact> {
        self.analysis.value(
            register,
            true,
            &self.analysis.outputs[self.source.source],
            &self.analysis.output_init[self.source.source],
        )
    }
    /// Iterates declared parameter/source pairs. Untyped edges have no bindings.
    pub fn bindings(&self) -> impl ExactSizeIterator<Item = EdgeBinding> + '_ {
        let (target, args) = self
            .instruction()
            .instr
            .edge(self.source.arm)
            .expect("CFG edge");
        let params = blocks::declaration(&self.analysis.decoded, target).unwrap_or(&[]);
        params
            .iter()
            .zip(args)
            .map(|(&parameter, &argument)| EdgeBinding {
                parameter,
                argument,
                value: self.register(argument).expect("verified edge register"),
            })
    }
}

fn freeze(
    arena: &Arena<'_>,
    mut pending: Vec<u32>,
    types: RegisterTypes<'_>,
) -> (
    ContractTable,
    Vec<Option<ContractId>>,
    BTreeMap<ValueType, ContractId>,
) {
    let mut keep = vec![false; arena.nodes.len()];
    while let Some(id) = pending.pop() {
        if core::mem::replace(&mut keep[id as usize], true) {
            continue;
        }
        let view = arena.view(Fact::Shape(id)).expect("shape node");
        for index in 0..view.members.len() {
            if let Some(Fact::Shape(child)) = view.members.get(index) {
                pending.push(child);
            }
        }
    }
    let mut builder = ContractTableBuilder::new();
    let mut kinds = BTreeMap::new();
    for kind in types.iter() {
        kinds
            .entry(kind)
            .or_insert_with(|| builder.intern(Contract::Kind(kind)).expect("kind capacity"));
    }
    let mut ids = vec![None; arena.nodes.len()];
    let mut tuple = Vec::new();
    let mut fields = Vec::new();
    for (index, retained) in keep.into_iter().enumerate() {
        if !retained {
            continue;
        }
        let view = arena
            .view(Fact::Shape(u32::try_from(index).expect("shape capacity")))
            .expect("shape node");
        tuple.clear();
        for child in 0..view.members.len() {
            tuple.push(view.members.get(child).map(|fact| match fact {
                Fact::Kind(kind) => *kinds.entry(kind).or_insert_with(|| {
                    builder.intern(Contract::Kind(kind)).expect("kind capacity")
                }),
                Fact::Shape(id) => ids[id as usize].expect("child-before-parent facts"),
            }));
        }
        let contract = match view.descriptor {
            AggType::Tuple { .. } => Contract::Tuple(&tuple),
            AggType::Struct { type_id } => {
                fields.clear();
                // A nominal field always has at least its schema kind, even if a nested proof
                // has been lost. Unknown tuple members, in contrast, remain unknown.
                let schema = &arena.program.types.structs[type_id.0 as usize];
                let schema = arena
                    .program
                    .types
                    .struct_field_types(schema)
                    .expect("verified schema");
                for (i, id) in tuple.iter().enumerate() {
                    fields.push(id.unwrap_or_else(|| {
                        *kinds.entry(schema[i]).or_insert_with(|| {
                            builder
                                .intern(Contract::Kind(schema[i]))
                                .expect("kind capacity")
                        })
                    }));
                }
                Contract::Struct {
                    type_id,
                    fields: &fields,
                }
            }
            AggType::Array { elem_type_id } => Contract::Array {
                elem_type_id,
                element: tuple[0].expect("array element kind"),
            },
        };
        ids[index] = Some(
            builder
                .intern(contract)
                .expect("inspection contract capacity"),
        );
    }
    (builder.finish(), ids, kinds)
}

impl fmt::Debug for ProgramAnalysis<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProgramAnalysis")
            .field("functions", &self.program.functions.len())
            .finish_non_exhaustive()
    }
}
impl fmt::Debug for FunctionAnalysis<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FunctionAnalysis")
            .field("function", &self.function)
            .field("blocks", &self.blocks.len())
            .field("contracts", &self.contracts.len())
            .field("aggregate_writes", &self.changes.len())
            .finish_non_exhaustive()
    }
}
impl fmt::Debug for InstructionFacts<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstructionFacts")
            .field("function", &self.function())
            .field("pc", &self.pc())
            .field("opcode", &self.opcode())
            .field("reachable", &self.reachable())
            .finish_non_exhaustive()
    }
}
impl fmt::Debug for IncomingEdge<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IncomingEdge")
            .field("source_pc", &self.source_pc())
            .field("arm", &self.arm())
            .field("target_pc", &self.target_pc())
            .finish_non_exhaustive()
    }
}
