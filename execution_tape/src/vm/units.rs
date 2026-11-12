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
use crate::contract::{ContractId, ContractTableBuilder};
use crate::program::{CallSigId, ElemTypeId, StructTypeDef, TagId, TypeId};
use crate::value::{ShapeId, TagRef};
use alloc::collections::BTreeMap;

/// A signature of canonical contracts interned once per VM, so indirect calls compare one integer.
///
/// Indirect calls require the target's exact contract signature, as within one program.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct SigId(u32);

/// Canonical argument and result contracts of one signature.
type SignatureKey = (Box<[ContractId]>, Box<[ContractId]>);

/// Store-wide canonical forms shared by every unit of one VM.
#[derive(Default)]
pub(crate) struct Canon {
    signatures: BTreeMap<SignatureKey, SigId>,
    /// Contracts with every nominal type replaced by its heap shape and children canonicalized.
    contracts: ContractTableBuilder,
}

impl Canon {
    fn signature(
        &mut self,
        args: &[ContractId],
        rets: &[ContractId],
    ) -> Result<SigId, InstallError> {
        let key: SignatureKey = (args.into(), rets.into());
        if let Some(&id) = self.signatures.get(&key) {
            return Ok(id);
        }
        let id = u32::try_from(self.signatures.len())
            .map(SigId)
            .map_err(|_| InstallError::CapacityExceeded)?;
        self.signatures.insert(key, id);
        Ok(id)
    }
}

/// Code and store-local metadata for one installed program.
pub(crate) struct Unit {
    pub(crate) program: Arc<VerifiedProgram>,
    /// Heap shape of each program [`TypeId`].
    structs: Box<[ShapeId]>,
    /// Heap shape of each program [`ElemTypeId`].
    arrays: Box<[ShapeId]>,
    /// Store-canonical identity of each program contract.
    contracts: Box<[ContractId]>,
    /// Kind signature of each function called directly.
    direct: Box<[SigId]>,
    /// Kind signature of each function called as a closure body, without its environment.
    closure: Box<[Option<SigId>]>,
    /// Kind signature of each indirect call site signature.
    call_sigs: Box<[SigId]>,
    /// VM tag of each program [`crate::program::TagId`].
    tags: Box<[TagRef]>,
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

    /// The VM tag a tag of this unit's program is bound to.
    #[inline]
    pub(crate) fn tag(&self, tag: TagId) -> Option<TagRef> {
        self.tags.get(tag.0 as usize).copied()
    }

    /// This unit's own name for a VM tag, if its program declares or imports it.
    pub(crate) fn local_tag(&self, tag: TagRef) -> Option<TagId> {
        self.tags
            .iter()
            .position(|&bound| bound == tag)
            .map(|index| TagId(u32::try_from(index).expect("verified tag count")))
    }

    /// The store-canonical identity of a verified contract of this unit's program.
    #[inline]
    pub(crate) fn canonical(&self, contract: ContractId) -> ContractId {
        self.contracts[contract.0 as usize]
    }

    /// Whether `callee` of this unit can be the target of `call_sig` in `caller`.
    #[inline]
    pub(crate) fn accepts_call(
        &self,
        callee: FuncId,
        closure: bool,
        caller: &Self,
        call_sig: CallSigId,
    ) -> bool {
        let actual = if closure {
            self.closure.get(callee.0 as usize).copied().flatten()
        } else {
            self.direct.get(callee.0 as usize).copied()
        };
        actual.is_some() && actual == caller.call_sigs.get(call_sig.0 as usize).copied()
    }

    fn metadata_bytes(&self) -> usize {
        size_of::<Self>()
            + size_of_val(&*self.structs)
            + size_of_val(&*self.arrays)
            + size_of_val(&*self.contracts)
            + size_of_val(&*self.direct)
            + size_of_val(&*self.closure)
            + size_of_val(&*self.call_sigs)
            + size_of_val(&*self.tags)
    }

