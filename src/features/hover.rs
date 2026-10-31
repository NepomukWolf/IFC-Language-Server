//! Hover feature logic.
//! Resolves text tokens under the cursor and renders schema-backed entity information or local
//! reference previews from the current document. Derived `*` hover is limited to `IfcSIUnit` and
//! `IfcGeometricRepresentationSubContext`.
//! Hover stays synchronous by reading only the in-memory document and schema collections.

use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position, Range};

use crate::document::{Document, EntityInstanceInfo, ParameterValue};
use crate::schema::{
    EntityAttributeDoc, EntityDoc, EnumerationTypeDef, NamedTypeKind, PrimitiveType, SchemaDoc,
    SchemaDocCollection, TypeDoc, TypeRef,
};

const FILE_DESCRIPTION_HOVER: &str = include_str!("hover/static/file_description.md");
const FILE_NAME_HOVER: &str = include_str!("hover/static/file_name.md");
const FILE_SCHEMA_HOVER: &str = include_str!("hover/static/file_schema.md");

pub fn hover(
    document: &Document,
    position: Position,
    schema_docs: &SchemaDocCollection,
    selected_schema_name: Option<&str>,
) -> Option<Hover> {
    if let Some((id, offset)) = document.id_token_at_position(position)
        && document.definitions.get(&id) != Some(&offset)
    {
        let value = render_reference_hover(document, id)?;
        let range = document.id_range_at_offset(offset)?;
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(range),
        });
    }

    if let Some((keyword, range)) = header_keyword_at_position(document, position) {
        let value = render_header_keyword_hover(keyword)?.to_string();
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(range),
        });
    }

    if let Some((entity_text, range)) = document.entity_name_at_position(position)
        && let Some(schema_name) = selected_schema_name.or(document.schema_name.as_deref())
        && let Some(entity_doc) = schema_docs.get_entity_doc(schema_name, &entity_text)
    {
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: render_entity_hover(entity_doc),
            }),
            range: Some(range),
        });
    }

    if let Some((type_text, range)) = typed_value_name_at_position(document, position)
        && let Some(schema_name) = selected_schema_name.or(document.schema_name.as_deref())
        && let Some(schema) = schema_docs.get(schema_name)
        && let Some(type_doc) = schema.type_decl(&type_text)
    {
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: render_type_hover(type_doc),
            }),
            range: Some(range),
        });
    }

    if let Some(schema_name) = selected_schema_name.or(document.schema_name.as_deref())
        && let Some(schema) = schema_docs.get(schema_name)
        && let Some((range, attribute, enum_def)) =
            enum_value_at_position(document, schema, position)
    {
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: render_enum_hover(attribute, enum_def),
            }),
            range: Some(range),
        });
    }

    if let Some((instance, parameter_index)) = parameter_at_position(document, position)
        && let Some(value) = instance.parameters.get(parameter_index)
        && matches!(value, ParameterValue::Omitted { .. })
    {
        let omitted_range = value.range();
        let value = derived_value_hover(
            document,
            instance.id?,
            &instance.entity_name,
            parameter_index,
        )?;
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(omitted_range),
        });
    }

    None
}

pub(crate) fn render_entity_hover(entity_doc: &EntityDoc) -> String {
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

pub(crate) fn render_reference_hover(document: &Document, id: u32) -> Option<String> {
    let preview = document.entity_instance_text_at_definition(id)?;

    Some(format!("```ifc\n{}\n```", preview.trim()))
}

fn header_keyword_at_position(
    document: &Document,
    position: Position,
) -> Option<(&'static str, Range)> {
    let offset = document.position_to_offset(position)?;
    let (start, end) = document.identifier_at_offset(offset)?;

    let keyword = match document.text.get(start..end)?.to_ascii_uppercase().as_str() {
        "FILE_DESCRIPTION" => "FILE_DESCRIPTION",
        "FILE_NAME" => "FILE_NAME",
        "FILE_SCHEMA" => "FILE_SCHEMA",
        _ => return None,
    };

    Some((keyword, document.range_for_offsets(start, end)?))
}

fn render_header_keyword_hover(keyword: &str) -> Option<&'static str> {
    match keyword {
        "FILE_DESCRIPTION" => Some(FILE_DESCRIPTION_HOVER),
        "FILE_NAME" => Some(FILE_NAME_HOVER),
        "FILE_SCHEMA" => Some(FILE_SCHEMA_HOVER),
        _ => None,
    }
}

