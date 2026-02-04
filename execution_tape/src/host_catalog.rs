// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Generic host catalog for registering host-call signatures.
//!
//! This module provides a lightweight registry abstraction for host-call
//! symbols and their argument/return types. It does **not** implement any
//! host functionality. It only registers signatures into a `ProgramBuilder`
//! and returns a stable lookup table of `HostSigId`s by `(symbol, sig_hash)`.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::asm::ProgramBuilder;
use crate::host::{HostSig, SigHash, sig_hash_slices};
use crate::program::{HostSigId, ValueType};

/// A host-call signature specification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSpec {
    /// Fully-qualified host symbol (stable across toolchains).
    pub symbol: Box<str>,
    /// Argument types for the host call.
    pub args: Box<[ValueType]>,
    /// Return types for the host call.
    pub rets: Box<[ValueType]>,
}

type SigSpec = (Box<[ValueType]>, Box<[ValueType]>);
type SigSpecMap = BTreeMap<SigHash, SigSpec>;
type SymbolSpecMap = BTreeMap<Box<str>, SigSpecMap>;
type SigIdMap = BTreeMap<SigHash, HostSigId>;
type SymbolIdMap = BTreeMap<Box<str>, SigIdMap>;

impl HostSpec {
    /// Construct a new host spec from symbol + arg/ret types.
    pub fn new(symbol: impl Into<Box<str>>, args: &[ValueType], rets: &[ValueType]) -> Self {
        Self {
            symbol: symbol.into(),
            args: Box::from(args),
            rets: Box::from(rets),
        }
    }
}

/// A collection of host-call signatures to be registered.
///
/// # Example
/// ```
/// extern crate alloc;
///
/// use alloc::vec;
///
/// use execution_tape::asm::ProgramBuilder;
/// use execution_tape::host_catalog::{HostCatalog, HostSpec};
/// use execution_tape::program::ValueType;
///
/// let mut cat = HostCatalog::new();
/// cat.push(HostSpec::new(
///     "geom::aabb3_from_size/3/1",
///     &[ValueType::F64, ValueType::F64, ValueType::F64],
///     &[ValueType::I64],
/// ));
///
/// let mut pb = ProgramBuilder::new();
/// let reg = cat.register_all(&mut pb).unwrap();
/// assert!(reg
///     .sig_id_for(
///         "geom::aabb3_from_size/3/1",
///         &[ValueType::F64, ValueType::F64, ValueType::F64],
///         &[ValueType::I64],
///     )
///     .is_some());
/// ```
#[derive(Clone, Debug, Default)]
pub struct HostCatalog {
    specs: Vec<HostSpec>,
}

impl HostCatalog {
    /// Create an empty catalog.
    pub fn new() -> Self {
        Self { specs: Vec::new() }
    }

    /// Add a host spec.
    pub fn push(&mut self, spec: HostSpec) {
        self.specs.push(spec);
    }

    /// Add multiple specs.
    pub fn extend<I: IntoIterator<Item = HostSpec>>(&mut self, iter: I) {
        self.specs.extend(iter);
    }

    /// Iterate over all specs.
    pub fn iter(&self) -> impl Iterator<Item = &HostSpec> {
        self.specs.iter()
    }

    /// Register all specs into `ProgramBuilder` and return a symbol registry.
    ///
    /// This consumes the catalog to avoid cloning argument/return type vectors.
    /// Registrations are performed in a deterministic order (sorted by symbol, then sig hash).
    /// Any duplicate `(symbol, sig_hash)` entries are rejected, and hash collisions are treated
    /// as errors.
    pub fn register_all(
        self,
        pb: &mut ProgramBuilder,
    ) -> Result<HostSigRegistry, HostCatalogError> {
        let mut specs_by_symbol: SymbolSpecMap = BTreeMap::new();

        // Pass 1: build a deterministic map keyed by (symbol, sig_hash),
        // validating duplicates and hash collisions as we go.
        for HostSpec { symbol, args, rets } in self.specs {
            let sig_hash = sig_hash_slices(args.as_ref(), rets.as_ref());
            if let Some((existing_args, existing_rets)) = specs_by_symbol
                .get(symbol.as_ref())
                .and_then(|sigs| sigs.get(&sig_hash))
            {
                if existing_args.as_ref() == args.as_ref()
                    && existing_rets.as_ref() == rets.as_ref()
                {
                    return Err(HostCatalogError::DuplicateSignature { symbol, sig_hash });
                }
                return Err(HostCatalogError::HashCollision { symbol, sig_hash });
            }

            specs_by_symbol
                .entry(symbol)
                .or_default()
                .insert(sig_hash, (args, rets));
        }

        let mut by_symbol: SymbolIdMap = BTreeMap::new();
        // Pass 2: register signatures in deterministic order from the map.
        for (symbol, sigs) in specs_by_symbol {
            let mut ids: SigIdMap = BTreeMap::new();
            for (sig_hash, (args, rets)) in sigs {
                let sig_id = pb.host_sig_for(
                    &symbol,
                    HostSig {
                        args: Vec::from(args),
                        rets: Vec::from(rets),
                    },
                );
                ids.insert(sig_hash, sig_id);
            }
            by_symbol.insert(symbol, ids);
        }

        Ok(HostSigRegistry { by_symbol })
    }
}

