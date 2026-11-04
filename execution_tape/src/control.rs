// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Typed control identities. Code instances own tags; an execution owns suspended computation.

pub(crate) mod arena;
mod error;
pub(crate) mod handler;
pub use error::{ControlBoundary, SignatureMismatch};
pub use handler::HandlerClause;

use crate::{
    contract::{ContractId, ContractKinds},
    instance::Instance,
    program::{ByteRange, CallSigId, ControlTagId, Program},
    verifier::VerifiedProgram,
};
use alloc::sync::Arc;
use core::{
    fmt,
    hash::{Hash, Hasher},
};

/// Contracts interpreted by one exact definition.
///
/// A continuation's response types and final result types may come from different definitions.
/// Keep the owner with each list rather than interpreting both in the currently running program.
#[derive(Clone, Copy)]
pub struct ContractList<'a> {
    program: &'a Program,
    ids: &'a [ContractId],
}
impl<'a> ContractList<'a> {
    pub(crate) fn new(program: &'a Program, ids: &'a [ContractId]) -> Self {
        Self { program, ids }
    }

    /// Exact definition whose contract table interprets [`Self::ids`].
    #[must_use]
    pub fn program(self) -> &'a Program {
        self.program
    }

    /// Contract IDs in parameter or result order.
    #[must_use]
    pub fn ids(self) -> &'a [ContractId] {
        self.ids
    }

    /// Storage kinds without materializing another list.
    #[must_use]
    pub fn kinds(self) -> ContractKinds<'a> {
        self.program.contracts.kinds(self.ids)
    }
}
impl fmt::Debug for ContractList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContractList")
            .field("program", &self.program.name())
            .field("ids", &self.ids)
            .finish()
    }
}

/// A control protocol in one exact instance, independently of its request/response signature.
///
/// Two tags with equal signatures remain different operations. Share this value explicitly
/// between independently compiled functions when they should use the same dynamic handlers.
#[derive(Clone)]
pub struct ControlTag {
    instance: Instance,
    id: ControlTagId,
    fresh: Option<Arc<()>>,
}
impl ControlTag {
    pub(crate) fn from_verified(instance: Instance, id: ControlTagId) -> Self {
        Self {
            instance,
            id,
            fresh: None,
        }
    }

    /// Creates a distinct operation with the same label and request/response contracts.
    ///
    /// Use a fresh tag for each lexical exit scope, so recursion and a later invocation cannot
    /// accidentally catch an earlier scope's exit. Cloning a tag preserves its identity; this
    /// method allocates a new identity. The originating store and code remain unchanged.
    #[must_use]
    pub fn fresh(&self) -> Self {
        Self {
            instance: self.instance.clone(),
            id: self.id,
            fresh: Some(Arc::new(())),
        }
    }

    pub(crate) fn identity(&self) -> (usize, u32, usize) {
        (
            Arc::as_ptr(&self.instance.0) as usize,
            self.id.0,
            self.fresh
                .as_ref()
                .map_or(0, |identity| Arc::as_ptr(identity) as usize),
        )
    }
    /// Whether this operation was created with a fresh lexical identity.
    #[must_use]
    pub fn is_fresh(&self) -> bool {
        self.fresh.is_some()
    }
    /// Resolves this operation in another namespace of the exact same verified definition.
    ///
    /// Declaration identity and any fresh lexical token survive. This method does not register
    /// code, import heap storage, or transfer host state. Use [`crate::vm::Vm::bind_instance`]
    /// to establish a destination store's persistent binding first.
    pub fn in_instance(&self, instance: &Instance) -> Result<Self, crate::instance::InstanceError> {
        if !instance
            .program()
            .shares_definition_with(self.instance.program())
        {
            return Err(crate::instance::InstanceError::DefinitionMismatch);
        }
        Ok(self.rebind(instance.clone()))
    }

    pub(crate) fn rebind(&self, instance: Instance) -> Self {
        debug_assert!(
            instance
                .program()
                .shares_definition_with(self.instance.program()),
            "tag declarations must be interpreted by their exact verified definition"
        );
        Self {
            instance,
            id: self.id,
            fresh: self.fresh.clone(),
        }
    }

