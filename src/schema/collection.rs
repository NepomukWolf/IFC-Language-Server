//! In-memory collection of loaded IFC schema docs by schema name.
//! The backend owns one collection and uses it for both hover and datatype diagnostics.
//! Bundled official schemas are parsed on first lookup and cached in `OfficialSchemas`, which
//! collections share; schema docs inserted into a collection take precedence over them.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, OnceLock};

use tracing::{info, warn};

use crate::schema::loader::load_official_schema;
use crate::schema::{EntityDoc, IfcVersion, SchemaDoc, normalize_name};

type OfficialSchemaLoader = dyn Fn(IfcVersion) -> Result<SchemaDoc, String> + Send + Sync;

/// Lazily parsed bundled official schemas, one slot per `IfcVersion::supported()` entry.
/// Each slot is filled at most once, including when loading fails.
pub struct OfficialSchemas {
    slots: [OnceLock<Result<Arc<SchemaDoc>, String>>; IfcVersion::SUPPORTED_COUNT],
    loader: Box<OfficialSchemaLoader>,
}

impl OfficialSchemas {
    /// The process-wide cache of the schemas bundled into the binary.
    pub fn bundled() -> Arc<Self> {
        static BUNDLED: LazyLock<Arc<OfficialSchemas>> = LazyLock::new(|| {
            Arc::new(OfficialSchemas::with_loader(|version| {
                load_official_schema(version).map_err(|error| error.to_string())
            }))
        });
        Arc::clone(&BUNDLED)
    }

    pub fn with_loader(
        loader: impl Fn(IfcVersion) -> Result<SchemaDoc, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            slots: [const { OnceLock::new() }; IfcVersion::SUPPORTED_COUNT],
            loader: Box::new(loader),
        }
    }

    /// `None` when `schema_name` is not an official schema name.
    fn load(&self, schema_name: &str) -> Option<&Result<Arc<SchemaDoc>, String>> {
        let versions = IfcVersion::supported();
        let index = versions
            .iter()
            .position(|version| version.schema_name() == schema_name)?;
        let version = versions[index];
        Some(
            self.slots[index].get_or_init(|| match (self.loader)(version) {
                Ok(schema) => {
                    info!(schema_name, "loaded bundled schema documentation");
                    Ok(Arc::new(schema))
                }
                Err(error) => {
                    warn!(
                        schema_name,
                        error, "failed to load bundled schema documentation"
                    );
                    Err(error)
                }
            }),
        )
    }
}

pub struct SchemaDocCollection {
    docs: HashMap<String, Arc<SchemaDoc>>,
    official: Option<Arc<OfficialSchemas>>,
}

impl SchemaDocCollection {
    pub fn new() -> Self {
        Self::with_official(OfficialSchemas::bundled())
    }

    pub fn with_official(official: Arc<OfficialSchemas>) -> Self {
        Self {
            docs: HashMap::new(),
            official: Some(official),
        }
    }

    /// An empty collection backed by the same official schemas as `self`.
    pub fn sharing_official(&self) -> Self {
        Self {
            docs: HashMap::new(),
            official: self.official.clone(),
        }
    }

    /// Test that the schema doc collection is empty by default.
    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            docs: HashMap::new(),
            official: None,
        }
    }

    /// Test that the schema doc collection can be constructed from a list of schema docs.
    #[cfg(test)]
    pub fn from_docs(docs: impl IntoIterator<Item = (String, SchemaDoc)>) -> Self {
        Self {
            docs: docs
                .into_iter()
                .map(|(name, schema)| (name, Arc::new(schema)))
                .collect(),
            official: None,
        }
    }

    pub fn insert(&mut self, schema_name: &str, schema: SchemaDoc) {
        self.docs
            .insert(normalize_name(schema_name), Arc::new(schema));
    }

    pub fn get(&self, schema_name: &str) -> Option<&SchemaDoc> {
        self.lookup(schema_name).map(Arc::as_ref)
    }

    pub fn get_shared(&self, schema_name: &str) -> Option<Arc<SchemaDoc>> {
        self.lookup(schema_name).cloned()
    }

    pub fn get_entity_doc(&self, schema_name: &str, entity_name: &str) -> Option<&EntityDoc> {
        self.get(schema_name)?.entity(entity_name)
    }

    /// The error from loading the bundled official schema `schema_name`, if it failed and no
    /// inserted schema doc of that name replaces it.
    pub fn official_schema_error(&self, schema_name: &str) -> Option<&str> {
        let schema_name = normalize_name(schema_name);
        if self.docs.contains_key(&schema_name) {
            return None;
        }
        self.official
            .as_ref()?
            .load(&schema_name)?
            .as_ref()
            .err()
            .map(String::as_str)
    }

    fn lookup(&self, schema_name: &str) -> Option<&Arc<SchemaDoc>> {
        let schema_name = normalize_name(schema_name);
        if let Some(schema) = self.docs.get(&schema_name) {
            return Some(schema);
        }
        self.official.as_ref()?.load(&schema_name)?.as_ref().ok()
    }
}

