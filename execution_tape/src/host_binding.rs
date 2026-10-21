// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Validate host imports once, then dispatch through a program-local callback table.
//!
//! [`HostFunctions`] owns operation definitions. Binding checks every declared import against
//! full argument/return types and produces immutable [`HostBindings`]. Each [`BoundHost`] supplies
//! independent mutable state while borrowing that table. The ordinary [`Host`] ABI, VM return
//! validation, fuel charging, tracing, and access recording continue to apply.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::fmt;

use crate::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef, sig_hash};
use crate::program::{HostSigId, Program};
use crate::value::Value;
use crate::verifier::VerifiedProgram;

/// A host operation with mutable per-instance state and the ordinary host ABI.
pub type HostCallback<S> =
    fn(&mut S, &[ValueRef<'_>], &mut [Value], HostContext<'_, '_>) -> Result<u64, HostError>;

struct Operation<S> {
    signature: HostSig,
    callback: HostCallback<S>,
}

type Overloads<S> = BTreeMap<SigHash, Operation<S>>;

/// Host operation definitions, supporting multiple signatures for one symbol.
///
/// Registration rejects duplicate signatures and hash collisions. Binding snapshots callback
/// choices; later registration does not change existing bindings. Rebind to adopt a new set of
/// implementations. No host state or program reference is retained by this registry.
pub struct HostFunctions<S> {
    operations: BTreeMap<Box<str>, Overloads<S>>,
}

impl<S> Default for HostFunctions<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> fmt::Debug for HostFunctions<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostFunctions")
            .field("symbol_count", &self.operations.len())
            .finish_non_exhaustive()
    }
}

impl<S> HostFunctions<S> {
    /// Creates an empty operation registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            operations: BTreeMap::new(),
        }
    }

    /// Registers one implementation, rejecting duplicates or collisions before replacement.
    pub fn register(
        &mut self,
        symbol: impl Into<Box<str>>,
        signature: HostSig,
        callback: HostCallback<S>,
    ) -> Result<(), RegistrationError> {
        let hash = sig_hash(&signature);
        self.insert(symbol.into(), hash, signature, callback)
    }

    fn insert(
        &mut self,
        symbol: Box<str>,
        hash: SigHash,
        signature: HostSig,
        callback: HostCallback<S>,
    ) -> Result<(), RegistrationError> {
        if let Some(previous) = self
            .operations
            .get(symbol.as_ref())
            .and_then(|s| s.get(&hash))
        {
            return Err(if previous.signature == signature {
                RegistrationError::DuplicateSignature {
                    symbol,
                    sig_hash: hash,
                }
            } else {
                RegistrationError::HashCollision {
                    symbol,
                    sig_hash: hash,
                }
            });
        }
        self.operations.entry(symbol).or_default().insert(
            hash,
            Operation {
                signature,
                callback,
            },
        );
        Ok(())
    }

    /// Validates every declared import, including unused imports, before producing bindings.
    ///
    /// Failures contain all missing symbols and full-signature mismatches in import-ID order.
    /// Success owns a callback table and borrows only `program`; the registry may then be dropped.
    /// Binding performs no host calls and needs no mutable host state.
    pub fn bind<'program>(
        &self,
        program: &'program VerifiedProgram,
    ) -> Result<HostBindings<'program, S>, BindingError> {
        let source = program.program();
        let mut callbacks = Vec::with_capacity(source.host_sigs.len());
        let mut errors = Vec::new();
        for (index, import) in source.host_sigs.iter().enumerate() {
            let symbol = source
                .symbol_str(import.symbol)
                .expect("verified host symbol");
            let args = source
                .host_sig_args(import)
                .expect("verified host arguments");
            let rets = source.host_sig_rets(import).expect("verified host returns");
            let overloads = self.operations.get(symbol);
            if let Some(operation) = overloads.and_then(|s| s.get(&import.sig_hash))
                && operation.signature.args == args
                && operation.signature.rets == rets
            {
                callbacks.push(operation.callback);
                continue;
            }
            errors.push(ImportError {
                import: HostSigId(u32::try_from(index).expect("verified import index")),
                symbol: Box::from(symbol),
                kind: if overloads.is_some() {
                    ImportErrorKind::SignatureMismatch
                } else {
                    ImportErrorKind::UnknownSymbol
                },
                expected: HostSig {
                    args: args.to_vec(),
                    rets: rets.to_vec(),
                },
                available: overloads.map_or_else(Vec::new, |s| {
                    s.values().map(|op| op.signature.clone()).collect()
                }),
            });
        }
        if !errors.is_empty() {
            return Err(BindingError { imports: errors });
        }
        Ok(HostBindings {
            program: source,
            callbacks: callbacks.into_boxed_slice(),
        })
    }
}

