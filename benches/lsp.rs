#[allow(dead_code)]
#[path = "../examples/lsp_session/mod.rs"]
mod support;

use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use criterion::{
    BatchSize, BenchmarkGroup, Criterion, SamplingMode, criterion_group, criterion_main,
};
use serde_json::json;
use tower_lsp::LanguageServer;
use tower_lsp::lsp_types::*;

use support::{Model, Session};

fn models() -> Vec<Model> {
    if let Some(paths) = std::env::var_os("IFC_BENCH_MODEL") {
        return std::env::split_paths(&paths)
            .map(|path| Model::load(&path))
            .collect();
    }
    let samples = Path::new(env!("CARGO_MANIFEST_DIR")).join("samples");
    ["Infra-Bridge-IFC4x3.ifc", "Duplex_A_20110907-IFC2x3.ifc"]
        .iter()
        .map(|name| Model::load(&samples.join(name)))
        .collect()
}

fn configure(group: &mut BenchmarkGroup<'_, criterion::measurement::WallTime>) {
    group
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5))
        .sampling_mode(SamplingMode::Flat);
}

fn open_diagnostics(c: &mut Criterion, models: &[Model]) {
    let mut group = c.benchmark_group("open_diagnostics");
    configure(&mut group);
    for model in models {
        let mut session = Session::new(json!({}));
        let uri = model.uri();
        group.bench_function(&model.name, |b| {
            b.iter_batched(
                || model.text.clone(),
                |text| black_box(session.open(&uri, text)),
                BatchSize::PerIteration,
            )
        });
    }
    group.finish();
}

fn edit_valid_diagnostics(c: &mut Criterion, models: &[Model]) {
    let mut group = c.benchmark_group("edit_valid_diagnostics");
    configure(&mut group);
    for model in models {
        let mut session = Session::new(json!({}));
        let uri = model.uri();
        session.open(&uri, model.text.clone());
        let offset = model.cartesian_point_digit();
        let range = Range::new(model.position(offset), model.position(offset + 1));
        let original = &model.text[offset..offset + 1];
        let replacement = if original == "9" { "8" } else { "9" };
        let mut edited = false;
        group.bench_function(&model.name, |b| {
            b.iter(|| {
                edited = !edited;
                let text = if edited { replacement } else { original };
                black_box(session.change(&uri, range, text))
            })
        });
    }
    group.finish();
}

fn edit_syntax_error_diagnostics(c: &mut Criterion, models: &[Model]) {
    let mut group = c.benchmark_group("edit_syntax_error_diagnostics");
    configure(&mut group);
    for model in models {
        let mut session = Session::new(json!({}));
        let uri = model.uri();
        let position = model.cartesian_point_digit();
        let insert_at = model.position(position);
        let inserted = Range::new(
            insert_at,
            Position::new(insert_at.line, insert_at.character + 1),
        );
        let clean_count = session.open(&uri, model.text.clone()).len();
        assert!(
            session
                .change(&uri, Range::new(insert_at, insert_at), "=")
                .len()
                > clean_count,
            "inserting `=` into a cartesian point should add a syntax diagnostic"
        );
        session.change(&uri, inserted, "");
        group.bench_function(&model.name, |b| {
            b.iter_custom(|iters| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iters {
                    let start = Instant::now();
                    black_box(session.change(&uri, Range::new(insert_at, insert_at), "="));
                    elapsed += start.elapsed();
                    session.change(&uri, inserted, "");
                }
                elapsed
            })
        });
    }
    group.finish();
}

/// Rewrites the first entity-reference parameter to a bare `#` and returns the position after it.
fn type_bare_reference(session: &mut Session, model: &Model) -> Position {
    let text = &model.text;
    let hash = text
        .find("IFCAXIS2PLACEMENT3D(#")
        .map(|index| index + "IFCAXIS2PLACEMENT3D(".len())
        .or_else(|| text.find("(#").map(|index| index + 1))
        .unwrap_or_else(|| panic!("{} has no entity-reference parameter", model.name));
    let digits_end = hash
        + 1
        + text[hash + 1..]
            .find(|c: char| !c.is_ascii_digit())
            .expect("reference is followed by a delimiter");
    let uri = model.uri();
    session.change(
        &uri,
        Range::new(model.position(hash + 1), model.position(digits_end)),
        "",
    );
    model.position(hash + 1)
}

fn reference_completion(c: &mut Criterion, models: &[Model]) {
    let mut group = c.benchmark_group("reference_completion");
    configure(&mut group);
    for model in models {
        let mut session = Session::new(json!({}));
        let uri = model.uri();
        session.open(&uri, model.text.clone());
        let position = type_bare_reference(&mut session, model);
        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams::new(
                TextDocumentIdentifier::new(uri.clone()),
                position,
            ),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        };
        let complete = |session: &Session| {
            session
                .block_on(session.backend().completion(params.clone()))
                .expect("completion succeeds")
        };
        let references = match complete(&session) {
            Some(CompletionResponse::List(list)) => list
                .items
                .iter()
                .filter(|item| item.kind == Some(CompletionItemKind::REFERENCE))
                .count(),
            _ => 0,
        };
        assert!(
            references > 0,
            "{} offers no reference completions",
            model.name
        );
        group.bench_function(&model.name, |b| b.iter(|| black_box(complete(&session))));
    }
    group.finish();
}

fn document_symbols(c: &mut Criterion, models: &[Model]) {
    let mut group = c.benchmark_group("document_symbols");
    configure(&mut group);
    for model in models {
        let mut session = Session::new(json!({}));
        let uri = model.uri();
        session.open(&uri, model.text.clone());
        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier::new(uri),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        group.bench_function(&model.name, |b| {
            b.iter(|| {
                let symbols = session
                    .block_on(session.backend().document_symbol(params.clone()))
                    .expect("document symbols succeed");
                black_box(symbols.expect("document has an outline"))
            })
        });
    }
    group.finish();
}

fn benches(c: &mut Criterion) {
    let models = models();
    open_diagnostics(c, &models);
    edit_valid_diagnostics(c, &models);
    edit_syntax_error_diagnostics(c, &models);
    reference_completion(c, &models);
    document_symbols(c, &models);
}

criterion_group!(lsp, benches);
criterion_main!(lsp);
