// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Adapter for translating `execution_tape` access events into `execution_graph` keys.

use execution_tape::host::{AccessSink, ResourceKeyRef};

use crate::access::{Access, AccessLog, HostOpId, ResourceKey};

/// Records `execution_tape` host access events as `execution_graph` [`Access`] entries.
#[derive(Clone, Debug, Default)]
pub(crate) struct TapeAccessLog {
    log: AccessLog,
}

impl TapeAccessLog {
    /// Creates an empty access log.
    #[must_use]
    #[inline]
    pub(crate) const fn new() -> Self {
        Self {
            log: AccessLog::new(),
        }
    }

    /// Returns the recorded access log.
    #[must_use]
    #[inline]
    pub(crate) fn log(&self) -> &AccessLog {
        &self.log
    }

    #[inline]
    fn map_key(key: ResourceKeyRef<'_>) -> ResourceKey {
        match key {
            ResourceKeyRef::Input(name) => ResourceKey::input(name),
            ResourceKeyRef::HostState { op, key } => {
                ResourceKey::host_state(HostOpId::new(op.0), key)
            }
            ResourceKeyRef::OpaqueHost { op } => ResourceKey::opaque_host(HostOpId::new(op.0)),
        }
    }
}

impl AccessSink for TapeAccessLog {
    fn read(&mut self, key: ResourceKeyRef<'_>) {
        self.log.push(Access::read(Self::map_key(key)));
    }

    fn write(&mut self, key: ResourceKeyRef<'_>) {
        self.log.push(Access::write(Self::map_key(key)));
    }
}
