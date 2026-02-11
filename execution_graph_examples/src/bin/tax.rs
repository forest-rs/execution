// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A small runnable `execution_graph` example.
//!
//! Shows:
//! - Host calls recording dependency keys
//! - Incremental reruns after invalidating an input
//! - A best-effort “why re-ran” cause path per executed node

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use core::cell::RefCell;

use execution_graph::{ExecutionGraph, GraphError, NodeId, ReportDetailMask, ResourceKey};
use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{
    AccessSink, Host, HostError, HostSig, ResourceKeyRef, SigHash, ValueRef, sig_hash,
};
use execution_tape::program::ValueType;
use execution_tape::value::{FuncId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::Limits;

#[derive(Debug)]
struct TaxHost {
    rate_bp: Rc<RefCell<i64>>,
}

impl TaxHost {
    const TAX_RATE_KEY: u64 = 0;
}

impl Host for TaxHost {
    fn call(
        &mut self,
        symbol: &str,
        sig_hash: SigHash,
        _args: &[ValueRef<'_>],
        mut access: Option<&mut dyn AccessSink>,
    ) -> Result<(Vec<Value>, u64), HostError> {
        match symbol {
            "tax_rate_bp" => {
                if let Some(sink) = access.as_mut() {
                    sink.read(ResourceKeyRef::HostState {
                        op: sig_hash,
                        key: Self::TAX_RATE_KEY,
                    });
                }
                Ok((vec![Value::I64(*self.rate_bp.borrow())], 0))
            }
            _ => Err(HostError::UnknownSymbol),
        }
    }
}

fn program_price_subtotal() -> (VerifiedProgram, FuncId) {
    // fn price_subtotal(qty: i64, unit_price: i64) -> i64 { qty * unit_price }
    let mut pb = ProgramBuilder::new();
    pb.set_program_name("tax_price_subtotal_program");
    let mut a = Asm::new();
    a.i64_mul(3, 1, 2);
    a.ret(0, &[3]);

    let f = pb
        .push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![ValueType::I64, ValueType::I64],
                ret_types: vec![ValueType::I64],
                reg_count: 4,
            },
        )
        .unwrap();
    pb.set_function_name(f, "price_subtotal").unwrap();
    pb.set_function_output_name(f, 0, "subtotal").unwrap();
    (pb.build_verified().unwrap(), f)
}

fn program_tax_amount() -> (VerifiedProgram, FuncId) {
    // fn tax_amount(subtotal: i64) -> i64 { subtotal * host.tax_rate_bp() / 10000 }
    let mut pb = ProgramBuilder::new();
    pb.set_program_name("tax_amount_program");
    let sig = HostSig {
        args: vec![],
        rets: vec![ValueType::I64],
    };
    let host_sig = pb.host_sig_for("tax_rate_bp", sig.clone());

    let mut a = Asm::new();
    a.host_call(0, host_sig, 0, &[], &[2]); // rate_bp in r2
    a.i64_mul(3, 1, 2); // subtotal * rate
    a.const_i64(4, 10_000);
    a.i64_div(5, 3, 4);
    a.ret(0, &[5]);

    let f = pb
        .push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
                reg_count: 6,
            },
        )
        .unwrap();
    pb.set_function_name(f, "tax_amount").unwrap();
    pb.set_function_output_name(f, 0, "tax").unwrap();
    (pb.build_verified().unwrap(), f)
}

fn program_total() -> (VerifiedProgram, FuncId) {
    // fn total(subtotal: i64, tax: i64) -> i64 { subtotal + tax }
    let mut pb = ProgramBuilder::new();
    pb.set_program_name("tax_total_program");
    let mut a = Asm::new();
    a.i64_add(3, 1, 2);
    a.ret(0, &[3]);

    let f = pb
        .push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![ValueType::I64, ValueType::I64],
                ret_types: vec![ValueType::I64],
                reg_count: 4,
            },
        )
        .unwrap();
    pb.set_function_name(f, "total").unwrap();
    pb.set_function_output_name(f, 0, "total").unwrap();
    (pb.build_verified().unwrap(), f)
}

fn label_for(node: NodeId) -> &'static str {
    match node.as_u64() {
        0 => "price_subtotal",
        1 => "tax_amount",
        2 => "total",
        _ => "<unknown>",
    }
}

fn fmt_key(node_labels: &BTreeMap<u64, &'static str>, key: &ResourceKey) -> String {
    match key {
        ResourceKey::Input(name) => format!("Input({name})"),
        ResourceKey::TapeOutput { node, output } => {
            let label = node_labels
                .get(&node.as_u64())
                .copied()
                .unwrap_or("<unknown>");
            format!("TapeOutput({label}:{output})")
        }
        ResourceKey::HostState { op, key } => format!("HostState(op={}, key={key})", op.as_u64()),
        ResourceKey::OpaqueHost(op) => format!("OpaqueHost(op={})", op.as_u64()),
    }
}

