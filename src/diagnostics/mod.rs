//! Diagnostic entry points and background-safe document snapshots.
//! Providers stay focused on one validation concern and operate on the parsed data they need plus
//! runtime schema docs.

use std::sync::Arc;

use tower_lsp::lsp_types::Diagnostic;
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

    pub fn text(&self) -> &str {
        &self.text
    }
}

pub fn collect_with_schema_name(
    snapshot: &DiagnosticSnapshot,
    schema: &SchemaDoc,
    schema_name: Option<&str>,
) -> Vec<Diagnostic> {
    let mut diagnostics = syntax::collect(snapshot);
    let instances = crate::document::build_entity_instances(snapshot.tree(), snapshot.text());
    diagnostics.extend(datatype::collect_with_schema_name(
        &instances,
        schema,
        schema_name,
    ));
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

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
}
