// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Signature-aware call emission without an additional instruction API.

use super::*;

/// A declared direct, host or indirect call in one [`ProgramBuilder`].
///
/// IDs and registers retain their ordinary program/function-local meaning. This authoring
/// descriptor does not import code or make a raw ID valid in another builder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallTarget {
    /// A declared guest function.
    Function(FuncId),
    /// A declared host import.
    Host(HostSigId),
    /// A function/closure register with its declared caller-visible signature.
    Indirect {
        /// Program-local exposed signature; excludes a closure's environment.
        signature: CallSigId,
        /// Function-local register containing a function or closure.
        callee: u32,
    },
}
impl From<FuncId> for CallTarget {
    fn from(id: FuncId) -> Self {
        Self::Function(id)
    }
}
impl From<HostSigId> for CallTarget {
    fn from(id: HostSigId) -> Self {
        Self::Host(id)
    }
}

/// A call could not be emitted. The assembler's bytes and register ledger are unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CallBuildError {
    /// The target ID has no declaration in this builder.
    UnknownTarget(CallTarget),
    /// The number of supplied arguments disagrees with the declaration.
    Arguments {
        /// Requested target.
        target: CallTarget,
        /// Declared argument count.
        expected: usize,
        /// Supplied argument count.
        actual: usize,
    },
    /// The requested result array size disagrees with the declaration.
    Results {
        /// Requested target.
        target: CallTarget,
        /// Declared result count.
        expected: usize,
        /// Requested result count.
        actual: usize,
    },
    /// An argument or indirect callee register is not declared.
    Register(u32),
    /// A constant argument's kind disagrees with the signature.
    ConstantKind {
        /// Requested target.
        target: CallTarget,
        /// Zero-based argument position.
        index: usize,
        /// Signature's required kind.
        expected: ValueType,
        /// The constant's kind.
        actual: ValueType,
    },
    /// An argument's declared storage kind disagrees with the signature.
    ArgumentKind {
        /// Requested target.
        target: CallTarget,
        /// Offending source register.
        register: u32,
        /// Zero-based argument position.
        index: usize,
        /// Signature's required kind.
        expected: ValueType,
        /// Register's declared kind.
        actual: ValueType,
    },
    /// An indirect target register contains neither a function nor a closure.
    CalleeKind(ValueType),
    /// r0 must be declared as Unit; start with [`ProgramBuilder::function_asm`].
    EffectRegister,
    /// Allocating the results would exceed the source register address space.
    RegisterSpace,
}
impl fmt::Display for CallBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTarget(target) => write!(f, "call target {target:?} has no declaration"),
            Self::Arguments {
                target,
                expected,
                actual,
            } => write!(
                f,
                "call to {target:?} expects {expected} arguments, got {actual}"
            ),
            Self::Results {
                target,
                expected,
                actual,
            } => write!(
                f,
                "call to {target:?} declares {expected} results, requested {actual}"
            ),
            Self::Register(register) => write!(f, "call uses undeclared register r{register}"),
            Self::ArgumentKind {
                target,
                register,
                index,
                expected,
                actual,
            } => write!(
                f,
                "call to {target:?}, argument {index} at r{register} requires {expected:?}, register declares {actual:?}"
            ),
            Self::ConstantKind {
                target,
                index,
                expected,
                actual,
            } => write!(
                f,
                "call to {target:?}, argument {index} requires {expected:?}, constant is {actual:?}"
            ),
            Self::CalleeKind(actual) => write!(
                f,
                "indirect call requires Func or Closure, register declares {actual:?}"
            ),
            Self::EffectRegister => f.write_str(
                "call requires r0 declared as Unit; start with ProgramBuilder::function_asm",
            ),
            Self::RegisterSpace => {
                f.write_str("call results exceed the u32 register address space")
            }
        }
    }
}
impl core::error::Error for CallBuildError {}

