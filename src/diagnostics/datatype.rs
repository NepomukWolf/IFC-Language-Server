//! Datatype validation diagnostics for IFC entity attributes.
//! This module compares parsed entity-instance arguments against the runtime schema docs and
//! reports arity, reference, primitive, aggregate, enumeration, and select-type mismatches.
//! General EXPRESS `WHERE` rules are intentionally out of scope for the current implementation.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Range};
use tree_sitter::Node;

use crate::document::{Document, EntityInstanceInfo, ParameterValue};
use crate::schema::{
    AggregateKind, AggregateTypeRef, BoundValue, EntityAttributeDoc, EntityDoc, NamedTypeKind,
    PrimitiveType, SchemaDoc, SelectTypeDef, TypeDoc, TypeRef,
};

pub fn collect_with_schema_name(
    document: &Document,
    schema: &SchemaDoc,
    schema_name: Option<&str>,
) -> Vec<Diagnostic> {
    collect_with_options(
        document,
        schema,
        schema_name,
        DiagnosticOptions {
            reference_document: None,
        },
    )
}

pub fn collect_visible_with_schema_name(
    document: &Document,
    reference_document: &Document,
    schema: &SchemaDoc,
    schema_name: Option<&str>,
) -> Vec<Diagnostic> {
    collect_with_options(
        document,
        schema,
        schema_name,
        DiagnosticOptions {
            reference_document: Some(reference_document),
        },
    )
}

struct DiagnosticOptions<'a> {
    reference_document: Option<&'a Document>,
}

fn collect_with_options(
    document: &Document,
    schema: &SchemaDoc,
    schema_name: Option<&str>,
    options: DiagnosticOptions<'_>,
) -> Vec<Diagnostic> {
    let mut diagnostics = collect_syntax_diagnostics(document);

    for instance in &document.instances {
        let Some(entity) = schema.entity(&instance.entity_name) else {
            diagnostics.push(unknown_entity_diagnostic(instance, schema_name));
            continue;
        };

        validate_instance(
            document,
            schema,
            entity,
            instance,
            &mut diagnostics,
            &options,
        );
    }

    diagnostics
}

fn validate_instance(
    document: &Document,
    schema: &SchemaDoc,
    entity: &EntityDoc,
    instance: &EntityInstanceInfo,
    diagnostics: &mut Vec<Diagnostic>,
    options: &DiagnosticOptions<'_>,
) {
    if instance.parameters.len() != entity.attributes.len() {
        diagnostics.push(Diagnostic {
            range: instance
                .parameter_list_range
                .unwrap_or(instance.entity_range),
            severity: Some(DiagnosticSeverity::ERROR),
            message: format!(
                "{} expects {} attributes but found {}",
                entity.name,
                entity.attributes.len(),
                instance.parameters.len()
            ),
            ..Default::default()
        });
    }

    for (value, attribute) in instance.parameters.iter().zip(&entity.attributes) {
        if let Some(message) = validate_value(document, schema, value, attribute, options) {
            diagnostics.push(Diagnostic {
                range: value.range(),
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!("{}: {}", attribute.name, message),
                ..Default::default()
            });
        }
    }
}

fn unknown_entity_diagnostic(
    instance: &EntityInstanceInfo,
    schema_name: Option<&str>,
) -> Diagnostic {
    let schema_context = schema_name
        .map(|name| format!(" in selected schema `{}`", name))
        .unwrap_or_else(|| " in the selected schema".to_string());

    Diagnostic {
        range: instance.entity_name_range,
        severity: Some(DiagnosticSeverity::ERROR),
        message: format!(
            "Unknown IFC entity `{}`{}",
            instance.entity_name, schema_context
        ),
        ..Default::default()
    }
}

