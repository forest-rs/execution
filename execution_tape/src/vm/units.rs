// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Verified programs installed in one VM store.
//!
//! A [`VerifiedProgram`] is immutable shared code. Installing it gives that code a [`UnitId`] in
//! one VM. Execution always enters an installed unit, and every frame runs the unit it entered:
//! installing new code never changes code that is already executing or referenced.
//!
//! Installation interns the program's struct and array types as shapes of the VM heap, so
//! values built by one unit keep their structural meaning in another.
//!
//! Units are never removed. The VM keeps each installed program alive until the VM is dropped;
//! [`Units`] reports how many there are and what they retain.

use super::*;
use crate::aggregates::AggShape;
use crate::program::{ElemTypeId, StructTypeDef, TypeId};
use crate::value::ShapeId;

/// Code and store-local metadata for one installed program.
pub(crate) struct Unit {
    pub(crate) program: Arc<VerifiedProgram>,
    /// Heap shape of each program [`TypeId`].
    structs: Box<[ShapeId]>,
    /// Heap shape of each program [`ElemTypeId`].
    arrays: Box<[ShapeId]>,
}

impl Unit {
    /// The heap shape of a struct type of this unit's program.
    #[inline]
    pub(crate) fn struct_shape(&self, ty: TypeId) -> Option<ShapeId> {
        self.structs.get(ty.0 as usize).copied()
    }

    /// The heap shape of an array element type of this unit's program.
    #[inline]
    pub(crate) fn array_shape(&self, ty: ElemTypeId) -> Option<ShapeId> {
        self.arrays.get(ty.0 as usize).copied()
    }

    fn metadata_bytes(&self) -> usize {
        size_of::<Self>() + size_of_val(&*self.structs) + size_of_val(&*self.arrays)
    }
}

/// The VM's append-only table of installed units.
#[derive(Default)]
pub(crate) struct UnitTable {
    units: Vec<Unit>,
}

impl UnitTable {
    #[inline]
    pub(crate) fn get(&self, unit: UnitId) -> Option<&Unit> {
        self.units.get(unit.0 as usize)
    }

    /// A unit this VM issued. Units are never removed, so frames and admitted values always
    /// name an existing unit.
    #[inline]
    pub(crate) fn unit(&self, unit: UnitId) -> &Unit {
        &self.units[unit.0 as usize]
    }

    fn install(
        &mut self,
        program: Arc<VerifiedProgram>,
        heap: &mut AggHeap,
    ) -> Result<UnitId, InstallError> {
        let id = u32::try_from(self.units.len())
            .ok()
            .filter(|&id| id < u32::MAX)
            .ok_or(InstallError::CapacityExceeded)?;
        self.units.push(Unit::new(program, heap)?);
        Ok(UnitId(id))
    }
}

impl Unit {
    /// Interns `program`'s struct and array types in `heap`.
    pub(crate) fn new(
        program: Arc<VerifiedProgram>,
        heap: &mut AggHeap,
    ) -> Result<Self, InstallError> {
        let types = &program.program().types;
        let mut intern = |shape| {
            heap.intern_shape(shape)
                .map_err(|_| InstallError::CapacityExceeded)
        };
        let structs = types
            .structs
            .iter()
            .map(|st| {
                let names = types
                    .struct_field_name_ids(st)
                    .expect("verified struct field names");
                let field_names = names
                    .iter()
                    .map(|&name| {
                        String::from(types.field_name_str(name).expect("verified field name"))
                    })
                    .collect();
                let field_types = types
                    .struct_field_types(st)
                    .expect("verified struct field types")
                    .to_vec();
                intern(AggShape::Struct(StructTypeDef {
                    field_names,
                    field_types,
                }))
            })
            .collect::<Result<_, _>>()?;
        let arrays = types
            .array_elems
            .iter()
            .map(|&element| intern(AggShape::Array(element)))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            program,
            structs,
            arrays,
        })
    }
}

/// Why a program could not be installed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstallError {
    /// The VM cannot represent another unit.
    CapacityExceeded,
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityExceeded => f.write_str("VM unit capacity exhausted"),
        }
    }
}

impl core::error::Error for InstallError {}

/// Read-only view of the units installed in one VM.
#[derive(Clone, Copy)]
pub struct Units<'a> {
    table: &'a UnitTable,
}

impl fmt::Debug for Units<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Units")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl<'a> Units<'a> {
    /// Number of installed units, including units no frame or value still references.
    #[must_use]
    pub fn len(&self) -> usize {
        self.table.units.len()
    }

    /// Whether nothing has been installed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.table.units.is_empty()
    }

    /// The program installed as `unit`, or `None` for an ID this VM did not issue.
    #[must_use]
    pub fn program(&self, unit: UnitId) -> Option<&'a Arc<VerifiedProgram>> {
        self.table.get(unit).map(|unit| &unit.program)
    }

    /// Bytes of VM-owned metadata for all units, excluding the shared programs themselves.
    ///
    /// Shapes live in the VM heap; see [`crate::aggregates::AggHeap::shape_count`].
    #[must_use]
    pub fn metadata_bytes(&self) -> usize {
        self.table.units.iter().map(Unit::metadata_bytes).sum()
    }

    /// Installed units in installation order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (UnitId, &'a Arc<VerifiedProgram>)> + 'a {
        self.table.units.iter().enumerate().map(|(index, unit)| {
            let index = u32::try_from(index).expect("install bounds unit IDs to u32");
            (UnitId(index), &unit.program)
        })
    }
}

impl<H: Host> Vm<H> {
    /// Installs `program` as a new unit of this VM and returns its identity.
    ///
    /// Installation executes no code and spends no execution budget. It interns the program's
    /// struct and array types as shapes of this VM's heap. Every call creates a distinct unit,
    /// including for a program that is already installed. The VM retains the
    /// program until the VM is dropped; no unit is ever removed. Already running or suspended
    /// frames keep the units they entered, so installing replacement code never changes them.
    pub fn install(
        &mut self,
        program: impl Into<Arc<VerifiedProgram>>,
    ) -> Result<UnitId, InstallError> {
        self.units.install(program.into(), self.memory.aggs_mut())
    }

    /// Inspects the units installed in this VM.
    #[must_use]
    pub fn units(&self) -> Units<'_> {
        Units { table: &self.units }
    }
}
