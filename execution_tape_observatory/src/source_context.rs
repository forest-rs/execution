// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Terminal-cell layout for the core's structured source context.

use execution_tape::{source::BoundSources, trace::TraceLocation};

fn escaped(ch: char) -> String {
    if ch.is_control()
        || matches!(ch, '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
    {
        ch.escape_default().to_string()
    } else {
        ch.to_string()
    }
}
fn safe_label(label: &str) -> String {
    let mut output = String::new();
    let mut count = 0;
    for ch in label.chars() {
        let escaped = escaped(ch);
        let width = escaped.chars().count();
        if count + width > 120 {
            output.push('…');
            break;
        }
        output.push_str(&escaped);
        count += width;
    }
    output
}
/// Ready-to-render bounded context, with explicit absence and exact byte coordinates.
/// Original names/text are escaped so this helper cannot emit terminal control sequences.
pub(super) fn context_lines(
    sources: BoundSources<'_>,
    location: TraceLocation,
    max_lines: usize,
    max_columns: usize,
) -> Vec<String> {
    let Some(context) = sources.context(location, max_lines, max_columns) else {
        return vec![format!(
            "Source: no frontend mapping for f{}:{}",
            location.func.0, location.pc
        )];
    };
    let mut out = vec![format!(
        "Source: line {} byte {} · {} · bytes {}..{}",
        context.start.line,
        context.start.byte_column,
        safe_label(context.name),
        context.range.start,
        context.range.end
    )];
    for line in context.lines {
        let left = if line.clipped_left { "…" } else { "" };
        let right = if line.clipped_right { "…" } else { "" };
        out.push(format!(
            "{:>4} | {left}{}{}{}{right}",
            line.number, line.before, line.selected, line.after
        ));
        if !line.selected.is_empty()
            || (context.range.is_empty() && line.number == context.start.line)
        {
            out.push(format!(
                "     | {}{}",
                " ".repeat(
                    ratatui::text::Line::from(line.before).width() + usize::from(line.clipped_left)
                ),
                "^".repeat(ratatui::text::Line::from(line.selected).width().max(1))
            ));
        }
    }
    if context.continued {
        out.push("     … source range continues beyond this context".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use execution_tape::{
        asm::{FunctionSig, ProgramBuilder},
        program::ValueType,
        source::{SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping},
    };

    #[test]
    fn terminal_source_context_escapes_labels_and_uses_display_cell_widths() {
        let mut builder = ProgramBuilder::new();
        let function = builder
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let mut body = builder.function_asm(function).unwrap();
        let value = body.alloc_register(ValueType::I64);
        body.const_i64(value, 42).ret(0, &[value]);
        builder.define_function(function, body).unwrap();
        let program = builder.build_verified().unwrap();
        let location = TraceLocation {
            func: function,
            pc: 0,
        };
        let map = SourceMap::new(
            program.program(),
            vec![SourceDocument {
                name: "name\u{1b}\t\u{202e}".into(),
                text: "界éX".into(),
            }],
            vec![SourceMapping {
                location,
                document: SourceId(0),
                range: 3..5,
            }],
            SourceLimits::default(),
        )
        .unwrap();
        let bound = map.bind(program.program()).unwrap();
        let shown = context_lines(bound, location, 1, 80);
        assert_eq!(shown[1], "   1 | 界éX");
        assert_eq!(shown[2], "     |   ^");
        assert!(shown[0].contains("line 1 byte 4"));
        for control in ['\u{1b}', '\t', '\u{202e}'] {
            assert!(
                !shown.join("\n").contains(control),
                "source display must escape controls"
            );
        }
        let missing = TraceLocation {
            pc: u32::MAX,
            ..location
        };
        assert!(context_lines(bound, missing, 1, 80)[0].contains("no frontend mapping"));
    }
}
