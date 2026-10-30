//! Datatype validation diagnostics for IFC entity attributes.
//! This module compares parsed entity-instance arguments against the runtime schema docs and
//! reports arity, reference, primitive, aggregate, enumeration, and select-type mismatches.
//! General EXPRESS `WHERE` rules are intentionally out of scope for the current implementation.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};

use crate::document::{
    EntityInstanceInfo, EntityTypeIndex, ParameterValue, build_entity_type_index,
    for_each_entity_instance,
};
use crate::schema::{
    AggregateKind, AggregateTypeRef, BoundValue, EntityAttributeDoc, EntityDoc, NamedTypeKind,
    PrimitiveType, SchemaDoc, SelectTypeDef, TypeDoc, TypeRef,
};

pub fn collect_with_schema_name(
    tree: Option<&tree_sitter::Tree>,
    text: &str,
    schema: &SchemaDoc,
    schema_name: Option<&str>,
) -> Vec<Diagnostic> {
    let entity_types = build_entity_type_index(tree, text);
    let mut diagnostics = Vec::new();

    for_each_entity_instance(
        tree,
        text,
        |_| Some(usize::MAX),
        |instance| {
            if let Some(entity) = schema.entity(&instance.entity_name) {
                validate_instance(&entity_types, schema, entity, &instance, &mut diagnostics);
            } else {
                diagnostics.push(unknown_entity_diagnostic(&instance, schema_name));
            }
        },
    );

    diagnostics
}

fn validate_instance(
    entity_types: &EntityTypeIndex,
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
        if let Some(message) = validate_value(entity_types, schema, value, attribute) {
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
    entity_types: &EntityTypeIndex,
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
        _ => validate_non_null_value(entity_types, schema, value, &attribute.ty),
    }
}

fn validate_non_null_value(
    entity_types: &EntityTypeIndex,
    schema: &SchemaDoc,
    value: &ParameterValue,
    expected: &TypeRef,
) -> Option<String> {
    match expected {
        TypeRef::Primitive(primitive) => validate_primitive(value, primitive),
        TypeRef::Aggregate(aggregate) => validate_aggregate(entity_types, schema, value, aggregate),
        TypeRef::GenericEntity { .. } | TypeRef::Generic { .. } => None,
        TypeRef::Named(named) => match named.kind {
            NamedTypeKind::Entity => {
                validate_entity_reference(entity_types, schema, value, &named.name)
            }
            NamedTypeKind::Type | NamedTypeKind::Unresolved => {
                validate_named_type(entity_types, schema, value, &named.name)
            }
        },
    }
}

fn validate_named_type(
    entity_types: &EntityTypeIndex,
    schema: &SchemaDoc,
    value: &ParameterValue,
    type_name: &str,
) -> Option<String> {
    let type_def = schema.type_decl(type_name)?;
    match type_def {
        TypeDoc::Alias(alias) => match unwrap_inline_typed_value(value, type_name) {
            Ok(value) => validate_non_null_value(entity_types, schema, value, &alias.target),
            Err(message) => Some(message),
        },
        TypeDoc::Enumeration(enum_def) => match unwrap_inline_typed_value(value, type_name) {
            Ok(value) => validate_enum_value(value, &enum_def.items),
            Err(message) => Some(message),
        },
        TypeDoc::Select(select) => validate_select(entity_types, schema, value, select),
    }
}

/// Reduces an inline typed value such as `IFCLABEL('Name')` to the single payload that has to be
/// checked against `type_name`'s underlying definition. Values that are not inline typed pass
/// through unchanged; a wrong inline name or argument count yields the diagnostic message.
fn unwrap_inline_typed_value<'a>(
    value: &'a ParameterValue,
    type_name: &str,
) -> Result<&'a ParameterValue, String> {
    let ParameterValue::Typed {
        type_name: inline_name,
        inner,
        ..
    } = value
    else {
        return Ok(value);
    };

    if inline_name != type_name {
        return Err(format!(
            "expected typed value `{}` but found `{}`",
            type_name, inline_name
        ));
    }

    match inner.as_slice() {
        [single] => Ok(single),
        _ => Err(format!(
            "typed value `{}` should contain exactly one argument",
            type_name
        )),
    }
}

fn validate_select(
    entity_types: &EntityTypeIndex,
    schema: &SchemaDoc,
    value: &ParameterValue,
    select: &SelectTypeDef,
) -> Option<String> {
    if select
        .options
        .iter()
        .any(|option| validate_non_null_value(entity_types, schema, value, option).is_none())
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
    entity_types: &EntityTypeIndex,
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
        if let Some(message) = validate_non_null_value(entity_types, schema, item, &aggregate.item)
        {
            return Some(message);
        }
    }

    None
}

