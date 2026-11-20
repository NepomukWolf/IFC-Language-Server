//! Datatype diagnostics of a statement index, cached per statement and revalidated after
//! statement-local edits: the reparsed statements plus every statement referring to an id whose
//! resolved entity name may have changed.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tower_lsp::lsp_types::{Diagnostic, Position};

use crate::diagnostics::datatype;
use crate::schema::SchemaDoc;
use crate::step::{self, EditOutcome};

const LOG_CAPACITY: usize = 64;
const MAX_CHANGED_IDS: usize = 64;
/// Up to this many ids, one substring search per id plus one for leading zeros is cheaper than
/// visiting every `#`.
const SEARCH_PER_ID_UP_TO: usize = 8;

/// Identifies a statement index across statement-local edits. A rebuilt index gets a new generation.
#[derive(Clone, Debug)]
pub struct StatementEdits {
    generation: u64,
    revision: u64,
    log: VecDeque<LoggedEdit>,
}

#[derive(Clone, Debug)]
struct LoggedEdit {
    revision: u64,
    first: usize,
    old_len: usize,
    new_len: usize,
    changed_ids: Vec<u32>,
}

impl StatementEdits {
    pub fn fresh() -> Self {
        static GENERATION: AtomicU64 = AtomicU64::new(0);
        Self {
            generation: GENERATION.fetch_add(1, Ordering::Relaxed) + 1,
            revision: 0,
            log: VecDeque::new(),
        }
    }

    pub fn record(&mut self, outcome: EditOutcome) {
        let EditOutcome::Local {
            first,
            old_len,
            new_len,
            changed_ids,
            ..
        } = outcome
        else {
            *self = Self::fresh();
            return;
        };
        self.revision += 1;
        if self.log.len() == LOG_CAPACITY {
            self.log.pop_front();
        }
        self.log.push_back(LoggedEdit {
            revision: self.revision,
            first,
            old_len,
            new_len,
            changed_ids,
        });
    }

    fn since(&self, revision: u64) -> Option<impl Iterator<Item = &LoggedEdit>> {
        let oldest = self
            .log
            .front()
            .map_or(self.revision + 1, |edit| edit.revision);
        (revision + 1 >= oldest)
            .then(|| self.log.iter().filter(move |edit| edit.revision > revision))
    }
}

#[derive(Debug, Default)]
pub struct ValidationCache(Mutex<Option<Cached>>);

#[derive(Debug)]
struct Cached {
    generation: u64,
    revision: u64,
    schema_name: String,
    entries: Entries,
}

/// Statement index and its diagnostics relative to the statement start, sorted by index.
type Entries = Vec<(usize, Vec<Diagnostic>)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Full,
    Incremental { revalidated: usize },
    StaleSnapshot,
}

#[tracing::instrument(skip_all)]
pub fn collect(
    parsed: &step::Document,
    text: &str,
    edits: &StatementEdits,
    cache: &ValidationCache,
    schema: &SchemaDoc,
    schema_name: &str,
) -> (Vec<Diagnostic>, Mode) {
    let entity_types = crate::document::entity_type_index(parsed, text);
    let mut guard = cache.0.lock().expect("validation cache lock");
    if guard.as_ref().is_some_and(|cached| {
        (cached.generation, cached.revision) > (edits.generation, edits.revision)
    }) {
        let diagnostics =
            datatype::collect_all(parsed, text, &entity_types, schema, Some(schema_name));
        return (diagnostics, Mode::StaleSnapshot);
    }

    let replay = guard
        .take()
        .filter(|cached| cached.generation == edits.generation && cached.schema_name == schema_name)
        .and_then(|cached| edits.since(cached.revision).map(|log| (cached, log)))
        .and_then(|(cached, log)| dirty_statements(parsed, text, cached, log));
    let validate = |index: usize, out: &mut Vec<Diagnostic>| {
        if let Some(instance) =
            crate::document::decode_entity_instance_relative(parsed, text, index)
        {
            datatype::validate(&entity_types, schema, Some(schema_name), &instance, out);
        }
    };

    let mode = match replay {
        Some((cached, dirty)) => {
            let mut entries = Vec::with_capacity(cached.len());
            let mut cached = cached.into_iter().peekable();
            for &index in &dirty {
                while let Some(entry) = cached.next_if(|&(i, _)| i < index) {
                    entries.push(entry);
                }
                cached.next_if(|&(i, _)| i == index);
                let mut diagnostics = Vec::new();
                validate(index, &mut diagnostics);
                if !diagnostics.is_empty() {
                    entries.push((index, diagnostics));
                }
            }
            entries.extend(cached);
            *guard = Some(Cached {
                generation: edits.generation,
                revision: edits.revision,
                schema_name: schema_name.to_string(),
                entries,
            });
            Mode::Incremental {
                revalidated: dirty.len(),
            }
        }
        None => {
            let mut entries = Vec::new();
            let mut diagnostics = Vec::new();
            for index in 0..parsed.statements.len() {
                validate(index, &mut diagnostics);
                if !diagnostics.is_empty() {
                    entries.push((index, std::mem::take(&mut diagnostics)));
                }
            }
            *guard = Some(Cached {
                generation: edits.generation,
                revision: edits.revision,
                schema_name: schema_name.to_string(),
                entries,
            });
            Mode::Full
        }
    };

    let entries = &guard.as_ref().expect("cache was just filled").entries;
    (absolute(parsed, text, entries), mode)
}

