// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Inspect prepared functions in an encoded program without running it.
//!
//! `cargo run -p execution_tape_wind_tunnel --bin inspect_prepared -- program.et [function-id]`

use std::error::Error;

use execution_tape::program::Program;
use execution_tape::value::FuncId;
use execution_tape::verifier::{VerifyConfig, verify_program_owned};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: inspect_prepared program.et [function-id]")?;
    let selected: Option<u32> = args.next().map(|id| id.parse()).transpose()?;
    if args.next().is_some() {
        return Err("usage: inspect_prepared program.et [function-id]".into());
    }
    let bytes = std::fs::read(path)?;
    let program = verify_program_owned(Program::decode(&bytes)?, &VerifyConfig::default())?;
    println!(
        "function\tbytecode_bytes\tinstructions\toperands\tvirtual_registers\tregister_slots\tregister_bytes\tprepared_used_bytes\tprepared_capacity_bytes"
    );
    for function in program.functions() {
        let s = function.stats();
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            function.id().0,
            s.bytecode_bytes,
            s.instruction_count,
            s.operand_count,
            s.virtual_registers,
            s.register_slots,
            s.register_bytes,
            s.prepared_used_bytes,
            s.prepared_capacity_bytes
        );
    }
    if let Some(id) = selected {
        let function = program.function(FuncId(id)).ok_or("unknown function id")?;
        println!("contract_id\tdefinition");
        for (id, contract) in program.program().contracts.iter() {
            println!("{}\t{contract:?}", id.0);
        }
        println!("arguments\t{:?}", function.arg_types());
        println!("returns\t{:?}", function.ret_types());
        for (direction, contracts) in [
            ("argument", function.arg_contracts()),
            ("result", function.ret_contracts()),
        ] {
            for (index, &contract) in contracts.iter().enumerate() {
                println!("{direction}_contract\t{index}\t{}", contract.0);
            }
        }
        println!("pc\tnext_pc\topcode\tspan");
        for instruction in function.instructions() {
            let span = instruction
                .span
                .map_or_else(String::new, |id| id.get().to_string());
            println!(
                "{}\t{}\t{:?}\t{span}",
                instruction.pc, instruction.next_pc, instruction.opcode
            );
        }
    }
    Ok(())
}
