// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// After you edit the crate's doc comment, regenerate README.md by running:
// cargo rdme --workspace-project=execution_tape --heading-base-level=0

//! Portable, verifiable bytecode container format and register VM runtime (draft).
//!
//! `execution_tape` is the low-level execution layer for already-lowered programs. It owns the
//! portable program format, verifier, register VM, host-call ABI, aggregate values, tracing hooks,
//! and disassembly tools. It does not own language semantics, graph authoring, or host object
//! lifetimes.
//!
//! The crate is `no_std + alloc` by default. The `std` feature is currently reserved for
//! integrations that need standard-library support.
//!
//! ## Quick Start
//!
//! Build, verify, and run a one-function program:
//!
//! ```rust
//! extern crate alloc;
//!
//! use alloc::vec;
//!
//! use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
//! use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
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
//!         _ctx: HostContext<'_, '_>,
//!     ) -> Result<u64, HostError> {
//!         Err(HostError::UnknownSymbol)
//!     }
//! }
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut asm = Asm::new();
//!     asm.const_i64(2, 1);
//!     asm.i64_add(3, 1, 2);
//!     asm.ret(0, &[3]);
//!
//!     let mut builder = ProgramBuilder::new();
//!     builder.set_program_name("add_one");
//!     let entry = builder.push_function_checked(
//!         asm,
//!         FunctionSig {
//!             arg_types: vec![ValueType::I64],
//!             ret_types: vec![ValueType::I64],
//!         },
//!     )?;
//!     builder.set_function_input_name(entry, 0, "x")?;
//!     builder.set_function_output_name(entry, 0, "y")?;
//!
//!     let program = builder.build_verified()?;
//!     let mut vm = Vm::new(NoHost, Limits::default());
//!     let out = vm.run(&program, entry, &[Value::I64(41)], TraceMask::NONE, None)?;
//!     assert_eq!(out, vec![Value::I64(42)]);
//!     Ok(())
//! }
//! ```
//!
//! ## Core Pieces
//!
//! - `asm`: ergonomic builders for functions, call signatures, constants, host signatures, and
//!   bytecode emission.
//! - `program`: serialized program model, type tables, constants, host signatures, and names.
//! - `verifier`: validation and lowering into an execution-ready `VerifiedProgram`.
//! - `vm`: bounded interpreter for verified programs.
//! - `host`: host-call trait, borrowed argument views, aggregate readers, and access recording
//!   hooks.
//! - `trace`: low-overhead tracing events for profiling and diagnostics.
//! - `disasm`: human-readable disassembly for verified programs.
//!
//! ## Design Docs
//!
//! The repository-level design notes live outside the packaged crate:
//!
//! - <https://github.com/forest-rs/execution/blob/main/docs/overview.md>
//! - <https://github.com/forest-rs/execution/blob/main/docs/v1_spec.md>
//!
//! ## Examples
//!
//! Print disassembly for a small branching program:
//!
//! ```sh
//! cargo run -p execution_tape --example disasm
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
pub mod program_memory;
pub mod trace;
pub(crate) mod typed;
pub mod value;
pub mod verifier;
pub mod vm;