/// One argument of [`ProgramBuilder::emit_call_with`]: a register, or a constant that the call
/// loads into a fresh register of its kind immediately before it.
///
/// A `u32` converts to [`Self::Register`], as registers are `u32` throughout the assembler, so an
/// integer constant is always spelled out: `CallArg::U32(1)`. Strings, `f32`, `f64`, `bool` and
/// [`Decimal`] convert to constants. Strings and bytes are interned in the program's constant
/// pool, so each distinct value is stored once.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum CallArg<'a> {
    /// A function-local register.
    Register(u32),
    /// `()`.
    Unit,
    /// A boolean constant.
    Bool(bool),
    /// An `i32` constant.
    I32(i32),
    /// A `u32` constant.
    U32(u32),
    /// An `i64` constant.
    I64(i64),
    /// A `u64` constant.
    U64(u64),
    /// An `f32` constant, loaded with its exact bits.
    F32(f32),
    /// An `f64` constant, loaded with its exact bits.
    F64(f64),
    /// A decimal constant.
    Decimal(Decimal),
    /// A byte-string constant.
    Bytes(&'a [u8]),
    /// A string constant.
    Str(&'a str),
}

impl CallArg<'_> {
    /// The kind of value this argument supplies, or `None` for a register.
    fn constant_kind(&self) -> Option<ValueType> {
        Some(match self {
            Self::Register(_) => return None,
            Self::Unit => ValueType::Unit,
            Self::Bool(_) => ValueType::Bool,
            Self::I32(_) => ValueType::I32,
            Self::U32(_) => ValueType::U32,
            Self::I64(_) => ValueType::I64,
            Self::U64(_) => ValueType::U64,
            Self::F32(_) => ValueType::F32,
            Self::F64(_) => ValueType::F64,
            Self::Decimal(_) => ValueType::Decimal,
            Self::Bytes(_) => ValueType::Bytes,
            Self::Str(_) => ValueType::Str,
        })
    }
}

impl From<u32> for CallArg<'_> {
    fn from(register: u32) -> Self {
        Self::Register(register)
    }
}
impl<'a> From<&'a str> for CallArg<'a> {
    fn from(value: &'a str) -> Self {
        Self::Str(value)
    }
}
impl From<bool> for CallArg<'_> {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}
impl From<f32> for CallArg<'_> {
    fn from(value: f32) -> Self {
        Self::F32(value)
    }
}
impl From<f64> for CallArg<'_> {
    fn from(value: f64) -> Self {
        Self::F64(value)
    }
}
impl From<Decimal> for CallArg<'_> {
    fn from(value: Decimal) -> Self {
        Self::Decimal(value)
    }
}

/// The arguments of [`ProgramBuilder::emit_call_with`]: a tuple of up to twelve values that
/// convert into [`CallArg`], such as `(handle, "name", 0.5_f32)`, or a slice of [`CallArg`].
pub trait CallArgs<'a> {
    /// Appends these arguments to `out`, in order.
    fn push_into(self, out: &mut Vec<CallArg<'a>>);
}

impl<'a> CallArgs<'a> for &[CallArg<'a>] {
    fn push_into(self, out: &mut Vec<CallArg<'a>>) {
        out.extend_from_slice(self);
    }
}

macro_rules! tuple_call_args {
    ($($name:ident)*) => {
        impl<'a, $($name: Into<CallArg<'a>>),*> CallArgs<'a> for ($($name,)*) {
            #[allow(non_snake_case, reason = "tuple fields are named after their types")]
            #[allow(unused_variables, reason = "the empty tuple pushes nothing")]
            fn push_into(self, out: &mut Vec<CallArg<'a>>) {
                let ($($name,)*) = self;
                $(out.push($name.into());)*
            }
        }
    };
}
tuple_call_args!();
tuple_call_args!(A);
tuple_call_args!(A B);
tuple_call_args!(A B C);
tuple_call_args!(A B C D);
tuple_call_args!(A B C D E);
tuple_call_args!(A B C D E F);
tuple_call_args!(A B C D E F G);
tuple_call_args!(A B C D E F G H);
tuple_call_args!(A B C D E F G H I);
tuple_call_args!(A B C D E F G H I J);
tuple_call_args!(A B C D E F G H I J K);
tuple_call_args!(A B C D E F G H I J K L);

