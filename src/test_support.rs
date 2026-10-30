use crate::document::{EntityInstanceCollection, for_each_entity_node, parse_entity_instance};
use crate::document_tree::DocumentTree;

/// Every entity instance with all of its parameters.
pub fn build_entity_instances(tree: Option<&DocumentTree>, text: &str) -> EntityInstanceCollection {
    let mut instances = Vec::new();
    for_each_entity_node(tree, |node| {
        instances.extend(parse_entity_instance(node, text));
    });
    EntityInstanceCollection { instances }
}
