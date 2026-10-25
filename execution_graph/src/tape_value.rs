// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Ownership and program domains at the graph/VM boundary.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use execution_tape::aggregates::{AggError, AggHeap, AggRemap, ValueSnapshot};
use execution_tape::program::{ElemTypeId, TypeId};
use execution_tape::value::{AggHandle, AggType, Value};
use execution_tape::verifier::VerifiedProgram;

use crate::TapeError;

#[cfg(test)]
#[path = "tape_value_tests.rs"]
mod tests;

/// An independently owned value carried by tape graph edges.
///
/// Cloning retains immutable storage, including string/byte payloads. Aggregate and closure
/// results remain readable after replacement, node removal, or executor destruction. Shared
/// aggregate outputs from one invocation form a bundle: retaining one keeps that bundle alive.
/// Dropping its last owner releases it without an executor callback or collection pass.
///
/// Program-local function and nominal type IDs retain their exact prepared program. Portable
/// scalar and tuple-only values can cross programs; scoped IDs cannot be reinterpreted by a
/// different program. Host object tokens remain host-owned and do not retain external objects.
///
/// Convert plain inputs with `Value::I64(42).into()`. Raw aggregate, function and closure values
/// have no owner and are rejected at execution ingress. Use [`Self::from_snapshot`] to import
/// retained values with an explicit program domain.
#[derive(Clone)]
pub struct TapeValue {
    repr: Repr,
}

/// Ownership domain needed to interpret a [`TapeValue`].
#[derive(Clone, Copy)]
pub enum TapeValueDomain<'a> {
    /// No program-local IDs occur in this value or its reachable aggregate graph.
    Portable,
    /// Scoped IDs belong to this exact prepared program. Equivalent names, signatures and
    /// numeric IDs in another preparation do not establish identity.
    Program(&'a Arc<VerifiedProgram>),
    /// A raw aggregate or callable was converted without an owner; execution rejects it.
    Unowned,
}

impl fmt::Debug for TapeValueDomain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Portable => f.write_str("Portable"),
            Self::Unowned => f.write_str("Unowned"),
            Self::Program(program) => f
                .debug_tuple("Program")
                .field(&program.program().name())
                .finish(),
        }
    }
}

#[derive(Clone)]
enum Repr {
    Inline(Value),
    Payload(Arc<Value>),
    Program {
        value: Value,
        program: Arc<VerifiedProgram>,
    },
    Shared {
        bundle: Arc<Bundle>,
        index: usize,
    },
}

struct Bundle {
    snapshot: ValueSnapshot,
    origins: Origins,
    program: Option<Arc<VerifiedProgram>>,
    local: Vec<bool>,
}

// Tokens identify original immutable nodes across copies made at invocation boundaries. They
// never retain ancestor bundles or payloads, and need no process-global ID allocator.
struct Domain;

#[derive(Clone, Copy)]
struct NodeOrigin {
    domain: u32,
    node: u32,
}

#[derive(Default)]
pub(crate) struct Origins {
    domains: Vec<Arc<Domain>>,
    nodes: Vec<NodeOrigin>,
}

type OriginKey = (usize, u32);

impl Origins {
    fn fresh(len: u32) -> Self {
        Self {
            domains: if len == 0 {
                vec![]
            } else {
                vec![Arc::new(Domain)]
            },
            nodes: (0..len)
                .map(|node| NodeOrigin { domain: 0, node })
                .collect(),
        }
    }

    fn key(&self, handle: AggHandle) -> OriginKey {
        let origin = self.nodes[handle.0 as usize];
        (
            Arc::as_ptr(&self.domains[origin.domain as usize]) as usize,
            origin.node,
        )
    }

    fn finish(mut self, len: u32, remap: &AggRemap) -> Self {
        if self.nodes.len() < len as usize {
            let domain = u32::try_from(self.domains.len()).expect("domains cannot outnumber nodes");
            self.domains.push(Arc::new(Domain));
            for node in u32::try_from(self.nodes.len()).expect("heap length fits handles")..len {
                self.nodes.push(NodeOrigin { domain, node });
            }
        }
        let mut original = 0_u32;
        self.nodes.retain(|_| {
            let keep = remap.remap_handle(AggHandle(original)).is_ok();
            original += 1;
            keep
        });
        // Compaction preserves node order. Keep only domain tokens used by retained nodes.
        let mut domains = Vec::new();
        let mut used = BTreeMap::new();
        for origin in &mut self.nodes {
            origin.domain = *used.entry(origin.domain).or_insert_with(|| {
                let id = u32::try_from(domains.len()).expect("domains cannot outnumber nodes");
                domains.push(Arc::clone(&self.domains[origin.domain as usize]));
                id
            });
        }
        self.domains = domains;
        self.nodes.shrink_to_fit();
        self
    }
}

