// `ifc/instances` uses byte columns for compatibility with its wire format, while the document
// and decoded parser values use UTF-16 columns.

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::{Position, Range, Url};

use crate::document::{LinePoints, ParameterValue, decode_entity_instance};
use crate::schema::SchemaDoc;

use super::step_scan::string_literal;

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

/// Counts readable statements and decodes only the requested prefix, labelled with `schema`.
pub fn instances(
    text: &str,
    line_offsets: &[usize],
    parsed: &crate::step::Document,
    schema: Option<(&str, &SchemaDoc)>,
    limit: Option<usize>,
) -> Instances {
    let mut source = Source {
        text,
        line_offsets,
        cursor: Position::new(0, 0),
        offset: 0,
    };
    let mut points = LinePoints {
        text: text.as_bytes(),
        offset: 0,
        row: 0,
        column: 0,
    };
    let mut total = 0;
    let mut instances = Vec::new();
    for (index, statement) in parsed.statements.iter().enumerate() {
        if statement.name.end == 0 {
            continue;
        }
        if instances.len() >= limit.unwrap_or(usize::MAX) {
            total += usize::from(parsed.has_parameters(text.as_bytes(), index));
            continue;
        }
        let Some(instance) = decode_entity_instance(parsed, text, index, &mut points)
            .filter(|instance| instance.parameter_list_range.is_some())
        else {
            continue;
        };
        total += 1;
        let start = source.byte_position(instance.entity_range.start);
        let attribute_names = schema
            .and_then(|(_, schema)| schema.entity(&instance.entity_name))
            .map(|entity| &entity.attributes);
        let attributes = instance
            .parameters
            .iter()
            .enumerate()
            .map(|(index, parameter)| Attribute {
                name: attribute_names
                    .and_then(|attributes| attributes.get(index))
                    .map(|attribute| attribute.name.clone()),
                value: source.value(parameter),
            })
            .collect();
        instances.push(Instance {
            id: instance.id,
            entity: instance.entity_name.clone(),
            range: Range::new(start, source.byte_position(instance.entity_range.end)),
            expected_attribute_count: attribute_names.map(Vec::len),
            attributes,
        });
    }

    Instances {
        schema: schema.map(|(name, _)| name.to_string()),
        total,
        instances,
    }
}

struct Source<'a> {
    text: &'a str,
    line_offsets: &'a [usize],
    cursor: Position,
    offset: usize,
}