fn main() -> Result<(), GraphError> {
    let emit_dot = std::env::args().skip(1).any(|arg| arg == "--dot");

    let rate_bp: Rc<RefCell<i64>> = Rc::new(RefCell::new(825));
    let mut g = ExecutionGraph::new(
        TaxHost {
            rate_bp: rate_bp.clone(),
        },
        Limits::default(),
    );
    g.set_strict_deps(true);

    let (p_prog, p_entry) = program_price_subtotal();
    let (t_prog, t_entry) = program_tax_amount();
    let (sum_prog, sum_entry) = program_total();

    let n_price = g.add_node(p_prog, p_entry, vec!["qty".into(), "unit_price".into()]);
    let n_tax = g.add_node(t_prog, t_entry, vec!["subtotal".into()]);
    let n_total = g.add_node(sum_prog, sum_entry, vec!["subtotal".into(), "tax".into()]);

    let node_labels: BTreeMap<u64, &'static str> = BTreeMap::from([
        (n_price.as_u64(), label_for(n_price)),
        (n_tax.as_u64(), label_for(n_tax)),
        (n_total.as_u64(), label_for(n_total)),
    ]);

    g.set_input_value(n_price, "qty", Value::I64(2));
    g.set_input_value(n_price, "unit_price", Value::I64(120));

    g.connect(n_price, "subtotal", n_tax, "subtotal");
    g.connect(n_price, "subtotal", n_total, "subtotal");
    g.connect(n_tax, "tax", n_total, "tax");

    if emit_dot {
        g.run_all()?;
        println!("{}", g.to_dot());
        return Ok(());
    }

    let _ = g.run_all_with_report(ReportDetailMask::FULL)?;
    let total0 = g
        .node_outputs(n_total)
        .and_then(|o| o.get("total"))
        .cloned();
    println!("🟩 total (first run): {total0:?}");

    // Change 1: the order quantity changes.
    println!();
    println!("🟦 change: qty");
    g.set_input_value(n_price, "qty", Value::I64(3));
    g.invalidate_input("qty");

    let report = g.run_all_with_report(ReportDetailMask::FULL)?;
    let total1 = g
        .node_outputs(n_total)
        .and_then(|o| o.get("total"))
        .cloned();
    println!("🟩 total (after qty change): {total1:?}");

    println!();
    println!("🟧 why re-ran after qty change (one plausible path per executed node):");
    println!("  🟫 note: this is not an exhaustive explanation; other causes may exist.");
    for r in report.executed {
        let label = node_labels
            .get(&r.node.as_u64())
            .copied()
            .unwrap_or("<unknown>");
        let because_of = r
            .because_of
            .as_ref()
            .map(|k| fmt_key(&node_labels, k))
            .unwrap_or_else(|| "<none>".to_string());
        println!(
            "  - {label} (node={}): because this is dirty: {because_of}",
            r.node.as_u64()
        );
        println!("    path:");
        for k in r.why_path.unwrap_or_default() {
            println!("      - {}", fmt_key(&node_labels, &k));
        }
    }

    // Change 2: the tax rate changes (host state). This should re-run only the nodes that depend
    // on that host state (tax_amount and total), not price_subtotal.
    println!();
    println!("🟦 change: tax rate (host state)");
    *rate_bp.borrow_mut() = 900;

    let tax_rate_sig = HostSig {
        args: vec![],
        rets: vec![ValueType::I64],
    };
    g.invalidate_tape_key(ResourceKeyRef::HostState {
        op: sig_hash(&tax_rate_sig),
        key: TaxHost::TAX_RATE_KEY,
    });

    let report = g.run_all_with_report(ReportDetailMask::FULL)?;
    let total2 = g
        .node_outputs(n_total)
        .and_then(|o| o.get("total"))
        .cloned();
    println!();
    println!("🟩 total (after tax rate change): {total2:?}");

    println!();
    println!("🟧 why re-ran after tax rate change (one plausible path per executed node):");
    println!("  🟫 note: this is not an exhaustive explanation; other causes may exist.");
    for r in report.executed {
        let label = node_labels
            .get(&r.node.as_u64())
            .copied()
            .unwrap_or("<unknown>");
        let because_of = r
            .because_of
            .as_ref()
            .map(|k| fmt_key(&node_labels, k))
            .unwrap_or_else(|| "<none>".to_string());
        println!(
            "  - {label} (node={}): because this is dirty: {because_of}",
            r.node.as_u64()
        );
        println!("    path:");
        for k in r.why_path.unwrap_or_default() {
            println!("      - {}", fmt_key(&node_labels, &k));
        }
    }

    Ok(())
}
