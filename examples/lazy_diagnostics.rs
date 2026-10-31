//! Compares diagnostics and timings of the AST path and the lazy statement-index path.
//!
//! `generate <mb> <out>` writes the synthetic property model.
//! `equiv <file>...` prints diagnostics that differ between the two paths.
//! `mutate <file>` does the same for single-statement mutations at sampled statements.
//! `measure <ast|lazy> <file> <valid|syntax|quote|id|star|slash>` prints one JSON record, timing
//! the edit and then definition, references and completion at a reference after it; with
//! `LAZY_DIAG_SPANS` set it also logs the server's span timings to stderr.

#[allow(dead_code)]
mod lsp_session;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;
use tower_lsp::LanguageServer;
use tower_lsp::lsp_types::{
    CompletionParams, CompletionResponse, Diagnostic, GotoDefinitionParams, GotoDefinitionResponse,
    Range, ReferenceContext, ReferenceParams, TextDocumentIdentifier, TextDocumentPositionParams,
    Url,
};

use lsp_session::{Model, Session};

const AST_LIMIT_MB: f64 = 4096.0;
const LAZY_LIMIT_MB: f64 = 0.000_001;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("generate") => {
            // SAFETY: single-threaded before any other code reads the environment.
            unsafe { std::env::set_var("IFC_BENCH_SYNTHETIC_MB", &args[1]) };
            std::fs::write(&args[2], Model::synthetic_properties().text).unwrap();
        }
        Some("equiv") => {
            for path in &args[1..] {
                equivalence(Path::new(path));
            }
        }
        Some("mutate") => mutations(Path::new(&args[1])),
        Some("measure") => measure(&args[1], Path::new(&args[2]), &args[3]),
        _ => panic!("usage: generate <mb> <out> | equiv <file>... | measure <mode> <file> <edit>"),
    }
}

fn limit(mode: &str) -> f64 {
    match mode {
        "ast" => AST_LIMIT_MB,
        "lazy" => LAZY_LIMIT_MB,
        _ => panic!("mode is ast or lazy"),
    }
}

type Key = (u32, u32, u32, u32, String);

fn keys(diagnostics: &[Diagnostic]) -> BTreeMap<Key, usize> {
    let mut keys = BTreeMap::new();
    for diagnostic in diagnostics {
        let Range { start, end } = diagnostic.range;
        *keys
            .entry((
                start.line,
                start.character,
                end.line,
                end.character,
                diagnostic.message.clone(),
            ))
            .or_default() += 1;
    }
    keys
}

fn equivalence(path: &Path) {
    compare(&Model::load(path));
}

fn compare(model: &Model) -> bool {
    let mut results = Vec::new();
    for mode in ["ast", "lazy"] {
        let mut session = Session::new(json!({ "astFileSizeLimitMb": limit(mode) }));
        results.push(keys(&session.open(&model.uri(), model.text.clone())));
    }
    let only = |a: &BTreeMap<Key, usize>, b: &BTreeMap<Key, usize>| -> Vec<(Key, usize)> {
        a.iter()
            .filter_map(|(key, &count)| {
                let other = b.get(key).copied().unwrap_or(0);
                (count > other).then(|| (key.clone(), count - other))
            })
            .collect()
    };
    let ast_only = only(&results[0], &results[1]);
    let lazy_only = only(&results[1], &results[0]);
    println!(
        "{}: ast {} lazy {} ast-only {} lazy-only {}",
        model.name,
        results[0].values().sum::<usize>(),
        results[1].values().sum::<usize>(),
        ast_only.iter().map(|(_, n)| n).sum::<usize>(),
        lazy_only.iter().map(|(_, n)| n).sum::<usize>(),
    );
    for (label, list) in [("ast-only", &ast_only), ("lazy-only", &lazy_only)] {
        for ((l1, c1, l2, c2, message), count) in list.iter().take(20) {
            println!("  {label} x{count} {l1}:{c1}-{l2}:{c2} {message}");
        }
    }
    ast_only.is_empty() && lazy_only.is_empty()
}