impl Source<'_> {
    fn value(&mut self, parameter: &ParameterValue) -> Value {
        let range = parameter.range();
        let start = self.byte_position(range.start);
        let start_offset = self.offset;
        let kind = match parameter {
            ParameterValue::Reference { id, .. } => ValueKind::Reference { id: *id },
            ParameterValue::Enumeration { value, .. } => ValueKind::Enumeration {
                value: value.clone(),
            },
            ParameterValue::String { .. } | ParameterValue::Binary { .. } => ValueKind::String {
                value: string_literal(self.text, start_offset),
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
            ParameterValue::Unknown { .. } | ParameterValue::Malformed { .. } => ValueKind::Unknown,
        };
        Value {
            kind,
            range: Range::new(start, self.byte_position(range.end)),
        }
    }

    fn byte_position(&mut self, position: Position) -> Position {
        assert!(
            position >= self.cursor,
            "decoded positions are in source order"
        );
        let line_start = *self
            .line_offsets
            .get(position.line as usize)
            .expect("decoded range lies in text");
        if position.line != self.cursor.line {
            self.offset = line_start;
            self.cursor = Position::new(position.line, 0);
        }
        let end = self
            .line_offsets
            .get(position.line as usize + 1)
            .map_or(self.text.len(), |next| next - 1);
        let mut characters = self.text[self.offset..end].chars();
        while self.cursor.character < position.character {
            let character = characters.next().expect("decoded range lies in text");
            self.cursor.character += character.len_utf16() as u32;
            self.offset += character.len_utf8();
        }
        assert_eq!(
            self.cursor, position,
            "decoded range lies on a character boundary"
        );
        Position::new(position.line, (self.offset - line_start) as u32)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::document::Document;
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
            all_supertypes: Default::default(),
        };
        SchemaDoc {
            entities: [("IFCWALL".to_string(), wall)].into_iter().collect(),
            types: Default::default(),
        }
    }

    fn list(text: &str, schema: Option<(&str, &SchemaDoc)>, limit: Option<usize>) -> Instances {
        let document = Document::new_unloaded(text.to_string());
        instances(
            &document.text,
            &document.line_offsets,
            document.statement_index.as_deref().unwrap(),
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
    fn preserves_byte_columns_and_values_after_non_ascii_text() {
        let listed = list(
            "DATA;\n#1=IFCX('é😀',(#2),IFCLABEL('ö'),STANDARD,$);\n#2=IFCX($);\nENDSEC;\n",
            None,
            None,
        );
        let first = &listed.instances[0];
        assert_eq!(
            first.range,
            Range::new(Position::new(1, 0), Position::new(1, 49))
        );
        assert_eq!(first.attributes.len(), 5);
        assert_eq!(
            first.attributes[0].value,
            Value {
                kind: ValueKind::String {
                    value: Some("é😀".to_string())
                },
                range: Range::new(Position::new(1, 8), Position::new(1, 16)),
            }
        );
        let ValueKind::List { items } = &first.attributes[1].value.kind else {
            panic!("expected reference list");
        };
        assert_eq!(
            items[0],
            Value {
                kind: ValueKind::Reference { id: 2 },
                range: Range::new(Position::new(1, 18), Position::new(1, 20)),
            }
        );
        let ValueKind::Typed { items, .. } = &first.attributes[2].value.kind else {
            panic!("expected typed string");
        };
        assert_eq!(
            items[0],
            Value {
                kind: ValueKind::String {
                    value: Some("ö".to_string())
                },
                range: Range::new(Position::new(1, 31), Position::new(1, 35)),
            }
        );
        assert_eq!(first.attributes[3].value.kind, ValueKind::Unknown);
        assert_eq!(first.attributes[4].value.kind, ValueKind::Null);
        assert_eq!(listed.instances[1].range.start, Position::new(2, 0));
    }

    #[test]
    fn limits_decode_work_without_changing_the_readable_total() {
        let text = "DATA;\n#1=IFCX();\n#2=IFCX('unclosed);\n#3=IFCWALL('g',$,'wall');\n#4=IFCX(#1;\n#5=IFCX('broken);\nENDSEC;";
        for limit in [0, 1, 2, 10] {
            crate::document::DECODED_STATEMENTS.with(|count| count.set(0));
            let listed = list(text, Some(("IFC4", &schema())), Some(limit));
            let decoded = crate::document::DECODED_STATEMENTS.with(|count| count.get());
            assert_eq!(listed.total, 3);
            assert_eq!(listed.instances.len(), limit.min(3));
            assert_eq!(listed.schema.as_deref(), Some("IFC4"));
            if limit <= 1 {
                assert_eq!(decoded, limit);
            }
            if limit >= 2 {
                assert_eq!(
                    listed.instances[1].attributes[2].name.as_deref(),
                    Some("Name")
                );
            }
        }
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
    fn binary_commas_preserve_attribute_positions() {
        let schema = schema();
        let listed = list(
            "DATA;\n#1=IFCWALL(\"0,1\",$);\nENDSEC;\n",
            Some(("IFC4", &schema)),
            None,
        );
        let attributes = &listed.instances[0].attributes;
        assert_eq!(attributes.len(), 2);
        assert_eq!(attributes[0].name.as_deref(), Some("GlobalId"));
        assert_eq!(attributes[0].value.kind, ValueKind::Unknown);
        let binary = list("#1=IFCWALL(\"0F\",$);", Some(("IFC4", &schema)), None);
        assert_eq!(
            binary.instances[0].attributes[0].value.kind,
            ValueKind::String { value: None }
        );
        assert_eq!(attributes[1].name.as_deref(), Some("OwnerHistory"));
        assert_eq!(attributes[1].value.kind, ValueKind::Null);
        assert_eq!(
            attributes[1].value.range,
            Range::new(Position::new(1, 17), Position::new(1, 18))
        );
    }

    #[test]
    fn an_empty_argument_list_has_no_values() {
        let listed = list("DATA;\n#1=IFCX();\nENDSEC;\n", None, None);

        assert!(listed.instances[0].attributes.is_empty());
    }
}
