// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Compositional value contracts owned by a program.
//!
//! Contracts refine storage and host ABI kinds without changing their representation. References
//! are local to one table and point only to earlier entries; contract graphs are finite DAGs.
//!
//! Most frontends use [`crate::asm::ProgramBuilder::contract`]. Low-level authors can construct
//! the same table explicitly, then install it in a raw program before ordinary verification:
//!
//! ```
//! extern crate alloc;
//! use alloc::vec;
//! use execution_tape::asm::Asm;
//! use execution_tape::contract::{Contract, ContractTableBuilder};
//! use execution_tape::program::{ByteRange, FunctionDef, Program, TypeTableDef, ValueType};
//! use execution_tape::verifier::{VerifyConfig, verify_program_owned};
//!
//! let mut contracts = ContractTableBuilder::new();
//! let integer = contracts.intern(Contract::Kind(ValueType::I64)).unwrap();
//! let tuple = contracts.intern(Contract::Tuple(&[Some(integer)])).unwrap();
//! let mut body = Asm::new();
//! body.set_register_types(vec![ValueType::Unit, ValueType::Agg]);
//! body.ret(0, &[1]);
//! let parts = body.finish_parts().unwrap();
//! let mut program = Program::new(vec![], vec![], vec![], TypeTableDef::default(), vec![FunctionDef {
//!     arg_types: vec![ValueType::Agg], ret_types: vec![ValueType::Agg],
//!     register_types: parts.register_types, bytecode: parts.bytecode, spans: parts.spans,
//! }]);
//! program.contracts = contracts.finish();
//! program.contract_ids = vec![tuple, tuple];
//! program.functions[0].arg_types = ByteRange { offset: 0, len: 1 };
//! program.functions[0].ret_types = ByteRange { offset: 1, len: 1 };
//! verify_program_owned(program, &VerifyConfig::default()).unwrap();
//! ```
//!
//! A contract proves neither heap ownership nor host object validity. Result validation can fail
//! after host work has occurred; it does not roll back external effects.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::fmt;
use core::hash::{Hash, Hasher};

use crate::program::{ByteRange, ElemTypeId, TypeId, ValueType};

/// An index in one program's contract table. It has no meaning in another program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContractId(pub u32);

/// A borrowed value contract, usable both for construction and inspection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Contract<'a> {
    /// A storage/ABI kind, without a structural refinement.
    Kind(ValueType),
    /// A fixed-length tuple. `None` leaves that slot unconstrained; it does not prove a kind.
    Tuple(&'a [Option<ContractId>]),
    /// A nominal struct with contracts for its fields, in schema order.
    Struct {
        /// Original nominal struct identity.
        type_id: TypeId,
        /// Field contracts; their kinds must agree with the nominal schema.
        fields: &'a [ContractId],
    },
    /// A nominal array whose every element satisfies one contract.
    Array {
        /// Original nominal element-type identity.
        elem_type_id: ElemTypeId,
        /// Element contract; its kind must agree with the nominal element schema.
        element: ContractId,
    },
}

impl Contract<'_> {
    /// The storage/ABI kind of values satisfying this contract.
    #[must_use]
    pub fn kind(self) -> ValueType {
        *self.kind_ref()
    }

    fn kind_ref(&self) -> &ValueType {
        match self {
            Self::Kind(kind) => kind,
            Self::Tuple(_) | Self::Struct { .. } | Self::Array { .. } => &ValueType::Agg,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    Kind(ValueType),
    Tuple(ByteRange),
    Struct {
        type_id: TypeId,
        fields: ByteRange,
    },
    Array {
        elem_type_id: ElemTypeId,
        element: ContractId,
    },
}

impl Node {
    fn kind_ref(&self) -> &ValueType {
        match self {
            Self::Kind(kind) => kind,
            Self::Tuple(_) | Self::Struct { .. } | Self::Array { .. } => &ValueType::Agg,
        }
    }
}

/// Packed immutable contracts. Equal nodes share one entry; children are borrowed in place.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContractTable {
    nodes: Vec<Node>,
    tuple_members: Vec<Option<ContractId>>,
    struct_members: Vec<ContractId>,
}

