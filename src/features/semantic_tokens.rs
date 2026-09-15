//! Semantic-token feature handler.
//! This module converts lexer output into LSP semantic-token deltas while keeping the lexer free
//! of protocol types.

mod lexer;

use tower_lsp::lsp_types::{
    Position, Range, SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens,
    SemanticTokensLegend,
};

use crate::document::Document;
use lexer::{LexedToken, TokenKind};

pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::KEYWORD,
            SemanticTokenType::CLASS,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::STRING,
            SemanticTokenType::NUMBER,
            SemanticTokenType::ENUM_MEMBER,
            SemanticTokenType::OPERATOR,
            SemanticTokenType::COMMENT,
        ],
        token_modifiers: Vec::<SemanticTokenModifier>::new(),
    }
}

pub fn semantic_tokens_range(document: &Document, range: Range) -> Option<SemanticTokens> {
    let emit_start = document.position_to_offset(range.start)?;
    let emit_end = document.position_to_offset(range.end)?;
    if emit_end < emit_start {
        return None;
    }

    let scan_start = document
        .line_offsets
        .get(range.start.line as usize)
        .copied()
        .unwrap_or(emit_start);
    let raw_tokens = lexer::lex_range(&document.text, scan_start, emit_end, emit_start, emit_end);

    Some(SemanticTokens {
        result_id: None,
        data: encode_tokens(document, &raw_tokens, emit_start, emit_end)?,
    })
}

fn encode_tokens(
    document: &Document,
    raw_tokens: &[LexedToken],
    emit_start: usize,
    emit_end: usize,
) -> Option<Vec<SemanticToken>> {
    let mut encoded = Vec::new();
    let mut previous_start: Option<Position> = None;

    for raw_token in raw_tokens {
        for (start, end) in split_token(document, raw_token, emit_start, emit_end)? {
            let start_position = document.offset_to_position(start)?;
            let end_position = document.offset_to_position(end)?;
            if start_position.line != end_position.line
                || end_position.character <= start_position.character
            {
                continue;
            }

            let (delta_line, delta_start) = match previous_start {
                Some(previous) if previous.line == start_position.line => (
                    0,
                    start_position.character.saturating_sub(previous.character),
                ),
                Some(previous) => (
                    start_position.line.saturating_sub(previous.line),
                    start_position.character,
                ),
                None => (start_position.line, start_position.character),
            };

            encoded.push(SemanticToken {
                delta_line,
                delta_start,
                length: end_position.character - start_position.character,
                token_type: token_type_index(raw_token.kind),
                token_modifiers_bitset: 0,
            });
            previous_start = Some(start_position);
        }
    }

    Some(encoded)
}

fn split_token(
    document: &Document,
    token: &LexedToken,
    emit_start: usize,
    emit_end: usize,
) -> Option<Vec<(usize, usize)>> {
    let start = token.start.max(emit_start);
    let end = token.end.min(emit_end);
    if start >= end {
        return Some(Vec::new());
    }

    let start_line = line_index_for_offset(&document.line_offsets, start)?;
    let end_line = line_index_for_offset(&document.line_offsets, end)?;
    let mut ranges = Vec::new();

    for line in start_line..=end_line {
        let line_start = document.line_offsets[line];
        let line_end = document.line_end_offset(line)?;
        let segment_start = start.max(line_start);
        let segment_end = end.min(line_end);
        if segment_start < segment_end {
            ranges.push((segment_start, segment_end));
        }
    }

    Some(ranges)
}

fn line_index_for_offset(line_offsets: &[usize], offset: usize) -> Option<usize> {
    match line_offsets.binary_search(&offset) {
        Ok(line) => Some(line),
        Err(next_line) => next_line.checked_sub(1),
    }
}

fn token_type_index(kind: TokenKind) -> u32 {
    match kind {
        TokenKind::Keyword => 0,
        TokenKind::Class => 1,
        TokenKind::Variable => 2,
        TokenKind::String => 3,
        TokenKind::Number => 4,
        TokenKind::EnumMember => 5,
        TokenKind::Operator => 6,
        TokenKind::Comment => 7,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_range_tokens_with_absolute_positions() {
        let text = "DATA;\n#1=IFCWALL(#2);\n#2=IFCDOOR();";
        let document = Document::new_unloaded(text.to_string());
        let range = Range {
            start: Position::new(1, 0),
            end: Position::new(2, 0),
        };

        let tokens = semantic_tokens_range(&document, range).unwrap();

        assert_eq!(tokens.data[0].delta_line, 1);
        assert_eq!(tokens.data[0].delta_start, 0);
        assert_eq!(tokens.data[0].length, 2);
        assert_eq!(
            tokens.data[0].token_type,
            token_type_index(TokenKind::Variable)
        );
        assert!(
            tokens
                .data
                .iter()
                .all(|token| token.delta_line == 0 || token.delta_line == 1)
        );
    }

    #[test]
    fn clamps_token_that_intersects_requested_range() {
        let text = "#1=IFCWALL();";
        let document = Document::new_unloaded(text.to_string());
        let range = Range {
            start: Position::new(0, 3),
            end: Position::new(0, 7),
        };

        let tokens = semantic_tokens_range(&document, range).unwrap();

        assert_eq!(tokens.data[0].delta_line, 0);
        assert_eq!(tokens.data[0].delta_start, 3);
        assert_eq!(tokens.data[0].length, 4);
        assert_eq!(
            tokens.data[0].token_type,
            token_type_index(TokenKind::Class)
        );
    }

    #[test]
    fn splits_multiline_comment_tokens() {
        let text = "/* first\nsecond */\n#1=IFCWALL();";
        let document = Document::new_unloaded(text.to_string());
        let range = Range {
            start: Position::new(0, 0),
            end: Position::new(2, 0),
        };

        let tokens = semantic_tokens_range(&document, range).unwrap();

        assert_eq!(
            tokens.data[0].token_type,
            token_type_index(TokenKind::Comment)
        );
        assert_eq!(tokens.data[0].length, 8);
        assert_eq!(tokens.data[1].delta_line, 1);
        assert_eq!(tokens.data[1].delta_start, 0);
        assert_eq!(
            tokens.data[1].token_type,
            token_type_index(TokenKind::Comment)
        );
    }
}
