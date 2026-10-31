//! Syntax diagnostics for IFC STEP documents.
//! This module walks the tree-sitter parse tree and reports invalid or missing syntax nodes.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::diagnostics::DiagnosticSnapshot;
use crate::document_tree::UnclosedString;

pub fn collect(snapshot: &DiagnosticSnapshot) -> Vec<Diagnostic> {
    let Some(tree) = &snapshot.tree else {
        return Vec::new();
    };

    let text = snapshot.text();
    let mut line = 0;
    let mut line_start = 0;
    let mut diagnostics: Vec<_> = tree
        .unclosed_strings
        .iter()
        .map(|string| {
            let scanned = line_start;
            for newline in memchr::memchr_iter(b'\n', &text.as_bytes()[scanned..string.quote]) {
                line += 1;
                line_start = scanned + newline + 1;
            }
            unclosed_string_diagnostic(text, line, line_start, string.quote)
        })
        .collect();
    let mut cursor = tree.root_node().walk();
    collect_error_nodes(&mut cursor, &tree.unclosed_strings, &mut diagnostics);
    diagnostics.sort_by_key(|diagnostic| diagnostic.range.start);
    diagnostics
}

fn overlaps_unclosed_string(
    unclosed_strings: &[UnclosedString],
    node: &tree_sitter::Node<'_>,
) -> bool {
    let reach = node.end_byte().saturating_sub(1).max(node.start_byte());
    let index = unclosed_strings.partition_point(|string| string.quote <= reach);
    index > 0 && unclosed_strings[index - 1].end >= node.start_byte()
}

fn unclosed_string_diagnostic(
    text: &str,
    line: u32,
    line_start: usize,
    offset: usize,
) -> Diagnostic {
    let utf16_len = |segment: &str| {
        segment
            .chars()
            .map(|character| character.len_utf16() as u32)
            .sum::<u32>()
    };
    let column = utf16_len(&text[line_start..offset]);
    let line_end = memchr::memchr(b'\n', &text.as_bytes()[offset..])
        .map_or(text.len(), |newline| offset + newline);
    let line_end = if text[..line_end].ends_with('\r') {
        line_end - 1
    } else {
        line_end
    };
    let end_column = column + utf16_len(&text[offset..line_end]);

    Diagnostic {
        range: Range::new(Position::new(line, column), Position::new(line, end_column)),
        severity: Some(DiagnosticSeverity::ERROR),
        message: "Unclosed string; expected `'`".to_string(),
        ..Default::default()
    }
}

