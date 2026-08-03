//! Parsed in-memory representation of one IFC document.
//! This module owns the source text, cheap text indexes for navigation, and optional tree-sitter
//! state for schema-aware diagnostics and derived-value hover.

use std::collections::HashMap;

use tower_lsp::lsp_types::{Position, Range, TextDocumentContentChangeEvent};
use tree_sitter::{InputEdit, Node, Parser, Point, Tree, TreeCursor};

use crate::document_index::{TextIndex, scan_text};

pub const DEFAULT_AST_FILE_SIZE_LIMIT_BYTES: usize = 70 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum ApplyChangeError {
    InvalidRange(Range),
}

#[derive(Debug)]
pub struct Document {
    pub text: String,
    pub tree: Option<Tree>,
    pub ast_skipped: bool,
    pub schema_name: Option<String>,
    pub line_offsets: Vec<usize>,
    pub definitions: HashMap<u32, usize>,
    pub references: HashMap<u32, Vec<usize>>,
    pub instances: Vec<EntityInstanceInfo>,
    pub(crate) instance_indexes_by_id: HashMap<u32, usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityInstanceInfo {
    pub id: Option<u32>,
    pub id_range: Option<Range>,
    pub entity_name: String,
    pub entity_name_range: Range,
    pub entity_range: Range,
    pub parameter_list_range: Option<Range>,
    pub parameters: Vec<ParameterValue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParameterValue {
    Reference {
        id: u32,
        range: Range,
    },
    Enumeration {
        value: String,
        range: Range,
    },
    String {
        range: Range,
    },
    Number {
        text: String,
        range: Range,
    },
    Null {
        range: Range,
    },
    Omitted {
        range: Range,
    },
    List {
        items: Vec<ParameterValue>,
        range: Range,
    },
    Typed {
        type_name: String,
        inner: Vec<ParameterValue>,
        range: Range,
    },
    Unknown {
        range: Range,
    },
}

impl ParameterValue {
    pub fn range(&self) -> Range {
        match self {
            ParameterValue::Reference { range, .. }
            | ParameterValue::Enumeration { range, .. }
            | ParameterValue::String { range }
            | ParameterValue::Number { range, .. }
            | ParameterValue::Null { range }
            | ParameterValue::Omitted { range }
            | ParameterValue::List { range, .. }
            | ParameterValue::Typed { range, .. }
            | ParameterValue::Unknown { range } => *range,
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            ParameterValue::Reference { .. } => "reference",
            ParameterValue::Enumeration { .. } => "enumeration",
            ParameterValue::String { .. } => "string",
            ParameterValue::Number { .. } => "number",
            ParameterValue::Null { .. } => "null",
            ParameterValue::Omitted { .. } => "omitted value",
            ParameterValue::List { .. } => "aggregate",
            ParameterValue::Typed { .. } => "typed value",
            ParameterValue::Unknown { .. } => "unknown value",
        }
    }
}

impl Document {
    pub fn new_unloaded(text: String) -> Self {
        let TextIndex {
            line_offsets,
            definitions,
            references,
            schema_name,
        } = scan_text(&text);

        Self {
            text,
            tree: None,
            ast_skipped: false,
            schema_name: None,
            line_offsets,
            definitions,
            references,
            instances: Vec::new(),
            instance_indexes_by_id: HashMap::new(),
        }
        .with_schema_name(schema_name)
    }

    fn with_schema_name(mut self, schema_name: Option<String>) -> Self {
        self.schema_name = schema_name;
        self
    }

    pub fn reload_text_index(&mut self) {
        let TextIndex {
            line_offsets,
            definitions,
            references,
            schema_name,
        } = scan_text(&self.text);

        self.line_offsets = line_offsets;
        self.definitions = definitions;
        self.references = references;
        self.schema_name = schema_name;
    }

    pub fn unload_parse_state(&mut self) {
        self.tree = None;
        self.ast_skipped = false;
        self.instances = Vec::new();
        self.instance_indexes_by_id = HashMap::new();
    }

    pub fn reload_parse_state(&mut self, parser: &mut Parser, ast_file_size_limit_bytes: usize) {
        self.reload_text_index();
        self.unload_parse_state();

        self.parse_current_text(parser, None, ast_file_size_limit_bytes);
    }

    pub fn apply_content_changes(
        &mut self,
        parser: &mut Parser,
        changes: &[TextDocumentContentChangeEvent],
        ast_file_size_limit_bytes: usize,
    ) -> Result<(), ApplyChangeError> {
        for (index, change) in changes.iter().enumerate() {
            if let Some(range) = change.range {
                if let Err(error) = self.apply_ranged_change(range, &change.text) {
                    self.reload_text_index();
                    let old_tree = self.tree.take();
                    self.parse_current_text(parser, old_tree.as_ref(), ast_file_size_limit_bytes);
                    return Err(error);
                }
            } else {
                self.text.clone_from(&change.text);
                self.tree = None;
            }

            if index + 1 < changes.len() {
                self.reload_line_offsets();
            }
        }

        self.reload_text_index();
        let old_tree = self.tree.take();
        self.parse_current_text(parser, old_tree.as_ref(), ast_file_size_limit_bytes);
        Ok(())
    }

    fn apply_ranged_change(
        &mut self,
        range: Range,
        replacement: &str,
    ) -> Result<(), ApplyChangeError> {
        let start_byte = self
            .position_to_offset(range.start)
            .ok_or(ApplyChangeError::InvalidRange(range))?;
        let old_end_byte = self
            .position_to_offset(range.end)
            .ok_or(ApplyChangeError::InvalidRange(range))?;
        if start_byte > old_end_byte {
            return Err(ApplyChangeError::InvalidRange(range));
        }

        let start_position = self
            .point_at_offset(range.start.line as usize, start_byte)
            .ok_or(ApplyChangeError::InvalidRange(range))?;
        let old_end_position = self
            .point_at_offset(range.end.line as usize, old_end_byte)
            .ok_or(ApplyChangeError::InvalidRange(range))?;
        let new_end_position = point_after_text(start_position, replacement);
        let edit = InputEdit {
            start_byte,
            old_end_byte,
            new_end_byte: start_byte + replacement.len(),
            start_position,
            old_end_position,
            new_end_position,
        };

        if let Some(tree) = self.tree.as_mut() {
            tree.edit(&edit);
        }
        self.text
            .replace_range(start_byte..old_end_byte, replacement);
        Ok(())
    }

    fn reload_line_offsets(&mut self) {
        self.line_offsets.clear();
        self.line_offsets.push(0);
        self.line_offsets.extend(
            self.text
                .bytes()
                .enumerate()
                .filter_map(|(offset, byte)| (byte == b'\n').then_some(offset + 1)),
        );
    }

    fn point_at_offset(&self, line: usize, offset: usize) -> Option<Point> {
        let line_start = *self.line_offsets.get(line)?;
        Some(Point {
            row: line,
            column: offset.checked_sub(line_start)?,
        })
    }

    fn parse_current_text(
        &mut self,
        parser: &mut Parser,
        old_tree: Option<&Tree>,
        ast_file_size_limit_bytes: usize,
    ) {
        self.ast_skipped = false;
        self.instances.clear();
        self.instance_indexes_by_id.clear();

        if self.text.len() > ast_file_size_limit_bytes {
            self.ast_skipped = true;
            return;
        }

        self.tree = parser.parse(&self.text, old_tree);
        self.instances = build_instances(&self.tree, &self.text);
        self.instance_indexes_by_id = self
            .instances
            .iter()
            .enumerate()
            .filter_map(|(index, instance)| instance.id.map(|id| (id, index)))
            .collect();
    }

    #[cfg(test)]
    pub fn parse(parser: &mut Parser, text: String) -> Self {
        let mut document = Self::new_unloaded(text);
        document.reload_parse_state(parser, DEFAULT_AST_FILE_SIZE_LIMIT_BYTES);
        document
    }

    pub fn is_parse_state_loaded(&self) -> bool {
        self.tree.is_some() || self.ast_skipped
    }

    pub fn has_ast(&self) -> bool {
        self.tree.is_some()
    }

    pub fn node_at_position(&self, position: Position) -> Option<Node<'_>> {
        let tree = self.tree.as_ref()?;
        let offset = self.position_to_offset(position)?;
        let line_start = *self.line_offsets.get(position.line as usize)?;
        let point = Point {
            row: position.line as usize,
            column: offset.checked_sub(line_start)?,
        };

        tree.root_node().descendant_for_point_range(point, point)
    }

    pub fn instance_by_id(&self, id: u32) -> Option<&EntityInstanceInfo> {
        self.instance_indexes_by_id
            .get(&id)
            .and_then(|index| self.instances.get(*index))
    }

    pub fn position_to_offset(&self, position: Position) -> Option<usize> {
        let line_start = *self.line_offsets.get(position.line as usize)?;
        let line_end = self.line_end_offset(position.line as usize)?;
        let line = self.text.get(line_start..line_end)?;
        let mut utf16_units = 0u32;

        if position.character == 0 {
            return Some(line_start);
        }

        for (byte_offset, character) in line.char_indices() {
            if utf16_units == position.character {
                return Some(line_start + byte_offset);
            }
            utf16_units += character.len_utf16() as u32;
        }

        (utf16_units == position.character).then_some(line_end)
    }

    pub fn offset_to_position(&self, offset: usize) -> Option<Position> {
        if offset > self.text.len() || !self.text.is_char_boundary(offset) {
            return None;
        }

        let line_index = match self.line_offsets.binary_search(&offset) {
            Ok(line) => line,
            Err(next_line) => next_line.checked_sub(1)?,
        };
        let line_start = self.line_offsets[line_index];
        let line_text = self.text.get(line_start..offset)?;
        let character = line_text
            .chars()
            .map(|character| character.len_utf16() as u32)
            .sum();

        Some(Position::new(line_index as u32, character))
    }

    pub fn range_for_offsets(&self, start: usize, end: usize) -> Option<Range> {
        Some(Range {
            start: self.offset_to_position(start)?,
            end: self.offset_to_position(end)?,
        })
    }

    pub fn id_range_at_offset(&self, offset: usize) -> Option<Range> {
        let (start, end, _) = self.id_token_at_offset(offset)?;
        self.range_for_offsets(start, end)
    }

    pub fn definition_range(&self, id: u32) -> Option<Range> {
        self.id_range_at_offset(*self.definitions.get(&id)?)
    }

    pub fn id_token_at_position(&self, position: Position) -> Option<(u32, usize)> {
        let offset = self.position_to_offset(position)?;
        let (start, _, id) = self.id_token_at_offset(offset)?;
        self.references.get(&id)?.contains(&start).then_some(())?;
        Some((id, start))
    }

    pub fn entity_name_at_position(&self, position: Position) -> Option<(String, Range)> {
        let offset = self.position_to_offset(position)?;
        let (start, end) = self.identifier_at_offset(offset)?;
        if !self.is_definition_entity_name(start) {
            return None;
        }
        let text = self.text.get(start..end)?;
        Some((
            text.to_ascii_uppercase(),
            self.range_for_offsets(start, end)?,
        ))
    }

    pub fn entity_instance_text_at_definition(&self, id: u32) -> Option<&str> {
        let definition_offset = *self.definitions.get(&id)?;
        let bytes = self.text.as_bytes();
        let mut start = definition_offset;
        while start > 0 && bytes[start - 1] != b';' {
            start -= 1;
        }
        while start < definition_offset && bytes[start].is_ascii_whitespace() {
            start += 1;
        }

        let mut end = definition_offset;
        while end < bytes.len() && bytes[end] != b';' {
            end += 1;
        }
        if end < bytes.len() {
            end += 1;
        }

        self.text.get(start..end)
    }

    fn line_end_offset(&self, line_index: usize) -> Option<usize> {
        let line_start = *self.line_offsets.get(line_index)?;
        let next_line_start = self
            .line_offsets
            .get(line_index + 1)
            .copied()
            .unwrap_or(self.text.len());
        if next_line_start > line_start
            && self
                .text
                .as_bytes()
                .get(next_line_start - 1)
                .is_some_and(|byte| *byte == b'\n')
        {
            Some(next_line_start - 1)
        } else {
            Some(next_line_start)
        }
    }

    fn id_token_at_offset(&self, offset: usize) -> Option<(usize, usize, u32)> {
        let bytes = self.text.as_bytes();
        if bytes.is_empty() {
            return None;
        }

        let candidate =
            if offset < bytes.len() && (bytes[offset] == b'#' || bytes[offset].is_ascii_digit()) {
                offset
            } else if offset > 0 && bytes[offset - 1].is_ascii_digit() {
                offset - 1
            } else {
                return None;
            };

        let mut start = candidate;
        while start > 0 && bytes[start - 1].is_ascii_digit() {
            start -= 1;
        }
        if bytes.get(start) != Some(&b'#') {
            start = start.checked_sub(1)?;
            if bytes.get(start) != Some(&b'#') {
                return None;
            }
        }

        let mut end = start + 1;
        let mut id = 0u32;
        let mut has_digit = false;
        while let Some(byte) = bytes.get(end).copied()
            && byte.is_ascii_digit()
        {
            has_digit = true;
            id = id.checked_mul(10)?.checked_add((byte - b'0') as u32)?;
            end += 1;
        }

        (has_digit && offset <= end).then_some((start, end, id))
    }

    fn identifier_at_offset(&self, offset: usize) -> Option<(usize, usize)> {
        let bytes = self.text.as_bytes();
        if bytes.is_empty() {
            return None;
        }

        let candidate = if offset < bytes.len() && is_identifier_part(bytes[offset]) {
            offset
        } else if offset > 0 && is_identifier_part(bytes[offset - 1]) {
            offset - 1
        } else {
            return None;
        };

        let mut start = candidate;
        while start > 0 && is_identifier_part(bytes[start - 1]) {
            start -= 1;
        }
        let mut end = candidate + 1;
        while end < bytes.len() && is_identifier_part(bytes[end]) {
            end += 1;
        }

        Some((start, end))
    }

    fn is_definition_entity_name(&self, identifier_start: usize) -> bool {
        let bytes = self.text.as_bytes();
        let mut offset = identifier_start;

        while offset > 0 && bytes[offset - 1].is_ascii_whitespace() {
            offset -= 1;
        }
        if offset == 0 || bytes[offset - 1] != b'=' {
            return false;
        }
        offset -= 1;

        while offset > 0 && bytes[offset - 1].is_ascii_whitespace() {
            offset -= 1;
        }
        let digit_end = offset;
        while offset > 0 && bytes[offset - 1].is_ascii_digit() {
            offset -= 1;
        }

        digit_end > offset && offset > 0 && bytes[offset - 1] == b'#'
    }
}

fn is_identifier_part(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn build_instances(tree: &Option<Tree>, text: &str) -> Vec<EntityInstanceInfo> {
    let mut instances = Vec::new();

    let tree = match tree {
        Some(t) => t,
        None => return instances,
    };

    let mut cursor = tree.root_node().walk();
    traverse(&mut cursor, text, &mut instances);

    instances
}

fn traverse(cursor: &mut TreeCursor, text: &str, instances: &mut Vec<EntityInstanceInfo>) {
    loop {
        let node = cursor.node();

        if node.kind() == "entity_instance"
            && let Some(instance) = parse_entity_instance(node, text)
        {
            instances.push(instance);
        }

        if cursor.goto_first_child() {
            traverse(cursor, text, instances);
            cursor.goto_parent();
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn parse_entity_instance(node: Node<'_>, text: &str) -> Option<EntityInstanceInfo> {
    let mut child_cursor = node.walk();
    let mut id = None;
    let mut id_range = None;
    let mut entity_name = None;
    let mut entity_name_range = None;
    let mut parameter_list_range = None;
    let mut parameters = Vec::new();

    for child in node.children(&mut child_cursor) {
        match child.kind() {
            "instance_id" => {
                let id_text = child.utf8_text(text.as_bytes()).ok()?;
                id = id_text.trim_start_matches('#').parse::<u32>().ok();
                id_range = Some(node_range(&child));
            }
            "entity_name" => {
                let name = child.utf8_text(text.as_bytes()).ok()?;
                entity_name = Some(name.to_ascii_uppercase());
                entity_name_range = Some(node_range(&child));
            }
            "parameter_list" => {
                parameter_list_range = Some(node_range(&child));
                parameters = parse_parameter_list(child, text);
            }
            _ => {}
        }
    }

    Some(EntityInstanceInfo {
        id,
        id_range,
        entity_name: entity_name?,
        entity_name_range: entity_name_range?,
        entity_range: node_range(&node),
        parameter_list_range,
        parameters,
    })
}

fn parse_parameter_list(node: Node<'_>, text: &str) -> Vec<ParameterValue> {
    let mut cursor = node.walk();
    let Some(sequence) = node
        .children(&mut cursor)
        .find(|child| child.kind() == "parameter_sequence")
    else {
        return Vec::new();
    };

    let mut seq_cursor = sequence.walk();
    sequence
        .children(&mut seq_cursor)
        .filter(|child| child.kind() == "parameter")
        .map(|parameter| parse_parameter(parameter, text))
        .collect()
}

fn parse_parameter(node: Node<'_>, text: &str) -> ParameterValue {
    let mut cursor = node.walk();
    let Some(value_node) = node.children(&mut cursor).find(|child| child.is_named()) else {
        return ParameterValue::Unknown {
            range: node_range(&node),
        };
    };

    parse_parameter_value(value_node, text)
}

fn parse_parameter_value(node: Node<'_>, text: &str) -> ParameterValue {
    match node.kind() {
        "reference" => {
            let range = node_range(&node);
            let id = node
                .utf8_text(text.as_bytes())
                .ok()
                .and_then(|value| value.trim_start_matches('#').parse::<u32>().ok());
            match id {
                Some(id) => ParameterValue::Reference { id, range },
                None => ParameterValue::Unknown { range },
            }
        }
        "enumeration" => {
            let range = node_range(&node);
            let value = node
                .utf8_text(text.as_bytes())
                .ok()
                .map(|value| value.trim_matches('.').to_ascii_uppercase())
                .unwrap_or_default();
            ParameterValue::Enumeration { value, range }
        }
        "string" => ParameterValue::String {
            range: node_range(&node),
        },
        "number" => ParameterValue::Number {
            text: node
                .utf8_text(text.as_bytes())
                .unwrap_or_default()
                .to_string(),
            range: node_range(&node),
        },
        "null_value" => ParameterValue::Null {
            range: node_range(&node),
        },
        "omitted_value" => ParameterValue::Omitted {
            range: node_range(&node),
        },
        "list" => {
            let range = node_range(&node);
            let mut cursor = node.walk();
            let items = node
                .children(&mut cursor)
                .find(|child| child.kind() == "parameter_sequence")
                .map(|sequence| {
                    let mut seq_cursor = sequence.walk();
                    sequence
                        .children(&mut seq_cursor)
                        .filter(|child| child.kind() == "parameter")
                        .map(|parameter| parse_parameter(parameter, text))
                        .collect()
                })
                .unwrap_or_default();
            ParameterValue::List { items, range }
        }
        "typed_parameter" => {
            let range = node_range(&node);
            let mut cursor = node.walk();
            let mut type_name = None;
            let mut inner = Vec::new();

            for child in node.children(&mut cursor) {
                match child.kind() {
                    "entity_name" => {
                        type_name = child
                            .utf8_text(text.as_bytes())
                            .ok()
                            .map(|name| name.to_ascii_uppercase());
                    }
                    "parameter_list" => inner = parse_parameter_list(child, text),
                    _ => {}
                }
            }

            ParameterValue::Typed {
                type_name: type_name.unwrap_or_default(),
                inner,
                range,
            }
        }
        _ => ParameterValue::Unknown {
            range: node_range(&node),
        },
    }
}

fn node_range(node: &Node<'_>) -> Range {
    let start = node.start_position();
    let end = node.end_position();

    Range {
        start: Position::new(start.row as u32, start.column as u32),
        end: Position::new(end.row as u32, end.column as u32),
    }
}

fn point_after_text(start: Point, text: &str) -> Point {
    let newline_count = text.bytes().filter(|byte| *byte == b'\n').count();
    if newline_count == 0 {
        return Point {
            row: start.row,
            column: start.column + text.len(),
        };
    }

    Point {
        row: start.row + newline_count,
        column: text
            .as_bytes()
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(text.len(), |offset| text.len() - offset - 1),
    }
}

//*----- TESTS BEGIN HERE -----*
#[cfg(test)]
mod tests {
    use super::*;

    fn change(range: Option<Range>, text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range,
            range_length: None,
            text: text.to_string(),
        }
    }

    fn parse_document(text: &str) -> Document {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("Error loading IFC parser");

        Document::parse(&mut parser, text.to_string())
    }

    fn position_at(text: &str, needle: &str) -> Position {
        let offset = text.find(needle).expect("needle should exist");
        let document = Document::new_unloaded(text.to_string());
        document
            .offset_to_position(offset)
            .expect("needle offset should convert to a position")
    }

    #[test]
    fn parse_builds_text_index_and_ast_state() {
        let text = "#1=IFCWALL($);";
        let document = parse_document(text);

        assert_eq!(document.text, text);
        assert!(document.tree.is_some());
        assert_eq!(document.schema_name, None);
        assert_eq!(document.definitions.get(&1), Some(&0));
        assert_eq!(document.references.get(&1), Some(&vec![0]));
    }

    #[test]
    fn parse_detects_schema_name_from_text_index() {
        let text = "ISO-10303-21;HEADER;FILE_SCHEMA(('IFC4X3_LOCAL_TEST'));ENDSEC;DATA;#1=IFCWALL($);ENDSEC;END-ISO-10303-21;";
        let document = parse_document(text);

        assert_eq!(document.schema_name.as_deref(), Some("IFC4X3_LOCAL_TEST"));
    }

    #[test]
    fn node_at_position_finds_entity_name() {
        let text = "#1=IFCWALL($);";
        let document = parse_document(text);

        let node = document
            .node_at_position(position_at(text, "IFCWALL"))
            .expect("entity_name node should exist");

        assert_eq!(node.kind(), "entity_name");
        assert_eq!(
            node.utf8_text(document.text.as_bytes()).ok(),
            Some("IFCWALL")
        );
    }

    #[test]
    fn node_at_position_finds_reference() {
        let text = "#1=IFCWALL(#2);";
        let document = parse_document(text);

        let node = document
            .node_at_position(position_at(text, "#2"))
            .expect("reference node should exist");

        assert_eq!(node.kind(), "reference");
        assert_eq!(node.utf8_text(document.text.as_bytes()).ok(), Some("#2"));
    }

    #[test]
    fn text_index_records_definition_and_reference_occurrences() {
        let text = "#1=IFCWALL(#2);\n#2=IFCDOOR(#1);";
        let document = parse_document(text);

        assert_eq!(document.definitions.get(&1), Some(&0));
        assert_eq!(document.definitions.get(&2), Some(&16));
        assert_eq!(document.references.get(&1), Some(&vec![0, 27]));
        assert_eq!(document.references.get(&2), Some(&vec![11, 16]));
    }

    #[test]
    fn position_conversion_uses_utf16_columns() {
        let document = Document::new_unloaded("a😀b\n#1=IFCWALL($);".to_string());

        assert_eq!(document.position_to_offset(Position::new(0, 3)), Some(5));
        assert_eq!(document.offset_to_position(5), Some(Position::new(0, 3)));
        assert_eq!(document.position_to_offset(Position::new(1, 0)), Some(7));
    }

    #[test]
    fn instance_by_id_returns_matching_instance() {
        let text = "#1=IFCWALL($);\n#2=IFCDOOR($);";
        let document = parse_document(text);

        let instance = document.instance_by_id(2).expect("instance should exist");

        assert_eq!(instance.entity_name, "IFCDOOR");
    }

    #[test]
    fn unload_parse_state_preserves_text_index_and_clears_ast_state() {
        let text = "#1=IFCWALL(#2);";
        let mut document = parse_document(text);

        document.unload_parse_state();

        assert_eq!(document.text, text);
        assert!(!document.is_parse_state_loaded());
        assert_eq!(document.schema_name, None);
        assert_eq!(document.definitions.get(&1), Some(&0));
        assert_eq!(document.references.get(&2), Some(&vec![11]));
        assert!(document.instances.is_empty());
        assert!(document.instance_indexes_by_id.is_empty());
    }

    #[test]
    fn reload_parse_state_rebuilds_derived_state_from_existing_text() {
        let text = "#1=IFCWALL(#2);\n#2=IFCDOOR($);";
        let mut document = parse_document(text);
        document.unload_parse_state();

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("Error loading IFC parser");
        document.reload_parse_state(&mut parser, DEFAULT_AST_FILE_SIZE_LIMIT_BYTES);

        assert_eq!(document.text, text);
        assert!(document.is_parse_state_loaded());
        assert!(document.definitions.contains_key(&1));
        assert!(document.definitions.contains_key(&2));
        assert_eq!(document.references[&2], vec![11, 16]);
        assert_eq!(
            document
                .instance_by_id(2)
                .expect("instance should be indexed")
                .entity_name,
            "IFCDOOR"
        );
    }

    #[test]
    fn incremental_changes_are_applied_sequentially() {
        let mut document = parse_document("#1=IFCWALL($);");
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("Error loading IFC parser");
        let changes = [
            change(
                Some(Range::new(Position::new(0, 6), Position::new(0, 10))),
                "SLAB",
            ),
            change(
                Some(Range::new(Position::new(0, 11), Position::new(0, 11))),
                "#2,",
            ),
        ];

        document
            .apply_content_changes(&mut parser, &changes, DEFAULT_AST_FILE_SIZE_LIMIT_BYTES)
            .expect("changes should be valid");

        assert_eq!(document.text, "#1=IFCSLAB(#2,$);");
        assert_eq!(document.references.get(&2), Some(&vec![11]));
        assert_eq!(document.instances[0].entity_name, "IFCSLAB");
        assert!(document.has_ast());
    }

    #[test]
    fn incremental_change_ranges_use_utf16_columns() {
        let mut document = parse_document("#1=IFCWALL('😀');");
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("Error loading IFC parser");
        let changes = [change(
            Some(Range::new(Position::new(0, 12), Position::new(0, 14))),
            "door",
        )];

        document
            .apply_content_changes(&mut parser, &changes, DEFAULT_AST_FILE_SIZE_LIMIT_BYTES)
            .expect("change should be valid");

        assert_eq!(document.text, "#1=IFCWALL('door');");
        assert!(document.has_ast());
    }

    #[test]
    fn full_text_change_falls_back_to_full_parse() {
        let mut document = parse_document("#1=IFCWALL($);");
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("Error loading IFC parser");

        document
            .apply_content_changes(
                &mut parser,
                &[change(None, "#2=IFCDOOR($);")],
                DEFAULT_AST_FILE_SIZE_LIMIT_BYTES,
            )
            .expect("change should be valid");

        assert_eq!(document.text, "#2=IFCDOOR($);");
        assert!(document.definitions.contains_key(&2));
        assert_eq!(document.instances[0].entity_name, "IFCDOOR");
    }

    #[test]
    fn incremental_change_respects_ast_size_limit_transitions() {
        let mut document = parse_document("#1=IFCWALL($);");
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("Error loading IFC parser");

        document
            .apply_content_changes(&mut parser, &[change(None, "large")], 4)
            .expect("change should be valid");
        assert!(document.ast_skipped);
        assert!(!document.has_ast());

        document
            .apply_content_changes(&mut parser, &[change(None, "#1=IFCWALL($);")], 1024)
            .expect("change should be valid");
        assert!(!document.ast_skipped);
        assert!(document.has_ast());
    }

    #[test]
    fn invalid_incremental_range_keeps_derived_state_consistent() {
        let mut document = parse_document("#1=IFCWALL($);");
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("Error loading IFC parser");
        let invalid_range = Range::new(Position::new(2, 0), Position::new(2, 1));

        let result = document.apply_content_changes(
            &mut parser,
            &[change(Some(invalid_range), "x")],
            DEFAULT_AST_FILE_SIZE_LIMIT_BYTES,
        );

        assert_eq!(result, Err(ApplyChangeError::InvalidRange(invalid_range)));
        assert_eq!(document.text, "#1=IFCWALL($);");
        assert!(document.definitions.contains_key(&1));
        assert_eq!(document.instances[0].entity_name, "IFCWALL");
        assert!(document.has_ast());
    }

    #[test]
    fn point_after_multiline_text_uses_byte_columns() {
        assert_eq!(
            point_after_text(Point::new(3, 7), "😀\nabc"),
            Point::new(4, 3)
        );
        assert_eq!(point_after_text(Point::new(3, 7), "😀"), Point::new(3, 11));
    }
}
