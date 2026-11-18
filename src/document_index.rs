//! Lightweight STEP text indexing.
//! This scanner intentionally recognizes only the tokens needed for cheap LSP navigation:
//! line starts, local instance ids, references, and FILE_SCHEMA declarations.

use rustc_hash::FxHashMap;

use crate::step;

#[derive(Debug, Default, PartialEq)]
pub struct TextIndex {
    pub line_offsets: Vec<usize>,
    pub definitions: FxHashMap<u32, usize>,
    pub references: FxHashMap<u32, Vec<usize>>,
    pub schema_name: Option<String>,
    pub schema_end: Option<usize>,
    /// Strings whose lookahead for where they end read past their end, by the offset of their
    /// opening quote: the offset where that reading stopped.
    pub string_reaches: Vec<(usize, usize)>,
}

pub fn scan_text(text: &str) -> TextIndex {
    rescan_text(text, TextIndex::default())
}

pub fn rescan_text(text: &str, mut index: TextIndex) -> TextIndex {
    index.line_offsets.clear();
    index.line_offsets.push(0);
    index.definitions.clear();
    index.string_reaches.clear();
    for offsets in index.references.values_mut() {
        offsets.clear();
    }
    let mut schema = None;
    scan(
        text.as_bytes(),
        0,
        usize::MAX,
        &mut index.line_offsets,
        &mut index.string_reaches,
        Some(&mut schema),
        |token| {
            index
                .references
                .entry(token.id)
                .or_default()
                .push(token.offset);
            if token.definition {
                index.definitions.insert(token.id, token.offset);
            }
            true
        },
    );
    (index.schema_name, index.schema_end) = schema.unzip();

    index.references.retain(|_, offsets| !offsets.is_empty());
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
    string_reaches: &mut Vec<(usize, usize)>,
    mut schema: Option<&mut Option<(String, usize)>>,
    mut visit: impl FnMut(IdToken) -> bool,
) -> usize {
    let mut ahead = step::Lookahead::default();
    while offset < bytes.len() && offset <= stop_after {
        let wants_schema = schema.as_deref().is_some_and(Option::is_none);
        match bytes[offset] {
            b'\'' => {
                offset = string_end(bytes, offset, line_offsets, string_reaches, &mut ahead).0;
            }
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
            b'\n' if wants_schema => {
                line_offsets.push(offset + 1);
                offset += 1;
            }
            byte if wants_schema && is_identifier_start(byte) => {
                let ident_start = offset;
                offset += 1;
                while offset < bytes.len() && is_identifier_part(bytes[offset]) {
                    offset += 1;
                }
                if let Some(schema) = schema.as_deref_mut()
                    && bytes[ident_start..offset].eq_ignore_ascii_case(b"FILE_SCHEMA")
                {
                    *schema = parse_file_schema_name(bytes, offset, &mut ahead);
                }
            }
            _ if wants_schema => offset += 1,
            _ => {
                let end = memchr::memchr3(b'#', b'\'', b'/', &bytes[offset + 1..])
                    .map_or(bytes.len(), |n| offset + 1 + n)
                    .min(stop_after.saturating_add(1));
                push_lines(bytes, offset, end, line_offsets);
                offset = end;
            }
        }
    }
    offset
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

/// End of the string opening at `offset` and whether a quote closed it. Like
/// `step::frame_end`, a string that runs into a line starting another statement or `ENDSEC;`
/// ends there, so an unclosed string swallows only its own. Records the string in
/// `string_reaches` when deciding where it ends read past that end.
fn string_end(
    bytes: &[u8],
    mut offset: usize,
    line_offsets: &mut Vec<usize>,
    string_reaches: &mut Vec<(usize, usize)>,
    ahead: &mut step::Lookahead,
) -> (usize, bool) {
    let len = bytes.len();
    let quote = offset;
    ahead.take_comment_reach();
    offset += 1;
    let (end, closed) = 'string: {
        while let Some(n) = memchr::memchr3(b'\n', b'\'', b';', &bytes[offset..]) {
            offset += n;
            match bytes[offset] {
                b'\n' => {
                    let mut next = offset + 1;
                    while next < len && matches!(bytes[next], b' ' | b'\t' | b'\r') {
                        next += 1;
                    }
                    if step::definition(bytes, next, len, ahead)
                        || step::section_end(bytes, next, len, ahead)
                    {
                        break 'string (offset, false);
                    }
                    line_offsets.push(offset + 1);
                    offset += 1;
                }
                b'\'' => {
                    if bytes.get(offset + 1) == Some(&b'\'') {
                        offset += 2;
                    } else {
                        break 'string (offset + 1, true);
                    }
                }
                _ => {
                    let next = step::trivia(bytes, offset + 1, len, ahead);
                    if step::definition(bytes, next, len, ahead)
                        && ahead.line_break(bytes, offset + 1, next)
                    {
                        break 'string (offset + 1, false);
                    }
                    offset += 1;
                }
            }
        }
        (len, false)
    };
    let comment_reach = ahead.take_comment_reach();
    if comment_reach > end {
        // Past its last comment, lookahead reads blanks, an `#id` and blanks, and the byte after.
        let mut read = comment_reach;
        let blanks = |read: &mut usize| {
            while bytes.get(*read).is_some_and(u8::is_ascii_whitespace) {
                *read += 1;
            }
        };
        blanks(&mut read);
        if bytes.get(read) == Some(&b'#') {
            read += 1;
            while bytes.get(read).is_some_and(u8::is_ascii_digit) {
                read += 1;
            }
            blanks(&mut read);
        }
        string_reaches.push((quote, read + 1));
    }
    (end, closed)
}

