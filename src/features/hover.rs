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

    if let Some(offset) = document.position_to_offset(position)
        && let Some((instance, parameter_index)) = parameter_at_offset(document, offset)
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
        let range = document.range_for_offsets(
            range_offset(document, omitted_range)?.0,
            range_offset(document, omitted_range)?.1,
        )?;
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(range),
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

fn parameter_at_offset(document: &Document, offset: usize) -> Option<(EntityInstanceInfo, usize)> {
    let instance = document.entity_instance_at_offset(offset)?;
    let index = instance
        .parameters
        .iter()
        .position(|parameter| parameter_contains_offset(document, parameter, offset))?;
    Some((instance, index))
}

fn parameter_contains_offset(
    document: &Document,
    parameter: &ParameterValue,
    offset: usize,
) -> bool {
    range_offset(document, parameter.range())
        .is_some_and(|(start, end)| start <= offset && offset < end)
}

fn typed_value_name_span(
    document: &Document,
    parameter: &ParameterValue,
    offset: usize,
) -> Option<(usize, usize)> {
    match parameter {
        ParameterValue::Typed { inner, range, .. } => {
            let (start, end) = range_offset(document, *range)?;
            let open = document.text.get(start..end)?.find('(')? + start;
            if start <= offset && offset < open {
                Some((start, open))
            } else {
                inner
                    .iter()
                    .find_map(|item| typed_value_name_span(document, item, offset))
            }
        }
        ParameterValue::List { items, .. } => items
            .iter()
            .find_map(|item| typed_value_name_span(document, item, offset)),
        _ => None,
    }
}

fn enum_value_at_position<'a>(
    document: &'a Document,
    schema: &'a SchemaDoc,
    position: Position,
) -> Option<(Range, &'a EntityAttributeDoc, &'a EnumerationTypeDef)> {
    let offset = document.position_to_offset(position)?;
    let (instance, parameter_index) = parameter_at_offset(document, offset)?;
    let parameter = instance.parameters.get(parameter_index)?;
    let attribute = schema
        .entity(&instance.entity_name)?
        .attributes
        .get(parameter_index)?;
    let enum_value = enum_value_in_parameter(document, parameter, offset)?;
    let enum_def = enum_value
        .typed_name
        .and_then(|type_name| enum_type_from_type_name(schema, type_name))
        .or_else(|| enum_type_from_type_ref(schema, &attribute.ty, 0))?;
    let range = document.range_for_offsets(enum_value.start_offset, enum_value.end_offset)?;

    Some((range, attribute, enum_def))
}

struct EnumValueAtPosition<'a> {
    start_offset: usize,
    end_offset: usize,
    typed_name: Option<&'a str>,
}

fn enum_value_in_parameter<'a>(
    document: &Document,
    parameter: &'a ParameterValue,
    offset: usize,
) -> Option<EnumValueAtPosition<'a>> {
    let (start_offset, end_offset) = range_offset(document, parameter.range())?;
    if offset < start_offset || offset >= end_offset {
        return None;
    }

    match parameter {
        ParameterValue::Enumeration { .. } => Some(EnumValueAtPosition {
            start_offset,
            end_offset,
            typed_name: None,
        }),
        ParameterValue::List { items, .. } => items
            .iter()
            .find_map(|item| enum_value_in_parameter(document, item, offset)),
        ParameterValue::Typed {
            type_name, inner, ..
        } => inner.iter().find_map(|item| {
            enum_value_in_parameter(document, item, offset).map(|mut enum_value| {
                enum_value.typed_name = Some(type_name);
                enum_value
            })
        }),
        _ => None,
    }
}

fn range_offset(document: &Document, range: Range) -> Option<(usize, usize)> {
    let start = document
        .line_offsets
        .get(range.start.line as usize)?
        .checked_add(range.start.character as usize)?;
    let end = document
        .line_offsets
        .get(range.end.line as usize)?
        .checked_add(range.end.character as usize)?;
    (end <= document.text.len()).then_some((start, end))
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
    let (instance, parameter_index) = parameter_at_offset(document, offset)?;
    let parameter = instance.parameters.get(parameter_index)?;
    let (start, end) = typed_value_name_span(document, parameter, offset)?;
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
