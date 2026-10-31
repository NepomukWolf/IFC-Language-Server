//! Parsed in-memory representation of one IFC document.
//! This module owns the source text, cheap text indexes for navigation, and optional tree-sitter
//! state for schema-aware diagnostics and derived-value hover.

use std::collections::HashMap;
use std::sync::Arc;

use tower_lsp::lsp_types::{Position, Range, TextDocumentContentChangeEvent};
use tree_sitter::{InputEdit, Node, Parser, Point, TreeCursor};

use crate::document_index::{TextIndex, index_neutral_edit, scan_text};
use crate::document_tree::DocumentTree;
use crate::step::compact::{self, CompactKind, CompactValue};

pub const DEFAULT_AST_FILE_SIZE_LIMIT_BYTES: usize = 200 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum ApplyChangeError {
    InvalidRange(Range),
}

#[derive(Debug)]
pub struct Document {
    pub text: Arc<String>,
    pub tree: Option<DocumentTree>,
    pub ast_skipped: bool,
    pub schema_name: Option<String>,
    pub line_offsets: Vec<usize>,
    pub definitions: HashMap<u32, usize>,
    pub sorted_definition_ids: Vec<u32>,
    pub references: HashMap<u32, Vec<usize>>,
    schema_end: Option<usize>,
}

#[derive(Debug)]
pub struct EntityInstanceCollection {
    pub instances: Vec<EntityInstanceInfo>,
}