/// Lookup registry for host signature ids.
#[derive(Clone, Debug, Default)]
pub struct HostSigRegistry {
    by_symbol: SymbolIdMap,
}

impl HostSigRegistry {
    /// Look up a host signature id by `(symbol, sig_hash)`.
    pub fn sig_id(&self, symbol: &str, sig_hash: SigHash) -> Option<HostSigId> {
        self.by_symbol
            .get(symbol)
            .and_then(|sigs| sigs.get(&sig_hash))
            .copied()
    }

    /// Look up a host signature id by `(symbol, args, rets)`.
    pub fn sig_id_for(
        &self,
        symbol: &str,
        args: &[ValueType],
        rets: &[ValueType],
    ) -> Option<HostSigId> {
        let sig_hash = sig_hash_slices(args, rets);
        self.sig_id(symbol, sig_hash)
    }
}

/// Errors when registering a host catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCatalogError {
    /// A `(symbol, sig_hash)` appeared more than once in the catalog.
    DuplicateSignature {
        /// Host symbol string.
        symbol: Box<str>,
        /// Signature hash for the duplicate entry.
        sig_hash: SigHash,
    },
    /// A `(symbol, sig_hash)` collision was detected with different signatures.
    HashCollision {
        /// Host symbol string.
        symbol: Box<str>,
        /// Colliding signature hash.
        sig_hash: SigHash,
    },
}

impl core::fmt::Display for HostCatalogError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DuplicateSignature { symbol, sig_hash } => write!(
                f,
                "duplicate host signature for '{symbol}' (sig_hash=0x{:016x})",
                sig_hash.0
            ),
            Self::HashCollision { symbol, sig_hash } => write!(
                f,
                "host signature hash collision for '{symbol}' (sig_hash=0x{:016x})",
                sig_hash.0
            ),
        }
    }
}

impl core::error::Error for HostCatalogError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_all_rejects_duplicate_signatures() {
        let mut cat = HostCatalog::new();
        cat.push(HostSpec::new("foo", &[ValueType::I64], &[ValueType::F64]));
        cat.push(HostSpec::new("foo", &[ValueType::I64], &[ValueType::F64]));

        let mut pb = ProgramBuilder::new();
        let err = cat.register_all(&mut pb).unwrap_err();
        assert_eq!(
            err,
            HostCatalogError::DuplicateSignature {
                symbol: "foo".into(),
                sig_hash: sig_hash_slices(&[ValueType::I64], &[ValueType::F64]),
            }
        );
    }

    #[test]
    fn register_all_allows_overloads() {
        let mut cat = HostCatalog::new();
        cat.push(HostSpec::new("foo", &[ValueType::I64], &[ValueType::F64]));
        cat.push(HostSpec::new("foo", &[ValueType::F64], &[ValueType::I64]));

        let mut pb = ProgramBuilder::new();
        let reg = cat.register_all(&mut pb).expect("register");

        assert!(
            reg.sig_id_for("foo", &[ValueType::I64], &[ValueType::F64])
                .is_some()
        );
        assert!(
            reg.sig_id_for("foo", &[ValueType::F64], &[ValueType::I64])
                .is_some()
        );
    }

    #[test]
    fn register_all_returns_registry() {
        let mut cat = HostCatalog::new();
        cat.push(HostSpec::new("foo", &[ValueType::I64], &[ValueType::F64]));
        cat.push(HostSpec::new("bar", &[ValueType::F64], &[ValueType::I64]));

        let mut pb = ProgramBuilder::new();
        let reg = cat.register_all(&mut pb).expect("register");

        assert!(
            reg.sig_id_for("foo", &[ValueType::I64], &[ValueType::F64])
                .is_some()
        );
        assert!(
            reg.sig_id_for("bar", &[ValueType::F64], &[ValueType::I64])
                .is_some()
        );
        assert!(
            reg.sig_id_for("baz", &[ValueType::I64], &[ValueType::F64])
                .is_none()
        );
    }

    #[test]
    fn register_all_is_deterministic() {
        let mut cat_a = HostCatalog::new();
        cat_a.push(HostSpec::new("foo", &[ValueType::I64], &[ValueType::F64]));
        cat_a.push(HostSpec::new("bar", &[ValueType::F64], &[ValueType::I64]));

        let mut cat_b = HostCatalog::new();
        cat_b.push(HostSpec::new("bar", &[ValueType::F64], &[ValueType::I64]));
        cat_b.push(HostSpec::new("foo", &[ValueType::I64], &[ValueType::F64]));

        let mut pb_a = ProgramBuilder::new();
        let reg_a = cat_a.register_all(&mut pb_a).expect("register");

        let mut pb_b = ProgramBuilder::new();
        let reg_b = cat_b.register_all(&mut pb_b).expect("register");

        assert_eq!(
            reg_a.sig_id_for("bar", &[ValueType::F64], &[ValueType::I64]),
            reg_b.sig_id_for("bar", &[ValueType::F64], &[ValueType::I64])
        );
        assert_eq!(
            reg_a.sig_id_for("foo", &[ValueType::I64], &[ValueType::F64]),
            reg_b.sig_id_for("foo", &[ValueType::I64], &[ValueType::F64])
        );
    }
}