fn parameter_at_position(
    document: &Document,
    position: Position,
) -> Option<(EntityInstanceInfo, usize)> {
    let instance = document.entity_instance_at_offset(document.position_to_offset(position)?)?;
    let index = instance
        .parameters
        .iter()
        .position(|parameter| contains(parameter.range(), position))?;
    Some((instance, index))
}

fn contains(range: Range, position: Position) -> bool {
    range.start <= position && position < range.end
}

fn typed_value_name_span(
    document: &Document,
    parameter: &ParameterValue,
    position: Position,
    offset: usize,
) -> Option<(usize, usize)> {
    match parameter {
        ParameterValue::Typed { inner, range, .. } if contains(*range, position) => {
            let start = document.position_to_offset(range.start)?;
            let open = document.text.get(start..)?.find('(')? + start;
            if offset < open {
                Some((start, open))
            } else {
                inner
                    .iter()
                    .find_map(|item| typed_value_name_span(document, item, position, offset))
            }
        }
        ParameterValue::List { items, .. } => items
            .iter()
            .find_map(|item| typed_value_name_span(document, item, position, offset)),
        _ => None,
    }
}

fn enum_value_at_position<'a>(
    document: &'a Document,
    schema: &'a SchemaDoc,
    position: Position,
) -> Option<(Range, &'a EntityAttributeDoc, &'a EnumerationTypeDef)> {
    let (instance, parameter_index) = parameter_at_position(document, position)?;
    let parameter = instance.parameters.get(parameter_index)?;
    let attribute = schema
        .entity(&instance.entity_name)?
        .attributes
        .get(parameter_index)?;
    let enum_value = enum_value_in_parameter(parameter, position)?;
    let enum_def = enum_value
        .typed_name
        .and_then(|type_name| enum_type_from_type_name(schema, type_name))
        .or_else(|| enum_type_from_type_ref(schema, &attribute.ty, 0))?;

    Some((enum_value.range, attribute, enum_def))
}

struct EnumValueAtPosition<'a> {
    range: Range,
    typed_name: Option<&'a str>,
}

fn enum_value_in_parameter(
    parameter: &ParameterValue,
    position: Position,
) -> Option<EnumValueAtPosition<'_>> {
    if !contains(parameter.range(), position) {
        return None;
    }

    match parameter {
        ParameterValue::Enumeration { range, .. } => Some(EnumValueAtPosition {
            range: *range,
            typed_name: None,
        }),
        ParameterValue::List { items, .. } => items
            .iter()
            .find_map(|item| enum_value_in_parameter(item, position)),
        ParameterValue::Typed {
            type_name, inner, ..
        } => inner.iter().find_map(|item| {
            enum_value_in_parameter(item, position).map(|mut enum_value| {
                enum_value.typed_name = Some(type_name);
                enum_value
            })
        }),
        _ => None,
    }
}

fn enum_type_from_type_name<'a>(
    schema: &'a SchemaDoc,
    type_name: &str,
) -> Option<&'a EnumerationTypeDef> {
    enum_type_from_type_doc(schema, schema.type_decl(type_name)?, 0)
}

fn enum_type_from_type_ref<'a>(
    schema: &'a SchemaDoc,
    type_ref: &'a TypeRef,
    depth: usize,
) -> Option<&'a EnumerationTypeDef> {
    if depth > 16 {
        return None;
    }

    match type_ref {
        TypeRef::Named(named) => match named.kind {
            NamedTypeKind::Type | NamedTypeKind::Unresolved => {
                enum_type_from_type_doc(schema, schema.type_decl(&named.name)?, depth + 1)
            }
            NamedTypeKind::Entity => None,
        },
        TypeRef::Aggregate(aggregate) => {
            enum_type_from_type_ref(schema, aggregate.item.as_ref(), depth + 1)
        }
        _ => None,
    }
}

fn enum_type_from_type_doc<'a>(
    schema: &'a SchemaDoc,
    type_doc: &'a TypeDoc,
    depth: usize,
) -> Option<&'a EnumerationTypeDef> {
    if depth > 16 {
        return None;
    }

    match type_doc {
        TypeDoc::Enumeration(enumeration) => Some(enumeration),
        TypeDoc::Alias(alias) => enum_type_from_type_ref(schema, &alias.target, depth + 1),
        TypeDoc::Select(select) => {
            let mut matches = select
                .options
                .iter()
                .filter_map(|option| enum_type_from_type_ref(schema, option, depth + 1));
            let first = matches.next()?;
            matches.next().is_none().then_some(first)
        }
    }
}

