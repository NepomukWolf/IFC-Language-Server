//! Diagnostic entry points and background-safe document snapshots.
//! Providers stay focused on one validation concern and operate on the parsed data they need plus
//! runtime schema docs.

use std::collections::HashMap;

use tower_lsp::lsp_types::Diagnostic;
use tree_sitter::Tree;

use crate::document::{Document, EntityInstanceInfo};
use crate::schema::SchemaDoc;

pub mod datatype;
pub mod scheduler;
pub mod syntax;

#[derive(Debug)]
pub struct DiagnosticSnapshot {
    tree: Option<Tree>,
    instances: Vec<EntityInstanceInfo>,
    instance_indexes_by_id: HashMap<u32, usize>,
    schema_name: Option<String>,
}

impl DiagnosticSnapshot {
    pub fn from_document(document: &Document) -> Self {
        Self {
            tree: document.tree.clone(),
            instances: document.instances.clone(),
            instance_indexes_by_id: document.instance_indexes_by_id.clone(),
            schema_name: document.schema_name.clone(),
        }
    }

    pub fn schema_name(&self) -> Option<&str> {
        self.schema_name.as_deref()
    }

    fn instance_by_id(&self, id: u32) -> Option<&EntityInstanceInfo> {
        self.instance_indexes_by_id
            .get(&id)
            .and_then(|index| self.instances.get(*index))
    }
}

pub fn collect_with_schema_name(
    snapshot: &DiagnosticSnapshot,
    schema: &SchemaDoc,
    schema_name: Option<&str>,
) -> Vec<Diagnostic> {
    let mut diagnostics = syntax::collect(snapshot);
    diagnostics.extend(datatype::collect_with_schema_name(
        snapshot,
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
}
