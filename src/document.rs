//! Parsed in-memory representation of one IFC document.
//! This module owns the source text, cheap text indexes for navigation, and the STEP statement index.
#![allow(dead_code)]

mod decode;

use decode::utf16_len;
#[cfg(test)]
pub(crate) use decode::{DECODED_STATEMENTS, UTF16_BYTES};
pub(crate) use decode::{LinePoints, decode_entity_instance, decode_entity_instance_relative};

use rustc_hash::FxHashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use tower_lsp::lsp_types::{Position, Range, TextDocumentContentChangeEvent};

use crate::diagnostics::incremental::{StatementEdits, ValidationCache};
use crate::document_index::{TextIndex, edit_text_index, rescan_text, scan_text};
use crate::schema::SchemaDoc;
use crate::step;

#[derive(Debug, PartialEq, Eq)]
pub enum ApplyChangeError {
    InvalidRange(Range),
}

#[derive(Debug)]
pub struct Document {
    pub text: Arc<String>,
    pub last_change: Option<crate::rt::Instant>,
    /// Statement index of this document; parameters are decoded on demand.
    pub statement_index: Option<Arc<step::Document>>,
    pub statement_edits: StatementEdits,
    pub validation_cache: Arc<ValidationCache>,
    pub schema_name: Option<String>,
    pub line_offsets: Vec<usize>,
    pub definitions: FxHashMap<u32, usize>,
    pub references: FxHashMap<u32, Vec<usize>>,
    schema_end: Option<usize>,
    string_reaches: Vec<(usize, usize)>,
}

#[derive(Debug)]
pub struct EntityInstanceCollection {
    pub instances: Vec<EntityInstanceInfo>,
}

#[derive(Debug)]
pub struct EntityTypeIndex<'a> {
    statement_index: &'a step::Document,
    text: &'a str,
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
    Binary {
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
    Malformed {
        range: Range,
    },
}

impl ParameterValue {
    pub fn range(&self) -> Range {
        match self {
            ParameterValue::Reference { range, .. }
            | ParameterValue::Enumeration { range, .. }
            | ParameterValue::String { range }
            | ParameterValue::Binary { range }
            | ParameterValue::Number { range, .. }
            | ParameterValue::Null { range }
            | ParameterValue::Omitted { range }
            | ParameterValue::List { range, .. }
            | ParameterValue::Typed { range, .. }
            | ParameterValue::Unknown { range }
            | ParameterValue::Malformed { range } => *range,
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            ParameterValue::Reference { .. } => "reference",
            ParameterValue::Enumeration { .. } => "enumeration",
            ParameterValue::String { .. } => "string",
            ParameterValue::Binary { .. } => "binary",
            ParameterValue::Number { .. } => "number",
            ParameterValue::Null { .. } => "null",
            ParameterValue::Omitted { .. } => "omitted value",
            ParameterValue::List { .. } => "aggregate",
            ParameterValue::Typed { .. } => "typed value",
            ParameterValue::Unknown { .. } => "unknown value",
            ParameterValue::Malformed { .. } => "malformed value",
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
            schema_end,
            string_reaches,
        } = scan_text(&text);

        let parsed = Arc::new(step::Document::parse(text.as_bytes(), false));
        Self {
            text: Arc::new(text),
            last_change: None,
            statement_index: Some(parsed),
            statement_edits: StatementEdits::fresh(),
            validation_cache: Arc::default(),
            schema_name,
            line_offsets,
            definitions,
            references,
            schema_end,
            string_reaches,
        }
    }

    pub fn reload_text_index(&mut self) {
        self.with_text_index(|index, text| *index = rescan_text(text, std::mem::take(index)));
    }

    fn with_text_index<R>(&mut self, update: impl FnOnce(&mut TextIndex, &str) -> R) -> R {
        let mut index = TextIndex {
            line_offsets: std::mem::take(&mut self.line_offsets),
            definitions: std::mem::take(&mut self.definitions),
            references: std::mem::take(&mut self.references),
            schema_name: self.schema_name.take(),
            schema_end: self.schema_end.take(),
            string_reaches: std::mem::take(&mut self.string_reaches),
        };
        let result = update(&mut index, &self.text);
        self.line_offsets = index.line_offsets;
        self.definitions = index.definitions;
        self.references = index.references;
        self.schema_name = index.schema_name;
        self.schema_end = index.schema_end;
        self.string_reaches = index.string_reaches;
        result
    }

    pub fn unload_parse_state(&mut self) {
        self.statement_index = None;
    }

    pub fn reload_parse_state(&mut self) {
        self.reload_text_index();
        self.unload_parse_state();
        self.statement_index = Some(Arc::new(step::Document::parse(self.text.as_bytes(), false)));
    }