fn render_enum_hover(attribute: &EntityAttributeDoc, enum_def: &EnumerationTypeDef) -> String {
    let mut markdown = format!(
        "# {}\n\nAttribute: `{}`\n\nOptions:",
        enum_def.name, attribute.name
    );

    for item in &enum_def.items {
        markdown.push_str(&format!("\n- `.{item}.`"));
    }

    markdown
}

fn typed_value_name_at_position(
    document: &Document,
    position: Position,
) -> Option<(String, tower_lsp::lsp_types::Range)> {
    let offset = document.position_to_offset(position)?;
    let (instance, parameter_index) = parameter_at_position(document, position)?;
    let parameter = instance.parameters.get(parameter_index)?;
    let (start, end) = typed_value_name_span(document, parameter, position, offset)?;
    Some((
        document.text[start..end].to_ascii_uppercase(),
        document.range_for_offsets(start, end)?,
    ))
}

fn render_type_hover(type_doc: &TypeDoc) -> String {
    match type_doc {
        TypeDoc::Alias(alias) => format!(
            "# {}\n\nAlias of `{}`",
            alias.name,
            format_alias_target(&alias.target)
        ),
        TypeDoc::Enumeration(enumeration) => format!(
            "# {}\n\nEnumeration type with {} values.",
            enumeration.name,
            enumeration.items.len()
        ),
        TypeDoc::Select(select) => format!(
            "# {}\n\nSelect type with {} options.",
            select.name,
            select.options.len()
        ),
    }
}

fn format_alias_target(ty: &crate::schema::TypeRef) -> String {
    match ty {
        crate::schema::TypeRef::Primitive(primitive) => format_primitive_type(primitive),
        crate::schema::TypeRef::Named(named) => named.name.clone(),
        _ => "complex type".to_string(),
    }
}

fn format_primitive_type(primitive: &PrimitiveType) -> String {
    match primitive {
        PrimitiveType::Number => "NUMBER".to_string(),
        PrimitiveType::Real => "REAL".to_string(),
        PrimitiveType::Integer => "INTEGER".to_string(),
        PrimitiveType::Logical => "LOGICAL".to_string(),
        PrimitiveType::Boolean => "BOOLEAN".to_string(),
        PrimitiveType::String { width, fixed } => format_width_type("STRING", *width, *fixed),
        PrimitiveType::Binary { width, fixed } => format_width_type("BINARY", *width, *fixed),
    }
}

fn format_width_type(name: &str, width: Option<usize>, fixed: bool) -> String {
    match width {
        Some(width) if fixed => format!("{name}({width}) FIXED"),
        Some(width) => format!("{name}({width})"),
        None => name.to_string(),
    }
}

fn derived_value_hover(
    document: &Document,
    instance_id: u32,
    entity_name: &str,
    parameter_index: usize,
) -> Option<String> {
    match (entity_name, parameter_index) {
        ("IFCSIUNIT", 0) => Some(ifc_si_unit_dimensions_hover(document, instance_id)),
        ("IFCGEOMETRICREPRESENTATIONSUBCONTEXT", parameter_index @ 2..=5) => Some(
            subcontext_inherited_attribute_hover(document, instance_id, parameter_index),
        ),
        _ => None,
    }
}

fn ifc_si_unit_dimensions_hover(document: &Document, instance_id: u32) -> String {
    let instance = document.entity_instance_by_id(instance_id);
    let dimensions = instance
        .as_ref()
        .and_then(|instance| instance.parameters.get(3))
        .and_then(|value| match value {
            ParameterValue::Enumeration { value, .. } => ifc_dimensions_for_si_unit(value),
            _ => None,
        });

    let Some(dimensions) = dimensions else {
        return "Dimensions: derived value".to_string();
    };

    format!(
        "Dimensions: `IfcDimensionalExponents({}, {}, {}, {}, {}, {}, {})`\n\nresolved from `self.Name`",
        dimensions[0],
        dimensions[1],
        dimensions[2],
        dimensions[3],
        dimensions[4],
        dimensions[5],
        dimensions[6],
    )
}