impl ContractTable {
    /// Number of distinct contracts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the table has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Borrows a contract, or returns `None` for an absent program-local ID.
    #[must_use]
    pub fn get(&self, id: ContractId) -> Option<Contract<'_>> {
        Some(match self.nodes.get(id.0 as usize)? {
            Node::Kind(kind) => Contract::Kind(*kind),
            Node::Tuple(range) => Contract::Tuple(
                &self.tuple_members[range.offset as usize..range.end().ok()? as usize],
            ),
            Node::Struct { type_id, fields } => Contract::Struct {
                type_id: *type_id,
                fields: &self.struct_members[fields.offset as usize..fields.end().ok()? as usize],
            },
            Node::Array {
                elem_type_id,
                element,
            } => Contract::Array {
                elem_type_id: *elem_type_id,
                element: *element,
            },
        })
    }

    /// Returns a contract's storage/ABI kind.
    #[must_use]
    pub fn kind(&self, id: ContractId) -> Option<ValueType> {
        self.nodes.get(id.0 as usize).map(|node| *node.kind_ref())
    }

    /// Iterates the table in child-before-parent order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (ContractId, Contract<'_>)> + '_ {
        (0..self.nodes.len()).map(|index| {
            let id = ContractId(u32::try_from(index).expect("contract table address limit"));
            (id, self.get(id).expect("stored contract"))
        })
    }

    pub(crate) fn kinds<'a>(&'a self, ids: &'a [ContractId]) -> ContractKinds<'a> {
        ContractKinds { ids, table: self }
    }
}

/// Why a contract could not be constructed. Failed insertion leaves the table unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractBuildError {
    /// A child is absent; create children before their parents in the same builder.
    UnknownChild(ContractId),
    /// The table or member buffer would exceed its 32-bit address space.
    Capacity,
}

impl fmt::Display for ContractBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownChild(id) => write!(f, "unknown contract child {}", id.0),
            Self::Capacity => f.write_str("contract table exceeds u32 addressing"),
        }
    }
}
impl core::error::Error for ContractBuildError {}

/// Constructs one shared contract table and discards its lookup indexes when frozen.
///
/// This validates references and topology. Nominal struct/array schema obligations require the
/// owning program's ordinary verifier. [`crate::asm::ProgramBuilder::contract`] uses this builder.
#[derive(Clone, Debug, Default)]
pub struct ContractTableBuilder {
    table: ContractTable,
    heads: BTreeMap<u64, u32>,
    previous: Vec<u32>,
}

impl ContractTableBuilder {
    /// Creates an empty table builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrows the definitions built so far without exposing mutable storage.
    #[must_use]
    pub fn table(&self) -> &ContractTable {
        &self.table
    }

    /// Interns a definition whose children already exist. Failure leaves the table unchanged.
    pub fn intern(&mut self, contract: Contract<'_>) -> Result<ContractId, ContractBuildError> {
        let check = |id: ContractId| {
            if self.table.get(id).is_some() {
                Ok(())
            } else {
                Err(ContractBuildError::UnknownChild(id))
            }
        };
        match contract {
            Contract::Tuple(members) => {
                for id in members.iter().flatten() {
                    check(*id)?;
                }
            }
            Contract::Struct { fields, .. } => {
                for id in fields {
                    check(*id)?;
                }
            }
            Contract::Array { element, .. } => check(element)?,
            Contract::Kind(_) => {}
        }
        let mut hash = ContractHasher(0xcbf2_9ce4_8422_2325);
        contract.hash(&mut hash);
        let hash = hash.finish();
        let previous = self.heads.get(&hash).copied().unwrap_or(u32::MAX);
        let mut candidate = previous;
        while candidate != u32::MAX {
            let id = ContractId(candidate);
            if self.table.get(id) == Some(contract) {
                return Ok(id);
            }
            candidate = self.previous[candidate as usize];
        }
        let id = ContractId(
            u32::try_from(self.table.nodes.len()).map_err(|_| ContractBuildError::Capacity)?,
        );
        if id.0 == u32::MAX {
            return Err(ContractBuildError::Capacity);
        }
        fn range(len: usize, added: usize) -> Result<ByteRange, ContractBuildError> {
            let offset = u32::try_from(len).map_err(|_| ContractBuildError::Capacity)?;
            let len = u32::try_from(added).map_err(|_| ContractBuildError::Capacity)?;
            offset
                .checked_add(len)
                .ok_or(ContractBuildError::Capacity)?;
            Ok(ByteRange { offset, len })
        }
        let node = match contract {
            Contract::Kind(kind) => Node::Kind(kind),
            Contract::Tuple(members) => {
                let range = range(self.table.tuple_members.len(), members.len())?;
                self.table.tuple_members.extend_from_slice(members);
                Node::Tuple(range)
            }
            Contract::Struct { type_id, fields } => {
                let range = range(self.table.struct_members.len(), fields.len())?;
                self.table.struct_members.extend_from_slice(fields);
                Node::Struct {
                    type_id,
                    fields: range,
                }
            }
            Contract::Array {
                elem_type_id,
                element,
            } => Node::Array {
                elem_type_id,
                element,
            },
        };
        self.table.nodes.push(node);
        self.previous.push(previous);
        self.heads.insert(hash, id.0);
        Ok(id)
    }

