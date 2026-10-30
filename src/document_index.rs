//! Lightweight STEP text indexing.
//! This scanner intentionally recognizes only the tokens needed for cheap LSP navigation:
//! line starts, local instance ids, references, and FILE_SCHEMA declarations.

use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct TextIndex {
    pub line_offsets: Vec<usize>,
    pub definitions: HashMap<u32, usize>,
    pub references: HashMap<u32, Vec<usize>>,
    pub schema_name: Option<String>,
    pub schema_end: Option<usize>,
}

pub fn scan_text(text: &str) -> TextIndex {
    let bytes = text.as_bytes();
    let mut index = TextIndex {
        line_offsets: vec![0],
        definitions: HashMap::new(),
        references: HashMap::new(),
        schema_name: None,
        schema_end: None,
    };
    let mut offset = 0usize;

    while offset < bytes.len() {
        match bytes[offset] {
            b'\n' => {
                index.line_offsets.push(offset + 1);
                offset += 1;
            }
            b'\'' => offset = skip_string(bytes, offset, &mut index.line_offsets),
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                offset = skip_block_comment(bytes, offset, &mut index.line_offsets);
            }
            b'#' => {
                if let Some((id, end)) = parse_id(bytes, offset) {
                    index.references.entry(id).or_default().push(offset);
                    if is_definition(bytes, end) {
                        index.definitions.insert(id, offset);
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
                if index.schema_name.is_none()
                    && bytes[ident_start..offset].eq_ignore_ascii_case(b"FILE_SCHEMA")
                    && let Some((name, end)) = parse_file_schema_name(bytes, offset)
                {
                    index.schema_name = Some(name);
                    index.schema_end = Some(end);
                }
            }
            _ => offset += 1,
        }
    }

    index
}

/// Whether replacing `start..end` leaves the index unchanged apart from shifted offsets, given that
/// the scanner is outside strings and comments at `anchor`, an offset at or before `start`.
pub fn index_neutral_edit(
    text: &str,
    anchor: usize,
    start: usize,
    end: usize,
    replacement: &str,
) -> bool {
    let bytes = text.as_bytes();
    let mut offset = anchor;
    let mut ignored_lines = Vec::new();
    while offset <= start && offset < bytes.len() {
        let token_start = offset;
        let (interior_start, interior_end) = match bytes[offset] {
            b'#' => {
                offset += 1;
                // Digits after `#` are never neutral, even in an id too large to index.
                while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
                    offset += 1;
                }
                continue;
            }
            b'\'' => {
                offset = skip_string(bytes, offset, &mut ignored_lines);
                (token_start + 1, offset.saturating_sub(1))
            }
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                offset = skip_block_comment(bytes, offset, &mut ignored_lines);
                (token_start + 2, offset.saturating_sub(2))
            }
            byte if is_identifier_start(byte) => {
                offset += 1;
                while bytes.get(offset).is_some_and(|b| is_identifier_part(*b)) {
                    offset += 1;
                }
                (token_start, offset)
            }
            byte if byte.is_ascii_digit() || byte.is_ascii_whitespace() => {
                offset += 1;
                while bytes.get(offset).is_some_and(|b| {
                    if byte.is_ascii_digit() {
                        b.is_ascii_digit()
                    } else {
                        b.is_ascii_whitespace()
                    }
                }) {
                    offset += 1;
                }
                (token_start, offset)
            }
            _ => {
                offset += 1;
                continue;
            }
        };
        if start < interior_start || end > interior_end {
            continue;
        }
        let old = &bytes[start..end];
        let new = replacement.as_bytes();
        return match bytes[token_start] {
            b'\'' => {
                // Inserting between doubled quotes would split a single escape.
                (start == token_start + 1
                    || !(bytes.get(start - 1) == Some(&b'\'') && bytes.get(start) == Some(&b'\'')))
                    && old.iter().chain(new).all(|b| !matches!(b, b'\'' | b'\n'))
            }
            b'/' => {
                // Deleting the bytes between `*` and `/` would end the comment early.
                !(new.is_empty() && bytes[start - 1] == b'*' && bytes.get(end) == Some(&b'/'))
                    && old
                        .iter()
                        .chain(new)
                        .all(|b| !matches!(b, b'*' | b'/' | b'\n'))
            }
            byte => {
                let remaining = offset - token_start - old.len() + new.len();
                remaining > 0
                    && if is_identifier_start(byte) {
                        new.iter().all(|b| is_identifier_part(*b))
                            && (start != token_start
                                || new.first().is_some_and(|b| is_identifier_start(*b))
                                || (new.is_empty()
                                    && bytes.get(end).is_some_and(|b| is_identifier_start(*b))))
                    } else if byte.is_ascii_digit() {
                        new.iter().all(u8::is_ascii_digit)
                    } else {
                        old.iter()
                            .chain(new)
                            .all(|b| b.is_ascii_whitespace() && *b != b'\n')
                    }
            }
        };
    }
    false
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
}
