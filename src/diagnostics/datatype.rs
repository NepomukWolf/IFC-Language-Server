//! Datatype validation diagnostics for IFC entity attributes.
//! This module compares parsed entity-instance arguments against the runtime schema docs and
//! reports arity, reference, primitive, aggregate, enumeration, and select-type mismatches.
//! General EXPRESS `WHERE` rules are intentionally out of scope for the current implementation.

use std::fmt;

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};

use crate::document::{EntityInstanceInfo, EntityTypeIndex, ParameterValue};
use crate::schema::{
    AggregateKind, AggregateTypeRef, BoundValue, EntityAttributeDoc, EntityDoc, NamedTypeKind,
    PrimitiveType, SchemaDoc, SelectTypeDef, TypeDoc, TypeRef,
};

#[tracing::instrument(skip_all)]
pub fn collect_lazy(
    lazy: &crate::handwritten::Document,
    text: &str,
    entity_types: &EntityTypeIndex,
    schema: &SchemaDoc,
    schema_name: Option<&str>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    crate::document::for_each_lazy_entity_instance(lazy, text, |instance| {
        validate(
            entity_types,
            schema,
            schema_name,
            &instance,
            &mut diagnostics,
        );
    });

    diagnostics
}

pub(super) fn validate(
    entity_types: &EntityTypeIndex,
    schema: &SchemaDoc,
    schema_name: Option<&str>,
    instance: &EntityInstanceInfo,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let Some(entity) = schema.entity(&instance.entity_name) {
        validate_instance(entity_types, schema, entity, instance, diagnostics);
    } else {
        diagnostics.push(unknown_entity_diagnostic(instance, schema_name));
    }
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
        if let Err(mismatch) = validate_value(entity_types, schema, value, attribute) {
            diagnostics.push(Diagnostic {
                range: value.range(),
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!("{}: {}", attribute.name, mismatch),
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

enum Mismatch<'a> {
    RequiredNull,
    OmittedUnsupported,
    TypedValueName {
        expected: &'a str,
        found: &'a str,
    },
    TypedValueArity {
        type_name: &'a str,
    },
    NoSelectOption {
        select: &'a str,
    },
    EnumerationItem {
        items: &'a [String],
        found: &'a str,
    },
    NotEnumeration {
        found: &'static str,
    },
    NotAggregate {
        kind: AggregateKind,
        found: &'static str,
    },
    TooFewItems {
        lower: u32,
        found: usize,
    },
    TooManyItems {
        upper: u32,
        found: usize,
    },
    NotReference {
        expected: &'a str,
        found: &'static str,
    },
    UnresolvedReference {
        id: u32,
    },
    IncompatibleReference {
        expected: &'a str,
        id: u32,
        found: &'a str,
    },
    NotIntegerNumber,
    NotInteger {
        found: &'static str,
    },
    NotNumber {
        found: &'static str,
    },
    NotString {
        found: &'static str,
    },
    NotLogical {
        found: &'static str,
    },
    NotBinary {
        found: &'static str,
    },
}

impl fmt::Display for Mismatch<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mismatch::RequiredNull => write!(f, "attribute is required and does not allow `$`"),
            Mismatch::OmittedUnsupported => write!(f, "`*` is not supported for this attribute"),
            Mismatch::TypedValueName { expected, found } => write!(
                f,
                "expected typed value `{}` but found `{}`",
                expected, found
            ),
            Mismatch::TypedValueArity { type_name } => write!(
                f,
                "typed value `{}` should contain exactly one argument",
                type_name
            ),
            Mismatch::NoSelectOption { select } => {
                write!(f, "value does not match any option of `{}`", select)
            }
            Mismatch::EnumerationItem { items, found } => {
                write!(f, "expected one of {:?} but found `.{}.`", items, found)
            }
            Mismatch::NotEnumeration { found } => {
                write!(f, "expected enumeration value but found {}", found)
            }
            Mismatch::NotAggregate { kind, found } => write!(
                f,
                "expected {} aggregate but found {}",
                kind.as_str(),
                found
            ),
            Mismatch::TooFewItems { lower, found } => {
                write!(f, "expected at least {} items but found {}", lower, found)
            }
            Mismatch::TooManyItems { upper, found } => {
                write!(f, "expected at most {} items but found {}", upper, found)
            }
            Mismatch::NotReference { expected, found } => write!(
                f,
                "expected reference to `{}` but found {}",
                expected, found
            ),
            Mismatch::UnresolvedReference { id } => {
                write!(f, "reference `#{}` does not resolve to a local entity", id)
            }
            Mismatch::IncompatibleReference {
                expected,
                id,
                found,
            } => write!(
                f,
                "expected reference to `{}` but `#{}` points to `{}`",
                expected, id, found
            ),
            Mismatch::NotIntegerNumber => write!(f, "expected integer number"),
            Mismatch::NotInteger { found } => write!(f, "expected integer but found {}", found),
            Mismatch::NotNumber { found } => {
                write!(f, "expected numeric value but found {}", found)
            }
            Mismatch::NotString { found } => write!(f, "expected string but found {}", found),
            Mismatch::NotLogical { found } => write!(
                f,
                "expected logical/boolean enumeration but found {}",
                found
            ),
            Mismatch::NotBinary { found } => {
                write!(f, "expected binary literal but found {}", found)
            }
        }
    }
}

