//! LSP backend and document store.
//! This is the `tower-lsp` entry point: it owns the open-document map, the shared tree-sitter
//! parser, and the in-memory schema docs used by hover and datatype diagnostics.
//! Request handlers stay thin here and delegate document-specific work to the feature modules.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::{Error as JsonRpcError, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tree_sitter::Parser;

use crate::config::{ServerConfig, expand_schema_candidates, parse_server_config};
use crate::diagnostics::datatype;
use crate::document::{Document, DocumentParseMetrics, DocumentParseMode};
use crate::features::{definition, hover, references};
use crate::schema::{
    SchemaDocCollection, inspect_local_schema_name, load_local_schema, normalize_name,
};

#[derive(Debug, Deserialize)]
pub struct DiskDocumentParams {
    pub uri: Url,
}

#[derive(Debug)]
pub struct VisibleDiagnosticsParams {
    pub uri: Url,
    pub ranges: Vec<Range>,
}

#[derive(Debug, Default)]
struct SchemaConfigState {
    forced_schema_name: Option<String>,
    additional_schema_paths: HashMap<String, std::path::PathBuf>,
    pending_init_config: Option<ServerConfig>,
}

pub struct Backend {
    client: Client,
    documents: Arc<RwLock<HashMap<Url, Document>>>,
    parser: Arc<RwLock<Parser>>,
    schema_docs: Arc<RwLock<SchemaDocCollection>>,
    schema_config: Arc<RwLock<SchemaConfigState>>,
}

const LARGE_FILE_DIAGNOSTICS_CAP_BYTES: usize = 50 * 1024 * 1024;

impl Backend {
    pub fn new(client: Client) -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("Error loading IFC parser");

