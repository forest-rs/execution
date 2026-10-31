// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Closure layouts derived from one capture list and one exposed signature.

use super::*;

/// A program-local closure declaration, retained only while authoring code.
///
/// The ordered capture list determines an exact tuple environment. The caller-visible ABI
/// determines both the indirect-call signature and the body signature with its hidden environment.
/// Captures are immutable values; shared mutable lexical variables need explicit cells supplied
/// by the language runtime. This layout and its IDs belong to their original program builder.
#[derive(Clone, Debug)]
pub struct ClosureLayout {
    function: FuncId,
    signature: CallSigId,
    captures: Vec<ValueType>,
    arguments: usize,
}

/// Closure emission was denied before changing instruction bytes or register declarations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClosureBuildError {
    /// The supplied capture list or requested capture array has the wrong length.
    CaptureCount {
        /// Declared number of captures.
        expected: usize,
        /// Supplied or requested number.
        actual: usize,
    },
    /// A capture register has an incompatible declared storage kind.
    CaptureKind {
        /// Zero-based capture position.
        index: usize,
        /// Required kind.
        expected: ValueType,
        /// Actual register kind.
        actual: ValueType,
    },
    /// A capture register has no declaration.
    Register(u32),
    /// Loading captures requires the hidden aggregate argument at r1.
    Environment,
    /// New registers would exceed the u32 address space.
    RegisterSpace,
}
impl fmt::Display for ClosureBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CaptureCount { expected, actual } => write!(f, "closure declares {expected} captures, got {actual}"),
            Self::CaptureKind { index, expected, actual } => write!(f, "closure capture {index} requires {expected:?}, register declares {actual:?}"),
            Self::Register(register) => write!(f, "closure uses undeclared capture register r{register}"),
            Self::Environment => f.write_str("closure body requires its hidden aggregate environment at r1; start with function_asm(layout.function())"),
            Self::RegisterSpace => f.write_str("closure emission exceeds the u32 register address space"),
        }
    }
}
impl core::error::Error for ClosureBuildError {}

impl ClosureLayout {
    /// The declared body function, including the hidden environment argument.
    #[must_use]
    pub fn function(&self) -> FuncId {
        self.function
    }

    /// Exact caller-visible signature for [`CallTarget::Indirect`].
    #[must_use]
    pub fn call_signature(&self) -> CallSigId {
        self.signature
    }

    /// Source register of a caller-visible body argument, excluding the hidden environment.
    pub fn argument(&self, index: u32) -> Result<u32, BuildError> {
        if index as usize >= self.arguments {
            return Err(BuildError::BadArgIndex {
                func: self.function.0,
                arg: index,
                arg_count: u32::try_from(self.arguments).unwrap_or(u32::MAX),
            });
        }
        Ok(index
            .checked_add(2)
            .expect("closure argument register fits declared body"))
    }

    /// Loads captures into newly allocated registers, in declaration order.
    ///
    /// Destructure the returned array into meaningful local names. Start the body with
    /// `program.function_asm(layout.function())`; this helper emits typed tuple projections.
    /// Initialization and rich capture contracts still use ordinary verification and VM boundary checks.
    pub fn load_captures<const N: usize>(
        &self,
        body: &mut Asm,
    ) -> Result<[u32; N], ClosureBuildError> {
        self.check_count(N)?;
        if body.register_types.get(1) != Some(&ValueType::Agg) {
            return Err(ClosureBuildError::Environment);
        }
        Self::check_space(body, N)?;
        Ok(core::array::from_fn(|index| {
            let register = body.alloc_register(self.captures[index]);
            body.tuple_get(
                register,
                1,
                u32::try_from(index).expect("capture index fits environment"),
            );
            register
        }))
    }

