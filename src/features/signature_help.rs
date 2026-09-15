//! Signature-help feature logic.
//! Finds the current STEP entity argument list with a lightweight text scanner and renders
//! schema-backed parameter information without requiring tree-sitter AST state.

use tower_lsp::lsp_types::{
    Documentation, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, Position,
    SignatureHelp, SignatureInformation,
};

use crate::document::Document;
use crate::features::step_scan::{scan_block_comment, scan_identifier, scan_string, skip_trivia};
use crate::schema::{EntityAttributeDoc, EntityDoc, SchemaDocCollection};

pub fn signature_help(
    document: &Document,
    position: Position,
    schema_docs: &SchemaDocCollection,
    selected_schema_name: Option<&str>,
) -> Option<SignatureHelp> {
    let offset = document.position_to_offset(position)?;
    let context = entity_argument_context(&document.text, offset)?;
    let schema_name = selected_schema_name.or(document.schema_name.as_deref())?;
    let entity = schema_docs.get_entity_doc(schema_name, &context.entity_name)?;
    let active_parameter = active_parameter_index(context.active_parameter, entity);

    Some(SignatureHelp {
        signatures: vec![render_signature(entity, active_parameter)],
        active_signature: Some(0),
        active_parameter: Some(active_parameter as u32),
    })
}

#[derive(Debug, PartialEq, Eq)]
struct EntityArgumentContext {
    entity_name: String,
    active_parameter: usize,
}

fn render_signature(entity: &EntityDoc, active_parameter: usize) -> SignatureInformation {
    SignatureInformation {
        label: format!(
            "{}({})",
            entity.name,
            entity
                .attributes
                .iter()
                .map(|attribute| attribute.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        documentation: None,
        parameters: Some(
            entity
                .attributes
                .iter()
                .map(render_parameter)
                .collect::<Vec<_>>(),
        ),
        active_parameter: Some(active_parameter as u32),
    }
}

fn render_parameter(attribute: &EntityAttributeDoc) -> ParameterInformation {
    let mut documentation = format!("Type: `{}`", attribute.type_name);
    if attribute.optional {
        documentation.push_str("\n\nOptional.");
    }
    if attribute.allows_omitted {
        documentation.push_str("\n\nMay be omitted with `*`.");
    }

    ParameterInformation {
        label: ParameterLabel::Simple(attribute.name.clone()),
        documentation: Some(Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: documentation,
        })),
    }
}

fn active_parameter_index(parameter_index: usize, entity: &EntityDoc) -> usize {
    entity
        .attributes
        .len()
        .checked_sub(1)
        .map_or(0, |last| parameter_index.min(last))
}

fn entity_argument_context(text: &str, cursor_offset: usize) -> Option<EntityArgumentContext> {
    if cursor_offset > text.len() || !text.is_char_boundary(cursor_offset) {
        return None;
    }

    let statement_start = statement_start_before_cursor(text, cursor_offset);
    let bytes = text.as_bytes();
    let mut offset = statement_start;

    offset = skip_trivia(bytes, offset, bytes.len());
    offset = parse_instance_id(bytes, offset)?;
    offset = skip_trivia(bytes, offset, bytes.len());
    if bytes.get(offset) != Some(&b'=') {
        return None;
    }
    offset += 1;
    offset = skip_trivia(bytes, offset, bytes.len());

    let entity_start = offset;
    offset = scan_identifier(bytes, offset, bytes.len())?;
    let entity_name = text.get(entity_start..offset)?.to_ascii_uppercase();
    offset = skip_trivia(bytes, offset, bytes.len());
    if bytes.get(offset) != Some(&b'(') || cursor_offset <= offset {
        return None;
    }

    let active_parameter = active_parameter_before_cursor(text, offset + 1, cursor_offset)?;
    Some(EntityArgumentContext {
        entity_name,
        active_parameter,
    })
}

fn statement_start_before_cursor(text: &str, cursor_offset: usize) -> usize {
    let bytes = text.as_bytes();
    let mut offset = 0;
    let mut statement_start = 0;

    while offset < cursor_offset {
        match bytes[offset] {
            b'\'' => offset = scan_string(bytes, offset, cursor_offset),
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                offset = scan_block_comment(bytes, offset, cursor_offset);
            }
            b';' => {
                offset += 1;
                statement_start = offset;
            }
            _ => offset += 1,
        }
    }

    statement_start
}

