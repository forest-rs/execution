// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `execution_tape`: a portable, verifiable bytecode format and register VM runtime.
//!
//! This crate is in early design/implementation. The current v1 draft spec lives in:
//! - `docs/v1_spec.md`
//! - `docs/overview.md`
//!
//! ## Example
//!
//! ```no_run
//! extern crate alloc;
//!
//! use alloc::vec;
//! use alloc::vec::Vec;
//!
//! use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
//! use execution_tape::host::{AccessSink, Host, HostError, SigHash, ValueRef};
//! use execution_tape::program::ValueType;
//! use execution_tape::trace::TraceMask;
//! use execution_tape::value::Value;
//! use execution_tape::vm::{Limits, Vm};
//!
//! struct NoHost;
//!
//! impl Host for NoHost {
//!     fn call(
//!         &mut self,
//!         _symbol: &str,
//!         _sig_hash: SigHash,
//!         _args: &[ValueRef<'_>],
//!         _rets: &mut [Value],
//!         _access: Option<&mut dyn AccessSink>,
//!     ) -> Result<u64, HostError> {
//!         Err(HostError::UnknownSymbol)
//!     }
//! }
//!
//! let mut a = Asm::new();
//! a.const_i64(2, 1);
//! a.i64_add(3, 1, 2);
//! a.ret(0, &[3]);
//!
//! let mut pb = ProgramBuilder::new();
//! let entry = pb.push_function_checked(
//!     a,
//!     FunctionSig {
//!         arg_types: vec![ValueType::I64],
//!         ret_types: vec![ValueType::I64],
//!     },
//! )?;
//! pb.set_function_input_name(entry, 0, "x")?;
//! pb.set_function_output_name(entry, 0, "y")?;
//! let program = pb.build_verified()?;
//!
//! let mut vm = Vm::new(NoHost, Limits::default());
//! let out = vm
//!     .run(&program, entry, &[Value::I64(7)], TraceMask::NONE, None)
//!     .unwrap();
//! assert_eq!(out, vec![Value::I64(8)]);
//! # Ok::<(), execution_tape::asm::BuildError>(())
//! ```

#![no_std]

extern crate alloc;

pub mod aggregates;
pub(crate) mod analysis;
pub(crate) mod arena;
pub mod asm;
pub(crate) mod bytecode;
pub mod codec;
pub(crate) mod codec_primitives;
pub mod disasm;
pub mod format;
pub mod host;
pub mod host_catalog;
pub(crate) mod instr_operands;
pub mod opcode;
pub mod program;
pub mod trace;
pub(crate) mod typed;
pub mod value;
pub mod verifier;
pub mod vm;
