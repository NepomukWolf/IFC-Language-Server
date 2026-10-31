use crate::document::{EntityInstanceCollection, for_each_lazy_entity_instance};
use crate::step::Document;

/// Every entity instance with all of its parameters.
pub fn build_entity_instances(lazy: Option<&Document>, text: &str) -> EntityInstanceCollection {
    let mut instances = Vec::new();
    if let Some(lazy) = lazy {
        for_each_lazy_entity_instance(lazy, text, |instance| instances.push(instance));
    }
    EntityInstanceCollection { instances }
}
