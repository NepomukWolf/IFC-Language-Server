#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }

    fn shift(&mut self, delta: i64) {
        self.start = (self.start as i64 + delta) as u32;
        self.end = (self.end as i64 + delta) as u32;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    List,
    Typed,
    Reference,
    String,
    Number,
    Enumeration,
    Binary,
    Null,
    Omitted,
    /// A malformed value the parser stepped over.
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Param {
    pub span: Span,
    pub next: u32,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Statement {
    pub id: u32,
    pub span: Span,
    pub name: Span,
    pub param_start: u32,
    pub param_len: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub span: Span,
    pub message: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    pub statements: Vec<Statement>,
    pub ids: Vec<(u32, u32)>,
    pub params: Vec<Param>,
    pub errors: Vec<ParseError>,
    pub header: Span,
    /// A `HEADER` or `DATA` section is still open where the text ends.
    pub unclosed_section: bool,
    pub retain: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Edit {
    pub start: usize,
    pub delete_len: usize,
    pub insert_len: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditOutcome {
    Rebuilt,
    /// Statements `first..first + old_len` were replaced by `first..first + new_len`.
    Local {
        first: usize,
        old_len: usize,
        new_len: usize,
        /// Ids whose resolved entity name may differ after the edit.
        changed_ids: Vec<u32>,
        reparsed_bytes: usize,
    },
}

impl Document {
    pub fn parse(text: &[u8], retain: bool) -> Self {
        assert!(text.len() < u32::MAX as usize);
        let mut doc = Self {
            statements: Vec::new(),
            ids: Vec::new(),
            params: Vec::new(),
            errors: Vec::new(),
            header: Span::default(),
            unclosed_section: false,
            retain,
        };
        let mut open_section = false;
        let mut p = if text.starts_with(b"\xEF\xBB\xBF") {
            3
        } else {
            0
        };
        let mut unclosed_comment = None;
        while p < text.len() {
            p = match closed_trivia(text, p, text.len()) {
                Ok(p) => p,
                Err(comment) => {
                    unclosed_comment = Some(comment);
                    break;
                }
            };
            if p == text.len() {
                break;
            }
            let errors = doc.errors.len();
            let frame = frame_end(text, p, text.len());
            let end = frame.unwrap_or_else(|comment| comment);
            if text[p] == b'#' {
                doc.statement(text, p, end);
            } else {
                let start = p;
                let word_end = text[start..end]
                    .iter()
                    .position(|b| !b.is_ascii_alphanumeric() && *b != b'-' && *b != b'_')
                    .map_or(end, |n| n + start);
                let word = &text[start..word_end];
                if word.eq_ignore_ascii_case(b"HEADER") {
                    doc.header.start = start as u32;
                }
                if word.eq_ignore_ascii_case(b"HEADER") || word.eq_ignore_ascii_case(b"DATA") {
                    if open_section {
                        doc.errors.push(ParseError {
                            span: Span {
                                start: start as u32,
                                end: word_end as u32,
                            },
                            message: SECTION_REOPENED,
                        });
                    }
                    open_section = true;
                } else if word.eq_ignore_ascii_case(b"ENDSEC") {
                    open_section = false;
                }
                if word.eq_ignore_ascii_case(b"ENDSEC")
                    && doc.header.end == 0
                    && doc.header.start != 0
                {
                    doc.header.end = end as u32;
                }
                let section = [
                    b"ISO-10303-21".as_slice(),
                    b"END-ISO-10303-21",
                    b"HEADER",
                    b"ENDSEC",
                    b"DATA",
                    b"ANCHOR",
                    b"REFERENCE",
                    b"SIGNATURE",
                ]
                .iter()
                .any(|w| word.eq_ignore_ascii_case(w));
                if !section {
                    let saved = doc.params.len();
                    let mut reader = Reader {
                        text,
                        p: word_end,
                        end,
                        params: &mut doc.params,
                        retain,
                        depth: 0,
                        recovered: false,
                        skipped: Vec::new(),
                    };
                    let result = reader.list(Kind::List).and_then(|()| reader.expect(b';'));
                    doc.errors.append(&mut reader.skipped);
                    if let Err(mut error) = result {
                        if error.message == "expected (" {
                            error.span = Span {
                                start: start as u32,
                                end: word_end.max(start + 1) as u32,
                            };
                            error.message = "unexpected token";
                        }
                        doc.errors.push(error);
                    }
                    doc.params.truncate(saved);
                }
            }
            p = end;
            if let Err(comment) = frame {
                doc.errors.truncate(errors);
                unclosed_comment = Some(comment);
                break;
            }
        }
        if let Some(comment) = unclosed_comment {
            doc.errors.push(ParseError {
                span: Span {
                    start: comment as u32,
                    end: comment as u32 + 2,
                },
                message: "unclosed comment",
            });
            // The comment may hide the section's `ENDSEC;`.
            open_section = false;
        }
        doc.unclosed_section = open_section;
        doc.errors
            .sort_by_key(|e| (e.span.start, e.span.end, e.message));
        doc.reindex();
        doc
    }

    fn reindex(&mut self) {
        self.ids.clear();
        self.ids.extend(
            self.statements
                .iter()
                .enumerate()
                .map(|(index, statement)| (statement.id, index as u32)),
        );
        self.ids.sort_unstable();
    }

    pub fn find(&self, id: u32) -> Option<usize> {
        let end = self.ids.partition_point(|&(candidate, _)| candidate <= id);
        end.checked_sub(1)
            .filter(|&index| self.ids[index].0 == id)
            .map(|index| self.ids[index].1 as usize)
    }

    fn statement(&mut self, text: &[u8], start: usize, end: usize) {
        let param_start = self.params.len() as u32;
        let mut statement = Statement {
            span: Span {
                start: start as u32,
                end: end as u32,
            },
            param_start,
            ..Statement::default()
        };
        let mut r = Reader {
            text,
            p: start + 1,
            end,
            params: &mut self.params,
            retain: self.retain,
            depth: 0,
            recovered: false,
            skipped: Vec::new(),
        };
        let result = (|| {
            let id_start = r.p;
            while r.p < end && text[r.p].is_ascii_digit() {
                r.p += 1;
            }
            statement.id = std::str::from_utf8(&text[id_start..r.p])
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| r.error("invalid instance id"))?;
            r.expect(b'=')?;
            r.p = trivia(text, r.p, end);
            let name_start = r.p;
            r.name()?;
            statement.name = Span {
                start: name_start as u32,
                end: r.p as u32,
            };
            if text[name_start..r.p].iter().any(u8::is_ascii_lowercase) {
                r.p = name_start;
                return Err(r.error("expected entity name"));
            }
            r.list(Kind::List)?;
            r.expect(b';')?;
            if trivia(text, r.p, end) != end {
                return Err(r.error("unexpected trailing text"));
            }
            Ok(())
        })();
        let recovered = r.recovered;
        self.errors.append(&mut r.skipped);
        if let Err(error) = result {
            self.errors.push(error);
            if !recovered {
                self.params.truncate(param_start as usize);
            }
        }
        statement.param_len = self.params.len() as u32 - param_start;
        self.statements.push(statement);
    }

    pub fn decode(&self, text: &[u8], index: usize) -> Vec<Param> {
        let s = self.statements[index];
        if self.retain {
            return self.params[s.param_start as usize..(s.param_start + s.param_len) as usize]
                .iter()
                .map(|p| Param {
                    next: p.next - s.param_start,
                    ..*p
                })
                .collect();
        }
        let mut params = Vec::new();
        let mut r = Reader {
            text,
            p: s.name.end as usize,
            end: s.span.end as usize,
            params: &mut params,
            retain: true,
            depth: 0,
            recovered: false,
            skipped: Vec::new(),
        };
        if r.list(Kind::List).is_err() && !r.recovered {
            params.clear();
        }
        params
    }

    pub fn edit(&mut self, removed: &[u8], new: &[u8], edit: Edit) -> EditOutcome {
        let first = self
            .statements
            .partition_point(|s| (s.span.end as usize) < edit.start)
            .saturating_sub(1);
        let last = (self
            .statements
            .partition_point(|s| s.span.start as usize <= edit.start + edit.delete_len)
            + 1)
        .min(self.statements.len());
        let gap = first
            .checked_sub(1)
            .map_or(0, |previous| self.statements[previous].span.end)
            ..self.statements.get(first).map_or(0, |s| s.span.start);
        if first >= last
            || edit.start < self.statements[first].span.start as usize
            || edit.start + edit.delete_len > self.statements[last - 1].span.end as usize
            // Text before the first statement that failed to parse may end there only by recovery,
            // which the edit can undo.
            || self.errors.iter().any(|e| gap.contains(&e.span.start))
        {
            *self = Self::parse(new, self.retain);
            return EditOutcome::Rebuilt;
        }
        let start = self.statements[first].span.start as usize;
        let old_end = self.statements[last - 1].span.end as usize;
        let delta = edit.insert_len as i64 - edit.delete_len as i64;
        let end = (old_end as i64 + delta) as usize;
        let mut local = Self {
            statements: Vec::new(),
            ids: Vec::new(),
            params: Vec::new(),
            errors: Vec::new(),
            header: self.header,
            unclosed_section: self.unclosed_section,
            retain: self.retain,
        };
        let mut p = start;
        while p < end {
            let Ok(after_trivia) = closed_trivia(new, p, end) else {
                *self = Self::parse(new, self.retain);
                return EditOutcome::Rebuilt;
            };
            p = after_trivia;
            if p == end {
                break;
            }
            let next = match frame_end(new, p, new.len()) {
                Ok(next) if new[p] == b'#' && next <= end => next,
                _ => {
                    *self = Self::parse(new, self.retain);
                    return EditOutcome::Rebuilt;
                }
            };
            local.statement(new, p, next);
            p = next;
        }
        let changed_ids = changed_ids(
            &self.statements[first..last],
            &local.statements,
            removed,
            new,
            edit,
            start,
            end,
        );
        let a = self.statements[first].param_start;
        let b = self.statements[last - 1].param_start + self.statements[last - 1].param_len;
        let param_delta = local.params.len() as i64 - (b - a) as i64;
        for param in &mut self.params[b as usize..] {
            param.span.shift(delta);
            param.next = (param.next as i64 + param_delta) as u32;
        }
        for param in &mut local.params {
            param.next += a;
        }
        for statement in &mut self.statements[last..] {
            statement.span.shift(delta);
            if statement.name.end != 0 {
                statement.name.shift(delta);
            }
            statement.param_start = (statement.param_start as i64 + param_delta) as u32;
        }
        for statement in &mut local.statements {
            statement.param_start += a;
        }
        self.params.splice(a as usize..b as usize, local.params);
        let new_len = local.statements.len();
        let same_ids = self.statements[first..last]
            .iter()
            .map(|s| s.id)
            .eq(local.statements.iter().map(|s| s.id));
        self.statements.splice(first..last, local.statements);
        // A replaced statement's error can end at `old_end` but never past it.
        self.errors
            .retain(|e| e.span.end as usize <= start || e.span.end as usize > old_end);
        for error in &mut self.errors {
            if error.span.end as usize > old_end {
                error.span.shift(delta);
            }
        }
        self.errors.extend(local.errors);
        self.errors
            .sort_by_key(|e| (e.span.start, e.span.end, e.message));
        if !same_ids {
            self.reindex();
        }
        EditOutcome::Local {
            first,
            old_len: last - first,
            new_len,
            changed_ids,
            reparsed_bytes: end - start,
        }
    }
}

/// Ids whose sequence of definitions (entity name, or none for an unnamed statement) differs
/// between the replaced statements and their reparse. `old` spans are in pre-edit offsets.
fn changed_ids(
    old: &[Statement],
    new_statements: &[Statement],
    removed: &[u8],
    new: &[u8],
    edit: Edit,
    start: usize,
    end: usize,
) -> Vec<u32> {
    let mut old_region = Vec::with_capacity(end - start + removed.len());
    old_region.extend_from_slice(&new[start..edit.start]);
    old_region.extend_from_slice(removed);
    old_region.extend_from_slice(&new[edit.start + edit.insert_len..end]);
    let name = |text: &[u8], base: usize, statement: &Statement| {
        (statement.name.end != 0).then(|| {
            text[statement.name.start as usize - base..statement.name.end as usize - base].to_vec()
        })
    };
    let mut before: Vec<_> = old
        .iter()
        .map(|s| (s.id, name(&old_region, start, s)))
        .collect();
    let mut after: Vec<_> = new_statements
        .iter()
        .map(|s| (s.id, name(new, 0, s)))
        .collect();
    before.sort_by_key(|&(id, _)| id);
    after.sort_by_key(|&(id, _)| id);
    let mut ids: Vec<u32> = before.iter().chain(&after).map(|&(id, _)| id).collect();
    ids.sort_unstable();
    ids.dedup();
    ids.retain(|&id| {
        let of = |defs: &[(u32, Option<Vec<u8>>)]| {
            let from = defs.partition_point(|&(other, _)| other < id);
            let to = defs.partition_point(|&(other, _)| other <= id);
            defs[from..to]
                .iter()
                .map(|(_, name)| name.clone())
                .collect::<Vec<_>>()
        };
        of(&before) != of(&after)
    });
    ids
}

struct Reader<'a, 'p> {
    text: &'a [u8],
    p: usize,
    end: usize,
    params: &'p mut Vec<Param>,
    retain: bool,
    depth: usize,
    /// Set when the outer list lacked only its `)` and its parameters were kept.
    recovered: bool,
    /// Malformed values left out of their list.
    skipped: Vec<ParseError>,
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
    fn error(&self, message: &'static str) -> ParseError {
        ParseError {
            span: Span {
                start: self.p as u32,
                end: self.p.saturating_add(1).min(self.end) as u32,
            },
            message,
        }
    }
    fn expect(&mut self, byte: u8) -> Result<(), ParseError> {
        let before_trivia = self.p;
        self.p = trivia(self.text, self.p, self.end);
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
    fn name(&mut self) -> Result<(), ParseError> {
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
    fn list(&mut self, kind: Kind) -> Result<(), ParseError> {
        self.p = trivia(self.text, self.p, self.end);
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
        self.p = trivia(self.text, self.p, self.end);
        if self.text.get(self.p) != Some(&b')') {
            let mut comma = None;
            loop {
                let value_start = trivia(self.text, self.p, self.end);
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
                self.p = trivia(self.text, self.p, self.end);
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
                    self.p = comment_end(self.text, self.p, self.end).unwrap_or(self.end);
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
        self.p = trivia(self.text, self.p, self.end);
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

pub(crate) fn trivia(text: &[u8], mut p: usize, end: usize) -> usize {
    while p < end {
        if text[p].is_ascii_whitespace() {
            p += 1;
        } else if text.get(p..p + 2) == Some(b"/*") {
            p = comment_end(text, p, end).unwrap_or(end);
        } else {
            break;
        }
    }
    p
}

/// End of the `/*` comment at `p`, past its `*/`.
#[cold]
#[inline(never)]
fn comment_end(text: &[u8], p: usize, end: usize) -> Option<usize> {
    memchr::memmem::find(&text[p + 2..end], b"*/").map(|n| p + n + 4)
}

/// `trivia` up to `end`, or the start of a comment that opens before `end` and doesn't close
/// before it.
fn closed_trivia(text: &[u8], mut p: usize, end: usize) -> Result<usize, usize> {
    while p < end {
        if text[p].is_ascii_whitespace() {
            p += 1;
        } else if text.get(p..p + 2) == Some(b"/*") {
            p = comment_end(text, p, end).ok_or(p)?;
        } else {
            break;
        }
    }
    Ok(p)
}

/// Whether `#<digits> =` starts at `p`.
pub(crate) fn definition(text: &[u8], mut p: usize, end: usize) -> bool {
    if text.get(p) != Some(&b'#') {
        return false;
    }
    p += 1;
    let start = p;
    while p < end && text[p].is_ascii_digit() {
        p += 1;
    }
    p > start && text.get(trivia(text, p, end)) == Some(&b'=')
}

/// Whether `ENDSEC ;` starts at `p`.
fn section_end(text: &[u8], p: usize, end: usize) -> bool {
    text.get(p..p + 6)
        .is_some_and(|word| word.eq_ignore_ascii_case(b"ENDSEC"))
        && text.get(trivia(text, p + 6, end)) == Some(&b';')
}

/// Where the statement at `start` ends, or the start of a comment that opens in it and doesn't
/// close.
fn frame_end(text: &[u8], start: usize, end: usize) -> Result<usize, usize> {
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
            if definition(text, next, end) || section_end(text, next, end) {
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
                let next = trivia(text, p + 1, end);
                if text[p + 1..next].contains(&b'\n') && definition(text, next, end) {
                    return Ok(p + 1);
                }
            }
        } else {
            match text[p] {
                b'\'' | b'"' => quote = text[p],
                b'/' if text.get(p + 1) == Some(&b'*') => {
                    p = closed_trivia(text, p, end)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_values_decode_the_same_eagerly_and_on_demand() {
        let text = b"ISO-10303-21;HEADER;FILE_DESCRIPTION(('x''y'),'2;1');ENDSEC;DATA;#7=X((#1,$,(1,.T.)),typed('abc'),/*x*/-3.2E-4,*);ENDSEC;END-ISO-10303-21;";
        let tape = Document::parse(text, true);
        let index = Document::parse(text, false);
        assert!(tape.errors.is_empty(), "{:?}", tape.errors);
        assert_eq!(tape.statements.len(), 1);
        assert_eq!(tape.decode(text, 0), index.decode(text, 0));
        assert_eq!(tape.params[0].next as usize, tape.params.len());
        assert!(tape.params.iter().any(|p| p.kind == Kind::Typed));
    }

    #[test]
    fn a_local_edit_orders_errors_like_a_full_parse() {
        let original = "#1321=IFCMATERIALLAYER(#1319=IFCAXIS2PLACEMENT3D('Concrete Masonry,10303-21;\nFILE_DESCRIPTION ((''), '2;\u{a0}1');\n";
        let text = original.replacen("(#", "(##", 1);
        let mut doc = Document::parse(original.as_bytes(), false);

        let outcome = doc.edit(
            b"",
            text.as_bytes(),
            Edit {
                start: 23,
                delete_len: 0,
                insert_len: 1,
            },
        );

        assert!(matches!(outcome, EditOutcome::Local { .. }), "{outcome:?}");
        assert_eq!(doc, Document::parse(text.as_bytes(), false));
    }

    #[test]
    fn edits_and_repairs_match_full_parse() {
        for original in [
            b"DATA;\n#1=X(1);\n#2=Y('hello',(#1,3));\n#3=Z(3);\nENDSEC;".as_slice(),
            b"DATA;\n#1=X(1);\n#2=Y(2,\n#3=Z(3);\n#4=W(4);\nENDSEC;",
            b"DATA;\n#1=X(1);\n#2=Y(2);/\n#3=Z(3);\n#4=W(4);\nENDSEC;",
            b"DATA;\n#1=X(1);/* a */\n#2=Y('b/*',*,'*/c');\n/*\n#3=Z(3);\n*/#4=W(4);\nENDSEC;",
        ] {
            edits_match_full_parse(original);
        }
    }

    fn edits_match_full_parse(original: &[u8]) {
        for retain in [true, false] {
            for start in 6..original.len() - 8 {
                for insert in [
                    b" ".as_slice(),
                    b"X",
                    b"'",
                    b";",
                    b"",
                    b"/*",
                    b"*/",
                    b"*",
                    b"/",
                ] {
                    let mut text = original.to_vec();
                    text.splice(start..start + 1, insert.iter().copied());
                    let mut doc = Document::parse(original, retain);
                    doc.edit(
                        &original[start..start + 1],
                        &text,
                        Edit {
                            start,
                            delete_len: 1,
                            insert_len: insert.len(),
                        },
                    );
                    assert_eq!(doc, Document::parse(&text, retain), "{start} {insert:?}");
                    doc.edit(
                        &text[start..start + insert.len()],
                        original,
                        Edit {
                            start,
                            delete_len: insert.len(),
                            insert_len: 1,
                        },
                    );
                    assert_eq!(doc, Document::parse(original, retain));
                }
            }
        }
    }

    #[test]
    fn only_comments_reaching_past_the_reparsed_statements_rebuild() {
        let original = b"DATA;\n#1=X(1);\n#2=Y('a',$);\n#3=Z(3);\n#4=W(4);\nENDSEC;";
        let string = original.iter().position(|b| *b == b'a').unwrap();
        let dollar = original.iter().position(|b| *b == b'$').unwrap();
        let after_two = string + 6;
        for (start, delete_len, insert, local) in [
            (string, 0, b"*".as_slice(), true),
            (string, 0, b"/", true),
            (string, 0, b"/*", true),
            (string, 0, b"*/", true),
            (dollar, 1, b"*", true),
            (after_two, 0, b"/* c */", true),
            (after_two, 0, b"/*", false),
            (dollar, 1, b"*/*", false),
        ] {
            let mut text = original.to_vec();
            text.splice(start..start + delete_len, insert.iter().copied());
            let mut doc = Document::parse(original, false);
            let outcome = doc.edit(
                &original[start..start + delete_len],
                &text,
                Edit {
                    start,
                    delete_len,
                    insert_len: insert.len(),
                },
            );
            assert_eq!(
                matches!(outcome, EditOutcome::Local { .. }),
                local,
                "{:?}",
                std::str::from_utf8(&text)
            );
            assert_eq!(doc, Document::parse(&text, false));
        }
    }

    #[test]
    fn id_lookups_after_an_edit_match_a_full_parse() {
        let original = b"DATA;\n#1=X(1);\n#2=Y(#1);\n#3=Z(#2);\n#4=W(4);\nENDSEC;".as_slice();
        for (from, to) in [
            (b"#2=Y".as_slice(), b"#9=Y".as_slice()),
            (b"#2=Y(#1);", b"#2=Y(#1);\n#5=V(5);"),
            (b"\n#2=Y(#1);", b""),
            (b"#2=Y(#1);\n#3", b"#3=Y(#1);\n#2"),
            (b"Y(#1)", b"Y(#3)"),
        ] {
            let start = original
                .windows(from.len())
                .position(|w| w == from)
                .unwrap();
            let mut text = original.to_vec();
            text.splice(start..start + from.len(), to.iter().copied());
            let mut doc = Document::parse(original, false);
            let outcome = doc.edit(
                from,
                &text,
                Edit {
                    start,
                    delete_len: from.len(),
                    insert_len: to.len(),
                },
            );
            assert!(matches!(outcome, EditOutcome::Local { .. }));
            let fresh = Document::parse(&text, false);
            for id in 0..12 {
                assert_eq!(
                    doc.find(id),
                    fresh.find(id),
                    "#{id} after {:?}",
                    String::from_utf8_lossy(to)
                );
            }
        }
    }

    #[test]
    fn an_edit_that_keeps_ids_costs_the_same_on_a_larger_document() {
        let edit_time = |statements: u32| {
            let mut text = b"DATA;\n".to_vec();
            for id in 1..=statements {
                text.extend_from_slice(format!("#{id}=X(1);\n").as_bytes());
            }
            text.extend_from_slice(b"ENDSEC;\n");
            let digit = text.len() - b"1);\nENDSEC;\n".len();
            let mut doc = Document::parse(&text, false);
            let fastest = (0..100)
                .map(|n| {
                    let (old, new) = if n % 2 == 0 {
                        (b"1", b"2")
                    } else {
                        (b"2", b"1")
                    };
                    text[digit] = new[0];
                    let start = std::time::Instant::now();
                    doc.edit(
                        old,
                        &text,
                        Edit {
                            start: digit,
                            delete_len: 1,
                            insert_len: 1,
                        },
                    );
                    start.elapsed()
                })
                .min()
                .unwrap();
            assert_eq!(doc.find(statements), Some(statements as usize - 1));
            fastest
        };
        let small = edit_time(10_000);
        let large = edit_time(160_000);
        assert!(
            large < small * 4,
            "16x the statements took {large:?} against {small:?}"
        );
    }

    #[test]
    fn a_statement_wrapped_over_many_lines_parses_in_linear_time() {
        let parse_time = |lines: usize| {
            let mut text = b"DATA;\n#1=IFCCARTESIANPOINTLIST3D((\n".to_vec();
            for _ in 1..lines {
                text.extend_from_slice(b"(0.,0.,0.),\n");
            }
            text.extend_from_slice(b"(0.,0.,0.)\n));\nENDSEC;\n");
            (0..3)
                .map(|_| {
                    let start = std::time::Instant::now();
                    let doc = Document::parse(&text, false);
                    let elapsed = start.elapsed();
                    assert!(doc.errors.is_empty(), "{:?}", doc.errors);
                    elapsed
                })
                .min()
                .unwrap()
        };
        let small = parse_time(10_000);
        let large = parse_time(40_000);
        assert!(
            large < small * 8,
            "4x the lines took {large:?} against {small:?}"
        );
    }

    #[test]
    fn statements_all_lacking_their_semicolon_parse_in_linear_time() {
        let growth = crate::test_support::growth_on_sixteenfold(
            1_000,
            |lines| {
                let mut text = b"DATA;\n".to_vec();
                for id in 1..=lines {
                    text.extend_from_slice(format!("#{id}=IFCX(1)\n").as_bytes());
                }
                text
            },
            |text| {
                let doc = Document::parse(text, false);
                assert_eq!(doc.errors.len(), doc.statements.len());
            },
        );
        assert!(
            growth < 4.0,
            "16x the statements took {growth:.1}x as long as 16 inputs"
        );
    }

    #[test]
    fn reports_an_unclosed_comment_at_its_opening() {
        let text = b"DATA;\n#1=X(1);\n/* never closed\n#2=Y(#3);\nENDSEC;\n";
        let doc = Document::parse(text, true);
        assert_eq!(
            doc.errors,
            [ParseError {
                span: Span { start: 15, end: 17 },
                message: "unclosed comment",
            }]
        );
        assert_eq!(doc.statements.len(), 1);
        edits_match_full_parse(text);
    }

    #[test]
    fn reports_an_unclosed_comment_inside_a_statement_at_its_opening() {
        for (text, ids) in [
            (
                b"DATA;\n#1=X(1);\n#2=Y(1, /* never closed\n#3=Z(#2);\nENDSEC;\n".as_slice(),
                [1, 2].as_slice(),
            ),
            (
                b"HEADER;\nFILE_NAME('a', /* never closed\nENDSEC;\nDATA;\n#1=X(1);\nENDSEC;\n",
                [].as_slice(),
            ),
        ] {
            let doc = Document::parse(text, true);
            let comment = memchr::memmem::find(text, b"/*").unwrap() as u32;
            assert_eq!(
                doc.errors,
                [ParseError {
                    span: Span {
                        start: comment,
                        end: comment + 2,
                    },
                    message: "unclosed comment",
                }]
            );
            assert!(!doc.unclosed_section);
            assert_eq!(doc.statements.iter().map(|s| s.id).collect::<Vec<_>>(), ids);
            edits_match_full_parse(text);
        }
    }

    #[test]
    fn flags_a_section_left_open_where_the_text_ends() {
        for (text, open) in [
            (b"HEADER;\nENDSEC;\nDATA;\n#1=X(1);\n".as_slice(), true),
            (b"HEADER;\nFILE_SCHEMA(('IFC4'));\n", true),
            (b"HEADER;\nENDSEC;\nDATA;\n", true),
            (b"HEADER;\nENDSEC;\nDATA;\n#1=X(1);\nENDSEC;\n", false),
            (b"HEADER;\nENDSEC;\n", false),
            (b"", false),
            (b"DATA;\n#1=X('a);\nENDSEC;\n", false),
            (b"DATA;\n#1=X(1\n  endsec ;\n", false),
            (
                b"HEADER;\nFILE_NAME('a);\nENDSEC;\nDATA;\n#1=X(1);\nENDSEC;\n",
                false,
            ),
            (b"DATA;\n#1=X(1);\n/* ENDSEC;\n", false),
        ] {
            assert_eq!(
                Document::parse(text, false).unclosed_section,
                open,
                "{:?}",
                std::str::from_utf8(text)
            );
        }
        edits_match_full_parse(b"DATA;\n#1=X(1);\n#2=Y('a',2);\n#3=Z(3);\n#4=W(4);\n");
        edits_match_full_parse(b"DATA;\n#1=X(1);\n#2=Y('a',2);\nENDSEC;\n#3=Z(3);\n#4=W(4);\n");
    }

    #[test]
    fn reports_a_section_opened_before_the_previous_one_ended() {
        let reopened = |text: &[u8]| {
            let doc = Document::parse(text, true);
            let keyword = memchr::memmem::rfind(text, b"DATA").unwrap() as u32;
            assert_eq!(
                doc.errors,
                [ParseError {
                    span: Span {
                        start: keyword,
                        end: keyword + 4,
                    },
                    message: SECTION_REOPENED,
                }]
            );
            assert!(!doc.unclosed_section);
            assert_eq!(doc.statements.len(), 1);
            edits_match_full_parse(text);
        };
        reopened(b"HEADER;\nFILE_SCHEMA(('IFC4'));\nDATA;\n#1=X(1);\nENDSEC;\n");
        reopened(b"DATA;\nDATA;\n#1=X(1);\nENDSEC;\n");
        for text in [
            b"HEADER;\nENDSEC;\nDATA;\n#1=X(1);\nENDSEC;\n".as_slice(),
            b"DATA;\n#1=X(1);\nENDSEC;\nDATA;\n#2=X(2);\nENDSEC;\n",
            b"HEADER;\nENDSEC;\nDATA;\nENDSEC;\nDATA;\n#2=X(2);\nENDSEC;\n",
        ] {
            let doc = Document::parse(text, true);
            assert!(doc.errors.is_empty(), "{:?}", doc.errors);
            assert!(!doc.unclosed_section);
            edits_match_full_parse(text);
        }
    }

    #[test]
    fn a_semicolon_and_a_definition_on_one_line_stay_inside_a_string() {
        let text = b"DATA;\n#1=X('b',$,IFCLABEL(';#9='),$);\n#2=X('a; #8 =');\nENDSEC;\n";
        let doc = Document::parse(text, true);
        assert!(doc.errors.is_empty(), "{:?}", doc.errors);
        assert_eq!(
            doc.statements.iter().map(|s| s.id).collect::<Vec<_>>(),
            [1, 2]
        );
        edits_match_full_parse(text);
    }

    #[test]
    fn an_edit_ending_a_recovered_header_string_reparses_the_header() {
        let original = b"HEADER;\nFILE_NAME('a\n#4=b');\n#5=Y(2);\n#6=Z(3);\n";
        let equals = original.iter().position(|b| *b == b'=').unwrap();
        let mut text = original.to_vec();
        text.remove(equals);
        let mut doc = Document::parse(original, false);
        assert!(!doc.errors.is_empty());
        doc.edit(
            b"=",
            &text,
            Edit {
                start: equals,
                delete_len: 1,
                insert_len: 0,
            },
        );
        assert!(doc.errors.is_empty(), "{:?}", doc.errors);
        assert_eq!(doc, Document::parse(&text, false));
        edits_match_full_parse(original);
    }

    #[test]
    fn skips_a_leading_byte_order_mark() {
        let text = b"\xEF\xBB\xBFDATA;\n#1=X(1);\n#2=Y('a',2);\n#3=Z(3);\nENDSEC;\n";
        let doc = Document::parse(text, true);
        assert!(doc.errors.is_empty(), "{:?}", doc.errors);
        assert_eq!(doc.statements.len(), 3);
        edits_match_full_parse(text);
    }

    #[test]
    fn recovers_two_strings_and_an_independent_error() {
        let text = b"DATA;\n#1=X('bad);\n#2=X(1);\n#3=X('bad);\n#4=X(2x);\n#5=X('ok');\nENDSEC;";
        let doc = Document::parse(text, true);
        assert_eq!(doc.errors.len(), 3);
        assert_eq!(
            doc.errors
                .iter()
                .filter(|e| e.message == "unclosed string")
                .count(),
            2
        );
        assert_eq!(doc.statements.len(), 5);
        assert!(!doc.decode(text, 4).is_empty());
    }

    #[test]
    fn corpus_parses_without_errors_and_both_modes_decode_alike() {
        fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for item in std::fs::read_dir(dir).unwrap() {
                let path = item.unwrap().path();
                if path.is_dir() {
                    files(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "ifc") {
                    out.push(path);
                }
            }
        }
        let mut paths = Vec::new();
        files(std::path::Path::new("samples"), &mut paths);
        if let Some(dir) = std::env::var_os("STEP_CORPUS_DIR") {
            for name in [
                "055.867908MB__uni-project-house-50mb.ifc",
                "013.646137MB__Ifc4_Revit_ARC.ifc",
            ] {
                paths.push(std::path::Path::new(&dir).join(name));
            }
        }
        assert!(!paths.is_empty());
        for path in paths {
            let text = std::fs::read_to_string(&path).unwrap();
            let tape = Document::parse(text.as_bytes(), true);
            let index = Document::parse(text.as_bytes(), false);
            assert!(
                tape.errors.is_empty(),
                "{}: {:?}",
                path.display(),
                tape.errors.iter().take(5).collect::<Vec<_>>()
            );
            assert!(!tape.statements.is_empty(), "{}", path.display());
            for statement in 0..tape.statements.len() {
                let decoded = tape.decode(text.as_bytes(), statement);
                assert!(!decoded.is_empty(), "{}", path.display());
                assert_eq!(decoded, index.decode(text.as_bytes(), statement));
            }
        }
    }
}