        Self {
            client,
            documents: Arc::new(RwLock::new(HashMap::new())),
            parser: Arc::new(RwLock::new(parser)),
            schema_docs: Arc::new(RwLock::new(SchemaDocCollection::new())),
            schema_config: Arc::new(RwLock::new(SchemaConfigState::default())),
        }
    }

    async fn store_document(&self, document: Document, uri: &Url) {
        let mut documents = self.documents.write().await;
        documents.insert(uri.clone(), document);
    }

    async fn check_schema_support(&self, document: &Document) {
        let forced_schema_name = self.schema_config.read().await.forced_schema_name.clone();

        if let Some(forced_schema_name) = forced_schema_name.as_deref()
            && let Some(document_schema_name) = document.schema_name.as_deref()
        {
            let normalized_document_schema_name = normalize_name(document_schema_name);
            if normalized_document_schema_name != forced_schema_name {
                self.client
                    .show_message(
                        MessageType::WARNING,
                        format!(
                            "This IFC file declares schema `{}`, but the server is configured to force schema `{}`. Diagnostics and hover information use the forced schema.",
                            normalized_document_schema_name,
                            forced_schema_name
                        ),
                    )
                    .await;
            }
        }

        if self.selected_schema_name(document).await.is_none() {
            self.client
                .show_message(
                    MessageType::WARNING,
                    "This IFC file uses an unknown or unsupported schema version. Schema-aware diagnostics and hover information may be incomplete.",
                )
                .await;
        }
    }

    async fn parse_document(
        &self,
        text: String,
        mode: DocumentParseMode,
    ) -> (Document, DocumentParseMetrics) {
        let mut parser = self.parser.write().await;
        let result = Document::parse_with_metrics(&mut parser, text, mode);
        drop(parser);
        result
    }

    async fn parse_document_with_diagnostics(&self, uri: &Url, text: String) -> Document {
        // Buffer parses happen on every keystroke. Skip detailed parse-metrics
        // logging here and emit only a one-line summary, and only for files
        // large enough that the cost is interesting. Disk-loaded documents
        // (`open_from_disk`) still get the full metrics log because they are
        // the path where parse cost actually matters.
        const SLOW_PARSE_LOG_THRESHOLD_BYTES: usize = 1024 * 1024;

        let started = std::time::Instant::now();
        let (document, metrics) = self.parse_document(text, DocumentParseMode::Full).await;

        let diagnostics_started = std::time::Instant::now();
        let diagnostics = self.collect_diagnostics(&document).await;
        let diagnostics_ms = diagnostics_started.elapsed().as_millis();

        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;

        if metrics.source_bytes >= SLOW_PARSE_LOG_THRESHOLD_BYTES {
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "Document parsed: {} ({} bytes, total {}ms = parse {}ms + diagnostics {}ms)",
                        uri,
                        metrics.source_bytes,
                        started.elapsed().as_millis(),
                        metrics.total_parse_ms(),
                        diagnostics_ms
                    ),
                )
                .await;
        }
        document
    }

    async fn log_parse_metrics(&self, label: &str, uri: &Url, metrics: &DocumentParseMetrics) {
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "{}: {} (mode={}, {} bytes, total {}ms = tree {}ms + schema {}ms + indexes {}ms; definitions={}, reference_groups={}, reference_ranges={}, instances={}, parameter_values={})",
                    label,
                    uri,
                    metrics.mode.as_str(),
                    metrics.source_bytes,
                    metrics.total_parse_ms(),
                    metrics.tree_parse_ms,
                    metrics.schema_detect_ms,
                    metrics.index_build_ms,
                    metrics.definitions,
                    metrics.reference_groups,
                    metrics.reference_ranges,
                    metrics.instances,
                    metrics.parameter_values
                ),
            )
            .await;
    }

    async fn collect_diagnostics(&self, document: &Document) -> Vec<Diagnostic> {
        if document.parse_mode != DocumentParseMode::Full {
            return Vec::new();
        }

        let selected_schema_name = self.selected_schema_name(document).await;
        if let Some(schema_name) = selected_schema_name.as_deref() {
            let schema_docs = self.schema_docs.read().await;
            schema_docs
                .get(schema_name)
                .map(|schema| {
                    datatype::collect_with_schema_name(document, schema, Some(schema_name))
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    async fn selected_schema_name(&self, document: &Document) -> Option<String> {
        let (forced_schema_name, additional_path) = {
            let schema_config = self.schema_config.read().await;
            let forced_schema_name = schema_config.forced_schema_name.clone();
            let additional_path = document.schema_name.as_ref().and_then(|schema_name| {
                schema_config
                    .additional_schema_paths
                    .get(&normalize_name(schema_name))
                    .cloned()
            });
            (forced_schema_name, additional_path)
        };

        if let Some(schema_name) = forced_schema_name {
            return Some(schema_name);
        }

        let schema_name = document.schema_name.as_ref()?;
        let normalized = normalize_name(schema_name);

        {
            let schema_docs = self.schema_docs.read().await;
            if schema_docs.get(&normalized).is_some() {
                return Some(normalized);
            }
        }

        let path = additional_path?;
        match load_local_schema(&path) {
            Ok((loaded_schema_name, schema)) => {
                let normalized_loaded = normalize_name(&loaded_schema_name);
                let mut schema_docs = self.schema_docs.write().await;
                if schema_docs.get(&normalized_loaded).is_none() {
                    schema_docs.insert(&loaded_schema_name, schema);
                }
                Some(normalized_loaded)
            }
            Err(error) => {
                self.client
                    .log_message(MessageType::WARNING, error.to_string())
                    .await;
                None
            }
        }
    }

    async fn apply_config(&self, config: ServerConfig) {
        let mut schema_docs = SchemaDocCollection::new();
        let mut next_state = SchemaConfigState::default();

        if let Some(path) = config.overwrite_exp_schema_with_local.as_ref() {
            match load_local_schema(path) {
                Ok((schema_name, schema)) => {
                    next_state.forced_schema_name = Some(normalize_name(&schema_name));
                    schema_docs.insert(&schema_name, schema);
                }
                Err(error) => {
                    self.client
                        .log_message(MessageType::WARNING, error.to_string())
                        .await;
                }
            }
        }

        for path in &config.add_local_schema_to_selection {
            for candidate in expand_schema_candidates(path) {
                match inspect_local_schema_name(&candidate) {
                    Ok(schema_name) => {
                        let normalized = normalize_name(&schema_name);
                        if let Some(previous) = next_state
                            .additional_schema_paths
                            .insert(normalized.clone(), candidate.clone())
                        {
                            self.client
                                .log_message(
                                    MessageType::WARNING,
                                    format!(
                                        "Duplicate local schema `{}` configured at `{}` and `{}`; using `{}`",
                                        normalized,
                                        previous.display(),
                                        candidate.display(),
                                        candidate.display()
                                    ),
                                )
                                .await;
                        }
                    }
                    Err(error) => {
                        self.client
                            .log_message(MessageType::WARNING, error.to_string())
                            .await;
                    }
                }
            }
        }

        *self.schema_docs.write().await = schema_docs;
        *self.schema_config.write().await = next_state;
    }

    pub async fn open_from_disk(&self, params: DiskDocumentParams) -> Result<()> {
        let uri = params.uri;
        let path = uri.to_file_path().map_err(|_| {
            JsonRpcError::invalid_params(format!("uri is not a local file path: {}", uri))
        })?;

        let started = std::time::Instant::now();

        let read_started = std::time::Instant::now();
        let text = tokio::fs::read_to_string(&path).await.map_err(|e| {
            let mut err = JsonRpcError::internal_error();
            err.message = format!("failed to read {}: {}", path.display(), e).into();
            err
        })?;
        let read_ms = read_started.elapsed().as_millis();
        let bytes = text.len();
        let mode = if bytes > LARGE_FILE_DIAGNOSTICS_CAP_BYTES {
            DocumentParseMode::NavigationOnly
        } else {
            DocumentParseMode::Full
        };

        let (document, metrics) = self.parse_document(text, mode).await;
        self.log_parse_metrics("Disk document parse", &uri, &metrics)
            .await;

        let schema_started = std::time::Instant::now();
        self.check_schema_support(&document).await;
        let schema_support_ms = schema_started.elapsed().as_millis();

        let store_started = std::time::Instant::now();
        self.store_document(document, &uri).await;
        let store_ms = store_started.elapsed().as_millis();

        let total_ms = started.elapsed().as_millis();
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Document opened from disk: {} ({} bytes, total {}ms = read {}ms + parse {}ms + schema-check {}ms + store {}ms)",
                    uri,
                    bytes,
                    total_ms,
                    read_ms,
                    metrics.total_parse_ms(),
                    schema_support_ms,
                    store_ms
                ),
            )
            .await;
        Ok(())
    }

    pub async fn close_from_disk(&self, params: DiskDocumentParams) -> Result<()> {
        let uri = params.uri;
        {
            let mut documents = self.documents.write().await;
            documents.remove(&uri);
        }
        self.client
            .publish_diagnostics(uri.clone(), Vec::new(), None)
            .await;
        self.client
            .log_message(
                MessageType::INFO,
                format!("Document closed from disk: {}", uri),
            )
            .await;
        Ok(())
    }

    pub async fn diagnostics(&self, params: Value) -> Result<Vec<Diagnostic>> {
        let uri = diagnostics_uri_from_value(&params)?;
        let documents = self.documents.read().await;
        let document = match documents.get(&uri) {
            Some(document) => document,
            None => return Ok(Vec::new()),
        };

        Ok(self.collect_diagnostics(document).await)
    }

    pub async fn visible_diagnostics(&self, params: Value) -> Result<Vec<Diagnostic>> {
        const CONTEXT_LINES: u32 = 20;
        const MAX_LINES_PER_RANGE: u32 = 500;

        let params = visible_diagnostics_params_from_value(&params)?;
        let documents = self.documents.read().await;
        let source_document = match documents.get(&params.uri) {
            Some(document) => document,
            None => return Ok(Vec::new()),
        };

        let selected_schema_name = self.selected_schema_name(source_document).await;
        let Some(schema_name) = selected_schema_name.as_deref() else {
            return Ok(Vec::new());
        };
        let schema_docs = self.schema_docs.read().await;
        let Some(schema) = schema_docs.get(schema_name) else {
            return Ok(Vec::new());
        };

        let line_offsets = LineOffsets::from_text(&source_document.text);

        let mut diagnostics = Vec::new();
        for range in params.ranges {
            let Some(slice) = visible_diagnostic_slice(
                &source_document.text,
                &line_offsets,
                range,
                CONTEXT_LINES,
                MAX_LINES_PER_RANGE,
            ) else {
                continue;
            };
            let (slice_document, _metrics) = self
                .parse_document(slice.text, DocumentParseMode::Full)
                .await;
            diagnostics.extend(
                datatype::collect_visible_with_schema_name(
                    &slice_document,
                    source_document,
                    schema,
                    Some(schema_name),
                )
                .into_iter()
                .map(|mut diagnostic| {
                    shift_diagnostic_lines(&mut diagnostic, slice.start_line);
                    diagnostic
                }),
            );
        }
        Ok(diagnostics)
    }
}

struct VisibleDiagnosticSlice {
    text: String,
    start_line: u32,
}

/// Byte offsets of each line start in the source text, plus a terminator equal
/// to `text.len()`. `line_offsets[i]` gives the byte index where line `i`
/// begins, so a `[start_line, end_line]` line range maps to the byte slice
/// `text[line_offsets[start_line]..line_offsets[end_line + 1]]`.
///
/// Building this once per `visible_diagnostics` call avoids re-scanning the
/// whole document for every visible range when multiple editors view the same
/// file.
struct LineOffsets {
    offsets: Vec<usize>,
}

impl LineOffsets {
    fn from_text(text: &str) -> Self {
        let mut offsets = Vec::with_capacity(text.len() / 64 + 1);
        offsets.push(0);
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                offsets.push(index + 1);
            }
        }
        if *offsets.last().expect("offsets always contains zero") != text.len() {
            offsets.push(text.len());
        }
        Self { offsets }
    }

    fn line_count(&self) -> u32 {
        // The last entry is the terminator (text length); subtract it.
        (self.offsets.len().saturating_sub(1)) as u32
    }

    fn slice_bytes<'a>(&self, text: &'a str, start_line: u32, end_line: u32) -> Option<&'a str> {
        let start = *self.offsets.get(start_line as usize)?;
        let end_index = (end_line as usize + 1).min(self.offsets.len() - 1);
        let end = self.offsets[end_index];
        if start >= end {
            return None;
        }
        Some(&text[start..end])
    }
}

