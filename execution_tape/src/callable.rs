// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Executable functions and closures with explicit code and capture ownership.
//!
//! [`Callable`] retains one exact prepared program and, for a closure, an independent immutable
//! capture snapshot. It survives the original VM, its frames and later lookup changes. Host object
//! tokens remain host-owned; retaining a callable does not retain external objects or their state.
//! Class/selector lookup and publication policy belong to the embedding language.

mod publication;
pub use publication::{
    CodeSlot, CodeUpdate, CodeUpdateError, CodeVersion, SignatureDifference, SignaturePath,
};

use alloc::sync::Arc;
use core::fmt;

use crate::{
    aggregates::{AggError, AggHeap, ValueSnapshot},
    contract::{ContractId, ContractKinds},
    host::ValueRef,
    program::{Program, ValueType},
    value::{AggHandle, FuncId, Value},
    verifier::VerifiedProgram,
};

/// Why an executable callable could not be retained.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallableError {
    /// The invocation has no successful outputs to retain.
    NoSuccessfulResult,
    /// The requested successful result slot is absent.
    OutputIndex {
        /// Requested result position.
        index: usize,
        /// Number of successful result values.
        count: usize,
    },
    /// Raw `Vm::start` borrowed its program without retaining an owning handle. Start a
    /// `Callable::function` instead when returned callables need independent code ownership.
    MissingCodeOwner,
    /// The selected value is neither a function nor a closure.
    NotCallable {
        /// Observed storage kind.
        actual: ValueType,
    },
    /// A snapshot must contain exactly the one callable being retained.
    RootCount {
        /// Number of supplied snapshot roots.
        actual: usize,
    },
    /// The target is absent from the supplied prepared program.
    MissingFunction {
        /// Program-local target.
        function: FuncId,
    },
    /// A closure body must accept an aggregate environment as its first argument.
    EnvironmentArgument {
        /// Selected body.
        function: FuncId,
        /// First argument kind, or `None` when the body has no arguments.
        actual: Option<ValueType>,
    },
    /// Captured aggregate storage could not be retained.
    Aggregate(AggError),
    /// Logical capture-copy work cannot be represented by the VM's counters.
    CaptureSizeOverflow,
}
impl fmt::Display for CallableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuccessfulResult => f.write_str("callable retention requires a successful invocation result"),
            Self::OutputIndex { index, count } => write!(f, "cannot retain callable output {index}: invocation returned {count} values"),
            Self::MissingCodeOwner => f.write_str("invocation has no retained code owner; start through Callable::function(program, entry) to retain callable results"),
            Self::NotCallable { actual } => {
                write!(f, "expected a function or closure, got {actual:?}")
            }
            Self::RootCount { actual } => {
                write!(f, "expected one callable snapshot root, got {actual}")
            }
            Self::MissingFunction { function } => write!(
                f,
                "callable target f{} is absent from its prepared program",
                function.0
            ),
            Self::EnvironmentArgument { function, actual } => write!(
                f,
                "closure body f{} must begin with an aggregate environment argument, got {actual:?}",
                function.0
            ),
            Self::Aggregate(error) => write!(f, "cannot retain closure environment: {error}"),
            Self::CaptureSizeOverflow => {
                f.write_str("closure capture exceeds logical work-counter capacity")
            }
        }
    }
}
impl core::error::Error for CallableError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Aggregate(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone)]
enum Target {
    Function(FuncId),
    Closure(Arc<Capture>),
}

pub(crate) struct Capture {
    pub(crate) snapshot: ValueSnapshot,
    pub(crate) function: FuncId,
    pub(crate) environment: AggHandle,
    pub(crate) aggregate_work: u64,
    pub(crate) payload_work: u64,
}