enum Kinds<'a> {
    Contracts(&'a [ContractId], &'a ContractTableBuilder),
    Plain(&'a [ValueType]),
}
impl Kinds<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Contracts(ids, _) => ids.len(),
            Self::Plain(kinds) => kinds.len(),
        }
    }
    fn get(&self, index: usize) -> ValueType {
        match self {
            Self::Contracts(ids, contracts) => contracts
                .table()
                .kind(ids[index])
                .expect("declared contract"),
            Self::Plain(kinds) => kinds[index],
        }
    }
}

impl ProgramBuilder {
    /// Returns the source register for one declared function argument, excluding r0.
    ///
    /// Use the returned register in this function's [`Self::function_asm`] body. A closure
    /// body's argument zero is its environment.
    pub fn argument(&self, function: FuncId, index: u32) -> Result<u32, BuildError> {
        let declaration = self
            .functions
            .get(function.0 as usize)
            .ok_or(BuildError::BadFuncId { func: function.0 })?;
        if index as usize >= declaration.args.len() {
            return Err(BuildError::BadArgIndex {
                func: function.0,
                arg: index,
                arg_count: u32::try_from(declaration.args.len()).unwrap_or(u32::MAX),
            });
        }
        Ok(index
            .checked_add(1)
            .expect("argument register fits declared function"))
    }

