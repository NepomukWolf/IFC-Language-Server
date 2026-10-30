//! Lightweight STEP text indexing.
//! This scanner intentionally recognizes only the tokens needed for cheap LSP navigation:
//! line starts, local instance ids, references, and FILE_SCHEMA declarations.

use std::collections::HashMap;

#[derive(Debug, Default, PartialEq)]
pub struct TextIndex {
    pub line_offsets: Vec<usize>,
    pub definitions: HashMap<u32, usize>,
    pub definitions_by_id: Vec<(u32, usize)>,
    pub references: HashMap<u32, Vec<usize>>,
    pub schema_name: Option<String>,
    pub schema_end: Option<usize>,
}

/// How far before an edit a local rescan looks for an indexed `#id` to restart from, and how far
/// past it the rescan may run before a full rescan is cheaper.
const MAX_ANCHOR_DISTANCE: usize = 64 * 1024;
const MAX_RESCAN_PAST_EDIT: usize = 1024 * 1024;

pub fn scan_text(text: &str) -> TextIndex {
    rescan_text(text, TextIndex::default())
}

pub fn rescan_text(text: &str, mut index: TextIndex) -> TextIndex {
    index.line_offsets.clear();
    index.line_offsets.push(0);
    index.definitions.clear();
    index.definitions_by_id.clear();
    for offsets in index.references.values_mut() {
        offsets.clear();
    }
    let mut schema = None;
    scan(
        text.as_bytes(),
        0,
        &mut index.line_offsets,
        Some(&mut schema),
        |token| {
            index
                .references
                .entry(token.id)
                .or_default()
                .push(token.offset);
            if token.definition {
                index.definitions.insert(token.id, token.offset);
                index.definitions_by_id.push((token.id, token.offset));
            }
            true
        },
    );
    (index.schema_name, index.schema_end) = schema.unzip();

    index.references.retain(|_, offsets| !offsets.is_empty());
    index.definitions_by_id.sort_unstable();
    index.definitions_by_id.dedup_by(|later, earlier| {
        let duplicate = later.0 == earlier.0;
        if duplicate {
            *earlier = *later;
        }
        duplicate
    });
    index
}

#[derive(Clone, Copy)]
struct IdToken {
    id: u32,
    offset: usize,
    definition: bool,
}

/// Scans from `offset`, which must be outside any string or comment, until the end or until
/// `visit` returns false for an id token; returns where it stopped. Only `rescan_text` looks for
/// FILE_SCHEMA, so the other callers must start after it.
fn scan(
    bytes: &[u8],
    mut offset: usize,
    line_offsets: &mut Vec<usize>,
    mut schema: Option<&mut Option<(String, usize)>>,
    mut visit: impl FnMut(IdToken) -> bool,
) -> usize {
    while offset < bytes.len() {
        match bytes[offset] {
            b'\n' => {
                line_offsets.push(offset + 1);
                offset += 1;
            }
            b'\'' => offset = skip_string(bytes, offset, line_offsets),
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                offset = skip_block_comment(bytes, offset, line_offsets);
            }
            b'#' => {
                if let Some((id, end)) = parse_id(bytes, offset) {
                    let token = IdToken {
                        id,
                        offset,
                        definition: is_definition(bytes, end),
                    };
                    if !visit(token) {
                        return offset;
                    }
                    offset = end;
                } else {
                    offset += 1;
                }
            }
            byte if is_identifier_start(byte) => {
                let ident_start = offset;
                offset += 1;
                while offset < bytes.len() && is_identifier_part(bytes[offset]) {
                    offset += 1;
                }
                if let Some(schema) = schema.as_deref_mut()
                    && schema.is_none()
                    && bytes[ident_start..offset].eq_ignore_ascii_case(b"FILE_SCHEMA")
                {
                    *schema = parse_file_schema_name(bytes, offset);
                }
            }
            _ => offset += 1,
        }
    }
    offset
}