fn validate_entity_reference(
    entity_types: &EntityTypeIndex,
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

    let Some(entity_name) = entity_types.entity_name_by_id(*id) else {
        return Some(format!(
            "reference `#{}` does not resolve to a local entity",
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
    use tower_lsp::lsp_types::Position;
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

    fn diagnostics_for(text: &str, schema: &SchemaDoc) -> Vec<Diagnostic> {
        let doc = parse_document(text);
        collect_with_schema_name(doc.tree.as_ref(), &doc.text, schema, None)
    }

    fn messages(diagnostics: &[Diagnostic]) -> Vec<&str> {
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect()
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
        let diagnostics = diagnostics_for("#1=IFCWALL(123,.MOVABLE.);", &test_schema());

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("GlobalId"));
        assert!(diagnostics[0].message.contains("expected string"));
    }

    /// Test that the datatype validator accepts valid values.
    #[test]
    fn datatype_validator_accepts_valid_values() {
        let diagnostics = diagnostics_for("#1=IFCWALL('gid',.MOVABLE.);", &test_schema());

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    /// Test that the datatype validator reports unresolved references.
    #[test]
    fn datatype_validator_reports_unresolved_reference() {
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

        let diagnostics = diagnostics_for("#1=IFCWALL('gid',#2);", &schema);

        assert_eq!(
            messages(&diagnostics),
            ["Parent: reference `#2` does not resolve to a local entity"]
        );
    }

    /// Test that references resolve against every entity in the file: forward references,
    /// subtypes, missing targets, and the last definition of a duplicated id.
    #[test]
    fn datatype_validator_resolves_references_across_the_file() {
        let schema = schema_from(
            r#"
            SCHEMA IFC4;
              ENTITY IfcTarget; END_ENTITY;
              ENTITY IfcChild SUBTYPE OF (IfcTarget); END_ENTITY;
              ENTITY IfcOther; END_ENTITY;
              ENTITY IfcHolder;
                A : LIST [1:?] OF IfcTarget;
                B : LIST [1:?] OF IfcTarget;
                C : IfcTarget;
                D : IfcTarget;
              END_ENTITY;
            END_SCHEMA;
            "#,
        );
        let diagnostics = diagnostics_for(
            "#1=IFCHOLDER((#2,#3),(#9),#4,#5);\n#2=IFCCHILD();\n#3=IFCOTHER();\n#4=IFCTARGET();\n#4=IFCOTHER();\n#5=IFCCHILD();",
            &schema,
        );

        assert_eq!(
            messages(&diagnostics),
            [
                "A: expected reference to `IFCTARGET` but `#3` points to `IFCOTHER`",
                "B: reference `#9` does not resolve to a local entity",
                "C: expected reference to `IFCTARGET` but `#4` points to `IFCOTHER`",
            ]
        );
    }

    /// Test that diagnostics follow source order across entities inside a full STEP file,
    /// including unknown entities and arity mismatches.
    #[test]
    fn datatype_validator_reports_in_source_order_within_data_section() {
        let text = "ISO-10303-21;\nHEADER;\nFILE_SCHEMA(('IFC4'));\nENDSEC;\nDATA;\n#1=IFCWALL(123,.MOVABLE.);\n#2=IFCDOOR('gid');\n#3=IFCWALL('gid');\n#4=IFCWALL('gid',.BROKEN.);\nENDSEC;\nEND-ISO-10303-21;";
        let doc = parse_document(text);
        let diagnostics =
            collect_with_schema_name(doc.tree.as_ref(), &doc.text, &test_schema(), Some("IFC4"));

        assert_eq!(
            messages(&diagnostics),
            [
                "GlobalId: expected string but found number",
                "Unknown IFC entity `IFCDOOR` in selected schema `IFC4`",
                "IfcWall expects 2 attributes but found 1",
                "PredefinedType: expected one of [\"MOVABLE\", \"USERDEFINED\"] but found `.BROKEN.`",
            ]
        );
        assert_eq!(diagnostics[0].range.start, Position::new(5, 11));
        assert_eq!(diagnostics[1].range.start, Position::new(6, 3));
    }

    /// Test that entities error recovery groups under an `ERROR` node are still validated.
    #[test]
    fn datatype_validator_reports_entities_under_error_recovery() {
        let schema = schema_from(
            r#"
            SCHEMA IFC4;
              ENTITY IfcWall; END_ENTITY;
            END_SCHEMA;
            "#,
        );
        let diagnostics = diagnostics_for(
            concat!(
                "DATA;\n",
                "#1=IFCCARTESIANPOINT((0.,1.,2.));\n",
                "#2=IFCPOLYLINE((#1,#4))\n",
                "#3=IFCDIRECTION((1.,0.,0.));\n",
                "#4=IFCWALL('a',(#2,#3);\n",
                "#5=IFCDIRECTION((1.,0.,0.));\n",
                "#6=IFCWALL('b',$) stray;\n",
                "#7=IFCCARTESIANPOINT((3.,4.,5.));\n",
                "#8=IFCWALL(g',$,.T.,IFCLABEL('x'),(#1,#2),*);\n",
                "#9=IFCDIRECTION((1.,0.,0.));\n",
                "ENDSEC;\n",
            ),
            &schema,
        );

        assert_eq!(
            messages(&diagnostics),
            [
                "Unknown IFC entity `IFCCARTESIANPOINT` in the selected schema",
                "Unknown IFC entity `IFCPOLYLINE` in the selected schema",
                "Unknown IFC entity `IFCDIRECTION` in the selected schema",
                "IfcWall expects 0 attributes but found 2",
                "Unknown IFC entity `IFCDIRECTION` in the selected schema",
                "IfcWall expects 0 attributes but found 2",
                "Unknown IFC entity `IFCCARTESIANPOINT` in the selected schema",
            ]
        );
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

        let diagnostics =
            diagnostics_for("#15=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);", &schema);

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

        let diagnostics = diagnostics_for(
            "#1=IFCPROPERTYSINGLEVALUE('Name',$,IFCLABEL('Living Room'));\n#2=IFCPROPERTYSINGLEVALUE('Offset',$,IFCLENGTHMEASURE(2.6));",
            &schema,
        );

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }
}