const MUTATIONS: [&str; 11] = [
    "paren-to-x",
    "delete-closing-quote",
    "delete-semicolon",
    "extra-close-paren",
    "unknown-entity",
    "dangling-reference",
    "drop-last-parameter",
    "string-to-number",
    "insert-comment",
    "lowercase-entity",
    "split-statement-lines",
];

/// Applies `mutation` to the statement line starting at `start`, or returns `None` when the
/// statement has nothing to mutate.
fn mutate(text: &str, start: usize, mutation: &str) -> Option<String> {
    let end = text[start..].find('\n').map_or(text.len(), |n| start + n);
    let line = &text[start..end];
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    let name = line.find('=')? + 1;
    let quote = line.find('\'');
    let closing_quote = quote.and_then(|quote| {
        let bytes = line.as_bytes();
        let mut p = quote + 1;
        while p < bytes.len() {
            if bytes[p] == b'\'' {
                if bytes.get(p + 1) == Some(&b'\'') {
                    p += 2;
                    continue;
                }
                return Some(p);
            }
            p += 1;
        }
        None
    });
    let replaced = |from: usize, to: usize, with: &str| {
        let mut line = line.to_string();
        line.replace_range(from..to, with);
        Some(line)
    };
    let new_line = match mutation {
        "paren-to-x" => replaced(open, open + 1, "X"),
        "delete-closing-quote" => replaced(closing_quote?, closing_quote? + 1, ""),
        "delete-semicolon" => replaced(close + 1, line.len(), ""),
        "extra-close-paren" => replaced(close + 1, close + 1, ")"),
        "unknown-entity" => replaced(name, open, "IFCNOTANENTITY"),
        "dangling-reference" => {
            let reference = line[open..].find('#')? + open;
            let digits = line[reference + 1..]
                .find(|c: char| !c.is_ascii_digit())
                .map_or(line.len(), |n| reference + 1 + n);
            replaced(reference + 1, digits, "999999999")
        }
        "drop-last-parameter" => {
            let comma = line[..close].rfind(',')?;
            (comma > open && !line[comma..close].contains(['(', ')', '\'']))
                .then(|| replaced(comma, close, ""))?
        }
        "string-to-number" => replaced(quote?, closing_quote? + 1, "42"),
        "insert-comment" => replaced(open + 1, open + 1, "/* c */"),
        "lowercase-entity" => replaced(name, open, &line[name..open].to_ascii_lowercase()),
        "split-statement-lines" => {
            let comma = line[open..].find(',')? + open;
            replaced(comma + 1, comma + 1, "\n  ")
        }
        _ => unreachable!(),
    }?;
    let mut mutated = String::with_capacity(text.len() + 16);
    mutated.push_str(&text[..start]);
    mutated.push_str(&new_line);
    mutated.push_str(&text[end..]);
    Some(mutated)
}

fn mutations(path: &Path) {
    let model = Model::load(path);
    let starts: Vec<usize> = model
        .text
        .match_indices("\n#")
        .map(|(index, _)| index + 1)
        .collect();
    let samples = 5;
    let mut summary = BTreeMap::<&str, (usize, usize)>::new();
    for mutation in MUTATIONS {
        for sample in 0..samples {
            let from = starts.len() * (2 * sample + 1) / (2 * samples);
            let Some((start, text)) = starts[from..]
                .iter()
                .find_map(|&start| Some((start, mutate(&model.text, start, mutation)?)))
            else {
                continue;
            };
            println!(
                "{mutation} at line {}:",
                model.text[..start].matches('\n').count()
            );
            let equal = compare(&Model {
                name: format!("{}-{mutation}-{sample}", model.name),
                text,
            });
            let entry = summary.entry(mutation).or_default();
            entry.0 += 1;
            entry.1 += usize::from(equal);
        }
    }
    for (mutation, (runs, equal)) in summary {
        println!("summary {mutation}: {equal}/{runs} identical");
    }
}

