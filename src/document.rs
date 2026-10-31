//! Parsed in-memory representation of one IFC document.
//! This module owns the source text, cheap text indexes for navigation, and the lazy STEP index.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use tower_lsp::lsp_types::{Position, Range, TextDocumentContentChangeEvent};

use crate::diagnostics::incremental::{LazyEdits, ValidationCache};
use crate::document_index::{TextIndex, edit_text_index, rescan_text, scan_text};
use crate::handwritten;
use crate::schema::SchemaDoc;

pub const DEFAULT_AST_FILE_SIZE_LIMIT_BYTES: usize = 70 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum ApplyChangeError {
    InvalidRange(Range),
}

#[derive(Debug)]
pub struct Document {
    pub text: Arc<String>,
    /// Statement index and lazy parameter tape for this document.
    pub lazy: Option<Arc<handwritten::Document>>,
    pub lazy_edits: LazyEdits,
    pub validation_cache: Arc<ValidationCache>,
    pub ast_skipped: bool,
    pub schema_name: Option<String>,
    pub line_offsets: Vec<usize>,
    pub definitions: HashMap<u32, usize>,
    pub definitions_by_id: Vec<(u32, usize)>,
    pub references: HashMap<u32, Vec<usize>>,
    schema_end: Option<usize>,
}

#[derive(Debug)]
pub struct EntityInstanceCollection {
    pub instances: Vec<EntityInstanceInfo>,
}

