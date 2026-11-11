//! `ifc/instances` custom request: the document's entity instances with every positional value
//! labelled by its attribute name from the selected schema.
//! Values come from a transient `EntityInstanceCollection`; a text scan of each argument list keeps
//! values the syntax tree drops (such as an enumeration without dots) at their positions.
//! Ranges use the syntax tree's convention: zero-based lines and byte columns.

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::{Position, Range, Url};

use crate::document::{EntityInstanceCollection, EntityInstanceInfo, ParameterValue};
use crate::schema::SchemaDoc;

use super::step_scan::{scan_argument_spans, skip_trivia, string_literal};

pub const METHOD: &str = "ifc/instances";

#[derive(Debug, Deserialize)]
pub struct InstancesParams {
    pub uri: Url,
    /// Lists only the first `limit` instances in document order.
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct Instances {
    /// The schema the attribute names come from; None when the document's schema is unknown.
    pub schema: Option<String>,
    pub total: usize,
    pub instances: Vec<Instance>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub id: Option<u32>,
    pub entity: String,
    pub range: Range,
    /// The number of attributes the schema defines for the entity; None when the schema or the
    /// entity is unknown.
    pub expected_attribute_count: Option<usize>,
    pub attributes: Vec<Attribute>,
}

#[derive(Debug, Serialize)]
pub struct Attribute {
    pub name: Option<String>,
    pub value: Value,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Value {
    #[serde(flatten)]
    pub kind: ValueKind,
    pub range: Range,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ValueKind {
    Reference {
        id: u32,
    },
    Enumeration {
        value: String,
    },
    String {
        value: Option<String>,
    },
    Number {
        value: String,
    },
    Null,
    Omitted,
    List {
        items: Vec<Value>,
    },
    Typed {
        #[serde(rename = "type")]
        type_name: String,
        items: Vec<Value>,
    },
    Unknown,
}

/// Lists `collection`, parsed from `text` whose line starts are `line_offsets`, labelled with
/// `schema`'s attribute names.
pub fn instances(
    text: &str,
    line_offsets: &[usize],
    collection: &EntityInstanceCollection,
    schema: Option<(&str, &SchemaDoc)>,
    limit: Option<usize>,
) -> Instances {
    let source = Source { text, line_offsets };
    let instances = collection
        .instances
        .iter()
        .take(limit.unwrap_or(usize::MAX))
        .map(|instance| {
            let attribute_names = schema
                .and_then(|(_, schema)| schema.entity(&instance.entity_name))
                .map(|entity| &entity.attributes);
            let attributes = source
                .positional_values(instance)
                .into_iter()
                .enumerate()
                .map(|(index, value)| Attribute {
                    name: attribute_names
                        .and_then(|attributes| attributes.get(index))
                        .map(|attribute| attribute.name.clone()),
                    value,
                })
                .collect();
            Instance {
                id: instance.id,
                entity: instance.entity_name.clone(),
                range: instance.entity_range,
                expected_attribute_count: attribute_names.map(Vec::len),
                attributes,
            }
        })
        .collect();

    Instances {
        schema: schema.map(|(name, _)| name.to_string()),
        total: collection.instances.len(),
        instances,
    }
}

struct Source<'a> {
    text: &'a str,
    line_offsets: &'a [usize],
}

impl Source<'_> {
    /// One value per top-level argument in the instance's text. An argument the syntax tree has
    /// no value for becomes `unknown`, so every later value keeps its position. Without a
    /// readable argument list, the syntax tree's values are returned as they are.
    fn positional_values(&self, instance: &EntityInstanceInfo) -> Vec<Value> {
        let parsed = || {
            instance
                .parameters
                .iter()
                .map(|parameter| self.value(parameter))
                .collect()
        };
        let Some(spans) = self.argument_spans(instance) else {
            return parsed();
        };

        let mut parameters = instance
            .parameters
            .iter()
            .filter_map(|parameter| Some((self.offset(parameter.range().start)?, parameter)))
            .peekable();
        let mut values = Vec::with_capacity(spans.len());
        for (start, end) in spans {
            while parameters.next_if(|(offset, _)| *offset < start).is_some() {}
            let value = match parameters.next_if(|(offset, _)| *offset < end.max(start + 1)) {
                Some((_, parameter)) => self.value(parameter),
                None => {
                    let Some(range) = self.range(start, end) else {
                        return parsed();
                    };
                    Value {
                        kind: ValueKind::Unknown,
                        range,
                    }
                }
            };
            values.push(value);
        }
        values
    }

