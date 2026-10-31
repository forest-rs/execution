// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Inspect prepared functions in an encoded program without running it.
//!
//! `cargo run -p execution_tape_examples --bin inspect_prepared -- program.et [function-id [pc]]`

use std::error::Error;

use execution_tape::program::Program;
use execution_tape::value::FuncId;
use execution_tape::verifier::{ProgramAnalysis, ValueFact, VerifyConfig, verify_program_owned};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: inspect_prepared program.et [function-id [pc]]")?;
    let selected: Option<u32> = args.next().map(|id| id.parse()).transpose()?;
    let pc: Option<u32> = args.next().map(|pc| pc.parse()).transpose()?;
    if args.next().is_some() {
        return Err("usage: inspect_prepared program.et [function-id [pc]]".into());
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
        if let Some(pc) = pc {
            let mut analysis = ProgramAnalysis::from(&program)
                .function(FuncId(id))
                .ok_or("unknown function id")?;
            let facts = analysis
                .instruction(pc)
                .ok_or("PC is not a source instruction boundary")?;
            println!("inferred_contract_id\tdefinition");
            for (id, contract) in facts.contracts().iter() {
                println!("{}\t{contract:?}", id.0);
            }
            println!("register\tstorage_kind\tbefore\tafter\torigin_before\torigin_after");
            for register in 0..function.stats().virtual_registers {
                let fact = facts.register(register).expect("declared register");
                println!(
                    "{register}\t{:?}\t{}\t{}\t{:?}\t{:?}",
                    fact.storage_kind,
                    value_fact(fact.before),
                    value_fact(fact.after),
                    fact.origin_before,
                    fact.origin_after
                );
            }
            println!("incoming_pc\tarm\tparameter\targument\tvalue");
            for edge in facts.incoming_edges() {
                for binding in edge.bindings() {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        edge.source_pc(),
                        edge.arm(),
                        binding.parameter,
                        binding.argument,
                        value_fact(binding.value)
                    );
                }
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

fn value_fact(fact: ValueFact) -> String {
    match fact {
        ValueFact::Unreachable => "unreachable".into(),
        ValueFact::Uninitialized => "uninitialized".into(),
        ValueFact::Contract(id) => format!("contract:{}", id.0),
    }
}
