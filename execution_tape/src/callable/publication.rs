// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Replaceable callable bindings with immutable publication snapshots.
//!
//! A slot owns one current publication. A retained version is independent of later updates;
//! executing it pins that callable's exact code and captures. Language method/class lookup,
//! batches of edits, synchronization and object migration remain embedding responsibilities.

use crate::{
    callable::{Callable, CallableSignature},
    contract::{Contract, ContractId},
    program::Program,
};
use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};
use core::{
    fmt,
    hash::{Hash, Hasher},
};

/// One immutable publication of a replaceable callable slot.
///
/// Clones share identity and storage. Revisions are local to one slot; `Eq`/`Hash` compare the
/// actual publication identity, not its display number. Holding this handle keeps code and
/// captures alive. It is an in-process identity, not a portable code/archive identifier.
#[derive(Clone)]
pub struct CodeVersion(Arc<Publication>);
struct Publication {
    slot: Arc<()>,
    revision: u64,
    callable: Callable,
}
impl CodeVersion {
    /// Slot-local revision number, starting at zero. Use handle equality for identity.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.0.revision
    }
    /// Executable target retained by this exact publication.
    #[must_use]
    pub fn callable(&self) -> &Callable {
        &self.0.callable
    }
    /// Whether two publications belong to the same slot, even at different revisions.
    #[must_use]
    pub fn same_slot(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0.slot, &other.0.slot)
    }
}
impl PartialEq for CodeVersion {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for CodeVersion {}
impl Hash for CodeVersion {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}
impl fmt::Debug for CodeVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodeVersion")
            .field("revision", &self.revision())
            .field("callable", self.callable())
            .finish_non_exhaustive()
    }
}

/// A source position inside a caller-visible signature contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignaturePath {
    /// Argument of the root or a nested callable signature.
    Argument(usize),
    /// Result of the root or a nested callable signature.
    Result(usize),
    /// Tuple member or nominal struct field.
    Member(usize),
    /// Element contract of a nominal array.
    Element,
}

/// First exact-signature disagreement found before a publication is changed.
///
/// Contract IDs belong to the current and proposed code owners respectively. `None` denotes
/// an unconstrained tuple member. No variance/subtyping or program-local ID translation is
/// inferred. Host object type IDs must keep their embedding-defined ABI meaning across edits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignatureDifference {
    /// The root or a nested callable has a different number of user arguments.
    ArgumentCount {
        /// Location of that callable signature; empty for the root.
        path: Vec<SignaturePath>,
        /// Current argument count.
        expected: usize,
        /// Proposed argument count.
        proposed: usize,
    },
    /// The root or a nested callable has a different result count.
    ResultCount {
        /// Location of that callable signature; empty for the root.
        path: Vec<SignaturePath>,
        /// Current result count.
        expected: usize,
        /// Proposed result count.
        proposed: usize,
    },
    /// Tuple/field contracts have different lengths.
    MemberCount {
        /// Location of the tuple/struct contract.
        path: Vec<SignaturePath>,
        /// Current member count.
        expected: usize,
        /// Proposed member count.
        proposed: usize,
    },
    /// Kinds, shapes, member guarantees or same-program nominal IDs differ.
    Contract {
        /// Location of the disagreeing contracts.
        path: Vec<SignaturePath>,
        /// Current contract ID, or an unconstrained tuple member.
        expected: Option<ContractId>,
        /// Proposed contract ID, or an unconstrained tuple member.
        proposed: Option<ContractId>,
    },
    /// Nominal struct/array identity belongs to different prepared programs.
    ProgramIdentity {
        /// Location of the nominal contracts.
        path: Vec<SignaturePath>,
        /// Current program-local nominal contract.
        expected: ContractId,
        /// Proposed program-local nominal contract.
        proposed: ContractId,
    },
}

/// Why a proposed publication was rejected. The slot remains unchanged.
#[derive(Clone, Debug)]
pub enum CodeUpdateError {
    /// The proposed target changes the exact exposed contract.
    Signature {
        /// Still-current immutable publication.
        current: CodeVersion,
        /// Rejected target, retained for inspection or recovery.
        proposed: Callable,
        /// Structured position and disagreement in the two owners' contract tables.
        difference: SignatureDifference,
    },
    /// The slot cannot allocate another local revision number.
    RevisionExhausted {
        /// Still-current immutable publication.
        current: CodeVersion,
        /// Rejected target.
        proposed: Callable,
    },
}
impl core::error::Error for CodeUpdateError {}

