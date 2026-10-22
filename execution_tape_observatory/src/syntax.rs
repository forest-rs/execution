// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Presentation-only token colors. Descriptions remain the source of displayed text.

use super::{describe, palette::*};
use execution_tape_recording::{RecordedValue, Recording};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub(super) fn source(text: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut callee: Option<Color> = None;
    while let Some((start, first)) = chars.next() {
        let quoted = first == '"';
        let word = first.is_alphanumeric() || first == '_';
        let mut end = start + first.len_utf8();
        if quoted {
            let mut escaped = false;
            for (index, next) in chars.by_ref() {
                end = index + next.len_utf8();
                if next == '"' && !escaped {
                    break;
                }
                escaped = next == '\\' && !escaped;
            }
        } else if word {
            while let Some(&(index, next)) = chars.peek() {
                if !next.is_alphanumeric() && next != '_' {
                    break;
                }
                chars.next();
                end = index + next.len_utf8();
            }
        }
        let token = &text[start..end];
        let register = token
            .strip_prefix('r')
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit()));
        let keyword = matches!(token, "if" | "else" | "goto" | "return" | "join");
        let color = if quoted {
            GREEN
        } else if register {
            CYAN
        } else if keyword {
            ROSE
        } else if token == "host" {
            GREEN
        } else if token == "call" {
            PURPLE
        } else if first.is_ascii_digit() {
            GOLD
        } else if word {
            callee
                .take()
                .unwrap_or(if first.is_uppercase() { PURPLE } else { FG })
        } else {
            MUTED
        };
        if token == "host" {
            callee = Some(GREEN);
        }
        if token == "call" {
            callee = Some(PURPLE);
        }
        let style = Style::default().fg(color).add_modifier(
            if keyword || token == "host" || token == "call" {
                Modifier::BOLD
            } else {
                Modifier::empty()
            },
        );
        spans.push(Span::styled(token.to_owned(), style));
    }
    Line::from(spans)
}

pub(super) fn value(record: &Recording, value: &RecordedValue) -> Vec<Span<'static>> {
    let text = describe::value(record, value);
    let color = match value {
        RecordedValue::Str(_) | RecordedValue::Bytes(_) => GREEN,
        RecordedValue::Func(_) | RecordedValue::Closure { .. } => PURPLE,
        RecordedValue::Agg(_) => BLUE,
        RecordedValue::Obj(_) => CYAN,
        _ => GOLD,
    };
    let index = text.rfind(" · ").unwrap_or(text.len());
    vec![
        Span::styled(
            text[..index].to_owned(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(text[index..].to_owned(), Style::default().fg(MUTED)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coloring_preserves_text_and_distinguishes_registers_literals_and_callees() {
        for text in [
            "r3, r4 = call read_and_double(r1)",
            "if r6 goto 000053 else 000071",
            "r2 = host read_sample(r1)",
            "r1 = \"x\\\" r99 \\\\ y\"",
            "r1 = -42",
            "r1 = \"λ🙂\"",
        ] {
            let line = source(text);
            assert_eq!(
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>(),
                text
            );
        }
        let line = source("r2 = host read_sample(r1)");
        assert_eq!(
            line.spans
                .iter()
                .find(|span| span.content == "r2")
                .unwrap()
                .style
                .fg,
            Some(CYAN)
        );
        assert_eq!(
            line.spans
                .iter()
                .find(|span| span.content == "read_sample")
                .unwrap()
                .style
                .fg,
            Some(GREEN)
        );
        assert_eq!(source("42").spans[0].style.fg, Some(GOLD));
    }
}
