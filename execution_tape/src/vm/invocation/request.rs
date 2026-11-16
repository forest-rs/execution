// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Names for the requests a computation waits at, stable across the invocations that run it.

use super::*;

/// Names one host request or yielded suspension of one computation.
///
/// Read it with [`PendingHostCall::id`] or [`PendingSuspension::id`]. It stays valid while the
/// computation waits there, across [`Invocation::save`] and [`Vm::resume`], so an embedding that
/// answers later can find the request again with [`Invocation::host_request`] or
/// [`Invocation::suspension`] and be refused if the computation has moved on. Each admission is
/// a new request, even of the same instruction; a host request that a guest callback returned
/// to keeps its ID. It names its computation's VM by a number no other VM in the process has,
/// so no other VM accepts it, including one created after its VM was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RequestId {
    vm: u64,
    stack: u64,
    admission: u64,
    kind: RequestKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum RequestKind {
    Host,
    Suspension,
}

impl RequestId {
    fn new<H: Host>(running: &Running<'_, '_, H>, admission: u64, kind: RequestKind) -> Self {
        Self {
            vm: running.vm.number,
            stack: running.continuation.stack.control.owner(),
            admission,
            kind,
        }
    }

    pub(super) fn host<H: Host>(running: &Running<'_, '_, H>, admission: u64) -> Self {
        Self::new(running, admission, RequestKind::Host)
    }

    pub(super) fn suspension<H: Host>(running: &Running<'_, '_, H>, admission: u64) -> Self {
        Self::new(running, admission, RequestKind::Suspension)
    }
}

/// Why [`Invocation::host_request`] or [`Invocation::suspension`] found no request by an ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RequestError {
    /// The ID names a request of another computation, possibly of another VM.
    OtherComputation,
    /// The ID names a suspension where a host request was asked for, or the reverse.
    OtherKind,
    /// The computation no longer waits at that request: it was answered or abandoned, the
    /// computation finished, or a guest callback is running on top of it.
    NotWaiting,
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OtherComputation => "the request ID names another computation's request",
            Self::OtherKind => "the request ID names a request of the other kind",
            Self::NotWaiting => "the computation no longer waits at this request",
        })
    }
}

impl core::error::Error for RequestError {}

impl<'vm, H: Host> Invocation<'vm, H> {
    /// Checks that `id` names a request of this invocation's computation, of `kind`.
    fn check_request(&mut self, id: RequestId, kind: RequestKind) -> Result<(), RequestError> {
        let running = self.running();
        let mine = RequestId::new(&running, id.admission, id.kind);
        if mine != id {
            return Err(RequestError::OtherComputation);
        }
        if id.kind != kind {
            return Err(RequestError::OtherKind);
        }
        Ok(())
    }

    /// Borrows the host request `id` names, if this invocation's computation still waits at it.
    ///
    /// Like [`Self::pending_host_call`], but refuses a request the computation has moved past,
    /// which an embedding answering a saved computation later would otherwise answer in its
    /// place.
    pub fn host_request(
        &mut self,
        id: RequestId,
    ) -> Result<PendingHostCall<'_, 'vm, H>, RequestError> {
        self.check_request(id, RequestKind::Host)?;
        self.pending_host_call()
            .filter(|pending| pending.admission() == id.admission)
            .ok_or(RequestError::NotWaiting)
    }

    /// Borrows the suspension `id` names, if this invocation's computation still waits at it.
    ///
    /// Like [`Self::pending_suspension`], but refuses a suspension the computation has moved
    /// past.
    pub fn suspension(
        &mut self,
        id: RequestId,
    ) -> Result<PendingSuspension<'_, 'vm, H>, RequestError> {
        self.check_request(id, RequestKind::Suspension)?;
        self.pending_suspension()
            .filter(|pending| pending.admission() == id.admission)
            .ok_or(RequestError::NotWaiting)
    }
}