fn subcontext_inherited_attribute_hover(
    document: &Document,
    instance_id: u32,
    parameter_index: usize,
) -> String {
    let attribute_name = match parameter_index {
        2 => "CoordinateSpaceDimension",
        3 => "Precision",
        4 => "WorldCoordinateSystem",
        5 => "TrueNorth",
        _ => return "derived value".to_string(),
    };
    let unresolved = || format!("{attribute_name}: derived value");

    let Some(instance) = document.entity_instance_by_id(instance_id) else {
        return unresolved();
    };
    let Some(parent_id) = instance.parameters.get(6).and_then(|value| match value {
        ParameterValue::Reference { id, .. } => Some(*id),
        _ => None,
    }) else {
        return unresolved();
    };
    let Some(parent) = document.entity_instance_by_id(parent_id) else {
        return unresolved();
    };
    let Some(parent_value) = parent.parameters.get(parameter_index) else {
        return unresolved();
    };

    let mut text = match parent_value {
        ParameterValue::Reference { id, .. } => {
            let mut text = format!("{attribute_name}: `#{id}`");
            if let Some(preview) = document.entity_instance_text_at_definition(*id) {
                text.push_str(&format!("\n\n```ifc\n{}\n```", preview.trim()));
            }
            text
        }
        ParameterValue::Number { text, .. } => format!("{attribute_name}: `{text}`"),
        ParameterValue::Null { .. } => format!("{attribute_name}: `$`"),
        _ => return unresolved(),
    };
    text.push_str("\n\nresolved from `self.ParentContext`");
    text
}