/// Byte offsets of the first `(` and the first string's closing quote of the quote-bearing
/// statement nearest the middle of the file.
fn edit_anchor(text: &str) -> (usize, usize, usize) {
    let middle = text.len() / 2;
    let candidates = text[middle..]
        .match_indices("\n#")
        .map(|(index, _)| middle + index + 1)
        .chain(text.match_indices("\n#").map(|(index, _)| index + 1));
    for start in candidates {
        let end = text[start..].find('\n').map_or(text.len(), |n| start + n);
        let line = &text[start..end];
        let (Some(open), Some(quote)) = (line.find('('), line.find('\'')) else {
            continue;
        };
        let bytes = line.as_bytes();
        let mut p = quote + 1;
        while p < bytes.len() {
            if bytes[p] == b'\'' {
                if bytes.get(p + 1) == Some(&b'\'') {
                    p += 2;
                    continue;
                }
                return (start, start + open, start + p);
            }
            p += 1;
        }
    }
    panic!("no statement with a string")
}

fn measure(mode: &str, path: &Path, edit: &str) {
    if std::env::var_os("LAZY_DIAG_SPANS").is_some() {
        tracing_subscriber::fmt()
            .with_env_filter("ifc_language_server=info")
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .init();
    }
    let model = Model::load(&PathBuf::from(path));
    let uri = model.uri();
    let (statement, open, close_quote) = edit_anchor(&model.text);
    let id_len = model.text.as_bytes()[statement + 1..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let (start, deleted, replacement) = match edit {
        "valid" => (open + 1, 0, " "),
        "syntax" => (open, 1, "X"),
        "quote" => (close_quote, 1, ""),
        "id" => (statement + 1, id_len, "999999999"),
        "star" => (close_quote, 0, "*"),
        "slash" => (close_quote, 0, "/"),
        _ => panic!("edit is valid, syntax, quote, id, star or slash"),
    };
    let mut edited_text = model.text.clone();
    edited_text.replace_range(start..start + deleted, replacement);
    let target = if edit == "id" {
        model.text[statement + 1..statement + 1 + id_len].to_string()
    } else {
        let line_end = model.text[open..]
            .find('\n')
            .map_or(model.text.len(), |n| open + n);
        let reference = model.text[open..line_end].find('#').map(|n| open + n + 1);
        reference.map_or_else(
            || "1".to_string(),
            |at| {
                model.text[at..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect()
            },
        )
    };
    let range = Range::new(model.position(start), model.position(start + deleted));
    let mut session = Session::new(json!({ "astFileSizeLimitMb": limit(mode) }));
    let text = model.text.clone();
    let footprint_before = footprint();

    let clock = Instant::now();
    let opened = session.open(&uri, text);
    let open_ms = clock.elapsed().as_secs_f64() * 1000.0;
    std::thread::sleep(Duration::from_secs(2));
    let footprint_open = footprint();

    let clock = Instant::now();
    let edited = session.change(&uri, range, replacement);
    let edit_ms = clock.elapsed().as_secs_f64() * 1000.0;
    std::thread::sleep(Duration::from_secs(2));
    let footprint_edit = footprint();
    let navigation = navigate(&session, &uri, &edited_text, &target);

    println!(
        "{}",
        json!({
            "mode": mode,
            "model": model.name,
            "bytes": model.text.len(),
            "edit": edit,
            "open_ms": open_ms,
            "open_diagnostics": opened.len(),
            "edit_ms": edit_ms,
            "edit_diagnostics": edited.len(),
            "edit_diagnostic_lines": edited.iter().map(|d| d.range.start.line).take(12).collect::<Vec<_>>(),
            "footprint_mib_before_open": footprint_before.0,
            "footprint_mib_after_open": footprint_open.0,
            "footprint_mib_after_edit": footprint_edit.0,
            "lifetime_peak_footprint_mib": footprint_edit.1,
            "navigation": navigation,
        })
    );
}

/// Offsets of `#<id>` tokens in `text` (not followed by a digit), each with whether it defines.
fn occurrences(text: &str, id: &str) -> Vec<(usize, bool)> {
    let needle = format!("#{id}");
    let bytes = text.as_bytes();
    text.match_indices(&needle)
        .map(|(at, _)| at)
        .filter(|&at| !bytes.get(at + needle.len()).is_some_and(u8::is_ascii_digit))
        .map(|at| {
            let rest = text[at + needle.len()..].trim_start();
            (at, rest.starts_with('='))
        })
        .collect()
}

/// Times definition, references and completion at the first non-defining `#<target>` of the
/// edited text and checks them against a plain search of that text.
fn navigate(session: &Session, uri: &Url, text: &str, target: &str) -> serde_json::Value {
    let position_of = |offset: usize| {
        let line_start = text[..offset].rfind('\n').map_or(0, |n| n + 1);
        tower_lsp::lsp_types::Position::new(
            text[..line_start].matches('\n').count() as u32,
            text[line_start..offset].encode_utf16().count() as u32,
        )
    };
    let found = occurrences(text, target);
    let Some(&(at, _)) = found.iter().find(|(_, definition)| !definition) else {
        return json!({ "target": target, "skipped": "no reference" });
    };
    let expected_definition_line = found
        .iter()
        .rev()
        .find(|(_, definition)| *definition)
        .map(|&(offset, _)| position_of(offset).line);
    let document_position = TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position: position_of(at),
    };
    let backend = session.backend();

    let clock = Instant::now();
    let definition = session
        .block_on(backend.goto_definition(GotoDefinitionParams {
            text_document_position_params: document_position.clone(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }))
        .expect("definition succeeds");
    let definition_ms = clock.elapsed().as_secs_f64() * 1000.0;
    let definition_line = match definition {
        Some(GotoDefinitionResponse::Scalar(location)) => Some(location.range.start.line),
        _ => None,
    };

    let clock = Instant::now();
    let references = session
        .block_on(backend.references(ReferenceParams {
            text_document_position: document_position.clone(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: ReferenceContext {
                include_declaration: true,
            },
        }))
        .expect("references succeed")
        .unwrap_or_default();
    let references_ms = clock.elapsed().as_secs_f64() * 1000.0;

    let mut completion_position = document_position;
    completion_position.position.character += 1 + target.len() as u32;
    let clock = Instant::now();
    let completion = session
        .block_on(backend.completion(CompletionParams {
            text_document_position: completion_position,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }))
        .expect("completion succeeds");
    let completion_ms = clock.elapsed().as_secs_f64() * 1000.0;
    let label = format!("#{target}");
    let items = match completion {
        Some(CompletionResponse::List(list)) => list.items,
        Some(CompletionResponse::Array(items)) => items,
        None => Vec::new(),
    };

    json!({
        "target": target,
        "definition_ms": definition_ms,
        "definition_correct": definition_line == expected_definition_line,
        "references_ms": references_ms,
        "references": references.len(),
        "references_correct": references.len() == found.len(),
        "completion_ms": completion_ms,
        "completion_items": items.len(),
        "completion_has_target": items.iter().any(|item| item.label == label),
    })
}

/// Current and lifetime-peak physical footprint of this process in MiB.
#[cfg(target_os = "macos")]
fn footprint() -> (f64, f64) {
    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
    }
    const RUSAGE_INFO_V4: i32 = 4;
    // rusage_info_v4: 16-byte uuid, then u64 fields; ri_phys_footprint is the 8th and
    // ri_lifetime_max_phys_footprint the 29th.
    let mut buffer = [0u64; 64];
    let status = unsafe {
        proc_pid_rusage(
            std::process::id() as i32,
            RUSAGE_INFO_V4,
            buffer.as_mut_ptr(),
        )
    };
    assert_eq!(status, 0, "proc_pid_rusage failed");
    let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
    (mib(buffer[2 + 7]), mib(buffer[2 + 28]))
}

#[cfg(not(target_os = "macos"))]
fn footprint() -> (f64, f64) {
    (f64::NAN, f64::NAN)
}
