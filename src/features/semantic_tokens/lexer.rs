//! Lightweight IFC semantic-token lexer.
//! It scans borrowed document text and emits absolute byte ranges so callers can decide how to
//! convert tokens into protocol-specific positions.

use crate::features::step_scan::{scan_block_comment, scan_string};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    Class,
    Variable,
    String,
    Number,
    EnumMember,
    Operator,
    Comment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LexedToken {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

pub fn lex_range(
    text: &str,
    scan_start: usize,
    scan_end: usize,
    emit_start: usize,
    emit_end: usize,
) -> Vec<LexedToken> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut offset = scan_start;

    while offset < scan_end {
        let byte = bytes[offset];
        let token = match byte {
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                let end = scan_block_comment(bytes, offset, scan_end);
                Some((TokenKind::Comment, offset, end))
            }
            b'\'' => {
                let end = scan_string(bytes, offset, scan_end);
                Some((TokenKind::String, offset, end))
            }
            b'#' => scan_instance_id(bytes, offset, scan_end)
                .map(|end| (TokenKind::Variable, offset, end)),
            b'.' => scan_enum_member(bytes, offset, scan_end)
                .map(|end| (TokenKind::EnumMember, offset, end)),
            b'+' | b'-' if starts_number(bytes, offset, scan_end) => {
                let end = scan_number(bytes, offset, scan_end);
                Some((TokenKind::Number, offset, end))
            }
            byte if byte.is_ascii_digit() => {
                let end = scan_number(bytes, offset, scan_end);
                Some((TokenKind::Number, offset, end))
            }
            byte if is_identifier_start(byte) => {
                let end = scan_identifier(bytes, offset, scan_end);
                let kind = if is_keyword(&bytes[offset..end]) {
                    TokenKind::Keyword
                } else {
                    TokenKind::Class
                };
                Some((kind, offset, end))
            }
            b'=' | b';' | b',' | b'(' | b')' | b'$' | b'*' => {
                Some((TokenKind::Operator, offset, offset + 1))
            }
            _ => None,
        };

        if let Some((kind, start, end)) = token {
            push_if_intersects(&mut tokens, kind, start, end, emit_start, emit_end);
            offset = end;
        } else {
            offset += 1;
        }
    }

    tokens
}

fn push_if_intersects(
    tokens: &mut Vec<LexedToken>,
    kind: TokenKind,
    start: usize,
    end: usize,
    emit_start: usize,
    emit_end: usize,
) {
    if start < emit_end && end > emit_start {
        tokens.push(LexedToken { kind, start, end });
    }
}

fn scan_instance_id(bytes: &[u8], mut offset: usize, scan_end: usize) -> Option<usize> {
    offset += 1;
    let start = offset;
    while offset < scan_end && bytes[offset].is_ascii_digit() {
        offset += 1;
    }
    (offset > start).then_some(offset)
}

fn scan_enum_member(bytes: &[u8], mut offset: usize, scan_end: usize) -> Option<usize> {
    offset += 1;
    let start = offset;
    while offset < scan_end && is_identifier_part(bytes[offset]) {
        offset += 1;
    }
    if offset == start || bytes.get(offset) != Some(&b'.') {
        return None;
    }
    Some(offset + 1)
}

fn starts_number(bytes: &[u8], offset: usize, scan_end: usize) -> bool {
    offset + 1 < scan_end && bytes[offset + 1].is_ascii_digit()
}

fn scan_number(bytes: &[u8], mut offset: usize, scan_end: usize) -> usize {
    if matches!(bytes[offset], b'+' | b'-') {
        offset += 1;
    }

    while offset < scan_end && bytes[offset].is_ascii_digit() {
        offset += 1;
    }

    if bytes.get(offset) == Some(&b'.')
        && bytes
            .get(offset + 1)
            .is_some_and(|byte| byte.is_ascii_digit())
    {
        offset += 1;
        while offset < scan_end && bytes[offset].is_ascii_digit() {
            offset += 1;
        }
    }

    if matches!(bytes.get(offset), Some(b'E' | b'e')) {
        let exponent_start = offset;
        offset += 1;
        if matches!(bytes.get(offset), Some(b'+' | b'-')) {
            offset += 1;
        }
        let digit_start = offset;
        while offset < scan_end && bytes[offset].is_ascii_digit() {
            offset += 1;
        }
        if offset == digit_start {
            return exponent_start;
        }
    }

    offset
}

fn scan_identifier(bytes: &[u8], mut offset: usize, scan_end: usize) -> usize {
    offset += 1;
    while offset < scan_end && is_identifier_part(bytes[offset]) {
        offset += 1;
    }
    offset
}

/// STEP P21 structural keywords highlighted as keywords rather than entity names.
const STEP_KEYWORDS: [&[u8]; 8] = [
    b"ISO",
    b"HEADER",
    b"ENDSEC",
    b"DATA",
    b"END",
    b"FILE_DESCRIPTION",
    b"FILE_NAME",
    b"FILE_SCHEMA",
];

fn is_keyword(text: &[u8]) -> bool {
    STEP_KEYWORDS
        .iter()
        .any(|keyword| text.eq_ignore_ascii_case(keyword))
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
    fn lexes_ifc_instance_tokens() {
        let text = "#1=IFCWALL('A''B',#2,12.5,.T.,$,*);";

        let tokens = lex_range(text, 0, text.len(), 0, text.len());

        assert_eq!(
            tokens
                .iter()
                .map(|token| token.kind)
                .collect::<Vec<TokenKind>>(),
            vec![
                TokenKind::Variable,
                TokenKind::Operator,
                TokenKind::Class,
                TokenKind::Operator,
                TokenKind::String,
                TokenKind::Operator,
                TokenKind::Variable,
                TokenKind::Operator,
                TokenKind::Number,
                TokenKind::Operator,
                TokenKind::EnumMember,
                TokenKind::Operator,
                TokenKind::Operator,
                TokenKind::Operator,
                TokenKind::Operator,
                TokenKind::Operator,
                TokenKind::Operator,
            ]
        );
    }

    #[test]
    fn ignores_fake_tokens_inside_strings_and_comments() {
        let text = "'#1=IFCWALL';/* #2=IFCDOOR; */#3=IFCWINDOW();";

        let tokens = lex_range(text, 0, text.len(), 0, text.len());

        assert_eq!(tokens[0].kind, TokenKind::String);
        assert_eq!(tokens[1].kind, TokenKind::Operator);
        assert_eq!(tokens[2].kind, TokenKind::Comment);
        assert_eq!(tokens[3].kind, TokenKind::Variable);
        assert_eq!(&text[tokens[3].start..tokens[3].end], "#3");
    }

    #[test]
    fn only_emits_tokens_intersecting_range() {
        let text = "#1=IFCWALL();\n#2=IFCDOOR();";
        let range_start = text.find("#2").unwrap();

        let tokens = lex_range(text, 0, text.len(), range_start, text.len());

        assert_eq!(&text[tokens[0].start..tokens[0].end], "#2");
        assert!(tokens.iter().all(|token| token.end > range_start));
    }
}