    /// Constructs this closure from capture registers, allocating its environment and value.
    ///
    /// This emits ordinary `TupleNew`, `ConstFunc` and `ClosureNew` instructions.
    /// Execution-time allocations use the ordinary VM aggregate storage and resource budgets.
    /// Captures must be in declaration order. Use the resulting register with this layout's
    /// [`Self::call_signature`] or pass it to a declared closure argument.
    pub fn instantiate(&self, body: &mut Asm, captures: &[u32]) -> Result<u32, ClosureBuildError> {
        self.check_count(captures.len())?;
        for (index, register) in captures.iter().enumerate() {
            let actual = *body
                .register_types
                .get(*register as usize)
                .ok_or(ClosureBuildError::Register(*register))?;
            let expected = self.captures[index];
            if actual != expected {
                return Err(ClosureBuildError::CaptureKind {
                    index,
                    expected,
                    actual,
                });
            }
        }
        Self::check_space(body, 3)?;
        let environment = body.alloc_register(ValueType::Agg);
        let function = body.alloc_register(ValueType::Func);
        let closure = body.alloc_register(ValueType::Closure);
        body.tuple_new(environment, captures)
            .const_func(function, self.function)
            .closure_new(closure, function, environment);
        Ok(closure)
    }
    fn check_count(&self, actual: usize) -> Result<(), ClosureBuildError> {
        if actual != self.captures.len() {
            return Err(ClosureBuildError::CaptureCount {
                expected: self.captures.len(),
                actual,
            });
        }
        Ok(())
    }
    fn check_space(body: &Asm, additional: usize) -> Result<(), ClosureBuildError> {
        // A raw empty Asm reserves r0 on its first allocation.
        if body
            .register_types
            .len()
            .max(1)
            .checked_add(additional)
            .is_none_or(|count| count.saturating_sub(1) > u32::MAX as usize)
        {
            return Err(ClosureBuildError::RegisterSpace);
        }
        Ok(())
    }
}