    /// Exact code and host namespace defining the operation.
    #[must_use]
    pub fn instance(&self) -> &Instance {
        &self.instance
    }

    /// Local label/signature declaration. Fresh tags can share this declaration in the same
    /// instance; compare complete tags when matching operations.
    #[must_use]
    pub fn id(&self) -> ControlTagId {
        self.id
    }

    pub(crate) fn signature_id(&self) -> CallSigId {
        self.instance.program().program().control_tags[self.id.0 as usize].signature
    }

    /// Authored protocol label, independent of the operation's opaque identity.
    #[must_use]
    pub fn name(&self) -> &str {
        let program = self.instance.program().program();
        program
            .symbol_str(program.control_tags[self.id.0 as usize].name)
            .expect("verified tag label")
    }

    /// Values supplied by a suspension to its matching handler.
    #[must_use]
    pub fn request(&self) -> ContractList<'_> {
        let program = self.instance.program().program();
        let signature = program
            .call_sig(self.signature_id())
            .expect("verified tag signature");
        ContractList::new(program, program.verified_contracts(signature.args))
    }

    /// Values a suspended computation must receive when resumed normally.
    #[must_use]
    pub fn response(&self) -> ContractList<'_> {
        let program = self.instance.program().program();
        let signature = program
            .call_sig(self.signature_id())
            .expect("verified tag signature");
        ContractList::new(program, program.verified_contracts(signature.rets))
    }
}

impl PartialEq for ControlTag {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}
impl Eq for ControlTag {}
impl Hash for ControlTag {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.identity().hash(state);
    }
}
impl fmt::Debug for ControlTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlTag")
            .field("instance", &self.instance)
            .field("declaration", &self.id)
            .field("name", &self.name())
            .field("fresh_identity", &self.fresh.as_ref().map(Arc::as_ptr))
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct ContractOwner {
    pub(crate) program: VerifiedProgram,
    pub(crate) range: ByteRange,
}
impl ContractOwner {
    pub(crate) fn view(&self) -> ContractList<'_> {
        ContractList::new(
            self.program.program(),
            self.program.program().verified_contracts(self.range),
        )
    }
}

/// Identity of one suspension's authority, qualified by its originating execution.
///
/// Display ordinals may be reused with another revision. Compare the complete identity, not
/// its ordinal or revision alone. An identity supports inspection; it does not grant resumption
/// authority or keep the suspended computation alive.
#[derive(Clone)]
pub struct ContinuationId {
    owner: Arc<()>,
    slot: u32,
    revision: u64,
}
impl ContinuationId {
    /// Display slot local to one execution. Reuse preserves a different revision witness.
    #[must_use]
    pub fn ordinal(&self) -> u32 {
        self.slot
    }
    /// Display revision of this single-use capability, not a code revision or persistent ID.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub(crate) fn identity(&self) -> (usize, u32, u64) {
        (Arc::as_ptr(&self.owner) as usize, self.slot, self.revision)
    }
}
impl PartialEq for ContinuationId {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}
impl Eq for ContinuationId {}
impl Hash for ContinuationId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.identity().hash(state);
    }
}
impl fmt::Debug for ContinuationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContinuationId")
            .field("execution", &Arc::as_ptr(&self.owner))
            .field("ordinal", &self.slot)
            .field("revision", &self.revision)
            .finish()
    }
}

struct Capability {
    key: ContinuationId,
    parameters: ContractOwner,
    results: ContractOwner,
}

/// One-use authority to resume a computation retained by its execution.
///
/// Cloning creates an alias to the same authority, not another copy of the computation.
/// Consuming any alias makes all of them stale. The originating execution owns the suspended
/// registers and storage; retaining this reference does not keep its VM or execution alive.
/// Normal completion or hard cancellation invalidates its remaining capabilities.
#[derive(Clone)]
pub struct ContinuationRef(Arc<Capability>);
impl ContinuationRef {
    pub(crate) fn new(
        key: ContinuationId,
        parameters: ContractOwner,
        results: ContractOwner,
    ) -> Self {
        Self(Arc::new(Capability {
            key,
            parameters,
            results,
        }))
    }
    /// Logical identity for inspection, distinct from authority to resume the computation.
    #[must_use]
    pub fn id(&self) -> &ContinuationId {
        &self.0.key
    }

