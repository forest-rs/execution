// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Cached static edges in independent lanes; no claimed taken-path history.

use super::palette::LANES;
use execution_tape::disasm::FunctionDisassembly;
use ratatui::style::Style;
use ratatui::text::Span;

#[derive(Clone, Copy, Debug)]
struct Edge {
    from: u32,
    to: u32,
    arm: &'static str,
}
impl Edge {
    fn start(self) -> u32 {
        self.from.min(self.to)
    }
    fn end(self) -> u32 {
        self.from.max(self.to)
    }
}

#[derive(Default)]
pub(super) struct Flow {
    // Inclusive intervals in each lane never overlap, so one binary search finds a row's edge.
    lanes: [Vec<Edge>; 4],
    omitted: usize,
}

impl Flow {
    pub(super) fn new(source: &FunctionDisassembly<'_>) -> Self {
        let mut edges = Vec::new();
        for instruction in source.instrs() {
            let conditional = instruction.edge(1).is_some();
            for arm in 0..2 {
                if let Some(edge) = instruction.edge(arm) {
                    edges.push(Edge {
                        from: instruction.pc(),
                        to: edge.target,
                        arm: if conditional {
                            if arm == 0 { "T" } else { "F" }
                        } else {
                            "─"
                        },
                    });
                }
            }
        }
        Self::assign(edges)
    }

    fn assign(mut edges: Vec<Edge>) -> Self {
        edges.sort_unstable_by_key(|edge| {
            (
                edge.start(),
                core::cmp::Reverse(edge.end()),
                edge.from,
                edge.arm,
            )
        });
        let mut flow = Self::default();
        for edge in edges {
            if let Some(lane) = flow
                .lanes
                .iter_mut()
                .find(|lane| lane.last().is_none_or(|last| last.end() < edge.start()))
            {
                lane.push(edge);
            } else {
                flow.omitted += 1;
            }
        }
        flow
    }

    pub(super) fn omitted(&self) -> usize {
        self.omitted
    }

