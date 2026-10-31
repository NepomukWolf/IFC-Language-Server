//! Signature-help feature logic.
//! Finds the current STEP entity argument list with a lightweight text scanner and renders
//! schema-backed parameter information without the statement index.

use tower_lsp::lsp_types::{
    Documentation, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, Position,
    SignatureHelp, SignatureInformation,
};

use crate::document::Document;
use crate::features::step_scan::entity_argument_context;
use crate::schema::{EntityAttributeDoc, EntityDoc, SchemaDocCollection};

pub fn signature_help(
    document: &Document,
    position: Position,
    schema_docs: &SchemaDocCollection,
    selected_schema_name: Option<&str>,
) -> Option<SignatureHelp> {
    let offset = document.position_to_offset(position)?;
    let context = entity_argument_context(&document.text, &document.definitions, offset)?;
    let schema_name = selected_schema_name.or(document.schema_name.as_deref())?;
    let entity = schema_docs.get_entity_doc(schema_name, &context.entity_name)?;
    let active_parameter = active_parameter_index(context.active_parameter, entity);

    Some(SignatureHelp {
        signatures: vec![render_signature(entity, active_parameter)],
        active_signature: Some(0),
        active_parameter: Some(active_parameter as u32),
    })
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

#[cfg(test)]
mod tests {
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
            all_supertypes: Default::default(),
        };
        SchemaDocCollection::from_docs([(
            "IFC4".to_string(),
            SchemaDoc {
                entities: [("IFCWALL".to_string(), entity)].into_iter().collect(),
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
