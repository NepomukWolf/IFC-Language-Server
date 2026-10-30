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

fn follows_unclosed_string(unclosed_strings: &[UnclosedString], offset: usize) -> bool {
    let index = unclosed_strings.partition_point(|string| string.quote <= offset);
    index > 0 && unclosed_strings[index - 1].end >= offset
}

fn unclosed_string_diagnostic(
    text: &str,
    line: u32,
    line_start: usize,
    offset: usize,
) -> Diagnostic {
    let column = text[line_start..offset]
        .chars()
        .map(|character| character.len_utf16() as u32)
        .sum::<u32>();

    Diagnostic {
        range: Range::new(Position::new(line, column), Position::new(line, column + 1)),
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

        if !follows_unclosed_string(unclosed_strings, node.start_byte())
            && let Some(diagnostic) = syntax_diagnostic_for_node(&node)
        {
            diagnostics.push(diagnostic);
        }

        // An ERROR node's range covers everything nested in it, including the leaf ERROR
        // that tree-sitter creates for a token the lexer cannot recognize. `has_error` is also
        // true on every ancestor of a MISSING node.
        if !node.is_error() && node.has_error() && cursor.goto_first_child() {
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
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
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
    fn reports_one_unclosed_string_diagnostic_at_its_opening_quote() {
        let doc = parse_document("#1=IFCWALL('Anna's wall');\n#2=IFCWALL('next');\n");
        let diagnostics = collect(&DiagnosticSnapshot::from_document(&doc));

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].range,
            Range::new(Position::new(0, 11), Position::new(0, 12))
        );
        assert_eq!(diagnostics[0].message, "Unclosed string; expected `'`");
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
