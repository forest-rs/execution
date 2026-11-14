// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Validate host imports once, then dispatch through a program-local callback table.
//!
//! [`HostFunctions`] owns operation definitions. Binding checks every declared import against
//! full argument/return types and produces immutable [`HostBindings`], with one callback table
//! per bound program, so units installed from any bound program can call the same host. Each
//! [`BoundHost`] supplies independent mutable state while sharing those tables. The ordinary
//! [`Host`] ABI, VM return validation, fuel charging, tracing, and access recording continue to
//! apply.
//!
//! [`HostFunctions::bind_checked`] also hands every host call's statically known arguments to an
//! embedder check, so constant arguments such as names are validated before anything runs.
//! [`VerifiedProgram::check_host_calls`] runs the same checks against a list of [`HostImport`]s,
//! for a host that dispatches without registered callbacks.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use crate::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef, sig_hash};
use crate::program::{HostSigId, Program, SpanId, ValueType};
use crate::value::{FuncId, Value};
use crate::verifier::{HostCallSite, VerifiedProgram, write_location};

/// A host operation's symbol and complete signature, declared once and shared by the program
/// that imports it and the host that implements it, so the two cannot drift.
///
/// A `const` declares it; [`ProgramBuilder::host_import`] adds it to a program's import table,
/// [`HostFunctions::implement`] registers its callback, and [`HostCallSite::calls`] recognizes
/// its calls in a link check.
///
/// The `const`s are also patterns. A host that dispatches with `match` rather than registered
/// callbacks resolves each call with [`HostContext::import_in`], and a link check each call site
/// with [`HostCallSite::import_in`], then matches the result against the same `const`s, which
/// compares the symbol and the complete signature. No separate symbol strings are needed.
///
/// ```
/// use std::sync::Arc;
///
/// use execution_tape::asm::{FunctionSig, ProgramBuilder};
/// use execution_tape::host::{HostContext, HostError, ValueRef};
/// use execution_tape::host_binding::{HostFunctions, HostImport};
/// use execution_tape::program::ValueType;
/// use execution_tape::value::Value;
///
/// const LOG: HostImport = HostImport::new("app.log", &[ValueType::Str], &[]);
///
/// fn log(lines: &mut Vec<String>, args: &[ValueRef<'_>], _: &mut [Value], _: HostContext<'_, '_>) -> Result<u64, HostError> {
///     let [ValueRef::Str(line)] = args else { return Err(HostError::Failed) };
///     lines.push((*line).into());
///     Ok(0)
/// }
/// let mut functions = HostFunctions::new();
/// functions.implement(LOG, log)?;
///
/// let mut pb = ProgramBuilder::new();
/// let entry = pb.declare_function(FunctionSig { arg_types: vec![], ret_types: vec![] })?;
/// let mut body = pb.function_asm(entry)?;
/// let log_import = pb.host_import(LOG);
/// let [] = pb.emit_call_with(&mut body, log_import, ("hello",))?;
/// body.ret(0, &[]);
/// pb.define_function(entry, body)?;
/// let program = Arc::new(pb.build_verified()?);
/// functions.bind(&program)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// [`ProgramBuilder::host_import`]: crate::asm::ProgramBuilder::host_import
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostImport {
    symbol: &'static str,
    args: &'static [ValueType],
    rets: &'static [ValueType],
}

impl HostImport {
    /// Declares `symbol` with argument and return types.
    #[must_use]
    pub const fn new(
        symbol: &'static str,
        args: &'static [ValueType],
        rets: &'static [ValueType],
    ) -> Self {
        Self { symbol, args, rets }
    }

