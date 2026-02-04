// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Internal analysis utilities (CFG, dataflow).
//!
//! This module is crate-internal: it exists to keep verifier and tooling analyses coherent and
//! reusable while the design is still evolving.

pub(crate) mod bitset;
pub(crate) mod cfg;
pub(crate) mod liveness;