impl TapeValue {
    /// Adopts a retained snapshot and returns one owned value per root.
    ///
    /// Supply the exact original prepared program for values containing function, closure,
    /// struct or array IDs, including IDs nested inside tuples. `None` admits portable roots
    /// only. Supplying a program declares how the snapshot's scoped IDs are interpreted; it
    /// does not translate IDs from another program. Sharing within the snapshot is preserved.
    /// Shared roots retain the whole snapshot bundle, so choose retention groups deliberately.
    pub fn from_snapshot(
        snapshot: ValueSnapshot,
        program: Option<Arc<VerifiedProgram>>,
    ) -> Result<Vec<Self>, TapeError> {
        let origins = Origins::fresh(snapshot.aggregates().len_u32());
        Self::adopt(snapshot, program, origins)
    }

    fn adopt(
        snapshot: ValueSnapshot,
        program: Option<Arc<VerifiedProgram>>,
        origins: Origins,
    ) -> Result<Vec<Self>, TapeError> {
        let mut memo = BTreeMap::new();
        let local: Vec<_> = snapshot
            .values()
            .iter()
            .map(|value| is_local(value, snapshot.aggregates(), &mut memo))
            .collect::<Result<_, _>>()
            .map_err(TapeError::Aggregate)?;
        if program.is_none()
            && let Some(root) = local.iter().position(|&local| local)
        {
            return Err(TapeError::MissingProgram { root });
        }
        let program = if local.iter().any(|&local| local) {
            program
        } else {
            None
        };
        let count = snapshot.values().len();
        let bundle = Arc::new(Bundle {
            snapshot,
            origins,
            program,
            local,
        });
        Ok((0..count)
            .map(|index| match &bundle.snapshot.values()[index] {
                Value::Agg(_) | Value::Closure(_) | Value::Bytes(_) | Value::Str(_) => Self {
                    repr: Repr::Shared {
                        bundle: Arc::clone(&bundle),
                        index,
                    },
                },
                Value::Func(_) => Self {
                    repr: Repr::Program {
                        value: bundle.snapshot.values()[index].clone(),
                        program: Arc::clone(
                            bundle.program.as_ref().expect("scoped value has program"),
                        ),
                    },
                },
                value => Self {
                    repr: Repr::Inline(value.clone()),
                },
            })
            .collect())
    }

    /// Borrows the root. Aggregate handles refer to [`Self::aggregates`], not a VM heap.
    #[must_use]
    pub fn value(&self) -> &Value {
        match &self.repr {
            Repr::Inline(value) | Repr::Program { value, .. } => value,
            Repr::Payload(value) => value,
            Repr::Shared { bundle, index } => &bundle.snapshot.values()[*index],
        }
    }

    /// Borrows immutable aggregate storage owned by this value, if it has shared storage.
    #[must_use]
    pub fn aggregates(&self) -> Option<&AggHeap> {
        match &self.repr {
            Repr::Shared { bundle, .. } => Some(bundle.snapshot.aggregates()),
            _ => None,
        }
    }

