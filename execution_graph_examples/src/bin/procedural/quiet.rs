// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::{Captured, Compiled, Graph, Mesh, SceneError};
use execution_graph::{GraphError, NodeId, RunDetailReport};
use std::{error::Error, path::Path};

#[derive(Clone, Default)]
pub(super) struct Probe {}
pub(super) struct Capture;
pub(super) struct Scope;
impl Drop for Scope {
    fn drop(&mut self) {}
}
#[derive(Debug)]
pub(super) struct Lease;
#[derive(Clone, Copy)]
pub(super) enum Buffer {
    Points,
    Topology,
}
impl Probe {
    pub(super) fn enabled(&self) -> bool {
        false
    }
    pub(super) fn edit(&self, _: &'static str) -> Scope {
        Scope
    }
    pub(super) fn node(&self, _: &'static str, _: NodeId, _: u64, _: usize, _: bool) -> Scope {
        Scope
    }
    pub(super) fn host(&self) -> Scope {
        Scope
    }
    pub(super) fn outcome(&self, _: NodeId, _: u64, _: bool) {}
    pub(super) fn buffer(&self, _: Buffer, _: usize) -> Lease {
        Lease
    }
    pub(super) fn root(&self, _: &'static str, _: u64, _: Option<&Mesh>) {}
    pub(super) fn completed(&self, _: &Graph, _: &Result<RunDetailReport, GraphError<SceneError>>) {
    }
    pub(super) fn checkpoint(&self, _: &'static str) {}
    pub(super) fn recording(&self, _: NodeId, _: u64) {}
}
impl Capture {
    pub(super) fn start(path: Option<&Path>) -> Result<Self, Box<dyn Error>> {
        if path.is_some() {
            return Err("capture requires --features footfall".into());
        }
        Ok(Self)
    }
    pub(super) fn probe(&self) -> Probe {
        Probe {}
    }
    pub(super) fn flush(&mut self) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
    pub(super) fn finish(self, _: &Compiled, _: &[Captured]) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
}