    /// Freezes definitions, releasing all construction-time lookup indexes.
    #[must_use]
    pub fn finish(self) -> ContractTable {
        self.table
    }
}

pub(crate) struct ContractHasher(pub(crate) u64);
impl Hasher for ContractHasher {
    // This is only an internal lookup hash; exact equality resolves every collision. Hash words
    // directly instead of serializing enum discriminants and IDs into a byte-at-a-time hash.
    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0 ^ value).wrapping_mul(0x100_0000_01b3);
    }
    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }
    fn write_u8(&mut self, value: u8) {
        self.write_u64(u64::from(value));
    }
    fn write_usize(&mut self, value: usize) {
        self.write_u64(u64::try_from(value).expect("supported pointer width"));
    }

    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
        }
    }
}

/// Borrowed ABI kinds projected from a guest signature's authoritative contract IDs.
#[derive(Clone, Copy)]
pub struct ContractKinds<'a> {
    ids: &'a [ContractId],
    table: &'a ContractTable,
}

impl<'a> ContractKinds<'a> {
    /// Number of signature positions.
    #[must_use]
    pub fn len(self) -> usize {
        self.ids.len()
    }
    /// Whether the signature is empty.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.ids.is_empty()
    }
    /// Returns the kind at one position.
    #[must_use]
    pub fn get(self, index: usize) -> Option<&'a ValueType> {
        let id = self.ids.get(index)?;
        self.table.nodes.get(id.0 as usize).map(Node::kind_ref)
    }
    /// Iterates kinds without allocating or expanding the contract graph.
    pub fn iter(self) -> ContractKindIter<'a> {
        ContractKindIter {
            ids: self.ids.iter(),
            table: self.table,
        }
    }
    /// Splits off the first ABI kind without expanding contracts.
    #[must_use]
    pub fn split_first(self) -> Option<(&'a ValueType, Self)> {
        let (first, rest) = self.ids.split_first()?;
        Some((
            self.table.nodes[first.0 as usize].kind_ref(),
            Self {
                ids: rest,
                table: self.table,
            },
        ))
    }
}
impl core::ops::Index<usize> for ContractKinds<'_> {
    type Output = ValueType;
    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("signature index")
    }
}
impl fmt::Debug for ContractKinds<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}
impl PartialEq for ContractKinds<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}
impl Eq for ContractKinds<'_> {}
impl Hash for ContractKinds<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.len().hash(state);
        for kind in self.iter() {
            kind.hash(state);
        }
    }
}

/// Iterator over a guest signature's projected ABI kinds.
#[derive(Clone, Debug)]
pub struct ContractKindIter<'a> {
    ids: core::slice::Iter<'a, ContractId>,
    table: &'a ContractTable,
}
impl<'a> Iterator for ContractKindIter<'a> {
    type Item = &'a ValueType;
    fn next(&mut self) -> Option<Self::Item> {
        self.ids
            .next()
            .map(|id| self.table.nodes[id.0 as usize].kind_ref())
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.ids.size_hint()
    }
}
impl DoubleEndedIterator for ContractKindIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.ids
            .next_back()
            .map(|id| self.table.nodes[id.0 as usize].kind_ref())
    }
}
impl ExactSizeIterator for ContractKindIter<'_> {}
impl<'a> IntoIterator for ContractKinds<'a> {
    type Item = &'a ValueType;
    type IntoIter = ContractKindIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl PartialEq<&[ValueType]> for ContractKinds<'_> {
    fn eq(&self, other: &&[ValueType]) -> bool {
        self.iter().eq(other.iter())
    }
}
impl<const N: usize> PartialEq<&[ValueType; N]> for ContractKinds<'_> {
    fn eq(&self, other: &&[ValueType; N]) -> bool {
        self.iter().eq(other.iter())
    }
}

