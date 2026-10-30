//! Diagnostic entry points and background-safe document snapshots.
//! Providers stay focused on one validation concern and operate on the parsed data they need plus
//! runtime schema docs.

use std::sync::Arc;

use tower_lsp::lsp_types::Diagnostic;

use crate::document::Document;
use crate::handwritten;
use crate::schema::SchemaDoc;

pub mod datatype;
pub mod incremental;
pub mod scheduler;
pub mod syntax;

#[derive(Debug)]
pub struct DiagnosticSnapshot {
    lazy: Option<Arc<handwritten::Document>>,
    lazy_edits: incremental::LazyEdits,
    validation_cache: Arc<incremental::ValidationCache>,
    text: Arc<String>,
    schema_name: Option<String>,
}

impl DiagnosticSnapshot {
    pub fn from_document(document: &Document) -> Self {
        Self {
            lazy: document.lazy.clone(),
            lazy_edits: document.lazy_edits.clone(),
            validation_cache: Arc::clone(&document.validation_cache),
            text: Arc::clone(&document.text),
            schema_name: document.schema_name.clone(),
        }
    }

    pub fn schema_name(&self) -> Option<&str> {
        self.schema_name.as_deref()
    }

    pub fn lazy(&self) -> Option<&handwritten::Document> {
        self.lazy.as_deref()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn shared_text(&self) -> &Arc<String> {
        &self.text
    }
}

/// Syntax diagnostics always run; schema-aware passes run only when a schema is available.
#[tracing::instrument(skip_all)]
pub fn collect(
    snapshot: &DiagnosticSnapshot,
    schema: Option<(&str, &SchemaDoc)>,
) -> Vec<Diagnostic> {
    let mut diagnostics = syntax::collect(snapshot);
    if let (Some(lazy), Some((schema_name, schema))) = (snapshot.lazy(), schema) {
        let (datatype, mode) = incremental::collect(
            lazy,
            snapshot.text(),
            &snapshot.lazy_edits,
            &snapshot.validation_cache,
            schema,
            schema_name,
        );
        tracing::info!(?mode, "lazy datatype diagnostics");
        diagnostics.extend(datatype);
    }
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn documents_above_the_ast_limit_get_syntax_diagnostics_from_the_lazy_index() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("IFC parser language should load");
        let mut document =
            Document::new_unloaded("DATA;\n#1=IFCWALL('a);\n#2=IFCSLAB X$);\nENDSEC;\n".into());
        document.reload_parse_state(&mut parser, 1);

        let diagnostics = collect(&DiagnosticSnapshot::from_document(&document), None);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| (diagnostic.range, diagnostic.message.as_str()))
                .collect::<Vec<_>>(),
            [
                (
                    tower_lsp::lsp_types::Range::new(
                        tower_lsp::lsp_types::Position::new(1, 11),
                        tower_lsp::lsp_types::Position::new(1, 15),
                    ),
                    "Unclosed string; expected `'`",
                ),
                (
                    tower_lsp::lsp_types::Range::new(
                        tower_lsp::lsp_types::Position::new(2, 11),
                        tower_lsp::lsp_types::Position::new(2, 14),
                    ),
                    "Invalid IFC STEP syntax",
                ),
            ]
        );
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

        assert_eq!(snapshot.shared_text().as_str(), "#1=IFCWALL($);");
        assert_eq!(document.text.as_str(), "#1=IFCDOOR($);");
    }
}