    /// Interns `program`'s struct and array types in `heap`, and its contracts and signatures
    /// in `canon`.
    fn new(
        program: Arc<VerifiedProgram>,
        heap: &mut AggHeap,
        canon: &mut Canon,
    ) -> Result<Self, InstallError> {
        let source = program.program();
        let types = &source.types;
        let mut intern = |shape| {
            heap.intern_shape(shape)
                .map_err(|_| InstallError::CapacityExceeded)
        };
        let structs: Box<[ShapeId]> = types
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
        let arrays: Box<[ShapeId]> = types
            .array_elems
            .iter()
            .map(|&element| intern(AggShape::Array(element)))
            .collect::<Result<_, _>>()?;
        let contracts = canonical_contracts(source, &structs, &arrays, &mut canon.contracts)?;
        let canonical = |ids: &[ContractId]| -> Vec<ContractId> {
            ids.iter().map(|id| contracts[id.0 as usize]).collect()
        };
        let mut direct = Vec::with_capacity(source.functions.len());
        let mut closure = Vec::with_capacity(source.functions.len());
        for function in &source.functions {
            let local = source.verified_contracts(function.arg_types);
            let args = canonical(local);
            let rets = canonical(source.verified_contracts(function.ret_types));
            direct.push(canon.signature(&args, &rets)?);
            let environment = local
                .first()
                .is_some_and(|&first| source.contracts.kind(first) == Some(ValueType::Agg));
            closure.push(if environment {
                Some(canon.signature(&args[1..], &rets)?)
            } else {
                None
            });
        }
        let call_sigs = source
            .call_sigs
            .iter()
            .map(|entry| {
                let args = source
                    .call_sig_arg_contracts(entry)
                    .expect("verified call signature");
                let rets = source
                    .call_sig_ret_contracts(entry)
                    .expect("verified call signature");
                canon.signature(&canonical(args), &canonical(rets))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            program,
            structs,
            arrays,
            contracts,
            direct: direct.into(),
            closure: closure.into(),
            call_sigs,
            tags: Box::default(),
        })
    }
}

/// Maps each program contract to its canonical form, child before parent.
fn canonical_contracts(
    program: &Program,
    structs: &[ShapeId],
    arrays: &[ShapeId],
    canon: &mut ContractTableBuilder,
) -> Result<Box<[ContractId]>, InstallError> {
    let mut ids: Vec<ContractId> = Vec::with_capacity(program.contracts.len());
    let mut members = Vec::new();
    let mut fields = Vec::new();
    for (_, contract) in program.contracts.iter() {
        let map = |id: ContractId| ids[id.0 as usize];
        let canonical = match contract {
            Contract::Kind(kind) => Contract::Kind(kind),
            Contract::Tuple(slots) => {
                members.clear();
                members.extend(slots.iter().map(|slot| slot.map(map)));
                Contract::Tuple(&members)
            }
            Contract::Struct { type_id, fields: f } => {
                fields.clear();
                fields.extend(f.iter().copied().map(map));
                Contract::Struct {
                    type_id: TypeId(structs[type_id.0 as usize].0),
                    fields: &fields,
                }
            }
            Contract::Array {
                elem_type_id,
                element,
            } => Contract::Array {
                elem_type_id: ElemTypeId(arrays[elem_type_id.0 as usize].0),
                element: map(element),
            },
            Contract::Function { args, rets } | Contract::Closure { args, rets } => {
                fields.clear();
                fields.extend(args.iter().chain(rets).copied().map(map));
                let (args, rets) = fields.split_at(args.len());
                if matches!(contract, Contract::Function { .. }) {
                    Contract::Function { args, rets }
                } else {
                    Contract::Closure { args, rets }
                }
            }
        };
        ids.push(
            canon
                .intern(canonical)
                .map_err(|_| InstallError::CapacityExceeded)?,
        );
    }
    Ok(ids.into())
}

/// One exception tag of a VM.
pub(crate) struct Tag {
    name: Box<str>,
    /// Payload storage kinds.
    kinds: Box<[ValueType]>,
    /// Canonical payload contracts, compared when a unit imports the tag.
    payload: Box<[ContractId]>,
    /// The unit and local tag that define it, whose contracts check host payloads.
    pub(crate) definer: Option<(UnitId, TagId)>,
}

impl Tag {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
    pub(crate) fn kinds(&self) -> &[ValueType] {
        &self.kinds
    }
}

/// One change to a VM's units and tags, in the order it happened.
///
/// [`Units::log`] lists them. Repeating the same installs and tag definitions in the same order
/// in a fresh VM reproduces every [`UnitId`] and [`TagRef`], which is how replay rebuilds a
/// recorded VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreEvent {
    /// [`Vm::install`] created this unit.
    Installed(UnitId),
    /// [`Vm::define_tag`] created this tag.
    DefinedTag(TagRef),
}

