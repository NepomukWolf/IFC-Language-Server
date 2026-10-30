//! Datatype validation diagnostics for IFC entity attributes.
//! This module compares parsed entity-instance arguments against the runtime schema docs and
//! reports arity, reference, primitive, aggregate, enumeration, and select-type mismatches.
//! General EXPRESS `WHERE` rules are intentionally out of scope for the current implementation.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};

use crate::document::{EntityInstanceCollection, EntityInstanceInfo, ParameterValue};
use crate::schema::{
    AggregateKind, AggregateTypeRef, BoundValue, EntityAttributeDoc, EntityDoc, NamedTypeKind,
    PrimitiveType, SchemaDoc, SelectTypeDef, TypeDoc, TypeRef,
};

pub fn collect_with_schema_name(
    instances: &EntityInstanceCollection,
    schema: &SchemaDoc,
    schema_name: Option<&str>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for instance in &instances.instances {
        let Some(entity) = schema.entity(&instance.entity_name) else {
            diagnostics.push(unknown_entity_diagnostic(instance, schema_name));
            continue;
        };

        validate_instance(instances, schema, entity, instance, &mut diagnostics);
    }

    diagnostics
}

fn validate_instance(
    instances: &EntityInstanceCollection,
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
        if let Some(message) = validate_value(instances, schema, value, attribute) {
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
    instances: &EntityInstanceCollection,
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
        _ => validate_non_null_value(instances, schema, value, &attribute.ty),
    }
}

fn validate_non_null_value(
    instances: &EntityInstanceCollection,
    schema: &SchemaDoc,
    value: &ParameterValue,
    expected: &TypeRef,
) -> Option<String> {
    match expected {
        TypeRef::Primitive(primitive) => validate_primitive(value, primitive),
        TypeRef::Aggregate(aggregate) => validate_aggregate(instances, schema, value, aggregate),
        TypeRef::GenericEntity { .. } | TypeRef::Generic { .. } => None,
        TypeRef::Named(named) => match named.kind {
            NamedTypeKind::Entity => {
                validate_entity_reference(instances, schema, value, &named.name)
            }
            NamedTypeKind::Type | NamedTypeKind::Unresolved => {
                validate_named_type(instances, schema, value, &named.name)
            }
        },
    }
}

fn validate_named_type(
    instances: &EntityInstanceCollection,
    schema: &SchemaDoc,
    value: &ParameterValue,
    type_name: &str,
) -> Option<String> {
    let type_def = schema.type_decl(type_name)?;
    match type_def {
        TypeDoc::Alias(alias) => match unwrap_inline_typed_value(value, type_name) {
            Ok(value) => validate_non_null_value(instances, schema, value, &alias.target),
            Err(message) => Some(message),
        },
        TypeDoc::Enumeration(enum_def) => match unwrap_inline_typed_value(value, type_name) {
            Ok(value) => validate_enum_value(value, &enum_def.items),
            Err(message) => Some(message),
        },
        TypeDoc::Select(select) => validate_select(instances, schema, value, select),
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
    instances: &EntityInstanceCollection,
    schema: &SchemaDoc,
    value: &ParameterValue,
    select: &SelectTypeDef,
) -> Option<String> {
    if select
        .options
        .iter()
        .any(|option| validate_non_null_value(instances, schema, value, option).is_none())
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
    instances: &EntityInstanceCollection,
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
        if let Some(message) = validate_non_null_value(instances, schema, item, &aggregate.item) {
            return Some(message);
        }
    }

    None
}