    /// Distinguishes portable values, an exact prepared-program owner, and unowned raw IDs.
    #[must_use]
    pub fn domain(&self) -> TapeValueDomain<'_> {
        match &self.repr {
            Repr::Inline(Value::Agg(_) | Value::Func(_) | Value::Closure(_)) => {
                TapeValueDomain::Unowned
            }
            Repr::Program { program, .. } => TapeValueDomain::Program(program),
            Repr::Shared { bundle, index } if bundle.local[*index] => {
                TapeValueDomain::Program(bundle.program.as_ref().expect("scoped value has program"))
            }
            _ => TapeValueDomain::Portable,
        }
    }

    fn program(&self) -> Option<&Arc<VerifiedProgram>> {
        match self.domain() {
            TapeValueDomain::Program(program) => Some(program),
            _ => None,
        }
    }

    pub(crate) fn validate_input(
        &self,
        index: usize,
        program: &Arc<VerifiedProgram>,
    ) -> Result<(), TapeError> {
        if matches!(self.domain(), TapeValueDomain::Unowned) {
            return Err(TapeError::UnownedInput { input: index });
        }
        if self
            .program()
            .is_some_and(|owner| !Arc::ptr_eq(owner, program))
        {
            return Err(TapeError::ProgramMismatch { input: index });
        }
        Ok(())
    }

    pub(crate) fn plain_result(value: Value, program: &Arc<VerifiedProgram>) -> Self {
        match value {
            Value::Func(_) => Self {
                repr: Repr::Program {
                    value,
                    program: Arc::clone(program),
                },
            },
            value => value.into(),
        }
    }

    pub(crate) fn publish(
        heap: AggHeap,
        values: Vec<Value>,
        program: Arc<VerifiedProgram>,
        origins: Origins,
        outputs: &mut Vec<Self>,
    ) -> Result<(), TapeError> {
        let mut shared = Vec::new();
        let mut positions = Vec::new();
        for value in values {
            if matches!(value, Value::Agg(_) | Value::Closure(_)) {
                positions.push(outputs.len());
                outputs.push(Value::Unit.into());
                shared.push(value);
            } else {
                outputs.push(Self::plain_result(value, &program));
            }
        }
        if shared.is_empty() {
            return Ok(());
        }
        let len = heap.len_u32();
        let (snapshot, remap) = heap.into_snapshot(shared).map_err(TapeError::Aggregate)?;
        let owned = Self::adopt(snapshot, Some(program), origins.finish(len, &remap))?;
        for (position, value) in positions.into_iter().zip(owned) {
            outputs[position] = value;
        }
        Ok(())
    }
}

impl From<Value> for TapeValue {
    fn from(value: Value) -> Self {
        if matches!(value, Value::Bytes(_) | Value::Str(_)) {
            Self {
                repr: Repr::Payload(Arc::new(value)),
            }
        } else {
            Self {
                repr: Repr::Inline(value),
            }
        }
    }
}

impl fmt::Debug for TapeValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TapeValue")
            .field("value", self.value())
            .field("domain", &self.domain())
            .field(
                "bundle_nodes",
                &self.aggregates().map_or(0, AggHeap::len_u32),
            )
            .finish()
    }
}

impl PartialEq for TapeValue {
    fn eq(&self, other: &Self) -> bool {
        match (self.value(), other.value()) {
            (Value::Agg(a), Value::Agg(b)) => match (&self.repr, &other.repr) {
                (
                    Repr::Shared {
                        bundle: a_owner, ..
                    },
                    Repr::Shared {
                        bundle: b_owner, ..
                    },
                ) => a_owner.origins.key(*a) == b_owner.origins.key(*b),
                _ => false,
            },
            (Value::Func(a), Value::Func(b)) => {
                a == b
                    && matches!((self.program(), other.program()), (Some(a), Some(b)) if Arc::ptr_eq(a, b))
            }
            (Value::Closure(a), Value::Closure(b)) => {
                a.func == b.func
                    && match (&self.repr, &other.repr) {
                        (
                            Repr::Shared {
                                bundle: a_owner, ..
                            },
                            Repr::Shared {
                                bundle: b_owner, ..
                            },
                        ) => {
                            a_owner.origins.key(a.env) == b_owner.origins.key(b.env)
                                && matches!((self.program(), other.program()), (Some(a), Some(b)) if Arc::ptr_eq(a, b))
                        }
                        _ => false,
                    }
            }
            (a, b) => a == b,
        }
    }
}

