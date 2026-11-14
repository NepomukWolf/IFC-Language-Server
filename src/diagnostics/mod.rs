//! Diagnostic entry points and background-safe document snapshots.
//! Providers stay focused on one validation concern and operate on the parsed data they need plus
//! runtime schema docs.

use std::sync::Arc;

use tower_lsp::lsp_types::{Diagnostic, Range};
use tree_sitter::Tree;

use crate::document::Document;
use crate::schema::SchemaDoc;

pub mod datatype;
pub mod scheduler;
pub mod syntax;

#[derive(Debug)]
pub struct DiagnosticSnapshot {
    tree: Option<Tree>,
    text: Arc<String>,
    schema_name: Option<String>,
}

impl DiagnosticSnapshot {
    pub fn from_document(document: &Document) -> Self {
        Self {
            tree: document.tree.clone(),
            text: Arc::clone(&document.text),
            schema_name: document.schema_name.clone(),
        }
    }

    pub fn schema_name(&self) -> Option<&str> {
        self.schema_name.as_deref()
    }

    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    pub fn shared_text(&self) -> &Arc<String> {
        &self.text
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Syntax diagnostics always run; schema-aware passes run only when a schema is available.
pub fn collect(
    snapshot: &DiagnosticSnapshot,
    schema: Option<(&str, &SchemaDoc)>,
) -> Vec<Diagnostic> {
    let mut diagnostics = syntax::collect(snapshot);
    if let Some((schema_name, schema)) = schema {
        let instances = crate::document::build_entity_instances(snapshot.tree(), snapshot.text());
        diagnostics.extend(datatype::collect_with_schema_name(
            snapshot.text(),
            &instances,
            schema,
            Some(schema_name),
        ));
    }
    diagnostics
}

/// Rewrites ranges whose columns are tree-sitter byte columns into the UTF-16 columns LSP expects.
fn byte_columns_to_utf16(text: &str, diagnostics: &mut [Diagnostic]) {
    if diagnostics.is_empty() {
        return;
    }

    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(index, _)| index + 1))
        .collect();

    let mut slots: Vec<(u32, u32, usize)> = diagnostics
        .iter()
        .enumerate()
        .flat_map(|(index, diagnostic)| {
            let Range { start, end } = diagnostic.range;
            [
                (start.line, start.character, 2 * index),
                (end.line, end.character, 2 * index + 1),
            ]
        })
        .collect();
    slots.sort_unstable();

    let (mut line, mut byte_column, mut utf16_column) = (0, 0, 0);
    for (slot_line, slot_byte_column, slot) in slots {
        if slot_line != line {
            (line, byte_column, utf16_column) = (slot_line, 0, 0);
        }
        let line_start = line_starts[line as usize];
        let segment = &text.as_bytes()
            [line_start + byte_column as usize..line_start + slot_byte_column as usize];
        let characters = segment.iter().filter(|byte| **byte & 0xC0 != 0x80);
        let supplementary = segment.iter().filter(|byte| **byte >= 0xF0);
        utf16_column += (characters.count() + supplementary.count()) as u32;
        byte_column = slot_byte_column;

        let range = &mut diagnostics[slot / 2].range;
        let position = if slot % 2 == 0 {
            &mut range.start
        } else {
            &mut range.end
        };
        position.character = utf16_column;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use tower_lsp::lsp_types::Position;
    use tree_sitter::Parser;

    fn snapshot_of(text: &str) -> DiagnosticSnapshot {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("IFC parser language should load");
        DiagnosticSnapshot::from_document(&Document::parse(&mut parser, text.to_string()))
    }

    #[test]
    fn syntax_errors_are_reported_without_a_schema() {
        let snapshot = snapshot_of("FILE_SCHEMA(('IFC4X3'));\n#10=IFCWALL((;\n");

        assert!(!collect(&snapshot, None).is_empty());
    }

    #[test]
    fn valid_file_without_a_schema_has_no_diagnostics() {
        let snapshot = snapshot_of("#10=IFCWALL('gid');\n");

        assert!(collect(&snapshot, None).is_empty());
    }

    #[test]
    fn snapshot_retains_diagnostic_data_after_document_unloads() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("IFC parser language should load");
        let mut document = Document::parse(&mut parser, "#1=IFCWALL('gid')".to_string());
        let snapshot = DiagnosticSnapshot::from_document(&document);

        document.unload_parse_state();

        assert!(!syntax::collect(&snapshot).is_empty());
    }

