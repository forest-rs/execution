// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Signature-aware control emission, with unchanged bytes and registers on rejection.

use super::*;

/// A symbolic suspension clause in the function containing `cont.resume`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandlerArm {
    /// Transfer to a local typed block, keeping the resumer's lexical registers and frame.
    Suspend {
        /// Exact operation identity register.
        tag: u32,
        /// Operation request/response protocol.
        signature: CallSigId,
        /// Block receiving request values followed by a continuation.
        target: Label,
    },
}

/// Typed clause parameters and its symbolic block target, created before emitting a resume.
#[derive(Clone, Debug)]
pub struct HandlerBlock {
    arm: HandlerArm,
    request: Vec<u32>,
    continuation: u32,
    label: Label,
}

/// Typed final-result parameters and the normal successor of a resumption.
#[derive(Clone, Debug)]
pub struct ResultBlock {
    results: Vec<u32>,
    label: Label,
}
impl ResultBlock {
    /// Final-result registers in signature order.
    #[must_use]
    pub fn results(&self) -> &[u32] {
        &self.results
    }
    /// Place the result block with [`Asm::place`].
    #[must_use]
    pub fn label(&self) -> Label {
        self.label
    }
}
impl HandlerBlock {
    /// Binding supplied to [`ProgramBuilder::emit_resume`].
    #[must_use]
    pub fn arm(&self) -> HandlerArm {
        self.arm
    }
    /// Request parameter registers in protocol order.
    #[must_use]
    pub fn request(&self) -> &[u32] {
        &self.request
    }
    /// Parameter receiving the captured one-use computation.
    #[must_use]
    pub fn continuation(&self) -> u32 {
        self.continuation
    }
    /// Place this target with [`Asm::place`] to emit the clause in its lexical function.
    #[must_use]
    pub fn label(&self) -> Label {
        self.label
    }
}

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
    /// r0 must be Unit; start with [`ProgramBuilder::function_asm`].
    EffectRegister,
    /// Allocating destinations would exceed the virtual register address space.
    RegisterSpace,
    /// A control outcome target was not declared as a typed block in this assembler.
    Target(Label),
    /// A target block has a different parameter count from its control outcome.
    BlockParameters {
        /// Target block.
        target: Label,
        /// Required outcome parameter count.
        expected: usize,
        /// Declared block parameter count.
        actual: usize,
    },
}
impl fmt::Display for ControlBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Signature(id) => write!(f, "control signature #{} is not declared", id.0),
            Self::Register(reg) => write!(f, "control instruction uses undeclared register r{reg}"),
            Self::Kind { register, expected, actual } => write!(f, "control register r{register} requires {expected:?}, declares {actual:?}"),
            Self::Arguments { expected, actual } => write!(f, "control signature requires {expected} input values, got {actual}"),
            Self::EffectRegister => f.write_str("control instruction requires r0 declared as Unit; use ProgramBuilder::function_asm"),
            Self::RegisterSpace => f.write_str("control results exceed the virtual register address space"),
            Self::Target(label) => write!(f, "control outcome target {label:?} is not a declared typed block"),
            Self::BlockParameters { target, expected, actual } => write!(f, "control outcome requires {expected} parameters at {target:?}, but the block declares {actual}"),
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

