// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Interned, compositional value-contract storage.
//!
//! Definitions share flat immutable member buffers. Child IDs belong to the same table and must
//! precede their parents. Construction validates references and topology; the owner must validate
//! nominal schema refinements before trusting them. Contracts do not establish value ownership.
//!
//! ```
//! use execution_tape::contract::{Contract, ContractTableBuilder};
//! use execution_tape::program::ValueType;
//! let mut builder = ContractTableBuilder::new();
//! let integer = builder.intern(Contract::Kind(ValueType::I64)).unwrap();
//! let tuple = builder.intern(Contract::Tuple(&[Some(integer)])).unwrap();
//! let table = builder.finish();
//! assert_eq!(table.get(tuple), Some(Contract::Tuple(&[Some(integer)])));
//! ```

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
/// owning program to validate its schema. This builder validates the graph itself.
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn deep_graph_is_flat_and_has_a_coarse_root_kind() {
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
        assert_eq!(table.kind(integer), Some(ValueType::I64));
        assert_eq!(table.kind(root), Some(ValueType::Agg));
    }
}