/// The VM's append-only table of installed units.
#[derive(Default)]
pub(crate) struct UnitTable {
    units: Vec<Unit>,
    canon: Canon,
    tags: Vec<Tag>,
    /// Tags that programs can import, by name.
    defined: BTreeMap<Box<str>, TagRef>,
    /// Installs and tag definitions in order.
    log: Vec<StoreEvent>,
}

impl fmt::Debug for UnitTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnitTable")
            .field("units", &self.units.len())
            .finish_non_exhaustive()
    }
}

impl UnitTable {
    #[inline]
    pub(crate) fn get(&self, unit: UnitId) -> Option<&Unit> {
        self.units.get(unit.0 as usize)
    }

    /// A unit this VM issued. Units are never removed, so a frame always names an existing
    /// unit. Values come from outside the VM too: resolve their units with [`Self::get`].
    #[inline]
    pub(crate) fn unit(&self, unit: UnitId) -> &Unit {
        &self.units[unit.0 as usize]
    }

    /// A tag this VM issued, or `None`.
    #[inline]
    pub(crate) fn tag(&self, tag: TagRef) -> Option<&Tag> {
        self.tags.get(tag.0 as usize)
    }

    /// Installs `program`, requiring every imported tag to be defined.
    pub(crate) fn install(
        &mut self,
        program: Arc<VerifiedProgram>,
        heap: &mut AggHeap,
    ) -> Result<UnitId, InstallError> {
        let id = u32::try_from(self.units.len())
            .ok()
            .filter(|&id| id < u32::MAX)
            .map(UnitId)
            .ok_or(InstallError::CapacityExceeded)?;
        let mut unit = Unit::new(program, heap, &mut self.canon)?;
        unit.tags = self.bind_tags(id, &unit)?;
        self.units.push(unit);
        self.log.push(StoreEvent::Installed(id));
        Ok(id)
    }

    /// Binds every tag of `unit`'s program: imports to defined tags, others to new tags.
    ///
    /// Imports are resolved before any tag is created, so a rejected install creates no tag.
    fn bind_tags(&mut self, id: UnitId, unit: &Unit) -> Result<Box<[TagRef]>, InstallError> {
        let program = unit.program.program();
        let declared = |tag: TagId| {
            let entry = program.tag(tag).expect("verified tag");
            let name = program.tag_name(tag).expect("verified tag name");
            let payload: Box<[ContractId]> = program
                .tag_payload_contracts(entry)
                .expect("verified tag payload")
                .iter()
                .map(|&contract| unit.canonical(contract))
                .collect();
            let kinds: Box<[ValueType]> = program
                .tag_payload(entry)
                .expect("verified tag payload")
                .iter()
                .copied()
                .collect();
            (name, payload, kinds)
        };
        let mut bound: Vec<Option<TagRef>> = vec![None; program.tags.len()];
        for &import in &program.tag_imports {
            let (name, payload, _) = declared(import);
            match self.defined.get(name) {
                Some(&tag) if self.tags[tag.0 as usize].payload == payload => {
                    bound[import.0 as usize] = Some(tag);
                }
                Some(_) => return Err(InstallError::TagPayloadMismatch { name: name.into() }),
                None => return Err(InstallError::UnresolvedTag { name: name.into() }),
            }
        }
        self.tags
            .len()
            .checked_add(program.tags.len())
            .filter(|&end| u32::try_from(end).is_ok())
            .ok_or(InstallError::CapacityExceeded)?;
        for (index, slot) in bound.iter_mut().enumerate() {
            if slot.is_some() {
                continue;
            }
            let local = TagId(u32::try_from(index).expect("verified tag count"));
            let (name, payload, kinds) = declared(local);
            let tag = TagRef(u32::try_from(self.tags.len()).expect("checked tag capacity"));
            self.tags.push(Tag {
                name: name.into(),
                kinds,
                payload,
                definer: Some((id, local)),
            });
            *slot = Some(tag);
        }
        Ok(bound
            .into_iter()
            .map(|tag| tag.expect("bound tag"))
            .collect())
    }

