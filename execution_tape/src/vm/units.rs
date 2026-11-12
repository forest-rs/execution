// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Verified programs installed in one VM store.
//!
//! A [`VerifiedProgram`] is immutable shared code. Installing it gives that code a [`UnitId`] in
//! one VM. Execution always enters an installed unit, and every frame runs the unit it entered:
//! installing new code never changes code that is already executing or referenced.
//!
//! Units are never removed. The VM keeps each installed program alive until the VM is dropped;
//! [`Units`] reports how many there are and what they retain.

use super::*;

/// Code and store-local metadata for one installed program.
pub(crate) struct Unit {
    pub(crate) program: Arc<VerifiedProgram>,
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

    fn install(&mut self, program: Arc<VerifiedProgram>) -> Result<UnitId, InstallError> {
        let id = u32::try_from(self.units.len())
            .ok()
            .filter(|&id| id < u32::MAX)
            .ok_or(InstallError::CapacityExceeded)?;
        self.units.push(Unit { program });
        Ok(UnitId(id))
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
    /// Installation executes no code and spends no execution budget. Every call creates a
    /// distinct unit, including for a program that is already installed. The VM retains the
    /// program until the VM is dropped; no unit is ever removed. Already running or suspended
    /// frames keep the units they entered, so installing replacement code never changes them.
    pub fn install(
        &mut self,
        program: impl Into<Arc<VerifiedProgram>>,
    ) -> Result<UnitId, InstallError> {
        self.units.install(program.into())
    }

    /// Inspects the units installed in this VM.
    #[must_use]
    pub fn units(&self) -> Units<'_> {
        Units { table: &self.units }
    }
}