    /// Byte spans of the instance's top-level arguments; None when no argument list follows the
    /// entity name.
    fn argument_spans(&self, instance: &EntityInstanceInfo) -> Option<Vec<(usize, usize)>> {
        let bytes = self.text.as_bytes();
        let instance_end = self.offset(instance.entity_range.end)?;
        let name_end = self.offset(instance.entity_name_range.end)?;
        let open = skip_trivia(bytes, name_end, instance_end);
        if bytes.get(open) != Some(&b'(') {
            return None;
        }
        Some(scan_argument_spans(bytes, open + 1, instance_end).0)
    }

    fn value(&self, parameter: &ParameterValue) -> Value {
        let kind = match parameter {
            ParameterValue::Reference { id, .. } => ValueKind::Reference { id: *id },
            ParameterValue::Enumeration { value, .. } => ValueKind::Enumeration {
                value: value.clone(),
            },
            ParameterValue::String { range } => ValueKind::String {
                value: self
                    .offset(range.start)
                    .and_then(|offset| string_literal(self.text, offset)),
            },
            ParameterValue::Number { text, .. } => ValueKind::Number {
                value: text.clone(),
            },
            ParameterValue::Null { .. } => ValueKind::Null,
            ParameterValue::Omitted { .. } => ValueKind::Omitted,
            ParameterValue::List { items, .. } => ValueKind::List {
                items: items.iter().map(|item| self.value(item)).collect(),
            },
            ParameterValue::Typed {
                type_name, inner, ..
            } => ValueKind::Typed {
                type_name: type_name.clone(),
                items: inner.iter().map(|item| self.value(item)).collect(),
            },
            ParameterValue::Unknown { .. } => ValueKind::Unknown,
        };
        Value {
            kind,
            range: parameter.range(),
        }
    }

    fn offset(&self, position: Position) -> Option<usize> {
        let offset = self.line_offsets.get(position.line as usize)? + position.character as usize;
        (offset <= self.text.len()).then_some(offset)
    }

    fn range(&self, start: usize, end: usize) -> Option<Range> {
        Some(Range::new(self.position(start)?, self.position(end)?))
    }

