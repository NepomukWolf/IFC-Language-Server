//! Hover feature logic.
//! Resolves the syntax node under the cursor and renders either schema-backed entity information
//! or local reference previews from the current document.
//! Hover stays synchronous by reading only the in-memory document and schema collections.

use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::document::{DefinitionInfo, Document};
use crate::schema::{EntityAttributeDoc, EntityDoc, SchemaDocCollection};
use crate::step::{ast, derived, derived::ResolvedDerivedValue};

pub fn hover(
    document: &Document,
    position: Position,
    schema_docs: &SchemaDocCollection,
    selected_schema_name: Option<&str>,
) -> Option<Hover> {
    document.tree.as_ref()?;

    if let Some(node) = document.node_at_position(position) {
        if node.kind() == "entity_name" {
            let entity_text = node.utf8_text(document.text.as_bytes()).ok()?;
            if let Some(schema_name) = selected_schema_name.or(document.schema_name.as_deref())
                && let Some(entity_doc) = schema_docs.get_entity_doc(schema_name, entity_text)
            {
                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: render_entity_hover(entity_doc),
                    }),
                    range: None,
                });
            }
        } else if node.kind() == "reference" {
            let reference_text = node.utf8_text(document.text.as_bytes()).ok()?;
            let id = reference_text.trim_start_matches('#').parse::<u32>().ok()?;
            let definition = document.definitions.get(&id)?;

            return Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: render_reference_hover(document, definition)?,
                }),
                range: Some(node_range(&node)),
            });
        } else if node.kind() == "omitted_value" {
            if let Some(schema_name) = selected_schema_name.or(document.schema_name.as_deref())
                && let Some(schema) = schema_docs.get(schema_name)
            {
                let context = ast::omitted_value_context(node, &document.text)?;
                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: omitted_value_hover(
                            document,
                            schema,
                            derived::resolve_omitted_value(
                                document,
                                schema,
                                context.instance_id,
                                &context.entity_name,
                                context.parameter_index,
                            )?,
                        )?,
                    }),
                    range: Some(node_range(&node)),
                });
            }
        }
    }

    None
}

fn render_entity_hover(entity_doc: &EntityDoc) -> String {
    let mut markdown = format!("# {}", entity_doc.name);

    let inherited_attributes: Vec<_> = entity_doc
        .attributes
        .iter()
        .filter(|attribute| attribute.declared_in != entity_doc.name)
        .collect();
    let direct_attributes: Vec<_> = entity_doc
        .attributes
        .iter()
        .filter(|attribute| attribute.declared_in == entity_doc.name)
        .collect();

    if !inherited_attributes.is_empty() {
        markdown.push_str("\n\n## Inherited Attributes");
        markdown.push_str(&render_attribute_table(&inherited_attributes, 1));
    }

    markdown.push_str("\n\n## Attributes Declared In This Entity");
    markdown.push_str(&render_attribute_table(
        &direct_attributes,
        inherited_attributes.len() + 1,
    ));

    if !entity_doc.url.is_empty() {
        markdown.push_str(&format!("\n\n[Official documentation]({})", entity_doc.url));
    }

    markdown
}

fn render_attribute_table(attributes: &[&EntityAttributeDoc], start_index: usize) -> String {
    let mut markdown = String::new();

    markdown.push_str("\n\n| # | Attribute | Type |\n| --- | --- | --- |");
    for (index, attribute) in attributes.iter().enumerate() {
        markdown.push_str(&format!(
            "\n| {} | {} | {} |",
            start_index + index,
            format_attribute_name(&attribute.name),
            format_attribute_type(&attribute.type_name),
        ));
    }

    markdown
}

fn render_reference_hover(document: &Document, definition: &DefinitionInfo) -> Option<String> {
    let preview = extract_range_text(document, definition.entity_range)?;

    Some(format!("```ifc\n{}\n```", preview.trim()))
}

fn omitted_value_hover(
    _document: &Document,
    _schema: &crate::schema::SchemaDoc,
    resolved: ResolvedDerivedValue,
) -> Option<String> {
    let mut text = match resolved.value {
        Some(value) => format!("{}: {}", resolved.attribute_name, value),
        None => format!("{}: derived value", resolved.attribute_name),
    };
    if let Some(preview) = resolved.preview {
        text.push_str("\n\n");
        text.push_str(&preview);
    }
    if let Some(note) = resolved.resolution_note {
        text.push_str("\n\n");
        text.push_str(&note);
    }
    Some(text)
}