type Validation<'a> = Result<(), Mismatch<'a>>;

fn validate_value<'a>(
    entity_types: &'a EntityTypeIndex,
    schema: &'a SchemaDoc,
    value: &'a ParameterValue,
    attribute: &'a EntityAttributeDoc,
) -> Validation<'a> {
    match value {
        ParameterValue::Null { .. } if !attribute.optional => Err(Mismatch::RequiredNull),
        ParameterValue::Omitted { .. } if !attribute.allows_omitted => {
            Err(Mismatch::OmittedUnsupported)
        }
        ParameterValue::Null { .. } | ParameterValue::Omitted { .. } => Ok(()),
        _ => validate_non_null_value(entity_types, schema, value, &attribute.ty),
    }
}

fn validate_non_null_value<'a>(
    entity_types: &'a EntityTypeIndex,
    schema: &'a SchemaDoc,
    value: &'a ParameterValue,
    expected: &'a TypeRef,
) -> Validation<'a> {
    if matches!(value, ParameterValue::Malformed { .. }) {
        return Ok(());
    }
    match expected {
        TypeRef::Primitive(primitive) => validate_primitive(value, primitive),
        TypeRef::Aggregate(aggregate) => validate_aggregate(entity_types, schema, value, aggregate),
        TypeRef::GenericEntity { .. } | TypeRef::Generic { .. } => Ok(()),
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

fn validate_named_type<'a>(
    entity_types: &'a EntityTypeIndex,
    schema: &'a SchemaDoc,
    value: &'a ParameterValue,
    type_name: &'a str,
) -> Validation<'a> {
    let Some(type_def) = schema.type_decl(type_name) else {
        return Ok(());
    };
    match type_def {
        TypeDoc::Alias(alias) => {
            let value = unwrap_inline_typed_value(value, type_name)?;
            validate_non_null_value(entity_types, schema, value, &alias.target)
        }
        TypeDoc::Enumeration(enum_def) => {
            let value = unwrap_inline_typed_value(value, type_name)?;
            validate_enum_value(value, &enum_def.items)
        }
        TypeDoc::Select(select) => validate_select(entity_types, schema, value, select),
    }
}

/// Reduces an inline typed value such as `IFCLABEL('Name')` to the single payload that has to be
/// checked against `type_name`'s underlying definition. Values that are not inline typed pass
/// through unchanged; a wrong inline name or argument count is a mismatch.
fn unwrap_inline_typed_value<'a>(
    value: &'a ParameterValue,
    type_name: &'a str,
) -> Result<&'a ParameterValue, Mismatch<'a>> {
    let ParameterValue::Typed {
        type_name: inline_name,
        inner,
        ..
    } = value
    else {
        return Ok(value);
    };

    if inline_name != type_name {
        return Err(Mismatch::TypedValueName {
            expected: type_name,
            found: inline_name,
        });
    }

    match inner.as_slice() {
        [single] => Ok(single),
        _ => Err(Mismatch::TypedValueArity { type_name }),
    }
}

fn validate_select<'a>(
    entity_types: &'a EntityTypeIndex,
    schema: &'a SchemaDoc,
    value: &'a ParameterValue,
    select: &'a SelectTypeDef,
) -> Validation<'a> {
    if select
        .options
        .iter()
        .any(|option| select_option_accepts(entity_types, schema, value, option))
    {
        Ok(())
    } else {
        Err(Mismatch::NoSelectOption {
            select: &select.name,
        })
    }
}