/// Updates `index`, built for the text before an edit, after `removed` at `start` was replaced
/// by the `inserted_len` bytes of `text` at `start`. Rescans from the last indexed `#id` before
/// the edit until the first `#id` after it that the old scan also indexed: both scans are outside
/// strings and comments there, so the rest of the index only shifts. Returns false, leaving
/// `index` untouched, when the edit reaches into the header or no such `#id` is near.
#[tracing::instrument(skip_all)]
pub fn edit_text_index(
    index: &mut TextIndex,
    text: &str,
    start: usize,
    removed: &[u8],
    inserted_len: usize,
) -> bool {
    let bytes = text.as_bytes();
    let Some(schema_end) = index.schema_end.filter(|&end| end <= start) else {
        return false;
    };
    let floor = schema_end.max(start.saturating_sub(MAX_ANCHOR_DISTANCE));
    let Some(anchor) = (floor..start).rev().find(|&offset| {
        bytes[offset] == b'#'
            && parse_id(bytes, offset).is_some_and(|(id, _)| {
                index
                    .references
                    .get(&id)
                    .is_some_and(|offsets| offsets.binary_search(&offset).is_ok())
            })
    }) else {
        return false;
    };

    let new_edit_end = start + inserted_len;
    let delta = inserted_len as isize - removed.len() as isize;
    let mut new_lines = Vec::new();
    let mut new_tokens = Vec::new();
    let mut synced = false;
    let new_stop = scan(bytes, anchor, &mut new_lines, None, |token| {
        if token.offset > new_edit_end + MAX_RESCAN_PAST_EDIT {
            return false;
        }
        synced = token.offset >= new_edit_end
            && index.references.get(&token.id).is_some_and(|offsets| {
                offsets
                    .binary_search(&((token.offset as isize - delta) as usize))
                    .is_ok()
            });
        if !synced {
            new_tokens.push(token);
        }
        !synced
    });
    if !synced && new_stop < bytes.len() {
        return false;
    }

    let mut old_window = Vec::with_capacity(new_stop - anchor - inserted_len + removed.len());
    old_window.extend_from_slice(&bytes[anchor..start]);
    old_window.extend_from_slice(removed);
    old_window.extend_from_slice(&bytes[new_edit_end..new_stop]);
    let old_stop = anchor + old_window.len();
    let mut old_lines = Vec::new();
    let mut old_tokens = Vec::new();
    scan(&old_window, 0, &mut old_lines, None, |token| {
        old_tokens.push(token);
        true
    });

    let mut touched = Vec::new();
    for token in &old_tokens {
        let offset = anchor + token.offset;
        let offsets = index
            .references
            .get_mut(&token.id)
            .expect("an old token is indexed");
        let at = offsets
            .binary_search(&offset)
            .expect("an old token's offset is indexed");
        offsets.remove(at);
        if offsets.is_empty() {
            index.references.remove(&token.id);
        }
        if token.definition {
            touched.push(token.id);
        }
    }
    let shift = |offset: &mut usize| {
        if *offset >= old_stop {
            *offset = offset.wrapping_add_signed(delta);
        }
    };
    if delta != 0 {
        index.line_offsets.iter_mut().for_each(shift);
        index.definitions.values_mut().for_each(shift);
        index
            .definitions_by_id
            .iter_mut()
            .for_each(|(_, offset)| shift(offset));
        index.references.values_mut().flatten().for_each(shift);
    }

    let lines_from = index.line_offsets.partition_point(|&offset| offset <= anchor);
    let lines_to = lines_from + old_lines.len();
    index.line_offsets.splice(lines_from..lines_to, new_lines);

    for token in &new_tokens {
        let offsets = index.references.entry(token.id).or_default();
        let at = offsets.partition_point(|&offset| offset < token.offset);
        offsets.insert(at, token.offset);
        if token.definition {
            touched.push(token.id);
        }
    }
    touched.sort_unstable();
    touched.dedup();
    for id in touched {
        let last = index.references.get(&id).and_then(|offsets| {
            offsets.iter().rev().copied().find(|&offset| {
                parse_id(bytes, offset).is_some_and(|(_, end)| is_definition(bytes, end))
            })
        });
        let slot = index.definitions_by_id.binary_search_by_key(&id, |&(id, _)| id);
        match (last, slot) {
            (Some(offset), Ok(at)) => {
                index.definitions.insert(id, offset);
                index.definitions_by_id[at].1 = offset;
            }
            (Some(offset), Err(at)) => {
                index.definitions.insert(id, offset);
                index.definitions_by_id.insert(at, (id, offset));
            }
            (None, Ok(at)) => {
                index.definitions.remove(&id);
                index.definitions_by_id.remove(at);
            }
            (None, Err(_)) => {}
        }
    }
    true
}

fn parse_id(bytes: &[u8], offset: usize) -> Option<(u32, usize)> {
    let mut end = offset + 1;
    let mut id = 0u32;
    let mut has_digit = false;

    while let Some(byte) = bytes.get(end).copied()
        && byte.is_ascii_digit()
    {
        has_digit = true;
        id = id.checked_mul(10)?.checked_add((byte - b'0') as u32)?;
        end += 1;
    }

    has_digit.then_some((id, end))
}

fn is_definition(bytes: &[u8], mut offset: usize) -> bool {
    while let Some(byte) = bytes.get(offset).copied()
        && byte.is_ascii_whitespace()
    {
        offset += 1;
    }

    bytes.get(offset) == Some(&b'=')
}