/// Concrete disagreement with a compositional contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContractMismatch {
    /// The storage kind differs, including nominal host object types.
    Kind {
        /// Required ABI kind.
        expected: ValueType,
        /// Observed ABI kind.
        actual: ValueType,
    },
    /// The aggregate kind, nominal identity, or tuple length differs.
    Aggregate {
        /// Required descriptor.
        expected: crate::value::AggType,
        /// Observed descriptor.
        actual: crate::value::AggType,
    },
    /// A fixed-length aggregate has an invalid member count.
    Arity {
        /// Required field count.
        expected: usize,
        /// Observed field count.
        actual: usize,
    },
    /// An aggregate handle does not resolve.
    Heap(crate::aggregates::AggError),
}

/// A contract failure at a specific nested member, without copying its contract graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractViolation {
    /// Expected contract at the failing position, local to the originating program.
    pub contract: ContractId,
    /// Field/tuple/array indices from the checked root to the failure; empty for the root.
    pub path: Vec<usize>,
    /// Expected versus observed kind, shape, or handle failure.
    pub mismatch: ContractMismatch,
}
impl fmt::Display for ContractViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "contract {} at {:?}: {:?}",
            self.contract.0, self.path, self.mismatch
        )
    }
}
impl core::error::Error for ContractViolation {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match &self.mismatch {
            ContractMismatch::Heap(error) => Some(error),
            _ => None,
        }
    }
}

/// Guest boundary whose contract is being checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractPosition {
    /// Argument admission, including a closure's injected environment at index zero.
    Argument {
        /// Function being entered.
        callee: crate::value::FuncId,
        /// Zero-based value argument.
        index: u32,
    },
    /// Result before copying to the caller or committing top-level outputs.
    Result {
        /// Zero-based return position.
        index: u32,
    },
}

/// An invalid nominal refinement in a program contract definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContractDefinitionError {
    /// Struct identity is absent from the program schema.
    UnknownStruct(TypeId),
    /// Array element identity is absent from the program schema.
    UnknownArray(ElemTypeId),
    /// A struct refinement does not describe exactly its declared fields.
    FieldCount {
        /// Schema field count.
        expected: usize,
        /// Contract field count.
        actual: usize,
    },
    /// A refined member contradicts the nominal schema's ABI kind.
    MemberKind {
        /// Zero-based field index, or zero for an array element.
        index: u32,
        /// Schema kind.
        expected: ValueType,
        /// Contract kind.
        actual: ValueType,
    },
}

impl PartialEq<Vec<ValueType>> for ContractKinds<'_> {
    fn eq(&self, other: &Vec<ValueType>) -> bool {
        self.iter().eq(other.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn shares_exact_contracts_and_preserves_unknown_members() {
        let mut b = ContractTableBuilder::default();
        let integer = b.intern(Contract::Kind(ValueType::I64)).unwrap();
        let opaque = b.intern(Contract::Kind(ValueType::Agg)).unwrap();
        let tuple = b
            .intern(Contract::Tuple(&[Some(integer), None, Some(opaque)]))
            .unwrap();
        assert_eq!(
            tuple,
            b.intern(Contract::Tuple(&[Some(integer), None, Some(opaque)]))
                .unwrap()
        );
        assert_ne!(
            tuple,
            b.intern(Contract::Tuple(&[Some(integer), Some(opaque), None]))
                .unwrap()
        );
        assert_eq!(
            b.table().get(tuple),
            Some(Contract::Tuple(&[Some(integer), None, Some(opaque)]))
        );
        let before = b.table().clone();
        assert_eq!(
            b.intern(Contract::Tuple(&[Some(ContractId(999))])),
            Err(ContractBuildError::UnknownChild(ContractId(999)))
        );
        assert_eq!(*b.table(), before);
    }

    #[test]
    fn deep_graph_is_flat_and_kind_view_is_local() {
        let mut b = ContractTableBuilder::default();
        let integer = b.intern(Contract::Kind(ValueType::I64)).unwrap();
        let mut root = integer;
        for _ in 0..10_000 {
            root = b
                .intern(Contract::Tuple(&[Some(root), Some(root)]))
                .unwrap();
        }
        let table = b.finish();
        assert_eq!(table.len(), 10_001);
        let ids = [integer, root];
        assert_eq!(
            table.kinds(&ids).iter().copied().collect::<Vec<_>>(),
            vec![ValueType::I64, ValueType::Agg]
        );
        assert_eq!(table.kinds(&ids).get(2), None);
    }
}
