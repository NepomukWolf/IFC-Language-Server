//! Shared lightweight STEP entity-instance argument scanner.
//! Given a known `#id=` definition offset, this walks the positional argument list without a
//! full tree-sitter parse. Shared by the text-index features (inlay hints, completions,
//! related-entity navigation, call hierarchy and the `ifc/*` instance requests), which only need argument
//! boundaries, enclosing entity names, string values, and referenced ids within a bounded region.

use crate::step::string_literal::decode_string;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ScannedInstance {
    pub(crate) entity_name: String,
    /// One span per positional argument, as `scan_argument_spans` reports them, so an empty
    /// argument still occupies its position.
    pub(crate) arguments: Vec<(usize, usize)>,
}

impl ScannedInstance {
    /// The start of every non-empty argument.
    pub(crate) fn argument_starts(&self) -> Vec<usize> {
        self.arguments
            .iter()
            .filter(|(start, end)| start < end)
            .map(|&(start, _)| start)
            .collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct EntityArgumentContext {
    pub(crate) entity_name: String,
    pub(crate) active_parameter: usize,
    pub(crate) aggregate_depth: usize,
    pub(crate) value_start: usize,
}

/// Locates the positional argument containing `cursor_offset` without requiring an AST.
pub(crate) fn entity_argument_context(
    text: &str,
    cursor_offset: usize,
) -> Option<EntityArgumentContext> {
    if cursor_offset > text.len() || !text.is_char_boundary(cursor_offset) {
        return None;
    }

    let bytes = text.as_bytes();
    let mut offset = statement_start_before_cursor(text, cursor_offset);
    offset = skip_trivia(bytes, offset, bytes.len());
    let (_, next) = scan_instance_id(bytes, offset, bytes.len())?;
    offset = skip_trivia(bytes, next, bytes.len());
    if bytes.get(offset) != Some(&b'=') {
        return None;
    }

    offset = skip_trivia(bytes, offset + 1, bytes.len());
    let entity_start = offset;
    offset = scan_identifier(bytes, offset, bytes.len())?;
    let entity_name = text.get(entity_start..offset)?.to_ascii_uppercase();
    offset = skip_trivia(bytes, offset, bytes.len());
    if bytes.get(offset) != Some(&b'(') || cursor_offset <= offset {
        return None;
    }

    let mut active_parameter = 0usize;
    let mut aggregate_depth = 0usize;
    let mut value_start = offset + 1;
    offset += 1;

    while offset < cursor_offset {
        match bytes[offset] {
            b'\'' => offset = scan_string(bytes, offset, cursor_offset),
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                let comment_start = offset;
                offset = scan_block_comment(bytes, offset, cursor_offset);
                if bytes[value_start..comment_start]
                    .iter()
                    .all(|byte| byte.is_ascii_whitespace())
                {
                    value_start = offset;
                }
            }
            b'(' => {
                aggregate_depth += 1;
                offset += 1;
                value_start = offset;
            }
            b')' if aggregate_depth == 0 => return None,
            b')' => {
                aggregate_depth -= 1;
                offset += 1;
            }
            b',' => {
                if aggregate_depth == 0 {
                    active_parameter += 1;
                }
                offset += 1;
                value_start = offset;
            }
            _ => offset += 1,
        }
    }

    Some(EntityArgumentContext {
        entity_name,
        active_parameter,
        aggregate_depth,
        value_start,
    })
}

fn statement_start_before_cursor(text: &str, cursor_offset: usize) -> usize {
    let bytes = text.as_bytes();
    let mut offset = 0;
    let mut statement_start = 0;

    while offset < cursor_offset {
        match bytes[offset] {
            b'\'' => offset = scan_string(bytes, offset, cursor_offset),
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                offset = scan_block_comment(bytes, offset, cursor_offset);
            }
            b';' => {
                offset += 1;
                statement_start = offset;
            }
            _ => offset += 1,
        }
    }

    statement_start
}

/// Scans one entity instance starting at its `#id=` offset, returning the parsed instance and
/// the offset just past its closing `)`.
pub(crate) fn scan_instance(
    text: &str,
    start: usize,
    scan_end: usize,
) -> Option<(ScannedInstance, usize)> {
    let bytes = text.as_bytes();
    let (_, mut offset) = scan_instance_id(bytes, start, scan_end)?;
    offset = skip_trivia(bytes, offset, scan_end);
    if bytes.get(offset) != Some(&b'=') {
        return None;
    }

    offset = skip_trivia(bytes, offset + 1, scan_end);
    let entity_start = offset;
    offset = scan_identifier(bytes, offset, scan_end)?;
    let entity_name = text.get(entity_start..offset)?.to_ascii_uppercase();
    offset = skip_trivia(bytes, offset, scan_end);
    if bytes.get(offset) != Some(&b'(') {
        return None;
    }

    let (arguments, end) = scan_argument_spans(bytes, offset + 1, scan_end);
    Some((
        ScannedInstance {
            entity_name,
            arguments,
        },
        end,
    ))
}