#[derive(Debug, Default)]
pub struct EntityTypeIndex<'a> {
    entity_names_by_id: Vec<(u32, &'a str)>,
    lazy: Option<(&'a handwritten::Document, &'a str)>,
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
            definitions_by_id,
            references,
            schema_name,
            schema_end,
        } = scan_text(&text);

        let lazy = Arc::new(handwritten::Document::parse(text.as_bytes(), false));
        Self {
            text: Arc::new(text),
            lazy: Some(lazy),
            lazy_edits: LazyEdits::fresh(),
            validation_cache: Arc::default(),
            ast_skipped: false,
            schema_name,
            line_offsets,
            definitions,
            definitions_by_id,
            references,
            schema_end,
        }
    }

    pub fn reload_text_index(&mut self) {
        self.with_text_index(|index, text| *index = rescan_text(text, std::mem::take(index)));
    }

    fn with_text_index<R>(&mut self, update: impl FnOnce(&mut TextIndex, &str) -> R) -> R {
        let mut index = TextIndex {
            line_offsets: std::mem::take(&mut self.line_offsets),
            definitions: std::mem::take(&mut self.definitions),
            definitions_by_id: std::mem::take(&mut self.definitions_by_id),
            references: std::mem::take(&mut self.references),
            schema_name: self.schema_name.take(),
            schema_end: self.schema_end.take(),
        };
        let result = update(&mut index, &self.text);
        self.line_offsets = index.line_offsets;
        self.definitions = index.definitions;
        self.definitions_by_id = index.definitions_by_id;
        self.references = index.references;
        self.schema_name = index.schema_name;
        self.schema_end = index.schema_end;
        result
    }

    pub fn unload_parse_state(&mut self) {
        self.lazy = None;
        self.ast_skipped = false;
    }

    pub fn reload_parse_state(&mut self) {
        self.reload_text_index();
        self.unload_parse_state();
        self.lazy = Some(Arc::new(handwritten::Document::parse(
            self.text.as_bytes(),
            false,
        )));
    }

    pub fn apply_content_changes(
        &mut self,
        changes: &[TextDocumentContentChangeEvent],
    ) -> Result<(), ApplyChangeError> {
        for change in changes {
            if let Some(range) = change.range {
                if let Err(error) = self.apply_ranged_change(range, &change.text) {
                    self.reload_parse_state();
                    return Err(error);
                }
            } else {
                self.text = Arc::new(change.text.clone());
                self.lazy = None;
                self.reload_text_index();
            }
        }

        if self.lazy.is_none() {
            self.lazy = Some(Arc::new(handwritten::Document::parse(
                self.text.as_bytes(),
                false,
            )));
            self.lazy_edits = LazyEdits::fresh();
        }
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

        let removed = self.text.as_bytes()[start_byte..old_end_byte].to_vec();
        replace_text(&mut self.text, start_byte..old_end_byte, replacement);
        if let Some(lazy) = self.lazy.as_mut() {
            let outcome = Arc::make_mut(lazy).edit(
                &removed,
                self.text.as_bytes(),
                handwritten::Edit {
                    start: start_byte,
                    delete_len: removed.len(),
                    insert_len: replacement.len(),
                },
            );
            self.lazy_edits.record(outcome);
        }
        let edited = self.with_text_index(|index, text| {
            edit_text_index(index, text, start_byte, &removed, replacement.len())
        });
        if !edited {
            self.reload_text_index();
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn assert_text_index_matches_scan(&self) {
        let index = TextIndex {
            line_offsets: self.line_offsets.clone(),
            definitions: self.definitions.clone(),
            definitions_by_id: self.definitions_by_id.clone(),
            references: self.references.clone(),
            schema_name: self.schema_name.clone(),
            schema_end: self.schema_end,
        };
        assert!(
            index == scan_text(&self.text),
            "text index differs from a fresh scan"
        );
    }

    #[cfg(test)]
    pub fn parse(text: String) -> Self {
        let mut document = Self::new_unloaded(text);
        document.reload_parse_state();
        document
    }

    pub fn is_parse_state_loaded(&self) -> bool {
        self.lazy.is_some()
    }

    pub fn has_ast(&self) -> bool {
        let _ = self;
        false
    }

    pub fn entity_instance_by_id(&self, id: u32) -> Option<EntityInstanceInfo> {
        let definition_offset = *self.definitions.get(&id)?;
        let lazy = self.lazy.as_deref()?;
        let index = lazy
            .statements
            .partition_point(|statement| (statement.span.start as usize) < definition_offset);
        (lazy.statements.get(index)?.span.start as usize == definition_offset).then_some(())?;
        self.decode_statement(lazy, index)
    }

    /// The statement whose span contains `offset`, decoded.
    pub fn entity_instance_at_offset(&self, offset: usize) -> Option<EntityInstanceInfo> {
        let lazy = self.lazy.as_deref()?;
        let index = lazy
            .statements
            .partition_point(|statement| statement.span.start as usize <= offset)
            .checked_sub(1)?;
        (offset < lazy.statements[index].span.end as usize).then_some(())?;
        self.decode_statement(lazy, index)
    }

    fn decode_statement(
        &self,
        lazy: &handwritten::Document,
        index: usize,
    ) -> Option<EntityInstanceInfo> {
        let start = lazy.statements[index].span.start as usize;
        let row = self.line_offsets.partition_point(|&line| line <= start) - 1;
        let mut points = LinePoints {
            text: self.text.as_bytes(),
            offset: self.line_offsets[row],
            row,
            line_start: self.line_offsets[row],
        };
        lazy_entity_instance(lazy, &self.text, index, &mut points)
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

/// Spare capacity given to the text when an edit has to reallocate it, so that typing does not
/// double the allocation of a large document.
const TEXT_HEADROOM: usize = 64 * 1024;

fn replace_text(text: &mut Arc<String>, range: std::ops::Range<usize>, replacement: &str) {
    let len = text.len() - range.len() + replacement.len();
    if Arc::strong_count(text) == 1 {
        let owned = Arc::make_mut(text);
        if owned.capacity() < len {
            owned.reserve_exact(len - owned.len() + TEXT_HEADROOM);
        }
        owned.replace_range(range, replacement);
    } else {
        let mut edited = String::with_capacity(len + TEXT_HEADROOM);
        edited.push_str(&text[..range.start]);
        edited.push_str(replacement);
        edited.push_str(&text[range.end..]);
        *text = Arc::new(edited);
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

pub fn lazy_entity_type_index<'a>(
    lazy: &'a handwritten::Document,
    text: &'a str,
) -> EntityTypeIndex<'a> {
    EntityTypeIndex {
        entity_names_by_id: Vec::new(),
        lazy: Some((lazy, text)),
    }
}

/// Decodes each well-formed statement of a lazy index, in document order, one at a time.
pub(crate) fn for_each_lazy_entity_instance(
    lazy: &handwritten::Document,
    text: &str,
    visit: impl FnMut(EntityInstanceInfo),
) {
    for_each_lazy_entity_instance_named(lazy, text, |_| true, visit);
}

/// Like `for_each_lazy_entity_instance`, but decodes only instances whose uppercase entity name
/// `keep` accepts.
pub(crate) fn for_each_lazy_entity_instance_named(
    lazy: &handwritten::Document,
    text: &str,
    mut keep: impl FnMut(&str) -> bool,
    mut visit: impl FnMut(EntityInstanceInfo),
) {
    let mut points = LinePoints {
        text: text.as_bytes(),
        offset: 0,
        row: 0,
        line_start: 0,
    };
    let mut name = String::new();
    for index in 0..lazy.statements.len() {
        let indexed_name = lazy.statements[index].name;
        if indexed_name.end != 0 {
            name.clear();
            name.push_str(&text[indexed_name.range()]);
            name.make_ascii_uppercase();
            if !keep(&name) {
                continue;
            }
        }
        if let Some(instance) = lazy_entity_instance(lazy, text, index, &mut points)
            && keep(&instance.entity_name)
        {
            visit(instance);
        }
    }
}

/// Decodes one statement with positions relative to its start: row 0 columns count from the
/// statement's first byte, later rows keep absolute columns.
pub(crate) fn lazy_entity_instance_relative(
    lazy: &handwritten::Document,
    text: &str,
    index: usize,
) -> Option<EntityInstanceInfo> {
    let start = lazy.statements[index].span.start as usize;
    let mut points = LinePoints {
        text: text.as_bytes(),
        offset: start,
        row: 0,
        line_start: start,
    };
    lazy_entity_instance(lazy, text, index, &mut points)
}

/// Positions of `offsets`, which must be ascending.
pub(crate) fn positions_of(text: &str, offsets: impl IntoIterator<Item = usize>) -> Vec<Position> {
    let mut points = LinePoints {
        text: text.as_bytes(),
        offset: 0,
        row: 0,
        line_start: 0,
    };
    offsets
        .into_iter()
        .map(|offset| points.position(offset))
        .collect()
}

#[cfg(test)]
thread_local! {
    pub(crate) static DECODED_STATEMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn lazy_entity_instance(
    lazy: &handwritten::Document,
    text: &str,
    index: usize,
    points: &mut LinePoints<'_>,
) -> Option<EntityInstanceInfo> {
    #[cfg(test)]
    DECODED_STATEMENTS.with(|count| count.set(count.get() + 1));
    let statement = lazy.statements[index];
    let span = statement.span.range();
    if statement.name.end == 0 {
        return None;
    }
    let tape = lazy.decode(text.as_bytes(), index);
    let list = tape.first()?;
    let id_range = Range {
        start: points.position(span.start),
        end: points.position(
            span.start
                + 1
                + text.as_bytes()[span.start + 1..]
                    .iter()
                    .take_while(|byte| byte.is_ascii_digit())
                    .count(),
        ),
    };
    let name = statement.name.range();
    let entity_name_range = Range {
        start: points.position(name.start),
        end: points.position(name.end),
    };
    let list_start = points.position(list.span.start as usize);
    let parameters = lazy_parameter_values(&tape, 1, tape.len(), text, points);
    let parameter_list_range = Range {
        start: list_start,
        end: points.position(list.span.end as usize),
    };
    let entity_end = points.position(span.end);
    Some(EntityInstanceInfo {
        id: Some(statement.id),
        id_range: Some(id_range),
        entity_name: text[name].to_ascii_uppercase(),
        entity_name_range,
        entity_range: Range {
            start: id_range.start,
            end: entity_end,
        },
        parameter_list_range: Some(parameter_list_range),
        parameters,
    })
}

fn lazy_parameter_values(
    tape: &[handwritten::Param],
    mut slot: usize,
    end: usize,
    text: &str,
    points: &mut LinePoints<'_>,
) -> Vec<ParameterValue> {
    use handwritten::Kind;
    let mut values = Vec::new();
    while slot < end {
        let param = tape[slot];
        let span = param.span.range();
        let start = points.position(span.start);
        let children = match param.kind {
            Kind::List | Kind::Typed => {
                lazy_parameter_values(tape, slot + 1, param.next as usize, text, points)
            }
            _ => Vec::new(),
        };
        let range = Range {
            start,
            end: points.position(span.end),
        };
        let source = &text[span];
        values.push(match param.kind {
            Kind::String | Kind::Binary => ParameterValue::String { range },
            Kind::Number => ParameterValue::Number {
                text: source.to_string(),
                range,
            },
            Kind::Reference => match source[1..].parse::<u32>() {
                Ok(id) => ParameterValue::Reference { id, range },
                Err(_) => ParameterValue::Unknown { range },
            },
            Kind::Enumeration => ParameterValue::Enumeration {
                value: source.trim_matches('.').to_ascii_uppercase(),
                range,
            },
            Kind::Null => ParameterValue::Null { range },
            Kind::Omitted => ParameterValue::Omitted { range },
            Kind::Typed => ParameterValue::Typed {
                type_name: source[..source.find('(').unwrap_or(source.len())]
                    .trim_end()
                    .to_ascii_uppercase(),
                inner: children,
                range,
            },
            Kind::List => ParameterValue::List {
                items: children,
                range,
            },
        });
        slot = param.next as usize;
    }
    values
}

/// Converts monotonically increasing byte offsets to positions with byte columns.
struct LinePoints<'a> {
    text: &'a [u8],
    offset: usize,
    row: usize,
    line_start: usize,
}

impl LinePoints<'_> {
    fn position(&mut self, offset: usize) -> Position {
        let skipped = &self.text[self.offset..offset];
        if let Some(last) = memchr::memrchr(b'\n', skipped) {
            self.row += memchr::memchr_iter(b'\n', skipped).count();
            self.line_start = self.offset + last + 1;
        }
        self.offset = offset;
        Position::new(self.row as u32, (offset - self.line_start) as u32)
    }
}

impl EntityTypeIndex<'_> {
    pub fn entity_name_by_id(&self, id: u32) -> Option<&str> {
        if let Some((lazy, text)) = self.lazy {
            let name = lazy.statements[lazy.find(id)?].name;
            return (name.end != 0).then(|| &text[name.range()]);
        }
        let index = self
            .entity_names_by_id
            .binary_search_by_key(&id, |&(id, _)| id)
            .ok()?;
        Some(self.entity_names_by_id[index].1)
    }
}

/// The weak text and schema keys prevent address reuse without retaining an obsolete document.
#[derive(Default)]
pub struct EntityInstanceCache {
    entry: Mutex<Option<CachedEntityInstances>>,
    build: Mutex<()>,
}

struct CachedEntityInstances {
    text: Weak<String>,
    schema: Option<Weak<SchemaDoc>>,
    instances: Arc<OnceLock<Arc<EntityInstanceCollection>>>,
}

impl EntityInstanceCache {
    pub fn get_or_build_with(
        &self,
        schema: Option<&Arc<SchemaDoc>>,
        text: &Arc<String>,
        build: impl FnOnce() -> EntityInstanceCollection,
    ) -> Arc<EntityInstanceCollection> {
        let slot = self.slot(schema, text);
        Arc::clone(slot.get_or_init(|| {
            let _permit = self
                .build
                .lock()
                .expect("entity instance build lock should not be poisoned");
            Arc::new(build())
        }))
    }

    pub fn clear(&self) {
        if let Some(cached) = self.lock().take() {
            drop_on_blocking_thread(cached.instances);
        }
    }

    /// Drops the cached instances if they were built from `text`.
    pub fn forget(&self, text: &Arc<String>) {
        let mut entry = self.lock();
        if entry
            .as_ref()
            .is_some_and(|cached| std::ptr::eq(cached.text.as_ptr(), Arc::as_ptr(text)))
            && let Some(cached) = entry.take()
        {
            drop(entry);
            drop_on_blocking_thread(cached.instances);
        }
    }

    fn slot(
        &self,
        schema: Option<&Arc<SchemaDoc>>,
        text: &Arc<String>,
    ) -> Arc<OnceLock<Arc<EntityInstanceCollection>>> {
        let mut entry = self.lock();
        if let Some(cached) = entry.as_ref()
            && std::ptr::eq(cached.text.as_ptr(), Arc::as_ptr(text))
            && match (&cached.schema, schema) {
                (None, None) => true,
                (Some(cached), Some(schema)) => std::ptr::eq(cached.as_ptr(), Arc::as_ptr(schema)),
                _ => false,
            }
        {
            return Arc::clone(&cached.instances);
        }
        let slot = Arc::new(OnceLock::new());
        let previous = entry.replace(CachedEntityInstances {
            text: Arc::downgrade(text),
            schema: schema.map(Arc::downgrade),
            instances: Arc::clone(&slot),
        });
        drop(entry);
        drop(previous);
        slot
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<CachedEntityInstances>> {
        self.entry
            .lock()
            .expect("entity instance cache lock should not be poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::build_entity_instances;

    fn change(range: Option<Range>, text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range,
            range_length: None,
            text: text.to_string(),
        }
    }

    fn parse_document(text: &str) -> Document {
        Document::parse(text.to_string())
    }

    #[test]
    fn numeric_aggregate_preserves_values_and_multiline_ranges() {
        let document = parse_document("#1=IFCCARTESIANPOINT(( 1.25,\n -2E-3, .75 ));");
        let instances = instances(&document);
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
        let instances = instances(&document);
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

    #[test]
    fn insertions_reserve_bounded_headroom_and_edit_unshared_text_in_place() {
        let text = format!("DATA;\n{}ENDSEC;\n", "#1=IFCWALL($);\n".repeat(100_000));
        let mut document = Document::new_unloaded(text.clone());
        let mut text = text;
        text.shrink_to_fit();
        document.text = Arc::new(text);
        let shared = Arc::clone(&document.text);

        for snapshot in [Some(shared), None] {
            document
                .apply_content_changes(&[change(
                    Some(Range::new(Position::new(1, 3), Position::new(1, 3))),
                    "x",
                )])
                .unwrap();
            drop(snapshot);
            let spare = document.text.capacity() - document.text.len();
            assert!(spare <= TEXT_HEADROOM, "{spare} bytes spare");
        }

        let cache_key = Arc::downgrade(&document.text);
        let data = document.text.as_ptr();
        document
            .apply_content_changes(&[change(
                Some(Range::new(Position::new(1, 3), Position::new(1, 3))),
                "y",
            )])
            .unwrap();
        assert_eq!(document.text.as_ptr(), data, "the edit copied the text");
        drop(cache_key);
    }

    fn instances(document: &Document) -> EntityInstanceCollection {
        build_entity_instances(document.lazy.as_deref(), &document.text)
    }

    fn position_at(text: &str, needle: &str) -> Position {
        let offset = text.find(needle).expect("needle should exist");
        let document = Document::new_unloaded(text.to_string());
        document
            .offset_to_position(offset)
            .expect("needle offset should convert to a position")
    }

    #[test]
    fn instance_cache_reuses_instances_only_for_the_same_text_and_schema() {
        let mut document = Document::parse("#1=IFCWALL($);".to_string());
        let cache = EntityInstanceCache::default();

        let build = |document: &Document| {
            cache.get_or_build_with(None, &document.text, || instances(document))
        };
        let first = build(&document);
        let repeated = build(&document);
        assert!(Arc::ptr_eq(&first, &repeated));

        document
            .apply_content_changes(&[TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(0, 6), Position::new(0, 10))),
                range_length: None,
                text: "DOOR".to_string(),
            }])
            .expect("replacement should parse");
        let edited = build(&document);
        let schema = Arc::new(SchemaDoc {
            entities: HashMap::new(),
            types: HashMap::new(),
        });
        let with_schema = cache.get_or_build_with(Some(&schema), &document.text, || {
            build_entity_instances(None, &document.text)
        });

        assert_eq!(first.instances[0].entity_name, "IFCWALL");
        assert_eq!(edited.instances[0].entity_name, "IFCDOOR");
        assert!(with_schema.instances.is_empty());
    }

    #[tokio::test]
    async fn instance_cache_forgets_only_instances_of_the_given_text() {
        let cache = EntityInstanceCache::default();
        let cached = Arc::new("#1=IFCWALL($);".to_string());
        let other = Arc::new("#1=IFCWALL($);".to_string());
        let build = || build_entity_instances(None, &cached);
        let first = cache.get_or_build_with(None, &cached, build);

        cache.forget(&other);
        assert!(Arc::ptr_eq(
            &first,
            &cache.get_or_build_with(None, &cached, build)
        ));
        cache.forget(&cached);
        assert!(!Arc::ptr_eq(
            &first,
            &cache.get_or_build_with(None, &cached, build)
        ));
    }

    #[test]
    fn instance_cache_builds_for_different_texts_never_overlap() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cache = EntityInstanceCache::default();
        let in_flight = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let texts: Vec<Arc<String>> = (0..4)
            .map(|i| Arc::new(format!("#{i}=IFCWALL($);")))
            .collect();

        std::thread::scope(|scope| {
            for text in &texts {
                scope.spawn(|| {
                    cache.get_or_build_with(None, text, || {
                        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        EntityInstanceCollection {
                            instances: Vec::new(),
                        }
                    });
                });
            }
        });

        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn parse_builds_text_index_and_lazy_state() {
        let text = "#1=IFCWALL($);";
        let document = parse_document(text);

        assert_eq!(document.text.as_str(), text);
        assert!(document.lazy.is_some());
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

        let (name, range) = document
            .entity_name_at_position(position_at(text, "IFCWALL"))
            .expect("entity name should exist");
        assert_eq!(name, "IFCWALL");
        assert_eq!(range, Range::new(Position::new(0, 3), Position::new(0, 10)));
    }

    #[test]
    fn node_at_position_finds_reference() {
        let text = "#1=IFCWALL(#2,/* structured */$);";
        let document = parse_document(text);

        assert_eq!(
            document.id_token_at_position(position_at(text, "#2")),
            Some((2, 11))
        );
        assert_eq!(
            document.id_range_at_offset(11),
            Some(Range::new(Position::new(0, 11), Position::new(0, 13)))
        );
    }

    #[test]
    fn compact_parameter_lists_match_structured_parsing() {
        let cases = [
            "#1=IFCPROPERTYSINGLEVALUE('Name',$,IFCLABEL~('it''s'),*);",
            "#2=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.~);",
            "#3=IFCX((1.,-2.5E-3,.5),(#1,#22),(),('a',IFCREAL(1.),*),~#4294967296,.T.,'/');",
            "#4=IFCX(\n  'a\nb',\n~#5,\n  (1,\n2), ( .A. , $ )\n);",
            "#5=IFCX(IFCBOOLEAN~(.T.),IFCINTEGER(-3),(IFCLABEL ( '' ),*));",
            "#6=IFCX(~);",
            "#7=IFCX(((1.,2.),(3.,\n4.)),((#1),('a',IFCLABEL('b'),*),()),~$);",
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
            assert!(compact.lazy.as_ref().unwrap().errors.is_empty(), "{case}");
            assert!(
                structured.lazy.as_ref().unwrap().errors.is_empty(),
                "{case}"
            );
            assert_eq!(
                instances(&compact).instances,
                instances(&structured).instances,
                "{case}"
            );

            for offset in 0..=case.len() {
                let position = compact.offset_to_position(offset).unwrap();
                let lookup = |document: &Document| {
                    let lazy = document.lazy.as_ref().unwrap();
                    let context = lazy.statements.iter().enumerate().find_map(|(index, s)| {
                        let tape = lazy.decode(document.text.as_bytes(), index);
                        let mut slot = 1;
                        let mut parameter_index = 0;
                        while slot < tape.len() {
                            let p = tape[slot];
                            if p.span.range().contains(&offset) {
                                return Some((
                                    s.id,
                                    document.text[s.name.range()].to_ascii_uppercase(),
                                    parameter_index,
                                ));
                            }
                            slot = p.next as usize;
                            parameter_index += 1;
                        }
                        None
                    });
                    let mut omitted = None;
                    let mut typed_name = None;
                    for index in 0..lazy.statements.len() {
                        for p in lazy.decode(document.text.as_bytes(), index) {
                            if !p.span.range().contains(&offset) {
                                continue;
                            }
                            if p.kind == handwritten::Kind::Omitted {
                                omitted = Some((p.span.start, p.span.end));
                            }
                            if p.kind == handwritten::Kind::Typed {
                                let start = p.span.start as usize;
                                let source = &document.text[p.span.range()];
                                let end =
                                    start + source[..source.find('(').unwrap()].trim_end().len();
                                if offset < end {
                                    typed_name = Some((start, end));
                                }
                            }
                        }
                    }
                    Some((
                        context,
                        omitted,
                        typed_name,
                        document.entity_name_at_position(position),
                        document.id_token_at_position(position),
                        instances(document)
                            .instances
                            .into_iter()
                            .flat_map(|i| i.parameters)
                            .filter(|p| p.range().start <= position && position < p.range().end)
                            .collect::<Vec<_>>(),
                    ))
                };
                assert_eq!(lookup(&compact), lookup(&structured), "{case} at {offset}");
            }
        }
    }

    #[test]
    fn lowercase_names_are_accepted_only_for_typed_values() {
        let document =
            parse_document("DATA;#1=IFCX(ifclabel('a'),ifcCountMeasure(3),typed_1(.T.));ENDSEC;");
        assert!(document.lazy.as_ref().unwrap().errors.is_empty());
        let collection = instances(&document);
        let names: Vec<_> = collection.instances[0]
            .parameters
            .iter()
            .map(|value| match value {
                ParameterValue::Typed { type_name, .. } => type_name.as_str(),
                other => panic!("expected typed value, got {other:?}"),
            })
            .collect();
        assert_eq!(names, ["IFCLABEL", "IFCCOUNTMEASURE", "TYPED_1"]);
        for text in [
            "DATA;#1=x(1.);ENDSEC;",
            "DATA;#1=X(.lower.);ENDSEC;",
            "DATA;#1=X(_typed(.T.));ENDSEC;",
        ] {
            let rejected = parse_document(text);
            assert!(!rejected.lazy.as_ref().unwrap().errors.is_empty(), "{text}");
        }
    }

    #[test]
    fn documents_with_syntax_errors_recover_after_edit_and_regress_after_undo() {
        let invalid = "DATA;\n#1=IFCPROPERTYSINGLEVALUE('Name',$,IFCLABEL'x'),$);\n#2=IFCPROPERTYSET('g',#3,'n',$,(#1));\nENDSEC;\n";
        let mut document = parse_document(invalid);
        assert!(!document.lazy.as_ref().unwrap().errors.is_empty());

        let label = Position::new(1, 43);
        let edit = |range: Range, text: &str| vec![change(Some(range), text)];
        document
            .apply_content_changes(&edit(Range::new(label, label), "("))
            .unwrap();
        assert!(document.lazy.as_ref().unwrap().errors.is_empty());
        assert_eq!(instances(&document).instances.len(), 2);

        document
            .apply_content_changes(&edit(Range::new(label, Position::new(1, 44)), ""))
            .unwrap();
        assert_eq!(document.text.as_str(), invalid);
        assert!(!document.lazy.as_ref().unwrap().errors.is_empty());
        assert_eq!(
            document.lazy.as_deref(),
            Some(&handwritten::Document::parse(invalid.as_bytes(), false))
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
        assert_eq!(
            instance.entity_range,
            Range::new(Position::new(1, 0), Position::new(1, 14))
        );
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
        document.reload_parse_state();

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
    fn lazy_index_stays_in_step_with_edits() {
        let mut document =
            Document::new_unloaded("DATA;\n#1=IFCWALL('a');\n#2=IFCSLAB($);\nENDSEC;\n".into());
        document.reload_parse_state();

        let edits = [
            (Position::new(1, 11), Position::new(1, 13), "'b"),
            (Position::new(2, 10), Position::new(2, 10), "X"),
            (Position::new(0, 5), Position::new(1, 0), "\n\n"),
            (Position::new(3, 10), Position::new(3, 11), ""),
        ];
        for (start, end, text) in edits {
            document
                .apply_content_changes(&[change(Some(Range::new(start, end)), text)])
                .expect("change should be valid");
            assert_eq!(
                document.lazy.as_deref(),
                Some(&handwritten::Document::parse(
                    document.text.as_bytes(),
                    false
                )),
                "{:?}",
                document.text
            );
        }

        document
            .apply_content_changes(&[change(None, "#1=IFCWALL($);")])
            .expect("change should be valid");
        assert_eq!(instances(&document).instances[0].entity_name, "IFCWALL");
    }

    #[test]
    fn incremental_changes_are_applied_sequentially() {
        let mut document = parse_document("#1=IFCWALL($);");
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
            .apply_content_changes(&changes)
            .expect("changes should be valid");

        assert_eq!(document.text.as_str(), "#1=IFCSLAB(#2,$);");
        assert_eq!(document.references.get(&2), Some(&vec![11]));
        assert_eq!(instances(&document).instances[0].entity_name, "IFCSLAB");
        assert!(document.is_parse_state_loaded());
    }

    #[test]
    fn incremental_change_ranges_use_utf16_columns() {
        let mut document = parse_document("#1=IFCWALL('😀');");
        let changes = [change(
            Some(Range::new(Position::new(0, 12), Position::new(0, 14))),
            "door",
        )];

        document
            .apply_content_changes(&changes)
            .expect("change should be valid");

        assert_eq!(document.text.as_str(), "#1=IFCWALL('door');");
        assert!(document.is_parse_state_loaded());
    }

    #[test]
    fn full_text_change_falls_back_to_full_parse() {
        let mut document = parse_document("#1=IFCWALL($);");

        document
            .apply_content_changes(&[change(None, "#2=IFCDOOR($);")])
            .expect("change should be valid");

        assert_eq!(document.text.as_str(), "#2=IFCDOOR($);");
        assert!(document.definitions.contains_key(&2));
        assert_eq!(instances(&document).instances[0].entity_name, "IFCDOOR");
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
        let invalid_range = Range::new(Position::new(2, 0), Position::new(2, 1));

        let result = document.apply_content_changes(&[change(Some(invalid_range), "x")]);

        assert_eq!(result, Err(ApplyChangeError::InvalidRange(invalid_range)));
        assert_eq!(document.text.as_str(), "#1=IFCWALL($);");
        assert!(document.definitions.contains_key(&1));
        assert_eq!(instances(&document).instances[0].entity_name, "IFCWALL");
        assert!(document.is_parse_state_loaded());
    }

    /// Whether `edit_text_index` handled the edit without a full rescan; asserts that its result
    /// equals one when it did.
    fn local_edit_matches_scan(text: &str, start: usize, end: usize, replacement: &str) -> bool {
        let mut index = scan_text(text);
        let mut edited = text.to_string();
        edited.replace_range(start..end, replacement);
        let local = edit_text_index(
            &mut index,
            &edited,
            start,
            &text.as_bytes()[start..end],
            replacement.len(),
        );
        if local {
            assert_eq!(
                index,
                scan_text(&edited),
                "{text:?} {start}..{end} {replacement:?}"
            );
        }
        local
    }

    #[test]
    fn edits_match_full_scan_at_token_boundaries() {
        let fixtures = [
            "FILE_SCHEMA(('IFC4'));\r\n#1 = IFCWALL('a''b😀\\X2\\00E4\\X0\\',12.34,#2); /* ab */\r\n#2=IFCDOOR(#1);",
            "FILE_SCHEMA /* x; */ (('IFC''4'));\n#1=ABC(#42949672960,1ABC,1FILE_SCHEMA,'a');#2=X(#1);",
            "FILE_SCHEMA #1=ABC('IFC4');\n#2=ABC(12,#1);",
            "FILE_SCHEMA;\n#1=ABC('IFC4');\n#2=ABC(12,#1);",
            "FILE_SCHEMA(('IFC4'));\n/*\n#1=ABC('fake');\n*/\n#1=ABC('real');\n#1=ABC('last');",
            "FILE_SCHEMA(('IFC4'));\n#1=ABC('unterminated\n#2=ABC('x');",
            "FILE_SCHEMA(('IFC4'));\n#1=ABC(12);/* unterminated\n#2=ABC(3);\n",
            "FILE_SCHEMA(('IFC4'));\n#1=ABC#2=XYZ#1;\n#3=ABC(12#2);",
            "FILE_SCHEMA(('IFC4'));\n#1=ABC('','''abc',#2);\n#2=ABC(1);",
        ];
        let replacements = [
            "", "x", "9", "_", " ", "\t", "\n", "'", "''", "/*", "*/", "#12=", ";", "😀", "\\X2\\",
        ];
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
                        fast += usize::from(local_edit_matches_scan(
                            &document.text,
                            start,
                            end,
                            replacement,
                        ));
                        document
                            .apply_content_changes(&[change(Some(range), replacement)])
                            .unwrap();
                        document.assert_text_index_matches_scan();
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
                    fast += usize::from(local_edit_matches_scan(
                        &document.text,
                        start,
                        end,
                        replacement,
                    ));
                    let event = change(Some(range), replacement);
                    document
                        .apply_content_changes(std::slice::from_ref(&event))
                        .unwrap();
                    document.assert_text_index_matches_scan();
                    changes.push(event);
                }
                let mut batched = Document::new_unloaded(before);
                batched.apply_content_changes(&changes).unwrap();
                assert_eq!(batched.text, document.text, "seed {seed}");
                batched.assert_text_index_matches_scan();
            }
        }
        assert!(fast > 100);
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
                (Some(9), "IFCDIRECTION", 1),
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
