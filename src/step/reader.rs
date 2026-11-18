use super::{Kind, Param, ParseError, Span};

pub(super) struct Reader<'a, 'p> {
    pub(super) text: &'a [u8],
    pub(super) p: usize,
    pub(super) end: usize,
    pub(super) params: &'p mut Vec<Param>,
    pub(super) retain: bool,
    pub(super) depth: usize,
    /// Set when the outer list lacked only its `)` and its parameters were kept.
    pub(super) recovered: bool,
    /// Malformed values left out of their list.
    pub(super) skipped: Vec<ParseError>,
    pub(super) ahead: Lookahead,
}

/// Deepest nesting of lists and typed values read. Decoded values are walked recursively, so this
/// keeps every consumer within a small (wasm) stack.
pub(crate) const MAX_NESTING: usize = 256;
pub(crate) const NESTING_TOO_DEEP: &str = "nesting too deep";
pub(crate) const SECTION_REOPENED: &str = "section opened before the previous one ended";

/// Errors confined to one scalar value, which `Reader::skip` can step over.
const SKIPPABLE: [&str; 7] = [
    "expected parameter",
    "invalid reference",
    "invalid enumeration",
    "unclosed enumeration",
    "invalid binary",
    "invalid exponent",
    "expected (",
];

impl Reader<'_, '_> {
    pub(super) fn error(&self, message: &'static str) -> ParseError {
        ParseError {
            span: Span {
                start: self.p as u32,
                end: self.p.saturating_add(1).min(self.end) as u32,
            },
            message,
        }
    }
    pub(super) fn expect(&mut self, byte: u8) -> Result<(), ParseError> {
        let before_trivia = self.p;
        self.p = trivia(self.text, self.p, self.end, &mut self.ahead);
        if self.p >= self.end || self.text[self.p] != byte {
            let mut error = self.error(match byte {
                b'(' => "expected (",
                b')' => "expected )",
                b';' => "expected ;",
                b'=' => "expected =",
                _ => "expected delimiter",
            });
            if byte == b';' && self.p >= self.end {
                error.span.start = before_trivia as u32;
                error.span.end = before_trivia as u32;
            } else if byte == b')' && self.text.get(self.p) == Some(&b';') {
                error.span.end = error.span.start;
            }
            return Err(error);
        }
        self.p += 1;
        Ok(())
    }
    pub(super) fn name(&mut self) -> Result<(), ParseError> {
        if self.p >= self.end || !self.text[self.p].is_ascii_alphabetic() {
            return Err(self.error("expected name"));
        }
        while self.p < self.end
            && (self.text[self.p].is_ascii_alphanumeric() || self.text[self.p] == b'_')
        {
            self.p += 1;
        }
        Ok(())
    }
    pub(super) fn list(&mut self, kind: Kind) -> Result<(), ParseError> {
        self.p = trivia(self.text, self.p, self.end, &mut self.ahead);
        let start = self.p;
        self.expect(b'(')?;
        self.list_body(start, kind)
    }
    fn list_body(&mut self, start: usize, kind: Kind) -> Result<(), ParseError> {
        if self.depth >= MAX_NESTING {
            return Err(ParseError {
                span: Span {
                    start: start as u32,
                    end: start as u32 + 1,
                },
                message: NESTING_TOO_DEEP,
            });
        }
        self.depth += 1;
        let slot = self.params.len();
        if self.retain {
            self.params.push(Param {
                span: Span::default(),
                next: 0,
                kind,
            });
        }
        self.p = trivia(self.text, self.p, self.end, &mut self.ahead);
        if self.text.get(self.p) != Some(&b')') {
            let mut comma = None;
            loop {
                let value_start = trivia(self.text, self.p, self.end, &mut self.ahead);
                let mark = self.params.len();
                if let Err(error) = self.value() {
                    if !SKIPPABLE.contains(&error.message) {
                        return Err(error);
                    }
                    self.params.truncate(mark);
                    self.p = value_start;
                    let end = self.skip(comma.unwrap_or(value_start))?;
                    if self.retain {
                        self.params.push(Param {
                            span: Span {
                                start: value_start as u32,
                                end: end.max(value_start) as u32,
                            },
                            next: self.params.len() as u32 + 1,
                            kind: Kind::Invalid,
                        });
                    }
                }
                self.p = trivia(self.text, self.p, self.end, &mut self.ahead);
                if self.p < self.end && !matches!(self.text[self.p], b',' | b')' | b';') {
                    self.skip(self.p)?;
                }
                if self.p < self.end && self.text[self.p] == b',' {
                    comma = Some(self.p);
                    self.p += 1;
                } else {
                    break;
                }
            }
        }
        let closed = self.expect(b')');
        let recoverable = self.depth == 1 && (self.p >= self.end || self.text[self.p] == b';');
        if closed.is_err() {
            if !recoverable {
                return closed;
            }
            self.recovered = true;
        }
        if self.retain {
            self.params[slot] = Param {
                span: Span {
                    start: start as u32,
                    end: self.p as u32,
                },
                next: self.params.len() as u32,
                kind,
            };
        }
        self.depth -= 1;
        closed
    }
    /// Skips one malformed value up to the next `,`, `)` or `;` of its list, records it and
    /// returns where it ends.
    fn skip(&mut self, error_start: usize) -> Result<usize, ParseError> {
        let mut depth = 0usize;
        let mut end = self.p;
        while self.p < self.end {
            match self.text[self.p] {
                b'/' if self.text.get(self.p..self.p + 2) == Some(b"/*") => {
                    self.p = self
                        .ahead
                        .comment_end(self.text, self.p, self.end)
                        .unwrap_or(self.end);
                    continue;
                }
                byte if byte.is_ascii_whitespace() => {
                    self.p += 1;
                    continue;
                }
                quote @ (b'\'' | b'"') => {
                    let start = self.p;
                    self.p += 1;
                    loop {
                        let Some(n) = memchr::memchr(quote, &self.text[self.p..self.end]) else {
                            self.p = start;
                            return Err(self.error("unclosed string"));
                        };
                        self.p += n + 1;
                        if quote != b'\'' || self.text.get(self.p) != Some(&quote) {
                            break;
                        }
                        self.p += 1;
                    }
                    end = self.p;
                    continue;
                }
                b'(' => depth += 1,
                b')' if depth == 0 => break,
                b')' => depth -= 1,
                b',' | b';' if depth == 0 => break,
                _ => {}
            }
            self.p += 1;
            end = self.p;
        }
        while end > error_start && self.text[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        self.skipped.push(ParseError {
            span: Span {
                start: error_start as u32,
                end: end as u32,
            },
            message: "unexpected value",
        });
        Ok(end)
    }
    fn value(&mut self) -> Result<(), ParseError> {
        self.p = trivia(self.text, self.p, self.end, &mut self.ahead);
        if self.p >= self.end {
            return Err(self.error("expected parameter"));
        }
        let start = self.p;
        let kind = match self.text[self.p] {
            b'(' => {
                return self.list(Kind::List);
            }
            b'\'' | b'"' => {
                let quote = self.text[self.p];
                self.p += 1;
                loop {
                    let Some(n) = memchr::memchr(quote, &self.text[self.p..self.end]) else {
                        self.p = start;
                        return Err(self.error("unclosed string"));
                    };
                    self.p += n + 1;
                    if quote == b'\'' && self.p < self.end && self.text[self.p] == quote {
                        self.p += 1;
                    } else {
                        break;
                    }
                }
                if quote == b'\'' {
                    Kind::String
                } else {
                    let content = &self.text[start + 1..self.p - 1];
                    if content
                        .first()
                        .is_none_or(|byte| !matches!(byte, b'0'..=b'3'))
                        || !content[1..].iter().all(u8::is_ascii_hexdigit)
                    {
                        self.p = start;
                        return Err(self.error("invalid binary"));
                    }
                    Kind::Binary
                }
            }
            b'$' => {
                self.p += 1;
                Kind::Null
            }
            b'*' => {
                self.p += 1;
                Kind::Omitted
            }
            b'#' => {
                self.p += 1;
                let digits = self.p;
                while self.p < self.end && self.text[self.p].is_ascii_digit() {
                    self.p += 1;
                }
                if self.p == digits {
                    return Err(self.error("invalid reference"));
                }
                Kind::Reference
            }
            b'.' if self
                .text
                .get(self.p + 1)
                .is_some_and(u8::is_ascii_alphabetic) =>
            {
                self.p += 1;
                self.name()?;
                if self.text[start + 1..self.p]
                    .iter()
                    .any(u8::is_ascii_lowercase)
                {
                    self.p = start;
                    return Err(self.error("invalid enumeration"));
                }
                if self.text.get(self.p) != Some(&b'.') {
                    return Err(self.error("unclosed enumeration"));
                }
                self.p += 1;
                Kind::Enumeration
            }
            b if b.is_ascii_alphabetic() => {
                self.name()?;
                self.expect(b'(')?;
                return self.list_body(start, Kind::Typed);
            }
            _ => {
                if matches!(self.text[self.p], b'+' | b'-') {
                    self.p += 1;
                }
                let digits = self.p;
                while self.p < self.end && self.text[self.p].is_ascii_digit() {
                    self.p += 1;
                }
                let mut count = self.p - digits;
                if self.text.get(self.p) == Some(&b'.') {
                    self.p += 1;
                    let d = self.p;
                    while self.p < self.end && self.text[self.p].is_ascii_digit() {
                        self.p += 1;
                    }
                    count += self.p - d;
                }
                if count == 0 {
                    return Err(self.error("expected parameter"));
                }
                if matches!(self.text.get(self.p), Some(b'E' | b'e')) {
                    self.p += 1;
                    if matches!(self.text.get(self.p), Some(b'+' | b'-')) {
                        self.p += 1;
                    }
                    let d = self.p;
                    while self.p < self.end && self.text[self.p].is_ascii_digit() {
                        self.p += 1;
                    }
                    if self.p == d {
                        return Err(self.error("invalid exponent"));
                    }
                }
                Kind::Number
            }
        };
        if self.retain {
            let next = self.params.len() as u32 + 1;
            self.params.push(Param {
                span: Span {
                    start: start as u32,
                    end: self.p as u32,
                },
                next,
                kind,
            });
        }
        Ok(())
    }
}

/// Forward searches that lookahead repeats from nearby offsets of one text, each remembering its
/// last answer, so that a byte is scanned about once while the starts grow.
#[derive(Default)]
pub(crate) struct Lookahead {
    comment_close: Option<Found>,
    line_break: Option<Found>,
    /// The furthest offset a comment search has read up to: past the `*/` it found, or the end
    /// it searched to.
    comment_reach: usize,
}

/// The first match at or after `from` starts at `at`, or nowhere in the text when `at` is `None`.
#[derive(Clone, Copy)]
struct Found {
    from: usize,
    at: Option<usize>,
}

impl Lookahead {
    /// End of the `/*` comment at `p`, past its `*/`, if that lies within `end`.
    #[cold]
    #[inline(never)]
    fn comment_end(&mut self, text: &[u8], p: usize, end: usize) -> Option<usize> {
        let found = (p + 2 <= end)
            .then(|| {
                first_match(&mut self.comment_close, text, p + 2, end, 2, |hay| {
                    memchr::memmem::find(hay, b"*/")
                })
            })
            .flatten()
            .map(|at| at + 2);
        self.comment_reach = self.comment_reach.max(found.unwrap_or(end));
        found
    }

