// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Signature-aware control emission, with unchanged bytes and registers on rejection.

use super::*;

/// A control instruction could not be authored. No bytes or result registers were added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlBuildError {
    /// The signature is not declared in this builder.
    Signature(CallSigId),
    /// A source or handler register is not declared in the body.
    Register(u32),
    /// A declared register has an incompatible storage kind.
    Kind {
        /// Source register.
        register: u32,
        /// Required storage kind. Callable targets accept either `Func` or `Closure`.
        expected: ValueType,
        /// Actual declaration.
        actual: ValueType,
    },
    /// The request or resume input count differs from the declared signature.
    Arguments {
        /// Declared count.
        expected: usize,
        /// Supplied count.
        actual: usize,
    },
    /// The result array size differs from the declared signature.
    Results {
        /// Declared count.
        expected: usize,
        /// Requested count.
        actual: usize,
    },
    /// r0 must be Unit; start with [`ProgramBuilder::function_asm`].
    EffectRegister,
    /// Allocating destinations would exceed the virtual register address space.
    RegisterSpace,
}
impl fmt::Display for ControlBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Signature(id) => write!(f, "control signature #{} is not declared", id.0),
            Self::Register(reg) => write!(f, "control instruction uses undeclared register r{reg}"),
            Self::Kind { register, expected, actual } => write!(f, "control register r{register} requires {expected:?}, declares {actual:?}"),
            Self::Arguments { expected, actual } => write!(f, "control signature requires {expected} input values, got {actual}"),
            Self::Results { expected, actual } => write!(f, "control signature declares {expected} results, requested {actual}"),
            Self::EffectRegister => f.write_str("control instruction requires r0 declared as Unit; use ProgramBuilder::function_asm"),
            Self::RegisterSpace => f.write_str("control results exceed the virtual register address space"),
        }
    }
}
impl core::error::Error for ControlBuildError {}

fn require(body: &Asm, register: u32, expected: ValueType) -> Result<(), ControlBuildError> {
    let actual = *body
        .register_types
        .get(register as usize)
        .ok_or(ControlBuildError::Register(register))?;
    if actual == expected {
        Ok(())
    } else {
        Err(ControlBuildError::Kind {
            register,
            expected,
            actual,
        })
    }
}

fn require_callable(body: &Asm, register: u32) -> Result<(), ControlBuildError> {
    let actual = *body
        .register_types
        .get(register as usize)
        .ok_or(ControlBuildError::Register(register))?;
    if matches!(actual, ValueType::Func | ValueType::Closure) {
        Ok(())
    } else {
        Err(ControlBuildError::Kind {
            register,
            expected: ValueType::Func,
            actual,
        })
    }
}

impl ProgramBuilder {
    /// Creates a one-use computation from a function or closure register without executing it.
    ///
    /// `signature` is its exposed argument/result signature. The verifier checks any statically
    /// known target; opaque targets are checked at runtime. The returned register is declared as
    /// [`ValueType::Continuation`]. Failure leaves the body unchanged.
    pub fn emit_continuation(
        &self,
        body: &mut Asm,
        callee: u32,
        signature: CallSigId,
    ) -> Result<u32, ControlBuildError> {
        self.call_sigs
            .get(signature.0 as usize)
            .ok_or(ControlBuildError::Signature(signature))?;
        require_callable(body, callee)?;
        self.control_capacity(body, 1)?;
        let continuation = body.alloc_register(ValueType::Continuation);
        body.cont_new(continuation, callee, signature);
        Ok(continuation)
    }

