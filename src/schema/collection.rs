//! In-memory collection of loaded IFC schema docs by schema name.
//! The backend owns one collection and uses it for both hover and datatype diagnostics.
//! Startup loading is best-effort: failures are recorded instead of panicking.

use std::collections::HashMap;
use std::sync::Arc;

use crate::schema::loader::load_official_schema;
use crate::schema::{EntityDoc, IfcVersion, SchemaDoc, normalize_name};

#[derive(Debug, Default)]
pub struct SchemaDocCollection {
    pub docs: HashMap<String, Arc<SchemaDoc>>,
    load_errors: Vec<String>,
}

impl SchemaDocCollection {
    pub fn new() -> Self {
        let mut collection = Self::default();

        for version in IfcVersion::supported() {
            match load_official_schema(version) {
                Ok(schema) => {
                    collection.insert(version.schema_name(), schema);
                }
                Err(error) => {
                    collection
                        .load_errors
                        .push(format!("{}: {}", version, error));
                }
            }
        }

        collection
    }

    /// Test that the schema doc collection is empty by default.
    #[cfg(test)]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Test that the schema doc collection can be constructed from a list of schema docs.
    #[cfg(test)]
    pub fn from_docs(docs: impl IntoIterator<Item = (String, SchemaDoc)>) -> Self {
        Self {
            docs: docs
                .into_iter()
                .map(|(name, schema)| (name, Arc::new(schema)))
                .collect(),
            load_errors: Vec::new(),
        }
    }

    pub fn insert(&mut self, schema_name: &str, schema: SchemaDoc) {
        self.docs
            .insert(normalize_name(schema_name), Arc::new(schema));
    }

    pub fn get(&self, schema_name: &str) -> Option<&SchemaDoc> {
        self.docs.get(&normalize_name(schema_name)).map(Arc::as_ref)
    }

    pub fn get_shared(&self, schema_name: &str) -> Option<Arc<SchemaDoc>> {
        self.docs.get(&normalize_name(schema_name)).cloned()
    }

    pub fn get_entity_doc(&self, schema_name: &str, entity_name: &str) -> Option<&EntityDoc> {
        self.get(schema_name)?.entity(entity_name)
    }

    pub fn load_errors(&self) -> &[String] {
        &self.load_errors
    }
}
