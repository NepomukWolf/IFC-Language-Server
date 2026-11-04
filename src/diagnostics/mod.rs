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
            &instances,
            schema,
            Some(schema_name),
        ));
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

    fn lowercase_fixture_schema() -> SchemaDoc {
        crate::schema::load_express(
            crate::schema::IfcVersion::Ifc4Add2Tc1,
            r#"
            SCHEMA IFC4;
              TYPE IfcLabel = STRING(255);
              END_TYPE;
              TYPE IfcValue = SELECT (IfcLabel);
              END_TYPE;
              TYPE IfcWallTypeEnum = ENUMERATION OF (MOVABLE, USERDEFINED);
              END_TYPE;
              ENTITY IfcWall;
                Name : OPTIONAL IfcLabel;
                PredefinedType : OPTIONAL IfcWallTypeEnum;
              END_ENTITY;
              ENTITY IfcPropertySingleValue;
                Name : IfcLabel;
                NominalValue : OPTIONAL IfcValue;
              END_ENTITY;
            END_SCHEMA;
            "#,
        )
        .expect("fixture schema should parse")
    }

    fn collect_with_fixture_schema(text: &str) -> Vec<Diagnostic> {
        collect(
            &snapshot_of(text),
            Some(("IFC4", &lowercase_fixture_schema())),
        )
    }

    fn assert_single_diagnostic(text: &str, message: &str, start: u32, end: u32) {
        let diagnostics = collect_with_fixture_schema(text);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].message, message);
        assert_eq!(
            diagnostics[0].range,
            tower_lsp::lsp_types::Range::new(
                tower_lsp::lsp_types::Position::new(0, start),
                tower_lsp::lsp_types::Position::new(0, end),
            )
        );
    }

    #[test]
    fn mixed_case_entity_name_reports_only_the_case() {
        assert_single_diagnostic(
            "#1=IfcWall('w',.MOVABLE.);",
            "Entity names must be upper case",
            3,
            10,
        );
    }

    #[test]
    fn lowercase_typed_value_name_reports_only_the_case() {
        assert_single_diagnostic(
            "#1=IFCPROPERTYSINGLEVALUE('p',ifclabel('x'));",
            "Typed-value names must be upper case",
            30,
            38,
        );
    }

    #[test]
    fn lowercase_enumeration_reports_only_the_case() {
        assert_single_diagnostic(
            "#1=IFCWALL('w',.movable.);",
            "Enumeration values must be upper case",
            15,
            24,
        );
    }

    #[test]
    fn mixed_case_instance_is_validated_as_its_upper_case_entity() {
        let diagnostics = collect_with_fixture_schema("#1=IfcWall(1,.USERDEFINED.);");
        let messages: Vec<_> = diagnostics.iter().map(|d| d.message.as_str()).collect();

        assert_eq!(messages.len(), 2, "{diagnostics:?}");
        assert_eq!(messages[0], "Entity names must be upper case");
        assert!(messages[1].starts_with("Name:"), "{diagnostics:?}");
    }

    #[test]
    fn unknown_lowercase_entity_reports_case_and_unknown_entity() {
        let diagnostics = collect_with_fixture_schema("#1=IfcWal($);");
        let messages: Vec<_> = diagnostics.iter().map(|d| d.message.as_str()).collect();

        assert_eq!(
            messages,
            [
                "Entity names must be upper case",
                "Unknown IFC entity `IFCWAL` in selected schema `IFC4`",
            ]
        );
        assert_eq!(diagnostics[0].range, diagnostics[1].range);
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
}
