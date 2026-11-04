// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Checked control boundaries retain the exact owners of both sides of a failed contract.

use super::{ContractList, ContractOwner};
use crate::{
    contract::{Contract, ContractId},
    program::Program,
};
use core::fmt;

/// The control operation whose contracts are being checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlBoundary {
    /// Callable arguments/results at a proper terminal call.
    TailCall,
    /// Resume arguments/results at a proper terminal resumption.
    TailResume,
    /// Original callable inputs and final results at continuation creation.
    Create,
    /// Inputs at a particular suspension and the computation's final results.
    Resume,
    /// A control operation's request and response values.
    Suspend,
    /// An installed handler's operation or callable protocol.
    Handler,
}
impl fmt::Display for ControlBoundary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TailCall => "tail call",
            Self::TailResume => "tail resumption",
            Self::Create => "continuation creation",
            Self::Resume => "resumption",
            Self::Suspend => "suspension",
            Self::Handler => "handler",
        })
    }
}

/// Exact contracts on both sides of a rejected control boundary.
///
/// Input and output lists can belong to different definitions. The error retains each owner;
/// local contract IDs are never interpreted through whichever program happened to initiate it.
#[derive(Clone)]
pub struct SignatureMismatch {
    pub(crate) boundary: ControlBoundary,
    pub(crate) expected_inputs: ContractOwner,
    pub(crate) expected_outputs: ContractOwner,
    pub(crate) actual_inputs: ContractOwner,
    pub(crate) actual_outputs: ContractOwner,
}
impl SignatureMismatch {
    /// Operation whose protocol differs.
    #[must_use]
    pub fn boundary(&self) -> ControlBoundary {
        self.boundary
    }
    /// Required input contracts and their exact definition.
    #[must_use]
    pub fn expected_inputs(&self) -> ContractList<'_> {
        self.expected_inputs.view()
    }
    /// Required output contracts and their exact definition.
    #[must_use]
    pub fn expected_outputs(&self) -> ContractList<'_> {
        self.expected_outputs.view()
    }
    /// Offered input contracts and their exact definition.
    #[must_use]
    pub fn actual_inputs(&self) -> ContractList<'_> {
        self.actual_inputs.view()
    }
    /// Offered output contracts and their exact definition.
    #[must_use]
    pub fn actual_outputs(&self) -> ContractList<'_> {
        self.actual_outputs.view()
    }
}
impl fmt::Display for SignatureMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} contracts differ: expected {} → {}, found {} → {}",
            self.boundary,
            self.expected_inputs.view(),
            self.expected_outputs.view(),
            self.actual_inputs.view(),
            self.actual_outputs.view()
        )
    }
}
impl fmt::Debug for SignatureMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
impl core::error::Error for SignatureMismatch {}
impl PartialEq for SignatureMismatch {
    fn eq(&self, other: &Self) -> bool {
        fn same(a: &ContractOwner, b: &ContractOwner) -> bool {
            a.range == b.range && a.program.shares_definition_with(&b.program)
        }
        self.boundary == other.boundary
            && same(&self.expected_inputs, &other.expected_inputs)
            && same(&self.expected_outputs, &other.expected_outputs)
            && same(&self.actual_inputs, &other.actual_inputs)
            && same(&self.actual_outputs, &other.actual_outputs)
    }
}
impl Eq for SignatureMismatch {}

impl fmt::Display for ContractList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        list(f, self.program, self.ids, 2)
    }
}
fn list(
    f: &mut fmt::Formatter<'_>,
    program: &Program,
    ids: &[ContractId],
    depth: u8,
) -> fmt::Result {
    f.write_str("(")?;
    for (index, &id) in ids.iter().take(4).enumerate() {
        if index != 0 {
            f.write_str(", ")?;
        }
        contract(f, program, id, depth)?;
    }
    if ids.len() > 4 {
        f.write_str(", …")?;
    }
    f.write_str(")")
}
fn contract(
    f: &mut fmt::Formatter<'_>,
    program: &Program,
    id: ContractId,
    depth: u8,
) -> fmt::Result {
    let Some(value) = program.contracts.get(id) else {
        return write!(f, "<invalid contract #{}>", id.0);
    };
    if let Contract::Kind(kind) = value {
        return write!(f, "{}", crate::contract::KindName(kind));
    }
    if depth == 0 {
        return write!(f, "{}(…)", crate::contract::KindName(value.kind()));
    }
    match value {
        Contract::Kind(_) => unreachable!("kind handled above"),
        Contract::Tuple(members) => {
            f.write_str("tuple(")?;
            for (index, id) in members.iter().take(4).enumerate() {
                if index != 0 {
                    f.write_str(", ")?;
                }
                if let Some(id) = id {
                    contract(f, program, *id, depth - 1)?;
                } else {
                    f.write_str("?")?;
                }
            }
            if members.len() > 4 {
                f.write_str(", …")?;
            }
            f.write_str(")")
        }
        Contract::Struct { type_id, fields } => {
            write!(f, "struct#{}", type_id.0)?;
            list(f, program, fields, depth - 1)
        }
        Contract::Array {
            elem_type_id,
            element,
        } => {
            write!(f, "array#{}<", elem_type_id.0)?;
            contract(f, program, element, depth - 1)?;
            f.write_str(">")
        }
        Contract::Function { args, rets }
        | Contract::Closure { args, rets }
        | Contract::Tag {
            request: args,
            response: rets,
        }
        | Contract::Continuation {
            parameters: args,
            results: rets,
        } => {
            write!(f, "{}", crate::contract::KindName(value.kind()))?;
            list(f, program, args, depth - 1)?;
            f.write_str("→")?;
            list(f, program, rets, depth - 1)
        }
    }
}
