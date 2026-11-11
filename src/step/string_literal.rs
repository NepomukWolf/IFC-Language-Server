//! Decoding of the content of STEP string literals (ISO 10303-21 §6.4.3).

/// Decodes the content between a string literal's quotes: `''`, `\\`, `\S\` (Latin-1), `\X\hh`,
/// `\X2\…\X0\` and `\X4\…\X0\`. Malformed directives are kept as written.
pub fn decode_string(content: &str) -> String {
    let mut decoded = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(index) = rest.find(['\\', '\'']) {
        decoded.push_str(&rest[..index]);
        rest = &rest[index..];
        let consumed = decode_escape(rest, &mut decoded).unwrap_or_else(|| {
            decoded.push_str(&rest[..1]);
            1
        });
        rest = &rest[consumed..];
    }
    decoded.push_str(rest);
    decoded
}

/// Decodes the escape at the start of `text` into `decoded`, returning its length in bytes.
fn decode_escape(text: &str, decoded: &mut String) -> Option<usize> {
    if text.starts_with("''") {
        decoded.push('\'');
        Some(2)
    } else if text.starts_with(r"\\") {
        decoded.push('\\');
        Some(2)
    } else if let Some(rest) = text.strip_prefix(r"\S\") {
        let code = *rest.as_bytes().first()?;
        if !(0x20..0x80).contains(&code) {
            return None;
        }
        decoded.push(char::from(code + 0x80));
        Some(4)
    } else if let Some(rest) = text.strip_prefix(r"\X2\") {
        let (units, length) = hex_groups(rest, 4)?;
        let units = units.into_iter().map(|unit| unit as u16);
        decoded.extend(char::decode_utf16(units).map(|unit| unit.unwrap_or('\u{fffd}')));
        Some(4 + length)
    } else if let Some(rest) = text.strip_prefix(r"\X4\") {
        let (units, length) = hex_groups(rest, 8)?;
        let chars: Option<Vec<char>> = units.into_iter().map(char::from_u32).collect();
        decoded.extend(chars?);
        Some(4 + length)
    } else if let Some(rest) = text.strip_prefix(r"\X\") {
        let code = u8::from_str_radix(rest.get(..2)?, 16).ok()?;
        decoded.push(char::from(code));
        Some(5)
    } else {
        None
    }
}

/// The values of the `digits`-digit hex groups at the start of `text` up to the `\X0\` that ends
/// them, with the length of the groups and terminator in bytes.
fn hex_groups(text: &str, digits: usize) -> Option<(Vec<u32>, usize)> {
    let mut values = Vec::new();
    let mut offset = 0;
    loop {
        let rest = &text[offset..];
        if rest.starts_with(r"\X0\") {
            return Some((values, offset + 4));
        }
        let group = rest.get(..digits)?;
        if !group.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        values.push(u32::from_str_radix(group, 16).ok()?);
        offset += digits;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_doubled_apostrophes_and_backslashes() {
        assert_eq!(decode_string(r"it''s a \\ b"), r"it's a \ b");
    }

    #[test]
    fn decodes_latin_1_escapes() {
        assert_eq!(decode_string(r"D\S\|sseldorf \S\\"), "Düsseldorf Ü");
        assert_eq!(decode_string(r"M\X\FCnchen"), "München");
    }

    #[test]
    fn keeps_page_directives_as_written() {
        assert_eq!(decode_string(r"\PB\x"), r"\PB\x");
    }

    #[test]
    fn decodes_two_and_four_byte_groups() {
        assert_eq!(decode_string(r"M\X2\00FC\X0\nchen"), "München");
        assert_eq!(decode_string(r"\X2\041F04400438\X0\"), "При");
        assert_eq!(decode_string(r"\X2\D83DDE00\X0\"), "😀");
        assert_eq!(decode_string(r"\X4\0001F600\X0\"), "😀");
    }

    #[test]
    fn keeps_malformed_directives() {
        for text in [
            r"a\b",
            r"\X2\00F\X0\",
            r"\X2\00FC",
            r"\X\G1",
            r"\S\",
            r"\X4\FFFFFFFF\X0\",
        ] {
            assert_eq!(decode_string(text), text);
        }
    }
}
