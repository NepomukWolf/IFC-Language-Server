//! Datatype validation diagnostics for IFC entity attributes.
//! This module compares parsed entity-instance arguments against the runtime schema docs and
//! reports arity, reference, primitive, aggregate, enumeration, and select-type mismatches.
//! General EXPRESS `WHERE` rules are intentionally out of scope for the current implementation.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};

use crate::diagnostics::DiagnosticSnapshot;
use crate::document::{EntityInstanceInfo, ParameterValue};
use crate::schema::{
    AggregateKind, AggregateTypeRef, BoundValue, EntityAttributeDoc, EntityDoc, NamedTypeKind,
    PrimitiveType, SchemaDoc, SelectTypeDef, TypeDoc, TypeRef,
};

pub fn collect_with_schema_name(
    snapshot: &DiagnosticSnapshot,
    schema: &SchemaDoc,
    schema_name: Option<&str>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for instance in &snapshot.instances {
        let Some(entity) = schema.entity(&instance.entity_name) else {
            diagnostics.push(unknown_entity_diagnostic(instance, schema_name));
            continue;
        };

        validate_instance(snapshot, schema, entity, instance, &mut diagnostics);
    }

    diagnostics
}

fn validate_instance(
    snapshot: &DiagnosticSnapshot,
    schema: &SchemaDoc,
    entity: &EntityDoc,
    instance: &EntityInstanceInfo,
    diagnostics: &mut Vec<Diagnostic>,
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
        if let Some(message) = validate_value(snapshot, schema, value, attribute) {
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
    snapshot: &DiagnosticSnapshot,
    schema: &SchemaDoc,
    value: &ParameterValue,
    attribute: &EntityAttributeDoc,
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
        _ => validate_non_null_value(snapshot, schema, value, &attribute.ty),
    }
}

fn validate_non_null_value(
    snapshot: &DiagnosticSnapshot,
    schema: &SchemaDoc,
    value: &ParameterValue,
    expected: &TypeRef,
) -> Option<String> {
    match expected {
        TypeRef::Primitive(primitive) => validate_primitive(value, primitive),
        TypeRef::Aggregate(aggregate) => validate_aggregate(snapshot, schema, value, aggregate),
        TypeRef::GenericEntity { .. } | TypeRef::Generic { .. } => None,
        TypeRef::Named(named) => match named.kind {
            NamedTypeKind::Entity => {
                validate_entity_reference(snapshot, schema, value, &named.name)
            }
            NamedTypeKind::Type | NamedTypeKind::Unresolved => {
                validate_named_type(snapshot, schema, value, &named.name)
            }
        },
    }
}

fn validate_named_type(
    snapshot: &DiagnosticSnapshot,
    schema: &SchemaDoc,
    value: &ParameterValue,
    type_name: &str,
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
                    validate_non_null_value(snapshot, schema, &inner[0], &alias.target)
                }
            } else {
                validate_non_null_value(snapshot, schema, value, &alias.target)
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
        TypeDoc::Select(select) => validate_select(snapshot, schema, value, select),
    }
}

fn validate_select(
    snapshot: &DiagnosticSnapshot,
    schema: &SchemaDoc,
    value: &ParameterValue,
    select: &SelectTypeDef,
) -> Option<String> {
    if select
        .options
        .iter()
        .any(|option| validate_non_null_value(snapshot, schema, value, option).is_none())
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
    snapshot: &DiagnosticSnapshot,
    schema: &SchemaDoc,
    value: &ParameterValue,
    aggregate: &AggregateTypeRef,
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
        if let Some(message) = validate_non_null_value(snapshot, schema, item, &aggregate.item) {
            return Some(message);
        }
    }

    None
}

fn validate_entity_reference(
    snapshot: &DiagnosticSnapshot,
    schema: &SchemaDoc,
    value: &ParameterValue,
    expected_entity: &str,
) -> Option<String> {
    let ParameterValue::Reference { id, .. } = value else {
        return Some(format!(
            "expected reference to `{}` but found {}",
            expected_entity,
            value.kind_name()
        ));
    };

    let Some(instance) = snapshot.instance_by_id(*id) else {
        return Some(format!(
            "reference `#{}` does not resolve to a local entity",
            id
        ));
    };

    if schema.is_entity_compatible(&instance.entity_name, expected_entity) {
        None
    } else {
        Some(format!(
            "expected reference to `{}` but `#{}` points to `{}`",
            expected_entity, id, instance.entity_name
        ))
    }
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
        let snapshot = DiagnosticSnapshot::from_document(&doc);
        let diagnostics = collect_with_schema_name(&snapshot, &test_schema(), None);

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("GlobalId"));
        assert!(diagnostics[0].message.contains("expected string"));
    }

    /// Test that the datatype validator accepts valid values.
    #[test]
    fn datatype_validator_accepts_valid_values() {
        let doc = parse_document("#1=IFCWALL('gid',.MOVABLE.);");
        let snapshot = DiagnosticSnapshot::from_document(&doc);
        let diagnostics = collect_with_schema_name(&snapshot, &test_schema(), None);

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    /// Test that the datatype validator reports unresolved references.
    #[test]
    fn datatype_validator_reports_unresolved_reference() {
        let mut document = Document::new_unloaded("#1=IFCWALL('gid',.MOVABLE.);".to_string());
        document.instances = vec![crate::document::EntityInstanceInfo {
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
        }];
        document.instance_indexes_by_id = HashMap::from([(1, 0)]);

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

        let snapshot = DiagnosticSnapshot::from_document(&document);
        let diagnostics = collect_with_schema_name(&snapshot, &schema, None);

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
        let snapshot = DiagnosticSnapshot::from_document(&doc);
        let diagnostics = collect_with_schema_name(&snapshot, &schema, None);

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
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
        let snapshot = DiagnosticSnapshot::from_document(&doc);
        let diagnostics = collect_with_schema_name(&snapshot, &schema, None);

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }
}
