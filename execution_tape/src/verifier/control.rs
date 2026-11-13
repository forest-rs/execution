// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Static checks for continuation instructions: `cont.new`, `resume` and `suspend`.
//!
//! A continuation register has the storage kind `Cont` only. Each instruction names the call
//! signature that types the continuation it consumes or produces: its arguments are the values
//! that resume it and its results the values it finally returns. The VM checks that type when
//! the instruction runs, as `call.indirect` checks its callee.

use super::{VerifyError, check_expected, tags};
use crate::bytecode::Instr;
use crate::contract::ContractKinds;
use crate::program::{CallSigId, Program, RegisterTypes, TagId, ValueType};

fn call_sig_in_bounds(program: &Program, func: u32, pc: u32, id: u32) -> Result<(), VerifyError> {
    if program.call_sig(CallSigId(id)).is_none() {
        return Err(VerifyError::CallSigOutOfBounds {
            func,
            pc,
            call_sig: id,
        });
    }
    Ok(())
}

fn tag_in_bounds(program: &Program, func: u32, pc: u32, tag: u32) -> Result<(), VerifyError> {
    if program.tag(TagId(tag)).is_none() {
        return Err(VerifyError::TagOutOfBounds { func, pc, tag });
    }
    Ok(())
}

/// Checks that every call signature and tag an instruction names exists.
pub(super) fn check_ids(
    program: &Program,
    func: u32,
    pc: u32,
    instr: &Instr,
) -> Result<(), VerifyError> {
    match instr {
        Instr::ContNew { call_sig, .. } | Instr::Resume { call_sig, .. } => {
            call_sig_in_bounds(program, func, pc, *call_sig)
        }
        Instr::Suspend { tag, .. } => tag_in_bounds(program, func, pc, *tag),
        _ => Ok(()),
    }
}

/// Argument and result contract counts of a verified call signature.
fn sig_arity(program: &Program, call_sig: u32) -> (usize, usize) {
    let entry = program
        .call_sig(CallSigId(call_sig))
        .expect("checked call signature");
    (entry.args.len as usize, entry.rets.len as usize)
}

/// Checks effect threading and operand counts.
///
/// `require_r0` rejects an effect operand other than `r0`, as for calls.
pub(super) fn check_structure(
    program: &Program,
    func: u32,
    pc: u32,
    instr: &Instr,
    require_r0: impl Fn(u32, u32) -> Result<(), VerifyError>,
) -> Result<(), VerifyError> {
    match instr {
        Instr::Resume {
            eff_out,
            call_sig,
            eff_in,
            args,
            rets,
            ..
        } => {
            require_r0(*eff_in, *eff_out)?;
            if (args.len(), rets.len()) != sig_arity(program, *call_sig) {
                return Err(VerifyError::CallArityMismatch { func, pc });
            }
        }
        Instr::Suspend {
            eff_out,
            tag,
            eff_in,
            args,
            rets,
        } => {
            require_r0(*eff_in, *eff_out)?;
            tags::check_throw_arity(program, func, pc, TagId(*tag), args.len())?;
            let entry = program.tag(TagId(*tag)).expect("checked tag");
            if rets.len() != entry.results.len as usize {
                return Err(VerifyError::CallArityMismatch { func, pc });
            }
        }
        _ => {}
    }
    Ok(())
}

/// Checks each register's declared kind against the instruction's signature or tag.
pub(super) fn check_types(
    program: &Program,
    func: u32,
    pc: u32,
    instr: &Instr,
    types: RegisterTypes<'_>,
) -> Result<(), VerifyError> {
    let t = |reg: u32| types.get(reg);
    let expect = |reg: u32, kind: ValueType| check_expected(func, pc, reg, t(reg), kind);
    let expect_all = |regs: &[u32], kinds: ContractKinds<'_>| {
        regs.iter()
            .zip(kinds.iter())
            .try_for_each(|(&reg, &kind)| expect(reg, kind))
    };
    let sig_kinds = |id: u32| {
        let entry = program.call_sig(CallSigId(id)).expect("checked call sig");
        let malformed = |_| VerifyError::CallIndirectSigMalformed {
            func,
            pc,
            call_sig: id,
        };
        let args = program.call_sig_args(entry).map_err(malformed)?;
        let rets = program.call_sig_rets(entry).map_err(malformed)?;
        Ok::<_, VerifyError>((args, rets))
    };
    let tag_results = |tag: u32| {
        let entry = program.tag(TagId(tag)).expect("checked tag");
        program
            .tag_results(entry)
            .map_err(|_| VerifyError::TagOutOfBounds { func, pc, tag })
    };
    match instr {
        Instr::ContNew {
            dst, func: callee, ..
        } => {
            expect(*dst, ValueType::Cont)?;
            if !matches!(t(*callee), Some(ValueType::Func | ValueType::Closure)) {
                expect(*callee, ValueType::Func)?;
            }
        }
        Instr::Resume {
            call_sig,
            cont,
            args,
            rets,
            ..
        } => {
            expect(*cont, ValueType::Cont)?;
            let (arg_kinds, ret_kinds) = sig_kinds(*call_sig)?;
            expect_all(args, arg_kinds)?;
            expect_all(rets, ret_kinds)?;
        }
        Instr::Suspend {
            tag, args, rets, ..
        } => {
            tags::check_throw_payload(program, func, pc, TagId(*tag), args, types)?;
            expect_all(rets, tag_results(*tag)?)?;
        }
        _ => {}
    }
    Ok(())
}