/// Immutable callback choices validated for one verified program.
///
/// Each instance borrows the same table without copying callbacks or signatures. Bindings refer
/// to the original program allocation: an equal cloned program is a different binding domain.
pub struct HostBindings<'program, S> {
    program: &'program Program,
    callbacks: Box<[HostCallback<S>]>,
}

impl<S> fmt::Debug for HostBindings<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostBindings")
            .field("import_count", &self.callbacks.len())
            .finish_non_exhaustive()
    }
}

impl<S> HostBindings<'_, S> {
    /// Creates a host instance with independent mutable state, borrowing this binding table.
    #[must_use]
    pub fn with_state(&self, state: S) -> BoundHost<'_, S> {
        BoundHost {
            program: self.program,
            callbacks: &self.callbacks,
            state,
        }
    }

    /// Returns the number of validated import slots.
    #[must_use]
    pub fn import_count(&self) -> usize {
        self.callbacks.len()
    }
}

/// Per-VM host state using a validated callback table.
///
/// Dispatch checks program identity before indexing the active import. Using this host with a
/// different program traps with [`HostError::ProgramMismatch`] before invoking a callback.
/// Argument/return buffers still follow [`Host::call`], including borrowed aggregate arguments.
pub struct BoundHost<'bindings, S> {
    program: &'bindings Program,
    callbacks: &'bindings [HostCallback<S>],
    state: S,
}

impl<S> fmt::Debug for BoundHost<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundHost")
            .field("import_count", &self.callbacks.len())
            .finish_non_exhaustive()
    }
}

impl<S> BoundHost<'_, S> {
    /// Returns this instance's state; accessible through [`crate::vm::Vm::host`] between runs.
    #[must_use]
    pub fn state(&self) -> &S {
        &self.state
    }

    /// Returns mutable instance state; accessible through [`crate::vm::Vm::host_mut`] between runs.
    pub fn state_mut(&mut self) -> &mut S {
        &mut self.state
    }
}

impl<S> Host for BoundHost<'_, S> {
    fn call(
        &mut self,
        _symbol: &str,
        _sig_hash: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        if !core::ptr::eq(self.program, ctx.program()) {
            return Err(HostError::ProgramMismatch);
        }
        let callback = self
            .callbacks
            .get(ctx.host_sig_id().0 as usize)
            .ok_or(HostError::UnknownSymbol)?;
        callback(&mut self.state, args, rets, ctx)
    }
}

/// Registration rejected without replacing the previous implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistrationError {
    /// The symbol already has this complete signature.
    DuplicateSignature {
        /// Symbol being registered.
        symbol: Box<str>,
        /// Signature hash shared by the registrations.
        sig_hash: SigHash,
    },
    /// Distinct signatures for the same symbol have the same hash.
    HashCollision {
        /// Symbol being registered.
        symbol: Box<str>,
        /// Conflicting signature hash.
        sig_hash: SigHash,
    },
}

impl fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateSignature { symbol, .. } => {
                write!(f, "duplicate host signature for {symbol}")
            }
            Self::HashCollision { symbol, .. } => {
                write!(f, "host signature hash collision for {symbol}")
            }
        }
    }
}

impl core::error::Error for RegistrationError {}

/// Why a declared import could not be bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImportErrorKind {
    /// No implementation has this symbol.
    UnknownSymbol,
    /// The symbol exists but none of its complete signatures match.
    SignatureMismatch,
}

/// One import that could not be bound, including expected and available signatures.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImportError {
    /// Program-local import index.
    pub import: HostSigId,
    /// Requested host symbol.
    pub symbol: Box<str>,
    /// Machine-readable failure category.
    pub kind: ImportErrorKind,
    /// Complete signature required by the program.
    pub expected: HostSig,
    /// Signatures available for the symbol; empty when the symbol is unknown.
    pub available: Vec<HostSig>,
}