    /// Emits a call, allocating its result registers from the target's declared signature.
    ///
    /// The array length is inferred from the receiving pattern, including `let []` for no
    /// results. Direct function and host IDs convert to [`CallTarget`] automatically. A body's
    /// assembler remains detached, so frontends can declare nested functions while emitting it.
    ///
    /// This checks target existence, arities, declared register kinds and r0 before mutation.
    /// Initialization, control flow and rich value contracts still require [`Self::build_verified`].
    /// Successful calls use the reserved Unit register r0, exactly as ordinary [`Asm`] calls do.
    /// No signatures, authoring state or additional instructions enter the executable format.
    ///
    /// ```
    /// use execution_tape::{asm::{FunctionSig, ProgramBuilder}, program::ValueType};
    /// let mut program = ProgramBuilder::new();
    /// let signature = FunctionSig { arg_types: vec![ValueType::I64], ret_types: vec![ValueType::I64] };
    /// let identity = program.declare_function(&signature)?;
    /// let mut body = program.function_asm(identity)?;
    /// let value = program.argument(identity, 0)?;
    /// body.ret(0, &[value]);
    /// program.define_function(identity, body)?;
    /// let caller = program.declare_function(signature)?;
    /// let mut body = program.function_asm(caller)?;
    /// let input = program.argument(caller, 0)?;
    /// let [answer] = program.emit_call(&mut body, identity, &[input])?;
    /// body.ret(0, &[answer]);
    /// program.define_function(caller, body)?;
    /// program.build_verified()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn emit_call<const N: usize>(
        &self,
        body: &mut Asm,
        target: impl Into<CallTarget>,
        args: &[u32],
    ) -> Result<[u32; N], CallBuildError> {
        let target = target.into();
        let results = self.check_call(
            body,
            target,
            args.iter().map(|&register| CallArg::Register(register)),
            N,
            0,
        )?;
        let results = core::array::from_fn(|index| body.alloc_register(results.get(index)));
        match target {
            CallTarget::Function(id) => {
                body.call(0, id, 0, args, &results);
            }
            CallTarget::Host(id) => {
                body.host_call(0, id, 0, args, &results);
            }
            CallTarget::Indirect { signature, callee } => {
                body.call_indirect(0, signature, callee, 0, args, &results);
            }
        }
        Ok(results)
    }

    /// Emits a call as [`Self::emit_call`] does, accepting constants as arguments.
    ///
    /// Each constant is loaded into a fresh register of its kind just before the call, in
    /// argument order, and strings and bytes are interned in the constant pool. The bytecode is
    /// exactly what [`Asm::alloc_register`], the matching `const_*` or [`Asm::const_pool`] load
    /// and [`Self::emit_call`] produce by hand. Every check, including each constant's kind
    /// against the signature, happens before anything is emitted.
    ///
    /// ```
    /// use execution_tape::asm::{CallArg, FunctionSig, ProgramBuilder};
    /// use execution_tape::host::HostSig;
    /// use execution_tape::program::ValueType;
    /// let mut program = ProgramBuilder::new();
    /// let set = program.host_sig_for(
    ///     "settings.set",
    ///     HostSig { args: vec![ValueType::Str, ValueType::U32, ValueType::F32], rets: vec![] },
    /// );
    /// let entry = program.declare_function(FunctionSig { arg_types: vec![ValueType::F32], ret_types: vec![] })?;
    /// let mut body = program.function_asm(entry)?;
    /// let value = program.argument(entry, 0)?;
    /// // A `u32` is a register; an integer constant is spelled out.
    /// let [] = program.emit_call_with(&mut body, set, ("age", CallArg::U32(1), value))?;
    /// let [] = program.emit_call_with(&mut body, set, ("age", CallArg::U32(2), 0.5_f32))?;
    /// body.ret(0, &[]);
    /// program.define_function(entry, body)?;
    /// program.build_verified()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn emit_call_with<'a, const N: usize>(
        &mut self,
        body: &mut Asm,
        target: impl Into<CallTarget>,
        args: impl CallArgs<'a>,
    ) -> Result<[u32; N], CallBuildError> {
        let target = target.into();
        let mut operands = Vec::new();
        args.push_into(&mut operands);
        let constants = operands
            .iter()
            .filter(|arg| !matches!(arg, CallArg::Register(_)))
            .count();
        self.check_call(body, target, operands.iter().copied(), N, constants)?;
        let registers: Vec<u32> = operands
            .into_iter()
            .map(|arg| self.load(body, arg))
            .collect();
        self.emit_call(body, target, &registers)
    }

    /// Interns `value` in the constant pool and loads it into a fresh `Str` register of `body`.
    ///
    /// One call in place of [`Self::const_str`], [`Asm::alloc_register`] and
    /// [`Asm::const_pool`], with identical bytecode. Equal strings share one pool entry.
    pub fn emit_const_str(&mut self, body: &mut Asm, value: &str) -> u32 {
        self.load(body, CallArg::Str(value))
    }

    /// Loads a constant into a fresh register, or returns a register argument unchanged.
    fn load(&mut self, body: &mut Asm, arg: CallArg<'_>) -> u32 {
        let Some(kind) = arg.constant_kind() else {
            let CallArg::Register(register) = arg else {
                unreachable!("only registers have no constant kind")
            };
            return register;
        };
        let dst = body.alloc_register(kind);
        match arg {
            CallArg::Register(_) => unreachable!("registers are returned above"),
            CallArg::Unit => body.const_unit(dst),
            CallArg::Bool(v) => body.const_bool(dst, v),
            CallArg::I32(v) => body.const_i32(dst, v),
            CallArg::U32(v) => body.const_u32(dst, v),
            CallArg::I64(v) => body.const_i64(dst, v),
            CallArg::U64(v) => body.const_u64(dst, v),
            CallArg::F32(v) => body.const_f32_bits(dst, v.to_bits()),
            CallArg::F64(v) => body.const_f64_bits(dst, v.to_bits()),
            CallArg::Decimal(v) => body.const_decimal_value(dst, v),
            CallArg::Bytes(v) => body.const_pool(dst, self.const_bytes(v)),
            CallArg::Str(v) => body.const_pool(dst, self.const_str(v)),
        };
        dst
    }

    /// Checks a call before anything is emitted, returning the target's result kinds.
    ///
    /// `temporaries` counts registers the caller allocates for constants before the call.
    fn check_call<'s>(
        &'s self,
        body: &Asm,
        target: CallTarget,
        args: impl ExactSizeIterator<Item = CallArg<'s>>,
        result_count: usize,
        temporaries: usize,
    ) -> Result<Kinds<'s>, CallBuildError> {
        let (parameters, results) = match target {
            CallTarget::Function(id) => {
                let function = self
                    .functions
                    .get(id.0 as usize)
                    .ok_or(CallBuildError::UnknownTarget(target))?;
                (
                    Kinds::Contracts(&function.args, &self.contracts),
                    Kinds::Contracts(&function.rets, &self.contracts),
                )
            }
            CallTarget::Host(id) => {
                let import = self
                    .host_sigs
                    .get(id.0 as usize)
                    .ok_or(CallBuildError::UnknownTarget(target))?;
                (Kinds::Plain(&import.args), Kinds::Plain(&import.rets))
            }
            CallTarget::Indirect { signature, callee } => {
                let signature = self
                    .call_sigs
                    .get(signature.0 as usize)
                    .ok_or(CallBuildError::UnknownTarget(target))?;
                let kind = body
                    .register_types
                    .get(callee as usize)
                    .ok_or(CallBuildError::Register(callee))?;
                if !matches!(kind, ValueType::Func | ValueType::Closure) {
                    return Err(CallBuildError::CalleeKind(*kind));
                }
                (
                    Kinds::Contracts(&signature.args, &self.contracts),
                    Kinds::Contracts(&signature.rets, &self.contracts),
                )
            }
        };
        if args.len() != parameters.len() {
            return Err(CallBuildError::Arguments {
                target,
                expected: parameters.len(),
                actual: args.len(),
            });
        }
        if result_count != results.len() {
            return Err(CallBuildError::Results {
                target,
                expected: results.len(),
                actual: result_count,
            });
        }
        if body.register_types.first() != Some(&ValueType::Unit) {
            return Err(CallBuildError::EffectRegister);
        }
        for (index, arg) in args.enumerate() {
            let expected = parameters.get(index);
            match arg {
                CallArg::Register(register) => {
                    let actual = *body
                        .register_types
                        .get(register as usize)
                        .ok_or(CallBuildError::Register(register))?;
                    if actual != expected {
                        return Err(CallBuildError::ArgumentKind {
                            target,
                            register,
                            index,
                            expected,
                            actual,
                        });
                    }
                }
                constant => {
                    let actual = constant.constant_kind().expect("a constant argument");
                    if actual != expected {
                        return Err(CallBuildError::ConstantKind {
                            target,
                            index,
                            expected,
                            actual,
                        });
                    }
                }
            }
        }
        if body
            .register_types
            .len()
            .checked_add(result_count)
            .and_then(|count| count.checked_add(temporaries))
            .is_none_or(|count| count.saturating_sub(1) > u32::MAX as usize)
        {
            return Err(CallBuildError::RegisterSpace);
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::FuncRef;
    use crate::{
        host::{Host, HostContext, HostError, SigHash, ValueRef},
        trace::TraceMask,
        value::Value,
        vm::{Limits, Vm},
    };

    #[derive(Default)]
    struct Negate {
        notifications: usize,
    }
    impl Host for Negate {
        fn call(
            &mut self,
            symbol: &str,
            _: SigHash,
            args: &[ValueRef<'_>],
            results: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            match (symbol, args) {
                ("negate", [ValueRef::I64(value)]) => results[0] = Value::I64(-value),
                ("notify", []) => self.notifications += 1,
                _ => return Err(HostError::UnknownSymbol),
            }
            Ok(0)
        }
    }

    #[test]
    fn declared_calls_execute_with_multiple_empty_and_indirect_results() {
        let mut program = ProgramBuilder::new();
        let signature = FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64, ValueType::Bool],
        };
        let leaf = program.declare_function(&signature).unwrap();
        let mut body = program.function_asm(leaf).unwrap();
        let argument = program.argument(leaf, 0).unwrap();
        let flag = body.alloc_register(ValueType::Bool);
        body.const_bool(flag, true).ret(0, &[argument, flag]);
        program.define_function(leaf, body).unwrap();
        let entry = program.declare_function(&signature).unwrap();
        let mut body = program.function_asm(entry).unwrap();
        // The detached body permits imports and signatures to be declared as needed.
        let negate = program.host_sig_for(
            "negate",
            HostSig {
                args: vec![ValueType::I64],
                rets: vec![ValueType::I64],
            },
        );
        let notify = program.host_sig_for(
            "notify",
            HostSig {
                args: vec![],
                rets: vec![],
            },
        );
        let indirect = program.call_sig(signature).unwrap();
        let argument = program.argument(entry, 0).unwrap();
        let [negative] = program.emit_call(&mut body, negate, &[argument]).unwrap();
        let [value, _flag] = program.emit_call(&mut body, leaf, &[negative]).unwrap();
        let callee = body.alloc_register(ValueType::Func);
        body.const_func(callee, leaf);
        let [answer, flag] = program
            .emit_call(
                &mut body,
                CallTarget::Indirect {
                    signature: indirect,
                    callee,
                },
                &[value],
            )
            .unwrap();
        let [] = program.emit_call(&mut body, notify, &[]).unwrap();
        body.ret(0, &[answer, flag]);
        program.define_function(entry, body).unwrap();
        let program = program.build_verified().unwrap();
        let mut vm = Vm::new(Negate::default(), Limits::default());
        let unit = vm.install(program.clone()).unwrap();
        assert_eq!(
            vm.run(
                FuncRef::new(unit, entry),
                &[Value::I64(42)],
                TraceMask::NONE,
                None
            )
            .unwrap(),
            [Value::I64(-42), Value::Bool(true)]
        );
        assert_eq!(vm.host().notifications, 1);
    }

    #[test]
    fn denied_call_requests_preserve_bytes_and_register_declarations() {
        let mut program = ProgramBuilder::new();
        let signature = FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::Bool],
        };
        let function = program.declare_function(&signature).unwrap();
        let host = program.host_sig_for(
            "check",
            HostSig {
                args: signature.arg_types,
                rets: signature.ret_types,
            },
        );
        let indirect = program
            .call_sig(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::Bool],
            })
            .unwrap();
        let mut body = program.function_asm(function).unwrap();
        let boolean = body.alloc_register(ValueType::Bool);
        body.const_bool(boolean, true);
        let before = body.clone().finish_parts().unwrap();
        let attempts: [Result<[u32; 1], _>; 7] = [
            program.emit_call(&mut body, FuncId(99), &[1]),
            program.emit_call(&mut body, HostSigId(99), &[1]),
            program.emit_call(&mut body, host, &[]),
            program.emit_call(&mut body, host, &[99]),
            program.emit_call(&mut body, host, &[boolean]),
            program.emit_call(
                &mut body,
                CallTarget::Indirect {
                    signature: indirect,
                    callee: boolean,
                },
                &[],
            ),
            program.emit_call(
                &mut body,
                CallTarget::Indirect {
                    signature: CallSigId(99),
                    callee: boolean,
                },
                &[],
            ),
        ];
        assert!(attempts.into_iter().all(|attempt| attempt.is_err()));
        let wrong_results: Result<[u32; 2], _> = program.emit_call(&mut body, host, &[1]);
        assert!(matches!(
            wrong_results,
            Err(CallBuildError::Results {
                expected: 1,
                actual: 2,
                ..
            })
        ));
        let after = body.finish_parts().unwrap();
        assert_eq!(before.bytecode, after.bytecode);
        assert_eq!(before.register_types, after.register_types);
        assert!(matches!(
            program.argument(function, 1),
            Err(BuildError::BadArgIndex { arg_count: 1, .. })
        ));
    }

    #[test]
    fn authoring_checks_do_not_replace_initialization_or_contract_verification() {
        let mut program = ProgramBuilder::new();
        let function = program
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let host = program.host_sig_for(
            "read",
            HostSig {
                args: vec![ValueType::I64],
                rets: vec![ValueType::I64],
            },
        );
        let mut body = program.function_asm(function).unwrap();
        let unwritten = body.alloc_register(ValueType::I64);
        let [result] = program.emit_call(&mut body, host, &[unwritten]).unwrap();
        body.ret(0, &[result]);
        program.define_function(function, body).unwrap();
        assert!(matches!(
            program.build_verified(),
            Err(BuildError::Verify(VerifyError::UninitializedRead { .. }))
        ));
    }

    /// Declares `f(f32)` importing `h(str, u32, f32, bool, i64, bytes, str) -> u32`.
    fn constant_program() -> (ProgramBuilder, Asm, HostSigId, u32) {
        let mut program = ProgramBuilder::new();
        let h = program.host_sig_for(
            "h",
            HostSig {
                args: vec![
                    ValueType::Str,
                    ValueType::U32,
                    ValueType::F32,
                    ValueType::Bool,
                    ValueType::I64,
                    ValueType::Bytes,
                    ValueType::Str,
                ],
                rets: vec![ValueType::U32],
            },
        );
        let f = program
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::F32],
                ret_types: vec![],
            })
            .unwrap();
        let body = program.function_asm(f).unwrap();
        let argument = program.argument(f, 0).unwrap();
        (program, body, h, argument)
    }

    fn finish(mut program: ProgramBuilder, mut body: Asm) -> Program {
        body.ret(0, &[]);
        program.define_function(FuncId(0), body).unwrap();
        program.build_checked().unwrap()
    }

    #[test]
    fn constant_arguments_emit_exactly_the_hand_written_loads() {
        let (mut program, mut body, h, x) = constant_program();
        for _ in 0..2 {
            let [_] = program
                .emit_call_with(
                    &mut body,
                    h,
                    (
                        "name",
                        CallArg::U32(7),
                        x,
                        true,
                        CallArg::I64(-3),
                        CallArg::Bytes(b"raw"),
                        "name",
                    ),
                )
                .unwrap();
        }
        let r = program.emit_const_str(&mut body, "name");
        let [_] = program
            .emit_call_with(
                &mut body,
                h,
                &[
                    CallArg::Register(r),
                    CallArg::U32(1),
                    CallArg::F32(f32::from_bits(0x7fc0_0001)),
                    CallArg::Bool(false),
                    CallArg::I64(0),
                    CallArg::Bytes(&[]),
                    CallArg::Str(""),
                ][..],
            )
            .unwrap();
        let sugared = finish(program, body);

        let (mut program, mut body, h, x) = constant_program();
        for _ in 0..2 {
            let name = body.alloc_register(ValueType::Str);
            body.const_pool(name, program.const_str("name"));
            let seven = body.alloc_register(ValueType::U32);
            body.const_u32(seven, 7);
            let flag = body.alloc_register(ValueType::Bool);
            body.const_bool(flag, true);
            let n = body.alloc_register(ValueType::I64);
            body.const_i64(n, -3);
            let raw = body.alloc_register(ValueType::Bytes);
            body.const_pool(raw, program.const_bytes(b"raw"));
            let again = body.alloc_register(ValueType::Str);
            body.const_pool(again, program.const_str("name"));
            let [_] = program
                .emit_call(&mut body, h, &[name, seven, x, flag, n, raw, again])
                .unwrap();
        }
        let r = body.alloc_register(ValueType::Str);
        body.const_pool(r, program.const_str("name"));
        let one = body.alloc_register(ValueType::U32);
        body.const_u32(one, 1);
        let nan = body.alloc_register(ValueType::F32);
        body.const_f32_bits(nan, 0x7fc0_0001);
        let flag = body.alloc_register(ValueType::Bool);
        body.const_bool(flag, false);
        let zero = body.alloc_register(ValueType::I64);
        body.const_i64(zero, 0);
        let empty = body.alloc_register(ValueType::Bytes);
        body.const_pool(empty, program.const_bytes(&[]));
        let blank = body.alloc_register(ValueType::Str);
        body.const_pool(blank, program.const_str(""));
        let [_] = program
            .emit_call(&mut body, h, &[r, one, nan, flag, zero, empty, blank])
            .unwrap();
        let by_hand = finish(program, body);

        assert_eq!(sugared, by_hand);
        // "name", b"raw", b"" and "" each occupy one pool entry.
        assert_eq!(sugared.const_pool.len(), 4);
    }

    #[test]
    fn rejected_constant_calls_emit_and_intern_nothing() {
        let (mut program, mut body, h, x) = constant_program();
        let (pc, registers) = (body.pc(), body.register_types.len());
        let args = (
            "fresh",
            7.0_f64,
            x,
            true,
            CallArg::I64(0),
            CallArg::Bytes(b""),
            "",
        );
        assert_eq!(
            program.emit_call_with::<1>(&mut body, h, args),
            Err(CallBuildError::ConstantKind {
                target: CallTarget::Host(h),
                index: 1,
                expected: ValueType::U32,
                actual: ValueType::F64,
            })
        );
        assert_eq!(
            program.emit_call_with::<1>(&mut body, h, ("fresh", x)),
            Err(CallBuildError::Arguments {
                target: CallTarget::Host(h),
                expected: 7,
                actual: 2,
            })
        );
        assert_eq!((body.pc(), body.register_types.len()), (pc, registers));
        assert!(program.const_pool.is_empty());
    }
}
