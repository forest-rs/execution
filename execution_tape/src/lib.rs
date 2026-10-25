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
//!     asm.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::I64, ValueType::I64]);
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
//! For repeated execution, keep an [`ExecutionContext`](vm::ExecutionContext) and a result `Vec`.
//! Clear the result buffer between runs and call [`Vm::run_with_ctx`](vm::Vm::run_with_ctx)
//! to reuse both allocations. Results append in return-signature order; a trap preserves the
//! buffer's original contents. Aggregate results belong to the VM heap and survive context resets.
//!
//! ## Nested Guest Contracts
//!
//! `ValueType` remains the storage/ABI kind. Intern shared compositional contracts when guest
//! arguments/results need richer structure, then use the ordinary function builder and VM:
//!
//! ```rust
//! use execution_tape::asm::{FunctionAbi, ProgramBuilder};
//! use execution_tape::contract::Contract;
//! use execution_tape::program::ValueType;
//! let mut builder = ProgramBuilder::new();
//! let integer = builder.contract(Contract::Kind(ValueType::I64)).unwrap();
//! let inner = builder.contract(Contract::Tuple(&[Some(integer)])).unwrap();
//! let outer = builder.contract(Contract::Tuple(&[Some(inner)])).unwrap();
//! let signature = FunctionAbi { args: vec![], rets: vec![outer.into()] };
//! # let _ = signature;
//! ```
//!
//! Constructors and projections preserve nested facts; direct calls propagate enforced result
//! contracts. Joins retain common structure. Opaque boundary values are checked before callee entry
//! or result installation, using a separate bounded contract-work budget. Contract inspection
//! borrows definitions without expanding shared graphs. `Contract::Function` and `Contract::Closure`
//! describe exact exposed signatures. Their argument/result contract IDs also author indirect calls
//! through `ProgramBuilder::call_sig(FunctionAbi { ... })`. `Kind(Func)` and `Kind(Closure)` remain
//! opaque; exact callable matching implements no variance or subtyping. Closure environment layouts
//! stay hidden from their exposed signature and are checked against the concrete body before use.
//!
//! Run `cargo run -p execution_tape_wind_tunnel --bin nested_contracts` for a complete example.
//! Run `cargo run -p execution_tape_wind_tunnel --bin typed_callable` for an indirect nested result.
//!
//! ## Inspecting Verifier Facts
//!
//! `ProgramAnalysis::new(&program, &config)` verifies a borrowed source once. When a prepared
//! program already exists, `ProgramAnalysis::from(&prepared)` reuses its proof. Request one
//! function, then query source PCs without retaining register snapshots at every instruction:
//!
//! ```rust
//! use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
//! use execution_tape::program::ValueType;
//! use execution_tape::verifier::{ProgramAnalysis, ValueFact, VerifyConfig};
//! let mut body = Asm::new();
//! body.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::Agg]);
//! body.const_i64(1, 42).tuple_new(2, &[1]);
//! let pc = body.pc();
//! body.tuple_get(1, 2, 0).ret(0, &[1]);
//! let mut builder = ProgramBuilder::new();
//! let entry = builder.push_function_checked(body, FunctionSig {
//!     arg_types: vec![], ret_types: vec![ValueType::I64],
//! }).unwrap();
//! let program = builder.build_checked().unwrap();
//! let proof = ProgramAnalysis::new(&program, &VerifyConfig::default()).unwrap();
//! let mut function = proof.function(entry).unwrap();
//! let facts = function.instruction(pc).unwrap();
//! let aggregate = facts.register(2).unwrap();
//! assert_eq!(aggregate.storage_kind, ValueType::Agg);
//! assert!(matches!(aggregate.before, ValueFact::Contract(_)));
//! ```
//!
//! Facts distinguish unreachable code, uninitialized registers, opaque kinds, and nested
//! contracts. IDs belong to the analysis's shared contract table. Origins identify arguments,
//! local defining instructions or incoming block facts. Incoming edges expose simultaneous
//! parameter bindings before their meet; external invocation seeds are separate from CFG edges.
//! “After” means local source dataflow: calls assume successful callee return, while branches
//! precede their outgoing edge transfers. These facts are not runtime observations.
//! Ordinary verification, preparation and execution retain no inspection data.
//!
//! ## Core Pieces
//!
//! - `asm`: ergonomic builders for functions, call signatures, constants, host signatures, and
//!   bytecode emission.
//! - `contract`: shared compositional guest value contracts, authoring and borrowed kind views.
//! - `program`: serialized program model, type tables, constants, host signatures, and names.
//! - `verifier`: validation and lowering into an execution-ready `VerifiedProgram`.
//! - `vm`: bounded interpreter for verified programs.
//! - `host`: host-call trait, borrowed argument views, aggregate readers, and access recording
//!   hooks.
//! - `trace`: profiling events and opt-in borrowed invocation, host outcome and return-candidate
//!   values for recording and diagnostics.
//! - `recording`: owned bounded observations, instruction histories and checked replay.
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
//! cargo run -p execution_tape_wind_tunnel --bin disasm
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
pub mod contract;
pub mod disasm;
pub mod format;
pub mod host;
pub mod host_binding;
pub mod host_catalog;
pub(crate) mod instr_operands;
pub mod opcode;
pub mod program;
pub mod program_memory;
pub mod recording;
mod register_operands;
pub mod trace;
pub(crate) mod typed;
pub mod value;
pub mod verifier;
pub mod vm;
