//! Schema data model shared by hover, diagnostics, parsing, and resolution.
//! Public types form the runtime `SchemaDoc` API; `pub(crate)` types are parser/resolver
//! intermediates that should not leak into feature code.

use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct SchemaDoc {
    pub entities: HashMap<String, EntityDoc>,
    pub types: HashMap<String, TypeDoc>,
}

impl SchemaDoc {
    pub fn entity(&self, name: &str) -> Option<&EntityDoc> {
        self.entities.get(&normalize_name(name))
    }

    pub fn type_decl(&self, name: &str) -> Option<&TypeDoc> {
        self.types.get(&normalize_name(name))
    }

    pub fn is_entity_compatible(&self, actual: &str, expected: &str) -> bool {
        let actual = normalize_name(actual);
        let expected = normalize_name(expected);

        actual == expected
            || self
                .entities
                .get(&actual)
                .is_some_and(|entity| entity.all_supertypes.contains(&expected))
    }
}

#[derive(Debug, Clone)]
pub struct EntityDoc {
    pub name: String,
    pub attributes: Vec<EntityAttributeDoc>,
    pub url: String,
    pub all_supertypes: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityAttributeDoc {
    pub name: String,
    pub type_name: String,
    pub declared_in: String,
    pub ty: TypeRef,
    pub optional: bool,
    pub allows_omitted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeDoc {
    Alias(AliasTypeDef),
    Enumeration(EnumerationTypeDef),
    Select(SelectTypeDef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasTypeDef {
    pub name: String,
    pub target: TypeRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumerationTypeDef {
    pub name: String,
    pub items: Vec<String>,
    pub extensible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectTypeDef {
    pub name: String,
    pub options: Vec<TypeRef>,
    pub extensible: bool,
    pub generic_entity: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeRef {
    Primitive(PrimitiveType),
    Named(NamedTypeRef),
    Aggregate(AggregateTypeRef),
    GenericEntity { label: Option<String> },
    Generic { label: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedTypeRef {
    pub name: String,
    pub kind: NamedTypeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamedTypeKind {
    Entity,
    Type,
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateTypeRef {
    pub kind: AggregateKind,
    pub bounds: Option<AggregateBounds>,
    pub item: Box<TypeRef>,
    pub unique: bool,
    pub optional: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateKind {
    Set,
    Bag,
    List,
    Array,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateBounds {
    pub lower: BoundValue,
    pub upper: BoundValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundValue {
    Integer(u32),
    Unbounded,
    UnsupportedExpression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrimitiveType {
    Number,
    Real,
    Integer,
    Logical,
    Boolean,
    String { width: Option<usize>, fixed: bool },
    Binary { width: Option<usize>, fixed: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawSchema {
    pub entities: BTreeMap<String, EntityDef>,
    pub types: BTreeMap<String, TypeDoc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EntityDef {
    pub name: String,
    pub attributes: Vec<AttributeDef>,
    pub derived_attributes: Vec<DerivedAttributeDef>,
    pub supertypes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttributeDef {
    pub name: String,
    pub ty: TypeRef,
    pub type_name: String,
    pub optional: bool,
    pub position: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DerivedAttributeDef {
    pub name: String,
    pub declared_in: Option<String>,
}

pub fn normalize_name(name: &str) -> String {
    name.to_ascii_uppercase()
}