    pub(super) fn gutter(&self, pc: u32, first: u32, last: u32) -> Vec<Span<'static>> {
        let used = self
            .lanes
            .iter()
            .rposition(|lane| !lane.is_empty())
            .map_or(0, |lane| lane + 1);
        self.lanes
            .iter()
            .zip(LANES)
            .take(used)
            .map(|(lane, color)| {
                let index = lane.partition_point(|edge| edge.end() < pc);
                let symbol = match lane.get(index).filter(|edge| edge.start() <= pc) {
                    None => "  ".into(),
                    Some(edge) if edge.from == pc && edge.to == pc => {
                        if edge.arm == "─" {
                            "↺▶".into()
                        } else {
                            format!("↺{}", edge.arm)
                        }
                    }
                    Some(edge) if edge.from == pc => format!(
                        "{}{arm}",
                        if edge.from < edge.to { "╭" } else { "╰" },
                        arm = edge.arm
                    ),
                    Some(edge) if edge.to == pc => if edge.from < edge.to {
                        "╰▶"
                    } else {
                        "╭▶"
                    }
                    .into(),
                    Some(edge)
                        if pc == first
                            && pc == last
                            && edge.start() < first
                            && edge.end() > last =>
                    {
                        "↕ ".into()
                    }
                    Some(edge) if pc == first && edge.start() < first => "↑ ".into(),
                    Some(edge) if pc == last && edge.end() > last => "↓ ".into(),
                    Some(_) => "│ ".into(),
                };
                Span::styled(symbol, Style::default().fg(color))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(flow: &Flow, pc: u32, first: u32, last: u32) -> String {
        flow.gutter(pc, first, last)
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }
    #[test]
    fn forward_backward_and_clipped_edges_keep_source_target_and_direction() {
        let f = Flow::assign(vec![Edge {
            from: 10,
            to: 40,
            arm: "T",
        }]);
        assert_eq!(row(&f, 10, 10, 40), "╭T");
        assert_eq!(row(&f, 40, 10, 40), "╰▶");
        assert_eq!(row(&f, 20, 20, 30), "↑ ");
        assert_eq!(row(&f, 30, 20, 30), "↓ ");
        let f = Flow::assign(vec![Edge {
            from: 40,
            to: 10,
            arm: "─",
        }]);
        assert_eq!(row(&f, 10, 10, 40), "╭▶");
        assert_eq!(row(&f, 40, 10, 40), "╰─");
        assert_eq!(row(&f, 25, 10, 40), "│ ");
    }
    #[test]
    fn crossing_and_same_target_edges_stay_independent_and_lane_reuse_is_safe() {
        let f = Flow::assign(vec![
            Edge {
                from: 10,
                to: 30,
                arm: "T",
            },
            Edge {
                from: 10,
                to: 30,
                arm: "F",
            },
            Edge {
                from: 20,
                to: 40,
                arm: "─",
            },
            Edge {
                from: 50,
                to: 60,
                arm: "─",
            },
        ]);
        assert_eq!(row(&f, 10, 0, 60), "╭F╭T  ");
        assert_eq!(row(&f, 30, 0, 60), "╰▶╰▶│ ");
        assert_eq!(row(&f, 50, 0, 60), "╭─    ");
        let colors: Vec<_> = f
            .gutter(30, 0, 60)
            .into_iter()
            .map(|span| span.style.fg)
            .collect();
        assert_ne!(colors[0], colors[1]);
    }
    #[test]
    fn self_edges_and_excess_overlap_are_explicit_and_storage_stays_linear() {
        let f = Flow::assign(vec![Edge {
            from: 7,
            to: 7,
            arm: "─",
        }]);
        assert_eq!(row(&f, 7, 0, 10), "↺▶");
        let f = Flow::assign(
            (0..20_000)
                .map(|from| Edge {
                    from,
                    to: 30_000,
                    arm: "─",
                })
                .collect(),
        );
        assert_eq!(f.omitted(), 19_996);
        assert_eq!(f.lanes.iter().map(Vec::len).sum::<usize>(), 4);
        assert_eq!(row(&f, 25_000, 0, 30_000), "│ │ │ │ ");
    }

    use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
    use execution_tape::disasm::disassemble_function;
    use execution_tape::program::ValueType;

    fn text(flow: &Flow, pc: u32) -> String {
        flow.gutter(pc, 0, u32::MAX)
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn real_conditional_self_loop_keeps_both_arm_labels() {
        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::Bool]);
        let head = a.label();
        a.place(head).unwrap();
        a.br(1, head, head);
        let mut b = ProgramBuilder::new();
        let f = b
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![ValueType::Bool],
                    ret_types: vec![],
                },
            )
            .unwrap();
        let p = b.build_verified().unwrap();
        let source = disassemble_function(p.program(), f).unwrap();
        assert_eq!(text(&Flow::new(&source), 0), "↺F↺T");
    }

    #[test]
    fn typed_edges_point_at_authored_declarations_and_preserve_same_target_arms() {
        let mut a = Asm::new();
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::Bool,
            ValueType::I64,
            ValueType::I64,
        ]);
        let join = a.block(&[3]);
        a.const_i64(2, 7)
            .br(1, join.with_args(&[2]), join.with_args(&[2]));
        a.place(join).unwrap();
        a.ret(0, &[3]);
        let mut b = ProgramBuilder::new();
        let f = b
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![ValueType::Bool],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        let p = b.build_verified().unwrap();
        let source = disassemble_function(p.program(), f).unwrap();
        let branch = source.instrs().find(|i| i.edge(1).is_some()).unwrap();
        let target = branch.edge(0).unwrap().target;
        assert_eq!(branch.edge(1).unwrap().target, target);
        assert!(source.executable_pc(target).unwrap() > target);
        let flow = Flow::new(&source);
        assert_eq!(text(&flow, branch.pc()), "╭F╭T");
        assert_eq!(text(&flow, target), "╰▶╰▶");
    }
}