fn active_parameter_before_cursor(
    text: &str,
    parameter_start: usize,
    cursor_offset: usize,
) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut offset = parameter_start;
    let mut depth = 0usize;
    let mut active_parameter = 0usize;

    while offset < cursor_offset {
        match bytes[offset] {
            b'\'' => offset = scan_string(bytes, offset, cursor_offset),
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                offset = scan_block_comment(bytes, offset, cursor_offset);
            }
            b'(' => {
                depth += 1;
                offset += 1;
            }
            b')' if depth == 0 => return None,
            b')' => {
                depth -= 1;
                offset += 1;
            }
            b',' if depth == 0 => {
                active_parameter += 1;
                offset += 1;
            }
            _ => offset += 1,
        }
    }

    Some(active_parameter)
}

fn parse_instance_id(bytes: &[u8], mut offset: usize) -> Option<usize> {
    if bytes.get(offset) != Some(&b'#') {
        return None;
    }
    offset += 1;
    let digit_start = offset;
    while bytes.get(offset).is_some_and(|byte| byte.is_ascii_digit()) {
        offset += 1;
    }
    (offset > digit_start).then_some(offset)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tower_lsp::lsp_types::Position;

    use super::*;
    use crate::schema::{EntityAttributeDoc, EntityDoc, SchemaDoc, SchemaDocCollection, TypeRef};

    fn position_at(document: &Document, text: &str, needle: &str) -> Position {
        let offset = text.find(needle).expect("needle should exist");
        document
            .offset_to_position(offset)
            .expect("offset should convert to an LSP position")
    }

    fn position_after(document: &Document, text: &str, needle: &str) -> Position {
        let offset = text.find(needle).expect("needle should exist") + needle.len();
        document
            .offset_to_position(offset)
            .expect("offset should convert to an LSP position")
    }

    fn schema_docs_with_wall() -> SchemaDocCollection {
        let entity = EntityDoc {
            name: "IfcWall".to_string(),
            attributes: vec![
                attribute("GlobalId", "IfcGloballyUniqueId", false, false),
                attribute("OwnerHistory", "OPTIONAL IfcOwnerHistory", true, false),
                attribute("Name", "OPTIONAL IfcLabel", true, false),
                attribute("Representation", "IfcProductRepresentation", false, true),
            ],
            url: String::new(),
            all_supertypes: HashSet::new(),
        };
        SchemaDocCollection::from_docs([(
            "IFC4".to_string(),
            SchemaDoc {
                entities: [("IFCWALL".to_string(), entity)].into(),
                types: Default::default(),
            },
        )])
    }

    fn attribute(
        name: &str,
        type_name: &str,
        optional: bool,
        allows_omitted: bool,
    ) -> EntityAttributeDoc {
        EntityAttributeDoc {
            name: name.to_string(),
            type_name: type_name.to_string(),
            declared_in: "IfcWall".to_string(),
            ty: TypeRef::Generic { label: None },
            optional,
            allows_omitted,
        }
    }

    fn active_parameter(help: SignatureHelp) -> u32 {
        help.active_parameter
            .expect("signature help should set active parameter")
    }

    #[test]
    fn returns_signature_inside_entity_parameter_list() {
        let text = "ISO-10303-21;HEADER;FILE_SCHEMA(('IFC4'));ENDSEC;DATA;#1=IFCWALL($);";
        let document = Document::new_unloaded(text.to_string());

        let help = signature_help(
            &document,
            position_at(&document, text, "$"),
            &schema_docs_with_wall(),
            None,
        )
        .expect("signature help should exist");

        assert_eq!(
            help.signatures[0].label,
            "IfcWall(GlobalId, OwnerHistory, Name, Representation)"
        );
        assert_eq!(active_parameter(help), 0);
    }

    #[test]
    fn counts_top_level_commas_for_active_parameter() {
        let text = "#1=IFCWALL('id',#2,$,*);";
        let document = Document::new_unloaded(text.to_string());

        let help = signature_help(
            &document,
            position_at(&document, text, "$"),
            &schema_docs_with_wall(),
            Some("IFC4"),
        )
        .expect("signature help should exist");

        assert_eq!(active_parameter(help), 2);
    }

    #[test]
    fn ignores_commas_inside_nested_values_strings_and_comments() {
        let text = "#1=IFCWALL(IFCLABEL('a,b'),/* c,d */#2,$);";
        let document = Document::new_unloaded(text.to_string());

        let help = signature_help(
            &document,
            position_at(&document, text, "$"),
            &schema_docs_with_wall(),
            Some("IFC4"),
        )
        .expect("signature help should exist");

        assert_eq!(active_parameter(help), 2);
    }

    #[test]
    fn returns_none_outside_entity_parameter_list() {
        let text = "#1=IFCWALL($);";
        let document = Document::new_unloaded(text.to_string());

        let help = signature_help(
            &document,
            position_after(&document, text, ");"),
            &schema_docs_with_wall(),
            Some("IFC4"),
        );

        assert!(help.is_none());
    }

    #[test]
    fn returns_none_for_unknown_schema_or_entity() {
        let text = "#1=IFCDOOR($);";
        let document = Document::new_unloaded(text.to_string());

        assert!(
            signature_help(
                &document,
                position_at(&document, text, "$"),
                &schema_docs_with_wall(),
                Some("IFC4")
            )
            .is_none()
        );
        assert!(
            signature_help(
                &document,
                position_at(&document, text, "$"),
                &schema_docs_with_wall(),
                Some("IFC2X3")
            )
            .is_none()
        );
    }

    #[test]
    fn supports_missing_closing_parenthesis_while_editing() {
        let text = "#1=IFCWALL('id',";
        let document = Document::new_unloaded(text.to_string());

        let help = signature_help(
            &document,
            position_after(&document, text, ","),
            &schema_docs_with_wall(),
            Some("IFC4"),
        )
        .expect("signature help should exist");

        assert_eq!(active_parameter(help), 1);
    }

    #[test]
    fn maps_positions_after_non_ascii_text() {
        let text = "/* Wänd */ #1=IFCWALL('id',#2);";
        let document = Document::new_unloaded(text.to_string());

        let help = signature_help(
            &document,
            position_at(&document, text, "#2"),
            &schema_docs_with_wall(),
            Some("IFC4"),
        )
        .expect("signature help should exist");

        assert_eq!(active_parameter(help), 1);
    }

    #[test]
    fn clamps_extra_commas_to_last_schema_attribute() {
        let text = "#1=IFCWALL($,$,$,$,$,$);";
        let document = Document::new_unloaded(text.to_string());

        let help = signature_help(
            &document,
            position_after(&document, text, "$,$,$,$,$,"),
            &schema_docs_with_wall(),
            Some("IFC4"),
        )
        .expect("signature help should exist");

        assert_eq!(active_parameter(help), 3);
    }

    #[test]
    fn includes_parameter_documentation() {
        let text = "#1=IFCWALL($,#2,$,*);";
        let document = Document::new_unloaded(text.to_string());

        let help = signature_help(
            &document,
            position_at(&document, text, "#2"),
            &schema_docs_with_wall(),
            Some("IFC4"),
        )
        .expect("signature help should exist");
        let parameters = help.signatures[0]
            .parameters
            .as_ref()
            .expect("parameters should exist");

        assert_eq!(parameters.len(), 4);
        assert!(matches!(
            parameters[1].label,
            ParameterLabel::Simple(ref label) if label == "OwnerHistory"
        ));
        assert!(matches!(
            parameters[1].documentation,
            Some(Documentation::MarkupContent(ref markdown))
                if markdown.value.contains("OPTIONAL IfcOwnerHistory")
                    && markdown.value.contains("Optional.")
        ));
        assert!(matches!(
            parameters[3].documentation,
            Some(Documentation::MarkupContent(ref markdown))
                if markdown.value.contains("May be omitted with `*`.")
        ));
    }
}
