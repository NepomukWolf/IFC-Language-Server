use super::{EntityInstanceInfo, ParameterValue, typed_name_len};
use crate::step;
use tower_lsp::lsp_types::{Position, Range};

/// Decodes one statement with positions relative to its start: row 0 columns count from the
/// statement's first byte, later rows keep absolute columns.
pub(crate) fn decode_entity_instance_relative(
    parsed: &step::Document,
    text: &str,
    index: usize,
) -> Option<EntityInstanceInfo> {
    let start = parsed.statements[index].span.start as usize;
    let mut points = LinePoints {
        text: text.as_bytes(),
        offset: start,
        row: 0,
        column: 0,
    };
    decode_entity_instance(parsed, text, index, &mut points)
        .filter(|instance| instance.parameter_list_range.is_some())
}

#[cfg(test)]
thread_local! {
    pub(crate) static DECODED_STATEMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(super) fn decode_entity_instance(
    parsed: &step::Document,
    text: &str,
    index: usize,
    points: &mut LinePoints<'_>,
) -> Option<EntityInstanceInfo> {
    #[cfg(test)]
    DECODED_STATEMENTS.with(|count| count.set(count.get() + 1));
    let statement = parsed.statements[index];
    let span = statement.span.range();
    if statement.name.end == 0 {
        return None;
    }
    let tape = parsed.decode(text.as_bytes(), index);
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
    let (parameters, parameter_list_range) = match tape.first() {
        Some(list) => {
            let list_start = points.position(list.span.start as usize);
            let parameters = decode_parameter_values(&tape, 1, tape.len(), text, points);
            let range = Range {
                start: list_start,
                end: points.position(list.span.end as usize),
            };
            (parameters, Some(range))
        }
        None => (Vec::new(), None),
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
        parameter_list_range,
        parameters,
    })
}

fn decode_parameter_values(
    tape: &[step::Param],
    mut slot: usize,
    end: usize,
    text: &str,
    points: &mut LinePoints<'_>,
) -> Vec<ParameterValue> {
    use step::Kind;
    let mut values = Vec::new();
    while slot < end {
        let param = tape[slot];
        let span = param.span.range();
        let start = points.position(span.start);
        let children = match param.kind {
            Kind::List | Kind::Typed => {
                decode_parameter_values(tape, slot + 1, param.next as usize, text, points)
            }
            _ => Vec::new(),
        };
        let range = Range {
            start,
            end: points.position(span.end),
        };
        let source = &text[span];
        values.push(match param.kind {
            Kind::String => ParameterValue::String { range },
            Kind::Binary => ParameterValue::Binary { range },
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
            Kind::Invalid => ParameterValue::Malformed { range },
            Kind::Typed => ParameterValue::Typed {
                type_name: source[..typed_name_len(source)].to_ascii_uppercase(),
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

/// Converts monotonically increasing byte offsets to positions with UTF-16 columns, scanning
/// only the bytes between consecutive offsets.
pub(super) struct LinePoints<'a> {
    pub(super) text: &'a [u8],
    pub(super) offset: usize,
    pub(super) row: usize,
    pub(super) column: usize,
}

impl LinePoints<'_> {
    pub(super) fn position(&mut self, offset: usize) -> Position {
        let skipped = &self.text[self.offset..offset];
        match memchr::memrchr(b'\n', skipped) {
            Some(last) => {
                self.row += memchr::memchr_iter(b'\n', skipped).count();
                self.column = utf16_len(&skipped[last + 1..]);
            }
            None => self.column += utf16_len(skipped),
        }
        self.offset = offset;
        Position::new(self.row as u32, self.column as u32)
    }
}

/// UTF-16 code units in UTF-8 `bytes`: one per leading byte, two for a four-byte sequence.
pub(crate) fn utf16_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .map(|&byte| usize::from(byte & 0xC0 != 0x80) + usize::from(byte >= 0xF0))
        .sum()
}