    pub(crate) fn identity(&self) -> (usize, u32, u64) {
        self.0.key.identity()
    }

    pub(crate) fn parameter_owner(&self) -> &ContractOwner {
        &self.0.parameters
    }
    pub(crate) fn result_owner(&self) -> &ContractOwner {
        &self.0.results
    }

    pub(crate) fn metadata_identity(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }

    pub(crate) fn has_signature(&self, other: &Self) -> bool {
        let equal = |a: &ContractOwner, b: &ContractOwner| {
            a.range == b.range && a.program.shares_definition_with(&b.program)
        };
        equal(&self.0.parameters, &other.0.parameters) && equal(&self.0.results, &other.0.results)
    }

    /// Contracts required at this suspension point, independent of the original function inputs.
    #[must_use]
    pub fn parameters(&self) -> ContractList<'_> {
        self.0.parameters.view()
    }

    /// Contracts produced when the captured computation eventually returns to its resumer.
    #[must_use]
    pub fn results(&self) -> ContractList<'_> {
        self.0.results.view()
    }
}
impl PartialEq for ContinuationRef {
    fn eq(&self, other: &Self) -> bool {
        self.0.key == other.0.key
    }
}
impl Eq for ContinuationRef {}
impl Hash for ContinuationRef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.key.hash(state);
    }
}
impl fmt::Debug for ContinuationRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContinuationRef")
            .field("capability", &self.0.key)
            .field("parameters", &self.parameters())
            .field("results", &self.results())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        asm::{Asm, FunctionSig, ProgramBuilder},
        host::{Host, HostContext, HostError, SigHash, ValueRef},
        program::ValueType,
        recording::{RecordLimits, Recorder, ReplayOutcome},
        trace::TraceSink,
        value::Value,
        vm::{Limits, Vm},
    };
    use alloc::vec;

    struct NoHost;
    impl Host for NoHost {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[ValueRef<'_>],
            _: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            panic!("control metadata and register transfers execute no native code");
        }
    }
    fn protocol() -> FunctionSig {
        FunctionSig {
            arg_types: vec![ValueType::Str],
            ret_types: vec![ValueType::I64],
        }
    }

    #[test]
    fn equal_protocols_do_not_merge_declarations_instances_or_fresh_scopes() {
        let mut builder = ProgramBuilder::new();
        let first = builder.control_tag("condition", protocol()).unwrap();
        let second = builder.control_tag("condition", protocol()).unwrap();
        let program = builder.build_verified().unwrap();
        assert_ne!(first, second);
        let mut vm = Vm::new(NoHost, Limits::default());
        let a = vm.instantiate(&program).unwrap();
        let b = vm.instantiate(&program).unwrap();
        let tag = a.control_tag(first).unwrap();
        assert_eq!(tag, a.control_tag(first).unwrap());
        assert_ne!(tag, a.control_tag(second).unwrap());
        assert_ne!(tag, b.control_tag(first).unwrap());
        assert_eq!(tag.name(), "condition");
        assert_eq!(tag.request().kinds(), &[ValueType::Str]);
        assert_eq!(tag.response().kinds(), &[ValueType::I64]);
        let home = tag.fresh();
        assert_eq!(home, home.clone());
        assert_ne!(home, tag);
        assert_ne!(
            home,
            tag.fresh(),
            "a later lexical home cannot catch this scope's exit"
        );
    }

    #[test]
    fn control_declarations_roundtrip_and_reject_bad_signatures_before_instantiation() {
        let mut builder = ProgramBuilder::new();
        let tag = builder.control_tag("supply-value", protocol()).unwrap();
        let program = builder.build_checked().unwrap();
        let decoded = Program::decode(&program.encode()).unwrap();
        assert_eq!(program, decoded);
        let mut malformed = decoded;
        malformed.control_tags[tag.0 as usize].signature = CallSigId(u32::MAX);
        assert!(matches!(
            crate::verifier::verify_program_owned(
                malformed,
                &crate::verifier::VerifyConfig::default()
            ),
            Err(crate::verifier::VerifyError::ControlTagSignature {
                tag: 0,
                signature: u32::MAX
            })
        ));
    }

    #[test]
    fn typed_join_cycles_and_replay_preserve_fresh_tag_sharing() {
        let mut builder = ProgramBuilder::new();
        let declaration = builder.control_tag("lexical-home", protocol()).unwrap();
        let mut body = Asm::new();
        body.set_register_types(vec![ValueType::Unit, ValueType::Tag, ValueType::Tag]);
        let joined = body.block(&[1, 2]);
        body.jmp(joined.with_args(&[2, 1]));
        body.place(joined).unwrap();
        body.ret(0, &[1, 2]);
        let entry = builder
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![ValueType::Tag, ValueType::Tag],
                    ret_types: vec![ValueType::Tag, ValueType::Tag],
                },
            )
            .unwrap();
        let program = Arc::new(builder.build_verified().unwrap());
        let mut vm = Vm::new(NoHost, Limits::default());
        let instance = vm.instantiate(&program).unwrap();
        let prototype = instance.control_tag(declaration).unwrap();
        for shared in [false, true] {
            let first = prototype.fresh();
            let second = if shared {
                first.clone()
            } else {
                prototype.fresh()
            };
            let args = [Value::Tag(first.clone()), Value::Tag(second.clone())];
            let mut recorder = Recorder::new(program.clone(), RecordLimits::default());
            let mask = recorder.mask();
            assert_eq!(
                vm.run(&program, entry, &args, mask, Some(&mut recorder))
                    .unwrap(),
                [Value::Tag(second), Value::Tag(first)]
            );
            let record = recorder.finish();
            let ReplayOutcome::Returned(replayed) = record.replay().unwrap() else {
                panic!("normal tag return");
            };
            assert_eq!(replayed.values()[0] == replayed.values()[1], shared);
            assert!(
                replayed
                    .values()
                    .iter()
                    .all(|value| matches!(value, Value::Tag(tag) if tag.name() == "lexical-home"))
            );
        }
        assert_eq!(
            program
                .function(entry)
                .unwrap()
                .register_slots(ValueType::Tag),
            3,
            "one scratch slot breaks the simultaneous swap cycle"
        );
        assert_eq!(vm.aggregates().len_u32(), 0);
    }
}