    /// Defines a tag that programs import by `name`.
    pub(crate) fn define_tag(
        &mut self,
        name: &str,
        kinds: &[ValueType],
    ) -> Result<TagRef, TagError> {
        if self.defined.contains_key(name) {
            return Err(TagError::DuplicateName { name: name.into() });
        }
        if kinds.contains(&ValueType::Exn) {
            return Err(TagError::ExceptionPayload);
        }
        let tag = u32::try_from(self.tags.len())
            .map(TagRef)
            .map_err(|_| TagError::CapacityExceeded)?;
        let payload = kinds
            .iter()
            .map(|&kind| {
                self.canon
                    .contracts
                    .intern(Contract::Kind(kind))
                    .map_err(|_| TagError::CapacityExceeded)
            })
            .collect::<Result<_, _>>()?;
        self.tags.push(Tag {
            name: name.into(),
            kinds: kinds.into(),
            payload,
            definer: None,
        });
        self.defined.insert(name.into(), tag);
        self.log.push(StoreEvent::DefinedTag(tag));
        Ok(tag)
    }
}

/// Why a tag could not be defined.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TagError {
    /// A tag with this name is already defined in the VM.
    DuplicateName {
        /// The requested name.
        name: Box<str>,
    },
    /// An exception reference cannot be part of a payload.
    ExceptionPayload,
    /// The VM cannot represent another tag.
    CapacityExceeded,
}

impl fmt::Display for TagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateName { name } => write!(f, "tag \"{name}\" is already defined"),
            Self::ExceptionPayload => {
                f.write_str("a tag payload cannot hold an exception reference")
            }
            Self::CapacityExceeded => f.write_str("VM tag capacity exhausted"),
        }
    }
}

impl core::error::Error for TagError {}

/// Why a program could not be installed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstallError {
    /// The VM cannot represent another unit.
    CapacityExceeded,
    /// The program imports a tag that is not defined in the VM.
    UnresolvedTag {
        /// The imported tag name.
        name: Box<str>,
    },
    /// The program imports a defined tag but declares a different payload for it.
    TagPayloadMismatch {
        /// The imported tag name.
        name: Box<str>,
    },
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityExceeded => f.write_str("VM unit capacity exhausted"),
            Self::UnresolvedTag { name } => write!(f, "imported tag \"{name}\" is not defined"),
            Self::TagPayloadMismatch { name } => {
                write!(f, "imported tag \"{name}\" declares a different payload")
            }
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

    /// The VM tag bound to `tag` of `unit`'s program.
    #[must_use]
    pub fn tag(&self, unit: UnitId, tag: TagId) -> Option<TagRef> {
        self.table.get(unit)?.tag(tag)
    }

    /// The name of a VM tag.
    #[must_use]
    pub fn tag_name(&self, tag: TagRef) -> Option<&'a str> {
        self.table.tag(tag).map(Tag::name)
    }

    /// The payload storage kinds of a VM tag.
    #[must_use]
    pub fn tag_payload(&self, tag: TagRef) -> Option<&'a [ValueType]> {
        self.table.tag(tag).map(Tag::kinds)
    }

    /// Every tag of `unit`'s program, in [`TagId`] order, bound to its VM tag.
    #[must_use]
    pub fn tags(&self, unit: UnitId) -> Option<&'a [TagRef]> {
        self.table.get(unit).map(|unit| &*unit.tags)
    }

    /// Installs and tag definitions in the order they happened.
    #[must_use]
    pub fn log(&self) -> &'a [StoreEvent] {
        &self.table.log
    }

    /// The defined tag that programs import by `name`.
    #[must_use]
    pub fn defined_tag(&self, name: &str) -> Option<TagRef> {
        self.table.defined.get(name).copied()
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
    ///
    /// Each tag the program declares becomes a new VM tag, so two installs of one program throw
    /// distinct exceptions. Each tag it imports binds to the tag defined under that name with
    /// [`Self::define_tag`]; an undefined name or a different payload rejects the install, which
    /// then creates no unit and no tag. Shapes it interned stay in the heap, as shapes always do.
    pub fn install(
        &mut self,
        program: impl Into<Arc<VerifiedProgram>>,
    ) -> Result<UnitId, InstallError> {
        self.units.install(program.into(), self.memory.aggs_mut())
    }

    /// Defines a tag that installed programs can import by `name`.
    ///
    /// The payload has these storage kinds and no further contracts; a program importing the
    /// tag must declare exactly that payload. Names are unique among defined tags, and tags are
    /// never removed. Hosts throw the returned tag with [`crate::host::HostError::throw`].
    pub fn define_tag(&mut self, name: &str, payload: &[ValueType]) -> Result<TagRef, TagError> {
        self.units.define_tag(name, payload)
    }

    /// Inspects the units installed in this VM.
    #[must_use]
    pub fn units(&self) -> Units<'_> {
        Units { table: &self.units }
    }
}