    /// The host symbol.
    #[must_use]
    pub const fn symbol(&self) -> &'static str {
        self.symbol
    }

    /// The argument types.
    #[must_use]
    pub const fn args(&self) -> &'static [ValueType] {
        self.args
    }

    /// The return types.
    #[must_use]
    pub const fn rets(&self) -> &'static [ValueType] {
        self.rets
    }

    /// Returns `true` when `program` declares this as its import `id`: the same symbol and
    /// complete signature.
    pub(crate) fn is_declared_as(&self, program: &Program, id: HostSigId) -> bool {
        let Some(entry) = program.host_sigs.get(id.0 as usize) else {
            return false;
        };
        program.symbol_str(entry.symbol).ok() == Some(self.symbol)
            && program.host_sig_args(entry).ok() == Some(self.args)
            && program.host_sig_rets(entry).ok() == Some(self.rets)
    }

    /// Returns the import in `surface` that `program` declares as its import `id`.
    pub(crate) fn find(surface: &[Self], program: &Program, id: HostSigId) -> Option<Self> {
        surface
            .iter()
            .find(|import| import.is_declared_as(program, id))
            .copied()
    }

    /// The complete signature, as [`HostFunctions::register`] and
    /// [`ProgramBuilder::host_sig_for`](crate::asm::ProgramBuilder::host_sig_for) take it.
    #[must_use]
    pub fn signature(&self) -> HostSig {
        HostSig {
            args: self.args.to_vec(),
            rets: self.rets.to_vec(),
        }
    }
}

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

    /// Registers the implementation of `import`, as [`Self::register`] does with its symbol and
    /// signature.
    pub fn implement(
        &mut self,
        import: HostImport,
        callback: HostCallback<S>,
    ) -> Result<(), RegistrationError> {
        self.register(import.symbol, import.signature(), callback)
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
    /// Success owns a callback table and shares `program`; the registry may then be dropped.
    /// Binding performs no host calls and needs no mutable host state. Units installed from this
    /// same allocation dispatch through the table, so install `program` itself.
    pub fn bind(&self, program: &Arc<VerifiedProgram>) -> Result<HostBindings<S>, BindingError> {
        Ok(HostBindings {
            tables: Arc::from([self.table(program)?]),
        })
    }

    /// Binds `program` as [`Self::bind`] does, and also passes every reachable host call to
    /// `check` with the arguments known before the program runs.
    ///
    /// This is where an embedder validates constant arguments, such as names, against its own
    /// schema, so a wrong one is rejected at load rather than when the call executes. Only facts
    /// true on every path reach `check`; see [`VerifiedProgram::host_call_sites`]. Calls to an
    /// import that fails to bind are not checked. Failures contain every rejected import and
    /// every rejected call, each call with its location and `check`'s message.
    /// [`VerifiedProgram::check_host_calls`] runs the same checks against declared imports,
    /// without callbacks.
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use execution_tape::asm::{FunctionSig, ProgramBuilder, Reg};
    /// use execution_tape::host::{HostContext, HostError, HostSig, ValueRef};
    /// use execution_tape::host_binding::HostFunctions;
    /// use execution_tape::program::ValueType;
    /// use execution_tape::value::Value;
    ///
    /// fn set(_: &mut (), _: &[ValueRef<'_>], _: &mut [Value], _: HostContext<'_, '_>) -> Result<u64, HostError> {
    ///     Ok(0)
    /// }
    /// let signature = HostSig { args: vec![ValueType::Str, ValueType::F32], rets: vec![] };
    /// let mut functions = HostFunctions::new();
    /// functions.register("settings.set", signature.clone(), set)?;
    ///
    /// let mut pb = ProgramBuilder::new();
    /// let import = pb.host_sig_for("settings.set", signature);
    /// let entry = pb.declare_function(FunctionSig { arg_types: vec![ValueType::F32], ret_types: vec![] })?;
    /// let mut body = pb.function_asm(entry)?;
    /// let value = Reg(pb.argument(entry, 0)?);
    /// let [] = pb.emit_call_with(&mut body, import, ("rustiness", value))?;
    /// body.ret(0, &[]);
    /// pb.define_function(entry, body)?;
    /// let program = Arc::new(pb.build_verified()?);
    ///
    /// let schema = ["age", "wetness"];
    /// let error = functions
    ///     .bind_checked(&program, |site| match (site.symbol(), site.arg(0)) {
    ///         ("settings.set", Some(ValueRef::Str(name))) if !schema.contains(&name) => {
    ///             Err(format!("nothing named {name:?}"))
    ///         }
    ///         ("settings.set", None) => Err("the name must be a constant".into()),
    ///         _ => Ok(()),
    ///     })
    ///     .unwrap_err();
    /// assert_eq!(
    ///     error.to_string(),
    ///     "1 host call rejected; function #0, pc 3, host call settings.set: nothing named \"rustiness\""
    /// );
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn bind_checked<E: fmt::Display>(
        &self,
        program: &Arc<VerifiedProgram>,
        check: impl FnMut(&HostCallSite<'_>) -> Result<(), E>,
    ) -> Result<HostBindings<S>, BindingError> {
        let (table, mut error) = match self.table(program) {
            Ok(table) => (Some(table), BindingError::default()),
            Err(error) => (None, error),
        };
        error.calls = check_calls(program, &error.imports, check);
        match table {
            Some(table) if error.calls.is_empty() => Ok(HostBindings {
                tables: Arc::from([table]),
            }),
            _ => Err(error),
        }
    }

    fn table(&self, program: &Arc<VerifiedProgram>) -> Result<Table<S>, BindingError> {
        let callbacks = resolve_imports(program.program(), |symbol, hash, args, rets| {
            let overloads = self.operations.get(symbol);
            match overloads.and_then(|s| s.get(&hash)) {
                Some(op) if op.signature.args == args && op.signature.rets == rets => {
                    Ok(op.callback)
                }
                _ => Err(overloads.map(|s| s.values().map(|op| op.signature.clone()).collect())),
            }
        })?;
        Ok(Table {
            program: Arc::clone(program),
            callbacks: callbacks.into_boxed_slice(),
        })
    }
}

impl VerifiedProgram {
    /// Checks every import against `imports` and passes every reachable host call to `check`,
    /// as [`HostFunctions::bind_checked`] does, without registering callbacks.
    ///
    /// Each declared import, including unused ones, must be one of `imports`: the same symbol
    /// and complete signature. This is the link check for a host that dispatches on its own,
    /// such as one matching [`HostContext::import_in`] against the same `imports`. Failures
    /// contain every rejected import and every rejected call, as [`BindingError`]s from binding
    /// do; calls to a rejected import are not checked.
    ///
    /// ```
    /// use execution_tape::asm::{FunctionSig, ProgramBuilder};
    /// use execution_tape::host::ValueRef;
    /// use execution_tape::host_binding::HostImport;
    /// use execution_tape::program::ValueType;
    ///
    /// const SET: HostImport = HostImport::new("settings.set", &[ValueType::Str], &[]);
    /// const SURFACE: [HostImport; 1] = [SET];
    ///
    /// let mut pb = ProgramBuilder::new();
    /// let set = pb.host_import(SET);
    /// let entry = pb.declare_function(FunctionSig { arg_types: vec![], ret_types: vec![] })?;
    /// let mut body = pb.function_asm(entry)?;
    /// let [] = pb.emit_call_with(&mut body, set, ("rustiness",))?;
    /// body.ret(0, &[]);
    /// pb.define_function(entry, body)?;
    /// let program = pb.build_verified()?;
    ///
    /// let error = program
    ///     .check_host_calls(&SURFACE, |site| match (site.import_in(&SURFACE), site.arg(0)) {
    ///         (Some(SET), Some(ValueRef::Str("age" | "wetness"))) => Ok(()),
    ///         (Some(SET), _) => Err("not a setting"),
    ///         _ => Ok(()),
    ///     })
    ///     .unwrap_err();
    /// assert_eq!(error.calls[0].message.as_ref(), "not a setting");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn check_host_calls<E: fmt::Display>(
        &self,
        imports: &[HostImport],
        check: impl FnMut(&HostCallSite<'_>) -> Result<(), E>,
    ) -> Result<(), BindingError> {
        let mut error = match resolve_imports(self.program(), |symbol, _, args, rets| {
            let mut same_symbol = imports.iter().filter(|i| i.symbol == symbol).peekable();
            if same_symbol.peek().is_none() {
                return Err(None);
            }
            match same_symbol
                .clone()
                .find(|i| i.args == args && i.rets == rets)
            {
                Some(_) => Ok(()),
                None => Err(Some(same_symbol.map(HostImport::signature).collect())),
            }
        }) {
            Ok(_) => BindingError::default(),
            Err(error) => error,
        };
        error.calls = check_calls(self, &error.imports, check);
        if error.imports.is_empty() && error.calls.is_empty() {
            Ok(())
        } else {
            Err(error)
        }
    }
}

/// Resolves each of `program`'s declared imports, in import-ID order, with `find`.
///
/// `find` receives the symbol, signature hash, arguments and returns, and on failure the
/// signatures available for the symbol, or `None` when the symbol is unknown.
fn resolve_imports<T>(
    program: &Program,
    mut find: impl FnMut(&str, SigHash, &[ValueType], &[ValueType]) -> Result<T, Option<Vec<HostSig>>>,
) -> Result<Vec<T>, BindingError> {
    let mut found = Vec::with_capacity(program.host_sigs.len());
    let mut errors = Vec::new();
    for (index, import) in program.host_sigs.iter().enumerate() {
        let symbol = program
            .symbol_str(import.symbol)
            .expect("verified host symbol");
        let args = program
            .host_sig_args(import)
            .expect("verified host arguments");
        let rets = program
            .host_sig_rets(import)
            .expect("verified host returns");
        match find(symbol, import.sig_hash, args, rets) {
            Ok(value) => found.push(value),
            Err(available) => errors.push(ImportError {
                import: HostSigId(u32::try_from(index).expect("verified import index")),
                symbol: Box::from(symbol),
                kind: if available.is_some() {
                    ImportErrorKind::SignatureMismatch
                } else {
                    ImportErrorKind::UnknownSymbol
                },
                expected: HostSig {
                    args: args.to_vec(),
                    rets: rets.to_vec(),
                },
                available: available.unwrap_or_default(),
            }),
        }
    }
    if errors.is_empty() {
        Ok(found)
    } else {
        Err(BindingError {
            imports: errors,
            calls: Vec::new(),
        })
    }
}

/// Passes every reachable host call of `program` to `check`, except calls to a rejected import.
fn check_calls<E: fmt::Display>(
    program: &VerifiedProgram,
    rejected: &[ImportError],
    mut check: impl FnMut(&HostCallSite<'_>) -> Result<(), E>,
) -> Vec<CallError> {
    let mut calls = Vec::new();
    for site in program.host_call_sites().iter() {
        if rejected.iter().any(|e| e.import == site.import()) {
            continue;
        }
        if let Err(message) = check(&site) {
            calls.push(CallError {
                import: site.import(),
                symbol: Box::from(site.symbol()),
                function: site.function(),
                pc: site.pc(),
                span: site.span(),
                message: alloc::string::ToString::to_string(&message).into_boxed_str(),
            });
        }
    }
    calls
}

/// The validated callbacks of one program, indexed by its import IDs.
struct Table<S> {
    program: Arc<VerifiedProgram>,
    callbacks: Box<[HostCallback<S>]>,
}

impl<S> Clone for Table<S> {
    fn clone(&self) -> Self {
        Self {
            program: Arc::clone(&self.program),
            callbacks: self.callbacks.clone(),
        }
    }
}

/// Immutable callback choices validated for one or more verified programs.
///
/// The bindings share each bound program, so they need no lifetime and can be stored beside it.
/// Each instance shares the same tables without copying callbacks or signatures. Bindings refer
/// to the bound program allocations: an equal program in another allocation is a different
/// binding domain. Every unit installed from a bound program dispatches through that program's
/// table.
pub struct HostBindings<S> {
    tables: Arc<[Table<S>]>,
}

impl<S> Clone for HostBindings<S> {
    fn clone(&self) -> Self {
        Self {
            tables: Arc::clone(&self.tables),
        }
    }
}

impl<S> fmt::Debug for HostBindings<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostBindings")
            .field("program_count", &self.tables.len())
            .field("import_count", &self.import_count())
            .finish_non_exhaustive()
    }
}

