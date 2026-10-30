//! Lightweight STEP text indexing.
//! This scanner intentionally recognizes only the tokens needed for cheap LSP navigation:
//! line starts, local instance ids, references, and FILE_SCHEMA declarations.

use std::collections::HashMap;

use crate::handwritten;

#[derive(Debug, Default, PartialEq)]
pub struct TextIndex {
    pub line_offsets: Vec<usize>,
    pub definitions: HashMap<u32, usize>,
    pub definitions_by_id: Vec<(u32, usize)>,
    pub references: HashMap<u32, Vec<usize>>,
    pub schema_name: Option<String>,
    pub schema_end: Option<usize>,
}

/// Bounds of a local rescan: how far before an edit it looks for an indexed `#id` to restart
/// from, how far past it it may run, and how many id tokens it may replace. Past them a full
/// rescan is cheaper.
const MAX_ANCHOR_DISTANCE: usize = 64 * 1024;
const MAX_RESCAN_PAST_EDIT: usize = 1024 * 1024;
const MAX_REPLACED_TOKENS: usize = 64 * 1024;

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
        usize::MAX,
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

/// Scans from `offset`, which must be outside any string or comment, until the end, until a
/// token starts past `stop_after`, or until `visit` returns false for an id token; returns where
/// it stopped. Only `rescan_text` looks for FILE_SCHEMA, so the other callers must start after it.
fn scan(
    bytes: &[u8],
    mut offset: usize,
    stop_after: usize,
    line_offsets: &mut Vec<usize>,
    mut schema: Option<&mut Option<(String, usize)>>,
    mut visit: impl FnMut(IdToken) -> bool,
) -> usize {
    while offset < bytes.len() && offset <= stop_after {
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
/// `index` untouched, when the edit reaches into the header or the rescan would exceed its bounds.
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
    // Whether a string before an `#id` ends there depends on the `=` after it, so that lookahead
    // must end before the edit.
    let indexed = (floor..start).rev().find(|&offset| {
        bytes[offset] == b'#'
            && parse_id(bytes, offset).is_some_and(|(id, end)| {
                handwritten::trivia(bytes, end, start) < start
                    && index
                        .references
                        .get(&id)
                        .is_some_and(|offsets| offsets.binary_search(&offset).is_ok())
            })
    });
    // The scan leaves the schema name's string right at `schema_end`.
    let Some(anchor) =
        indexed.or((floor == schema_end && schema_end < start).then_some(schema_end))
    else {
        return false;
    };

    let new_edit_end = start + inserted_len;
    let delta = inserted_len as isize - removed.len() as isize;
    let mut new_lines = Vec::new();
    let mut new_tokens = Vec::new();
    let mut synced = false;
    let new_stop = scan(
        bytes,
        anchor,
        new_edit_end + MAX_RESCAN_PAST_EDIT,
        &mut new_lines,
        None,
        |token| {
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
        },
    );
    if !synced && new_stop < bytes.len() || new_tokens.len() > MAX_REPLACED_TOKENS {
        return false;
    }

    let mut old_window = Vec::with_capacity(new_stop - anchor - inserted_len + removed.len());
    old_window.extend_from_slice(&bytes[anchor..start]);
    old_window.extend_from_slice(removed);
    old_window.extend_from_slice(&bytes[new_edit_end..new_stop]);
    let old_stop = anchor + old_window.len();
    let mut old_lines = Vec::new();
    let mut old_tokens = Vec::new();
    scan(&old_window, 0, usize::MAX, &mut old_lines, None, |token| {
        old_tokens.push(token);
        old_tokens.len() <= MAX_REPLACED_TOKENS
    });
    if old_tokens.len() > MAX_REPLACED_TOKENS {
        return false;
    }

    let mut touched: Vec<u32> = old_tokens
        .iter()
        .filter(|token| token.definition)
        .map(|token| token.id)
        .collect();
    let mut old_ids: Vec<u32> = old_tokens.iter().map(|token| token.id).collect();
    old_ids.sort_unstable();
    old_ids.dedup();
    for id in old_ids {
        let offsets = index
            .references
            .get_mut(&id)
            .expect("an old token is indexed");
        offsets.retain(|&offset| offset < anchor || offset >= old_stop);
        if offsets.is_empty() {
            index.references.remove(&id);
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

    let lines_from = index
        .line_offsets
        .partition_point(|&offset| offset <= anchor);
    let lines_to = lines_from + old_lines.len();
    index.line_offsets.splice(lines_from..lines_to, new_lines);

    touched.extend(
        new_tokens
            .iter()
            .filter(|token| token.definition)
            .map(|token| token.id),
    );
    new_tokens.sort_by_key(|token| token.id);
    for tokens in new_tokens.chunk_by(|a, b| a.id == b.id) {
        let offsets = index.references.entry(tokens[0].id).or_default();
        let at = offsets.partition_point(|&offset| offset < tokens[0].offset);
        offsets.splice(at..at, tokens.iter().map(|token| token.offset));
    }
    touched.sort_unstable();
    touched.dedup();
    for id in touched {
        let last = index.references.get(&id).and_then(|offsets| {
            offsets.iter().rev().copied().find(|&offset| {
                parse_id(bytes, offset).is_some_and(|(_, end)| is_definition(bytes, end))
            })
        });
        let slot = index
            .definitions_by_id
            .binary_search_by_key(&id, |&(id, _)| id);
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

fn skip_string(bytes: &[u8], offset: usize, line_offsets: &mut Vec<usize>) -> usize {
    string_end(bytes, offset, line_offsets).0
}

/// End of the string opening at `offset` and whether a quote closed it. Like
/// `handwritten::frame_end`, a string that runs into the start of another statement ends there,
/// so an unclosed string swallows only its own.
fn string_end(bytes: &[u8], mut offset: usize, line_offsets: &mut Vec<usize>) -> (usize, bool) {
    let len = bytes.len();
    offset += 1;
    while offset < len {
        match bytes[offset] {
            b'\n' => {
                let mut next = offset + 1;
                while next < len && matches!(bytes[next], b' ' | b'\t' | b'\r') {
                    next += 1;
                }
                if handwritten::definition(bytes, next, len) {
                    return (offset, false);
                }
                line_offsets.push(offset + 1);
                offset += 1;
            }
            b'\'' => {
                if bytes.get(offset + 1) == Some(&b'\'') {
                    offset += 2;
                } else {
                    return (offset + 1, true);
                }
            }
            b';' if handwritten::definition(
                bytes,
                handwritten::trivia(bytes, offset + 1, len),
                len,
            ) =>
            {
                return (offset + 1, false);
            }
            _ => offset += 1,
        }
    }
    (offset, false)
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

fn parse_schema_string(bytes: &[u8], offset: usize) -> Option<(String, usize)> {
    let (end, closed) = string_end(bytes, offset, &mut Vec::new());
    closed.then_some(())?;
    let value = std::str::from_utf8(&bytes[offset + 1..end - 1]).ok()?;
    Some((value.replace("''", "'").to_ascii_uppercase(), end))
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
    fn edits_that_change_the_rest_of_a_large_file_fall_back_to_a_full_rescan() {
        let mut text = String::from("FILE_SCHEMA(('IFC4'));\n");
        for id in 1..=60_000 {
            text.push_str(&format!("#{id}=IFCLABEL('label {id}',#{});\n", id / 2 + 1));
        }
        let open = text.find("#2=").expect("second statement");
        let mut with_comment = text.clone();
        with_comment.insert_str(open, "/*");
        let mut index = scan_text(&text);
        assert!(!edit_text_index(&mut index, &with_comment, open, b"", 2));
        assert!(index == scan_text(&text));

        let quote = text.find("1',").expect("first closing quote") + 1;
        let mut with_space = text.clone();
        with_space.insert(quote, ' ');
        assert!(edit_text_index(&mut index, &with_space, quote, b"", 1));
        assert!(index == scan_text(&with_space));
    }

    #[test]
    fn an_unclosed_string_ends_at_the_next_statement() {
        let text = "#1=X('a);\n#2=Y(#1);\n#3=Z('b,\n  #4=W(#2);";
        let at = |needle: &str| text.find(needle).unwrap();
        let index = scan_text(text);

        assert_eq!(
            index.definitions_by_id,
            vec![(1, 0), (2, at("#2=")), (3, at("#3=")), (4, at("#4="))]
        );
        assert_eq!(index.references.get(&1), Some(&vec![0, at("#1)")]));
        assert_eq!(index.references.get(&2), Some(&vec![at("#2="), at("#2)")]));
        assert_eq!(
            index.line_offsets,
            vec![0, at("#2="), at("#3="), at("  #4=")]
        );
    }

    #[test]
    fn deleting_a_closing_quote_rescans_only_its_statement() {
        let mut text = String::from("FILE_SCHEMA(('IFC4'));\n");
        for id in 1..=60_000 {
            text.push_str(&format!("#{id}=IFCLABEL('label {id}',#{});\n", id / 2 + 1));
        }
        let quote = text.find("1',").expect("first closing quote") + 1;
        let mut without_quote = text.clone();
        without_quote.remove(quote);
        let mut index = scan_text(&text);

        assert!(edit_text_index(&mut index, &without_quote, quote, b"'", 0));
        assert!(index == scan_text(&without_quote));
        assert!(edit_text_index(&mut index, &text, quote, b"", 1));
        assert!(index == scan_text(&text));
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