impl ProgramBuilder {
    /// Declares a closure's ordered captures and caller-visible ABI once.
    ///
    /// Captures become exact tuple members; the body receives that environment before its
    /// visible arguments. The returned layout loads captures and constructs matching closure
    /// values. Keep it alongside the frontend's lexical names; no layout is retained by the VM.
    ///
    /// ```
    /// use execution_tape::{asm::{FunctionSig, ProgramBuilder}, program::ValueType};
    /// let mut program = ProgramBuilder::new();
    /// let increment = program.declare_closure(&[ValueType::I64.into()], FunctionSig {
    ///     arg_types: vec![ValueType::I64], ret_types: vec![ValueType::I64],
    /// })?;
    /// let mut body = program.function_asm(increment.function())?;
    /// let [captured] = increment.load_captures(&mut body)?;
    /// let argument = increment.argument(0)?;
    /// let answer = body.alloc_register(ValueType::I64);
    /// body.i64_add(answer, captured, argument).ret(0, &[answer]);
    /// program.define_function(increment.function(), body)?;
    /// program.build_verified()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn declare_closure(
        &mut self,
        captures: &[ContractRef],
        abi: impl Into<FunctionAbi>,
    ) -> Result<ClosureLayout, ContractBuildError> {
        let abi = abi.into();
        // Validate explicit references across both inputs before interning new contracts.
        for reference in captures.iter().chain(&abi.args).chain(&abi.rets) {
            if let ContractRef::Id(id) = reference
                && self.contracts.table().get(*id).is_none()
            {
                return Err(ContractBuildError::UnknownChild(*id));
            }
        }
        let (captures, _) = self.resolve_signature(FunctionAbi {
            args: captures.to_vec(),
            rets: vec![],
        })?;
        let environment = self.contract(Contract::Tuple(
            &captures.iter().copied().map(Some).collect::<Vec<_>>(),
        ))?;
        let arguments = abi.args.len();
        let signature = self.call_sig(&abi)?;
        let mut body_abi = abi;
        body_abi.args.insert(0, environment.into());
        let function = self.declare_function(body_abi)?;
        let captures = captures
            .into_iter()
            .map(|id| self.contracts.table().kind(id).expect("capture contract"))
            .collect();
        Ok(ClosureLayout {
            function,
            signature,
            captures,
            arguments,
        })
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
    struct NoHost;
    impl Host for NoHost {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[ValueRef<'_>],
            _: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            Err(HostError::UnknownSymbol)
        }
    }

    #[test]
    fn one_layout_constructs_loads_and_calls_rich_captures_with_visible_arguments() {
        let mut program = ProgramBuilder::new();
        let integer = program.contract(Contract::Kind(ValueType::I64)).unwrap();
        let tuple = program.contract(Contract::Tuple(&[Some(integer)])).unwrap();
        let increment = program
            .declare_closure(
                &[tuple.into()],
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        let mut body = program.function_asm(increment.function()).unwrap();
        let [captured_tuple] = increment.load_captures(&mut body).unwrap();
        let captured = body.alloc_register(ValueType::I64);
        let answer = body.alloc_register(ValueType::I64);
        body.tuple_get(captured, captured_tuple, 0)
            .i64_add(answer, captured, increment.argument(0).unwrap())
            .ret(0, &[answer]);
        program.define_function(increment.function(), body).unwrap();
        let entry = program
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let mut body = program.function_asm(entry).unwrap();
        let forty = body.alloc_register(ValueType::I64);
        let tuple = body.alloc_register(ValueType::Agg);
        let two = body.alloc_register(ValueType::I64);
        body.const_i64(forty, 40)
            .tuple_new(tuple, &[forty])
            .const_i64(two, 2);
        let closure = increment.instantiate(&mut body, &[tuple]).unwrap();
        let [answer] = program
            .emit_call(
                &mut body,
                CallTarget::Indirect {
                    signature: increment.call_signature(),
                    callee: closure,
                },
                &[two],
            )
            .unwrap();
        body.ret(0, &[answer]);
        program.define_function(entry, body).unwrap();
        let program = program.build_verified().unwrap();
        assert_eq!(
            Vm::new(NoHost, Limits::default())
                .run(&program, entry, &[], TraceMask::NONE, None)
                .unwrap(),
            [Value::I64(42)]
        );
    }

    #[test]
    fn capture_errors_do_not_mutate_assembly_and_visible_arguments_exclude_environment() {
        let mut program = ProgramBuilder::new();
        let block = program
            .declare_closure(
                &[ValueType::I64.into()],
                FunctionSig {
                    arg_types: vec![ValueType::Bool],
                    ret_types: vec![],
                },
            )
            .unwrap();
        assert_eq!(block.argument(0).unwrap(), 2);
        assert!(matches!(
            block.argument(1),
            Err(BuildError::BadArgIndex { arg_count: 1, .. })
        ));
        let mut body = program.function_asm(block.function()).unwrap();
        let before = body.clone().finish_parts().unwrap();
        assert!(matches!(
            block.instantiate(&mut body, &[]),
            Err(ClosureBuildError::CaptureCount { .. })
        ));
        assert!(matches!(
            block.instantiate(&mut body, &[2]),
            Err(ClosureBuildError::CaptureKind { index: 0, .. })
        ));
        assert!(matches!(
            block.instantiate(&mut body, &[99]),
            Err(ClosureBuildError::Register(99))
        ));
        let wrong_count: Result<[u32; 2], _> = block.load_captures(&mut body);
        assert!(wrong_count.is_err());
        let after = body.finish_parts().unwrap();
        assert_eq!(before.bytecode, after.bytecode);
        assert_eq!(before.register_types, after.register_types);
        let mut not_a_closure = Asm::new();
        let attempt: Result<[u32; 1], _> = block.load_captures(&mut not_a_closure);
        assert!(matches!(attempt, Err(ClosureBuildError::Environment)));
        assert!(not_a_closure.register_types.is_empty());
    }

    #[test]
    fn rich_capture_mismatch_is_rejected_at_the_ordinary_closure_call_boundary() {
        let mut program = ProgramBuilder::new();
        let integer = program.contract(Contract::Kind(ValueType::I64)).unwrap();
        let integer_tuple = program.contract(Contract::Tuple(&[Some(integer)])).unwrap();
        let block = program
            .declare_closure(
                &[integer_tuple.into()],
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![],
                },
            )
            .unwrap();
        let mut body = program.function_asm(block.function()).unwrap();
        body.ret(0, &[]);
        program.define_function(block.function(), body).unwrap();
        let entry = program
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            })
            .unwrap();
        let mut body = program.function_asm(entry).unwrap();
        let boolean = body.alloc_register(ValueType::Bool);
        let wrong_tuple = body.alloc_register(ValueType::Agg);
        body.const_bool(boolean, true)
            .tuple_new(wrong_tuple, &[boolean]);
        let closure = block.instantiate(&mut body, &[wrong_tuple]).unwrap();
        let [] = program
            .emit_call(
                &mut body,
                CallTarget::Indirect {
                    signature: block.call_signature(),
                    callee: closure,
                },
                &[],
            )
            .unwrap();
        body.ret(0, &[]);
        program.define_function(entry, body).unwrap();
        let program = program.build_verified().unwrap();
        let failure = Vm::new(NoHost, Limits::default())
            .run(&program, entry, &[], TraceMask::NONE, None)
            .unwrap_err();
        assert!(matches!(failure.trap, crate::vm::Trap::ContractMismatch {
            position: crate::contract::ContractPosition::Argument { callee, index: 0 }, ..
        } if callee == block.function()));
    }
}