/// A function or closure retaining the code and immutable captures needed to execute it.
///
/// Cloning shares code/captures; it does not copy an environment. Function IDs are interpreted
/// only in this exact prepared program. Captures use the core's ordinary [`ValueSnapshot`]
/// representation rather than borrowing a VM heap. Source snapshots can be bound to
/// [`Self::program`] in the same way as any verified program.
///
/// [`Self::start`] creates the ordinary resumable invocation with this target. A closure's hidden
/// environment is imported into that invocation's heap transaction. Existing invocations and
/// retained callables keep their original code even if an embedding replaces its lookup entry.
/// This does not implement cross-program guest calls or transfer program-local IDs between code
/// revisions. Host objects, mutable cells and their collection remain embedding responsibilities.
///
/// Equality compares code-owner/target identity for functions and shared retained-capture identity
/// for closures. Clones compare equal; independently retained closure snapshots remain distinct.
#[derive(Clone)]
pub struct Callable {
    program: Arc<VerifiedProgram>,
    target: Target,
}
impl fmt::Debug for Callable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Callable")
            .field("program", &self.program.program().name())
            .field("function", &self.function_id())
            .field("closure", &self.capture().is_some())
            .field(
                "capture_nodes",
                &self
                    .capture()
                    .map_or(0, |c| c.snapshot.aggregates().len_u32()),
            )
            .finish_non_exhaustive()
    }
}
impl PartialEq for Callable {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.program, &other.program)
            && match (&self.target, &other.target) {
                (Target::Function(a), Target::Function(b)) => a == b,
                (Target::Closure(a), Target::Closure(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }
}
impl Eq for Callable {}

/// Borrowed caller-visible contracts for a retained callable.
///
/// The closure's environment and the implicit effect token are excluded. Contract IDs belong
/// to [`Self::program`]; numeric IDs from different programs cannot be compared as identities.
#[derive(Clone, Copy)]
pub struct CallableSignature<'a> {
    program: &'a Program,
    args: &'a [ContractId],
    rets: &'a [ContractId],
}
impl fmt::Debug for CallableSignature<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallableSignature")
            .field("args", &self.arg_types())
            .field("rets", &self.ret_types())
            .finish_non_exhaustive()
    }
}
impl<'a> CallableSignature<'a> {
    /// Program whose contract table interprets this signature.
    #[must_use]
    pub fn program(self) -> &'a Program {
        self.program
    }
    /// Caller-visible argument contracts, excluding the captured environment.
    #[must_use]
    pub fn arg_contracts(self) -> &'a [ContractId] {
        self.args
    }
    /// Result contracts in return order.
    #[must_use]
    pub fn ret_contracts(self) -> &'a [ContractId] {
        self.rets
    }
    /// Caller-visible argument storage kinds.
    #[must_use]
    pub fn arg_types(self) -> ContractKinds<'a> {
        self.program.contracts.kinds(self.args)
    }
    /// Result storage kinds.
    #[must_use]
    pub fn ret_types(self) -> ContractKinds<'a> {
        self.program.contracts.kinds(self.rets)
    }
}

impl Callable {
    /// Retains a verified function's exact code owner without capturing a VM heap.
    pub fn function(
        program: Arc<VerifiedProgram>,
        function: FuncId,
    ) -> Result<Self, CallableError> {
        if program.function(function).is_none() {
            return Err(CallableError::MissingFunction { function });
        }
        Ok(Self {
            program,
            target: Target::Function(function),
        })
    }

    /// Retains a raw function or closure using its declared original program and source heap.
    ///
    /// This copies a closure's reachable immutable environment, preserving sharing and distinct
    /// nodes within it. It never retains other heap nodes. The caller must supply the actual
    /// original program: attaching a different program does not translate local function/type IDs.
    /// Retention is explicit preparation outside guest fuel/work limits; start-time copies are
    /// separately charged before import. Concrete environment contracts are checked on start.
    pub fn retain(
        program: Arc<VerifiedProgram>,
        heap: &AggHeap,
        value: ValueRef<'_>,
    ) -> Result<Self, CallableError> {
        match value {
            ValueRef::Func(function) => Self::function(program, function),
            ValueRef::Closure(closure) => Self::from_snapshot(
                program,
                heap.snapshot(&[Value::Closure(closure)])
                    .map_err(CallableError::Aggregate)?,
            ),
            other => Err(CallableError::NotCallable {
                actual: other.value_type(),
            }),
        }
    }