fn skip_string(bytes: &[u8], mut offset: usize, line_offsets: &mut Vec<usize>) -> usize {
    offset += 1;
    while offset < bytes.len() {
        match bytes[offset] {
            b'\n' => {
                line_offsets.push(offset + 1);
                offset += 1;
            }
            b'\'' => {
                if bytes.get(offset + 1) == Some(&b'\'') {
                    offset += 2;
                } else {
                    return offset + 1;
                }
            }
            _ => offset += 1,
        }
    }
    offset
}

fn skip_block_comment(bytes: &[u8], mut offset: usize, line_offsets: &mut Vec<usize>) -> usize {
    offset += 2;
    while offset + 1 < bytes.len() {
        if bytes[offset] == b'*' && bytes[offset + 1] == b'/' {
            return offset + 2;
        }
        if bytes[offset] == b'\n' {
            line_offsets.push(offset + 1);
        }
        offset += 1;
    }
    bytes.len()
}

fn parse_file_schema_name(bytes: &[u8], mut offset: usize) -> Option<(String, usize)> {
    while offset < bytes.len() {
        match bytes[offset] {
            b'\'' => return parse_schema_string(bytes, offset),
            b';' => return None,
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                let mut ignored_line_offsets = Vec::new();
                offset = skip_block_comment(bytes, offset, &mut ignored_line_offsets);
            }
            _ => offset += 1,
        }
    }

    None
}

fn parse_schema_string(bytes: &[u8], mut offset: usize) -> Option<(String, usize)> {
    offset += 1;
    let start = offset;
    let mut value = Vec::new();

    while offset < bytes.len() {
        if bytes[offset] == b'\'' {
            if bytes.get(offset + 1) == Some(&b'\'') {
                value.extend_from_slice(&bytes[start..offset]);
                value.push(b'\'');
                offset += 2;
                return parse_schema_string_tail(bytes, offset, value);
            }
            value.extend_from_slice(&bytes[start..offset]);
            return String::from_utf8(value)
                .ok()
                .map(|schema_name| (schema_name.to_ascii_uppercase(), offset + 1));
        }
        offset += 1;
    }

    None
}

fn parse_schema_string_tail(
    bytes: &[u8],
    mut offset: usize,
    mut value: Vec<u8>,
) -> Option<(String, usize)> {
    let mut start = offset;
    while offset < bytes.len() {
        if bytes[offset] == b'\'' {
            if bytes.get(offset + 1) == Some(&b'\'') {
                value.extend_from_slice(&bytes[start..offset]);
                value.push(b'\'');
                offset += 2;
                start = offset;
            } else {
                value.extend_from_slice(&bytes[start..offset]);
                return String::from_utf8(value)
                    .ok()
                    .map(|schema_name| (schema_name.to_ascii_uppercase(), offset + 1));
            }
        } else {
            offset += 1;
        }
    }

    None
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_identifier_part(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_schema_definitions_references_and_lines() {
        let text = "HEADER;FILE_SCHEMA(('IFC4'));\nDATA;\n#1=IFCWALL(#2);\n#2=IFCDOOR($);";

        let index = scan_text(text);

        assert_eq!(index.schema_name.as_deref(), Some("IFC4"));
        assert_eq!(index.line_offsets, vec![0, 30, 36, 52]);
        assert_eq!(index.definitions.get(&1), Some(&36));
        assert_eq!(index.definitions.get(&2), Some(&52));
        assert_eq!(index.references.get(&1), Some(&vec![36]));
        assert_eq!(index.references.get(&2), Some(&vec![47, 52]));
    }

    #[test]
    fn ignores_ids_inside_strings_and_comments() {
        let text = "#1=IFCWALL('#2');/* #3=IFCDOOR($); */#4=IFCWINDOW(#1);";

        let index = scan_text(text);

        assert!(index.definitions.contains_key(&1));
        assert!(index.definitions.contains_key(&4));
        assert!(!index.definitions.contains_key(&3));
        assert_eq!(index.references.get(&1), Some(&vec![0, 50]));
        assert!(!index.references.contains_key(&2));
        assert!(!index.references.contains_key(&3));
    }

    #[test]
    fn lists_definitions_by_id_with_the_offsets_of_the_map() {
        let text = "#10=A();\n#2=B();\n#10=C();\n#1=D('#3=E();');\n#2=F();\n";

        let index = scan_text(text);

        let mut from_map = index
            .definitions
            .iter()
            .map(|(&id, &offset)| (id, offset))
            .collect::<Vec<_>>();
        from_map.sort();
        assert_eq!(index.definitions_by_id, from_map);
        assert_eq!(index.definitions_by_id, vec![(1, 26), (2, 43), (10, 17)]);
    }
}
