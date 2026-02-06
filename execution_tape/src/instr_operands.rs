// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Instruction operand visitors (generated).
//!
//! These helpers are generated from `execution_tape/opcodes.json` and let verifier/analysis code
//! query instruction operand subsets (PC targets, id operands, etc.) without hardcoding operand
//! positions.

#![allow(
    dead_code,
    reason = "generated operand visitors are used incrementally"
)]

include!("instr_operands_gen.rs");

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::vec::Vec;

    use crate::bytecode::Instr;
    use crate::instr_operands;
    use crate::program::{ConstId, ElemTypeId, HostSigId, TypeId};
    use crate::value::FuncId;

    #[test]
    fn visit_pcs_finds_targets() {
        let instr = Instr::Br {
            cond: 1,
            pc_true: 10,
            pc_false: 20,
        };
        let mut pcs: Vec<u32> = Vec::new();
        instr_operands::visit_pcs(&instr, |pc| pcs.push(pc));
        assert_eq!(pcs.as_slice(), &[10, 20]);
    }

    #[test]
    fn visit_ids_find_operands() {
        let mut ids: Vec<u32> = Vec::new();

        instr_operands::visit_const_ids(
            &Instr::ConstPool {
                dst: 0,
                idx: ConstId(7),
            },
            |id| {
                ids.push(id.0);
            },
        );
        instr_operands::visit_func_ids(
            &Instr::Call {
                eff_out: 0,
                func_id: FuncId(1),
                eff_in: 0,
                args: Vec::new(),
                rets: Vec::new(),
            },
            |id| ids.push(id.0),
        );
        instr_operands::visit_host_sig_ids(
            &Instr::HostCall {
                eff_out: 0,
                host_sig: HostSigId(2),
                eff_in: 0,
                args: Vec::new(),
                rets: Vec::new(),
            },
            |id| ids.push(id.0),
        );
        instr_operands::visit_type_ids(
            &Instr::StructNew {
                dst: 0,
                type_id: TypeId(3),
                values: Vec::new(),
            },
            |id| ids.push(id.0),
        );
        instr_operands::visit_elem_type_ids(
            &Instr::ArrayNew {
                dst: 0,
                elem_type_id: ElemTypeId(4),
                len: 0,
                values: Vec::new(),
            },
            |id| ids.push(id.0),
        );

        assert_eq!(ids.as_slice(), &[7, 1, 2, 3, 4]);
    }
}