impl<S> HostBindings<S> {
    /// Validates `program`'s imports against `functions` and adds them to these bindings.
    ///
    /// Units of every bound program can then share one [`BoundHost`], as calls across installed
    /// units require. Binding a program again replaces its table. Hosts created earlier with
    /// [`Self::with_state`] keep the tables they were created with. On failure the bindings are
    /// unchanged; the error lists `program`'s rejected imports as [`HostFunctions::bind`] does.
    pub fn bind(
        &mut self,
        functions: &HostFunctions<S>,
        program: &Arc<VerifiedProgram>,
    ) -> Result<(), BindingError> {
        let table = functions.table(program)?;
        let mut tables = self.tables.to_vec();
        match tables
            .iter_mut()
            .find(|bound| Arc::ptr_eq(&bound.program, &table.program))
        {
            Some(bound) => *bound = table,
            None => tables.push(table),
        }
        self.tables = tables.into();
        Ok(())
    }

    /// Creates a host instance with independent mutable state, sharing these binding tables.
    #[must_use]
    pub fn with_state(&self, state: S) -> BoundHost<S> {
        BoundHost {
            tables: Arc::clone(&self.tables),
            state,
        }
    }

    /// Returns the number of bound programs.
    #[must_use]
    pub fn program_count(&self) -> usize {
        self.tables.len()
    }

