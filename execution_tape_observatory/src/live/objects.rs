// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Embedding-supplied, read-only views of opaque host objects.

use execution_tape::{
    host::Host,
    value::{Obj, Value},
};

/// A bounded read-only projection of a host-owned object, supplied on demand.
///
/// Names and string/byte previews are bounded and control text is escaped by the explorer. Object-valued
/// fields can be followed through the same inspector. VM-local aggregate, function and
/// closure handles have no ownership context here and are displayed as unavailable;
/// return scalar previews or host object references instead.
#[derive(Clone, Debug)]
pub struct ObjectView {
    /// Human-readable object name or class, such as `CounterCell`.
    pub name: String,
    /// Ordered field labels and owned preview values. Supply at most the requested field limit.
    pub fields: Vec<(String, Value)>,
    /// Actual field count when known, including fields omitted from this bounded view.
    pub total_fields: Option<usize>,
}

/// Reads host object state between guest execution quanta, without invoking guest code.
///
/// This adapter is optional and belongs to the live UI. It must avoid effects, guest/host
/// method evaluation and unbounded materialization. Respect `max_fields` before constructing
/// the result and bound payload previews at the source. `None` means unavailable, not empty
/// or collected. Inspection neither retains external objects nor adds collection roots.
/// Recordings cannot use this adapter after their original host has disappeared.
pub trait ObjectInspector<H: Host> {
    /// Projects an object from the actual invocation host without changing guest state.
    fn inspect(&mut self, host: &H, object: Obj, max_fields: usize) -> Option<ObjectView>;
}

impl<H: Host, F: FnMut(&H, Obj, usize) -> Option<ObjectView>> ObjectInspector<H> for F {
    fn inspect(&mut self, host: &H, object: Obj, max_fields: usize) -> Option<ObjectView> {
        self(host, object, max_fields)
    }
}