fn visible_diagnostic_slice(
    text: &str,
    line_offsets: &LineOffsets,
    range: Range,
    context_lines: u32,
    max_lines_per_range: u32,
) -> Option<VisibleDiagnosticSlice> {
    let line_count = line_offsets.line_count();
    if line_count == 0 {
        return None;
    }
    let last_line = line_count.saturating_sub(1);

    let visible_start = range.start.line.saturating_sub(context_lines).min(last_line);
    let visible_end = range
        .end
        .line
        .saturating_add(context_lines)
        .min(visible_start.saturating_add(max_lines_per_range))
        .min(last_line);

    let slice = line_offsets.slice_bytes(text, visible_start, visible_end)?;

    Some(VisibleDiagnosticSlice {
        text: slice.to_string(),
        start_line: visible_start,
    })
}

fn shift_diagnostic_lines(diagnostic: &mut Diagnostic, line_offset: u32) {
    diagnostic.range.start.line = diagnostic.range.start.line.saturating_add(line_offset);
    diagnostic.range.end.line = diagnostic.range.end.line.saturating_add(line_offset);
}

// `ifc/diagnostics` is invoked from the VS Code extension's pull-diagnostics
// path. We've seen the client send the URI in several shapes depending on how
// the request was wired (raw string, `{ uri }` object, single-item array), so
// the parser accepts all three rather than failing the request and losing the
// diagnostics refresh.
fn diagnostics_uri_from_value(value: &Value) -> Result<Url> {
    match value {
        Value::String(uri) => Url::parse(uri)
            .map_err(|error| JsonRpcError::invalid_params(format!("invalid URI: {error}"))),
        Value::Object(object) => object
            .get("uri")
            .ok_or_else(|| JsonRpcError::invalid_params("missing diagnostics URI"))
            .and_then(diagnostics_uri_from_value),
        Value::Array(items) => items
            .first()
            .ok_or_else(|| JsonRpcError::invalid_params("missing diagnostics URI"))
            .and_then(diagnostics_uri_from_value),
        _ => Err(JsonRpcError::invalid_params(
            "diagnostics URI must be a string, { uri }, or single-item array",
        )),
    }
}

