//! `ifc/entityCounts` and `ifc/findInstances` custom requests: overviews of a document's
//! instances, such as for a side panel in the client.
//! They read the lightweight text index and scan only the instances they report, without
//! tree-sitter state, so they also work on files above the AST parsing limit.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::{Position, Url};

use crate::document::Document;
use crate::schema::SchemaDoc;

use super::step_scan::{
    ScannedInstance, scan_definition_entity_name, scan_instance, string_literal,
};

pub const ENTITY_COUNTS_METHOD: &str = "ifc/entityCounts";
pub const FIND_INSTANCES_METHOD: &str = "ifc/findInstances";

#[derive(Debug, Deserialize)]
pub struct EntityCountsParams {
    pub uri: Url,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct EntityCounts {
    /// The document's selected schema; None when it is unknown.
    pub schema: Option<String>,
    pub total: usize,
    /// Most frequent entity first, then by name.
    pub entities: Vec<EntityCount>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct EntityCount {
    pub entity: String,
    pub count: usize,
}

#[derive(Debug, Deserialize)]
pub struct FindInstancesParams {
    pub uri: Url,
    /// Only instances of this entity, ignoring case.
    pub entity: Option<String>,
    /// Only instances whose GlobalId or Name contains this text, ignoring case.
    pub query: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct FoundInstances {
    /// The number of matching instances, of which the first `limit` are listed.
    pub total: usize,
    pub instances: Vec<FoundInstance>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FoundInstance {
    pub id: u32,
    pub entity: String,
    /// Where the instance's `#id` starts.
    pub position: Position,
    pub global_id: Option<String>,
    pub name: Option<String>,
}

pub fn entity_counts(document: &Document, schema_name: Option<&str>) -> EntityCounts {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for &offset in document.definitions.values() {
        if let Some(entity) = definition_entity_name(document, offset) {
            *counts.entry(entity).or_default() += 1;
        }
    }

    let total = counts.values().sum();
    let mut entities: Vec<EntityCount> = counts
        .into_iter()
        .map(|(entity, count)| EntityCount { entity, count })
        .collect();
    entities.sort_unstable_by(|a, b| b.count.cmp(&a.count).then_with(|| a.entity.cmp(&b.entity)));

    EntityCounts {
        schema: schema_name.map(str::to_string),
        total,
        entities,
    }
}

/// Lists matching instances in document order. GlobalId and Name are read at the positions
/// `schema` gives them, so without a schema they are None and a `query` matches nothing.
pub fn find_instances(
    document: &Document,
    schema: Option<&SchemaDoc>,
    params: &FindInstancesParams,
) -> Option<FoundInstances> {
    let query = params
        .query
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_lowercase);
    let mut labels = LabelReader::new(document, schema);
    let mut total = 0;
    let mut instances = Vec::new();

    for (offset, id) in document.definitions_in_order() {
        let Some(entity) = definition_entity_name(document, offset) else {
            continue;
        };
        if params
            .entity
            .as_deref()
            .is_some_and(|wanted| !entity.eq_ignore_ascii_case(wanted))
        {
            continue;
        }

        let label = match &query {
            Some(query) => {
                let label = labels.read(offset, &entity);
                if !label.contains(query) {
                    continue;
                }
                Some(label)
            }
            None => None,
        };
        total += 1;
        if instances.len() < params.limit {
            let label = label.unwrap_or_else(|| labels.read(offset, &entity));
            instances.push(FoundInstance {
                id,
                position: document.offset_to_position(offset)?,
                entity,
                global_id: label.global_id,
                name: label.name,
            });
        }
    }

    Some(FoundInstances { total, instances })
}

fn definition_entity_name(document: &Document, offset: usize) -> Option<String> {
    scan_definition_entity_name(&document.text, offset, document.text.len())
}

#[derive(Default)]
pub(super) struct Label {
    pub(super) global_id: Option<String>,
    pub(super) name: Option<String>,
}

impl Label {
    fn contains(&self, lowercase_query: &str) -> bool {
        [&self.global_id, &self.name].into_iter().any(|value| {
            value
                .as_deref()
                .is_some_and(|value| value.to_lowercase().contains(lowercase_query))
        })
    }
}

/// Reads instances' GlobalId and Name, looking up each entity's attribute positions once.
pub(super) struct LabelReader<'a> {
    document: &'a Document,
    schema: Option<&'a SchemaDoc>,
    attribute_indexes: HashMap<String, (Option<usize>, Option<usize>)>,
}

impl<'a> LabelReader<'a> {
    pub(super) fn new(document: &'a Document, schema: Option<&'a SchemaDoc>) -> Self {
        Self {
            document,
            schema,
            attribute_indexes: HashMap::new(),
        }
    }

    fn read(&mut self, offset: usize, entity: &str) -> Label {
        let (global_id_index, name_index) = self.attribute_indexes(entity);
        if global_id_index.is_none() && name_index.is_none() {
            return Label::default();
        }
        let text = self.document.text.as_str();
        scan_instance(text, offset, text.len())
            .map_or_else(Label::default, |(instance, _)| self.read_scanned(&instance))
    }

    pub(super) fn read_scanned(&mut self, instance: &ScannedInstance) -> Label {
        let (global_id_index, name_index) = self.attribute_indexes(&instance.entity_name);
        let text = self.document.text.as_str();
        let string_at =
            |index: Option<usize>| string_literal(text, instance.arguments.get(index?)?.0);
        Label {
            global_id: string_at(global_id_index),
            name: string_at(name_index),
        }
    }

    fn attribute_indexes(&mut self, entity: &str) -> (Option<usize>, Option<usize>) {
        let schema = self.schema;
        *self
            .attribute_indexes
            .entry(entity.to_string())
            .or_insert_with(|| {
                let attributes = schema
                    .and_then(|schema| schema.entity(entity))
                    .map(|entity| entity.attributes.as_slice())
                    .unwrap_or_default();
                let index = |name: &str| {
                    attributes
                        .iter()
                        .position(|attribute| attribute.name == name)
                };
                (index("GlobalId"), index("Name"))
            })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::schema::{EntityAttributeDoc, EntityDoc, TypeRef};

    const TEXT: &str = "DATA;\n#1=IFCWALL('0abc',$,'North wall',$);\n#2=IFCWALL('1def',$,'South',$);\n#3=IFCRELAGGREGATES('2ghi',$,$,$,(#1,#2),#1);\n#4=ifcwall('3jkl',$,$,$);\nENDSEC;\n";

    fn document() -> Document {
        Document::new_unloaded(TEXT.to_string())
    }

    fn schema() -> SchemaDoc {
        let attribute = |name: &str| EntityAttributeDoc {
            name: name.to_string(),
            type_name: "IfcLabel".to_string(),
            declared_in: "IfcRoot".to_string(),
            ty: TypeRef::Generic { label: None },
            optional: true,
            allows_omitted: false,
        };
        let entity = |name: &str, attributes: &[&str]| EntityDoc {
            name: name.to_string(),
            attributes: attributes.iter().map(|name| attribute(name)).collect(),
            url: String::new(),
            all_supertypes: HashSet::new(),
        };
        SchemaDoc {
            entities: [
                (
                    "IFCWALL".to_string(),
                    entity(
                        "IfcWall",
                        &["GlobalId", "OwnerHistory", "Name", "Description"],
                    ),
                ),
                (
                    "IFCRELAGGREGATES".to_string(),
                    entity(
                        "IfcRelAggregates",
                        &[
                            "GlobalId",
                            "OwnerHistory",
                            "Name",
                            "Description",
                            "RelatedObjects",
                            "RelatingObject",
                        ],
                    ),
                ),
            ]
            .into(),
            types: Default::default(),
        }
    }

    fn find(entity: Option<&str>, query: Option<&str>, limit: usize) -> FoundInstances {
        let params = FindInstancesParams {
            uri: Url::parse("file:///a.ifc").unwrap(),
            entity: entity.map(str::to_string),
            query: query.map(str::to_string),
            limit,
        };
        find_instances(&document(), Some(&schema()), &params).expect("instances")
    }

    fn ids(found: &FoundInstances) -> Vec<u32> {
        found.instances.iter().map(|instance| instance.id).collect()
    }

    #[test]
    fn counts_instances_per_entity_ignoring_case() {
        let counts = entity_counts(&document(), Some("IFC4"));

        assert_eq!(
            serde_json::to_value(&counts).unwrap(),
            serde_json::json!({
                "schema": "IFC4",
                "total": 4,
                "entities": [
                    { "entity": "IFCWALL", "count": 3 },
                    { "entity": "IFCRELAGGREGATES", "count": 1 },
                ],
            })
        );
    }

    #[test]
    fn counts_without_a_schema() {
        assert_eq!(entity_counts(&document(), None).schema, None);
    }

    #[test]
    fn finds_instances_of_one_entity_up_to_the_limit() {
        let found = find(Some("IfcWall"), None, 2);

        assert_eq!(found.total, 3);
        assert_eq!(ids(&found), [1, 2]);
        assert_eq!(
            serde_json::to_value(&found.instances[0]).unwrap(),
            serde_json::json!({
                "id": 1,
                "entity": "IFCWALL",
                "position": { "line": 1, "character": 0 },
                "globalId": "0abc",
                "name": "North wall",
            })
        );
    }

    #[test]
    fn finds_instances_by_global_id_or_name_ignoring_case() {
        assert_eq!(ids(&find(None, Some("SOUTH"), 10)), [2]);
        assert_eq!(ids(&find(None, Some(" 2gh "), 10)), [3]);
    }

    #[test]
    fn queries_match_decoded_names() {
        let text = "DATA;\n#1=IFCWALL('0abc',$,'M\\X2\\00FC\\X0\\nchen',$);\nENDSEC;\n";
        let params = FindInstancesParams {
            uri: Url::parse("file:///a.ifc").unwrap(),
            entity: None,
            query: Some("MÜNCHEN".to_string()),
            limit: 10,
        };

        let found = find_instances(
            &Document::new_unloaded(text.to_string()),
            Some(&schema()),
            &params,
        )
        .expect("instances");

        assert_eq!(found.total, 1);
        assert_eq!(found.instances[0].name.as_deref(), Some("München"));
    }

    #[test]
    fn without_a_schema_labels_are_unknown_and_queries_match_nothing() {
        let params = FindInstancesParams {
            uri: Url::parse("file:///a.ifc").unwrap(),
            entity: None,
            query: None,
            limit: 10,
        };
        let found = find_instances(&document(), None, &params).expect("instances");

        assert_eq!(found.total, 4);
        assert!(
            found
                .instances
                .iter()
                .all(|instance| { instance.global_id.is_none() && instance.name.is_none() })
        );

        let params = FindInstancesParams {
            query: Some("south".to_string()),
            ..params
        };
        assert_eq!(find_instances(&document(), None, &params).unwrap().total, 0);
    }
}