    /// Returns the number of validated import slots across all bound programs.
    #[must_use]
    pub fn import_count(&self) -> usize {
        self.tables.iter().map(|table| table.callbacks.len()).sum()
    }
}

/// Per-VM host state using validated callback tables.
///
/// Dispatch finds the calling program's table by identity before indexing the active import.
/// A call from a program that was never bound traps with [`HostError::ProgramMismatch`] before
/// invoking a callback. Argument/return buffers still follow [`Host::call`], including borrowed
/// aggregate arguments.
pub struct BoundHost<S> {
    tables: Arc<[Table<S>]>,
    state: S,
}

impl<S> fmt::Debug for BoundHost<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundHost")
            .field("program_count", &self.tables.len())
            .finish_non_exhaustive()
    }
}

impl<S> BoundHost<S> {
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

impl<S> Host for BoundHost<S> {
    fn call(
        &mut self,
        _symbol: &str,
        _sig_hash: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let program = ctx.program();
        let table = self
            .tables
            .iter()
            .find(|table| core::ptr::eq(table.program.program(), program))
            .ok_or(HostError::ProgramMismatch)?;
        let callback = table
            .callbacks
            .get(ctx.host_sig_id().0 as usize)
            .ok_or(HostError::UnknownSymbol)?;
        callback(&mut self.state, args, rets, ctx)
    }
}

/// Registration rejected without replacing the previous implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
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

/// One host call rejected by the check passed to [`HostFunctions::bind_checked`].
///
/// [`fmt::Display`] reads like a [`VerifyError`](crate::verifier::VerifyError): the location,
/// then the symbol, then the check's message, as
/// `function #F, pc P (span S), host call SYMBOL: message`. The span is left out when the program
/// records none. [`Self::display_in`] adds the names the program's debug metadata records.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CallError {
    /// Program-local import index of the called operation.
    pub import: HostSigId,
    /// The called symbol.
    pub symbol: Box<str>,
    /// Function containing the call.
    pub function: FuncId,
    /// Bytecode offset of the call within its function.
    pub pc: u32,
    /// Source span of the call, if the program records one.
    pub span: Option<SpanId>,
    /// Why the check rejected the call.
    pub message: Box<str>,
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "function #{}, pc {}", self.function.0, self.pc)?;
        if let Some(span) = self.span {
            write!(f, " (span {span})")?;
        }
        self.fmt_call(f)
    }
}