fn visible_diagnostics_params_from_value(value: &Value) -> Result<VisibleDiagnosticsParams> {
    match value {
        Value::Array(items) => items
            .first()
            .ok_or_else(|| JsonRpcError::invalid_params("missing visible diagnostics params"))
            .and_then(visible_diagnostics_params_from_value),
        Value::Object(object) => {
            let uri = object
                .get("uri")
                .ok_or_else(|| JsonRpcError::invalid_params("missing visible diagnostics URI"))
                .and_then(url_from_value)?;
            let ranges = object
                .get("ranges")
                .ok_or_else(|| JsonRpcError::invalid_params("missing visible diagnostics ranges"))
                .and_then(ranges_from_value)?;
            Ok(VisibleDiagnosticsParams { uri, ranges })
        }
        _ => Err(JsonRpcError::invalid_params(
            "visible diagnostics params must be { uri, ranges } or a single-item array",
        )),
    }
}

// The URI inside a custom-method payload must always be a plain string. If the
// extension forgets `.toString()` on a `vscode.Uri`, its `toJSON()` serializes
// as `{scheme, path, ...}` and corrupts the wire format — see
// https://github.com/microsoft/vscode/issues/121198. We reject that shape
// rather than absorb it so regressions surface at the call site.
fn url_from_value(value: &Value) -> Result<Url> {
    match value {
        Value::String(uri) => Url::parse(uri)
            .map_err(|error| JsonRpcError::invalid_params(format!("invalid URI: {error}"))),
        Value::Object(_) => Err(JsonRpcError::invalid_params(
            "URI must be a string; received a JSON object (likely a raw `vscode.Uri` \
             that was not stringified before sending — call `.toString()` on the \
             client side)",
        )),
        _ => Err(JsonRpcError::invalid_params(
            "URI must be a string",
        )),
    }
}