    fn position(&self, offset: usize) -> Option<Position> {
        let line = self.line_offsets.partition_point(|&start| start <= offset);
        let line_start = self.line_offsets.get(line.checked_sub(1)?)?;
        Some(Position::new(
            (line - 1) as u32,
            (offset - line_start) as u32,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::json;

    use super::*;
    use crate::document::{Document, build_entity_instances};
    use crate::schema::{EntityAttributeDoc, EntityDoc, TypeRef};

    fn schema() -> SchemaDoc {
        let attribute = |name: &str| EntityAttributeDoc {
            name: name.to_string(),
            type_name: "IfcLabel".to_string(),
            declared_in: "IfcRoot".to_string(),
            ty: TypeRef::Generic { label: None },
            optional: true,
            allows_omitted: false,
        };
        let wall = EntityDoc {
            name: "IfcWall".to_string(),
            attributes: vec![
                attribute("GlobalId"),
                attribute("OwnerHistory"),
                attribute("Name"),
            ],
            url: String::new(),
            all_supertypes: HashSet::new(),
        };
        SchemaDoc {
            entities: [("IFCWALL".to_string(), wall)].into(),
            types: Default::default(),
        }
    }

    fn list(text: &str, schema: Option<(&str, &SchemaDoc)>, limit: Option<usize>) -> Instances {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("Error loading IFC parser");
        let document = Document::parse(&mut parser, text.to_string());
        let collection = build_entity_instances(document.tree.as_ref(), &document.text);
        instances(
            &document.text,
            &document.line_offsets,
            &collection,
            schema,
            limit,
        )
    }

    fn kinds(instance: &Instance) -> Vec<String> {
        instance
            .attributes
            .iter()
            .map(|attribute| serde_json::to_value(&attribute.value).unwrap()["kind"].to_string())
            .collect()
    }

    #[test]
    fn serializes_named_attributes() {
        let schema = schema();
        let listed = list(
            "DATA;\n#1=IFCWALL('0abc',#2,'It''s');\nENDSEC;\n",
            Some(("IFC4", &schema)),
            None,
        );

        assert_eq!(
            serde_json::to_value(&listed).unwrap(),
            json!({
                "schema": "IFC4",
                "total": 1,
                "instances": [{
                    "id": 1,
                    "entity": "IFCWALL",
                    "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 30 } },
                    "expectedAttributeCount": 3,
                    "attributes": [
                        { "name": "GlobalId", "value": { "kind": "string", "value": "0abc",
                            "range": { "start": { "line": 1, "character": 11 }, "end": { "line": 1, "character": 17 } } } },
                        { "name": "OwnerHistory", "value": { "kind": "reference", "id": 2,
                            "range": { "start": { "line": 1, "character": 18 }, "end": { "line": 1, "character": 20 } } } },
                        { "name": "Name", "value": { "kind": "string", "value": "It's",
                            "range": { "start": { "line": 1, "character": 21 }, "end": { "line": 1, "character": 28 } } } },
                    ],
                }],
            })
        );
    }

    #[test]
    fn limit_lists_only_the_first_instances_but_counts_all() {
        let listed = list(
            "DATA;\n#1=IFCWALL($);\n#2=IFCWALL($);\n#3=IFCWALL($);\nENDSEC;\n",
            None,
            Some(2),
        );

        assert_eq!(listed.total, 3);
        let ids: Vec<Option<u32>> = listed
            .instances
            .iter()
            .map(|instance| instance.id)
            .collect();
        assert_eq!(ids, [Some(1), Some(2)]);
    }

    #[test]
    fn unknown_schema_lists_values_without_names() {
        let listed = list("DATA;\n#1=IFCWALL('0abc',$);\nENDSEC;\n", None, None);

        assert_eq!(listed.schema, None);
        let instance = &listed.instances[0];
        assert_eq!(instance.expected_attribute_count, None);
        assert_eq!(kinds(instance), ["\"string\"", "\"null\""]);
        assert!(
            instance
                .attributes
                .iter()
                .all(|attribute| attribute.name.is_none())
        );
    }

    #[test]
    fn entity_missing_from_the_schema_has_no_expected_count() {
        let schema = schema();
        let listed = list(
            "DATA;\n#1=IFCDOOR($);\nENDSEC;\n",
            Some(("IFC4", &schema)),
            None,
        );

        assert_eq!(listed.instances[0].expected_attribute_count, None);
        assert_eq!(listed.instances[0].attributes[0].name, None);
    }

    #[test]
    fn keeps_the_position_of_a_value_the_parser_drops() {
        let listed = list(
            "DATA;\n#30=IFCWALLTYPE('id',$,'Name',$,$,$,$,$,$,STANDARD);\nENDSEC;\n",
            None,
            None,
        );

        let instance = &listed.instances[0];
        assert_eq!(instance.attributes.len(), 10);
        assert_eq!(kinds(instance)[2], "\"string\"");
        assert_eq!(kinds(instance)[8], "\"null\"");
        assert_eq!(
            instance.attributes[9].value,
            Value {
                kind: ValueKind::Unknown,
                range: Range::new(Position::new(1, 42), Position::new(1, 50)),
            }
        );
    }

    #[test]
    fn reads_nested_lists_typed_values_and_empty_lists() {
        let listed = list(
            "DATA;\n#1=IFCX('a,(b', (#2,#3), IFCLABEL('x'), (), .T.);\nENDSEC;\n",
            None,
            None,
        );

        assert_eq!(
            kinds(&listed.instances[0]),
            [
                "\"string\"",
                "\"list\"",
                "\"typed\"",
                "\"list\"",
                "\"enumeration\""
            ]
        );
    }

    #[test]
    fn an_empty_argument_list_has_no_values() {
        let listed = list("DATA;\n#1=IFCX();\nENDSEC;\n", None, None);

        assert!(listed.instances[0].attributes.is_empty());
    }
}