/// An inline typed value can only match the defined type it names, so differently named aliases
/// and enumerations are rejected without validating the payload; nested selects are searched.
fn select_option_accepts<'a>(
    entity_types: &'a EntityTypeIndex,
    schema: &'a SchemaDoc,
    value: &'a ParameterValue,
    option: &'a TypeRef,
) -> bool {
    if let (ParameterValue::Typed { type_name, .. }, TypeRef::Named(named)) = (value, option)
        && named.kind != NamedTypeKind::Entity
        && named.name != *type_name
    {
        return match schema.type_decl(&named.name) {
            Some(TypeDoc::Select(select)) => {
                validate_select(entity_types, schema, value, select).is_ok()
            }
            Some(TypeDoc::Alias(_) | TypeDoc::Enumeration(_)) => false,
            None => true,
        };
    }

    validate_non_null_value(entity_types, schema, value, option).is_ok()
}

fn validate_enum_value<'a>(value: &'a ParameterValue, items: &'a [String]) -> Validation<'a> {
    match value {
        ParameterValue::Enumeration { value, .. } => {
            if items.contains(value) {
                Ok(())
            } else {
                Err(Mismatch::EnumerationItem {
                    items,
                    found: value,
                })
            }
        }
        _ => Err(Mismatch::NotEnumeration {
            found: value.kind_name(),
        }),
    }
}

fn validate_aggregate<'a>(
    entity_types: &'a EntityTypeIndex,
    schema: &'a SchemaDoc,
    value: &'a ParameterValue,
    aggregate: &'a AggregateTypeRef,
) -> Validation<'a> {
    let ParameterValue::List { items, .. } = value else {
        return Err(Mismatch::NotAggregate {
            kind: aggregate.kind,
            found: value.kind_name(),
        });
    };

    if let Some(bounds) = &aggregate.bounds {
        if let BoundValue::Integer(lower) = bounds.lower
            && items.len() < lower as usize
        {
            return Err(Mismatch::TooFewItems {
                lower,
                found: items.len(),
            });
        }
        if let BoundValue::Integer(upper) = bounds.upper
            && items.len() > upper as usize
        {
            return Err(Mismatch::TooManyItems {
                upper,
                found: items.len(),
            });
        }
    }

    for item in items {
        validate_non_null_value(entity_types, schema, item, &aggregate.item)?;
    }

    Ok(())
}

fn validate_entity_reference<'a>(
    entity_types: &'a EntityTypeIndex,
    schema: &'a SchemaDoc,
    value: &'a ParameterValue,
    expected_entity: &'a str,
) -> Validation<'a> {
    let ParameterValue::Reference { id, .. } = value else {
        return Err(Mismatch::NotReference {
            expected: expected_entity,
            found: value.kind_name(),
        });
    };

    let Some(entity_name) = entity_types.entity_name_by_id(*id) else {
        return Err(Mismatch::UnresolvedReference { id: *id });
    };

    if schema.is_entity_compatible(entity_name, expected_entity) {
        Ok(())
    } else {
        Err(Mismatch::IncompatibleReference {
            expected: expected_entity,
            id: *id,
            found: entity_name,
        })
    }
}