/// A denied resumption leaves all valid authority in its original execution intact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeError {
    /// The capability was created by a different execution.
    ForeignExecution,
    /// An alias has already resumed or discarded this suspension.
    Consumed,
    /// The execution has completed or been cancelled.
    Closed,
}
impl fmt::Display for ResumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ForeignExecution => "continuation belongs to another execution",
            Self::Consumed => "continuation authority has already been consumed",
            Self::Closed => "continuation execution is closed",
        })
    }
}
impl core::error::Error for ResumeError {}
impl From<arena::LookupError> for ResumeError {
    fn from(error: arena::LookupError) -> Self {
        match error {
            arena::LookupError::ForeignExecution => Self::ForeignExecution,
            arena::LookupError::Consumed => Self::Consumed,
            arena::LookupError::Closed => Self::Closed,
        }
    }
}

/// A dynamic handler installed by the embedding when resuming a computation.
///
/// Register-based authoring uses [`crate::asm::HandlerArm`]. Runtime bindings retain actual
/// operation identities and qualified callables; equal local IDs or signatures do not link them.
#[derive(Clone, Debug)]
pub enum Handler {
    /// Capture the computation inside the nearest matching resume delimiter.
    Suspend {
        /// Exact control operation, possibly shared between independently compiled instances.
        tag: ControlTag,
        /// Qualified function or closure receiving request values and a continuation.
        handler: crate::value::Value,
    },
}
impl Handler {
    /// Creates a suspension clause from a function, closure, or dynamic callable value.
    /// The resume operation validates its contracts against the operation and final results.
    #[must_use]
    pub fn suspend(tag: ControlTag, handler: impl Into<crate::value::Value>) -> Self {
        Self::Suspend {
            tag,
            handler: handler.into(),
        }
    }
}
