// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Exception tag declarations and signature-aware `throw` emission.

use super::*;

/// An exception tag could not be declared or thrown. The builder and assembler are unchanged.
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
    /// The tag has no declaration in this builder.
    UnknownTag(TagId),
    /// The number of payload registers disagrees with the declaration.
    Payload {
        /// Thrown tag.
        tag: TagId,
        /// Declared payload arity.
        expected: usize,
        /// Supplied payload register count.
        actual: usize,
    },
    /// A payload register is not declared.
    Register(u32),
    /// A payload register's declared kind disagrees with the tag.
    PayloadKind {
        /// Thrown tag.
        tag: TagId,
        /// Offending source register.
        register: u32,
        /// Zero-based payload position.
        index: usize,
        /// Kind declared by the tag.
        expected: ValueType,
        /// Register's declared kind.
        actual: ValueType,
    },
    /// r0 must be declared as Unit; start with [`ProgramBuilder::function_asm`].
    EffectRegister,
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
            Self::UnknownTag(tag) => write!(f, "tag#{} has no declaration", tag.0),
            Self::Payload {
                tag,
                expected,
                actual,
            } => write!(
                f,
                "throw tag#{} expects {expected} payload values, got {actual}",
                tag.0
            ),
            Self::Register(register) => write!(f, "throw uses undeclared register r{register}"),
            Self::PayloadKind {
                tag,
                register,
                index,
                expected,
                actual,
            } => write!(
                f,
                "throw tag#{}, payload {index} at r{register} requires {expected:?}, register declares {actual:?}",
                tag.0
            ),
            Self::EffectRegister => f.write_str(
                "throw requires r0 declared as Unit; start with ProgramBuilder::function_asm",
            ),
        }
    }
}

impl core::error::Error for TagBuildError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Contract(error) => Some(error),
            _ => None,
        }
    }
}

impl ProgramBuilder {
    /// Declares a nominal exception tag carrying `payload`, in order.
    ///
    /// Every declaration is a distinct tag, even when another tag has the same payload
    /// contracts. Names must be unique within the program; they identify the tag in verifier
    /// diagnostics, disassembly and uncaught-exception traps. Payload positions are
    /// [`ContractRef`]s, as in [`FunctionAbi`]: plain kinds convert with `.into()`, and rich
    /// contracts use their IDs.
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
        self.add_tag(name, payload, &[], false)
    }

    /// Declares a tag this program imports by `name` instead of defining.
    ///
    /// Installing the program binds it to the VM tag defined under that name (see
    /// [`crate::vm::Vm::define_tag`]), which must carry exactly these payload contracts. Units
    /// importing one tag throw and catch the same exception. The returned ID is used like any
    /// declared tag.
    pub fn import_tag(
        &mut self,
        name: &str,
        payload: &[ContractRef],
    ) -> Result<TagId, TagBuildError> {
        self.add_tag(name, payload, &[], true)
    }

    /// Declares a tag that a suspension can be resumed from with `results`.
    ///
    /// `suspend` raises the tag with `payload`; the handler resumes the suspended continuation
    /// with `results`, which become the `suspend` instruction's results. Such a tag can also be
    /// thrown, which ignores its results. See [`Self::declare_tag`] for naming rules.
    pub fn declare_tag_with_results(
        &mut self,
        name: &str,
        payload: &[ContractRef],
        results: &[ContractRef],
    ) -> Result<TagId, TagBuildError> {
        self.add_tag(name, payload, results, false)
    }

    /// Imports a tag with resumption results, as [`Self::import_tag`] imports an exception tag.
    ///
    /// The VM tag defined under `name` must declare exactly this payload and these results.
    pub fn import_tag_with_results(
        &mut self,
        name: &str,
        payload: &[ContractRef],
        results: &[ContractRef],
    ) -> Result<TagId, TagBuildError> {
        self.add_tag(name, payload, results, true)
    }

    fn add_tag(
        &mut self,
        name: &str,
        payload: &[ContractRef],
        results: &[ContractRef],
        import: bool,
    ) -> Result<TagId, TagBuildError> {
        let abi = FunctionAbi {
            args: payload.to_vec(),
            rets: results.to_vec(),
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
        let (payload, results) = self
            .resolve_signature(abi)
            .map_err(TagBuildError::Contract)?;
        let name = self.symbol(name);
        let id = TagId(u32::try_from(self.tags.len()).unwrap_or(u32::MAX));
        self.tags.push(TagDef {
            name,
            payload,
            results,
            import,
        });
        Ok(id)
    }

    /// Emits `throw`, checking the payload registers against the tag's declaration first.
    ///
    /// This checks the tag, arity, declared register kinds and r0. Initialization and rich
    /// payload contracts still require [`Self::build_verified`]; refined contracts that cannot
    /// be proven statically are checked when the exception is raised.
    ///
    /// ```
    /// use execution_tape::{asm::{FunctionSig, ProgramBuilder}, program::ValueType};
    /// let mut program = ProgramBuilder::new();
    /// let failed = program.declare_tag("failed", &[ValueType::I64.into()])?;
    /// let signature = FunctionSig { arg_types: vec![ValueType::I64], ret_types: vec![] };
    /// let function = program.declare_function(signature)?;
    /// let mut body = program.function_asm(function)?;
    /// let code = program.argument(function, 0)?;
    /// program.emit_throw(&mut body, failed, &[code])?;
    /// program.define_function(function, body)?;
    /// program.build_verified()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn emit_throw(
        &self,
        body: &mut Asm,
        tag: TagId,
        payload: &[u32],
    ) -> Result<(), TagBuildError> {
        let declaration = self
            .tags
            .get(tag.0 as usize)
            .ok_or(TagBuildError::UnknownTag(tag))?;
        if payload.len() != declaration.payload.len() {
            return Err(TagBuildError::Payload {
                tag,
                expected: declaration.payload.len(),
                actual: payload.len(),
            });
        }
        if body.register_types.first() != Some(&ValueType::Unit) {
            return Err(TagBuildError::EffectRegister);
        }
        for (index, (&register, &contract)) in payload.iter().zip(&declaration.payload).enumerate()
        {
            let actual = *body
                .register_types
                .get(register as usize)
                .ok_or(TagBuildError::Register(register))?;
            let expected = self
                .contracts
                .table()
                .kind(contract)
                .expect("declared contract");
            if actual != expected {
                return Err(TagBuildError::PayloadKind {
                    tag,
                    register,
                    index,
                    expected,
                    actual,
                });
            }
        }
        body.throw(0, tag, payload);
        Ok(())
    }
}