    #[test]
    fn snapshot_shares_text_and_retains_its_source_revision() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("IFC parser language should load");
        let mut document = Document::parse(&mut parser, "#1=IFCWALL($);".to_string());
        let snapshot = DiagnosticSnapshot::from_document(&document);

        assert!(Arc::ptr_eq(&snapshot.text, &document.text));

        document
            .apply_content_changes(
                &mut parser,
                &[tower_lsp::lsp_types::TextDocumentContentChangeEvent {
                    range: Some(tower_lsp::lsp_types::Range::new(
                        tower_lsp::lsp_types::Position::new(0, 6),
                        tower_lsp::lsp_types::Position::new(0, 10),
                    )),
                    range_length: None,
                    text: "DOOR".to_string(),
                }],
                crate::document::DEFAULT_AST_FILE_SIZE_LIMIT_BYTES,
            )
            .expect("replacement should parse");

        assert_eq!(snapshot.text(), "#1=IFCWALL($);");
        assert_eq!(document.text.as_str(), "#1=IFCDOOR($);");
    }

    fn diagnostic_at(start: (u32, u32), end: (u32, u32)) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(start.0, start.1), Position::new(end.0, end.1)),
            ..Diagnostic::default()
        }
    }

    #[test]
    fn unsorted_diagnostics_on_one_line_get_utf16_columns() {
        let text = "aä😀bé😀c\né!😀x";
        let mut diagnostics = vec![
            diagnostic_at((1, 3), (1, 7)),
            diagnostic_at((0, 10), (0, 14)),
            diagnostic_at((0, 1), (0, 3)),
            diagnostic_at((0, 3), (0, 7)),
            diagnostic_at((0, 8), (0, 8)),
            diagnostic_at((0, 0), (0, 15)),
        ];

        byte_columns_to_utf16(text, &mut diagnostics);

        let ranges: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| {
                let Range { start, end } = diagnostic.range;
                (start.line, start.character, end.line, end.character)
            })
            .collect();
        assert_eq!(
            ranges,
            [
                (1, 2, 1, 4),
                (0, 6, 0, 8),
                (0, 1, 0, 2),
                (0, 2, 0, 4),
                (0, 5, 0, 5),
                (0, 0, 0, 9),
            ]
        );
    }

    fn time_conversion(statements: usize) -> Duration {
        let mut text = String::new();
        let mut diagnostics = Vec::new();
        for id in 1..=statements {
            let prefix = format!("#{id}=IFCWALL('ä😀',");
            let start = text.len() + prefix.len();
            text.push_str(&prefix);
            text.push_str("123);");
            diagnostics.push(diagnostic_at((0, start as u32), (0, start as u32 + 3)));
        }
        (0..5)
            .map(|_| {
                let mut diagnostics = diagnostics.clone();
                let started = Instant::now();
                byte_columns_to_utf16(std::hint::black_box(&text), &mut diagnostics);
                started.elapsed()
            })
            .min()
            .expect("five timings")
    }

    #[test]
    fn utf16_conversion_cost_does_not_grow_with_diagnostics_times_line_length() {
        let small = time_conversion(4_000);
        let large = time_conversion(64_000);

        let ratio = large.as_secs_f64() / small.as_secs_f64();
        assert!(
            ratio < 64.0,
            "16x the diagnostics on a 16x longer line took {ratio:.1}x as long ({small:?} -> {large:?})"
        );
    }
}
