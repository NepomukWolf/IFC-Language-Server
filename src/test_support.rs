use crate::document::{EntityInstanceCollection, for_each_entity_instance};
use crate::step::Document;

/// Every entity instance with all of its parameters.
pub fn build_entity_instances(parsed: Option<&Document>, text: &str) -> EntityInstanceCollection {
    let mut instances = Vec::new();
    if let Some(parsed) = parsed {
        for_each_entity_instance(parsed, text, |instance| instances.push(instance));
    }
    EntityInstanceCollection { instances }
}