fn validate_value(
    document: &Document,
    schema: &SchemaDoc,
    value: &ParameterValue,
    attribute: &EntityAttributeDoc,
    options: &DiagnosticOptions<'_>,
) -> Option<String> {
    match value {
        ParameterValue::Null { .. } => {
            if attribute.optional {
                None
            } else {
                Some("attribute is required and does not allow `$`".to_string())
            }
        }
        ParameterValue::Omitted { .. } => {
            if attribute.allows_omitted {
                None
            } else {
                Some("`*` is not supported for this attribute".to_string())
            }
        }
        _ => validate_non_null_value(document, schema, value, &attribute.ty, options),
    }
}

fn validate_non_null_value(
    document: &Document,
    schema: &SchemaDoc,
    value: &ParameterValue,
    expected: &TypeRef,
    options: &DiagnosticOptions<'_>,
) -> Option<String> {
    match expected {
        TypeRef::Primitive(primitive) => validate_primitive(value, primitive),
        TypeRef::Aggregate(aggregate) => {
            validate_aggregate(document, schema, value, aggregate, options)
        }
        TypeRef::GenericEntity { .. } | TypeRef::Generic { .. } => None,
        TypeRef::Named(named) => match named.kind {
            NamedTypeKind::Entity => {
                validate_entity_reference(document, schema, value, &named.name, options)
            }
            NamedTypeKind::Type | NamedTypeKind::Unresolved => {
                validate_named_type(document, schema, value, &named.name, options)
            }
        },
    }
}

fn validate_named_type(
    document: &Document,
    schema: &SchemaDoc,
    value: &ParameterValue,
    type_name: &str,
    options: &DiagnosticOptions<'_>,
) -> Option<String> {
    let type_def = schema.type_decl(type_name)?;
    match type_def {
        TypeDoc::Alias(alias) => {
            if let ParameterValue::Typed {
                type_name: inline_name,
                inner,
                ..
            } = value
            {
                if inline_name != type_name {
                    return Some(format!(
                        "expected typed value `{}` but found `{}`",
                        type_name, inline_name
                    ));
                }

                if inner.len() != 1 {
                    Some(format!(
                        "typed value `{}` should contain exactly one argument",
                        type_name
                    ))
                } else {
                    validate_non_null_value(document, schema, &inner[0], &alias.target, options)
                }
            } else {
                validate_non_null_value(document, schema, value, &alias.target, options)
            }
        }
        TypeDoc::Enumeration(enum_def) => {
            if let ParameterValue::Typed {
                type_name: inline_name,
                inner,
                ..
            } = value
            {
                if inline_name != type_name {
                    return Some(format!(
                        "expected typed value `{}` but found `{}`",
                        type_name, inline_name
                    ));
                }

                if inner.len() != 1 {
                    Some(format!(
                        "typed value `{}` should contain exactly one argument",
                        type_name
                    ))
                } else {
                    validate_enum_value(&inner[0], &enum_def.items)
                }
            } else {
                validate_enum_value(value, &enum_def.items)
            }
        }
        TypeDoc::Select(select) => validate_select(document, schema, value, select, options),
    }
}

fn validate_select(
    document: &Document,
    schema: &SchemaDoc,
    value: &ParameterValue,
    select: &SelectTypeDef,
    options: &DiagnosticOptions<'_>,
) -> Option<String> {
    if select
        .options
        .iter()
        .any(|option| validate_non_null_value(document, schema, value, option, options).is_none())
    {
        None
    } else {
        Some(format!(
            "value does not match any option of `{}`",
            select.name
        ))
    }
}

fn validate_enum_value(value: &ParameterValue, items: &[String]) -> Option<String> {
    match value {
        ParameterValue::Enumeration { value, .. } => {
            if items.contains(value) {
                None
            } else {
                Some(format!(
                    "expected one of {:?} but found `.{}.`",
                    items, value
                ))
            }
        }
        _ => Some(format!(
            "expected enumeration value but found {}",
            value.kind_name()
        )),
    }
}