    /// Whether a line break lies in `text[from..to]`.
    pub(crate) fn line_break(&mut self, text: &[u8], from: usize, to: usize) -> bool {
        first_match(&mut self.line_break, text, from, text.len(), 1, |hay| {
            memchr::memchr(b'\n', hay)
        })
        .is_some_and(|at| at < to)
    }
}

/// Start of the first `len`-byte match of `search` at or after `from` that ends within `end`.
fn first_match(
    found: &mut Option<Found>,
    text: &[u8],
    from: usize,
    end: usize,
    len: usize,
    search: impl Fn(&[u8]) -> Option<usize>,
) -> Option<usize> {
    // What the search finds past `limit` when it finds nothing before it, if that is known.
    let (limit, beyond) = match *found {
        Some(known) if known.from <= from && known.at.is_none_or(|at| from <= at) => {
            return known.at.filter(|&at| at + len <= end);
        }
        Some(known) if from < known.from && known.from + len - 1 < end => {
            (known.from + len - 1, Some(known.at))
        }
        _ => (end, (end == text.len()).then_some(None)),
    };
    let at = match search(&text[from..limit]) {
        Some(n) => Some(from + n),
        None => beyond?,
    };
    *found = Some(Found { from, at });
    at.filter(|&at| at + len <= end)
}

pub(crate) fn trivia(text: &[u8], mut p: usize, end: usize, ahead: &mut Lookahead) -> usize {
    while p < end {
        if text[p].is_ascii_whitespace() {
            p += 1;
        } else if text.get(p..p + 2) == Some(b"/*") {
            p = ahead.comment_end(text, p, end).unwrap_or(end);
        } else {
            break;
        }
    }
    p
}

/// `trivia` up to `end`, or the start of a comment that opens before `end` and doesn't close
/// before it.
pub(super) fn closed_trivia(
    text: &[u8],
    mut p: usize,
    end: usize,
    ahead: &mut Lookahead,
) -> Result<usize, usize> {
    while p < end {
        if text[p].is_ascii_whitespace() {
            p += 1;
        } else if text.get(p..p + 2) == Some(b"/*") {
            p = ahead.comment_end(text, p, end).ok_or(p)?;
        } else {
            break;
        }
    }
    Ok(p)
}

/// Whether `#<digits> =` starts at `p`.
pub(crate) fn definition(text: &[u8], mut p: usize, end: usize, ahead: &mut Lookahead) -> bool {
    if text.get(p) != Some(&b'#') {
        return false;
    }
    p += 1;
    let start = p;
    while p < end && text[p].is_ascii_digit() {
        p += 1;
    }
    p > start && text.get(trivia(text, p, end, ahead)) == Some(&b'=')
}

/// Whether `ENDSEC ;` starts at `p`.
pub(crate) fn section_end(text: &[u8], p: usize, end: usize, ahead: &mut Lookahead) -> bool {
    text.get(p..p + 6)
        .is_some_and(|word| word.eq_ignore_ascii_case(b"ENDSEC"))
        && text.get(trivia(text, p + 6, end, ahead)) == Some(&b';')
}

/// Where the statement at `start` ends, or the start of a comment that opens in it and doesn't
/// close.
pub(super) fn frame_end(
    text: &[u8],
    start: usize,
    end: usize,
    ahead: &mut Lookahead,
) -> Result<usize, usize> {
    let mut p = start;
    let mut quote = 0;
    let mut next_stop = None;
    // How far past `p` to look for the next `;` or quote. Doubling it on each miss keeps the search
    // within about twice the frame's length, also when recovery ends the frame long before a stop.
    let mut window = 256;
    while p < end {
        let rest = &text[p..end];
        p += if quote == 0 {
            let stop = match next_stop {
                Some(stop) if stop >= p => stop,
                _ => {
                    let ahead = &rest[..rest.len().min(window)];
                    memchr::memchr3(b';', b'\'', b'"', ahead).map_or_else(
                        || {
                            window *= 2;
                            p + ahead.len()
                        },
                        |n| p + n,
                    )
                }
            };
            next_stop = Some(stop);
            memchr::memchr2(b'\n', b'/', &text[p..stop]).unwrap_or(stop - p)
        } else {
            memchr::memchr3(quote, b';', b'\n', rest).unwrap_or(rest.len())
        };
        if p == end {
            break;
        }
        // A definition-shaped or `ENDSEC;` line inside a string is treated as recovery, not string
        // content.
        if text[p] == b'\n' {
            let mut next = p + 1;
            while next < end && matches!(text[next], b' ' | b'\t' | b'\r') {
                next += 1;
            }
            if definition(text, next, end, ahead) || section_end(text, next, end, ahead) {
                return Ok(next);
            }
        }
        if quote != 0 {
            if text[p] == quote {
                if quote == b'\'' && text.get(p + 1) == Some(&quote) {
                    p += 2;
                    continue;
                }
                quote = 0;
            } else if text[p] == b';' {
                let next = trivia(text, p + 1, end, ahead);
                if definition(text, next, end, ahead) && ahead.line_break(text, p + 1, next) {
                    return Ok(p + 1);
                }
            }
        } else {
            match text[p] {
                b'\'' | b'"' => quote = text[p],
                b'/' if text.get(p + 1) == Some(&b'*') => {
                    p = closed_trivia(text, p, end, ahead)?;
                    continue;
                }
                b';' => return Ok(p + 1),
                _ => {}
            }
        }
        p += 1;
    }
    Ok(end)
}
