use std::collections::HashMap;

use crate::document::Document;
use crate::schema::SchemaDoc;

use super::step_scan::{ScannedInstance, string_literal};

#[derive(Default)]
pub(super) struct Label {
    pub(super) global_id: Option<String>,
    pub(super) name: Option<String>,
}

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

    pub(super) fn read_scanned(&mut self, instance: &ScannedInstance) -> Label {
        let (global_id_index, name_index) = self.attribute_indexes(&instance.entity_name);
        let text = self.document.text.as_str();
        let string_at = |index: Option<usize>| {
            let &(start, end) = instance.arguments.get(index?)?;
            string_literal(&text[..end], start)
        };
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
