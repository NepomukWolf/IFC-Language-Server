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
            let statement_index = lazy
                .statements
                .partition_point(|statement| statement.span.start as usize <= start);
            let statement = statement_index.checked_sub(1).and_then(|index| {
                let statement = lazy.statements[index];
                (start < statement.span.end as usize).then_some(statement)
            });
            if let Some(offset) = statement
                .and_then(|statement| unclosed_quote(text.as_bytes(), statement.span.range()))
            {
                return unclosed_string_diagnostic(text, offset);
            }
            if error.message == "unclosed string" {
                return unclosed_string_diagnostic(text, start);
            }
            let missing = match error.message {
                "expected ;" => Some(';'),
                "expected )" => Some(')'),
                _ => None,
            };
            if let Some(token) = missing
                && start == end
            {
                let at = position(text, start);
                return Diagnostic {
                    range: Range::new(at, at),
                    severity: Some(DiagnosticSeverity::ERROR),
                    message: format!("Missing `{token}`"),
                    ..Default::default()
                };
            }
            if let Some(statement) = statement
                && error.message != "unexpected value"
            {
                end = (statement.span.end as usize).saturating_sub(1);
            }
            while !text.is_char_boundary(start) {
                start -= 1;
            }
            while !text.is_char_boundary(end) {
                end += 1;
            }
            Diagnostic {
                range: Range::new(position(text, start), position(text, end)),
                severity: Some(DiagnosticSeverity::ERROR),
                message: "Invalid IFC STEP syntax".to_string(),
                ..Default::default()
            }
        })
        .fold(Vec::new(), |mut diagnostics, diagnostic| {
            if diagnostics.last() != Some(&diagnostic) {
                diagnostics.push(diagnostic);
            }
            diagnostics
        })
}

/// The first quote of a statement whose quotes, outside comments, never balance.
fn unclosed_quote(text: &[u8], span: std::ops::Range<usize>) -> Option<usize> {
    let mut offset = span.start;
    let mut first = None;
    let mut quoted = false;
    while offset < span.end {
        if text[offset] == b'\'' {
            first.get_or_insert(offset);
            quoted = !quoted;
            offset += 1;
        } else if !quoted && text[offset..span.end].starts_with(b"/*") {
            offset = memchr::memmem::find(&text[offset + 2..span.end], b"*/")
                .map_or(span.end, |end| offset + end + 4);
        } else {
            offset += 1;
        }
    }
    quoted.then_some(first).flatten()
}

fn position(text: &str, offset: usize) -> Position {
    let line = memchr::memchr_iter(b'\n', &text.as_bytes()[..offset]).count() as u32;
    let line_start = text[..offset].rfind('\n').map_or(0, |newline| newline + 1);
    Position::new(line, utf16_len(&text[line_start..offset]))
}

fn utf16_len(text: &str) -> u32 {
    crate::document::utf16_len(text.as_bytes()) as u32
}