fn require_parameters(
    body: &Asm,
    target: Label,
    count: usize,
) -> Result<&[u32], ControlBuildError> {
    let parameters = body
        .block_params
        .get(target.0 as usize)
        .and_then(Option::as_ref)
        .ok_or(ControlBuildError::Target(target))?;
    if parameters.len() != count {
        return Err(ControlBuildError::BlockParameters {
            target,
            expected: count,
            actual: parameters.len(),
        });
    }
    Ok(parameters)
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
    /// Declares the normal successor of a resume with its typed final-result parameters.
    pub fn result_block(
        &self,
        body: &mut Asm,
        signature: CallSigId,
    ) -> Result<ResultBlock, ControlBuildError> {
        let entry = self
            .call_sigs
            .get(signature.0 as usize)
            .ok_or(ControlBuildError::Signature(signature))?;
        self.control_capacity(body, entry.rets.len())?;
        let results: Vec<_> = entry
            .rets
            .iter()
            .map(|&id| {
                body.alloc_register(self.contracts.table().kind(id).expect("declared contract"))
            })
            .collect();
        let label = body.block(&results);
        Ok(ResultBlock { results, label })
    }

    /// Declares a local handler block and allocates its request and continuation parameters.
    ///
    /// Declare clauses before emitting their resume, then place their labels where the clause
    /// bodies belong. Local clauses share lexical registers and do not introduce call frames.
    pub fn handler_block(
        &self,
        body: &mut Asm,
        tag: u32,
        signature: CallSigId,
    ) -> Result<HandlerBlock, ControlBuildError> {
        require(body, tag, ValueType::Tag)?;
        let entry = self
            .call_sigs
            .get(signature.0 as usize)
            .ok_or(ControlBuildError::Signature(signature))?;
        self.control_capacity(body, entry.args.len() + 1)?;
        let request: Vec<_> = entry
            .args
            .iter()
            .map(|&contract| {
                body.alloc_register(
                    self.contracts
                        .table()
                        .kind(contract)
                        .expect("declared contract"),
                )
            })
            .collect();
        let continuation = body.alloc_register(ValueType::Continuation);
        let mut parameters = request.clone();
        parameters.push(continuation);
        let label = body.block(&parameters);
        Ok(HandlerBlock {
            arm: HandlerArm::Suspend {
                tag,
                signature,
                target: label,
            },
            request,
            continuation,
            label,
        })
    }

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

    /// Terminates this block by resuming a computation with typed normal and clause successors.
    ///
    /// `signature` describes the inputs expected at this suspension, independently of the
    /// computation's original arguments. Each suspension clause receives request values followed
    /// by a continuation in its local block. Dynamic tag contracts and
    /// one-use authority are checked by execution. Authoring errors leave the body unchanged.
    pub fn emit_resume(
        &self,
        body: &mut Asm,
        continuation: u32,
        signature: CallSigId,
        args: &[u32],
        returned: Label,
        handlers: &[HandlerArm],
    ) -> Result<(), ControlBuildError> {
        require(body, continuation, ValueType::Continuation)?;
        for arm in handlers {
            match *arm {
                HandlerArm::Suspend {
                    tag,
                    signature,
                    target,
                } => {
                    require(body, tag, ValueType::Tag)?;
                    let protocol = self
                        .call_sigs
                        .get(signature.0 as usize)
                        .ok_or(ControlBuildError::Signature(signature))?;
                    let parameters = require_parameters(body, target, protocol.args.len() + 1)?;
                    for (&register, kind) in parameters.iter().zip(
                        protocol
                            .args
                            .iter()
                            .map(|&id| self.contracts.table().kind(id).expect("declared contract"))
                            .chain(core::iter::once(ValueType::Continuation)),
                    ) {
                        require(body, register, kind)?;
                    }
                }
            }
        }
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
        for (&register, &id) in args.iter().zip(&entry.args) {
            require(
                body,
                register,
                self.contracts.table().kind(id).expect("declared contract"),
            )?;
        }
        let parameters = require_parameters(body, returned, entry.rets.len())?;
        for (&register, &id) in parameters.iter().zip(&entry.rets) {
            require(
                body,
                register,
                self.contracts.table().kind(id).expect("declared contract"),
            )?;
        }
        self.control_capacity(body, 0)?;
        body.cont_resume(continuation, signature, args, returned, handlers);
        Ok(())
    }

    /// Suspends with a typed request and allocates the registers for its eventual response.
    ///
    /// Handler lookup and capture are VM operations. The enclosing computation resumes only when
    /// its one-use capability is consumed, possibly under a different handler context.
    /// Authoring errors leave the body unchanged.
    pub fn emit_suspend(
        &self,
        body: &mut Asm,
        tag: u32,
        signature: CallSigId,
        request: &[u32],
    ) -> Result<Vec<u32>, ControlBuildError> {
        require(body, tag, ValueType::Tag)?;
        let response = self.control_results(body, signature, request)?;
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

    fn control_results(
        &self,
        body: &mut Asm,
        signature: CallSigId,
        args: &[u32],
    ) -> Result<Vec<u32>, ControlBuildError> {
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
        self.control_capacity(body, entry.rets.len())?;
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
        Ok(entry
            .rets
            .iter()
            .map(|&contract| {
                body.alloc_register(
                    self.contracts
                        .table()
                        .kind(contract)
                        .expect("declared contract"),
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspension_destinations_follow_runtime_sized_protocols() {
        for response in [
            vec![],
            vec![ValueType::Bool],
            vec![ValueType::I64, ValueType::Str],
        ] {
            let mut builder = ProgramBuilder::new();
            let protocol = builder
                .call_sig(FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: response.clone(),
                })
                .unwrap();
            let entry = builder
                .declare_function(FunctionSig {
                    arg_types: vec![ValueType::Tag, ValueType::I64],
                    ret_types: response.clone(),
                })
                .unwrap();
            let mut body = builder.function_asm(entry).unwrap();
            let tag = builder.argument(entry, 0).unwrap();
            let request = builder.argument(entry, 1).unwrap();
            let before = body.register_types.len();
            let results = builder
                .emit_suspend(&mut body, tag, protocol, &[request])
                .unwrap();
            assert_eq!(results.len(), response.len());
            assert_eq!(&body.register_types[before..], response);
            body.ret(0, &results);
            builder.define_function(entry, body).unwrap();
            builder.build_verified().unwrap();
        }
    }

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
        let callable = body.alloc_register(ValueType::Func);
        let returned = builder.result_block(&mut body, signature).unwrap();
        let wrong_results = body.block(&[closure, continuation]);
        let bytes = body.bytes.clone();
        let ledger = body.register_types.clone();
        assert!(
            matches!(builder.emit_resume(&mut body, continuation, signature, &[closure], returned.label(), &[]),
            Err(ControlBuildError::Kind { register, expected: ValueType::Func, actual: ValueType::Closure }) if register == closure)
        );
        assert_eq!(body.bytes, bytes);
        assert_eq!(body.register_types, ledger);
        assert!(matches!(
            builder.emit_resume(
                &mut body,
                continuation,
                signature,
                &[callable],
                wrong_results,
                &[]
            ),
            Err(ControlBuildError::BlockParameters {
                expected: 1,
                actual: 2,
                ..
            })
        ));
        assert_eq!(body.bytes, bytes);
        assert_eq!(body.register_types, ledger);
    }
}
