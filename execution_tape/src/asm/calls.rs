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
        if N != results.len() {
            return Err(CallBuildError::Results {
                target,
                expected: results.len(),
                actual: N,
            });
        }
        if body.register_types.first() != Some(&ValueType::Unit) {
            return Err(CallBuildError::EffectRegister);
        }
        for (index, register) in args.iter().enumerate() {
            let actual = *body
                .register_types
                .get(*register as usize)
                .ok_or(CallBuildError::Register(*register))?;
            let expected = parameters.get(index);
            if actual != expected {
                return Err(CallBuildError::ArgumentKind {
                    target,
                    register: *register,
                    index,
                    expected,
                    actual,
                });
            }
        }
        if body
            .register_types
            .len()
            .checked_add(N)
            .is_none_or(|count| count.saturating_sub(1) > u32::MAX as usize)
        {
            return Err(CallBuildError::RegisterSpace);
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(
            vm.run(&program, entry, &[Value::I64(42)], TraceMask::NONE, None)
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
}