/// Reads only the entity name at a known `#id=` definition offset.
pub(crate) fn scan_definition_entity_name(
    text: &str,
    start: usize,
    scan_end: usize,
) -> Option<String> {
    let bytes = text.as_bytes();
    let (_, mut offset) = scan_instance_id(bytes, start, scan_end)?;
    offset = skip_trivia(bytes, offset, scan_end);
    if bytes.get(offset) != Some(&b'=') {
        return None;
    }

    offset = skip_trivia(bytes, offset + 1, scan_end);
    let entity_start = offset;
    offset = scan_identifier(bytes, offset, scan_end)?;
    text.get(entity_start..offset).map(str::to_ascii_uppercase)
}

/// Collects every local instance id (`#N`) referenced within `[start, end)`, skipping strings
/// and block comments. Intended for reading the ids inside a single already-located argument.
pub(crate) fn ids_in_span(text: &str, start: usize, end: usize) -> Vec<u32> {
    id_tokens_in_span(text, start, end)
        .into_iter()
        .map(|(_, id)| id)
        .collect()
}

/// Like `ids_in_span`, with the offset of each id's `#`.
pub(crate) fn id_tokens_in_span(text: &str, start: usize, end: usize) -> Vec<(usize, u32)> {
    let bytes = text.as_bytes();
    let end = end.min(bytes.len());
    let mut offset = start.min(end);
    let mut ids = Vec::new();

    while offset < end {
        match bytes[offset] {
            b'#' => {
                if let Some((id, next)) = scan_instance_id(bytes, offset, end) {
                    ids.push((offset, id));
                    offset = next;
                } else {
                    offset += 1;
                }
            }
            b'\'' => offset = scan_string(bytes, offset, end),
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                offset = scan_block_comment(bytes, offset, end);
            }
            _ => offset += 1,
        }
    }

    ids
}

/// Collects the span of every top-level argument, from just past an argument list's `(`, and
/// returns them with the offset just past the closing `)`. Spans exclude surrounding whitespace
/// and leading comments; an empty argument between two commas has an empty span, while an empty
/// list `()` has no arguments.
pub(crate) fn scan_argument_spans(
    bytes: &[u8],
    mut offset: usize,
    scan_end: usize,
) -> (Vec<(usize, usize)>, usize) {
    let mut spans = Vec::new();
    let mut argument_start = offset;
    let mut depth = 0usize;

    while offset < scan_end {
        match bytes[offset] {
            b'\'' => offset = scan_string(bytes, offset, scan_end),
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                offset = scan_block_comment(bytes, offset, scan_end);
            }
            b'(' => {
                depth += 1;
                offset += 1;
            }
            b')' if depth > 0 => {
                depth -= 1;
                offset += 1;
            }
            b')' => {
                spans.push(trim_argument(bytes, argument_start, offset));
                if let [(start, end)] = spans[..]
                    && start == end
                {
                    spans.clear();
                }
                return (spans, offset + 1);
            }
            b',' if depth == 0 => {
                spans.push(trim_argument(bytes, argument_start, offset));
                offset += 1;
                argument_start = offset;
            }
            _ => offset += 1,
        }
    }

    let (start, end) = trim_argument(bytes, argument_start, scan_end);
    if start < end {
        spans.push((start, end));
    }
    (spans, scan_end)
}

fn trim_argument(bytes: &[u8], start: usize, end: usize) -> (usize, usize) {
    let start = skip_trivia(bytes, start, end);
    let mut trimmed_end = end;
    while trimmed_end > start && bytes[trimmed_end - 1].is_ascii_whitespace() {
        trimmed_end -= 1;
    }
    (start, trimmed_end)
}

/// The unquoted, unescaped content of the STEP string literal starting at `offset`, or None when
/// no string starts there.
pub(crate) fn string_literal(text: &str, offset: usize) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.get(offset) != Some(&b'\'') {
        return None;
    }
    let end = scan_string(bytes, offset, bytes.len());
    let content = text.get(offset + 1..end.checked_sub(1)?)?;
    Some(decode_string(content))
}

/// The offset just past an instance's terminating `;`, given the offset just past its argument
/// list, or that offset itself when no `;` follows.
pub(crate) fn statement_end(bytes: &[u8], arguments_end: usize) -> usize {
    let terminator = skip_trivia(bytes, arguments_end, bytes.len());
    if bytes.get(terminator) == Some(&b';') {
        terminator + 1
    } else {
        arguments_end
    }
}

pub(crate) fn skip_trivia(bytes: &[u8], mut offset: usize, scan_end: usize) -> usize {
    loop {
        while offset < scan_end && bytes[offset].is_ascii_whitespace() {
            offset += 1;
        }

        if bytes.get(offset) == Some(&b'/') && bytes.get(offset + 1) == Some(&b'*') {
            offset = scan_block_comment(bytes, offset, scan_end);
            continue;
        }

        return offset;
    }
}