fn ranges_from_value(value: &Value) -> Result<Vec<Range>> {
    let Value::Array(items) = value else {
        return Err(JsonRpcError::invalid_params(
            "visible diagnostics ranges must be an array",
        ));
    };

    items
        .iter()
        .cloned()
        .map(|item| {
            serde_json::from_value::<Range>(item).map_err(|error| {
                JsonRpcError::invalid_params(format!("invalid visible diagnostics range: {error}"))
            })
        })
        .collect()
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let pending_init_config = params
            .initialization_options
            .as_ref()
            .map(parse_server_config)
            .unwrap_or_default();

        let mut schema_config = self.schema_config.write().await;
        schema_config.pending_init_config = Some(pending_init_config);

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                experimental: Some(serde_json::json!({
                    "ifcLargeFileFeatures": true,
                })),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "IFC LSP server initialized!")
            .await;

        let load_errors = self.schema_docs.read().await.load_errors().to_vec();
        for error in load_errors {
            self.client.log_message(MessageType::WARNING, error).await;
        }

        let pending_init_config = {
            let mut schema_config = self.schema_config.write().await;
            schema_config.pending_init_config.take()
        };

        if let Some(config) = pending_init_config {
            self.apply_config(config).await;
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;

        self.client
            .log_message(MessageType::INFO, format!("Document opened: {}", uri))
            .await;

        let document = self.parse_document_with_diagnostics(&uri, text).await;
        self.check_schema_support(&document).await;
        self.store_document(document, &uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Some(change) = params.content_changes.into_iter().next() {
            let document = self
                .parse_document_with_diagnostics(&uri, change.text)
                .await;
            self.check_schema_support(&document).await;
            self.store_document(document, &uri).await;
        }
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let forced_schema_name = self.schema_config.read().await.forced_schema_name.clone();

        let documents = self.documents.read().await;
        let document = match documents.get(&uri) {
            Some(document) => document,
            None => return Ok(None),
        };

        let schema_docs = self.schema_docs.read().await;

        Ok(hover::hover(
            document,
            position,
            &schema_docs,
            forced_schema_name.as_deref(),
        ))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let documents = self.documents.read().await;
        let document = match documents.get(&uri) {
            Some(document) => document,
            None => return Ok(None),
        };

        Ok(definition::goto_definition(&uri, document, position))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let documents = self.documents.read().await;
        let document = match documents.get(&uri) {
            Some(document) => document,
            None => return Ok(None),
        };

        Ok(references::find_references(&uri, document, position))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URI: &str = "file:///example.ifc";

    #[test]
    fn diagnostics_uri_accepts_bare_string() {
        let value = Value::String(URI.into());
        let uri = diagnostics_uri_from_value(&value).expect("parse string");
        assert_eq!(uri.as_str(), URI);
    }

    #[test]
    fn diagnostics_uri_accepts_object_with_uri_field() {
        let value = serde_json::json!({ "uri": URI });
        let uri = diagnostics_uri_from_value(&value).expect("parse object");
        assert_eq!(uri.as_str(), URI);
    }

    #[test]
    fn diagnostics_uri_accepts_single_item_array() {
        let value = serde_json::json!([URI]);
        let uri = diagnostics_uri_from_value(&value).expect("parse array");
        assert_eq!(uri.as_str(), URI);
    }

    #[test]
    fn diagnostics_uri_accepts_array_of_object() {
        let value = serde_json::json!([{ "uri": URI }]);
        let uri = diagnostics_uri_from_value(&value).expect("parse array of object");
        assert_eq!(uri.as_str(), URI);
    }

    #[test]
    fn diagnostics_uri_rejects_empty_array() {
        let value = serde_json::json!([]);
        assert!(diagnostics_uri_from_value(&value).is_err());
    }

    #[test]
    fn diagnostics_uri_rejects_object_without_uri() {
        let value = serde_json::json!({ "other": URI });
        assert!(diagnostics_uri_from_value(&value).is_err());
    }

    #[test]
    fn diagnostics_uri_rejects_non_url_string() {
        let value = Value::String("not a url".into());
        assert!(diagnostics_uri_from_value(&value).is_err());
    }

    #[test]
    fn diagnostics_uri_rejects_unsupported_type() {
        let value = serde_json::json!(42);
        assert!(diagnostics_uri_from_value(&value).is_err());
    }

    #[test]
    fn url_from_value_accepts_plain_string() {
        let value = Value::String(URI.into());
        let uri = url_from_value(&value).expect("parse string");
        assert_eq!(uri.as_str(), URI);
    }

    #[test]
    fn url_from_value_rejects_vscode_uri_tojson_form() {
        let value = serde_json::json!({
            "$mid": 1,
            "scheme": "file",
            "path": "/example.ifc"
        });
        let err = url_from_value(&value).expect_err("must reject map form");
        assert!(
            err.message.contains("vscode.Uri") || err.message.contains("toString"),
            "error should hint at the client-side fix: {}",
            err.message
        );
    }

    #[test]
    fn visible_diagnostics_params_round_trip_named_object() {
        let value = serde_json::json!({
            "uri": "file:///example.ifc",
            "ranges": [
                {
                    "start": { "line": 10, "character": 0 },
                    "end": { "line": 12, "character": 3 }
                }
            ]
        });
        let params = visible_diagnostics_params_from_value(&value).expect("parse params");
        assert_eq!(params.uri.scheme(), "file");
        assert_eq!(params.ranges.len(), 1);
        assert_eq!(params.ranges[0].start.line, 10);
    }

    #[test]
    fn visible_diagnostics_params_reject_vscode_uri_object() {
        let value = serde_json::json!({
            "uri": {
                "$mid": 1,
                "scheme": "file",
                "path": "/example.ifc"
            },
            "ranges": []
        });
        assert!(visible_diagnostics_params_from_value(&value).is_err());
    }

    #[test]
    fn line_offsets_indexes_each_line_start() {
        let text = "a\nbb\nccc\n";
        let offsets = LineOffsets::from_text(text);
        // line_count counts content lines (between starts); trailing terminator
        // is the byte length of the text.
        assert_eq!(offsets.line_count(), 3);
        assert_eq!(offsets.slice_bytes(text, 0, 0), Some("a\n"));
        assert_eq!(offsets.slice_bytes(text, 1, 1), Some("bb\n"));
        assert_eq!(offsets.slice_bytes(text, 0, 2), Some("a\nbb\nccc\n"));
    }

    #[test]
    fn line_offsets_handles_text_without_trailing_newline() {
        let text = "a\nbb";
        let offsets = LineOffsets::from_text(text);
        assert_eq!(offsets.line_count(), 2);
        assert_eq!(offsets.slice_bytes(text, 0, 1), Some("a\nbb"));
    }

    #[test]
    fn visible_diagnostic_slice_extracts_window_with_context() {
        let text = "L0\nL1\nL2\nL3\nL4\nL5\n";
        let offsets = LineOffsets::from_text(text);
        let range = Range {
            start: Position {
                line: 2,
                character: 0,
            },
            end: Position {
                line: 3,
                character: 0,
            },
        };
        let slice = visible_diagnostic_slice(text, &offsets, range, 1, 100)
            .expect("slice produces a window");
        assert_eq!(slice.start_line, 1);
        assert_eq!(slice.text, "L1\nL2\nL3\nL4\n");
    }

    #[test]
    fn visible_diagnostic_slice_caps_window_to_max_lines() {
        let text = "a\n".repeat(50);
        let offsets = LineOffsets::from_text(&text);
        let range = Range {
            start: Position {
                line: 5,
                character: 0,
            },
            end: Position {
                line: 45,
                character: 0,
            },
        };
        let slice = visible_diagnostic_slice(&text, &offsets, range, 0, 10)
            .expect("slice produces a window");
        // `max_lines_per_range` clamps the visible end relative to the start.
        assert_eq!(slice.start_line, 5);
        // Slice should cover lines 5..=15 inclusive (11 lines = max + 1).
        assert_eq!(slice.text.lines().count(), 11);
    }
}
