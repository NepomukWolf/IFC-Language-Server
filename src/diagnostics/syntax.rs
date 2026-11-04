//! Syntax diagnostics for IFC STEP documents.
//! This module walks the tree-sitter parse tree and reports invalid or missing syntax nodes.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::diagnostics::DiagnosticSnapshot;

pub fn collect(snapshot: &DiagnosticSnapshot) -> Vec<Diagnostic> {
    let Some(tree) = &snapshot.tree else {
        return Vec::new();
    };

    let mut diagnostics = Vec::new();
    let mut cursor = tree.root_node().walk();
    collect_error_nodes(&mut cursor, &mut diagnostics);
    diagnostics
}

fn collect_error_nodes(
    cursor: &mut tree_sitter::TreeCursor<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    loop {
        let node = cursor.node();

        if let Some(diagnostic) = syntax_diagnostic_for_node(&node) {
            diagnostics.push(diagnostic);
        }

        // An ERROR node's range covers everything nested in it, including the leaf ERROR
        // that tree-sitter creates for a token the lexer cannot recognize.
        if !node.is_error() && cursor.goto_first_child() {
            collect_error_nodes(cursor, diagnostics);
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
}