pub(crate) fn scan_instance_id(
    bytes: &[u8],
    mut offset: usize,
    scan_end: usize,
) -> Option<(u32, usize)> {
    if bytes.get(offset) != Some(&b'#') {
        return None;
    }
    offset += 1;
    let digit_start = offset;
    while offset < scan_end && bytes[offset].is_ascii_digit() {
        offset += 1;
    }
    // IDs beyond u32::MAX are rejected (matches document.definitions' u32 key type);
    // not a realistic limit for STEP/IFC instance ids.
    let id = std::str::from_utf8(bytes.get(digit_start..offset)?)
        .ok()?
        .parse()
        .ok()?;
    Some((id, offset))
}

pub(crate) fn scan_identifier(bytes: &[u8], mut offset: usize, scan_end: usize) -> Option<usize> {
    if offset >= scan_end || !(bytes[offset].is_ascii_alphabetic() || bytes[offset] == b'_') {
        return None;
    }

    offset += 1;
    while offset < scan_end && (bytes[offset].is_ascii_alphanumeric() || bytes[offset] == b'_') {
        offset += 1;
    }
    Some(offset)
}

pub(crate) fn scan_string(bytes: &[u8], mut offset: usize, scan_end: usize) -> usize {
    offset += 1;
    while offset < scan_end {
        if bytes[offset] == b'\'' {
            if bytes.get(offset + 1) == Some(&b'\'') && offset + 1 < scan_end {
                offset += 2;
            } else {
                return offset + 1;
            }
        } else {
            offset += 1;
        }
    }
    scan_end
}

pub(crate) fn scan_block_comment(bytes: &[u8], mut offset: usize, scan_end: usize) -> usize {
    offset += 2;
    while offset + 1 < scan_end {
        if bytes[offset] == b'*' && bytes[offset + 1] == b'/' {
            return offset + 2;
        }
        offset += 1;
    }
    scan_end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_instance_entity_name_and_argument_starts() {
        let text = "#1=IFCWALL('id', #2, $)";

        let (instance, end) = scan_instance(text, 0, text.len()).expect("should scan instance");

        assert_eq!(instance.entity_name, "IFCWALL");
        assert_eq!(instance.argument_starts(), vec![11, 17, 21]);
        assert_eq!(end, text.len());
    }

    #[test]
    fn scans_only_definition_entity_name() {
        let text = "#12 = IfcWall(";

        assert_eq!(
            scan_definition_entity_name(text, 0, text.len()).as_deref(),
            Some("IFCWALL")
        );
    }

    #[test]
    fn argument_context_rejects_statement_without_leading_hash() {
        let text = "12=IFCWALL(";
        assert!(entity_argument_context(text, text.len()).is_none());
    }

    #[test]
    fn argument_context_skips_leading_comments() {
        let text = "#1=IFCWALL($, /* explanation */ .SO";
        let context = entity_argument_context(text, text.len()).expect("expected argument context");

        assert_eq!(context.active_parameter, 1);
        assert_eq!(&text[context.value_start..], " .SO");
    }

    #[test]
    fn ids_in_span_skips_strings_and_comments() {
        let text = "(#2, 'not #9', /* #7 */ #3)";

        let ids = ids_in_span(text, 0, text.len());

        assert_eq!(ids, vec![2, 3]);
    }

    #[test]
    fn argument_spans_trim_whitespace_and_keep_empty_arguments() {
        let text = "( 'a,(b' , (#2,#3) ,, /* c */ X )";

        let (spans, end) = scan_argument_spans(text.as_bytes(), 1, text.len());

        let arguments: Vec<&str> = spans
            .iter()
            .map(|&(start, end)| &text[start..end])
            .collect();
        assert_eq!(arguments, ["'a,(b'", "(#2,#3)", "", "X"]);
        assert_eq!(end, text.len());
    }

    #[test]
    fn an_empty_argument_list_has_no_argument_spans() {
        let text = "( )";

        assert!(
            scan_argument_spans(text.as_bytes(), 1, text.len())
                .0
                .is_empty()
        );
    }

    #[test]
    fn argument_starts_skip_empty_arguments_but_spans_keep_them() {
        let text = "#1=IFCX(a,,b)";

        let (instance, _) = scan_instance(text, 0, text.len()).expect("should scan instance");

        assert_eq!(instance.argument_starts(), vec![8, 11]);
        assert_eq!(instance.arguments, vec![(8, 9), (10, 10), (11, 12)]);
    }

    #[test]
    fn reads_string_literals() {
        assert_eq!(string_literal("'it''s'", 0).as_deref(), Some("it's"));
        assert_eq!(string_literal("''", 0).as_deref(), Some(""));
        assert_eq!(string_literal("$", 0), None);
        assert_eq!(
            string_literal(r"'D\S\|sseldorf \\ M\X\FCnchen'", 0).as_deref(),
            Some(r"Düsseldorf \ München")
        );
    }

    #[test]
    fn ids_in_span_reads_single_scalar_reference() {
        let text = "#12";

        let ids = ids_in_span(text, 0, text.len());

        assert_eq!(ids, vec![12]);
    }
}
