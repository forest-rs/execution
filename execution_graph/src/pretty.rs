// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Pretty-printing and Graphviz DOT export for [`ExecutionGraph`].

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use execution_tape::host::Host;

use crate::graph::{Binding, ExecutionGraph, Node};

fn escape_record(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 8);
    for ch in value.chars() {
        match ch {
            '|' => out.push_str("\\|"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '<' => out.push_str("\\<"),
            '>' => out.push_str("\\>"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
    out
}

fn record_inputs(node: &Node) -> String {
    if node.input_names.is_empty() {
        return String::new();
    }

    let mut parts: Vec<String> = Vec::with_capacity(node.input_names.len());
    for (i, input_name) in node.input_names.iter().enumerate() {
        let rendered_raw = match node.inputs.get(i).and_then(Option::as_ref) {
            Some(Binding::External(_)) => format!("{input_name} (external)"),
            Some(Binding::FromNode { node, output }) => {
                format!("{input_name} <- {}.{output}", node.as_u64())
            }
            None => format!("{input_name} (unbound)"),
        };
        let rendered = escape_record(&rendered_raw);
        parts.push(format!("<in{i}> {rendered}"));
    }
    format!("{{ {} }}", parts.join(" | "))
}

fn record_outputs(node: &Node) -> String {
    if node.output_names.is_empty() {
        return String::new();
    }

    let mut parts: Vec<String> = Vec::with_capacity(node.output_names.len());
    for (i, output_name) in node.output_names.iter().enumerate() {
        parts.push(format!("<out{i}> {}", escape_record(output_name)));
    }
    format!("{{ {} }}", parts.join(" | "))
}

fn output_slot(node: &Node, output_name: &str) -> Option<usize> {
    node.output_names
        .iter()
        .position(|candidate| candidate.as_ref() == output_name)
}

impl<H: Host> ExecutionGraph<H> {
    /// Renders the graph as Graphviz DOT.
    ///
    /// Nodes are rendered as record-shaped boxes with one input and output port per declared
    /// argument/return. If a connection references a non-existent upstream output name, the edge
    /// is still emitted as a dashed fallback edge so broken wiring remains visible.
    #[must_use]
    pub fn to_dot(&self) -> String {
        let mut dot = String::from(
            "digraph ExecutionGraph {\n\
             \trankdir=LR;\n\
             \tranksep=1.1;\n\
             \tnodesep=0.7;\n\
             \tnode [shape=record, fontname=\"monospace\", fontsize=10, margin=\"0.08,0.04\"];\n\
             \tedge [fontname=\"monospace\", fontsize=9, arrowsize=0.7];\n",
        );

        for (node_id, node) in self.nodes.iter().enumerate() {
            let input_block = record_inputs(node);
            let output_block = record_outputs(node);
            let program = node.program.program();
            let node_line = match program.name() {
                Some(name) => format!("node#{node_id} ({name})"),
                None => format!("node#{node_id}"),
            };
            let entry_line = match program.function_name(node.entry.0) {
                Some(name) => format!("entry=f{} ({name})", node.entry.0),
                None => format!("entry=f{}", node.entry.0),
            };
            let center = escape_record(&format!("{node_line}\n{entry_line}"));

            let label = match (input_block.is_empty(), output_block.is_empty()) {
                (true, true) => center,
                (true, false) => format!("{{ {center} | {output_block} }}"),
                (false, true) => format!("{{ {input_block} | {center} }}"),
                (false, false) => format!("{{ {input_block} | {center} | {output_block} }}"),
            };

            let _ = writeln!(dot, "  n{node_id} [label=\"{label}\"];");
        }

        for (dst_id, node) in self.nodes.iter().enumerate() {
            for (dst_slot, _input_name) in node.input_names.iter().enumerate() {
                let Some(Binding::FromNode {
                    node: src_node,
                    output,
                }) = node.inputs.get(dst_slot).and_then(Option::as_ref)
                else {
                    continue;
                };

                let src_id = src_node.as_u64();
                let src_slot = usize::try_from(src_id)
                    .ok()
                    .and_then(|src_index| self.nodes.get(src_index))
                    .and_then(|src| output_slot(src, output.as_ref()));

                match src_slot {
                    Some(src_slot) => {
                        let _ =
                            writeln!(dot, "  n{src_id}:out{src_slot} -> n{dst_id}:in{dst_slot};");
                    }
                    None => {
                        let label = escape_record(output);
                        let _ = writeln!(
                            dot,
                            "  n{src_id} -> n{dst_id}:in{dst_slot} [label=\"{label}\", style=dashed, color=\"firebrick\"];"
                        );
                    }
                }
            }
        }

        dot.push_str("}\n");
        dot
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use alloc::vec;
    use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
    use execution_tape::host::{AccessSink, Host, HostError, SigHash, ValueRef};
    use execution_tape::program::ValueType;
    use execution_tape::value::{FuncId, Value};
    use execution_tape::verifier::VerifiedProgram;
    use execution_tape::vm::Limits;

    #[derive(Debug, Default)]
    struct HostNoop;

    impl Host for HostNoop {
        fn call(
            &mut self,
            _symbol: &str,
            _sig_hash: SigHash,
            _args: &[ValueRef<'_>],
            _access: Option<&mut dyn AccessSink>,
        ) -> Result<(Vec<Value>, u64), HostError> {
            Err(HostError::UnknownSymbol)
        }
    }

    fn make_identity_program(output_name: &str) -> (VerifiedProgram, FuncId) {
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        a.ret(0, &[1]);
        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::I64],
                    reg_count: 2,
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, output_name).unwrap();
        (pb.build_verified().unwrap(), f)
    }

    #[test]
    fn to_dot_renders_ports_and_wired_edges() {
        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let (a_prog, a_entry) = make_identity_program("subtotal");
        let (b_prog, b_entry) = make_identity_program("total");

        let na = g.add_node(a_prog, a_entry, vec!["qty".into()]);
        let nb = g.add_node(b_prog, b_entry, vec!["subtotal".into()]);
        g.set_input_value(na, "qty", Value::I64(2));
        g.connect(na, "subtotal", nb, "subtotal");

        let dot = g.to_dot();

        assert!(dot.contains("digraph ExecutionGraph {"));
        assert!(dot.contains("<in0> qty (external)"));
        assert!(dot.contains("<in0> subtotal \\<- 0.subtotal"));
        assert!(dot.contains("<out0> subtotal"));
        assert!(dot.contains("n0:out0 -> n1:in0;"));
    }

    #[test]
    fn to_dot_marks_unknown_upstream_outputs_with_dashed_edges() {
        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let (a_prog, a_entry) = make_identity_program("subtotal");
        let (b_prog, b_entry) = make_identity_program("total");

        let na = g.add_node(a_prog, a_entry, vec!["qty".into()]);
        let nb = g.add_node(b_prog, b_entry, vec!["subtotal".into()]);
        g.set_input_value(na, "qty", Value::I64(2));
        g.connect(na, "missing", nb, "subtotal");

        let dot = g.to_dot();

        assert!(
            dot.contains("n0 -> n1:in0 [label=\"missing\", style=dashed, color=\"firebrick\"];")
        );
    }

    #[test]
    fn to_dot_includes_program_and_function_names_when_available() {
        let mut pb = ProgramBuilder::new();
        pb.set_program_name("named_program");
        let mut a = Asm::new();
        a.ret(0, &[1]);
        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::I64],
                    reg_count: 2,
                },
            )
            .unwrap();
        pb.set_function_name(f, "named_entry").unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();
        let prog = pb.build_verified().unwrap();

        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let n = g.add_node(prog, f, vec!["x".into()]);
        g.set_input_value(n, "x", Value::I64(1));

        let dot = g.to_dot();
        assert!(dot.contains("node#0 (named_program)"));
        assert!(dot.contains("entry=f0 (named_entry)"));
    }
}