fn validate_aggregate(
    document: &Document,
    schema: &SchemaDoc,
    value: &ParameterValue,
    aggregate: &AggregateTypeRef,
    options: &DiagnosticOptions<'_>,
) -> Option<String> {
    let ParameterValue::List { items, .. } = value else {
        return Some(format!(
            "expected {} aggregate but found {}",
            aggregate.kind.as_str(),
            value.kind_name()
        ));
    };

    if let Some(bounds) = &aggregate.bounds {
        if let BoundValue::Integer(lower) = bounds.lower
            && items.len() < lower as usize
        {
            return Some(format!(
                "expected at least {} items but found {}",
                lower,
                items.len()
            ));
        }
        if let BoundValue::Integer(upper) = bounds.upper
            && items.len() > upper as usize
        {
            return Some(format!(
                "expected at most {} items but found {}",
                upper,
                items.len()
            ));
        }
    }

    for item in items {
        if let Some(message) =
            validate_non_null_value(document, schema, item, &aggregate.item, options)
        {
            return Some(message);
        }
    }

    None
}

fn validate_entity_reference(
    document: &Document,
    schema: &SchemaDoc,
    value: &ParameterValue,
    expected_entity: &str,
    options: &DiagnosticOptions<'_>,
) -> Option<String> {
    let ParameterValue::Reference { id, .. } = value else {
        return Some(format!(
            "expected reference to `{}` but found {}",
            expected_entity,
            value.kind_name()
        ));
    };

    let reference_document = options.reference_document.unwrap_or(document);
    let Some(definition) = reference_document.definitions.get(id) else {
        return Some(format!(
            "reference `#{}` does not resolve to a local entity",
            id
        ));
    };

    let entity_name = definition
        .entity_name
        .as_deref()
        .map(str::to_string)
        .or_else(|| lazy_definition_entity_name(reference_document, definition.entity_range));
    let Some(entity_name) = entity_name.as_deref() else {
        return Some(format!(
            "reference `#{}` does not have entity type information",
            id
        ));
    };

    if schema.is_entity_compatible(entity_name, expected_entity) {
        None
    } else {
        Some(format!(
            "expected reference to `{}` but `#{}` points to `{}`",
            expected_entity, id, entity_name
        ))
    }
}

fn lazy_definition_entity_name(document: &Document, range: Range) -> Option<String> {
    let mut node = document.node_at_position(range.start)?;
    while node.kind() != "entity_instance" {
        node = node.parent()?;
    }

    entity_name_child(node, &document.text)
}

fn entity_name_child(node: Node<'_>, text: &str) -> Option<String> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == "entity_name")
        .and_then(|child| child.utf8_text(text.as_bytes()).ok())
        .map(str::to_ascii_uppercase)
}

fn validate_primitive(value: &ParameterValue, primitive: &PrimitiveType) -> Option<String> {
    match primitive {
        PrimitiveType::Integer => match value {
            ParameterValue::Number { text, .. } if is_integer(text) => None,
            ParameterValue::Number { .. } => Some("expected integer number".to_string()),
            _ => Some(format!("expected integer but found {}", value.kind_name())),
        },
        PrimitiveType::Real | PrimitiveType::Number => match value {
            ParameterValue::Number { .. } => None,
            _ => Some(format!(
                "expected numeric value but found {}",
                value.kind_name()
            )),
        },
        PrimitiveType::String { .. } => match value {
            ParameterValue::String { .. } => None,
            _ => Some(format!("expected string but found {}", value.kind_name())),
        },
        PrimitiveType::Logical | PrimitiveType::Boolean => match value {
            ParameterValue::Enumeration { value, .. }
                if matches!(
                    value.as_str(),
                    "TRUE" | "FALSE" | "UNKNOWN" | "T" | "F" | "U"
                ) =>
            {
                None
            }
            _ => Some(format!(
                "expected logical/boolean enumeration but found {}",
                value.kind_name()
            )),
        },
        PrimitiveType::Binary { .. } => match value {
            ParameterValue::String { .. } => None,
            _ => Some(format!(
                "expected binary literal but found {}",
                value.kind_name()
            )),
        },
    }
}

fn is_integer(text: &str) -> bool {
    !text.contains(['.', 'E', 'e'])
}

trait AggregateKindDisplay {
    fn as_str(&self) -> &'static str;
}