/// Official schemas whose loader records each version it is asked to parse and answers with
/// `result`.
#[cfg(test)]
pub(crate) fn recording_official_schemas(
    result: fn(IfcVersion) -> Result<SchemaDoc, String>,
) -> (Arc<OfficialSchemas>, Arc<std::sync::Mutex<Vec<IfcVersion>>>) {
    let parsed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::clone(&parsed);
    let official = OfficialSchemas::with_loader(move |version| {
        recorder.lock().unwrap().push(version);
        result(version)
    });
    (Arc::new(official), parsed)
}

//*----- TESTS BEGIN HERE -----*
#[cfg(test)]
mod tests {
    use super::*;

    fn empty_schema() -> SchemaDoc {
        SchemaDoc {
            entities: HashMap::new(),
            types: HashMap::new(),
        }
    }

    #[test]
    fn looking_up_one_official_schema_parses_only_that_schema() {
        let (official, parsed) = recording_official_schemas(|_| Ok(empty_schema()));
        let collection = SchemaDocCollection::with_official(official);

        assert!(collection.get("ifc4").is_some());

        assert_eq!(*parsed.lock().unwrap(), [IfcVersion::Ifc4Add2Tc1]);
    }

    #[test]
    fn repeated_lookups_across_shared_collections_parse_once() {
        let (official, parsed) = recording_official_schemas(|_| Ok(empty_schema()));
        let first = SchemaDocCollection::with_official(official);
        let second = first.sharing_official();

        let from_first = first.get_shared("IFC4").unwrap();
        let from_second = second.get_shared("IFC4").unwrap();
        assert!(first.get_entity_doc("IFC4", "IFCWALL").is_none());

        assert!(Arc::ptr_eq(&from_first, &from_second));
        assert_eq!(*parsed.lock().unwrap(), [IfcVersion::Ifc4Add2Tc1]);
    }

    #[test]
    fn unknown_schema_name_parses_nothing() {
        let (official, parsed) = recording_official_schemas(|_| Ok(empty_schema()));
        let collection = SchemaDocCollection::with_official(official);

        assert!(collection.get("IFC5").is_none());
        assert!(collection.official_schema_error("IFC5").is_none());

        assert!(parsed.lock().unwrap().is_empty());
    }

    #[test]
    fn inserted_schema_takes_precedence_over_official_schema() {
        let (official, parsed) = recording_official_schemas(|_| Ok(empty_schema()));
        let mut collection = SchemaDocCollection::with_official(official);
        let mut local = empty_schema();
        local.entities.insert(
            "IFCLOCALONLY".to_string(),
            EntityDoc {
                name: "IfcLocalOnly".to_string(),
                attributes: Vec::new(),
                url: String::new(),
                all_supertypes: Default::default(),
            },
        );
        collection.insert("IFC4", local);

        assert!(collection.get_entity_doc("IFC4", "IfcLocalOnly").is_some());
        assert!(parsed.lock().unwrap().is_empty());
    }

    #[test]
    fn failed_official_schema_reports_error_and_is_not_retried() {
        let (official, parsed) = recording_official_schemas(|_| Err("broken".to_string()));
        let collection = SchemaDocCollection::with_official(official);

        assert!(collection.get("IFC4").is_none());
        assert!(collection.get("IFC4").is_none());
        assert_eq!(collection.official_schema_error("IFC4"), Some("broken"));

        assert_eq!(*parsed.lock().unwrap(), [IfcVersion::Ifc4Add2Tc1]);
    }
}