/// Result of a publication request, including an explicit no-op.
#[derive(Clone, Debug)]
pub enum CodeUpdate {
    /// The exact callable identity was already current; no revision was allocated.
    Unchanged(CodeVersion),
    /// A compatible target was published. Retaining this report also retains the old code.
    Replaced {
        /// Previous publication, still independently executable.
        previous: CodeVersion,
        /// New current publication.
        current: CodeVersion,
    },
}

/// One replaceable callable binding with immutable retained versions.
///
/// `current()` returns an owned snapshot so a paused call does not borrow the mutable slot.
/// `replace()` requires exact caller-visible contract compatibility and publishes only after
/// comparison succeeds. Concrete capture/input checks still happen on callable start.
///
/// This is a publication primitive, not a method dictionary or cross-program call mechanism.
/// Embeddings choose names/class lookup and synchronization. Existing guest calls remain within
/// their immutable program. Updating a slot affects future lookups; it never patches an active
/// frame or an escaped closure. Nominal struct/array identities from different preparations
/// cannot be equated by matching their numeric IDs. Compatible signatures do not translate raw
/// argument values between program/heap ownership domains.
/// Updating a slot does not invalidate application/graph caches; the embedding owns that policy.
///
/// ```no_run
/// # use execution_tape::{callable::{Callable, CodeSlot}, host::Host,
/// #     vm::{InvocationOptions, RunStatus, Vm}};
/// # fn update<H: Host>(slot: &mut CodeSlot, replacement: Callable, vm: &mut Vm<H>)
/// #     -> Result<(), Box<dyn std::error::Error>> {
/// let mut old_call = slot.current().callable().start(vm, vec![], InvocationOptions::default())?;
/// old_call.resume(1)?;
/// slot.replace(replacement)?;
/// // Future lookups use the new publication; this active call owns its old code.
/// while old_call.resume(1024)? == RunStatus::Suspended {}
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct CodeSlot {
    current: CodeVersion,
}
impl CodeSlot {
    /// Creates a new independent slot at revision zero.
    #[must_use]
    pub fn new(callable: Callable) -> Self {
        Self {
            current: CodeVersion(Arc::new(Publication {
                slot: Arc::new(()),
                revision: 0,
                callable,
            })),
        }
    }
    /// Retains the current publication without borrowing the mutable slot across execution.
    #[must_use]
    pub fn current(&self) -> CodeVersion {
        self.current.clone()
    }
    /// Publishes a compatible callable, explicitly reporting replacement or unchanged identity.
    ///
    /// Comparison is iterative and memoized over contract pairs; signature graphs are not
    /// expanded into trees. This preparation work is outside guest execution limits. Failure
    /// returns both code owners and structured disagreement without changing publication.
    pub fn replace(&mut self, proposed: Callable) -> Result<CodeUpdate, CodeUpdateError> {
        if self.current.callable() == &proposed {
            return Ok(CodeUpdate::Unchanged(self.current()));
        }
        if let Err(difference) = compare(self.current.callable(), &proposed) {
            return Err(CodeUpdateError::Signature {
                current: self.current(),
                proposed,
                difference,
            });
        }
        let Some(revision) = self.current.revision().checked_add(1) else {
            return Err(CodeUpdateError::RevisionExhausted {
                current: self.current(),
                proposed,
            });
        };
        let current = CodeVersion(Arc::new(Publication {
            slot: Arc::clone(&self.current.0.slot),
            revision,
            callable: proposed,
        }));
        let previous = core::mem::replace(&mut self.current, current.clone());
        Ok(CodeUpdate::Replaced { previous, current })
    }
}