    /// Replaces the whole text, dropping the parse state and the edit log of the old one.
    pub fn replace_text(&mut self, text: String) {
        self.statement_index = None;
        self.statement_edits = StatementEdits::fresh();
        self.text = Arc::new(text);
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
                self.replace_text(change.text.clone());
                self.reload_text_index();
            }
        }

        if self.statement_index.is_none() {
            self.statement_index =
                Some(Arc::new(step::Document::parse(self.text.as_bytes(), false)));
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
        if let Some(parsed) = self.statement_index.as_mut() {
            let outcome = Arc::make_mut(parsed).edit(
                &removed,
                self.text.as_bytes(),
                step::Edit {
                    start: start_byte,
                    delete_len: removed.len(),
                    insert_len: replacement.len(),
                },
            );
            self.statement_edits.record(outcome);
        } else {
            self.statement_edits = StatementEdits::fresh();
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
            references: self.references.clone(),
            schema_name: self.schema_name.clone(),
            schema_end: self.schema_end,
            string_reaches: self.string_reaches.clone(),
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
        self.statement_index.is_some()
    }

    pub fn entity_instance_by_id(&self, id: u32) -> Option<EntityInstanceInfo> {
        let definition_offset = *self.definitions.get(&id)?;
        let parsed = self.statement_index.as_deref()?;
        let index = parsed
            .statements
            .partition_point(|statement| (statement.span.start as usize) < definition_offset);
        (parsed.statements.get(index)?.span.start as usize == definition_offset).then_some(())?;
        self.decode_statement(parsed, index)
    }

    /// The statement whose span contains `offset`, decoded.
    pub fn entity_instance_at_offset(&self, offset: usize) -> Option<EntityInstanceInfo> {
        let parsed = self.statement_index.as_deref()?;
        let index = parsed
            .statements
            .partition_point(|statement| statement.span.start as usize <= offset)
            .checked_sub(1)?;
        (offset < parsed.statements[index].span.end as usize).then_some(())?;
        self.decode_statement(parsed, index)
    }

    fn decode_statement(
        &self,
        parsed: &step::Document,
        index: usize,
    ) -> Option<EntityInstanceInfo> {
        let start = parsed.statements[index].span.start as usize;
        let row = self.line_offsets.partition_point(|&line| line <= start) - 1;
        let mut points = LinePoints {
            text: self.text.as_bytes(),
            offset: self.line_offsets[row],
            row,
            column: 0,
        };
        decode_entity_instance(parsed, &self.text, index, &mut points)
            .filter(|instance| instance.parameter_list_range.is_some())
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
        let character = utf16_len(&self.text.as_bytes()[line_start..offset]);

        Some(Position::new(line_index as u32, character as u32))
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

    /// The ranges of the id tokens at `offsets`. Offsets in ascending order are converted from the
    /// previous one, so many tokens on one line cost no more than the line's length.
    pub fn id_ranges_at_offsets(&self, offsets: &[usize]) -> Vec<Range> {
        let mut cursor: Option<(usize, Position)> = None;
        let mut position_at = |offset: usize| {
            let position = match cursor {
                Some((from, at))
                    if from <= offset
                        && self.text.is_char_boundary(offset)
                        && self
                            .line_offsets
                            .get(at.line as usize + 1)
                            .is_none_or(|&next| offset < next) =>
                {
                    let units: u32 = self.text[from..offset]
                        .chars()
                        .map(|character| character.len_utf16() as u32)
                        .sum();
                    Position::new(at.line, at.character + units)
                }
                _ => self.offset_to_position(offset)?,
            };
            cursor = Some((offset, position));
            Some(position)
        };
        offsets
            .iter()
            .filter_map(|&offset| {
                let (start, end, _) = self.id_token_at_offset(offset)?;
                Some(Range {
                    start: position_at(start)?,
                    end: position_at(end)?,
                })
            })
            .collect()
    }

    pub fn definition_range(&self, id: u32) -> Option<Range> {
        self.id_range_at_offset(*self.definitions.get(&id)?)
    }

    pub fn id_token_at_position(&self, position: Position) -> Option<(u32, usize)> {
        let offset = self.position_to_offset(position)?;
        let (start, _, id) = self.id_token_at_offset(offset)?;
        self.occurs(id, start).then_some((id, start))
    }

    /// The instance under the cursor: an id token, or the entity name of a definition.
    pub fn instance_id_at_position(&self, position: Position) -> Option<u32> {
        if let Some((id, _)) = self.id_token_at_position(position) {
            return Some(id);
        }
        let offset = self.position_to_offset(position)?;
        let (identifier_start, _) = self.identifier_at_offset(offset)?;
        let (start, id) = self.definition_id_before_entity_name(identifier_start)?;
        self.occurs(id, start).then_some(id)
    }

    fn occurs(&self, id: u32, start: usize) -> bool {
        self.references
            .get(&id)
            .is_some_and(|offsets| offsets.contains(&start))
    }

    /// Definition offsets and their ids, in document order.
    pub fn definitions_in_order(&self) -> Vec<(usize, u32)> {
        let mut definitions: Vec<(usize, u32)> = self
            .definitions
            .iter()
            .map(|(&id, &offset)| (offset, id))
            .collect();
        definitions.sort_unstable();
        definitions
    }

    /// The last `#id=` definition at or before `offset`, as its offset and id. Walks back from
    /// `offset` and accepts only the `#` tokens the text index records as definitions, so ids in
    /// strings and comments are passed over; the cost is the distance to that definition.
    pub fn enclosing_definition(&self, offset: usize) -> Option<(usize, u32)> {
        let bytes = self.text.as_bytes();
        let mut end = offset.min(bytes.len().checked_sub(1)?);
        loop {
            let start = bytes[..=end].iter().rposition(|&byte| byte == b'#')?;
            let digits = bytes[start + 1..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            let id = self.text[start + 1..start + 1 + digits].parse::<u32>().ok();
            if let Some(id) = id
                && self.definitions.get(&id) == Some(&start)
            {
                return Some((start, id));
            }
            end = start.checked_sub(1)?;
        }
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
        self.definition_id_before_entity_name(identifier_start)
            .is_some()
    }

    /// Offset of the `#` and the id of the `#id=NAME` definition whose entity name begins at
    /// `identifier_start`.
    fn definition_id_before_entity_name(&self, identifier_start: usize) -> Option<(usize, u32)> {
        let bytes = self.text.as_bytes();
        let mut offset = identifier_start;

        while offset > 0 && bytes[offset - 1].is_ascii_whitespace() {
            offset -= 1;
        }
        if offset == 0 || bytes[offset - 1] != b'=' {
            return None;
        }
        offset -= 1;

        while offset > 0 && bytes[offset - 1].is_ascii_whitespace() {
            offset -= 1;
        }
        let digit_end = offset;
        while offset > 0 && bytes[offset - 1].is_ascii_digit() {
            offset -= 1;
        }

        if !(digit_end > offset && offset > 0 && bytes[offset - 1] == b'#') {
            return None;
        }

        let id = self.text[offset..digit_end].parse().ok()?;
        Some((offset - 1, id))
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

// Large cached collections can take hundreds of milliseconds to free; browsers have no blocking
// thread pool, so they must release them inline.
pub fn drop_on_blocking_thread<T: Send + 'static>(value: T) {
    #[cfg(not(target_arch = "wasm32"))]
    tokio::task::spawn_blocking(move || drop(value));
    #[cfg(target_arch = "wasm32")]
    drop(value);
}

pub(crate) fn is_identifier_part(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

pub(crate) fn typed_name_len(source: &str) -> usize {
    source
        .bytes()
        .position(|byte| !is_identifier_part(byte))
        .unwrap_or(source.len())
}

pub fn entity_type_index<'a>(parsed: &'a step::Document, text: &'a str) -> EntityTypeIndex<'a> {
    EntityTypeIndex {
        statement_index: parsed,
        text,
    }
}

/// Decodes each well-formed statement, in document order, one at a time.
pub(crate) fn for_each_entity_instance(
    parsed: &step::Document,
    text: &str,
    mut visit: impl FnMut(EntityInstanceInfo),
) {
    for_each_entity_instance_named(
        parsed,
        text,
        |_| true,
        |instance| {
            if instance.parameter_list_range.is_some() {
                visit(instance);
            }
        },
    );
}

/// Like `for_each_entity_instance`, but decodes only instances whose uppercase entity name
/// `keep` accepts, and also visits statements whose parameters could not be read, with no
/// parameters and no `parameter_list_range`.
pub(crate) fn for_each_entity_instance_named(
    parsed: &step::Document,
    text: &str,
    mut keep: impl FnMut(&str) -> bool,
    mut visit: impl FnMut(EntityInstanceInfo),
) {
    let mut points = LinePoints {
        text: text.as_bytes(),
        offset: 0,
        row: 0,
        column: 0,
    };
    let mut name = String::new();
    for index in 0..parsed.statements.len() {
        let indexed_name = parsed.statements[index].name;
        if indexed_name.end != 0 {
            name.clear();
            name.push_str(&text[indexed_name.range()]);
            name.make_ascii_uppercase();
            if !keep(&name) {
                continue;
            }
        }
        if let Some(instance) = decode_entity_instance(parsed, text, index, &mut points)
            && keep(&instance.entity_name)
        {
            visit(instance);
        }
    }
}

/// Positions of `offsets`, which must be ascending.
pub(crate) fn positions_of(text: &str, offsets: impl IntoIterator<Item = usize>) -> Vec<Position> {
    let mut points = LinePoints {
        text: text.as_bytes(),
        offset: 0,
        row: 0,
        column: 0,
    };
    offsets
        .into_iter()
        .map(|offset| points.position(offset))
        .collect()
}

impl EntityTypeIndex<'_> {
    pub fn entity_name_by_id(&self, id: u32) -> Option<&str> {
        let parsed = self.statement_index;
        let name = parsed.statements[parsed.find(id)?].name;
        (name.end != 0).then(|| &self.text[name.range()])
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
mod tests;
