//! Tree-sitter helpers for locating STEP instance and parameter context around a cursor node.

use crate::step::compact::{self, CompactKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterContext {
    pub instance_id: u32,
    pub entity_name: String,
    pub parameter_index: usize,
}

/// `offset` is the byte offset `node` was looked up at.
pub fn parameter_context(
    node: tree_sitter::Node<'_>,
    offset: usize,
    text: &str,
) -> Option<ParameterContext> {
    let entity_instance = ancestor_with_kind(node, "entity_instance")?;
    let parameter_index = if let Some(start) = compact::parameters_start(node, text) {
        compact::parameters(text, start)
            .iter()
            .position(|parameter| parameter.start <= offset && offset < parameter.end)?
    } else {
        let parameter_sequence = entity_parameter_sequence(entity_instance)?;
        parameter_sequence
            .named_children(&mut parameter_sequence.walk())
            .filter(|child| child.kind() == "parameter")
            .position(|parameter| contains_node(parameter, node))?
    };

    Some(ParameterContext {
        instance_id: parse_instance_id(entity_instance, text)?,
        entity_name: parse_entity_name(entity_instance, text)?,
        parameter_index,
    })
}

/// Byte span of the omitted value (`*`) at `offset`, looked up as `node`.
pub fn omitted_value_span(
    node: tree_sitter::Node<'_>,
    offset: usize,
    text: &str,
) -> Option<(usize, usize)> {
    if node.kind() == "omitted_value" {
        return Some((node.start_byte(), node.end_byte()));
    }
    let parameters = compact::parameters(text, compact::parameters_start(node, text)?);
    let value = *compact::values_at(&parameters, offset).last()?;
    (value.kind == CompactKind::Omitted).then_some((value.start, value.end))
}

/// Byte span of a typed value's type name (`IFCLABEL` in `IFCLABEL('x')`) when `offset`, looked
/// up as `node`, is inside that name.
pub fn typed_value_name_span(
    node: tree_sitter::Node<'_>,
    offset: usize,
    text: &str,
) -> Option<(usize, usize)> {
    let (start, end) = if let Some(parameters_start) = compact::parameters_start(node, text) {
        let parameters = compact::parameters(text, parameters_start);
        compact::values_at(&parameters, offset)
            .into_iter()
            .rev()
            .find_map(|value| match value.kind {
                CompactKind::Typed { name_end, .. } => Some((value.start, name_end)),
                _ => None,
            })?
    } else {
        let typed_parameter = ancestor_with_kind(node, "typed_parameter")?;
        let entity_name = child_with_kind(typed_parameter, "entity_name")?;
        (entity_name.start_byte(), entity_name.end_byte())
    };
    (start <= offset && offset < end).then_some((start, end))
}

/// Walks up from `node` to the nearest ancestor (or `node` itself) with `expected_kind`.
fn ancestor_with_kind<'tree>(
    mut node: tree_sitter::Node<'tree>,
    expected_kind: &str,
) -> Option<tree_sitter::Node<'tree>> {
    loop {
        if node.kind() == expected_kind {
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// First direct child of `node` with `expected_kind`.
fn child_with_kind<'tree>(
    node: tree_sitter::Node<'tree>,
    expected_kind: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == expected_kind)
}

fn parse_instance_id(node: tree_sitter::Node<'_>, text: &str) -> Option<u32> {
    header_text(node, "instance_id", text)
        .and_then(|value| value.trim_start_matches('#').parse::<u32>().ok())
}

fn parse_entity_name(node: tree_sitter::Node<'_>, text: &str) -> Option<String> {
    header_text(node, "entity_name", text).map(|value| value.to_ascii_uppercase())
}

fn header_text<'a>(
    entity_instance: tree_sitter::Node<'_>,
    kind: &str,
    text: &'a str,
) -> Option<&'a str> {
    if compact::is_compact_entity(entity_instance) {
        let entity = compact::entity(text, entity_instance.start_byte());
        let (start, end) = if kind == "instance_id" {
            entity.id
        } else {
            entity.name
        };
        return text.get(start..end);
    }
    child_with_kind(entity_instance, kind).and_then(|child| child.utf8_text(text.as_bytes()).ok())
}

fn entity_parameter_sequence(
    entity_instance: tree_sitter::Node<'_>,
) -> Option<tree_sitter::Node<'_>> {
    let mut cursor = entity_instance.walk();
    let parameter_list = entity_instance
        .named_children(&mut cursor)
        .find(|child| child.kind() == "parameter_list")?;
    let mut cursor = parameter_list.walk();
    parameter_list
        .named_children(&mut cursor)
        .find(|child| child.kind() == "parameter_sequence")
}

fn contains_node(outer: tree_sitter::Node<'_>, inner: tree_sitter::Node<'_>) -> bool {
    outer.start_byte() <= inner.start_byte() && inner.end_byte() <= outer.end_byte()
}