impl CallError {
    /// Renders this rejection with what `program`'s debug metadata knows about its location, as
    /// [`VerifyError::display_in`](crate::verifier::VerifyError::display_in) does: the function
    /// name, the nearest label, the span and the instruction.
    ///
    /// ```text
    /// function #0 "paint", pc 3 (label "weathered" + 3, span 7, host_call eff_out=r0, host_sig#0("settings.set"), eff_in=r0, args=[r1], rets=[]), host call settings.set: nothing named "rustiness"
    /// ```
    ///
    /// `program` must be the program that was checked; metadata it lacks is left out.
    #[must_use]
    pub fn display_in<'a>(&'a self, program: &'a Program) -> impl fmt::Display + 'a {
        CallInProgram {
            error: self,
            program,
        }
    }

    /// Writes `, host call SYMBOL: message`, which follows the location.
    fn fmt_call(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, ", host call {}: {}", self.symbol, self.message)
    }
}

/// A [`CallError`] rendered with names from the checked program.
struct CallInProgram<'a> {
    error: &'a CallError,
    program: &'a Program,
}

impl fmt::Display for CallInProgram<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { error, program } = *self;
        write_location(f, program, error.function.0, Some(error.pc))?;
        error.fmt_call(f)
    }
}

/// Everything rejected by one binding attempt, in program order.
///
/// A failed binding always has at least one entry in `imports` or `calls`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BindingError {
    /// Missing or mismatched imports, in import-ID order.
    pub imports: Vec<ImportError>,
    /// Calls rejected by [`HostFunctions::bind_checked`], in function then bytecode order.
    pub calls: Vec<CallError>,
}

