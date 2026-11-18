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
    let parsed = snapshot
        .statement_index()
        .expect("document has a statement index");
    let mut diagnostics = syntax::collect(&snapshot);
    diagnostics.extend(datatype::collect_all(
        parsed,
        text,
        &crate::document::entity_type_index(parsed, text),
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
        let parsed = snapshot
            .statement_index()
            .expect("document has a statement index");
        let mut diagnostics = syntax::collect(&snapshot);
        let (datatype, mode) = super::collect(
            parsed,
            snapshot.text(),
            &snapshot.statement_edits,
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
            *parsed == step::Document::parse(snapshot.text().as_bytes(), false),
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
            let Some((start, end, replacement)) = random_edit(&pending, kind, &mut random) else {
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
                let generation = document.statement_edits.generation;
                document
                    .apply_content_changes(&[change])
                    .expect("change should be valid");
                let entry = rebuilds.entry(kind).or_default();
                entry.0 += 1;
                entry.1 += usize::from(document.statement_edits.generation != generation);
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
            let parsed = stale
                .statement_index()
                .expect("document has a statement index");
            let mut diagnostics = syntax::collect(&stale);
            diagnostics.extend(
                super::collect(
                    parsed,
                    stale.text(),
                    &stale.statement_edits,
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

fn reloaded_document(text: &str) -> Document {
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
        snapshot
            .statement_index()
            .expect("document has a statement index"),
        snapshot.text(),
        &snapshot.statement_edits,
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
    let mut document = reloaded_document(POINTS);
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
    let mut document = reloaded_document(POINTS);
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
fn replacing_the_text_before_an_invalid_range_diagnoses_the_new_text() {
    let schema = ifc4();
    let mut document = reloaded_document(
        "DATA;\n#1=IFCCARTESIANPOINT((0.,0.));\n#2=IFCCARTESIANPOINT((1.,0.));\n#3=IFCCARTESIANPOINT('x');\nENDSEC;\n",
    );
    assert_eq!(incremental(&document, &schema).0.len(), 1);
    let replacement = "DATA;\n#1=IFCCARTESIANPOINT('y');\nENDSEC;\n";

    let result = document.apply_content_changes(&[
        TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: replacement.to_string(),
        },
        TextDocumentContentChangeEvent {
            range: Some(Range::new(Position::new(99, 0), Position::new(99, 1))),
            range_length: None,
            text: String::new(),
        },
    ]);

    assert!(result.is_err());
    assert_eq!(document.text.as_str(), replacement);
    let (diagnostics, _) = incremental(&document, &schema);
    assert_eq!(diagnostics, full(replacement, &schema, "IFC4"));
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].range.start.line, 1);
}

#[test]
fn editing_an_unloaded_document_before_an_invalid_range_diagnoses_the_new_text() {
    let schema = ifc4();
    let mut document = reloaded_document(
        "DATA;\n#1=IFCCARTESIANPOINT((0.,0.));\n#2=IFCCARTESIANPOINT((1.,0.));\n#3=IFCCARTESIANPOINT('x');\nENDSEC;\n",
    );
    assert_eq!(incremental(&document, &schema).0.len(), 1);
    document.unload_parse_state();

    let result = document.apply_content_changes(&[
        TextDocumentContentChangeEvent {
            range: Some(Range::new(Position::new(2, 0), Position::new(4, 0))),
            range_length: None,
            text: String::new(),
        },
        TextDocumentContentChangeEvent {
            range: Some(Range::new(Position::new(99, 0), Position::new(99, 1))),
            range_length: None,
            text: String::new(),
        },
    ]);

    assert!(result.is_err());
    let (diagnostics, _) = incremental(&document, &schema);
    assert_eq!(diagnostics, full(&document.text, &schema, "IFC4"));
    assert_eq!(diagnostics, []);
}

#[test]
fn diagnostics_on_later_lines_of_a_statement_starting_mid_line_keep_their_columns() {
    let schema = ifc4();
    let mut document = reloaded_document(
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
    let mut document = reloaded_document(
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

#[test]
fn revalidated_statements_merge_into_the_cache_like_a_full_revalidation() {
    let schema = ifc4();
    let mut document = reloaded_document(
        "DATA;\n\
         #1=IFCDIRECTION((0.,0.,1.));\n\
         #2=IFCCARTESIANPOINT((0.,0.));\n\
         #3=IFCPOLYLINE((#1,#1));\n\
         #4=IFCCARTESIANPOINT('x');\n\
         #5=IFCPOLYLINE((#2,#2));\n\
         #6=IFCCARTESIANPOINT((0.,0.));\n\
         #7=IFCCARTESIANPOINT('z');\n\
         ENDSEC;\n",
    );
    let lines_with_diagnostics = |diagnostics: &[Diagnostic]| {
        let mut lines: Vec<u32> = diagnostics.iter().map(|d| d.range.start.line).collect();
        lines.dedup();
        lines
    };
    assert_eq!(
        lines_with_diagnostics(&incremental(&document, &schema).0),
        [3, 4, 7]
    );

    edit(&mut document, "#1=IFCDIRECTION", "#1=IFCCARTESIANPOINT");
    edit(&mut document, "#2=IFCCARTESIANPOINT", "#2=IFCDIRECTION");
    edit(&mut document, "('x')", "(('x'))");
    edit(
        &mut document,
        "#6=IFCCARTESIANPOINT((0.,0.))",
        "#6=IFCCARTESIANPOINT('y')",
    );
    let (diagnostics, mode) = incremental(&document, &schema);
    assert!(matches!(mode, Mode::Incremental { .. }), "{mode:?}");
    assert_eq!(diagnostics, full(&document.text, &schema, "IFC4"));
    assert_eq!(lines_with_diagnostics(&diagnostics), [4, 5, 6, 7]);

    edit(
        &mut document,
        "#7=IFCCARTESIANPOINT('z');",
        "#7=IFCCARTESIANPOINT('z');\n#8=IFCCARTESIANPOINT('w');",
    );
    let (diagnostics, mode) = incremental(&document, &schema);
    assert!(matches!(mode, Mode::Incremental { .. }), "{mode:?}");
    assert_eq!(diagnostics, full(&document.text, &schema, "IFC4"));
    assert_eq!(lines_with_diagnostics(&diagnostics), [4, 5, 6, 7, 8]);
}

#[test]
fn clearing_diagnostics_of_many_statements_grows_linearly_with_their_number() {
    let schema = ifc4();
    let growth = crate::test_support::growth_on_sixteenfold(
        4_000,
        |polylines| {
            let mut text = "DATA;\n#1=IFCDIRECTION((0.,0.,1.));\n".to_string();
            for id in 2..polylines + 2 {
                text += &format!("#{id}=IFCPOLYLINE((#1,#1));\n");
            }
            text += "ENDSEC;\n";
            let mut document = reloaded_document(&text);
            assert!(incremental(&document, &schema).0.len() >= polylines);
            edit(&mut document, "#1=IFCDIRECTION", "#1=IFCCARTESIANPOINT");
            document
        },
        |document| {
            let (diagnostics, mode) = incremental(document, &schema);
            assert!(matches!(mode, Mode::Incremental { .. }), "{mode:?}");
            assert_eq!(diagnostics, []);
        },
    );
    assert!(
        growth < 4.0,
        "16x the cleared statements in one document took {growth:.1}x as long as in 16 documents"
    );
}

#[test]
fn a_reference_with_leading_zeros_is_revalidated_when_its_target_changes_type() {
    let schema = ifc4();
    let mut document = reloaded_document(
        "DATA;\n#1=IFCCARTESIANPOINT((0.,0.));\n#2=IFCCARTESIANPOINT((1.,0.));\n#3=IFCPOLYLINE((#2,#01));\nENDSEC;\n",
    );
    assert_eq!(incremental(&document, &schema).0, []);

    edit(&mut document, "#1=IFCCARTESIANPOINT", "#1=IFCDIRECTION");
    let (diagnostics, mode) = incremental(&document, &schema);

    assert!(matches!(mode, Mode::Incremental { .. }), "{mode:?}");
    assert_eq!(diagnostics, full(&document.text, &schema, "IFC4"));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
}

#[test]
fn looking_up_referrers_of_many_ids_costs_about_as_much_as_of_a_few() {
    let text: String = (1..=200_000u32)
        .map(|id| format!("#{id}=IFCPOLYLINE((#{},#{}));\n", id + 1, id + 2))
        .collect();
    let parsed = step::Document::parse(text.as_bytes(), false);
    let ids = |count: u32| (0..count).map(|n| 1000 + n * 1500).collect::<Vec<_>>();
    let (few_ids, many_ids) = (ids(16), ids(128));
    let time = |ids: &[u32]| {
        let start = std::time::Instant::now();
        std::hint::black_box(referrers(&parsed, &text, ids));
        start.elapsed()
    };

    let (few, many) = (0..7)
        .map(|_| (time(&few_ids), time(&many_ids)))
        .reduce(|(few, many), (f, m)| (few.min(f), many.min(m)))
        .expect("seven rounds");

    assert!(many < few * 3, "16 ids {few:?}, 128 ids {many:?}");
}

#[test]
fn referrers_include_references_written_with_leading_zeros() {
    let text = "DATA;\n#1=IFCCARTESIANPOINT((0.,0.));\n#2=IFCPOLYLINE((#001));\n#3=IFCPOLYLINE((#10));\nENDSEC;\n";
    let parsed = step::Document::parse(text.as_bytes(), false);
    let many: Vec<u32> = std::iter::once(1).chain(1000..1000 + 64).collect();

    for ids in [&[1][..], &many] {
        let mut found = referrers(&parsed, text, ids);
        found.sort_unstable();
        found.dedup();
        assert_eq!(found, [0, 1], "{} ids", ids.len());
    }
}

#[test]
fn more_changed_ids_than_the_limit_revalidate_everything_as_a_full_run() {
    let schema = ifc4();
    let points: String = (1..=MAX_CHANGED_IDS + 1)
        .map(|id| format!("#{id}=IFCCARTESIANPOINT((0.,0.));\n"))
        .collect();
    let mut document = reloaded_document(&format!(
        "DATA;\n{points}#999=IFCPOLYLINE((#1,#2));\nENDSEC;\n"
    ));
    incremental(&document, &schema);

    edit(
        &mut document,
        &points,
        &points.replace("IFCCARTESIANPOINT((0.,0.))", "IFCDIRECTION((0.,1.))"),
    );
    let (diagnostics, mode) = incremental(&document, &schema);

    assert_eq!(mode, Mode::Full);
    assert_eq!(diagnostics, full(&document.text, &schema, "IFC4"));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
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