/// From the opening quote to the end of its line, before any `\r`.
fn unclosed_string_diagnostic(text: &str, offset: usize) -> Diagnostic {
    let start = position(text, offset);
    let line_end = memchr::memchr(b'\n', &text.as_bytes()[offset..])
        .map_or(text.len(), |newline| offset + newline);
    let line_end = if text[..line_end].ends_with('\r') {
        line_end - 1
    } else {
        line_end
    };

    Diagnostic {
        range: Range::new(
            start,
            Position::new(
                start.line,
                start.character + utf16_len(&text[offset..line_end]),
            ),
        ),
        severity: Some(DiagnosticSeverity::ERROR),
        message: "Unclosed string; expected `'`".to_string(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use tower_lsp::lsp_types::Position;

    use super::*;
    use crate::document::Document;

    fn parse_document(text: &str) -> Document {
        Document::parse(text.to_string())
    }

    #[test]
    fn reports_unrecognized_token_once() {
        let doc = parse_document("DATA;\n#1=IFCWALL($);\nstray\nENDSEC;\n");
        let diagnostics = collect(&DiagnosticSnapshot::from_document(&doc));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].range.start, Position::new(2, 0));
        assert_eq!(diagnostics[0].range.end, Position::new(2, 5));
    }

    #[test]
    fn reports_invalid_step_syntax() {
        let doc = parse_document(r#"#14=IFCUNITASSIGNMENT((#15,#16,#17, "test"));"#);
        let diagnostics = collect(&DiagnosticSnapshot::from_document(&doc));

        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("Invalid IFC STEP syntax")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn invalid_coordinate_number_preserves_diagnostic_range() {
        let doc = parse_document("DATA;\n#1=IFCCARTESIANPOINT((1.0,2x,3.0));\nENDSEC;\n");
        assert_eq!(
            collect(&DiagnosticSnapshot::from_document(&doc)),
            vec![Diagnostic {
                range: Range::new(Position::new(1, 27), Position::new(1, 28)),
                severity: Some(DiagnosticSeverity::ERROR),
                message: "Invalid IFC STEP syntax".to_string(),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn skipping_an_invalid_value_steps_over_a_comment_with_delimiters() {
        let doc = parse_document("DATA;\n#1=IFCCARTESIANPOINT((1.0,2x /* ; ) */,3.0));\nENDSEC;\n");
        assert_eq!(
            collect(&DiagnosticSnapshot::from_document(&doc)),
            vec![Diagnostic {
                range: Range::new(Position::new(1, 27), Position::new(1, 28)),
                severity: Some(DiagnosticSeverity::ERROR),
                message: "Invalid IFC STEP syntax".to_string(),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn reports_missing_step_semicolon() {
        let doc = parse_document("#1=IFCWALL('gid')\n#2=IFCWALL('next');");
        let diagnostics = collect(&DiagnosticSnapshot::from_document(&doc));

        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.message == "Missing `;`")
            .expect("missing semicolon should be reported");

        assert_eq!(diagnostic.range.start, Position::new(0, 17));
        assert_eq!(diagnostic.range.end, Position::new(0, 17));
    }

    #[test]
    fn reports_a_stray_closing_parenthesis_instead_of_a_missing_semicolon() {
        let doc = parse_document("DATA;\n#1=IFCX((1)));\n#2=IFCX(2);\nENDSEC;\n");
        let diagnostics: Vec<_> = collect(&DiagnosticSnapshot::from_document(&doc))
            .into_iter()
            .map(|diagnostic| (diagnostic.range, diagnostic.message))
            .collect();

        assert_eq!(
            diagnostics,
            [(
                Range::new(Position::new(1, 12), Position::new(1, 13)),
                "Invalid IFC STEP syntax".to_string()
            )]
        );
    }

    #[test]
    fn reports_one_unclosed_string_diagnostic_at_its_opening_quote() {
        let doc = parse_document("#1=IFCWALL('Anna's wall');\n#2=IFCWALL('next');\n");
        let diagnostics = collect(&DiagnosticSnapshot::from_document(&doc));

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].range,
            Range::new(Position::new(0, 11), Position::new(0, 26))
        );
        assert_eq!(diagnostics[0].message, "Unclosed string; expected `'`");
    }

    #[test]
    fn unclosed_string_range_counts_utf16_units_and_stops_before_crlf() {
        let doc = parse_document("#1=X('y😀z,$);\r\n#2=IFCWALL('next');\r\n");
        let ranges: Vec<_> = collect(&DiagnosticSnapshot::from_document(&doc))
            .into_iter()
            .map(|diagnostic| (diagnostic.range, diagnostic.message))
            .collect();

        assert_eq!(
            ranges,
            [(
                Range::new(Position::new(0, 5), Position::new(0, 14)),
                "Unclosed string; expected `'`".to_string()
            )]
        );
    }

    #[test]
    fn reports_a_stray_token_right_after_a_statement() {
        let doc = parse_document("DATA;\n#1=IFCWALL($);q\r\n#2=IFCWALL($);\nENDSEC;\n");
        let diagnostics: Vec<_> = collect(&DiagnosticSnapshot::from_document(&doc))
            .into_iter()
            .map(|diagnostic| (diagnostic.range, diagnostic.message))
            .collect();

        assert_eq!(
            diagnostics,
            [(
                Range::new(Position::new(1, 14), Position::new(1, 15)),
                "Invalid IFC STEP syntax".to_string()
            )]
        );
    }

    #[test]
    fn a_statement_rejected_as_a_whole_is_reported_from_where_it_went_wrong() {
        let doc =
            parse_document("DATA;\n#4=IFCAXIS2PLACEMENT3DX#8,$,$);\n#8=IFCWALL($);\nENDSEC;\n");
        let diagnostics: Vec<_> = collect(&DiagnosticSnapshot::from_document(&doc))
            .into_iter()
            .map(|diagnostic| (diagnostic.range, diagnostic.message))
            .collect();

        assert_eq!(
            diagnostics,
            [(
                Range::new(Position::new(1, 23), Position::new(1, 30)),
                "Invalid IFC STEP syntax".to_string()
            )]
        );
    }

    #[test]
    fn reports_two_unclosed_strings_and_unrelated_errors() {
        let doc = parse_document(concat!(
            "DATA;\n",
            "#1=X('a);\n",
            "#2=X(1);\n",
            "#3=X('b);\n",
            "#4=X(1);\n",
            "#5=X(2)\n",
            "#6=X(3);\n",
            "ENDSEC;\n",
        ));
        let diagnostics: Vec<_> = collect(&DiagnosticSnapshot::from_document(&doc))
            .into_iter()
            .map(|diagnostic| (diagnostic.range.start, diagnostic.message))
            .collect();

        assert_eq!(
            diagnostics,
            [
                (
                    Position::new(1, 5),
                    "Unclosed string; expected `'`".to_string()
                ),
                (
                    Position::new(3, 5),
                    "Unclosed string; expected `'`".to_string()
                ),
                (Position::new(5, 7), "Missing `;`".to_string()),
            ]
        );
    }

    #[test]
    fn skips_errors_read_out_of_phase_until_the_next_unclosed_string() {
        let text = (1..=30)
            .map(|id| match id {
                10 | 20 => format!("#{id}=X('a);\n"),
                25 => format!("#{id}=X(1)\n"),
                _ => format!("#{id}=X('v',(1.,2.),.T.,'w',#3);\n"),
            })
            .collect::<String>();
        let doc = parse_document(&format!("DATA;\n{text}ENDSEC;\n"));
        let diagnostics: Vec<_> = collect(&DiagnosticSnapshot::from_document(&doc))
            .into_iter()
            .map(|diagnostic| (diagnostic.range.start, diagnostic.message))
            .collect();

        assert_eq!(
            diagnostics,
            [
                (
                    Position::new(10, 6),
                    "Unclosed string; expected `'`".to_string()
                ),
                (
                    Position::new(20, 6),
                    "Unclosed string; expected `'`".to_string()
                ),
                (Position::new(25, 8), "Missing `;`".to_string()),
            ]
        );
    }

    #[test]
    fn leaves_a_balanced_multiline_string_alone() {
        let doc = parse_document("#1=IFCWALL('line\ncontinued');\n");
        assert!(collect(&DiagnosticSnapshot::from_document(&doc)).is_empty());
    }

    #[test]
    fn reports_errors_nested_among_valid_entities() {
        let doc = parse_document(concat!(
            "DATA;\n",
            "#1=IFCCARTESIANPOINT((0.,1.,2.));\n",
            "#2=IFCPOLYLINE((#1,#4))\n",
            "#3=IFCDIRECTION((1.,0.,0.));\n",
            "#4=IFCWALL('a',IFCLABEL('x',(#2,#3));\n",
            "#5=IFCDIRECTION((1.,0.,0.));\n",
            "#6=IFCWALL('b',$,%);\n",
            "#7=IFCCARTESIANPOINT((3.,4.,5.));\n",
            "ENDSEC;\n",
        ));
        let diagnostics: Vec<_> = collect(&DiagnosticSnapshot::from_document(&doc))
            .into_iter()
            .map(|diagnostic| {
                let Range { start, end } = diagnostic.range;
                (
                    (start.line, start.character, end.line, end.character),
                    diagnostic.message,
                )
            })
            .collect();

        assert_eq!(
            diagnostics,
            [
                ((2, 23, 2, 23), "Missing `;`".to_string()),
                ((4, 36, 4, 36), "Missing `)`".to_string()),
                ((6, 16, 6, 18), "Invalid IFC STEP syntax".to_string()),
            ]
        );
    }
}