/// All imports rejected by one binding attempt, in program declaration order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingError {
    /// Missing or mismatched imports. A failed binding always has at least one entry.
    pub imports: Vec<ImportError>,
}

impl fmt::Display for BindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} host imports could not be bound", self.imports.len())?;
        for import in &self.imports {
            write!(
                f,
                "; import {} ({}) {:?}",
                import.import.0, import.symbol, import.kind
            )?;
        }
        Ok(())
    }
}

impl core::error::Error for BindingError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::{Asm, FunctionSig, ProgramBuilder};
    use crate::host::{AccessSink, ResourceKeyRef};
    use crate::program::ValueType;
    use crate::trace::TraceMask;
    use crate::value::FuncId;
    use crate::vm::{ExecutionContext, Limits, Trap, Vm};
    use alloc::vec;

    fn signature() -> HostSig {
        HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        }
    }

    fn increment(
        state: &mut i64,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        mut ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let [ValueRef::I64(value)] = args else {
            return Err(HostError::Failed);
        };
        *state += 1;
        ctx.record_read(ResourceKeyRef::Input("counter"));
        rets[0] = Value::I64(value + *state);
        Ok(0)
    }

    fn program(symbol: &str) -> VerifiedProgram {
        let mut pb = ProgramBuilder::new();
        let sig = pb.host_sig_for(symbol, signature());
        let mut body = Asm::new();
        body.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::I64]);
        body.host_call_sig_id(0, sig, 0, &[1], &[2]);
        body.ret(0, &[2]);
        pb.push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        pb.build_verified().unwrap()
    }

    #[test]
    fn binding_reports_all_imports_with_full_expected_and_available_signatures() {
        let mut pb = ProgramBuilder::new();
        let missing = pb.host_sig_for("missing", signature());
        let mismatch = pb.host_sig_for("overloaded", signature());
        pb.host_sig_for("present", signature());
        // All imports are unused; binding validates the complete declared contract.
        let p = pb.build_verified().unwrap();
        let mut functions = HostFunctions::new();
        functions
            .register("present", signature(), increment)
            .unwrap();
        let available = HostSig {
            args: vec![ValueType::Bool],
            rets: vec![ValueType::I64],
        };
        functions
            .register("overloaded", available.clone(), increment)
            .unwrap();
        let error = functions.bind(&p).unwrap_err();
        assert_eq!(error.imports.len(), 2);
        assert_eq!(error.imports[0].import, missing);
        assert_eq!(error.imports[0].kind, ImportErrorKind::UnknownSymbol);
        assert_eq!(error.imports[0].expected, signature());
        assert!(error.imports[0].available.is_empty());
        assert_eq!(error.imports[1].import, mismatch);
        assert_eq!(error.imports[1].kind, ImportErrorKind::SignatureMismatch);
        assert_eq!(error.imports[1].available, [available]);
        assert_eq!(error.imports[1].expected, signature());
        assert!(alloc::format!("{error}").contains("missing"));
    }

    #[test]
    fn bound_instances_share_callbacks_but_keep_independent_state_and_access_events() {
        #[derive(Default)]
        struct Reads(usize);
        impl AccessSink for Reads {
            fn read(&mut self, key: ResourceKeyRef<'_>) {
                assert_eq!(key, ResourceKeyRef::Input("counter"));
                self.0 += 1;
            }
            fn write(&mut self, _: ResourceKeyRef<'_>) {
                panic!("unexpected write");
            }
        }
        let p = program("increment");
        let bindings = {
            let mut functions = HostFunctions::new();
            functions
                .register("increment", signature(), increment)
                .unwrap();
            functions.bind(&p).unwrap()
        }; // Binding no longer borrows the definition registry.
        assert_eq!(bindings.import_count(), 1);
        let mut a = Vm::new(bindings.with_state(0), Limits::default());
        let mut b = Vm::new(bindings.with_state(100), Limits::default());
        assert_eq!(
            a.run(&p, FuncId(0), &[Value::I64(40)], TraceMask::NONE, None)
                .unwrap(),
            [Value::I64(41)]
        );
        assert_eq!(
            b.run(&p, FuncId(0), &[Value::I64(40)], TraceMask::NONE, None)
                .unwrap(),
            [Value::I64(141)]
        );
        assert_eq!(*a.host().state(), 1);
        *a.host_mut().state_mut() = 10;
        let mut ctx = ExecutionContext::new();
        let mut outputs = Vec::new();
        let mut reads = Reads::default();
        a.run_with_ctx(
            &mut ctx,
            &p,
            FuncId(0),
            &[Value::I64(40)],
            &mut outputs,
            TraceMask::NONE,
            None,
            Some(&mut reads),
        )
        .unwrap();
        assert_eq!(outputs, [Value::I64(51)]);
        assert_eq!(reads.0, 1);
        assert_eq!(*b.host().state(), 101);
    }

    #[test]
    fn program_identity_is_checked_before_a_callback_can_run() {
        let p = program("increment");
        let clone = p.clone();
        let other = program("different");
        let mut functions = HostFunctions::new();
        functions
            .register("increment", signature(), increment)
            .unwrap();
        let bindings = functions.bind(&p).unwrap();
        let mut vm = Vm::new(bindings.with_state(0), Limits::default());
        for wrong in [&clone, &other] {
            let error = vm
                .run(wrong, FuncId(0), &[Value::I64(1)], TraceMask::NONE, None)
                .unwrap_err();
            assert!(matches!(
                error.trap,
                Trap::HostCallFailed {
                    error: HostError::ProgramMismatch,
                    ..
                }
            ));
            assert_eq!(*vm.host().state(), 0);
        }
        assert_eq!(
            vm.run(&p, FuncId(0), &[Value::I64(1)], TraceMask::NONE, None)
                .unwrap(),
            [Value::I64(2)]
        );
    }

    #[test]
    fn duplicates_and_hash_collisions_do_not_replace_callbacks_and_overloads_bind() {
        fn fail(
            _: &mut i64,
            _: &[ValueRef<'_>],
            _: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            Err(HostError::Failed)
        }
        let mut functions = HostFunctions::new();
        functions
            .register("increment", signature(), increment)
            .unwrap();
        assert!(matches!(
            functions.register("increment", signature(), fail),
            Err(RegistrationError::DuplicateSignature { .. })
        ));
        let overload = HostSig {
            args: vec![ValueType::Bool],
            rets: vec![ValueType::I64],
        };
        assert!(matches!(
            functions.insert(
                "increment".into(),
                sig_hash(&signature()),
                overload.clone(),
                fail
            ),
            Err(RegistrationError::HashCollision { .. })
        ));
        functions.register("increment", overload, fail).unwrap();
        let p = program("increment");
        let mut colliding = HostFunctions::new();
        colliding
            .insert(
                "increment".into(),
                sig_hash(&signature()),
                HostSig {
                    args: vec![ValueType::Bool],
                    rets: vec![ValueType::I64],
                },
                fail,
            )
            .unwrap();
        assert_eq!(
            colliding.bind(&p).unwrap_err().imports[0].kind,
            ImportErrorKind::SignatureMismatch
        );
        let bindings = functions.bind(&p).unwrap();
        let mut vm = Vm::new(bindings.with_state(0), Limits::default());
        assert_eq!(
            vm.run(&p, FuncId(0), &[Value::I64(1)], TraceMask::NONE, None)
                .unwrap(),
            [Value::I64(2)]
        );
    }

    #[test]
    fn callbacks_keep_runtime_return_validation_and_fuel_charging() {
        fn wrong_type(
            _: &mut (),
            _: &[ValueRef<'_>],
            rets: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            rets[0] = Value::Bool(true);
            Ok(0)
        }
        fn costly(
            _: &mut (),
            args: &[ValueRef<'_>],
            rets: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            rets[0] = args[0].to_value();
            Ok(1_000_000)
        }
        let p = program("increment");
        for (callback, fuel_trap) in [(wrong_type as HostCallback<()>, false), (costly, true)] {
            let mut functions = HostFunctions::new();
            functions
                .register("increment", signature(), callback)
                .unwrap();
            let bindings = functions.bind(&p).unwrap();
            let error = Vm::new(bindings.with_state(()), Limits::default())
                .run(&p, FuncId(0), &[Value::I64(1)], TraceMask::NONE, None)
                .unwrap_err();
            if fuel_trap {
                assert!(matches!(error.trap, Trap::FuelExceeded));
            } else {
                assert!(matches!(
                    error.trap,
                    Trap::TypeMismatch {
                        expected: ValueType::I64,
                        actual: ValueType::Bool
                    }
                ));
            }
        }
    }
}