impl AggregateKindDisplay for AggregateKind {
    fn as_str(&self) -> &'static str {
        match self {
            AggregateKind::Set => "set",
            AggregateKind::Bag => "bag",
            AggregateKind::List => "list",
            AggregateKind::Array => "array",
        }
    }
}

fn collect_syntax_diagnostics(document: &Document) -> Vec<Diagnostic> {
    let Some(tree) = &document.tree else {
        return Vec::new();
    };

    let mut diagnostics = Vec::new();
    let mut cursor = tree.root_node().walk();
    collect_error_nodes(&mut cursor, &mut diagnostics);
    diagnostics
}

fn collect_error_nodes(
    cursor: &mut tree_sitter::TreeCursor<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    loop {
        let node = cursor.node();

        if node.is_error() {
            diagnostics.push(Diagnostic {
                range: node_range(&node),
                severity: Some(DiagnosticSeverity::ERROR),
                message: "Invalid IFC STEP syntax".to_string(),
                ..Default::default()
            });
        }

        if cursor.goto_first_child() {
            collect_error_nodes(cursor, diagnostics);
            cursor.goto_parent();
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn node_range(node: &tree_sitter::Node<'_>) -> tower_lsp::lsp_types::Range {
    let start = node.start_position();
    let end = node.end_position();

    tower_lsp::lsp_types::Range {
        start: tower_lsp::lsp_types::Position::new(start.row as u32, start.column as u32),
        end: tower_lsp::lsp_types::Position::new(end.row as u32, end.column as u32),
    }
}

//*----- TESTS BEGIN HERE -----*
#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tower_lsp::lsp_types::{Position, Range};
    use tree_sitter::Parser;

    use super::*;
    use crate::document::Document;
    use crate::schema::{IfcVersion, load_express};

    /// Helper function to parse a document and return the result.
    fn parse_document(text: &str) -> Document {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("Error loading IFC parser");
        Document::parse(&mut parser, text.to_string())
    }

    /// Helper function to load a schema from a fixture source string.
    fn schema_from(source: &str) -> SchemaDoc {
        load_express(IfcVersion::Ifc4Add2Tc1, source).expect("fixture schema should parse")
    }

    /// Helper function to create a test schema from a string.
    fn test_schema() -> SchemaDoc {
        schema_from(
            r#"
            SCHEMA IFC4;
              TYPE IfcLabel = STRING(255);
              END_TYPE;
              TYPE IfcWallTypeEnum = ENUMERATION OF (MOVABLE, USERDEFINED);
              END_TYPE;
              ENTITY IfcRoot;
                GlobalId : IfcLabel;
              END_ENTITY;
              ENTITY IfcWall
                SUBTYPE OF (IfcRoot);
                PredefinedType : OPTIONAL IfcWallTypeEnum;
              END_ENTITY;
            END_SCHEMA;
            "#,
        )
    }

    /// Test that the datatype validator reports mismatched attribute types.
    #[test]
    fn datatype_validator_reports_mismatched_attribute_type() {
        let doc = parse_document("#1=IFCWALL(123,.MOVABLE.);");
        let diagnostics = collect_with_schema_name(&doc, &test_schema(), None);

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("GlobalId"));
        assert!(diagnostics[0].message.contains("expected string"));
    }

    /// Test that the datatype validator accepts valid values.
    #[test]
    fn datatype_validator_accepts_valid_values() {
        let doc = parse_document("#1=IFCWALL('gid',.MOVABLE.);");
        let diagnostics = collect_with_schema_name(&doc, &test_schema(), None);

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    /// Test that the datatype validator reports unresolved references.
    #[test]
    fn datatype_validator_reports_unresolved_reference() {
        // `tree` is not necessary for this test, as the validator resolves references without it.
        let document = Document {
            text: "#1=IFCWALL('gid',.MOVABLE.);".to_string(),
            tree: None,
            schema_name: None,
            parse_mode: crate::document::DocumentParseMode::Full,
            definitions: HashMap::new(),
            references: HashMap::new(),
            instances: vec![crate::document::EntityInstanceInfo {
                id: Some(1),
                id_range: Some(Range {
                    start: Position::new(0, 0),
                    end: Position::new(0, 2),
                }),
                entity_name: "IFCWALL".to_string(),
                entity_name_range: Range {
                    start: Position::new(0, 3),
                    end: Position::new(0, 10),
                },
                entity_range: Range {
                    start: Position::new(0, 0),
                    end: Position::new(0, 27),
                },
                parameter_list_range: Some(Range {
                    start: Position::new(0, 10),
                    end: Position::new(0, 27),
                }),
                parameters: vec![
                    ParameterValue::String {
                        range: Range {
                            start: Position::new(0, 11),
                            end: Position::new(0, 16),
                        },
                    },
                    ParameterValue::Reference {
                        id: 2,
                        range: Range {
                            start: Position::new(0, 17),
                            end: Position::new(0, 19),
                        },
                    },
                ],
            }],
            instance_indexes_by_id: HashMap::from([(1, 0)]),
        };

        let schema = schema_from(
            r#"
            SCHEMA IFC4;
              ENTITY IfcRoot;
                GlobalId : STRING;
              END_ENTITY;
              ENTITY IfcWall
                SUBTYPE OF (IfcRoot);
                Parent : IfcWall;
              END_ENTITY;
            END_SCHEMA;
            "#,
        );

        let diagnostics = collect_with_schema_name(&document, &schema, None);

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("does not resolve"));
    }

    /// Test that the datatype validator allows omitted (`*`) values for inherited
    /// attributes that are marked as derived on the concrete entity.
    #[test]
    fn datatype_validator_allows_omitted_for_derived_inherited_attribute() {
        let schema = schema_from(
            r#"
            SCHEMA IFC4;
              TYPE IfcUnitEnum = ENUMERATION OF (LENGTHUNIT);
              END_TYPE;
              TYPE IfcSIPrefix = ENUMERATION OF (MILLI);
              END_TYPE;
              TYPE IfcSIUnitName = ENUMERATION OF (METRE);
              END_TYPE;
              ENTITY IfcDimensionalExponents;
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
            END_SCHEMA;
            "#,
        );

        let doc = parse_document("#15=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);");
        let diagnostics = collect_with_schema_name(&doc, &schema, None);

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    /// Test that the datatype validator reports invalid STEP syntax.
    #[test]
    fn datatype_validator_reports_invalid_step_syntax() {
        // string literals in STEP use single quotes instead of double quotes.
        let doc = parse_document(r#"#14=IFCUNITASSIGNMENT((#15,#16,#17, "test"));"#);
        let diagnostics = collect_with_schema_name(&doc, &test_schema(), None);

        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("Invalid IFC STEP syntax")),
            "{diagnostics:?}"
        );
    }

    /// Test that the datatype validator accepts inline typed values for selects.
    #[test]
    fn datatype_validator_accepts_inline_typed_values_for_selects() {
        let schema = schema_from(
            r#"
            SCHEMA IFC4;
              TYPE IfcLabel = STRING(255);
              END_TYPE;
              TYPE IfcLengthMeasure = REAL;
              END_TYPE;
              TYPE IfcValue = SELECT (IfcLabel, IfcLengthMeasure);
              END_TYPE;
              ENTITY IfcRoot;
                Name : IfcLabel;
              END_ENTITY;
              ENTITY IfcPropertySingleValue
                SUBTYPE OF (IfcRoot);
                Description : OPTIONAL IfcLabel;
                NominalValue : OPTIONAL IfcValue;
              END_ENTITY;
            END_SCHEMA;
            "#,
        );

        let doc = parse_document(
            "#1=IFCPROPERTYSINGLEVALUE('Name',$,IFCLABEL('Living Room'));\n#2=IFCPROPERTYSINGLEVALUE('Offset',$,IFCLENGTHMEASURE(2.6));",
        );
        let diagnostics = collect_with_schema_name(&doc, &schema, None);

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }
}
