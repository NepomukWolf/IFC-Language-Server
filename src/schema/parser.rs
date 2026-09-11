//! EXPRESS parser and normalizer.
//! This module uses `espr` to parse EXPRESS text, then maps the declarations needed by current
//! hover and datatype diagnostics into the internal raw schema model.
//! Global algorithm and rule blocks are stripped because current features do not evaluate them.

use std::collections::{BTreeMap, BTreeSet};

use espr::ast::{
    AttributeDecl, Bound, BuiltInConstant, Entity, EntityAttribute, Expression, Extensibility,
    Literal, SimpleType, SyntaxTree, Type, TypeDecl,
};

use crate::schema::LoadExpressError;
use crate::schema::types::{AttributeDef, DerivedAttributeDef, EntityDef, RawSchema};
use crate::schema::{
    AggregateBounds, AggregateKind, AggregateTypeRef, AliasTypeDef, BoundValue, EnumerationTypeDef,
    NamedTypeKind, NamedTypeRef, PrimitiveType, SelectTypeDef, TypeDoc, TypeRef, normalize_name,
};

pub(crate) fn parse_express_source(source: &str) -> Result<RawSchema, LoadExpressError> {
    let sanitized = sanitize_express_source(source);
    let syntax_tree = SyntaxTree::parse(&sanitized)
        .map_err(|error| LoadExpressError::Parse(format!("{error:?}")))?;
    let schema = syntax_tree
        .schemas
        .first()
        .ok_or(LoadExpressError::MissingSchema)?;

    let entity_names = schema
        .entities
        .iter()
        .map(|entity| normalize_name(&entity.name))
        .collect::<BTreeSet<_>>();
    let type_names = schema
        .types
        .iter()
        .map(|type_decl| normalize_name(&type_decl.type_id))
        .collect::<BTreeSet<_>>();

    let mut entities = BTreeMap::new();
    let mut types = BTreeMap::new();

    for entity in &schema.entities {
        entities.insert(
            normalize_name(&entity.name),
            normalize_entity(entity, &entity_names, &type_names),
        );
    }

    for type_decl in &schema.types {
        types.insert(
            normalize_name(&type_decl.type_id),
            normalize_type_decl(type_decl, &entity_names, &type_names),
        );
    }

    Ok(RawSchema { entities, types })
}

fn sanitize_express_source(source: &str) -> String {
    // Global algorithms/rules are not used by current hover or datatype diagnostics.
    let source = source.replace("\r\n", "\n");
    let source = strip_block_sections(&source, "FUNCTION", "END_FUNCTION;");
    let source = strip_block_sections(&source, "PROCEDURE", "END_PROCEDURE;");
    strip_block_sections(&source, "RULE", "END_RULE;")
}

fn strip_block_sections(source: &str, start_keyword: &str, end_keyword: &str) -> String {
    let mut output = String::new();
    let mut skipping = false;

    for line in source.lines() {
        let trimmed = line.trim_start();

        if skipping {
            if trimmed.starts_with(end_keyword) {
                skipping = false;
            }
            continue;
        }

        if trimmed.starts_with(start_keyword) {
            skipping = true;
            continue;
        }

        output.push_str(line);
        output.push('\n');
    }

    output
}