fn aggregate(value: &Value) -> Option<AggHandle> {
    match value {
        Value::Agg(handle) => Some(*handle),
        Value::Closure(closure) => Some(closure.env),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum Shape {
    Tuple(usize),
    Struct(TypeId, usize),
    Array(ElemTypeId, usize),
}

impl Shape {
    fn read(heap: &AggHeap, handle: AggHandle) -> Result<Self, AggError> {
        Ok(match heap.agg_type(handle)? {
            AggType::Tuple { arity } => Self::Tuple(arity as usize),
            AggType::Struct { type_id } => Self::Struct(type_id, heap.struct_field_count(handle)?),
            AggType::Array { elem_type_id } => Self::Array(elem_type_id, heap.array_len(handle)?),
        })
    }
    fn len(self) -> usize {
        match self {
            Self::Tuple(n) | Self::Struct(_, n) | Self::Array(_, n) => n,
        }
    }
    fn element(self, heap: &AggHeap, handle: AggHandle, index: usize) -> Result<&Value, AggError> {
        match self {
            Self::Tuple(_) => heap.tuple_get_ref(handle, index),
            Self::Struct(..) => heap.struct_get_ref(handle, index),
            Self::Array(..) => heap.array_get_ref(handle, index),
        }
    }
}

fn is_local(
    value: &Value,
    heap: &AggHeap,
    memo: &mut BTreeMap<u32, bool>,
) -> Result<bool, AggError> {
    match value {
        Value::Func(_) | Value::Closure(_) => return Ok(true),
        Value::Agg(_) => {}
        _ => return Ok(false),
    }
    let root = aggregate(value).expect("aggregate value");
    if let Some(&local) = memo.get(&root.0) {
        return Ok(local);
    }
    let mut pending = vec![(root, false)];
    while let Some((handle, visited)) = pending.pop() {
        if memo.contains_key(&handle.0) {
            continue;
        }
        let shape = Shape::read(heap, handle)?;
        if !matches!(shape, Shape::Tuple(_)) {
            memo.insert(handle.0, true);
            continue;
        }
        if !visited {
            pending.push((handle, true));
            for index in 0..shape.len() {
                if let Value::Agg(child) = shape.element(heap, handle, index)? {
                    pending.push((*child, false));
                }
            }
        } else {
            let mut local = false;
            for index in 0..shape.len() {
                local |= match shape.element(heap, handle, index)? {
                    Value::Func(_) | Value::Closure(_) => true,
                    Value::Agg(child) => memo[&child.0],
                    _ => false,
                };
            }
            memo.insert(handle.0, local);
        }
    }
    Ok(memo[&root.0])
}

/// One invocation's identity-preserving bridge into ordinary VM storage.
#[derive(Default)]
pub(crate) struct Importer {
    pub(crate) origins: Origins,
    domains: BTreeMap<usize, u32>,
    handles: BTreeMap<OriginKey, AggHandle>,
}

impl Importer {
    pub(crate) fn clear(&mut self) {
        self.origins.domains.clear();
        self.origins.nodes.clear();
        self.domains.clear();
        self.handles.clear();
    }

    pub(crate) fn import(
        &mut self,
        value: &TapeValue,
        target: &mut AggHeap,
    ) -> Result<Value, AggError> {
        let Some(root) = aggregate(value.value()) else {
            return Ok(value.value().clone());
        };
        let Repr::Shared { bundle, .. } = &value.repr else {
            return Err(AggError::BadHandle);
        };
        let source = bundle.snapshot.aggregates();
        if self.handles.contains_key(&bundle.origins.key(root)) {
            return Ok(self.remap(value.value(), &bundle.origins));
        }
        let mut pending = vec![(root, false)];
        while let Some((handle, visited)) = pending.pop() {
            let key = bundle.origins.key(handle);
            if self.handles.contains_key(&key) {
                continue;
            }
            let shape = Shape::read(source, handle)?;
            if !visited {
                pending.push((handle, true));
                for index in 0..shape.len() {
                    if let Some(child) = aggregate(shape.element(source, handle, index)?) {
                        pending.push((child, false));
                    }
                }
                continue;
            }
            let mut values = Vec::with_capacity(shape.len());
            for index in 0..shape.len() {
                values.push(self.remap(shape.element(source, handle, index)?, &bundle.origins));
            }
            let imported = match shape {
                Shape::Tuple(_) => target.tuple_new(values)?,
                Shape::Struct(id, _) => target.struct_new(id, values)?,
                Shape::Array(id, _) => target.array_new(id, values)?,
            };
            let original = bundle.origins.nodes[handle.0 as usize];
            let domain = *self.domains.entry(key.0).or_insert_with(|| {
                let id = u32::try_from(self.origins.domains.len())
                    .expect("domains cannot outnumber nodes");
                self.origins.domains.push(Arc::clone(
                    &bundle.origins.domains[original.domain as usize],
                ));
                id
            });
            debug_assert_eq!(
                imported.0 as usize,
                self.origins.nodes.len(),
                "origin metadata follows destination allocation order"
            );
            self.origins.nodes.push(NodeOrigin {
                domain,
                node: original.node,
            });
            self.handles.insert(key, imported);
        }
        Ok(self.remap(value.value(), &bundle.origins))
    }

    fn remap(&self, value: &Value, origins: &Origins) -> Value {
        match value {
            Value::Agg(handle) => Value::Agg(self.handles[&origins.key(*handle)]),
            Value::Closure(closure) => {
                let mut closure = *closure;
                closure.env = self.handles[&origins.key(closure.env)];
                Value::Closure(closure)
            }
            value => value.clone(),
        }
    }
}
