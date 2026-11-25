mod reader;
pub mod string_literal;

pub(crate) use reader::{
    Lookahead, MAX_NESTING, NESTING_TOO_DEEP, SECTION_REOPENED, definition, frame_end, section_end,
    trivia,
};
use reader::{Reader, closed_trivia};

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

const OFFSET_LIMIT: usize = u32::MAX as usize;
pub(crate) const DOCUMENT_TOO_LARGE: &str =
    "Documents of 4294967295 bytes or more are not supported";

impl Document {
    pub fn parse(text: &[u8], retain: bool) -> Self {
        Self::parse_with_limit(text, retain, OFFSET_LIMIT)
    }

    fn parse_with_limit(text: &[u8], retain: bool, offset_limit: usize) -> Self {
        let mut doc = Self {
            statements: Vec::new(),
            ids: Vec::new(),
            params: Vec::new(),
            errors: Vec::new(),
            header: Span::default(),
            unclosed_section: false,
            retain,
        };
        if text.len() >= offset_limit {
            doc.errors.push(ParseError {
                span: Span::default(),
                message: DOCUMENT_TOO_LARGE,
            });
            return doc;
        }
        let mut open_section = false;
        let mut p = if text.starts_with(b"\xEF\xBB\xBF") {
            3
        } else {
            0
        };
        let mut unclosed_comment = None;
        let mut ahead = Lookahead::default();
        while p < text.len() {
            p = match closed_trivia(text, p, text.len(), &mut ahead) {
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
            let frame = frame_end(text, p, text.len(), &mut ahead);
            let mut end = frame.unwrap_or_else(|comment| comment);
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
                if section {
                    let mut reader = Reader {
                        text,
                        p: word_end,
                        end,
                        params: &mut doc.params,
                        retain: false,
                        depth: 0,
                        recovered: false,
                        skipped: Vec::new(),
                        ahead: Lookahead::default(),
                    };
                    if let Err(error) = reader.expect(b';') {
                        if reader::definition(text, reader.p, end, &mut reader.ahead)
                            || reader::section_end(text, reader.p, end, &mut reader.ahead)
                        {
                            end = reader.p;
                        }
                        doc.errors.push(error);
                    }
                } else {
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
                        ahead: Lookahead::default(),
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
            ahead: Lookahead::default(),
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
            r.p = trivia(text, r.p, end, &mut r.ahead);
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
            if trivia(text, r.p, end, &mut r.ahead) != end {
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
        self.read_parameters(text, index, true).1
    }

    /// Whether decoding a statement produces a parameter list, without retaining its values.
    pub fn has_parameters(&self, text: &[u8], index: usize) -> bool {
        if self.retain {
            self.statements[index].param_len != 0
        } else {
            self.read_parameters(text, index, false).0
        }
    }

    fn read_parameters(&self, text: &[u8], index: usize, retain: bool) -> (bool, Vec<Param>) {
        let s = self.statements[index];
        let mut params = Vec::new();
        let mut r = Reader {
            text,
            p: s.name.end as usize,
            end: s.span.end as usize,
            params: &mut params,
            retain,
            depth: 0,
            recovered: false,
            skipped: Vec::new(),
            ahead: Lookahead::default(),
        };
        let valid = r.list(Kind::List).is_ok() || r.recovered;
        if !valid {
            params.clear();
        }
        (valid, params)
    }

    pub fn edit(&mut self, removed: &[u8], new: &[u8], edit: Edit) -> EditOutcome {
        self.edit_with_limit(removed, new, edit, OFFSET_LIMIT)
    }

    fn edit_with_limit(
        &mut self,
        removed: &[u8],
        new: &[u8],
        edit: Edit,
        offset_limit: usize,
    ) -> EditOutcome {
        if new.len() >= offset_limit {
            *self = Self::parse_with_limit(new, self.retain, offset_limit);
            return EditOutcome::Rebuilt;
        }
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
        let mut ahead = Lookahead::default();
        while p < end {
            let Ok(after_trivia) = closed_trivia(new, p, end, &mut ahead) else {
                *self = Self::parse(new, self.retain);
                return EditOutcome::Rebuilt;
            };
            p = after_trivia;
            if p == end {
                break;
            }
            let next = match frame_end(new, p, new.len(), &mut ahead) {
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

#[cfg(test)]
mod tests;