fn extract_range_text(document: &Document, range: tower_lsp::lsp_types::Range) -> Option<&str> {
    let start = offset_at_position(&document.text, range.start)?;
    let end = offset_at_position(&document.text, range.end)?;
    document.text.get(start..end)
}

fn offset_at_position(text: &str, position: Position) -> Option<usize> {
    let mut offset = 0usize;
    let mut lines = text.split('\n');

    for _ in 0..position.line {
        let line = lines.next()?;
        offset += line.len() + 1;
    }

    let line = lines.next()?;
    let character = position.character as usize;
    if character > line.len() {
        return None;
    }

    Some(offset + character)
}

fn node_range(node: &tree_sitter::Node<'_>) -> tower_lsp::lsp_types::Range {
    let start = node.start_position();
    let end = node.end_position();

    tower_lsp::lsp_types::Range {
        start: Position::new(start.row as u32, start.column as u32),
        end: Position::new(end.row as u32, end.column as u32),
    }
}

fn escape_table_cell(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

fn format_attribute_name(name: &str) -> String {
    format!("*{}*", escape_table_cell(name))
}

fn format_attribute_type(type_name: &str) -> String {
    format!("`{}`", escape_table_cell(type_name).replace('`', "\\`"))
}

//*----- TESTS BEGIN HERE -----*
#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;
    use crate::schema::EntityAttributeDoc;

    /// Helper function to parse a document and return the result.
    fn parse_document(text: &str) -> Document {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("Error loading IFC parser");

        Document::parse(&mut parser, text.to_string())
    }

    /// Helper function to find the position of a substring in a document.
    fn position_at(text: &str, needle: &str) -> Position {
        let offset = text.find(needle).expect("needle should exist") as u32;
        Position::new(0, offset)
    }

    /// Helper function to find the last occurence of a substring in a document.
    /// This is helpful since "#1" can appear multiple times in different contexts (e.g., as a definition vs as a reference)
    fn position_at_last(text: &str, needle: &str) -> Position {
        let offset = text.rfind(needle).expect("needle should exist") as u32;
        let prefix = &text[..offset as usize];
        let line = prefix.bytes().filter(|&b| b == b'\n').count() as u32;
        let column = prefix
            .rsplit_once('\n')
            .map(|(_, tail)| tail.len() as u32)
            .unwrap_or(offset);
        Position::new(line, column)
    }

    /// Helper function to extract the hover text from a hover response.
    fn hover_text(hover: Hover) -> String {
        match hover.contents {
            HoverContents::Markup(markup) => markup.value,
            contents => panic!("unexpected hover contents: {contents:?}"),
        }
    }

    fn empty_schema_docs() -> SchemaDocCollection {
        SchemaDocCollection::empty()
    }

    /// Helper function to create a schema document collection with a wall entity.
    fn schema_docs_with_wall() -> SchemaDocCollection {
        let source = r#"
        SCHEMA IFC4;
          TYPE IfcGloballyUniqueId = STRING(22) FIXED;
          END_TYPE;
          TYPE IfcWallTypeEnum = ENUMERATION OF (MOVABLE, USERDEFINED);
          END_TYPE;
          ENTITY IfcRoot;
            GlobalId : IfcGloballyUniqueId;
          END_ENTITY;
          ENTITY IfcWall
            SUBTYPE OF (IfcRoot);
            PredefinedType : OPTIONAL IfcWallTypeEnum;
          END_ENTITY;
        END_SCHEMA;
        "#;
        let schema = crate::schema::load_express(crate::schema::IfcVersion::Ifc4Add2Tc1, source)
            .expect("fixture schema should parse");
        SchemaDocCollection::from_docs([("IFC4".to_string(), schema)])
    }

    /// Test schema docs include derived attributes for entities.
    fn schema_docs_with_derived_attributes() -> SchemaDocCollection {
        let source = r#"
        SCHEMA IFC4;
          TYPE IfcUnitEnum = ENUMERATION OF (LENGTHUNIT, AREAUNIT, VOLUMEUNIT, PLANEANGLEUNIT);
          END_TYPE;
          TYPE IfcSIPrefix = ENUMERATION OF (MILLI);
          END_TYPE;
          TYPE IfcSIUnitName = ENUMERATION OF (METRE, SQUARE_METRE, CUBIC_METRE, RADIAN);
          END_TYPE;
          TYPE IfcGeometricProjectionEnum = ENUMERATION OF (MODEL_VIEW, PLAN_VIEW);
          END_TYPE;
          ENTITY IfcDimensionalExponents;
          END_ENTITY;
          ENTITY IfcAxis2Placement3D;
          END_ENTITY;
          ENTITY IfcDirection;
          END_ENTITY;
          ENTITY IfcNamedUnit;
            Dimensions : IfcDimensionalExponents;
            UnitType : IfcUnitEnum;
          END_ENTITY;
          ENTITY IfcSIUnit
            SUBTYPE OF (IfcNamedUnit);
            Prefix : OPTIONAL IfcSIPrefix;
            Name : IfcSIUnitName;
          DERIVE
            SELF\IfcNamedUnit.Dimensions : IfcDimensionalExponents := ?;
          END_ENTITY;
          ENTITY IfcRepresentationContext;
            ContextIdentifier : OPTIONAL STRING;
            ContextType : OPTIONAL STRING;
            CoordinateSpaceDimension : INTEGER;
            Precision : OPTIONAL REAL;
            WorldCoordinateSystem : IfcAxis2Placement3D;
            TrueNorth : OPTIONAL IfcDirection;
          END_ENTITY;
          ENTITY IfcGeometricRepresentationContext
            SUBTYPE OF (IfcRepresentationContext);
          END_ENTITY;
          ENTITY IfcGeometricRepresentationSubContext
            SUBTYPE OF (IfcGeometricRepresentationContext);
            ParentContext : IfcRepresentationContext;
            TargetScale : OPTIONAL REAL;
            TargetView : IfcGeometricProjectionEnum;
            UserDefinedTargetView : OPTIONAL STRING;
          DERIVE
            SELF\IfcRepresentationContext.CoordinateSpaceDimension : INTEGER := ?;
            SELF\IfcRepresentationContext.Precision : REAL := ?;
            SELF\IfcRepresentationContext.WorldCoordinateSystem : IfcAxis2Placement3D := ?;
            SELF\IfcRepresentationContext.TrueNorth : IfcDirection := ?;
          END_ENTITY;
        END_SCHEMA;
        "#;
        let schema = crate::schema::load_express(crate::schema::IfcVersion::Ifc4Add2Tc1, source)
            .expect("fixture schema should parse");
        SchemaDocCollection::from_docs([("IFC4".to_string(), schema)])
    }

    /// Test that hover returns schema docs for entity names with detected version in the text.
    /// Partial hover text assertion since this should only test the schema docs are returned, not the exact text.
    #[test]
    fn hover_returns_schema_docs_for_entity_names_with_detected_version() {
        let text = "ISO-10303-21;HEADER;FILE_SCHEMA(('IFC4'));ENDSEC;DATA;#1=IFCWALL($);ENDSEC;END-ISO-10303-21;";
        let document = parse_document(text);

        let hover = hover(
            &document,
            position_at(text, "IFCWALL"),
            &schema_docs_with_wall(),
            None,
        )
        .expect("hover should exist");
        let value = hover_text(hover);

        assert!(value.contains("# IfcWall"));
        assert!(value.contains("Official documentation"));
    }

    /// Test that hover returns definition preview for references to definitions.
    #[test]
    fn omitted_value_context_uses_ast_to_find_instance_and_parameter() {
        let text = "#15=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);";
        let document = parse_document(text);
        let node = document
            .node_at_position(position_at(text, "*"))
            .expect("omitted value node should exist");

        let context = crate::step::ast::omitted_value_context(node, &document.text)
            .expect("context should resolve from AST");

        assert_eq!(context.instance_id, 15);
        assert_eq!(context.entity_name, "IFCSIUNIT");
        assert_eq!(context.parameter_index, 0);
    }

    #[test]
    fn hover_returns_definition_preview_for_references() {
        let text = "#1=IFCWALL($);\n#2=IFCDOOR(#1);";
        let document = parse_document(text);

        let hover = hover(
            &document,
            position_at_last(text, "#1"),
            &empty_schema_docs(),
            None,
        )
        .expect("hover exists");
        let value = hover_text(hover);

        assert!(value.contains("#1=IFCWALL($);"));
    }

    /// Test that hover does not return definition preview for definition IDs (as opposed to reference IDs).
    #[test]
    fn hover_does_not_return_definition_preview_for_definition_ids() {
        let text = "#1=IFCWALL($);";
        let document = parse_document(text);

        let hover = hover(
            &document,
            position_at(text, "#1"),
            &empty_schema_docs(),
            None,
        );
        assert!(hover.is_none());
    }

    /// Test that hover returns the resolved value for an IFC SI unit with omitted dimensions.
    #[test]
    fn hover_returns_resolved_value_for_ifc_si_unit_omitted_dimensions() {
        let text = "#15=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);";
        let document = parse_document(text);

        let hover = hover(
            &document,
            position_at(text, "*"),
            &schema_docs_with_derived_attributes(),
            Some("IFC4"),
        )
        .expect("hover should exist");
        let value = hover_text(hover);

        assert!(value.contains("Dimensions"));
        assert!(value.contains("IfcDimensionalExponents(1, 0, 0, 0, 0, 0, 0)"));
        assert!(value.contains("resolved from `Name`"));
    }

    #[test]
    fn hover_returns_resolved_zero_dimensions_for_radian() {
        let text = "#18=IFCSIUNIT(*,.PLANEANGLEUNIT.,$,.RADIAN.);";
        let document = parse_document(text);

        let hover = hover(
            &document,
            position_at(text, "*"),
            &schema_docs_with_derived_attributes(),
            Some("IFC4"),
        )
        .expect("hover should exist");
        let value = hover_text(hover);

        assert!(value.contains("Dimensions"));
        assert!(value.contains("IfcDimensionalExponents(0, 0, 0, 0, 0, 0, 0)"));
    }

    #[test]
    fn hover_resolves_subcontext_value_from_parent_context() {
        let text = "#7=IFCAXIS2PLACEMENT3D();\n#11=IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,$,#7,$);\n#12=IFCGEOMETRICREPRESENTATIONSUBCONTEXT('Body','Model',*,*,*,*,#11,$,.MODEL_VIEW.,$);";
        let document = parse_document(text);

        let hover = hover(
            &document,
            position_at_last(text, "*"),
            &schema_docs_with_derived_attributes(),
            Some("IFC4"),
        )
        .expect("hover should exist");
        let value = hover_text(hover);

        assert!(value.contains("TrueNorth"));
        assert!(value.contains("TrueNorth: `$`"));
        assert!(value.contains("resolved from `ParentContext`"));
    }

    #[test]
    fn hover_resolves_subcontext_reference_from_parent_context() {
        let text = "#7=IFCAXIS2PLACEMENT3D();\n#11=IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,$,#7,$);\n#12=IFCGEOMETRICREPRESENTATIONSUBCONTEXT('Body','Model',*,*,*,*,#11,$,.MODEL_VIEW.,$);";
        let document = parse_document(text);
        let first_star = text.match_indices('*').nth(2).expect("third star exists").0 as u32;
        let prefix = &text[..first_star as usize];
        let line = prefix.bytes().filter(|&b| b == b'\n').count() as u32;
        let column = prefix
            .rsplit_once('\n')
            .map(|(_, tail)| tail.len() as u32)
            .unwrap_or(first_star);

        let hover = hover(
            &document,
            Position::new(line, column),
            &schema_docs_with_derived_attributes(),
            Some("IFC4"),
        )
        .expect("hover should exist");
        let value = hover_text(hover);

        assert!(value.contains("WorldCoordinateSystem"));
        assert!(value.contains("`#7`"));
        assert!(value.contains("#7=IFCAXIS2PLACEMENT3D();"));
        assert!(value.contains("resolved from `ParentContext`"));
    }

    #[test]
    fn hover_explains_unresolved_parent_context() {
        let text = "#12=IFCGEOMETRICREPRESENTATIONSUBCONTEXT('Body','Model',*,*,*,*,#99,$,.MODEL_VIEW.,$);";
        let document = parse_document(text);

        let hover = hover(
            &document,
            position_at(text, "*"),
            &schema_docs_with_derived_attributes(),
            Some("IFC4"),
        )
        .expect("hover should exist");
        let value = hover_text(hover);

        assert!(value.contains("CoordinateSpaceDimension"));
        assert!(value.contains("derived value"));
    }

    /// Test that hover returns none when there is no syntax tree.
    /// This tests important defensive behavior, since `tree` is defined as `Option<Tree>`, which can be `None`.
    #[test]
    fn hover_returns_none_without_a_syntax_tree() {
        let document = Document {
            text: "#1=IFCWALL($);".to_string(),
            tree: None,
            schema_name: None,
            parse_mode: crate::document::DocumentParseMode::Full,
            definitions: HashMap::new(),
            references: HashMap::new(),
            instances: Vec::new(),
            instance_indexes_by_id: HashMap::new(),
        };

        assert!(hover(&document, Position::new(0, 0), &empty_schema_docs(), None).is_none());
    }

    /// Test that the markdown rendering for entity hover includes inherited and direct attribute tables.
    /// This test breaks when markdown shape changes.
    #[test]
    fn render_entity_hover_renders_inherited_and_direct_attribute_tables() {
        // Create fake EntityDoc with inherited and direct attributes
        let entity = EntityDoc {
            name: "IfcWall".to_string(),
            attributes: vec![
                EntityAttributeDoc {
                    name: "GlobalId".to_string(),
                    type_name: "IfcGloballyUniqueId".to_string(),
                    declared_in: "IfcRoot".to_string(),
                    ty: crate::schema::TypeRef::Primitive(crate::schema::PrimitiveType::String {
                        width: None,
                        fixed: false,
                    }),
                    optional: false,
                    allows_omitted: false,
                },
                EntityAttributeDoc {
                    name: "PredefinedType".to_string(),
                    type_name: "OPTIONAL IfcWallTypeEnum".to_string(),
                    declared_in: "IfcWall".to_string(),
                    ty: crate::schema::TypeRef::Named(crate::schema::NamedTypeRef {
                        name: "IFCWALLTYPEENUM".to_string(),
                        kind: crate::schema::NamedTypeKind::Type,
                    }),
                    optional: true,
                    allows_omitted: false,
                },
            ],
            url: "https://example.invalid/IfcWall.htm".to_string(),
            all_supertypes: HashSet::new(),
        };

        let markdown = render_entity_hover(&entity);

        assert!(markdown.contains("## Inherited Attributes"));
        assert!(markdown.contains("| 1 | *GlobalId* | `IfcGloballyUniqueId` |"));
        assert!(markdown.contains("## Attributes Declared In This Entity"));
        assert!(markdown.contains("| 2 | *PredefinedType* | `OPTIONAL IfcWallTypeEnum` |"));
        assert!(!markdown.contains("Declared In |"));
        assert!(markdown.contains("[Official documentation](https://example.invalid/IfcWall.htm)"));
    }

    /// Test that the markdown rendering for entity hover omits the inherited table when it is empty.
    #[test]
    fn render_entity_hover_omits_empty_inherited_table() {
        // Create fake EntityDoc with empty inherited table
        let entity = EntityDoc {
            name: "IfcRoot".to_string(),
            attributes: vec![EntityAttributeDoc {
                name: "GlobalId".to_string(),
                type_name: "IfcGloballyUniqueId".to_string(),
                declared_in: "IfcRoot".to_string(),
                ty: crate::schema::TypeRef::Primitive(crate::schema::PrimitiveType::String {
                    width: None,
                    fixed: false,
                }),
                optional: false,
                allows_omitted: false,
            }],
            url: "https://example.invalid/IfcRoot.htm".to_string(),
            all_supertypes: HashSet::new(),
        };

        let markdown = render_entity_hover(&entity);

        assert!(!markdown.contains("## Inherited Attributes"));
        assert!(markdown.contains("## Attributes Declared In This Entity"));
        assert!(markdown.contains("| 1 | *GlobalId* | `IfcGloballyUniqueId` |"));
    }
}