impl fmt::Display for BindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut sep = "";
        if !self.imports.is_empty() {
            write!(f, "{} host imports could not be bound", self.imports.len())?;
            sep = "; ";
        }
        for import in &self.imports {
            write!(
                f,
                "; import {} ({}) {:?}",
                import.import.0, import.symbol, import.kind
            )?;
        }
        if !self.calls.is_empty() {
            let plural = if self.calls.len() == 1 { "" } else { "s" };
            write!(f, "{sep}{} host call{plural} rejected", self.calls.len())?;
        }
        for call in &self.calls {
            write!(f, "; {call}")?;
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
    use crate::value::FuncRef;
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

    fn program(symbol: &str) -> Arc<VerifiedProgram> {
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
        Arc::new(pb.build_verified().unwrap())
    }

    #[test]
    fn binding_reports_all_imports_with_full_expected_and_available_signatures() {
        let mut pb = ProgramBuilder::new();
        let missing = pb.host_sig_for("missing", signature());
        let mismatch = pb.host_sig_for("overloaded", signature());
        pb.host_sig_for("present", signature());
        // All imports are unused; binding validates the complete declared contract.
        let p = Arc::new(pb.build_verified().unwrap());
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
    fn checked_binding_reports_rejected_calls_with_locations_and_skips_unbound_imports() {
        let str_sig = HostSig {
            args: vec![ValueType::Str],
            rets: vec![],
        };
        let mut pb = ProgramBuilder::new();
        let named = pb.host_sig_for("named", str_sig.clone());
        let missing = pb.host_sig_for("missing", str_sig.clone());
        let f = pb
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            })
            .unwrap();
        let mut a = pb.function_asm(f).unwrap();
        let mut calls = Vec::new();
        for (import, text) in [(named, "good"), (named, "bad"), (missing, "bad")] {
            let r = a.alloc_register(ValueType::Str);
            a.const_pool(r, pb.const_str(text));
            calls.push(a.pc());
            let [] = pb.emit_call(&mut a, import, &[r]).unwrap();
        }
        a.ret(0, &[]);
        pb.define_function(f, a).unwrap();
        let p = Arc::new(pb.build_verified().unwrap());

        let mut functions = HostFunctions::<()>::new();
        functions
            .register("named", str_sig, |_, _, _, _| Ok(0))
            .unwrap();
        let mut seen = Vec::new();
        let error = functions
            .bind_checked(&p, |site| {
                seen.push(site.pc());
                match site.arg(0) {
                    Some(ValueRef::Str("bad")) => Err("nothing named \"bad\""),
                    _ => Ok(()),
                }
            })
            .unwrap_err();
        // The call to the unbound import is not checked.
        assert_eq!(seen, calls[..2]);
        assert_eq!(error.imports.len(), 1);
        assert_eq!(error.imports[0].import, missing);
        assert_eq!(error.calls.len(), 1);
        let call = &error.calls[0];
        assert_eq!(
            (call.import, &*call.symbol, call.function, call.pc),
            (named, "named", FuncId(0), calls[1])
        );
        assert_eq!(
            alloc::format!("{call}"),
            alloc::format!(
                "function #0, pc {}, host call named: nothing named \"bad\"",
                calls[1]
            )
        );
        assert!(alloc::format!("{error}").contains("1 host call rejected"));

        functions
            .register(
                "missing",
                HostSig {
                    args: vec![ValueType::Str],
                    rets: vec![],
                },
                |_, _, _, _| Ok(0),
            )
            .unwrap();
        let bindings = functions.bind_checked(&p, |_| Ok::<(), &str>(())).unwrap();
        assert_eq!(bindings.import_count(), 2);
    }

    #[test]
    fn rejected_calls_read_like_verifier_diagnostics() {
        const SET: HostImport = HostImport::new("settings.set", &[ValueType::Str], &[]);
        let mut pb = ProgramBuilder::new();
        let set = pb.host_import(SET);
        let f = pb
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            })
            .unwrap();
        pb.set_function_name(f, "paint").unwrap();
        let mut a = pb.function_asm(f).unwrap();
        let weathered = a.label_named("weathered");
        a.place(weathered).unwrap();
        let r = a.alloc_register(ValueType::Str);
        a.const_pool(r, pb.const_str("rustiness"));
        let pc = a.pc();
        a.span(SpanId::try_from(7_u64).unwrap());
        let [] = pb.emit_call(&mut a, set, &[r]).unwrap();
        a.ret(0, &[]);
        pb.define_function(f, a).unwrap();
        let p = Arc::new(pb.build_verified().unwrap());

        let mut functions = HostFunctions::<()>::new();
        functions.implement(SET, |_, _, _, _| Ok(0)).unwrap();
        let error = functions
            .bind_checked(&p, |_| Err("nothing named \"rustiness\""))
            .unwrap_err();
        let [call] = &error.calls[..] else {
            panic!("{error}");
        };
        assert_eq!(call.span, Some(SpanId::try_from(7_u64).unwrap()));
        assert_eq!(
            alloc::format!("{call}"),
            alloc::format!(
                "function #0, pc {pc} (span 7), host call settings.set: nothing named \"rustiness\""
            )
        );
        assert_eq!(
            alloc::format!("{}", call.display_in(p.program())),
            alloc::format!(
                "function #0 \"paint\", pc {pc} (label \"weathered\" + {pc}, span 7, host_call eff_out=r0, \
                 host_sig#0(\"settings.set\"), eff_in=r0, args=[r1], rets=[]), \
                 host call settings.set: nothing named \"rustiness\""
            )
        );
    }

    #[test]
    fn declared_imports_check_like_binding_without_callbacks() {
        const NAMED: HostImport = HostImport::new("named", &[ValueType::Str], &[]);
        const MISSING: HostImport = HostImport::new("missing", &[ValueType::Str], &[]);
        const NAMED_I64: HostImport = HostImport::new("named", &[ValueType::I64], &[]);
        let mut pb = ProgramBuilder::new();
        let [named, missing] = [NAMED, MISSING].map(|i| pb.host_import(i));
        let f = pb
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            })
            .unwrap();
        let mut a = pb.function_asm(f).unwrap();
        for (import, text) in [(named, "good"), (named, "bad"), (missing, "bad")] {
            let [] = pb.emit_call_with(&mut a, import, (text,)).unwrap();
        }
        a.ret(0, &[]);
        pb.define_function(f, a).unwrap();
        let p = Arc::new(pb.build_verified().unwrap());
        let check = |site: &HostCallSite<'_>| match (site.import_in(&[NAMED]), site.arg(0)) {
            (Some(NAMED), Some(ValueRef::Str("bad"))) => Err("nothing named \"bad\""),
            _ => Ok(()),
        };

        // The same rejections as binding with callbacks for `named` only.
        let mut functions = HostFunctions::<()>::new();
        functions.implement(NAMED, |_, _, _, _| Ok(0)).unwrap();
        let bound = functions.bind_checked(&p, check).unwrap_err();
        let checked = p.check_host_calls(&[NAMED], check).unwrap_err();
        assert_eq!(checked, bound);
        assert_eq!(checked.imports[0].kind, ImportErrorKind::UnknownSymbol);
        assert_eq!(checked.calls.len(), 1);

        let error = p
            .check_host_calls(&[NAMED_I64, MISSING], check)
            .unwrap_err();
        assert_eq!(error.imports.len(), 1);
        assert_eq!(error.imports[0].import, named);
        assert_eq!(error.imports[0].kind, ImportErrorKind::SignatureMismatch);
        assert_eq!(error.imports[0].available, [NAMED_I64.signature()]);
        assert!(
            error.calls.is_empty(),
            "calls to a rejected import are not checked"
        );

        assert_eq!(
            p.check_host_calls(&[NAMED, MISSING], check)
                .unwrap_err()
                .calls
                .len(),
            1
        );
        assert!(
            p.check_host_calls(&[NAMED, MISSING], |_| Ok::<(), &str>(()))
                .is_ok()
        );
    }

    #[test]
    fn bindings_and_hosts_own_what_they_dispatch_through() {
        // Bindings stored beside the program they bind, with no borrow between them.
        struct Loaded {
            program: Arc<VerifiedProgram>,
            bindings: HostBindings<i64>,
        }
        let mut functions = HostFunctions::new();
        functions
            .register("increment", signature(), increment)
            .unwrap();
        let program = program("increment");
        let loaded = Loaded {
            bindings: functions.bind(&program).unwrap(),
            program,
        };
        let mut vm = Vm::new(loaded.bindings.with_state(0), Limits::default());
        let unit = vm.install(Arc::clone(&loaded.program)).unwrap();
        // The host outlives the bindings it was created from.
        drop(loaded);
        let entry = FuncRef::new(unit, FuncId(0));
        let run =
            |vm: &mut Vm<BoundHost<i64>>| vm.run(entry, &[Value::I64(1)], TraceMask::NONE, None);
        assert_eq!(run(&mut vm).unwrap(), [Value::I64(2)]);
        assert_eq!(*vm.host().state(), 1);
    }

    #[test]
    fn one_import_declaration_feeds_the_program_the_host_and_link_checks() {
        const INCREMENT: HostImport =
            HostImport::new("increment", &[ValueType::I64], &[ValueType::I64]);
        const OVERLOAD: HostImport =
            HostImport::new("increment", &[ValueType::Bool], &[ValueType::I64]);
        assert_eq!(INCREMENT.signature(), signature());

        let mut pb = ProgramBuilder::new();
        let import = pb.host_import(INCREMENT);
        assert_eq!(pb.host_import(INCREMENT), import);
        assert_eq!(pb.host_sig_for("increment", signature()), import);
        let f = pb
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let mut a = pb.function_asm(f).unwrap();
        let [value] = pb
            .emit_call_with(&mut a, import, (crate::asm::CallArg::I64(41),))
            .unwrap();
        a.ret(0, &[value.0]);
        pb.define_function(f, a).unwrap();
        let p = Arc::new(pb.build_verified().unwrap());

        let mut functions = HostFunctions::new();
        functions.implement(INCREMENT, increment).unwrap();
        assert!(matches!(
            functions.implement(INCREMENT, increment),
            Err(RegistrationError::DuplicateSignature { .. })
        ));
        let bindings = functions
            .bind_checked(&p, |site| {
                if site.calls(INCREMENT) && !site.calls(OVERLOAD) {
                    Ok(())
                } else {
                    Err("unexpected call")
                }
            })
            .unwrap();
        let mut vm = Vm::new(bindings.with_state(0), Limits::default());
        let unit = vm.install(Arc::clone(&p)).unwrap();
        assert_eq!(
            vm.run(FuncRef::new(unit, FuncId(0)), &[], TraceMask::NONE, None)
                .unwrap(),
            [Value::I64(42)]
        );
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
        let unit = a.install(Arc::clone(&p)).unwrap();
        assert_eq!(
            a.run(
                FuncRef::new(unit, FuncId(0)),
                &[Value::I64(40)],
                TraceMask::NONE,
                None
            )
            .unwrap(),
            [Value::I64(41)]
        );
        let unit = b.install(Arc::clone(&p)).unwrap();
        assert_eq!(
            b.run(
                FuncRef::new(unit, FuncId(0)),
                &[Value::I64(40)],
                TraceMask::NONE,
                None
            )
            .unwrap(),
            [Value::I64(141)]
        );
        assert_eq!(*a.host().state(), 1);
        *a.host_mut().state_mut() = 10;
        let mut ctx = ExecutionContext::new();
        let mut outputs = Vec::new();
        let mut reads = Reads::default();
        let unit = a.install(Arc::clone(&p)).unwrap();
        a.run_with_ctx(
            &mut ctx,
            FuncRef::new(unit, FuncId(0)),
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
        // An equal copy is still different code identity.
        let clone = Arc::new(VerifiedProgram::clone(&p));
        let other = program("different");
        let mut functions = HostFunctions::new();
        functions
            .register("increment", signature(), increment)
            .unwrap();
        let bindings = functions.bind(&p).unwrap();
        let mut vm = Vm::new(bindings.with_state(0), Limits::default());
        for wrong in [&clone, &other] {
            let unit = vm.install(Arc::clone(wrong)).unwrap();
            let error = vm
                .run(
                    FuncRef::new(unit, FuncId(0)),
                    &[Value::I64(1)],
                    TraceMask::NONE,
                    None,
                )
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
        let unit = vm.install(Arc::clone(&p)).unwrap();
        assert_eq!(
            vm.run(
                FuncRef::new(unit, FuncId(0)),
                &[Value::I64(1)],
                TraceMask::NONE,
                None
            )
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
        let unit = vm.install(Arc::clone(&p)).unwrap();
        assert_eq!(
            vm.run(
                FuncRef::new(unit, FuncId(0)),
                &[Value::I64(1)],
                TraceMask::NONE,
                None
            )
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
            let error = {
                let mut vm = Vm::new(bindings.with_state(()), Limits::default());
                let unit = vm.install(Arc::clone(&p)).unwrap();
                vm.run(
                    FuncRef::new(unit, FuncId(0)),
                    &[Value::I64(1)],
                    TraceMask::NONE,
                    None,
                )
            }
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