fn validate_primitive<'a>(value: &'a ParameterValue, primitive: &PrimitiveType) -> Validation<'a> {
    let found = value.kind_name();
    match primitive {
        PrimitiveType::Integer => match value {
            ParameterValue::Number { text, .. } if is_integer(text) => Ok(()),
            ParameterValue::Number { .. } => Err(Mismatch::NotIntegerNumber),
            _ => Err(Mismatch::NotInteger { found }),
        },
        PrimitiveType::Real | PrimitiveType::Number => match value {
            ParameterValue::Number { .. } => Ok(()),
            _ => Err(Mismatch::NotNumber { found }),
        },
        PrimitiveType::String { .. } => match value {
            ParameterValue::String { .. } => Ok(()),
            _ => Err(Mismatch::NotString { found }),
        },
        PrimitiveType::Logical | PrimitiveType::Boolean => match value {
            ParameterValue::Enumeration { value, .. }
                if matches!(
                    value.as_str(),
                    "TRUE" | "FALSE" | "UNKNOWN" | "T" | "F" | "U"
                ) =>
            {
                Ok(())
            }
            _ => Err(Mismatch::NotLogical { found }),
        },
        PrimitiveType::Binary { .. } => match value {
            ParameterValue::String { .. } => Ok(()),
            _ => Err(Mismatch::NotBinary { found }),
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
    use super::*;
    use crate::document::Document;
    use crate::schema::{IfcVersion, load_express};
    use tower_lsp::lsp_types::Range;

    /// Helper function to parse a document and return the result.
    fn parse_document(text: &str) -> Document {
        Document::parse(text.to_string())
    }

    fn collect(
        document: &Document,
        schema: &SchemaDoc,
        schema_name: Option<&str>,
    ) -> Vec<Diagnostic> {
        let lazy = document.lazy.as_deref().expect("document has lazy index");
        let lazy_types = crate::document::lazy_entity_type_index(lazy, &document.text);
        collect_lazy(lazy, &document.text, &lazy_types, schema, schema_name)
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
        let diagnostics = collect(&doc, &test_schema(), None);

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("GlobalId"));
        assert!(diagnostics[0].message.contains("expected string"));
    }

    #[test]
    fn datatype_validation_recovers_parameters_before_inside_and_after_a_syntax_error() {
        let doc = parse_document(concat!(
            "#1=IFCWALL(123,.MOVABLE.);\n",
            "#2=IFCWALL(456,.MOVABLE.;\n",
            "#3=IFCWALL(789,.MOVABLE.);\n",
        ));
        let diagnostics = collect(&doc, &test_schema(), None);
        assert_eq!(diagnostics.len(), 3, "{diagnostics:?}");
        assert_eq!(
            diagnostics
                .iter()
                .map(|d| d.range.start.line)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert!(
            diagnostics
                .iter()
                .all(|d| d.message.contains("expected string"))
        );
    }

    #[test]
    fn datatype_validation_skips_statements_with_an_unclosed_value() {
        let doc = parse_document(concat!(
            "#1=IFCWALL(123,'abc);\n",
            "#2=IFCWALL(456,(1,(2,.MOVABLE.);\n",
            "#3=IFCWALL(789,.MOVABLE.);\n",
        ));
        let diagnostics = collect(&doc, &test_schema(), None);
        assert_eq!(
            diagnostics
                .iter()
                .map(|d| d.range.start.line)
                .collect::<Vec<_>>(),
            [2],
            "{diagnostics:?}"
        );
    }

    #[test]
    fn datatype_validation_keeps_positions_after_a_malformed_value() {
        let doc = parse_document("#1=IFCWALL(#,.MOVABLE.);\n#2=IFCWALL(1E,#);\n");
        let diagnostics = collect(&doc, &test_schema(), None);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    /// Test that the datatype validator accepts valid values.
    #[test]
    fn datatype_validator_accepts_valid_values() {
        let doc = parse_document("#1=IFCWALL('gid',.MOVABLE.);");
        let diagnostics = collect(&doc, &test_schema(), None);

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    /// Test that the datatype validator reports unresolved references.
    #[test]
    fn datatype_validator_reports_unresolved_reference() {
        let doc = parse_document("#1=IFCWALL('gid',#2);");

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

        let diagnostics = collect(&doc, &schema, None);

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("does not resolve"));
    }

    #[test]
    fn reference_checks_resolve_the_last_definition_of_each_id() {
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
        let text = "#1=IFCHOLDER((#2,#3),(#9),#4,#5);\n#2=IFCCHILD();\n#3=IFCOTHER();\n#4=IFCTARGET();\n#4=IFCOTHER();\n#5=IFCCHILD();";
        let document = parse_document(text);

        assert_eq!(
            collect(&document, &schema, None)
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            [
                "A: expected reference to `IFCTARGET` but `#3` points to `IFCOTHER`",
                "B: reference `#9` does not resolve to a local entity",
                "C: expected reference to `IFCTARGET` but `#4` points to `IFCOTHER`",
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

        let doc = parse_document("#15=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);");
        let diagnostics = collect(&doc, &schema, None);

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
        let diagnostics = collect(&doc, &schema, None);

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
        collect(&doc, &select_schema(), Some("IFC4"))
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