/// Replays `log` over the cached entries and returns them with the statements to
/// revalidate, ascending, or `None` when more ids changed than are worth looking up.
fn dirty_statements<'a>(
    parsed: &step::Document,
    text: &str,
    cached: Cached,
    log: impl Iterator<Item = &'a LoggedEdit>,
) -> Option<(Entries, Vec<usize>)> {
    let mut entries = cached.entries;
    let mut dirty = Vec::new();
    let mut changed_ids = Vec::new();
    for edit in log {
        let old_end = edit.first + edit.old_len;
        let shift = |index: &mut usize| {
            if *index >= old_end {
                *index = *index - edit.old_len + edit.new_len;
            }
        };
        entries.retain(|&(index, _)| !(edit.first..old_end).contains(&index));
        entries.iter_mut().for_each(|(index, _)| shift(index));
        dirty.retain(|index| !(edit.first..old_end).contains(index));
        dirty.iter_mut().for_each(shift);
        dirty.extend(edit.first..edit.first + edit.new_len);
        changed_ids.extend_from_slice(&edit.changed_ids);
    }
    changed_ids.sort_unstable();
    changed_ids.dedup();
    if changed_ids.len() > MAX_CHANGED_IDS {
        return None;
    }
    dirty.extend(referrers(parsed, text, &changed_ids));
    dirty.sort_unstable();
    dirty.dedup();
    Some((entries, dirty))
}

/// Statements whose text contains `#` and digits, leading zeros included, naming one of `ids`
/// (sorted), a superset of those referring to it.
fn referrers(parsed: &step::Document, text: &str, ids: &[u32]) -> Vec<usize> {
    let mut statements = Vec::new();
    if ids.is_empty() {
        return statements;
    }
    let bytes = text.as_bytes();
    let mut visit = |at: usize| {
        let digits = &bytes[at + 1..];
        let len = digits
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        let id = digits[..len].iter().try_fold(0u32, |id, &digit| {
            id.checked_mul(10)?.checked_add(u32::from(digit - b'0'))
        });
        if len == 0 || !id.is_some_and(|id| ids.binary_search(&id).is_ok()) {
            return;
        }
        let index = parsed
            .statements
            .partition_point(|statement| (statement.span.end as usize) <= at);
        if parsed
            .statements
            .get(index)
            .is_some_and(|statement| statement.span.start as usize <= at)
        {
            statements.push(index);
        }
    };
    if ids.len() <= SEARCH_PER_ID_UP_TO {
        for needle in ids.iter().map(|id| format!("#{id}")).chain(["#0".into()]) {
            memchr::memmem::find_iter(bytes, needle.as_bytes()).for_each(&mut visit);
        }
    } else {
        memchr::memchr_iter(b'#', bytes).for_each(visit);
    }
    statements
}

fn absolute(
    parsed: &step::Document,
    text: &str,
    entries: &[(usize, Vec<Diagnostic>)],
) -> Vec<Diagnostic> {
    let starts = crate::document::positions_of(
        text,
        entries
            .iter()
            .map(|&(index, _)| parsed.statements[index].span.start as usize),
    );
    let place = |base: Position, relative: Position| {
        if relative.line == 0 {
            Position::new(base.line, base.character + relative.character)
        } else {
            Position::new(base.line + relative.line, relative.character)
        }
    };
    entries
        .iter()
        .zip(starts)
        .flat_map(|((_, diagnostics), base)| {
            diagnostics.iter().map(move |diagnostic| {
                let mut diagnostic = diagnostic.clone();
                diagnostic.range.start = place(base, diagnostic.range.start);
                diagnostic.range.end = place(base, diagnostic.range.end);
                diagnostic
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
