//! Shared lightweight STEP entity-instance argument scanner.
//! Given a known `#id=` definition offset, this walks the positional argument list without a
//! full tree-sitter parse. Shared by inlay hints and related-entity navigation, which both only
//! need argument boundaries, enclosing entity names, and referenced ids within a bounded region.

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ScannedInstance {
    pub(crate) entity_name: String,
    pub(crate) argument_starts: Vec<usize>,
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
    offset += 1;
    let digit_start = offset;
    while offset < scan_end && bytes[offset].is_ascii_digit() {
        offset += 1;
    }
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