fn validate_entity_reference(
    instances: &EntityInstanceCollection,
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

    let Some(instance) = instances.instance_by_id(*id) else {
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

    fn instances(document: &Document) -> EntityInstanceCollection {
        crate::document::build_entity_instances(document.tree.as_ref(), &document.text)
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
        let instances = instances(&doc);
        let diagnostics = collect_with_schema_name(&instances, &test_schema(), None);

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("GlobalId"));
        assert!(diagnostics[0].message.contains("expected string"));
    }

    /// Test that the datatype validator accepts valid values.
    #[test]
    fn datatype_validator_accepts_valid_values() {
        let doc = parse_document("#1=IFCWALL('gid',.MOVABLE.);");
        let instances = instances(&doc);
        let diagnostics = collect_with_schema_name(&instances, &test_schema(), None);

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    /// Test that the datatype validator reports unresolved references.
    #[test]
    fn datatype_validator_reports_unresolved_reference() {
        let instances = EntityInstanceCollection::new(vec![crate::document::EntityInstanceInfo {
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
        }]);

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

        let diagnostics = collect_with_schema_name(&instances, &schema, None);

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
        let instances = instances(&doc);
        let diagnostics = collect_with_schema_name(&instances, &schema, None);

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
        let instances = instances(&doc);
        let diagnostics = collect_with_schema_name(&instances, &schema, None);

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    fn select_schema() -> SchemaDoc {
        schema_from(
            r#"
            SCHEMA IFC4;
              TYPE IfcLabel = STRING(255);
              END_TYPE;
              TYPE IfcIdentifier = IfcLabel;
              END_TYPE;
              TYPE IfcLengthMeasure = REAL;
              END_TYPE;
              TYPE IfcCountMeasure = INTEGER;
              END_TYPE;
              TYPE IfcBoolean = BOOLEAN;
              END_TYPE;
              TYPE IfcWallTypeEnum = ENUMERATION OF (MOVABLE, USERDEFINED);
              END_TYPE;
              TYPE IfcMeasureValue = SELECT (IfcLengthMeasure, IfcCountMeasure);
              END_TYPE;
              TYPE IfcSimpleValue = SELECT (IfcLabel, IfcIdentifier, IfcBoolean);
              END_TYPE;
              TYPE IfcValue = SELECT (IfcMeasureValue, IfcSimpleValue);
              END_TYPE;
              TYPE IfcWallSelect = SELECT (IfcWall, IfcWallTypeEnum);
              END_TYPE;
              TYPE IfcLooseSelect = SELECT (IfcUndeclared, IfcLabel);
              END_TYPE;
              ENTITY IfcRoot;
              END_ENTITY;
              ENTITY IfcWall
                SUBTYPE OF (IfcRoot);
              END_ENTITY;
              ENTITY IfcProperty;
                NominalValue : OPTIONAL IfcValue;
                Wall : OPTIONAL IfcWallSelect;
                ListValues : OPTIONAL LIST [1:?] OF IfcValue;
                Loose : OPTIONAL IfcLooseSelect;
              END_ENTITY;
              ENTITY IfcMisc;
                Count : INTEGER;
                Ratio : REAL;
                Name : IfcLabel;
                Flag : LOGICAL;
                Data : BINARY;
                Kind : IfcWallTypeEnum;
                Parent : IfcRoot;
                Pair : LIST [1:2] OF IfcLabel;
              END_ENTITY;
            END_SCHEMA;
            "#,
        )
    }

    fn rendered_diagnostics(text: &str) -> Vec<String> {
        let doc = parse_document(text);
        let instances = instances(&doc);
        collect_with_schema_name(&instances, &select_schema(), Some("IFC4"))
            .iter()
            .map(|diagnostic| {
                let Range { start, end } = diagnostic.range;
                format!(
                    "{}:{}-{}:{} {:?} {:?} {}",
                    start.line,
                    start.character,
                    end.line,
                    end.character,
                    diagnostic.severity,
                    diagnostic.code,
                    diagnostic.message
                )
            })
            .collect()
    }

    /// Test that select values matching an option, directly or through nested selects, pass.
    #[test]
    fn datatype_validator_accepts_matching_select_values() {
        let diagnostics = rendered_diagnostics(
            "#1=IFCWALL();
#2=IFCPROPERTY(IFCLABEL('x'),$,$,$);
#3=IFCPROPERTY(IFCLENGTHMEASURE(2.5),$,$,$);
#4=IFCPROPERTY(IFCCOUNTMEASURE(3),$,$,$);
#5=IFCPROPERTY('untyped',$,$,$);
#6=IFCPROPERTY(1.5,$,$,$);
#7=IFCPROPERTY(IFCIDENTIFIER('a'),$,$,$);
#8=IFCPROPERTY(IFCBOOLEAN(.T.),$,$,$);
#9=IFCPROPERTY(.T.,#1,$,$);
#10=IFCPROPERTY($,.MOVABLE.,$,$);
#11=IFCPROPERTY($,IFCWALLTYPEENUM(.USERDEFINED.),$,$);
#12=IFCPROPERTY($,$,(IFCLABEL('a'),IFCCOUNTMEASURE(1),'b'),$);
#13=IFCPROPERTY($,$,$,IFCWALLTYPEENUM(.MOVABLE.));
#14=IFCPROPERTY($,$,$,#1);",
        );

        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    }

    /// Test the exact diagnostics reported for select values that match no option.
    #[test]
    fn datatype_validator_reports_select_mismatches() {
        let diagnostics = rendered_diagnostics(
            "#1=IFCWALL();
#2=IFCPROPERTY(IFCCOUNTMEASURE(2.5),$,$,$);
#3=IFCPROPERTY(IFCWALLTYPEENUM(.MOVABLE.),$,$,$);
#4=IFCPROPERTY(#1,$,$,$);
#5=IFCPROPERTY(IFCLABEL('a','b'),$,$,$);
#6=IFCPROPERTY(IFCLABEL(1.0),$,$,$);
#7=IFCPROPERTY(.MOVABLE.,$,$,$);
#8=IFCPROPERTY(('a'),$,$,$);
#9=IFCPROPERTY($,#99,$,$);
#10=IFCPROPERTY($,.NOPE.,$,$);
#11=IFCPROPERTY($,IFCLABEL('x'),$,$);
#12=IFCPROPERTY($,$,(IFCLABEL('a'),IFCREAL(1.)),$);
#13=IFCPROPERTY($,$,IFCLABEL('a'),$);
#14=IFCPROPERTY($,$,(),$);
#15=IFCPROPERTY(IFCUNKNOWNTYPE('x'),$,$,$);",
        );

        assert_eq!(
            diagnostics,
            [
                "1:15-1:35 Some(Error) None NominalValue: value does not match any option of `IfcValue`",
                "2:15-2:41 Some(Error) None NominalValue: value does not match any option of `IfcValue`",
                "3:15-3:17 Some(Error) None NominalValue: value does not match any option of `IfcValue`",
                "4:15-4:32 Some(Error) None NominalValue: value does not match any option of `IfcValue`",
                "5:15-5:28 Some(Error) None NominalValue: value does not match any option of `IfcValue`",
                "6:15-6:24 Some(Error) None NominalValue: value does not match any option of `IfcValue`",
                "7:15-7:20 Some(Error) None NominalValue: value does not match any option of `IfcValue`",
                "8:17-8:20 Some(Error) None Wall: value does not match any option of `IfcWallSelect`",
                "9:18-9:24 Some(Error) None Wall: value does not match any option of `IfcWallSelect`",
                "10:18-10:31 Some(Error) None Wall: value does not match any option of `IfcWallSelect`",
                "11:20-11:47 Some(Error) None ListValues: value does not match any option of `IfcValue`",
                "12:20-12:33 Some(Error) None ListValues: expected list aggregate but found typed value",
                "13:20-13:22 Some(Error) None ListValues: expected at least 1 items but found 0",
                "14:16-14:35 Some(Error) None NominalValue: value does not match any option of `IfcValue`",
            ]
        );
    }

    /// Test the exact diagnostics reported for non-select attribute mismatches.
    #[test]
    fn datatype_validator_reports_attribute_mismatches() {
        let diagnostics = rendered_diagnostics(
            "#1=IFCWALL();
#2=IFCMISC(1.5,'r',2,.X.,3,.NOPE.,#99,('a','b','c'));
#3=IFCMISC('1',#1,IFCIDENTIFIER('a'),'t',.T.,'e',#2,'p');
#4=IFCMISC($,*,IFCLABEL('a','b'),.T.,'d',IFCWALLTYPEENUM(.MOVABLE.),#1,());
#5=IFCMISC(1,2.0);
#6=IFCWALLX();",
        );

        assert_eq!(
            diagnostics,
            [
                "1:11-1:14 Some(Error) None Count: expected integer number",
                "1:15-1:18 Some(Error) None Ratio: expected numeric value but found string",
                "1:19-1:20 Some(Error) None Name: expected string but found number",
                "1:21-1:24 Some(Error) None Flag: expected logical/boolean enumeration but found enumeration",
                "1:25-1:26 Some(Error) None Data: expected binary literal but found number",
                "1:27-1:33 Some(Error) None Kind: expected one of [\"MOVABLE\", \"USERDEFINED\"] but found `.NOPE.`",
                "1:34-1:37 Some(Error) None Parent: reference `#99` does not resolve to a local entity",
                "1:38-1:51 Some(Error) None Pair: expected at most 2 items but found 3",
                "2:11-2:14 Some(Error) None Count: expected integer but found string",
                "2:15-2:17 Some(Error) None Ratio: expected numeric value but found reference",
                "2:18-2:36 Some(Error) None Name: expected typed value `IFCLABEL` but found `IFCIDENTIFIER`",
                "2:37-2:40 Some(Error) None Flag: expected logical/boolean enumeration but found string",
                "2:41-2:44 Some(Error) None Data: expected binary literal but found enumeration",
                "2:45-2:48 Some(Error) None Kind: expected enumeration value but found string",
                "2:49-2:51 Some(Error) None Parent: expected reference to `IFCROOT` but `#2` points to `IFCMISC`",
                "2:52-2:55 Some(Error) None Pair: expected list aggregate but found string",
                "3:11-3:12 Some(Error) None Count: attribute is required and does not allow `$`",
                "3:13-3:14 Some(Error) None Ratio: `*` is not supported for this attribute",
                "3:15-3:32 Some(Error) None Name: typed value `IFCLABEL` should contain exactly one argument",
                "3:71-3:73 Some(Error) None Pair: expected at least 1 items but found 0",
                "4:10-4:17 Some(Error) None IfcMisc expects 8 attributes but found 2",
                "5:3-5:11 Some(Error) None Unknown IFC entity `IFCWALLX` in selected schema `IFC4`",
            ]
        );
    }
}