fn collect_error_nodes(
    cursor: &mut tree_sitter::TreeCursor<'_>,
    unclosed_strings: &[UnclosedString],
    diagnostics: &mut Vec<Diagnostic>,
) {
    loop {
        let node = cursor.node();

        if !overlaps_unclosed_string(unclosed_strings, &node)
            && let Some(diagnostic) = syntax_diagnostic_for_node(&node)
        {
            diagnostics.push(diagnostic);
        }

        // An ERROR node's range covers everything nested in it, including the leaf ERROR
        // that tree-sitter creates for a token the lexer cannot recognize.
        if !node.is_error() && cursor.goto_first_child() {
            collect_error_nodes(cursor, unclosed_strings, diagnostics);
            cursor.goto_parent();
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn syntax_diagnostic_for_node(node: &tree_sitter::Node<'_>) -> Option<Diagnostic> {
    let message = if node.is_error() {
        Some("Invalid IFC STEP syntax".to_string())
    } else if node.is_missing() {
        Some(missing_node_message(node))
    } else {
        None
    }?;

    Some(Diagnostic {
        range: node_range(node),
        severity: Some(DiagnosticSeverity::ERROR),
        message,
        ..Default::default()
    })
}

fn missing_node_message(node: &tree_sitter::Node<'_>) -> String {
    match node.kind() {
        ";" => "Missing `;`".to_string(),
        kind => format!("Missing `{kind}`"),
    }
}

fn node_range(node: &tree_sitter::Node<'_>) -> Range {
    let start = node.start_position();
    let end = node.end_position();

    Range {
        start: Position::new(start.row as u32, start.column as u32),
        end: Position::new(end.row as u32, end.column as u32),
    }
}

#[cfg(test)]
mod tests {
    use tower_lsp::lsp_types::Position;
    use tree_sitter::Parser;

    use super::*;
    use crate::document::Document;

    fn parse_document(text: &str) -> Document {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE_COMPACT.into())
            .expect("Error loading IFC parser");
        Document::parse(&mut parser, text.to_string())
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
    fn reports_one_unclosed_string_diagnostic_from_its_opening_quote_to_the_end_of_the_line() {
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
    fn quote_in_an_unterminated_block_comment_is_not_an_unclosed_string() {
        let doc = parse_document("#1=IFCWALL(1);\n#2=IFCWALL(2) /* it's open\n");
        let diagnostics = collect(&DiagnosticSnapshot::from_document(&doc));

        assert!(!diagnostics.is_empty());
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.message.starts_with("Unclosed string")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn unclosed_string_range_counts_utf16_units_and_stops_before_crlf() {
        let doc = parse_document("#1=É😀('y😀z);\r\n#2=IFCWALL('next');\r\n");
        let ranges: Vec<_> = collect(&DiagnosticSnapshot::from_document(&doc))
            .into_iter()
            .filter(|diagnostic| diagnostic.message.starts_with("Unclosed string"))
            .map(|diagnostic| diagnostic.range)
            .collect();

        assert_eq!(
            ranges,
            [Range::new(Position::new(0, 7), Position::new(0, 14))]
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
    fn a_comment_opener_inside_a_string_does_not_hide_a_later_unclosed_string() {
        let doc = parse_document(concat!(
            "DATA;\n",
            "#1=IFCWALL('a /* b');\n",
            "#2=IFCWALL('c);\n",
            "#3=IFCWALL(1);\n",
            "#4=IFCWALL('d');\n",
            "ENDSEC;\n",
        ));
        let diagnostics: Vec<_> = collect(&DiagnosticSnapshot::from_document(&doc))
            .into_iter()
            .map(|diagnostic| (diagnostic.range.start, diagnostic.message))
            .collect();

        assert_eq!(
            diagnostics,
            [(
                Position::new(2, 11),
                "Unclosed string; expected `'`".to_string()
            )]
        );
    }

    #[test]
    fn reports_an_unclosed_string_in_a_header_entity_to_the_end_of_its_line() {
        let doc = parse_document("ISO-10303-21;\nHEADER;\nFILE_NAME('unterminated);\n");
        let diagnostics: Vec<_> = collect(&DiagnosticSnapshot::from_document(&doc))
            .into_iter()
            .map(|diagnostic| (diagnostic.range, diagnostic.message))
            .collect();

        assert_eq!(
            diagnostics,
            [(
                Range::new(Position::new(2, 10), Position::new(2, 25)),
                "Unclosed string; expected `'`".to_string()
            )]
        );
    }

    #[test]
    fn an_unclosed_string_in_the_header_leaves_the_data_section_alone() {
        let doc = parse_document(concat!(
            "ISO-10303-21;\n",
            "HEADER;\n",
            "FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');\n",
            "FILE_NAME('unterminated,'2026-10-31T00:00:00',(''),(''),'','','');\n",
            "FILE_SCHEMA(('IFC4'));\n",
            "ENDSEC;\n",
            "DATA;\n",
            "#1=IFCWALL('a',$,$,$,'Wall',$,$,$,$);\n",
            "#2=IFCWALL('b',$,$,$,'Wall',$,$,$,$);\n",
            "ENDSEC;\n",
            "END-ISO-10303-21;\n",
        ));
        let diagnostics: Vec<_> = collect(&DiagnosticSnapshot::from_document(&doc))
            .into_iter()
            .map(|diagnostic| (diagnostic.range.start, diagnostic.message))
            .collect();

        assert_eq!(
            diagnostics,
            [(
                Position::new(3, 10),
                "Unclosed string; expected `'`".to_string()
            )]
        );
    }
}