struct Link {
    parent: Option<usize>,
    step: SignaturePath,
}
struct Pair {
    current: Option<ContractId>,
    proposed: Option<ContractId>,
    path: usize,
}
struct Comparison<'a> {
    current: &'a Program,
    proposed: &'a Program,
    same_program: bool,
    links: Vec<Link>,
    pending: Vec<Pair>,
    visited: BTreeSet<(ContractId, ContractId)>,
}
impl Comparison<'_> {
    fn path(&self, mut index: Option<usize>) -> Vec<SignaturePath> {
        let mut path = Vec::new();
        while let Some(at) = index {
            path.push(self.links[at].step);
            index = self.links[at].parent;
        }
        path.reverse();
        path
    }
    fn push(
        &mut self,
        current: Option<ContractId>,
        proposed: Option<ContractId>,
        parent: Option<usize>,
        step: SignaturePath,
    ) {
        let path = self.links.len();
        self.links.push(Link { parent, step });
        self.pending.push(Pair {
            current,
            proposed,
            path,
        });
    }
    fn lists(
        &mut self,
        args: &[ContractId],
        rets: &[ContractId],
        other_args: &[ContractId],
        other_rets: &[ContractId],
        path: Option<usize>,
    ) -> Result<(), SignatureDifference> {
        if args.len() != other_args.len() {
            return Err(SignatureDifference::ArgumentCount {
                path: self.path(path),
                expected: args.len(),
                proposed: other_args.len(),
            });
        }
        if rets.len() != other_rets.len() {
            return Err(SignatureDifference::ResultCount {
                path: self.path(path),
                expected: rets.len(),
                proposed: other_rets.len(),
            });
        }
        for (index, (&a, &b)) in rets.iter().zip(other_rets).enumerate().rev() {
            self.push(Some(a), Some(b), path, SignaturePath::Result(index));
        }
        for (index, (&a, &b)) in args.iter().zip(other_args).enumerate().rev() {
            self.push(Some(a), Some(b), path, SignaturePath::Argument(index));
        }
        Ok(())
    }
    fn different(&self, pair: &Pair) -> SignatureDifference {
        SignatureDifference::Contract {
            path: self.path(Some(pair.path)),
            expected: pair.current,
            proposed: pair.proposed,
        }
    }
    fn check(
        mut self,
        current: CallableSignature<'_>,
        proposed: CallableSignature<'_>,
    ) -> Result<(), SignatureDifference> {
        self.lists(
            current.arg_contracts(),
            current.ret_contracts(),
            proposed.arg_contracts(),
            proposed.ret_contracts(),
            None,
        )?;
        while let Some(pair) = self.pending.pop() {
            let (Some(a), Some(b)) = (pair.current, pair.proposed) else {
                if pair.current != pair.proposed {
                    return Err(self.different(&pair));
                }
                continue;
            };
            if (self.same_program && a == b) || !self.visited.insert((a, b)) {
                continue;
            }
            let current = self.current.contracts.get(a).expect("verified contract");
            let proposed = self.proposed.contracts.get(b).expect("verified contract");
            match (current, proposed) {
                (Contract::Kind(a), Contract::Kind(b)) if a == b => {}
                (Contract::Tuple(a), Contract::Tuple(b)) => {
                    if a.len() != b.len() {
                        return Err(SignatureDifference::MemberCount {
                            path: self.path(Some(pair.path)),
                            expected: a.len(),
                            proposed: b.len(),
                        });
                    }
                    for (index, (&a, &b)) in a.iter().zip(b).enumerate().rev() {
                        self.push(a, b, Some(pair.path), SignaturePath::Member(index));
                    }
                }
                (
                    Contract::Function { args, rets },
                    Contract::Function {
                        args: b_args,
                        rets: b_rets,
                    },
                )
                | (
                    Contract::Closure { args, rets },
                    Contract::Closure {
                        args: b_args,
                        rets: b_rets,
                    },
                ) => {
                    self.lists(args, rets, b_args, b_rets, Some(pair.path))?;
                }
                (
                    Contract::Struct {
                        type_id: a_id,
                        fields: a_fields,
                    },
                    Contract::Struct {
                        type_id: b_id,
                        fields: b_fields,
                    },
                ) => {
                    if !self.same_program {
                        return Err(SignatureDifference::ProgramIdentity {
                            path: self.path(Some(pair.path)),
                            expected: a,
                            proposed: b,
                        });
                    }
                    if a_id != b_id {
                        return Err(self.different(&pair));
                    }
                    if a_fields.len() != b_fields.len() {
                        return Err(SignatureDifference::MemberCount {
                            path: self.path(Some(pair.path)),
                            expected: a_fields.len(),
                            proposed: b_fields.len(),
                        });
                    }
                    for (index, (&a, &b)) in a_fields.iter().zip(b_fields).enumerate().rev() {
                        self.push(
                            Some(a),
                            Some(b),
                            Some(pair.path),
                            SignaturePath::Member(index),
                        );
                    }
                }
                (
                    Contract::Array {
                        elem_type_id: a_id,
                        element: a_elem,
                    },
                    Contract::Array {
                        elem_type_id: b_id,
                        element: b_elem,
                    },
                ) => {
                    if !self.same_program {
                        return Err(SignatureDifference::ProgramIdentity {
                            path: self.path(Some(pair.path)),
                            expected: a,
                            proposed: b,
                        });
                    }
                    if a_id != b_id {
                        return Err(self.different(&pair));
                    }
                    self.push(
                        Some(a_elem),
                        Some(b_elem),
                        Some(pair.path),
                        SignaturePath::Element,
                    );
                }
                _ => return Err(self.different(&pair)),
            }
        }
        Ok(())
    }
}
fn compare(current: &Callable, proposed: &Callable) -> Result<(), SignatureDifference> {
    Comparison {
        current: current.program().program(),
        proposed: proposed.program().program(),
        same_program: Arc::ptr_eq(current.program(), proposed.program()),
        links: Vec::new(),
        pending: Vec::new(),
        visited: BTreeSet::new(),
    }
    .check(current.signature(), proposed.signature())
}

