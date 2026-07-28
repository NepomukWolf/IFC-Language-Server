//! Resolver for parsed raw schemas.
//! It flattens inherited attributes, marks derived inherited attributes as omittable, and records
//! transitive supertypes for reference compatibility checks.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::schema::types::{DerivedAttributeDef, EntityDef, RawSchema};
use crate::schema::{EntityAttributeDoc, EntityDoc, IfcVersion, SchemaDoc, TypeDoc};

pub(crate) fn resolve_schema(version: Option<IfcVersion>, raw: RawSchema) -> SchemaDoc {
    let mut entities = HashMap::new();

    for name in raw.entities.keys() {
        let attributes = resolve_all_attributes(&raw.entities, name, &mut HashSet::new());
        let all_supertypes = resolve_all_supertypes(&raw.entities, name, &mut HashSet::new());
        let declared = raw
            .entities
            .get(name)
            .expect("entity key should resolve to entity");

        entities.insert(
            name.clone(),
            EntityDoc {
                name: declared.name.clone(),
                attributes,
                all_supertypes,
                url: version
                    .map(|version| version.documentation_url(&declared.name))
                    .unwrap_or_default(),
            },
        );
    }

    SchemaDoc {
        entities,
        types: raw.types.into_iter().collect::<HashMap<String, TypeDoc>>(),
    }
}

fn resolve_all_attributes(
    entities: &BTreeMap<String, EntityDef>,
    name: &str,
    visiting: &mut HashSet<String>,
) -> Vec<EntityAttributeDoc> {
    if !visiting.insert(name.to_string()) {
        return Vec::new();
    }

    let Some(entity) = entities.get(name) else {
        visiting.remove(name);
        return Vec::new();
    };

    let mut attributes = Vec::new();

    for supertype in &entity.supertypes {
        attributes.extend(resolve_all_attributes(entities, supertype, visiting));
    }

    let derived_attributes = &entity.derived_attributes;

    attributes.extend(entity.attributes.iter().map(|attr| EntityAttributeDoc {
        name: attr.name.clone(),
        type_name: attr.type_name.clone(),
        declared_in: entity.name.clone(),
        ty: attr.ty.clone(),
        optional: attr.optional,
        allows_omitted: false,
    }));

    for attribute in &mut attributes {
        if matches_derived_override(attribute, derived_attributes) {
            attribute.allows_omitted = true;
        }
    }

    visiting.remove(name);
    attributes
}

fn matches_derived_override(
    attribute: &EntityAttributeDoc,
    derived_attributes: &[DerivedAttributeDef],
) -> bool {
    derived_attributes.iter().any(|derived| {
        derived.name.eq_ignore_ascii_case(&attribute.name)
            && derived
                .declared_in
                .as_ref()
                .is_none_or(|declared_in| declared_in.eq_ignore_ascii_case(&attribute.declared_in))
    })
}

fn resolve_all_supertypes(
    entities: &BTreeMap<String, EntityDef>,
    name: &str,
    visiting: &mut HashSet<String>,
) -> HashSet<String> {
    if !visiting.insert(name.to_string()) {
        return HashSet::new();
    }

    let mut all_supertypes = HashSet::new();

    if let Some(entity) = entities.get(name) {
        for supertype in &entity.supertypes {
            all_supertypes.insert(supertype.clone());
            all_supertypes.extend(resolve_all_supertypes(entities, supertype, visiting));
        }
    }

    visiting.remove(name);
    all_supertypes
}
