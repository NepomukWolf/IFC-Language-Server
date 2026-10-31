//! Syntax diagnostics for IFC STEP documents.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::diagnostics::DiagnosticSnapshot;

#[tracing::instrument(skip_all)]
pub fn collect(snapshot: &DiagnosticSnapshot) -> Vec<Diagnostic> {
    snapshot
        .lazy()
        .map_or_else(Vec::new, |lazy| collect_lazy(lazy, snapshot.text()))
}

fn collect_lazy(lazy: &crate::handwritten::Document, text: &str) -> Vec<Diagnostic> {
    lazy.errors
        .iter()
        .map(|error| {
            let mut start = error.span.start as usize;
            let mut end = error.span.end as usize;
            if error.message == "unclosed string" {
                let line = memchr::memchr_iter(b'\n', &text.as_bytes()[..start]).count() as u32;
                let line_start = text[..start].rfind('\n').map_or(0, |newline| newline + 1);
                let line_end = memchr::memchr(b'\n', &text.as_bytes()[start..])
                    .map_or(text.len(), |newline| start + newline)
                    .saturating_sub(usize::from(
                        text.as_bytes().get(end.saturating_sub(1)) == Some(&b'\r'),
                    ));
                return Diagnostic {
                    range: Range::new(
                        Position::new(line, utf16_len(&text[line_start..start])),
                        Position::new(line, utf16_len(&text[line_start..line_end])),
                    ),
                    severity: Some(DiagnosticSeverity::ERROR),
                    message: "Unclosed string; expected `'`".to_string(),
                    ..Default::default()
                };
            }
            if let Some(statement) = lazy.statements.iter().find(|statement| {
                statement.span.start as usize <= start && start < statement.span.end as usize
            }) {
                end = (statement.span.end as usize).saturating_sub(1);
            }
            while !text.is_char_boundary(start) {
                start -= 1;
            }
            while !text.is_char_boundary(end) {
                end += 1;
            }
            let position = |offset| {
                let line = memchr::memchr_iter(b'\n', &text.as_bytes()[..offset]).count() as u32;
                let line_start = text[..offset].rfind('\n').map_or(0, |newline| newline + 1);
                Position::new(line, utf16_len(&text[line_start..offset]))
            };
            Diagnostic {
                range: Range::new(position(start), position(end)),
                severity: Some(DiagnosticSeverity::ERROR),
                message: if error.message == "unclosed string" {
                    "Unclosed string; expected `'`".to_string()
                } else {
                    "Invalid IFC STEP syntax".to_string()
                },
                ..Default::default()
            }
        })
        .collect()
}

fn utf16_len(text: &str) -> u32 {
    text.chars()
        .map(|character| character.len_utf16() as u32)
        .sum()
}