fn ifc_dimensions_for_si_unit(unit_name: &str) -> Option<[i32; 7]> {
    Some(match unit_name {
        "METRE" => [1, 0, 0, 0, 0, 0, 0],
        "SQUARE_METRE" => [2, 0, 0, 0, 0, 0, 0],
        "CUBIC_METRE" => [3, 0, 0, 0, 0, 0, 0],
        "GRAM" => [0, 1, 0, 0, 0, 0, 0],
        "SECOND" => [0, 0, 1, 0, 0, 0, 0],
        "AMPERE" => [0, 0, 0, 1, 0, 0, 0],
        "KELVIN" => [0, 0, 0, 0, 1, 0, 0],
        "MOLE" => [0, 0, 0, 0, 0, 1, 0],
        "CANDELA" => [0, 0, 0, 0, 0, 0, 1],
        "RADIAN" | "STERADIAN" => [0, 0, 0, 0, 0, 0, 0],
        "HERTZ" | "BECQUEREL" => [0, 0, -1, 0, 0, 0, 0],
        "NEWTON" => [1, 1, -2, 0, 0, 0, 0],
        "PASCAL" => [-1, 1, -2, 0, 0, 0, 0],
        "JOULE" => [2, 1, -2, 0, 0, 0, 0],
        "WATT" => [2, 1, -3, 0, 0, 0, 0],
        "COULOMB" => [0, 0, 1, 1, 0, 0, 0],
        "VOLT" => [2, 1, -3, -1, 0, 0, 0],
        "FARAD" => [-2, -1, 4, 2, 0, 0, 0],
        "OHM" => [2, 1, -3, -2, 0, 0, 0],
        "SIEMENS" => [-2, -1, 3, 2, 0, 0, 0],
        "WEBER" => [2, 1, -2, -1, 0, 0, 0],
        "TESLA" => [0, 1, -2, -1, 0, 0, 0],
        "HENRY" => [2, 1, -2, -2, 0, 0, 0],
        "DEGREE_CELSIUS" => [0, 0, 0, 0, 1, 0, 0],
        "LUMEN" => [0, 0, 0, 0, 0, 0, 1],
        "LUX" => [-2, 0, 0, 0, 0, 0, 1],
        "GRAY" | "SIEVERT" => [2, 0, -2, 0, 0, 0, 0],
        _ => return None,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::EntityAttributeDoc;

    /// Helper function to parse a document and return the result.
    fn parse_document(text: &str) -> Document {
        Document::parse(text.to_string())
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

    #[test]
    fn hover_decodes_only_the_statements_it_reads() {
        let mut text = String::from("DATA;\n");
        for id in 1..=2000 {
            text.push_str(&format!(
                "#{id}=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);\n"
            ));
        }
        let document = parse_document(&text);
        let star = text.find("#1000=").unwrap() + "#1000=IFCSIUNIT(".len();
        let position = document.offset_to_position(star).unwrap();
        crate::document::DECODED_STATEMENTS.with(|count| count.set(0));

        let value = hover_text(
            hover(&document, position, &empty_schema_docs(), None).expect("hover exists"),
        );

        assert!(value.contains("IfcDimensionalExponents(1, 0, 0, 0, 0, 0, 0)"));
        let decoded = crate::document::DECODED_STATEMENTS.with(|count| count.get());
        assert!(decoded <= 4, "decoded {decoded} statements");
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

    /// Test that hover returns schema docs for entity names with detected version in the text.
    /// Partial hover text assertion since this should only test the schema docs are returned, not the exact text.
    #[test]
    fn hover_returns_schema_docs_for_entity_names_with_detected_version() {
        let text = "ISO-10303-21;HEADER;FILE_SCHEMA(('IFC4'));ENDSEC;DATA;#1=IFCWALL($);ENDSEC;END-ISO-10303-21;";
        let document = Document::new_unloaded(text.to_string());

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

    #[test]
    fn hover_returns_schema_docs_for_inline_defined_types() {
        let text = "ISO-10303-21;HEADER;FILE_SCHEMA(('IFC4'));ENDSEC;DATA;#1=IFCPROPERTYSINGLEVALUE('Name',$,IFCLABEL('Living Room'));ENDSEC;END-ISO-10303-21;";
        let schema = crate::schema::load_express(
            crate::schema::IfcVersion::Ifc4Add2Tc1,
            r#"
            SCHEMA IFC4;
              TYPE IfcLabel = STRING(255);
              END_TYPE;
            END_SCHEMA;
            "#,
        )
        .expect("fixture schema should parse");
        let schema_docs = SchemaDocCollection::from_docs([("IFC4".to_string(), schema)]);
        let document = parse_document(text);

        let hover = hover(&document, position_at(text, "IFCLABEL"), &schema_docs, None)
            .expect("hover should exist");
        let value = hover_text(hover);

        assert!(value.contains("# IfcLabel"));
        assert!(value.contains("Alias of `STRING(255)`"));
    }

    #[test]
    fn hover_returns_enum_options_for_attribute_value() {
        let text = "#1=IFCWALL($,.USERDEFINED.);";
        let document = parse_document(text);

        let hover = hover(
            &document,
            position_at(text, ".USERDEFINED."),
            &schema_docs_with_wall(),
            Some("IFC4"),
        )
        .expect("hover should exist");
        let range = hover.range.expect("hover should have a range");
        let value = hover_text(hover);

        assert_eq!(range.start, Position::new(0, 13));
        assert_eq!(range.end, Position::new(0, 26));
        assert!(value.contains("# IfcWallTypeEnum"));
        assert!(value.contains("Attribute: `PredefinedType`"));
        assert!(!value.contains("Current value:"));
        assert!(value.contains("- `.MOVABLE.`"));
        assert!(value.contains("- `.USERDEFINED.`"));
    }

    #[test]
    fn hover_returns_enum_options_after_non_ascii_text() {
        let text = "#1=IFCWALL('Wänd',.USERDEFINED.);";
        let document = parse_document(text);
        let enum_offset = text.find(".USERDEFINED.").expect("enum value should exist");
        let position = document
            .offset_to_position(enum_offset)
            .expect("enum offset should convert to an LSP position");

        let hover = hover(&document, position, &schema_docs_with_wall(), Some("IFC4"))
            .expect("hover should exist");
        let range = hover.range.expect("hover should have a range");

        assert_eq!(range.start, position);
        assert!(hover_text(hover).contains("# IfcWallTypeEnum"));
    }

    #[test]
    fn hover_returns_enum_options_with_bundled_ifc4_schema() {
        let text = "ISO-10303-21;HEADER;FILE_SCHEMA(('IFC4'));ENDSEC;DATA;#1=IFCWALL('id',$,$,$,$,$,$,$,.STANDARD.);ENDSEC;END-ISO-10303-21;";
        let document = parse_document(text);

        let hover = hover(
            &document,
            position_at(text, ".STANDARD."),
            &SchemaDocCollection::new(),
            None,
        )
        .expect("hover should exist");
        let value = hover_text(hover);

        assert!(value.contains("# IfcWallTypeEnum"));
        assert!(value.contains("Attribute: `PredefinedType`"));
        assert!(value.contains("- `.STANDARD.`"));
    }

    #[test]
    fn hover_returns_enum_options_for_typed_enum_value() {
        let text = "#1=IFCTHING($,IFCWALLTYPEENUM(.MOVABLE.));";
        let schema = crate::schema::load_express(
            crate::schema::IfcVersion::Ifc4Add2Tc1,
            r#"
            SCHEMA IFC4;
              TYPE IfcWallTypeEnum = ENUMERATION OF (MOVABLE, USERDEFINED);
              END_TYPE;
              ENTITY IfcThing;
                Name : OPTIONAL STRING;
                PredefinedType : IfcWallTypeEnum;
              END_ENTITY;
            END_SCHEMA;
            "#,
        )
        .expect("fixture schema should parse");
        let schema_docs = SchemaDocCollection::from_docs([("IFC4".to_string(), schema)]);
        let document = parse_document(text);

        let hover = hover(
            &document,
            position_at(text, ".MOVABLE."),
            &schema_docs,
            Some("IFC4"),
        )
        .expect("hover should exist");
        let value = hover_text(hover);

        assert!(value.contains("# IfcWallTypeEnum"));
        assert!(value.contains("Attribute: `PredefinedType`"));
        assert!(!value.contains("Current value:"));
        assert!(value.contains("- `.USERDEFINED.`"));
    }

    #[test]
    fn hover_ranges_count_utf16_units_after_non_ascii_text() {
        let text = "#1=IFCTHING('é😀',IFCWALLTYPEENUM(.MOVABLE.));";
        let schema = crate::schema::load_express(
            crate::schema::IfcVersion::Ifc4Add2Tc1,
            r#"
            SCHEMA IFC4;
              TYPE IfcWallTypeEnum = ENUMERATION OF (MOVABLE, USERDEFINED);
              END_TYPE;
              ENTITY IfcThing;
                Name : OPTIONAL STRING;
                PredefinedType : IfcWallTypeEnum;
              END_ENTITY;
            END_SCHEMA;
            "#,
        )
        .expect("fixture schema should parse");
        let schema_docs = SchemaDocCollection::from_docs([("IFC4".to_string(), schema)]);
        let document = parse_document(text);
        let utf16_position = |needle: &str| {
            Position::new(
                0,
                text[..text.find(needle).unwrap()].encode_utf16().count() as u32,
            )
        };
        let utf16_range = |needle: &str| {
            let start = utf16_position(needle);
            Range::new(
                start,
                Position::new(0, start.character + needle.encode_utf16().count() as u32),
            )
        };

        let enum_hover = hover(
            &document,
            utf16_position(".MOVABLE."),
            &schema_docs,
            Some("IFC4"),
        )
        .expect("enum hover should exist");
        let (name, type_range) =
            typed_value_name_at_position(&document, utf16_position("IFCWALLTYPEENUM"))
                .expect("inline typed value should resolve");

        assert_eq!(enum_hover.range, Some(utf16_range(".MOVABLE.")));
        assert_eq!(name, "IFCWALLTYPEENUM");
        assert_eq!(type_range, utf16_range("IFCWALLTYPEENUM"));
    }

    #[test]
    fn hover_returns_none_for_enum_value_without_schema() {
        let text = "#1=IFCWALL($,.USERDEFINED.);";
        let document = parse_document(text);

        let hover = hover(
            &document,
            position_at(text, ".USERDEFINED."),
            &empty_schema_docs(),
            Some("IFC4"),
        );

        assert!(hover.is_none());
    }

    #[test]
    fn typed_value_name_detection_finds_inline_type_name() {
        let text = "#1=IFCPROPERTYSINGLEVALUE('Name',$,IFCLABEL('Living Room'));";
        let document = parse_document(text);

        let (name, range) = typed_value_name_at_position(&document, position_at(text, "IFCLABEL"))
            .expect("inline typed value should resolve");

        assert_eq!(name, "IFCLABEL");
        assert_eq!(range.start, Position::new(0, 35));
        assert_eq!(range.end, Position::new(0, 43));
    }

    #[test]
    fn typed_value_name_detection_ignores_entity_definition_name() {
        let text = "#1=IFCWALL($);";
        let document = parse_document(text);

        assert!(typed_value_name_at_position(&document, position_at(text, "IFCWALL")).is_none());
    }

    /// Test that omitted values resolve through the handwritten parameter index.
    #[test]
    fn omitted_value_context_finds_instance_and_parameter() {
        let text = "#15=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);";
        let document = parse_document(text);
        let (instance, parameter_index) = parameter_at_position(&document, position_at(text, "*"))
            .expect("context should resolve from index");

        assert_eq!(instance.id, Some(15));
        assert_eq!(instance.entity_name, "IFCSIUNIT");
        assert_eq!(parameter_index, 0);
    }

    #[test]
    fn hover_returns_definition_preview_for_references() {
        let text = "#1=IFCWALL($);\n#2=IFCDOOR(#1);";
        let document = Document::new_unloaded(text.to_string());

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
            &empty_schema_docs(),
            None,
        )
        .expect("hover should exist");
        let value = hover_text(hover);

        assert!(value.contains("Dimensions"));
        assert!(value.contains("IfcDimensionalExponents(1, 0, 0, 0, 0, 0, 0)"));
    }

    #[test]
    fn hover_returns_resolved_zero_dimensions_for_radian() {
        let text = "#18=IFCSIUNIT(*,.PLANEANGLEUNIT.,$,.RADIAN.);";
        let document = parse_document(text);

        let hover = hover(
            &document,
            position_at(text, "*"),
            &empty_schema_docs(),
            None,
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
            &empty_schema_docs(),
            None,
        )
        .expect("hover should exist");
        let value = hover_text(hover);

        assert!(value.contains("TrueNorth"));
        assert!(value.contains("TrueNorth: `$`"));
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
            &empty_schema_docs(),
            None,
        )
        .expect("hover should exist");
        let value = hover_text(hover);

        assert!(value.contains("WorldCoordinateSystem"));
        assert!(value.contains("`#7`"));
        assert!(value.contains("#7=IFCAXIS2PLACEMENT3D();"));
    }

    #[test]
    fn hover_explains_unresolved_parent_context() {
        let text = "#12=IFCGEOMETRICREPRESENTATIONSUBCONTEXT('Body','Model',*,*,*,*,#99,$,.MODEL_VIEW.,$);";
        let document = parse_document(text);

        let hover = hover(
            &document,
            position_at(text, "*"),
            &empty_schema_docs(),
            None,
        )
        .expect("hover should exist");
        let value = hover_text(hover);

        assert!(value.contains("CoordinateSpaceDimension"));
        assert!(value.contains("derived value"));
    }

    #[test]
    fn hover_ignores_unsupported_omitted_values() {
        let text = "#1=IFCWALL(*);";
        let document = parse_document(text);

        assert!(
            hover(
                &document,
                position_at(text, "*"),
                &empty_schema_docs(),
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn hover_returns_none_for_definition_id_without_a_syntax_tree() {
        let document = Document::new_unloaded("#1=IFCWALL($);".to_string());

        assert!(hover(&document, Position::new(0, 0), &empty_schema_docs(), None).is_none());
    }

    #[test]
    fn hover_returns_enum_value_without_a_syntax_tree() {
        let text = "#1=IFCWALL($,.USERDEFINED.);";
        let document = Document::new_unloaded(text.to_string());

        let hover = hover(
            &document,
            position_at(text, ".USERDEFINED."),
            &schema_docs_with_wall(),
            Some("IFC4"),
        );

        let value = hover_text(hover.expect("handwritten index should resolve enum context"));
        assert!(value.contains("# IfcWallTypeEnum"));
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
            all_supertypes: Default::default(),
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
            all_supertypes: Default::default(),
        };

        let markdown = render_entity_hover(&entity);

        assert!(!markdown.contains("## Inherited Attributes"));
        assert!(markdown.contains("## Attributes Declared In This Entity"));
        assert!(markdown.contains("| 1 | *GlobalId* | `IfcGloballyUniqueId` |"));
    }

    #[test]
    fn render_type_hover_renders_alias_types() {
        let markdown = render_type_hover(&TypeDoc::Alias(crate::schema::AliasTypeDef {
            name: "IfcLabel".to_string(),
            target: crate::schema::TypeRef::Primitive(crate::schema::PrimitiveType::String {
                width: Some(255),
                fixed: false,
            }),
        }));

        assert!(markdown.contains("# IfcLabel"));
        assert!(markdown.contains("Alias of `STRING(255)`"));
    }
}