    /// Consumes a computation under the supplied handlers and allocates its typed final results.
    ///
    /// `signature` describes the inputs expected at this suspension, independently of the
    /// computation's original arguments. Each suspension clause receives request values followed
    /// by a continuation, and returns the final results. Dynamic tag/callable contracts and
    /// one-use authority are checked by execution. Authoring errors leave the body unchanged.
    pub fn emit_resume<const N: usize>(
        &self,
        body: &mut Asm,
        continuation: u32,
        signature: CallSigId,
        args: &[u32],
        handlers: &[HandlerArm],
    ) -> Result<[u32; N], ControlBuildError> {
        require(body, continuation, ValueType::Continuation)?;
        for arm in handlers {
            match *arm {
                HandlerArm::Suspend { tag, handler } => {
                    require(body, tag, ValueType::Tag)?;
                    require_callable(body, handler)?;
                }
            }
        }
        let results = self.control_results::<N>(body, signature, args)?;
        body.cont_resume(continuation, signature, args, handlers, &results);
        Ok(results)
    }

    /// Suspends with a typed request and allocates the registers for its eventual response.
    ///
    /// Handler lookup and capture are VM operations. The enclosing computation resumes only when
    /// its one-use capability is consumed, possibly under a different handler context.
    /// Authoring errors leave the body unchanged.
    pub fn emit_suspend<const N: usize>(
        &self,
        body: &mut Asm,
        tag: u32,
        signature: CallSigId,
        request: &[u32],
    ) -> Result<[u32; N], ControlBuildError> {
        require(body, tag, ValueType::Tag)?;
        let response = self.control_results::<N>(body, signature, request)?;
        body.suspend(tag, signature, request, &response);
        Ok(response)
    }

    fn control_capacity(&self, body: &Asm, count: usize) -> Result<(), ControlBuildError> {
        if body.register_types.first() != Some(&ValueType::Unit) {
            return Err(ControlBuildError::EffectRegister);
        }
        if body
            .register_types
            .len()
            .checked_add(count)
            .is_none_or(|len| len.saturating_sub(1) > u32::MAX as usize)
        {
            return Err(ControlBuildError::RegisterSpace);
        }
        Ok(())
    }

    fn control_results<const N: usize>(
        &self,
        body: &mut Asm,
        signature: CallSigId,
        args: &[u32],
    ) -> Result<[u32; N], ControlBuildError> {
        let entry = self
            .call_sigs
            .get(signature.0 as usize)
            .ok_or(ControlBuildError::Signature(signature))?;
        if entry.args.len() != args.len() {
            return Err(ControlBuildError::Arguments {
                expected: entry.args.len(),
                actual: args.len(),
            });
        }
        if entry.rets.len() != N {
            return Err(ControlBuildError::Results {
                expected: entry.rets.len(),
                actual: N,
            });
        }
        self.control_capacity(body, N)?;
        for (&reg, &contract) in args.iter().zip(&entry.args) {
            require(
                body,
                reg,
                self.contracts
                    .table()
                    .kind(contract)
                    .expect("declared contract"),
            )?;
        }
        Ok(core::array::from_fn(|index| {
            body.alloc_register(
                self.contracts
                    .table()
                    .kind(entry.rets[index])
                    .expect("declared contract"),
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_control_emission_preserves_bytes_and_register_ledger() {
        let mut builder = ProgramBuilder::new();
        let signature = builder
            .call_sig(FunctionSig {
                arg_types: vec![ValueType::Func],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let function = builder
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::Continuation, ValueType::Closure],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let mut body = builder.function_asm(function).unwrap();
        let continuation = builder.argument(function, 0).unwrap();
        let closure = builder.argument(function, 1).unwrap();
        let bytes = body.bytes.clone();
        let ledger = body.register_types.clone();
        assert!(
            matches!(builder.emit_resume::<1>(&mut body, continuation, signature, &[closure], &[]),
            Err(ControlBuildError::Kind { register, expected: ValueType::Func, actual: ValueType::Closure }) if register == closure)
        );
        assert_eq!(body.bytes, bytes);
        assert_eq!(body.register_types, ledger);
        assert!(matches!(
            builder.emit_resume::<2>(&mut body, continuation, signature, &[closure], &[]),
            Err(ControlBuildError::Results {
                expected: 1,
                actual: 2
            })
        ));
        assert_eq!(body.bytes, bytes);
        assert_eq!(body.register_types, ledger);
    }
}