#[derive(Debug, Default)]
pub struct EntityTypeIndex {
    entity_names_by_id: HashMap<u32, String>,
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
            sorted_definition_ids,
            references,
            schema_name,
            schema_end,
        } = scan_text(&text);

        Self {
            text: Arc::new(text),
            tree: None,
            ast_skipped: false,
            schema_name,
            line_offsets,
            definitions,
            sorted_definition_ids,
            references,
            schema_end,
        }
    }

    pub fn reload_text_index(&mut self) {
        let TextIndex {
            line_offsets,
            definitions,
            sorted_definition_ids,
            references,
            schema_name,
            schema_end,
        } = scan_text(&self.text);

        self.line_offsets = line_offsets;
        self.definitions = definitions;
        self.sorted_definition_ids = sorted_definition_ids;
        self.references = references;
        self.schema_name = schema_name;
        self.schema_end = schema_end;
    }

    pub fn unload_parse_state(&mut self) {
        self.tree = None;
        self.ast_skipped = false;
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
        let mut index_valid = true;
        for (index, change) in changes.iter().enumerate() {
            if let Some(range) = change.range {
                match self.apply_ranged_change(range, &change.text, index_valid) {
                    Ok(kept_index) => index_valid = kept_index,
                    Err(error) => {
                        self.reload_text_index();
                        let old_tree = self.tree.take();
                        self.parse_current_text(parser, old_tree, ast_file_size_limit_bytes);
                        return Err(error);
                    }
                }
            } else {
                self.text = Arc::new(change.text.clone());
                self.tree = None;
                index_valid = false;
            }
            if !index_valid && index + 1 < changes.len() {
                self.reload_line_offsets();
            }
        }

        if !index_valid {
            self.reload_text_index();
        }
        let old_tree = self.tree.take();
        self.parse_current_text(parser, old_tree, ast_file_size_limit_bytes);
        Ok(())
    }

    fn apply_ranged_change(
        &mut self,
        range: Range,
        replacement: &str,
        index_valid: bool,
    ) -> Result<bool, ApplyChangeError> {
        let start_byte = self
            .position_to_offset(range.start)
            .ok_or(ApplyChangeError::InvalidRange(range))?;
        let old_end_byte = self
            .position_to_offset(range.end)
            .ok_or(ApplyChangeError::InvalidRange(range))?;
        if start_byte > old_end_byte {
            return Err(ApplyChangeError::InvalidRange(range));
        }

        let keep_index =
            index_valid && self.can_keep_text_index(range, start_byte, old_end_byte, replacement);
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
        Arc::make_mut(&mut self.text).replace_range(start_byte..old_end_byte, replacement);
        if keep_index {
            let delta = replacement.len() as isize - (old_end_byte - start_byte) as isize;
            if delta != 0 {
                for offset in self
                    .line_offsets
                    .iter_mut()
                    .chain(self.definitions.values_mut())
                    .chain(self.references.values_mut().flatten())
                {
                    if *offset >= old_end_byte {
                        *offset = offset.checked_add_signed(delta).expect("offset shift fits");
                    }
                }
            }
        }
        Ok(keep_index)
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

    fn can_keep_text_index(
        &self,
        range: Range,
        start: usize,
        end: usize,
        replacement: &str,
    ) -> bool {
        if range.start.line != range.end.line || self.schema_end.is_none_or(|offset| start < offset)
        {
            return false;
        }
        let line_start = self.line_offsets[range.start.line as usize];
        let line = &self.text[line_start..];
        let prefix = line.len() - line.trim_start_matches([' ', '\t']).len();
        let anchor = line_start + prefix;
        let Some(digits) = self.text[anchor..].strip_prefix('#') else {
            return false;
        };
        let length = digits.bytes().take_while(u8::is_ascii_digit).count();
        let Ok(id) = digits[..length].parse::<u32>() else {
            return false;
        };
        // A matching indexed definition certifies the scanner's lexical state at this byte.
        self.definitions.get(&id) == Some(&anchor)
            && index_neutral_edit(&self.text, anchor, start, end, replacement)
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
        old_tree: Option<DocumentTree>,
        ast_file_size_limit_bytes: usize,
    ) {
        self.ast_skipped = false;
        if self.text.len() > ast_file_size_limit_bytes {
            self.ast_skipped = true;
            return;
        }

        self.tree = DocumentTree::parse(parser, self.text.as_bytes(), old_tree);
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
        tree.node_at_offset(offset)
    }

    pub fn entity_instance_by_id(&self, id: u32) -> Option<EntityInstanceInfo> {
        let definition_offset = *self.definitions.get(&id)?;
        let tree = self.tree.as_ref()?;
        let mut node = tree.node_at_offset(definition_offset)?;
        while node.kind() != "entity_instance" {
            node = node.parent()?;
        }
        parse_entity_instance(node, &self.text, &|_| Some(usize::MAX))
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

    /// Like `position_to_offset`, but a line past the last line maps to the end of the
    /// text and a character past the end of a line maps to the end of that line.
    pub fn clamped_position_to_offset(&self, position: Position) -> Option<usize> {
        let line_index = position.line as usize;
        if line_index >= self.line_offsets.len() {
            return Some(self.text.len());
        }
        let line_start = self.line_offsets[line_index];
        let line_end = self.line_end_offset(line_index)?;
        let line_utf16_len: usize = self
            .text
            .get(line_start..line_end)?
            .chars()
            .map(char::len_utf16)
            .sum();
        if position.character as usize >= line_utf16_len {
            return Some(line_end);
        }
        self.position_to_offset(position)
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

    /// Byte offset of the end of `line_index`, excluding a trailing `\n`.
    pub(crate) fn line_end_offset(&self, line_index: usize) -> Option<usize> {
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

    /// Byte span of the identifier token covering (or ending at) `offset`.
    pub(crate) fn identifier_at_offset(&self, offset: usize) -> Option<(usize, usize)> {
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

/// Freeing the tree of a large document takes hundreds of milliseconds, so values that may own the
/// last reference to one are dropped on a blocking thread instead of inside an async handler.
pub fn drop_on_blocking_thread<T: Send + 'static>(value: T) {
    tokio::task::spawn_blocking(move || drop(value));
}

pub(crate) fn is_identifier_part(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
pub fn build_entity_instances(tree: Option<&DocumentTree>, text: &str) -> EntityInstanceCollection {
    let mut instances = Vec::new();
    for_each_entity_instance(
        tree,
        text,
        |_| Some(usize::MAX),
        |instance| instances.push(instance),
    );
    EntityInstanceCollection { instances }
}

/// Visits the entity instances for which `parameter_limit`, given the upper-case entity name,
/// returns how many leading parameters to parse; `None` skips the instance.
pub fn for_each_entity_instance(
    tree: Option<&DocumentTree>,
    text: &str,
    parameter_limit: impl Fn(&str) -> Option<usize>,
    mut visit: impl FnMut(EntityInstanceInfo),
) {
    for_each_entity_node(tree, |node| {
        if let Some(instance) = parse_entity_instance(node, text, &parameter_limit) {
            visit(instance);
        }
    });
}

pub fn build_entity_type_index(tree: Option<&DocumentTree>, text: &str) -> EntityTypeIndex {
    let mut index = EntityTypeIndex::default();

    for_each_entity_node(tree, |node| {
        if let Some(instance) = parse_entity_header(node, text)
            && let Some(id) = instance.id
        {
            index.entity_names_by_id.insert(id, instance.entity_name);
        }
    });

    index
}

impl EntityTypeIndex {
    pub fn entity_name_by_id(&self, id: u32) -> Option<&str> {
        self.entity_names_by_id.get(&id).map(String::as_str)
    }
}

fn for_each_entity_node<'a>(tree: Option<&'a DocumentTree>, visit: impl FnMut(Node<'a>)) {
    if let Some(tree) = tree {
        tree.for_each_entity(visit);
    }
}

pub(crate) fn visit_entity_nodes<'a>(
    cursor: &mut TreeCursor<'a>,
    visit: &mut impl FnMut(Node<'a>),
) {
    loop {
        let node = cursor.node();
        let is_entity = node.kind() == "entity_instance";
        if is_entity {
            visit(node);
        }

        // Valid entity parameters cannot contain entity instances; error recovery can.
        if (!is_entity || node.has_error()) && cursor.goto_first_child() {
            visit_entity_nodes(cursor, visit);
            cursor.goto_parent();
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn parse_entity_instance(
    node: Node<'_>,
    text: &str,
    parameter_limit: &impl Fn(&str) -> Option<usize>,
) -> Option<EntityInstanceInfo> {
    if compact::is_compact_entity(node) {
        return parse_compact_entity(node, text, parameter_limit);
    }
    let mut instance = parse_entity_header(node, text)?;
    let limit = parameter_limit(&instance.entity_name)?;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "parameter_list" => instance.parameters = parse_parameter_list(child, text, limit),
            "compact_parameter_list" => {
                instance.parameters = parse_compact_parameter_list(child, text, limit);
            }
            _ => {}
        }
    }
    Some(instance)
}

fn parse_entity_header(node: Node<'_>, text: &str) -> Option<EntityInstanceInfo> {
    if compact::is_compact_entity(node) {
        return parse_compact_entity(node, text, |_| Some(0));
    }
    let mut child_cursor = node.walk();
    let mut id = None;
    let mut id_range = None;
    let mut entity_name = None;
    let mut entity_name_range = None;
    let mut parameter_list_range = None;

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
            "parameter_list" | "compact_parameter_list" => {
                parameter_list_range = Some(node_range(&child));
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
        parameters: Vec::new(),
    })
}

fn parse_parameter_list(node: Node<'_>, text: &str, limit: usize) -> Vec<ParameterValue> {
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
        .take(limit)
        .map(|parameter| parse_parameter(parameter, text))
        .collect()
}

fn parse_compact_entity(
    node: Node<'_>,
    text: &str,
    parameter_limit: impl Fn(&str) -> Option<usize>,
) -> Option<EntityInstanceInfo> {
    let entity = compact::entity(text, node.start_byte());
    let entity_name = text[entity.name.0..entity.name.1].to_ascii_uppercase();
    let limit = parameter_limit(&entity_name)?;
    let mut points = PointCursor {
        text: text.as_bytes(),
        offset: node.start_byte(),
        point: node.start_position(),
    };
    let id_range = Range {
        start: points.position(entity.id.0),
        end: points.position(entity.id.1),
    };
    let entity_name_range = Range {
        start: points.position(entity.name.0),
        end: points.position(entity.name.1),
    };
    let parameters_start = points.position(entity.parameters.0);
    let parameters = if limit == 0 {
        Vec::new()
    } else {
        compact::parameters(text, entity.parameters.0)
            .iter()
            .take(limit)
            .map(|value| compact_parameter_value(value, text, &mut points))
            .collect()
    };
    Some(EntityInstanceInfo {
        id: text[entity.id.0 + 1..entity.id.1].parse::<u32>().ok(),
        id_range: Some(id_range),
        entity_name,
        entity_name_range,
        entity_range: node_range(&node),
        parameter_list_range: Some(Range {
            start: parameters_start,
            end: points.position(entity.parameters.1),
        }),
        parameters,
    })
}

fn parse_compact_parameter_list(node: Node<'_>, text: &str, limit: usize) -> Vec<ParameterValue> {
    let mut points = PointCursor {
        text: text.as_bytes(),
        offset: node.start_byte(),
        point: node.start_position(),
    };
    compact::parameters(text, node.start_byte())
        .iter()
        .take(limit)
        .map(|value| compact_parameter_value(value, text, &mut points))
        .collect()
}

fn compact_parameter_value(
    value: &CompactValue,
    text: &str,
    points: &mut PointCursor<'_>,
) -> ParameterValue {
    let start = points.position(value.start);
    let source = &text[value.start..value.end];
    let children = match &value.kind {
        CompactKind::Typed { inner: values, .. } | CompactKind::List(values) => values
            .iter()
            .map(|value| compact_parameter_value(value, text, points))
            .collect(),
        _ => Vec::new(),
    };
    let range = Range {
        start,
        end: points.position(value.end),
    };
    match &value.kind {
        CompactKind::String => ParameterValue::String { range },
        CompactKind::Number => ParameterValue::Number {
            text: source.to_string(),
            range,
        },
        CompactKind::Reference => match source[1..].parse::<u32>() {
            Ok(id) => ParameterValue::Reference { id, range },
            Err(_) => ParameterValue::Unknown { range },
        },
        CompactKind::Enumeration => ParameterValue::Enumeration {
            value: source.trim_matches('.').to_ascii_uppercase(),
            range,
        },
        CompactKind::Null => ParameterValue::Null { range },
        CompactKind::Omitted => ParameterValue::Omitted { range },
        CompactKind::Typed { name_end, .. } => ParameterValue::Typed {
            type_name: text[value.start..*name_end].to_ascii_uppercase(),
            inner: children,
            range,
        },
        CompactKind::List(_) => ParameterValue::List {
            items: children,
            range,
        },
    }
}

/// Converts monotonically increasing byte offsets to tree-sitter style positions.
struct PointCursor<'a> {
    text: &'a [u8],
    offset: usize,
    point: Point,
}

impl PointCursor<'_> {
    fn position(&mut self, offset: usize) -> Position {
        for byte in &self.text[self.offset..offset] {
            if *byte == b'\n' {
                self.point.row += 1;
                self.point.column = 0;
            } else {
                self.point.column += 1;
            }
        }
        self.offset = offset;
        Position::new(self.point.row as u32, self.point.column as u32)
    }
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
        "compact_number_list" | "compact_reference_list" => {
            let range = node_range(&node);
            let value = node.utf8_text(text.as_bytes()).expect("compact list text");
            let mut position = point_after_text(node.start_position(), "(");
            let items = value[1..value.len() - 1]
                .split(',')
                .map(|part| {
                    let leading = part.len() - part.trim_start().len();
                    let item = part.trim();
                    let start = point_after_text(position, &part[..leading]);
                    let end = point_after_text(start, item);
                    position = point_after_text(point_after_text(position, part), ",");
                    let range = Range {
                        start: Position::new(start.row as u32, start.column as u32),
                        end: Position::new(end.row as u32, end.column as u32),
                    };
                    if let Some(id) = item.strip_prefix('#') {
                        match id.parse::<u32>() {
                            Ok(id) => ParameterValue::Reference { id, range },
                            Err(_) => ParameterValue::Unknown { range },
                        }
                    } else {
                        ParameterValue::Number {
                            text: item.to_string(),
                            range,
                        }
                    }
                })
                .collect();
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
                    "parameter_list" => inner = parse_parameter_list(child, text, usize::MAX),
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
    use tree_sitter::Tree;

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
            .set_language(&tree_sitter_ifc::LANGUAGE_COMPACT.into())
            .expect("Error loading IFC parser");

        Document::parse(&mut parser, text.to_string())
    }

    #[test]
    fn numeric_aggregate_preserves_values_and_multiline_ranges() {
        let document = parse_document("#1=IFCCARTESIANPOINT(( 1.25,\n -2E-3, .75 ));");
        let instances = build_entity_instances(document.tree.as_ref(), &document.text);
        let ParameterValue::List { items, range } = &instances.instances[0].parameters[0] else {
            panic!("coordinate aggregate expected");
        };
        assert_eq!(
            *range,
            Range::new(Position::new(0, 21), Position::new(1, 13))
        );
        assert_eq!(
            items,
            &vec![
                ParameterValue::Number {
                    text: "1.25".into(),
                    range: Range::new(Position::new(0, 23), Position::new(0, 27))
                },
                ParameterValue::Number {
                    text: "-2E-3".into(),
                    range: Range::new(Position::new(1, 1), Position::new(1, 6))
                },
                ParameterValue::Number {
                    text: ".75".into(),
                    range: Range::new(Position::new(1, 8), Position::new(1, 11))
                },
            ]
        );
    }

    #[test]
    fn reference_aggregate_preserves_ranges_and_overflow() {
        let document = parse_document("#1=IFCPOLYLINE((#12,\n #4294967296,#3));");
        let instances = build_entity_instances(document.tree.as_ref(), &document.text);
        let ParameterValue::List { items, .. } = &instances.instances[0].parameters[0] else {
            panic!("reference aggregate expected");
        };
        assert_eq!(
            items,
            &vec![
                ParameterValue::Reference {
                    id: 12,
                    range: Range::new(Position::new(0, 16), Position::new(0, 19))
                },
                ParameterValue::Unknown {
                    range: Range::new(Position::new(1, 1), Position::new(1, 12))
                },
                ParameterValue::Reference {
                    id: 3,
                    range: Range::new(Position::new(1, 13), Position::new(1, 15))
                },
            ]
        );
    }

    fn instances(document: &Document) -> EntityInstanceCollection {
        build_entity_instances(document.tree.as_ref(), &document.text)
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

        assert_eq!(document.text.as_str(), text);
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
        let text = "#1=IFCWALL(#2,/* structured */$);";
        let document = parse_document(text);

        let node = document
            .node_at_position(position_at(text, "#2"))
            .expect("reference node should exist");

        assert_eq!(node.kind(), "reference");
        assert_eq!(node.utf8_text(document.text.as_bytes()).ok(), Some("#2"));
    }

    fn contains_compact(node: Node<'_>) -> bool {
        node.kind() == "compact_parameter_list"
            || compact::is_compact_entity(node)
            || node.children(&mut node.walk()).any(contains_compact)
    }

    #[test]
    fn compact_parameter_lists_match_structured_parsing() {
        // `~` becomes a space in the compact text and a form feed, which the compact token does
        // not accept, in the structured text, so both parse at identical offsets.
        let cases = [
            "#1=IFCPROPERTYSINGLEVALUE('Name',$,IFCLABEL~('it''s'),*);",
            "#2=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.~);",
            "#3=IFCX((1.,-2.5E-3,.5),(#1,#22),(),('a',IFCREAL(1.),*),~#4294967296,.T.,'/');",
            "#4=IFCX(\n  'a\nb',\n~#5,\n  (1,\n2), ( .A. , $ )\n);",
            "#5=IFCX(IFCBOOLEAN~(.T.),IFCINTEGER(-3),(IFCLABEL ( '' ),*));",
            "#6=IFCX(~);",
            "#7=IFCX(((1.,2.),(3.,\n4.)),((#1),('a',IFCLABEL('b'),*),()),~$);",
            "#8=IFCX('a /* b',IFCLABEL('*/ c'),~'/**/');",
        ];
        let texts = cases.iter().flat_map(|case| {
            [
                case.to_string(),
                format!("DATA;\n{case}\n{case}\nENDSEC;\n"),
            ]
        });
        for case in texts {
            let compact = parse_document(&case.replace('~', " "));
            let structured = parse_document(&case.replace('~', "\u{c}"));
            let compact_root = compact.tree.as_ref().unwrap().root_node();
            let structured_root = structured.tree.as_ref().unwrap().root_node();
            assert!(contains_compact(compact_root), "{case}");
            assert!(!contains_compact(structured_root), "{case}");
            assert!(
                !compact_root.has_error() && !structured_root.has_error(),
                "{case}"
            );
            assert_eq!(
                build_entity_instances(compact.tree.as_ref(), &compact.text).instances,
                build_entity_instances(structured.tree.as_ref(), &structured.text).instances,
                "{case}"
            );

            for offset in 0..=case.len() {
                let position = compact.offset_to_position(offset).unwrap();
                let lookup = |document: &Document| {
                    let node = document.node_at_position(position)?;
                    Some((
                        crate::step::ast::parameter_context(node, offset, &document.text),
                        crate::step::ast::omitted_value_span(node, offset, &document.text),
                        crate::step::ast::typed_value_name_span(node, offset, &document.text),
                    ))
                };
                assert_eq!(lookup(&compact), lookup(&structured), "{case} at {offset}");
            }
        }
    }

    fn node_spans(tree: &Tree) -> Vec<(String, usize, usize)> {
        fn visit(node: Node<'_>, spans: &mut Vec<(String, usize, usize)>) {
            spans.push((node.kind().to_string(), node.start_byte(), node.end_byte()));
            for child in node.children(&mut node.walk()) {
                visit(child, spans);
            }
        }
        let mut spans = Vec::new();
        visit(tree.root_node(), &mut spans);
        spans
    }

    #[test]
    fn documents_with_syntax_errors_keep_the_structured_tree() {
        let structured_spans = |text: &str| {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_ifc::LANGUAGE.into())
                .unwrap();
            node_spans(&parser.parse(text, None).unwrap())
        };
        let invalid = "DATA;\n#1=IFCPROPERTYSINGLEVALUE('Name',$,IFCLABEL'x'),$);\n#2=IFCPROPERTYSET('g',#3,'n',$,(#1));\nENDSEC;\n";
        let mut parser = Parser::new();
        let mut document = parse_document(invalid);
        assert_eq!(
            node_spans(document.tree.as_ref().unwrap().structured().unwrap()),
            structured_spans(invalid)
        );

        let label = Position::new(1, 43);
        let edit = |range: Range, text: &str| vec![change(Some(range), text)];
        document
            .apply_content_changes(
                &mut parser,
                &edit(Range::new(label, label), "("),
                DEFAULT_AST_FILE_SIZE_LIMIT_BYTES,
            )
            .unwrap();
        let root = document.tree.as_ref().unwrap().root_node();
        assert!(!root.has_error());
        assert!(contains_compact(root));

        document
            .apply_content_changes(
                &mut parser,
                &edit(Range::new(label, Position::new(1, 44)), ""),
                DEFAULT_AST_FILE_SIZE_LIMIT_BYTES,
            )
            .unwrap();
        assert_eq!(document.text.as_str(), invalid);
        assert_eq!(
            node_spans(document.tree.as_ref().unwrap().structured().unwrap()),
            structured_spans(invalid)
        );
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

        let instance = document
            .entity_instance_by_id(2)
            .expect("instance should exist");

        assert_eq!(instance.entity_name, "IFCDOOR");
    }

    #[test]
    fn unload_parse_state_preserves_text_index_and_clears_ast_state() {
        let text = "#1=IFCWALL(#2);";
        let mut document = parse_document(text);

        document.unload_parse_state();

        assert_eq!(document.text.as_str(), text);
        assert!(!document.is_parse_state_loaded());
        assert_eq!(document.schema_name, None);
        assert_eq!(document.definitions.get(&1), Some(&0));
        assert_eq!(document.references.get(&2), Some(&vec![11]));
    }

    #[test]
    fn reload_parse_state_rebuilds_derived_state_from_existing_text() {
        let text = "#1=IFCWALL(#2);\n#2=IFCDOOR($);";
        let mut document = parse_document(text);
        document.unload_parse_state();

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE_COMPACT.into())
            .expect("Error loading IFC parser");
        document.reload_parse_state(&mut parser, DEFAULT_AST_FILE_SIZE_LIMIT_BYTES);

        assert_eq!(document.text.as_str(), text);
        assert!(document.is_parse_state_loaded());
        assert!(document.definitions.contains_key(&1));
        assert!(document.definitions.contains_key(&2));
        assert_eq!(document.references[&2], vec![11, 16]);
        assert_eq!(
            document
                .entity_instance_by_id(2)
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
            .set_language(&tree_sitter_ifc::LANGUAGE_COMPACT.into())
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

        assert_eq!(document.text.as_str(), "#1=IFCSLAB(#2,$);");
        assert_eq!(document.references.get(&2), Some(&vec![11]));
        assert_eq!(instances(&document).instances[0].entity_name, "IFCSLAB");
        assert!(document.has_ast());
    }

    #[test]
    fn incremental_change_ranges_use_utf16_columns() {
        let mut document = parse_document("#1=IFCWALL('😀');");
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE_COMPACT.into())
            .expect("Error loading IFC parser");
        let changes = [change(
            Some(Range::new(Position::new(0, 12), Position::new(0, 14))),
            "door",
        )];

        document
            .apply_content_changes(&mut parser, &changes, DEFAULT_AST_FILE_SIZE_LIMIT_BYTES)
            .expect("change should be valid");

        assert_eq!(document.text.as_str(), "#1=IFCWALL('door');");
        assert!(document.has_ast());
    }

    #[test]
    fn full_text_change_falls_back_to_full_parse() {
        let mut document = parse_document("#1=IFCWALL($);");
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE_COMPACT.into())
            .expect("Error loading IFC parser");

        document
            .apply_content_changes(
                &mut parser,
                &[change(None, "#2=IFCDOOR($);")],
                DEFAULT_AST_FILE_SIZE_LIMIT_BYTES,
            )
            .expect("change should be valid");

        assert_eq!(document.text.as_str(), "#2=IFCDOOR($);");
        assert!(document.definitions.contains_key(&2));
        assert_eq!(instances(&document).instances[0].entity_name, "IFCDOOR");
    }

    #[test]
    fn incremental_change_respects_ast_size_limit_transitions() {
        let mut document = parse_document("#1=IFCWALL($);");
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE_COMPACT.into())
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
    fn clamped_position_to_offset_clamps_past_line_and_document_end() {
        let document = parse_document("ab😀\ncd\n");

        assert_eq!(
            document.clamped_position_to_offset(Position::new(0, 1)),
            Some(1)
        );
        assert_eq!(
            document.clamped_position_to_offset(Position::new(0, 99)),
            Some(6)
        );
        assert_eq!(
            document.clamped_position_to_offset(Position::new(1, 99)),
            Some(9)
        );
        assert_eq!(
            document.clamped_position_to_offset(Position::new(99, 0)),
            Some(document.text.len())
        );
        assert_eq!(document.position_to_offset(Position::new(0, 99)), None);
    }

    #[test]
    fn invalid_incremental_range_keeps_derived_state_consistent() {
        let mut document = parse_document("#1=IFCWALL($);");
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE_COMPACT.into())
            .expect("Error loading IFC parser");
        let invalid_range = Range::new(Position::new(2, 0), Position::new(2, 1));

        let result = document.apply_content_changes(
            &mut parser,
            &[change(Some(invalid_range), "x")],
            DEFAULT_AST_FILE_SIZE_LIMIT_BYTES,
        );

        assert_eq!(result, Err(ApplyChangeError::InvalidRange(invalid_range)));
        assert_eq!(document.text.as_str(), "#1=IFCWALL($);");
        assert!(document.definitions.contains_key(&1));
        assert_eq!(instances(&document).instances[0].entity_name, "IFCWALL");
        assert!(document.has_ast());
    }

    fn assert_fresh_index(document: &Document) {
        let fresh = scan_text(&document.text);
        assert_eq!(
            document.line_offsets, fresh.line_offsets,
            "{}",
            document.text
        );
        assert_eq!(document.definitions, fresh.definitions, "{}", document.text);
        assert_eq!(document.references, fresh.references, "{}", document.text);
        assert_eq!(document.schema_name, fresh.schema_name, "{}", document.text);
        assert_eq!(document.schema_end, fresh.schema_end, "{}", document.text);
    }

    #[test]
    fn index_neutral_edits_match_full_scan_at_token_boundaries() {
        let fixtures = [
            "FILE_SCHEMA(('IFC4'));\r\n#1 = IFCWALL('a''b😀\\X2\\00E4\\X0\\',12.34,#2); /* ab */\r\n#2=IFCDOOR(#1);",
            "FILE_SCHEMA /* x; */ (('IFC''4'));\n#1=ABC(#42949672960,1ABC,1FILE_SCHEMA,'a');#2=X(#1);",
            "FILE_SCHEMA #1=ABC('IFC4');\n#2=ABC(12,#1);",
            "FILE_SCHEMA;\n#1=ABC('IFC4');\n#2=ABC(12,#1);",
            "FILE_SCHEMA(('IFC4'));\n/*\n#1=ABC('fake');\n*/\n#1=ABC('real');\n#1=ABC('last');",
            "FILE_SCHEMA(('IFC4'));\n#1=ABC('unterminated\n#2=ABC('x');",
            "FILE_SCHEMA(('IFC4'));\n#1=ABC(12);/* unterminated\n#2=ABC(3);\n",
            "FILE_SCHEMA(('IFC4'));\n#1=ABC(12);/* a*x/ #5=ABC(#1); */\n",
            "FILE_SCHEMA(('IFC4'));\n#1=ABC#2=XYZ#1;\n#3=ABC(12#2);",
            "FILE_SCHEMA(('IFC4'));\n#1=ABC('','''abc',#2);\n#2=ABC(1);",
        ];
        let replacements = [
            "", "x", "9", "_", " ", "\t", "\n", "'", "''", "/*", "*/", "#12=", ";", "😀", "\\X2\\",
        ];
        let mut parser = Parser::new();
        let mut fast = 0;
        for text in fixtures {
            let original = Document::new_unloaded(text.to_string());
            let boundaries: Vec<_> = text
                .char_indices()
                .map(|(i, _)| i)
                .chain([text.len()])
                .collect();
            for (i, &start) in boundaries.iter().enumerate() {
                for &end in boundaries[i..].iter().take(4) {
                    let range = Range::new(
                        original.offset_to_position(start).unwrap(),
                        original.offset_to_position(end).unwrap(),
                    );
                    if original.position_to_offset(range.start) != Some(start)
                        || original.position_to_offset(range.end) != Some(end)
                    {
                        continue;
                    }
                    for replacement in replacements {
                        let mut document = Document::new_unloaded(text.to_string());
                        fast += usize::from(document.can_keep_text_index(
                            range,
                            start,
                            end,
                            replacement,
                        ));
                        document
                            .apply_content_changes(
                                &mut parser,
                                &[change(Some(range), replacement)],
                                0,
                            )
                            .unwrap();
                        assert_fresh_index(&document);
                    }
                }
            }
        }
        assert!(fast > 100);
    }

    #[test]
    fn random_edit_sequences_match_full_scan() {
        let text = "FILE_SCHEMA(('IFC4'));\n#1 = IFCWALL('Name😀 a''b \\X2\\00E4\\X0\\',12.34,#2); /* comment */\n#2=IFCDOOR(#1);\n";
        let replacements = [
            "a", "Name", "IFCDOOR", "1", "99", "#2", "#12=", " ", "\t", "\n", "", ";", "'", "''",
            "/*", "*/", "😀", "\\X2\\",
        ];
        let mut parser = Parser::new();
        let mut fast = 0;
        for seed in 1..=128u64 {
            let mut state = seed;
            let mut random = |limit: usize| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as usize % limit
            };
            let mut document = Document::new_unloaded(text.to_string());
            for step in 0..128 {
                if step % 16 == 0 {
                    document = Document::new_unloaded(text.to_string());
                }
                let mut changes = Vec::new();
                let before = document.text.to_string();
                for _ in 0..1 + random(3) {
                    let boundaries: Vec<_> = document
                        .text
                        .char_indices()
                        .map(|(i, _)| i)
                        .chain([document.text.len()])
                        .collect();
                    let i = random(boundaries.len());
                    let start = boundaries[i];
                    let end = boundaries[(i + random(4)).min(boundaries.len() - 1)];
                    let range = Range::new(
                        document.offset_to_position(start).unwrap(),
                        document.offset_to_position(end).unwrap(),
                    );
                    if document.position_to_offset(range.start) != Some(start)
                        || document.position_to_offset(range.end) != Some(end)
                    {
                        continue;
                    }
                    let replacement = replacements[random(replacements.len())];
                    fast +=
                        usize::from(document.can_keep_text_index(range, start, end, replacement));
                    let event = change(Some(range), replacement);
                    document
                        .apply_content_changes(&mut parser, std::slice::from_ref(&event), 0)
                        .unwrap();
                    assert_fresh_index(&document);
                    changes.push(event);
                }
                let mut batched = Document::new_unloaded(before);
                batched
                    .apply_content_changes(&mut parser, &changes, 0)
                    .unwrap();
                assert_eq!(batched.text, document.text, "seed {seed}");
                assert_fresh_index(&batched);
            }
        }
        assert!(fast > 100);
    }

    #[test]
    fn point_after_multiline_text_uses_byte_columns() {
        assert_eq!(
            point_after_text(Point::new(3, 7), "😀\nabc"),
            Point::new(4, 3)
        );
        assert_eq!(point_after_text(Point::new(3, 7), "😀"), Point::new(3, 11));
    }

    #[test]
    fn discovers_entities_inside_error_recovery() {
        let document = parse_document(concat!(
            "DATA;\n",
            "#1=IFCCARTESIANPOINT((0.,1.,2.));\n",
            "#2=IFCPOLYLINE((#1,#4))\n",
            "#3=IFCDIRECTION((1.,0.,0.));\n",
            "#4=IFCWALL('a',(#2,#3);\n",
            "#5=IFCDIRECTION((1.,0.,0.));\n",
            "#6=IFCWALL('b',$) stray;\n",
            "#7=IFCCARTESIANPOINT((3.,4.,5.));\n",
            "#8=IFCWALL(g',$,.T.,IFCLABEL('x'),(#1,#2),*);\n",
            "#9=IFCDIRECTION((1.,0.,0.));\n",
            "ENDSEC;\n",
        ));
        let instances = instances(&document);
        let discovered: Vec<_> = instances
            .instances
            .iter()
            .map(|instance| {
                (
                    instance.id,
                    instance.entity_name.as_str(),
                    instance.parameters.len(),
                )
            })
            .collect();

        assert_eq!(
            discovered,
            [
                (Some(1), "IFCCARTESIANPOINT", 1),
                (Some(2), "IFCPOLYLINE", 1),
                (Some(3), "IFCDIRECTION", 1),
                (Some(4), "IFCWALL", 2),
                (Some(5), "IFCDIRECTION", 1),
                (Some(6), "IFCWALL", 2),
                (Some(7), "IFCCARTESIANPOINT", 1),
            ]
        );
    }

    #[test]
    fn discovers_entities_after_malformed_syntax() {
        let document = parse_document(concat!(
            "DATA;\n",
            "#1=IFCCARTESIANPOINT((0.,1.,2.));\n",
            "#2=IFCPOLYLINE((#1,#4))\n",
            "#3=IFCDIRECTION((1.,0.,0.));\n",
            "#4=IFCWALL('a',IFCLABEL('x',(#2,#3));\n",
            "#5=IFCDIRECTION((1.,0.,0.));\n",
            "#6=IFCWALL('b',$,%);\n",
            "#7=IFCCARTESIANPOINT((3.,4.,5.));\n",
            "ENDSEC;\n",
        ));
        let ids: Vec<_> = instances(&document)
            .instances
            .iter()
            .map(|instance| instance.id)
            .collect();

        assert_eq!(ids, (1..=7).map(Some).collect::<Vec<_>>());
    }
}