    /// Adopts exactly one retained function/closure root, without recopying its environment.
    ///
    /// Supply its actual prepared program. This declares the identity domain of all captured
    /// function/nominal type IDs; it does not relocate them to another program. Invalid target
    /// IDs and bodies without an environment argument are rejected. Value contracts are checked
    /// when execution starts, with that invocation's normal contract-work budget.
    pub fn from_snapshot(
        program: Arc<VerifiedProgram>,
        snapshot: ValueSnapshot,
    ) -> Result<Self, CallableError> {
        let [value] = snapshot.values() else {
            return Err(CallableError::RootCount {
                actual: snapshot.values().len(),
            });
        };
        let closure = match value {
            Value::Func(function) => return Self::function(program, *function),
            Value::Closure(closure) => *closure,
            other => {
                return Err(CallableError::NotCallable {
                    actual: other.value_type(),
                });
            }
        };
        let body = program
            .function(closure.func)
            .ok_or(CallableError::MissingFunction {
                function: closure.func,
            })?;
        let actual = body.arg_types().get(0).copied();
        if actual != Some(ValueType::Agg) {
            return Err(CallableError::EnvironmentArgument {
                function: closure.func,
                actual,
            });
        }
        let mut aggregate_work = 0_u64;
        let mut payload_work = 0_u64;
        for fields in snapshot.aggregates().node_values() {
            aggregate_work = aggregate_work
                .checked_add(1)
                .and_then(|n| n.checked_add(u64::try_from(fields.len()).ok()?))
                .ok_or(CallableError::CaptureSizeOverflow)?;
            for value in fields {
                let bytes = match value {
                    Value::Str(s) => s.len(),
                    Value::Bytes(b) => b.len(),
                    _ => 0,
                };
                payload_work = payload_work
                    .checked_add(
                        u64::try_from(bytes).map_err(|_| CallableError::CaptureSizeOverflow)?,
                    )
                    .ok_or(CallableError::CaptureSizeOverflow)?;
            }
        }
        Ok(Self {
            program,
            target: Target::Closure(Arc::new(Capture {
                snapshot,
                function: closure.func,
                environment: closure.env,
                aggregate_work,
                payload_work,
            })),
        })
    }

    /// Exact code owner retained by this callable, including after lookup replacement.
    #[must_use]
    pub fn program(&self) -> &Arc<VerifiedProgram> {
        &self.program
    }
    /// Program-local body identifier. A closure body includes its hidden environment argument.
    #[must_use]
    pub fn function_id(&self) -> FuncId {
        match &self.target {
            Target::Function(function) => *function,
            Target::Closure(capture) => capture.function,
        }
    }
    /// Caller-visible contracts; a closure's environment remains hidden.
    #[must_use]
    pub fn signature(&self) -> CallableSignature<'_> {
        let body = self
            .program
            .function(self.function_id())
            .expect("retained target");
        let args = body.arg_contracts();
        CallableSignature {
            program: self.program.program(),
            args: if self.capture().is_some() {
                &args[1..]
            } else {
                args
            },
            rets: body.ret_contracts(),
        }
    }
    /// Captured environment handle and its owning heap; `None` for a plain function.
    /// Handles returned here belong to this immutable capture heap, not any running VM.
    #[must_use]
    pub fn environment(&self) -> Option<(AggHandle, &AggHeap)> {
        self.capture()
            .map(|capture| (capture.environment, capture.snapshot.aggregates()))
    }
    /// Logical work to import the immutable captures once, without performing the import.
    /// Plain functions cost zero. User arguments move into the invocation without cloning their
    /// byte/string payloads; this reports only the retained environment's copy debit.
    #[must_use]
    pub fn capture_work(&self) -> crate::vm::DataWork {
        self.capture()
            .map_or_else(crate::vm::DataWork::default, |capture| {
                crate::vm::DataWork {
                    aggregate: capture.aggregate_work,
                    payload: capture.payload_work,
                }
            })
    }
    /// Visits opaque object tokens retained in the captured graph. Does not retain or collect
    /// those external objects; embeddings add these references to their own roots.
    pub fn visit_objects(&self, mut visit: impl FnMut(crate::value::Obj)) {
        if let Some(capture) = self.capture() {
            for (_, _, object) in capture.snapshot.aggregates().object_references() {
                visit(object);
            }
        }
    }
    pub(crate) fn capture(&self) -> Option<&Capture> {
        match &self.target {
            Target::Function(_) => None,
            Target::Closure(capture) => Some(capture),
        }
    }
}
