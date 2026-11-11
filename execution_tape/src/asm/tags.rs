// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Exception tag declarations.

use super::*;

/// An exception tag could not be declared. The builder is unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TagBuildError {
    /// Another tag in this builder already uses the name.
    DuplicateName {
        /// The repeated name.
        name: String,
        /// The tag that already uses it.
        existing: TagId,
    },
    /// A payload contract does not belong to this builder.
    Contract(ContractBuildError),
}

impl fmt::Display for TagBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateName { name, existing } => {
                write!(
                    f,
                    "tag name \"{name}\" is already declared by tag#{}",
                    existing.0
                )
            }
            Self::Contract(error) => error.fmt(f),
        }
    }
}

impl core::error::Error for TagBuildError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Contract(error) => Some(error),
            Self::DuplicateName { .. } => None,
        }
    }
}

impl ProgramBuilder {
    /// Declares a nominal exception tag carrying `payload`, in order.
    ///
    /// Every declaration is a distinct tag, even when another tag has the same payload
    /// contracts. Names must be unique within the program; they identify the tag in verifier
    /// diagnostics and disassembly. Payload positions are [`ContractRef`]s, as in
    /// [`FunctionAbi`]: plain kinds convert with `.into()`, and rich contracts use their IDs.
    ///
    /// ```
    /// use execution_tape::{asm::ProgramBuilder, program::ValueType};
    /// let mut program = ProgramBuilder::new();
    /// let failed = program.declare_tag("division-by-zero", &[ValueType::I64.into()])?;
    /// let aborted = program.declare_tag("abort", &[])?;
    /// assert_ne!(failed, aborted);
    /// # Ok::<(), execution_tape::asm::TagBuildError>(())
    /// ```
    pub fn declare_tag(
        &mut self,
        name: &str,
        payload: &[ContractRef],
    ) -> Result<TagId, TagBuildError> {
        let abi = FunctionAbi {
            args: payload.to_vec(),
            rets: Vec::new(),
        };
        if let Some(index) = self
            .tags
            .iter()
            .position(|tag| self.symbols[tag.name.index() as usize - 1].symbol == name)
        {
            return Err(TagBuildError::DuplicateName {
                name: name.into(),
                existing: TagId(u32::try_from(index).unwrap_or(u32::MAX)),
            });
        }
        let (payload, _) = self
            .resolve_signature(abi)
            .map_err(TagBuildError::Contract)?;
        let name = self.symbol(name);
        let id = TagId(u32::try_from(self.tags.len()).unwrap_or(u32::MAX));
        self.tags.push(TagDef { name, payload });
        Ok(id)
    }
}