fn path_text(f: &mut fmt::Formatter<'_>, path: &[SignaturePath]) -> fmt::Result {
    if path.is_empty() {
        return f.write_str("signature");
    }
    for (index, part) in path.iter().enumerate() {
        match part {
            SignaturePath::Argument(n) => {
                write!(f, "{}argument[{n}]", if index == 0 { "" } else { "." })?;
            }
            SignaturePath::Result(n) => {
                write!(f, "{}result[{n}]", if index == 0 { "" } else { "." })?;
            }
            SignaturePath::Member(n) => write!(f, "[{n}]")?,
            SignaturePath::Element => f.write_str("[*]")?,
        }
    }
    Ok(())
}
fn summary(f: &mut fmt::Formatter<'_>, program: &Program, id: Option<ContractId>) -> fmt::Result {
    match id.and_then(|id| program.contracts.get(id)) {
        None => f.write_str("no member guarantee"),
        Some(Contract::Kind(kind)) => write!(f, "{kind:?}"),
        Some(Contract::Tuple(members)) => write!(f, "tuple with {} members", members.len()),
        Some(Contract::Struct { type_id, .. }) => write!(f, "struct #{}", type_id.0),
        Some(Contract::Array { elem_type_id, .. }) => write!(f, "array type #{}", elem_type_id.0),
        Some(Contract::Function { args, rets }) => write!(
            f,
            "function({} arguments, {} results)",
            args.len(),
            rets.len()
        ),
        Some(Contract::Closure { args, rets }) => write!(
            f,
            "closure({} arguments, {} results)",
            args.len(),
            rets.len()
        ),
    }
}
impl fmt::Display for CodeUpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self::Signature {
            current,
            proposed,
            difference,
        } = self
        else {
            return f.write_str("code slot revision counter exhausted; publication unchanged");
        };
        f.write_str("code replacement rejected at ")?;
        match difference {
            SignatureDifference::ArgumentCount {
                path,
                expected,
                proposed,
            }
            | SignatureDifference::ResultCount {
                path,
                expected,
                proposed,
            }
            | SignatureDifference::MemberCount {
                path,
                expected,
                proposed,
            } => {
                path_text(f, path)?;
                let kind = match difference {
                    SignatureDifference::ArgumentCount { .. } => "argument",
                    SignatureDifference::ResultCount { .. } => "result",
                    _ => "member",
                };
                write!(f, ": {kind} count expected {expected}, got {proposed}")
            }
            SignatureDifference::Contract {
                path,
                expected,
                proposed: other,
            } => {
                path_text(f, path)?;
                f.write_str(": expected ")?;
                summary(f, current.callable().program().program(), *expected)?;
                f.write_str(", got ")?;
                summary(f, proposed.program().program(), *other)
            }
            SignatureDifference::ProgramIdentity { path, .. } => {
                path_text(f, path)?;
                f.write_str(": nominal identity belongs to a different prepared program")
            }
        }?;
        f.write_str("; publication unchanged")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        asm::{FunctionSig, ProgramBuilder},
        program::ValueType,
    };
    use alloc::vec;
    fn constant(value: i64) -> Callable {
        let mut builder = ProgramBuilder::new();
        let function = builder
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let mut body = builder.function_asm(function).unwrap();
        let result = body.alloc_register(ValueType::I64);
        body.const_i64(result, value).ret(0, &[result]);
        builder.define_function(function, body).unwrap();
        Callable::function(Arc::new(builder.build_verified().unwrap()), function).unwrap()
    }
    #[test]
    fn revision_exhaustion_preserves_publication_and_still_allows_a_no_op() {
        let mut slot = CodeSlot::new(constant(1));
        Arc::get_mut(&mut slot.current.0).unwrap().revision = u64::MAX;
        let old = slot.current();
        assert!(matches!(
            slot.replace(constant(2)),
            Err(CodeUpdateError::RevisionExhausted { .. })
        ));
        assert_eq!(slot.current(), old);
        assert!(
            matches!(slot.replace(old.callable().clone()), Ok(CodeUpdate::Unchanged(version)) if version == old)
        );
    }
}
