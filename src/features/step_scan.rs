//! Shared lightweight STEP entity-instance argument scanner.
//! Given a known `#id=` definition offset, this walks the positional argument list without a
//! full parse. Shared by inlay hints and related-entity navigation, which both only
//! need argument boundaries, enclosing entity names, and referenced ids within a bounded region.

use rustc_hash::FxHashMap;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ScannedInstance {
    pub(crate) entity_name: String,
    pub(crate) argument_starts: Vec<usize>,
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
    definitions: &FxHashMap<u32, usize>,
    cursor_offset: usize,
) -> Option<EntityArgumentContext> {
    if cursor_offset > text.len() || !text.is_char_boundary(cursor_offset) {
        return None;
    }

    let bytes = text.as_bytes();
    let mut offset = statement_start_before_cursor(text, definitions, cursor_offset);
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

fn statement_start_before_cursor(
    text: &str,
    definitions: &FxHashMap<u32, usize>,
    cursor_offset: usize,
) -> usize {
    let bytes = text.as_bytes();
    let second_nearest = memchr::memrchr_iter(b'\n', &bytes[..cursor_offset])
        .map(|newline| newline + 1)
        .chain(std::iter::once(0))
        .filter(|&start| {
            start < cursor_offset
                && scan_instance_id(bytes, start, bytes.len())
                    .is_some_and(|(id, _)| definitions.get(&id) == Some(&start))
        })
        .nth(1);

    // Indexed definitions lie outside strings and comments, so a scan starting at one sees the
    // same statement terminators as a scan from the start of the text.
    second_nearest
        .and_then(|anchor| last_statement_end(bytes, anchor, cursor_offset))
        .or_else(|| last_statement_end(bytes, 0, cursor_offset))
        .unwrap_or(0)
}

fn last_statement_end(bytes: &[u8], mut offset: usize, cursor_offset: usize) -> Option<usize> {
    let mut statement_start = None;

    while offset < cursor_offset {
        match bytes[offset] {
            b'\'' => offset = scan_string(bytes, offset, cursor_offset),
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                offset = scan_block_comment(bytes, offset, cursor_offset);
            }
            b';' => {
                offset += 1;
                statement_start = Some(offset);
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

    let (argument_starts, end) = scan_arguments(bytes, offset + 1, scan_end);
    Some((
        ScannedInstance {
            entity_name,
            argument_starts,
        },
        end,
    ))
}

/// Reads only the entity name, as written, at a known `#id=` definition offset.
pub(crate) fn scan_definition_entity_name(
    text: &str,
    start: usize,
    scan_end: usize,
) -> Option<&str> {
    let bytes = text.as_bytes();
    let (_, mut offset) = scan_instance_id(bytes, start, scan_end)?;
    offset = skip_trivia(bytes, offset, scan_end);
    if bytes.get(offset) != Some(&b'=') {
        return None;
    }

    offset = skip_trivia(bytes, offset + 1, scan_end);
    let entity_start = offset;
    offset = scan_identifier(bytes, offset, scan_end)?;
    text.get(entity_start..offset)
}

/// Collects every local instance id (`#N`) referenced within `[start, end)`, skipping strings
/// and block comments. Intended for reading the ids inside a single already-located argument.
pub(crate) fn ids_in_span(text: &str, start: usize, end: usize) -> Vec<u32> {
    let bytes = text.as_bytes();
    let end = end.min(bytes.len());
    let mut offset = start.min(end);
    let mut ids = Vec::new();

    while offset < end {
        match bytes[offset] {
            b'#' => {
                if let Some((id, next)) = scan_instance_id(bytes, offset, end) {
                    ids.push(id);
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

pub(crate) fn scan_arguments(
    bytes: &[u8],
    mut offset: usize,
    scan_end: usize,
) -> (Vec<usize>, usize) {
    let mut argument_starts = Vec::new();
    let mut expecting_argument = true;
    let mut depth = 0usize;

    while offset < scan_end {
        if expecting_argument {
            offset = skip_trivia(bytes, offset, scan_end);
            match bytes.get(offset) {
                Some(b')') if depth == 0 => return (argument_starts, offset + 1),
                Some(b',') if depth == 0 => {
                    offset += 1;
                    continue;
                }
                None => break,
                _ => {
                    argument_starts.push(offset);
                    expecting_argument = false;
                }
            }
        }

        match bytes[offset] {
            b'\'' => offset = scan_string(bytes, offset, scan_end),
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                offset = scan_block_comment(bytes, offset, scan_end);
            }
            b'(' => {
                depth += 1;
                offset += 1;
            }
            b')' if depth == 0 => return (argument_starts, offset + 1),
            b')' => {
                depth -= 1;
                offset += 1;
            }
            b',' if depth == 0 => {
                expecting_argument = true;
                offset += 1;
            }
            _ => offset += 1,
        }
    }

    (argument_starts, scan_end)
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
        assert_eq!(instance.argument_starts, vec![11, 17, 21]);
        assert_eq!(end, text.len());
    }

    #[test]
    fn scans_only_definition_entity_name() {
        let text = "#12 = IfcWall(";

        assert_eq!(
            scan_definition_entity_name(text, 0, text.len()),
            Some("IfcWall")
        );
    }

    #[test]
    fn argument_context_rejects_statement_without_leading_hash() {
        let text = "12=IFCWALL(";
        assert!(entity_argument_context(text, &FxHashMap::default(), text.len()).is_none());
    }

    #[test]
    fn argument_context_skips_leading_comments() {
        let text = "#1=IFCWALL($, /* explanation */ .SO";
        let context = entity_argument_context(text, &FxHashMap::default(), text.len())
            .expect("expected argument context");

        assert_eq!(context.active_parameter, 1);
        assert_eq!(&text[context.value_start..], " .SO");
    }

    #[test]
    fn argument_context_from_indexed_definitions_matches_scan_from_start() {
        let text = "ISO-10303-21;\nHEADER;FILE_SCHEMA(('IFC4'));ENDSEC;\nDATA;\n\
            #1=IFCWALL('a;b', /* #9=X(; */ #2, $);\n\
            #2 = IFCSLAB('#3=IFCBEAM(', (#1, #4));junk #4=IFCBEAM('it''s;', .T.);\n\
            #5=IFCROOF($, 'multi\nline; #6=IFCX(', (#1,\n#2));\n\
            #1=IFCWALL(#4, /* open; comment";
        let definitions = crate::document_index::scan_text(text).definitions;
        assert!(definitions.len() >= 4);

        for cursor in 0..=text.len() {
            assert_eq!(
                entity_argument_context(text, &definitions, cursor),
                entity_argument_context(text, &FxHashMap::default(), cursor),
                "cursor {cursor}"
            );
        }
    }

    #[test]
    fn ids_in_span_skips_strings_and_comments() {
        let text = "(#2, 'not #9', /* #7 */ #3)";

        let ids = ids_in_span(text, 0, text.len());

        assert_eq!(ids, vec![2, 3]);
    }

    #[test]
    fn ids_in_span_reads_single_scalar_reference() {
        let text = "#12";

        let ids = ids_in_span(text, 0, text.len());

        assert_eq!(ids, vec![12]);
    }
}
