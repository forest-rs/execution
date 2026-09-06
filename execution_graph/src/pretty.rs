// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Pretty-printing and Graphviz DOT export for [`ExecutionGraph`].

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::executor::Executor;
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

fn record_inputs<X: Executor>(node: &Node<X>) -> String {
    if node.input_names.is_empty() {
        return String::new();
    }

    let mut parts: Vec<String> = Vec::with_capacity(node.input_names.len());
    for (i, input_name) in node.input_names.iter().enumerate() {
        let rendered_raw = match node.inputs.get(i).and_then(Option::as_ref) {
            Some(Binding::External { .. }) => format!("{input_name} (external)"),
            Some(Binding::FromNode { node, output, .. }) => {
                format!("{input_name} <- {}.{output}", node.as_u64())
            }
            None => format!("{input_name} (unbound)"),
        };
        let rendered = escape_record(&rendered_raw);
        parts.push(format!("<in{i}> {rendered}"));
    }
    format!("{{ {} }}", parts.join(" | "))
}

fn record_outputs<X: Executor>(node: &Node<X>) -> String {
    if node.output_names.is_empty() {
        return String::new();
    }

    let mut parts: Vec<String> = Vec::with_capacity(node.output_names.len());
    for (i, output_name) in node.output_names.iter().enumerate() {
        parts.push(format!("<out{i}> {}", escape_record(output_name)));
    }
    format!("{{ {} }}", parts.join(" | "))
}

fn output_slot<X: Executor>(node: &Node<X>, output_name: &str) -> Option<usize> {
    node.output_names
        .iter()
        .position(|candidate| candidate.as_ref() == output_name)
}

impl<X: Executor> ExecutionGraph<X> {
    /// Renders the graph as Graphviz DOT.
    ///
    /// Nodes are rendered as record-shaped boxes with one input and output port per declared
    /// input/output. The executor's [`describe`](Executor::describe) text, if any, is shown under
    /// the node id.
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
            let node_line = match node.label.as_deref() {
                Some(label) => format!("{label}\nnode#{node_id}"),
                None => format!("node#{node_id}"),
            };
            let center = match self.executor.describe(&node.body) {
                Some(description) => escape_record(&format!("{node_line}\n{description}")),
                None => escape_record(&node_line),
            };

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
                    ..
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

    use alloc::vec;

    use super::*;
    use crate::native::{FnExecutor, FnNode};

    fn identity_node(name: &str) -> FnNode<i64, ()> {
        FnNode::named(name, |inputs, outputs, _access| {
            outputs.push(inputs[0]);
            Ok(())
        })
    }

    #[test]
    fn to_dot_renders_ports_and_wired_edges() {
        let mut g = ExecutionGraph::new(FnExecutor::<i64, ()>::new());
        let na = g
            .add_node(
                identity_node("price"),
                vec!["qty".into()],
                vec!["subtotal".into()],
            )
            .unwrap();
        let nb = g
            .add_node(
                identity_node("sum"),
                vec!["subtotal".into()],
                vec!["total".into()],
            )
            .unwrap();
        g.set_input_value(na, "qty", 2).unwrap();
        g.connect(na, "subtotal", nb, "subtotal").unwrap();

        let dot = g.to_dot();

        assert!(dot.contains("digraph ExecutionGraph {"));
        assert!(dot.contains("<in0> qty (external)"));
        assert!(dot.contains("<in0> subtotal \\<- 0.subtotal"));
        assert!(dot.contains("<out0> subtotal"));
        assert!(dot.contains("n0:out0 -> n1:in0;"));
    }

    #[test]
    fn to_dot_includes_labels_and_executor_descriptions() {
        let mut g = ExecutionGraph::new(FnExecutor::<i64, ()>::new());
        let n = g
            .add_node(
                identity_node("described"),
                vec!["x".into()],
                vec!["value".into()],
            )
            .unwrap();
        g.set_node_label(n, "friendly node").unwrap();
        g.set_input_value(n, "x", 1).unwrap();

        let dot = g.to_dot();
        assert!(dot.contains("friendly node\\nnode#0\\ndescribed"));
    }
}