fn skip_block_comment(bytes: &[u8], offset: usize, line_offsets: &mut Vec<usize>) -> usize {
    match memchr::memmem::find(&bytes[offset + 2..], b"*/") {
        Some(n) => {
            let end = offset + 2 + n;
            push_lines(bytes, offset + 2, end, line_offsets);
            end + 2
        }
        None => {
            push_lines(bytes, offset + 2, bytes.len(), line_offsets);
            bytes.len()
        }
    }
}

fn push_lines(bytes: &[u8], start: usize, end: usize, line_offsets: &mut Vec<usize>) {
    line_offsets.extend(memchr::memchr_iter(b'\n', &bytes[start..end]).map(|n| start + n + 1));
}

fn parse_file_schema_name(
    bytes: &[u8],
    mut offset: usize,
    ahead: &mut step::Lookahead,
) -> Option<(String, usize)> {
    while offset < bytes.len() {
        match bytes[offset] {
            b'\'' => return parse_schema_string(bytes, offset, ahead),
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

fn parse_schema_string(
    bytes: &[u8],
    offset: usize,
    ahead: &mut step::Lookahead,
) -> Option<(String, usize)> {
    let (end, closed) = string_end(bytes, offset, &mut Vec::new(), &mut Vec::new(), ahead);
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
    fn counts_a_final_newline_inside_an_unterminated_comment() {
        assert_eq!(scan_text("#1=X(1);/*\n").line_offsets, vec![0, 11]);
    }

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
    fn an_unclosed_string_ends_at_the_next_statement() {
        let text = "#1=X('a);\n#2=Y(#1);\n#3=Z('b,\n  #4=W(#2);";
        let at = |needle: &str| text.find(needle).unwrap();
        let index = scan_text(text);

        assert_eq!(
            index.definitions,
            FxHashMap::from_iter([(1, 0), (2, at("#2=")), (3, at("#3=")), (4, at("#4="))])
        );
        assert_eq!(index.references.get(&1), Some(&vec![0, at("#1)")]));
        assert_eq!(index.references.get(&2), Some(&vec![at("#2="), at("#2)")]));
        assert_eq!(
            index.line_offsets,
            vec![0, at("#2="), at("#3="), at("  #4=")]
        );
    }

    #[test]
    fn comments_opening_again_and_again_inside_strings_index_in_linear_time() {
        let growths = crate::test_support::growth_on_repeated_comment_openings(1_000, |text| {
            std::hint::black_box(scan_text(text));
        });
        assert!(
            growths.iter().all(|&(_, growth)| growth < 4.0),
            "16x the units took this many times as long as 16 inputs: {growths:?}"
        );
    }

    #[test]
    fn an_unclosed_string_ends_at_an_endsec_line() {
        let text = "DATA;\n#1=X('a);\nENDSEC;\nDATA; #2=Y(#1);\nENDSEC;\n";
        let at = |needle: &str| text.find(needle).unwrap();
        let index = scan_text(text);

        assert_eq!(
            index.definitions,
            FxHashMap::from_iter([(1, at("#1=")), (2, at("#2="))])
        );
        assert_eq!(index.references.get(&1), Some(&vec![at("#1="), at("#1)")]));
    }

    #[test]
    fn a_semicolon_and_a_definition_on_one_line_stay_inside_a_string() {
        let text = "#1=X(';#9=',#2);\n#2=Y(1);";
        let index = scan_text(text);

        assert_eq!(
            index.definitions,
            FxHashMap::from_iter([(1, 0), (2, text.find("#2=").unwrap())])
        );
        assert!(!index.references.contains_key(&9));
    }
}
