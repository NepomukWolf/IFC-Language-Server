//! Datatype diagnostics of a lazy statement index, cached per statement and revalidated after
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

/// Identifies a lazy index across statement-local edits. A rebuilt index gets a new generation.
#[derive(Clone, Debug)]
pub struct LazyEdits {
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

impl LazyEdits {
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
    /// Statement index and its diagnostics relative to the statement start, sorted by index.
    entries: Vec<(usize, Vec<Diagnostic>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Full,
    Incremental { revalidated: usize },
    StaleSnapshot,
}

#[tracing::instrument(skip_all)]
pub fn collect(
    lazy: &step::Document,
    text: &str,
    edits: &LazyEdits,
    cache: &ValidationCache,
    schema: &SchemaDoc,
    schema_name: &str,
) -> (Vec<Diagnostic>, Mode) {
    let entity_types = crate::document::lazy_entity_type_index(lazy, text);
    let mut guard = cache.0.lock().expect("validation cache lock");
    if guard.as_ref().is_some_and(|cached| {
        (cached.generation, cached.revision) > (edits.generation, edits.revision)
    }) {
        let diagnostics =
            datatype::collect_lazy(lazy, text, &entity_types, schema, Some(schema_name));
        return (diagnostics, Mode::StaleSnapshot);
    }

    let replay = guard
        .take()
        .filter(|cached| cached.generation == edits.generation && cached.schema_name == schema_name)
        .and_then(|cached| edits.since(cached.revision).map(|log| (cached, log)))
        .map(|(cached, log)| dirty_statements(lazy, text, cached, log));
    let validate = |index: usize, out: &mut Vec<Diagnostic>| {
        if let Some(instance) = crate::document::lazy_entity_instance_relative(lazy, text, index) {
            datatype::validate(&entity_types, schema, Some(schema_name), &instance, out);
        }
    };

    let mode = match replay {
        Some((mut entries, dirty)) => {
            for &index in &dirty {
                let mut diagnostics = Vec::new();
                validate(index, &mut diagnostics);
                match entries.binary_search_by_key(&index, |&(i, _)| i) {
                    Ok(at) if diagnostics.is_empty() => {
                        entries.remove(at);
                    }
                    Ok(at) => entries[at].1 = diagnostics,
                    Err(_) if diagnostics.is_empty() => {}
                    Err(at) => entries.insert(at, (index, diagnostics)),
                }
            }
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
            for index in 0..lazy.statements.len() {
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
    (absolute(lazy, text, entries), mode)
}

/// Replays `log` over the cached entries and returns them with the statements to
/// revalidate, ascending.
fn dirty_statements<'a>(
    lazy: &step::Document,
    text: &str,
    cached: Cached,
    log: impl Iterator<Item = &'a LoggedEdit>,
) -> (Vec<(usize, Vec<Diagnostic>)>, Vec<usize>) {
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
        dirty = (0..lazy.statements.len()).collect();
    } else {
        dirty.extend(referrers(lazy, text, &changed_ids));
    }
    dirty.sort_unstable();
    dirty.dedup();
    (entries, dirty)
}

/// Statements whose text contains `#id` for one of `ids`, a superset of those referring to it.
fn referrers(lazy: &step::Document, text: &str, ids: &[u32]) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut statements = Vec::new();
    for id in ids {
        let needle = format!("#{id}");
        for at in memchr::memmem::find_iter(bytes, needle.as_bytes()) {
            let end = at + needle.len();
            if bytes.get(end).is_some_and(u8::is_ascii_digit) {
                continue;
            }
            let index = lazy
                .statements
                .partition_point(|statement| (statement.span.end as usize) <= at);
            if lazy
                .statements
                .get(index)
                .is_some_and(|statement| statement.span.start as usize <= at)
            {
                statements.push(index);
            }
        }
    }
    statements
}

fn absolute(
    lazy: &step::Document,
    text: &str,
    entries: &[(usize, Vec<Diagnostic>)],
) -> Vec<Diagnostic> {
    let starts = crate::document::positions_of(
        text,
        entries
            .iter()
            .map(|&(index, _)| lazy.statements[index].span.start as usize),
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
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use tower_lsp::lsp_types::{Range, TextDocumentContentChangeEvent};

    use super::*;
    use crate::diagnostics::{DiagnosticSnapshot, syntax};
    use crate::document::Document;
    use crate::schema::{SchemaDocCollection, normalize_name};

    struct Random(u64);

    impl Random {
        fn below(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % bound as u64) as usize
        }
    }

    const EDITS: [&str; 12] = [
        "valid",
        "comment",
        "omitted",
        "syntax",
        "quote",
        "id",
        "type",
        "reference",
        "insert",
        "delete",
        "join",
        "split",
    ];

    fn position(text: &str, offset: usize) -> Position {
        let line_start = text[..offset].rfind('\n').map_or(0, |n| n + 1);
        Position::new(
            memchr::memchr_iter(b'\n', &text.as_bytes()[..offset]).count() as u32,
            text[line_start..offset].encode_utf16().count() as u32,
        )
    }

    fn full(text: &str, schema: &SchemaDoc, schema_name: &str) -> Vec<Diagnostic> {
        let mut document = Document::new_unloaded(text.to_string());
        document.reload_parse_state();
        let snapshot = DiagnosticSnapshot::from_document(&document);
        let lazy = snapshot.lazy().expect("forced lazy path");
        let mut diagnostics = syntax::collect(&snapshot);
        diagnostics.extend(datatype::collect_lazy(
            lazy,
            text,
            &crate::document::lazy_entity_type_index(lazy, text),
            schema,
            Some(schema_name),
        ));
        diagnostics
    }

    /// Statement lines as (start, end) byte ranges, end excluding the newline.
    fn statement_lines(text: &str) -> Vec<(usize, usize)> {
        let mut lines = Vec::new();
        let mut start = 0;
        for line in text.split_inclusive('\n') {
            let body = line.trim_end_matches(['\n', '\r']);
            if body.starts_with('#') {
                lines.push((start, start + body.len()));
            }
            start += line.len();
        }
        lines
    }

    /// One random edit of `kind` as (start, end, replacement) in bytes, or `None` when the picked
    /// statement offers nothing to edit.
    fn random_edit(text: &str, kind: &str, random: &mut Random) -> Option<(usize, usize, String)> {
        let lines = statement_lines(text);
        if lines.len() < 2 {
            return None;
        }
        let (start, end) = lines[random.below(lines.len())];
        let line = &text[start..end];
        let other = {
            let (s, e) = lines[random.below(lines.len())];
            &text[s..e]
        };
        let id_end = start + 1 + line[1..].bytes().take_while(u8::is_ascii_digit).count();
        let name_start = line.find('=')? + 1;
        let open = line.find('(')?;
        let pick = |random: &mut Random, byte: u8| {
            let hits: Vec<_> = line
                .bytes()
                .enumerate()
                .filter(|&(_, b)| b == byte)
                .collect();
            (!hits.is_empty()).then(|| start + hits[random.below(hits.len())].0)
        };
        let other_id = &other[1..1 + other[1..].bytes().take_while(u8::is_ascii_digit).count()];
        Some(match kind {
            "valid" => (start + open + 1, start + open + 1, " ".into()),
            "syntax" => {
                let at = pick(random, b'(')?;
                (at, at + 1, ["X", ")", "", "(("][random.below(4)].into())
            }
            "quote" => {
                let at = pick(random, b'\'')?;
                (at, at + 1, String::new())
            }
            "id" => {
                let id = if random.below(2) == 0 {
                    other_id.to_string()
                } else {
                    (900_000_000 + random.below(1000)).to_string()
                };
                (start + 1, id_end, id)
            }
            "type" => {
                let other_name = &other[other.find('=')? + 1..other.find('(')?];
                (start + name_start, start + open, other_name.into())
            }
            "reference" => {
                let at = pick(random, b'#').filter(|&at| at > start)?;
                let digits = text[at + 1..]
                    .bytes()
                    .take_while(u8::is_ascii_digit)
                    .count();
                let id = if random.below(3) == 0 {
                    "999999999".to_string()
                } else {
                    other_id.to_string()
                };
                (at + 1, at + 1 + digits, id)
            }
            "insert" => {
                let id = if random.below(2) == 0 {
                    other_id.to_string()
                } else {
                    (900_000_000 + random.below(1000)).to_string()
                };
                let body = &other[other.find('=')?..];
                (start, start, format!("#{id}{body}\n"))
            }
            "delete" => (start, (end + 1).min(text.len()), String::new()),
            "join" => (end, (end + 1).min(text.len()), String::new()),
            "comment" => {
                let at = match random.below(3) {
                    0 => pick(random, b'\'')? + 1,
                    1 => pick(random, b',').or_else(|| pick(random, b'('))? + 1,
                    _ => end,
                };
                (at, at, ["*", "/", "/*", "*/"][random.below(4)].into())
            }
            "omitted" => {
                let at = pick(random, b'$')?;
                (at, at + 1, "*".into())
            }
            "split" => {
                let at = pick(random, b',')?;
                (at + 1, at + 1, "\n  ".into())
            }
            _ => unreachable!("unknown edit kind"),
        })
    }

    /// Applies `steps` random edits, sometimes several before collecting and sometimes as one
    /// multi-change event, and compares incremental diagnostics with a full revalidation.
    /// Returns the incremental and other collections, and per edit kind applied alone how often
    /// it was applied and how often it rebuilt the statement index.
    fn check_random_edits(
        path: &Path,
        steps: usize,
        seed: u64,
    ) -> (usize, usize, BTreeMap<&'static str, (usize, usize)>) {
        let text = std::fs::read_to_string(path).expect("sample should be readable");
        let mut document = Document::new_unloaded(text);
        let schema_name = normalize_name(
            document
                .schema_name
                .as_deref()
                .expect("sample declares a schema"),
        );
        let schema = SchemaDocCollection::new()
            .get_shared(&schema_name)
            .expect("sample schema is bundled");
        document.reload_parse_state();
        let mut random = Random(seed);
        let (mut incremental, mut fallbacks) = (0, 0);
        let mut rebuilds = BTreeMap::<&'static str, (usize, usize)>::new();
        let mut collect = |document: &Document, step: &str| {
            let snapshot = DiagnosticSnapshot::from_document(document);
            let lazy = snapshot.lazy().expect("forced lazy path");
            let mut diagnostics = syntax::collect(&snapshot);
            let (datatype, mode) = super::collect(
                lazy,
                snapshot.text(),
                &snapshot.lazy_edits,
                &snapshot.validation_cache,
                &schema,
                &schema_name,
            );
            match mode {
                Mode::Incremental { .. } => incremental += 1,
                _ => fallbacks += 1,
            }
            diagnostics.extend(datatype);
            document.assert_text_index_matches_scan();
            assert!(
                *lazy == step::Document::parse(snapshot.text().as_bytes(), false),
                "{} seed {seed} {step}: statement index differs from a fresh parse",
                path.display()
            );
            assert_eq!(
                diagnostics,
                full(&document.text, &schema, &schema_name),
                "{} seed {seed} {step}",
                path.display()
            );
        };
        collect(&document, "open");
        for step in 0..steps {
            let batch = 1 + random.below(3);
            let mut kinds = Vec::new();
            let mut changes = Vec::new();
            let mut stale = None;
            let mut pending = document.text.to_string();
            for _ in 0..batch {
                let kind = EDITS[random.below(EDITS.len())];
                let Some((start, end, replacement)) = random_edit(&pending, kind, &mut random)
                else {
                    continue;
                };
                kinds.push(kind);
                let change = TextDocumentContentChangeEvent {
                    range: Some(Range::new(
                        position(&pending, start),
                        position(&pending, end),
                    )),
                    range_length: None,
                    text: replacement.clone(),
                };
                pending.replace_range(start..end, &replacement);
                if random.below(4) == 0 {
                    changes.push(change);
                    continue;
                }
                if !changes.is_empty() {
                    changes.push(change);
                    document
                        .apply_content_changes(&std::mem::take(&mut changes))
                        .expect("change should be valid");
                } else {
                    let generation = document.lazy_edits.generation;
                    document
                        .apply_content_changes(&[change])
                        .expect("change should be valid");
                    let entry = rebuilds.entry(kind).or_default();
                    entry.0 += 1;
                    entry.1 += usize::from(document.lazy_edits.generation != generation);
                }
                if random.below(5) == 0 {
                    stale = Some(DiagnosticSnapshot::from_document(&document));
                }
            }
            if !changes.is_empty() {
                document
                    .apply_content_changes(&changes)
                    .expect("change should be valid");
            }
            collect(&document, &format!("step {step} {kinds:?}"));
            if let Some(stale) = stale {
                let lazy = stale.lazy().expect("forced lazy path");
                let mut diagnostics = syntax::collect(&stale);
                diagnostics.extend(
                    super::collect(
                        lazy,
                        stale.text(),
                        &stale.lazy_edits,
                        &stale.validation_cache,
                        &schema,
                        &schema_name,
                    )
                    .0,
                );
                assert_eq!(diagnostics, full(stale.text(), &schema, &schema_name));
            }
        }
        (incremental, fallbacks, rebuilds)
    }

    fn lazy_document(text: &str) -> Document {
        let mut document = Document::new_unloaded(text.to_string());
        document.reload_parse_state();
        document
    }

    fn edit(document: &mut Document, from: &str, to: &str) {
        let start = document.text.find(from).expect("edited text is present");
        let change = TextDocumentContentChangeEvent {
            range: Some(Range::new(
                position(&document.text, start),
                position(&document.text, start + from.len()),
            )),
            range_length: None,
            text: to.to_string(),
        };
        document
            .apply_content_changes(&[change])
            .expect("change should be valid");
    }

    fn incremental(document: &Document, schema: &SchemaDoc) -> (Vec<Diagnostic>, Mode) {
        let snapshot = DiagnosticSnapshot::from_document(document);
        super::collect(
            snapshot.lazy().expect("forced lazy path"),
            snapshot.text(),
            &snapshot.lazy_edits,
            &snapshot.validation_cache,
            schema,
            "IFC4",
        )
    }

    fn ifc4() -> Arc<SchemaDoc> {
        SchemaDocCollection::new()
            .get_shared("IFC4")
            .expect("IFC4 is bundled")
    }

    const POINTS: &str = "DATA;\n#1=IFCCARTESIANPOINT((0.,0.));\n#2=IFCCARTESIANPOINT((1.,0.));\n#3=IFCCARTESIANPOINT((2.,0.));\nENDSEC;\n";

    #[test]
    fn statements_edited_before_a_later_insertion_stay_revalidated() {
        let schema = ifc4();
        let mut document = lazy_document(POINTS);
        incremental(&document, &schema);

        edit(
            &mut document,
            "#3=IFCCARTESIANPOINT",
            "#3=IFCCARTESIANPOINTX",
        );
        edit(&mut document, "#1=", "#9=IFCCARTESIANPOINT((5.,0.));\n#1=");
        let (diagnostics, mode) = incremental(&document, &schema);

        assert!(matches!(mode, Mode::Incremental { .. }), "{mode:?}");
        assert_eq!(
            diagnostics,
            full(&document.text, &schema, "IFC4"),
            "{diagnostics:?}"
        );
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn more_edits_than_the_log_keeps_revalidate_everything() {
        let schema = ifc4();
        let mut document = lazy_document(POINTS);
        incremental(&document, &schema);

        edit(
            &mut document,
            "#2=IFCCARTESIANPOINT",
            "#2=IFCCARTESIANPOINTX",
        );
        for _ in 0..LOG_CAPACITY {
            edit(&mut document, "#3=", "#3= ");
        }
        let (diagnostics, mode) = incremental(&document, &schema);

        assert_eq!(mode, Mode::Full);
        assert_eq!(diagnostics, full(&document.text, &schema, "IFC4"));
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn diagnostics_on_later_lines_of_a_statement_starting_mid_line_keep_their_columns() {
        let schema = ifc4();
        let mut document = lazy_document(
            "DATA;\n#1=IFCCARTESIANPOINT((0.,0.)); #2=IFCCARTESIANPOINT((1.,0.),\n  'x');\nENDSEC;\n",
        );
        assert_eq!(
            incremental(&document, &schema).0,
            full(&document.text, &schema, "IFC4")
        );

        edit(&mut document, "#1=", "#1= ");
        let (diagnostics, mode) = incremental(&document, &schema);

        assert!(matches!(mode, Mode::Incremental { .. }), "{mode:?}");
        assert_eq!(diagnostics, full(&document.text, &schema, "IFC4"));
        assert_eq!(diagnostics[0].range.end, Position::new(2, 6));
    }

    #[test]
    fn diagnostics_after_non_ascii_text_on_a_shared_line_count_utf16_units() {
        let schema = ifc4();
        let mut document = lazy_document(
            "DATA;\n#1=IFCCARTESIANPOINT((0.,0.)); /*é😀*/ #2=IFCCARTESIANPOINT('é😀');\nENDSEC;\n",
        );
        incremental(&document, &schema);
        edit(&mut document, "#1=", "#1= ");
        let (diagnostics, mode) = incremental(&document, &schema);

        assert!(matches!(mode, Mode::Incremental { .. }), "{mode:?}");
        assert_eq!(diagnostics, full(&document.text, &schema, "IFC4"));
        let start = document.text.find("'é😀'").unwrap();
        let end = start + "'é😀'".len();
        assert_eq!(
            diagnostics[0].range,
            Range::new(
                position(&document.text, start),
                position(&document.text, end)
            )
        );
    }

    fn samples() -> Vec<PathBuf> {
        let mut paths: Vec<_> = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/samples"))
            .expect("samples directory")
            .map(|entry| entry.expect("sample entry").path())
            .filter(|path| {
                path.extension().is_some_and(|extension| extension == "ifc")
                    && !path.ends_with("Unsupported-Schema.ifc")
            })
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn random_edits_match_full_revalidation_on_samples() {
        for path in samples() {
            for seed in 1..=2 {
                let (incremental, _, _) = check_random_edits(&path, 25, seed);
                assert!(
                    incremental > 0,
                    "{} never took the incremental path",
                    path.display()
                );
            }
        }
    }

    /// `IFC_INCREMENTAL_FILES` lists extra files separated by `:`; `IFC_INCREMENTAL_STEPS` and
    /// `IFC_INCREMENTAL_SEEDS` scale the run.
    #[test]
    #[ignore = "reads files outside the repository"]
    fn random_edits_match_full_revalidation_on_listed_files() {
        let env = |name: &str, default: usize| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        };
        let files = std::env::var("IFC_INCREMENTAL_FILES").expect("IFC_INCREMENTAL_FILES");
        for path in files.split(':') {
            for seed in 1..=env("IFC_INCREMENTAL_SEEDS", 2) as u64 {
                let (incremental, fallbacks, rebuilds) =
                    check_random_edits(Path::new(path), env("IFC_INCREMENTAL_STEPS", 50), seed);
                eprintln!(
                    "{path} seed {seed}: incremental {incremental} other {fallbacks} \
                     single edits (applied, rebuilt) {rebuilds:?}"
                );
            }
        }
    }
}