fn normalize_entity(
    entity: &Entity,
    entity_names: &BTreeSet<String>,
    type_names: &BTreeSet<String>,
) -> EntityDef {
    EntityDef {
        name: entity.name.clone(),
        attributes: entity
            .attributes
            .iter()
            .enumerate()
            .filter_map(|(position, attr)| {
                normalize_attribute(attr, position, entity_names, type_names)
            })
            .collect(),
        derived_attributes: normalize_derived_attributes(entity),
        supertypes: entity
            .subtype_of
            .as_ref()
            .map(|decl| {
                decl.entity_references
                    .iter()
                    .map(|name| normalize_name(name))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn normalize_derived_attributes(entity: &Entity) -> Vec<DerivedAttributeDef> {
    entity
        .derive_clause
        .as_ref()
        .map(|clause| {
            clause
                .attributes
                .iter()
                .map(|attribute| match &attribute.attr {
                    AttributeDecl::Reference(name) => DerivedAttributeDef {
                        name: name.clone(),
                        declared_in: None,
                    },
                    AttributeDecl::Qualified {
                        group,
                        attribute,
                        rename: _,
                    } => DerivedAttributeDef {
                        name: attribute.clone(),
                        declared_in: Some(normalize_name(group)),
                    },
                })
                .collect()
        })
        .unwrap_or_default()
}

fn normalize_attribute(
    attr: &EntityAttribute,
    position: usize,
    entity_names: &BTreeSet<String>,
    type_names: &BTreeSet<String>,
) -> Option<AttributeDef> {
    let AttributeDecl::Reference(name) = &attr.name else {
        return None;
    };

    Some(AttributeDef {
        name: name.clone(),
        ty: normalize_type_ref(&attr.ty, entity_names, type_names),
        type_name: format_attribute_type(&attr.ty, attr.optional),
        optional: attr.optional,
        position,
    })
}

fn normalize_type_decl(
    type_decl: &TypeDecl,
    entity_names: &BTreeSet<String>,
    type_names: &BTreeSet<String>,
) -> TypeDoc {
    match &type_decl.underlying_type {
        Type::Enumeration {
            extensibility,
            items,
        } => TypeDoc::Enumeration(EnumerationTypeDef {
            name: type_decl.type_id.clone(),
            items: items.iter().map(|item| normalize_name(item)).collect(),
            extensible: !matches!(extensibility, Extensibility::None),
        }),
        Type::Select {
            extensibility,
            types,
        } => TypeDoc::Select(SelectTypeDef {
            name: type_decl.type_id.clone(),
            options: types
                .iter()
                .map(|name| normalize_named_type_ref(name, entity_names, type_names))
                .map(TypeRef::Named)
                .collect(),
            extensible: !matches!(extensibility, Extensibility::None),
            generic_entity: matches!(extensibility, Extensibility::GenericEntity),
        }),
        ty => TypeDoc::Alias(AliasTypeDef {
            name: type_decl.type_id.clone(),
            target: normalize_type_ref(ty, entity_names, type_names),
        }),
    }
}

fn normalize_type_ref(
    ty: &Type,
    entity_names: &BTreeSet<String>,
    type_names: &BTreeSet<String>,
) -> TypeRef {
    match ty {
        Type::Simple(simple) => TypeRef::Primitive(normalize_simple_type(simple)),
        Type::Named(name) => {
            TypeRef::Named(normalize_named_type_ref(name, entity_names, type_names))
        }
        Type::Set { base, bound } => TypeRef::Aggregate(AggregateTypeRef {
            kind: AggregateKind::Set,
            bounds: bound.as_ref().map(normalize_bounds),
            item: Box::new(normalize_type_ref(base, entity_names, type_names)),
            unique: false,
            optional: false,
        }),
        Type::Bag { base, bound } => TypeRef::Aggregate(AggregateTypeRef {
            kind: AggregateKind::Bag,
            bounds: bound.as_ref().map(normalize_bounds),
            item: Box::new(normalize_type_ref(base, entity_names, type_names)),
            unique: false,
            optional: false,
        }),
        Type::List {
            base,
            bound,
            unique,
        } => TypeRef::Aggregate(AggregateTypeRef {
            kind: AggregateKind::List,
            bounds: bound.as_ref().map(normalize_bounds),
            item: Box::new(normalize_type_ref(base, entity_names, type_names)),
            unique: *unique,
            optional: false,
        }),
        Type::Array {
            base,
            bound,
            unique,
            optional,
        } => TypeRef::Aggregate(AggregateTypeRef {
            kind: AggregateKind::Array,
            bounds: bound.as_ref().map(normalize_bounds),
            item: Box::new(normalize_type_ref(base, entity_names, type_names)),
            unique: *unique,
            optional: *optional,
        }),
        Type::Aggregate { base, label } => TypeRef::Aggregate(AggregateTypeRef {
            kind: AggregateKind::List,
            bounds: None,
            item: Box::new(normalize_type_ref(base, entity_names, type_names)),
            unique: false,
            optional: label.is_some(),
        }),
        Type::GenericEntity(label) => TypeRef::GenericEntity {
            label: label.clone(),
        },
        Type::Generic(label) => TypeRef::Generic {
            label: label.clone(),
        },
        Type::Enumeration {
            extensibility,
            items,
        } => TypeRef::Named(NamedTypeRef {
            name: format!(
                "__anonymous_enum:{}:{}",
                matches!(extensibility, Extensibility::Extensible),
                items.join("|")
            ),
            kind: NamedTypeKind::Type,
        }),
        Type::Select {
            extensibility,
            types,
        } => TypeRef::Named(NamedTypeRef {
            name: format!(
                "__anonymous_select:{}:{}",
                matches!(extensibility, Extensibility::GenericEntity),
                types.join("|")
            ),
            kind: NamedTypeKind::Type,
        }),
    }
}

fn normalize_simple_type(simple: &SimpleType) -> PrimitiveType {
    match simple {
        SimpleType::Number => PrimitiveType::Number,
        SimpleType::Real => PrimitiveType::Real,
        SimpleType::Integer => PrimitiveType::Integer,
        SimpleType::Logical => PrimitiveType::Logical,
        SimpleType::Boolen => PrimitiveType::Boolean,
        SimpleType::String_ { width_spec } => PrimitiveType::String {
            width: width_spec.map(|width| width.width),
            fixed: width_spec.is_some_and(|width| width.fixed),
        },
        SimpleType::Binary { width_spec } => PrimitiveType::Binary {
            width: width_spec.map(|width| width.width),
            fixed: width_spec.is_some_and(|width| width.fixed),
        },
    }
}

fn normalize_named_type_ref(
    name: &str,
    entity_names: &BTreeSet<String>,
    type_names: &BTreeSet<String>,
) -> NamedTypeRef {
    let normalized = normalize_name(name);
    let kind = if entity_names.contains(&normalized) {
        NamedTypeKind::Entity
    } else if type_names.contains(&normalized) {
        NamedTypeKind::Type
    } else {
        NamedTypeKind::Unresolved
    };

    NamedTypeRef {
        name: normalized,
        kind,
    }
}

fn normalize_bounds(bound: &Bound) -> AggregateBounds {
    AggregateBounds {
        lower: normalize_bound_value(&bound.lower),
        upper: normalize_bound_value(&bound.upper),
    }
}

fn normalize_bound_value(expr: &Expression) -> BoundValue {
    match expr {
        Expression::Literal(Literal::Real(value)) if *value >= 0.0 && value.fract() == 0.0 => {
            BoundValue::Integer(*value as u32)
        }
        Expression::QualifiableFactor {
            factor: espr::ast::QualifiableFactor::BuiltInConstant(BuiltInConstant::Indeterminate),
            qualifiers,
        } if qualifiers.is_empty() => BoundValue::Unbounded,
        _ => BoundValue::UnsupportedExpression,
    }
}

fn format_attribute_type(ty: &Type, optional: bool) -> String {
    let type_name = format_type(ty);
    if optional {
        format!("OPTIONAL {type_name}")
    } else {
        type_name
    }
}

fn format_type(ty: &Type) -> String {
    match ty {
        Type::Simple(simple) => format_simple_type(simple),
        Type::Named(name) => name.clone(),
        Type::Set { base, bound } => {
            format!("SET{} OF {}", format_bound(bound), format_type(base))
        }
        Type::Bag { base, bound } => {
            format!("BAG{} OF {}", format_bound(bound), format_type(base))
        }
        Type::List {
            base,
            bound,
            unique,
        } => {
            let unique = if *unique { " UNIQUE" } else { "" };
            format!(
                "LIST{} OF{} {}",
                format_bound(bound),
                unique,
                format_type(base)
            )
        }
        Type::Array {
            base,
            bound,
            unique,
            optional,
        } => {
            let unique = if *unique { " UNIQUE" } else { "" };
            let optional = if *optional { " OPTIONAL" } else { "" };
            format!(
                "ARRAY{} OF{}{} {}",
                format_bound(bound),
                optional,
                unique,
                format_type(base)
            )
        }
        Type::Enumeration { items, .. } => format!("ENUMERATION OF ({})", items.join(", ")),
        Type::Select { types, .. } => format!("SELECT ({})", types.join(", ")),
        Type::Aggregate { base, .. } => format!("AGGREGATE OF {}", format_type(base)),
        Type::GenericEntity(label) => format_generic_type("GENERIC_ENTITY", label),
        Type::Generic(label) => format_generic_type("GENERIC", label),
    }
}

fn format_simple_type(simple: &SimpleType) -> String {
    match simple {
        SimpleType::Number => "NUMBER".to_string(),
        SimpleType::Real => "REAL".to_string(),
        SimpleType::Integer => "INTEGER".to_string(),
        SimpleType::Logical => "LOGICAL".to_string(),
        SimpleType::Boolen => "BOOLEAN".to_string(),
        SimpleType::String_ { width_spec } => format_width_type("STRING", width_spec),
        SimpleType::Binary { width_spec } => format_width_type("BINARY", width_spec),
    }
}

fn format_width_type(name: &str, width_spec: &Option<espr::ast::WidthSpec>) -> String {
    match width_spec {
        Some(width_spec) if width_spec.fixed => format!("{name}({}) FIXED", width_spec.width),
        Some(width_spec) => format!("{name}({})", width_spec.width),
        None => name.to_string(),
    }
}

fn format_bound(bound: &Option<Bound>) -> String {
    bound
        .as_ref()
        .map(|bound| {
            format!(
                " [{}:{}]",
                format_bound_value(&bound.lower),
                format_bound_value(&bound.upper)
            )
        })
        .unwrap_or_default()
}

fn format_bound_value(expr: &Expression) -> String {
    match normalize_bound_value(expr) {
        BoundValue::Integer(value) => value.to_string(),
        BoundValue::Unbounded | BoundValue::UnsupportedExpression => "?".to_string(),
    }
}

fn format_generic_type(name: &str, label: &Option<String>) -> String {
    label
        .as_ref()
        .map(|label| format!("{name}:{label}"))
        .unwrap_or_else(|| name.to_string())
}
