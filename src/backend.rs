//! LSP backend and document store.
//! This is the `tower-lsp` entry point: it owns the open-document map, the shared tree-sitter
//! parser, and the in-memory schema docs used by hover and datatype diagnostics.
//! Request handlers stay thin here and delegate document-specific work to the feature modules.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tracing::{debug, info, instrument, warn};

use crate::config::{ServerConfig, expand_schema_candidates, parse_server_config};
use crate::diagnostics::DiagnosticSnapshot;
use crate::diagnostics::scheduler::{DiagnosticScheduler, DiagnosticTicket};
use crate::document::{DEFAULT_AST_FILE_SIZE_LIMIT_BYTES, Document};
use crate::features::{
    completions, definition, document_highlight, document_symbols, hover, inlay_hints, references,
    related_entities, scaffold_completions, semantic_tokens, signature_help,
};
use crate::schema::{
    SchemaDoc, SchemaDocCollection, inspect_local_schema_name, load_local_schema, normalize_name,
};

#[derive(Clone, Debug)]
struct ConfigState {
    forced_schema_name: Option<String>,
    additional_schema_paths: HashMap<String, std::path::PathBuf>,
    pending_init_config: Option<ServerConfig>,
    ast_file_size_limit_bytes: usize,
    semantic_tokens_enabled: bool,
    completion_snippet_support: bool,
    completion_documentation_resolve_support: bool,
}

impl Default for ConfigState {
    fn default() -> Self {
        Self {
            forced_schema_name: None,
            additional_schema_paths: HashMap::new(),
            pending_init_config: None,
            ast_file_size_limit_bytes: DEFAULT_AST_FILE_SIZE_LIMIT_BYTES,
            semantic_tokens_enabled: true,
            completion_snippet_support: false,
            completion_documentation_resolve_support: false,
        }
    }
}

pub struct Backend {
    client: Client,
    documents: Arc<RwLock<HashMap<Url, Document>>>,
    schema_docs: Arc<RwLock<SchemaDocCollection>>,
    config: Arc<RwLock<ConfigState>>,
    ast_skip_warning_shown: Arc<RwLock<HashSet<Url>>>,
    unknown_schema_warned: Arc<RwLock<HashMap<Url, Option<String>>>>,
    diagnostic_scheduler: DiagnosticScheduler,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        let diagnostic_scheduler = DiagnosticScheduler::new(client.clone());
        Self {
            client,
            documents: Arc::new(RwLock::new(HashMap::new())),
            schema_docs: Arc::new(RwLock::new(SchemaDocCollection::new())),
            config: Arc::new(RwLock::new(ConfigState::default())),
            ast_skip_warning_shown: Arc::new(RwLock::new(HashSet::new())),
            unknown_schema_warned: Arc::new(RwLock::new(HashMap::new())),
            diagnostic_scheduler,
        }
    }

    async fn ast_file_size_limit_bytes(&self) -> usize {
        self.config.read().await.ast_file_size_limit_bytes
    }

    #[instrument(skip(self), fields(uri = %uri, text_len, limit_bytes))]
    async fn check_ast_support(&self, uri: &Url, text_len: usize, limit_bytes: usize) -> bool {
        let mut shown = self.ast_skip_warning_shown.write().await;
        if text_len <= limit_bytes {
            shown.remove(uri);
            return false;
        }

        if shown.insert(uri.clone()) {
            self.client
                .show_message(
                    MessageType::WARNING,
                    format!(
                        "This IFC file is larger than the AST parsing limit ({} MB).\nNavigation and basic hover remain available, but schema diagnostics and derived-value hover are disabled.",
                        limit_bytes / 1024 / 1024
                    ),
                )
                .await;
            warn!("skipping AST parse because document exceeds configured size limit");
            return true;
        }

        false
    }

    #[instrument(skip(self), fields(uri = %uri, schema_name = schema_name.unwrap_or("<none>")))]
    async fn check_schema_support(&self, uri: &Url, schema_name: Option<&str>) {
        let forced_schema_name = self.config.read().await.forced_schema_name.clone();

        if let Some(forced_schema_name) = forced_schema_name.as_deref()
            && let Some(document_schema_name) = schema_name
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

        let schema_known = self.selected_schema_name(schema_name).await.is_some();
        let load_error = match (schema_known, schema_name) {
            (false, Some(schema_name)) => self
                .schema_docs
                .read()
                .await
                .official_schema_error(schema_name)
                .map(|error| (schema_name.to_string(), error.to_string())),
            _ => None,
        };

        let mut warned = self.unknown_schema_warned.write().await;
        if should_warn_unknown_schema(
            &mut warned,
            uri,
            schema_name,
            schema_known,
            load_error.as_ref().map(|(_, error)| error.as_str()),
        ) {
            drop(warned);
            match load_error {
                Some((schema_name, error)) => {
                    warn!(schema_name, error, "bundled schema failed to load");
                    self.client
                        .show_message(
                            MessageType::ERROR,
                            format!(
                                "The bundled schema `{}` failed to load, so schema-aware diagnostics and hover information are unavailable: {error}",
                                normalize_name(&schema_name)
                            ),
                        )
                        .await;
                }
                None => {
                    warn!("document schema is unknown or unsupported");
                    self.client
                        .show_message(
                            MessageType::WARNING,
                            "This IFC file uses an unknown or unsupported schema version. Schema-aware diagnostics and hover information may be incomplete.",
                        )
                        .await;
                }
            }
        }
    }

    #[instrument(skip(self, text), fields(uri = %uri, text_len = text.len()))]
    async fn load_text_as_active_document(&self, uri: &Url, text: String) -> DiagnosticSnapshot {
        let ast_file_size_limit_bytes = self.ast_file_size_limit_bytes().await;
        if self
            .check_ast_support(uri, text.len(), ast_file_size_limit_bytes)
            .await
        {
            tokio::task::yield_now().await;
        }

        let mut documents = self.documents.write().await;
        unload_other_documents(&mut documents, uri);

        let document = documents
            .entry(uri.clone())
            .or_insert_with(|| Document::new_unloaded(String::new()));
        document.unload_parse_state();
        document.text = Arc::new(text);

        {
            let mut parser = new_parser();
            document.reload_parse_state(&mut parser, ast_file_size_limit_bytes);
        }

        info!(
            schema_name = document.schema_name.as_deref().unwrap_or("<none>"),
            has_ast = document.has_ast(),
            ast_skipped = document.ast_skipped,
            definition_count = document.definitions.len(),
            reference_id_count = document.references.len(),
            "reloaded active document"
        );
        let snapshot = DiagnosticSnapshot::from_document(document);
        drop(documents);

        self.check_schema_support(uri, snapshot.schema_name()).await;
        snapshot
    }

    #[instrument(skip(self, changes), fields(uri = %uri, change_count = changes.len()))]
    async fn apply_changes_to_active_document(
        &self,
        uri: &Url,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) -> Option<DiagnosticSnapshot> {
        let ast_file_size_limit_bytes = self.ast_file_size_limit_bytes().await;
        let mut documents = self.documents.write().await;
        unload_other_documents(&mut documents, uri);

        let document = documents.get_mut(uri)?;
        let mut parser = new_parser();
        if let Err(error) =
            document.apply_content_changes(&mut parser, &changes, ast_file_size_limit_bytes)
        {
            warn!(?error, "received invalid incremental document change");
        }

        info!(
            text_len = document.text.len(),
            schema_name = document.schema_name.as_deref().unwrap_or("<none>"),
            has_ast = document.has_ast(),
            ast_skipped = document.ast_skipped,
            definition_count = document.definitions.len(),
            reference_id_count = document.references.len(),
            "updated active document"
        );
        let text_len = document.text.len();
        let snapshot = DiagnosticSnapshot::from_document(document);
        drop(documents);

        self.check_ast_support(uri, text_len, ast_file_size_limit_bytes)
            .await;
        self.check_schema_support(uri, snapshot.schema_name()).await;
        Some(snapshot)
    }

    #[instrument(skip(self, documents), fields(uri = %uri))]
    async fn ensure_document_loaded(
        &self,
        documents: &mut HashMap<Url, Document>,
        uri: &Url,
    ) -> Option<Option<DiagnosticSnapshot>> {
        if documents.get(uri)?.is_parse_state_loaded() {
            return Some(None);
        }

        unload_other_documents(documents, uri);

        {
            let mut parser = new_parser();
            documents
                .get_mut(uri)?
                .reload_parse_state(&mut parser, self.ast_file_size_limit_bytes().await);
        }

        let snapshot = {
            let document = documents.get(uri)?;
            info!(
                schema_name = document.schema_name.as_deref().unwrap_or("<none>"),
                has_ast = document.has_ast(),
                ast_skipped = document.ast_skipped,
                definition_count = document.definitions.len(),
                reference_id_count = document.references.len(),
                "reloaded document parse state on demand"
            );
            DiagnosticSnapshot::from_document(document)
        };

        Some(Some(snapshot))
    }

    #[instrument(skip(self, ticket, snapshot))]
    async fn schedule_diagnostics(&self, ticket: DiagnosticTicket, snapshot: DiagnosticSnapshot) {
        let schema = self.resolve_schema(snapshot.schema_name()).await;
        self.diagnostic_scheduler
            .schedule(ticket, snapshot, schema)
            .await;
    }

    #[instrument(skip(self, snapshot), fields(uri = %uri))]
    async fn request_time_diagnostics(&self, snapshot: Option<DiagnosticSnapshot>, uri: &Url) {
        if let Some(snapshot) = snapshot {
            let ticket = self.diagnostic_scheduler.begin(uri.clone()).await;
            self.schedule_diagnostics(ticket, snapshot).await;
        }
    }

    async fn forced_schema_and_additional_path(
        &self,
        schema_name: Option<&str>,
    ) -> (Option<String>, Option<std::path::PathBuf>) {
        let config = self.config.read().await;
        let forced_schema_name = config.forced_schema_name.clone();
        let additional_path = schema_name.and_then(|schema_name| {
            config
                .additional_schema_paths
                .get(&normalize_name(schema_name))
                .cloned()
        });
        (forced_schema_name, additional_path)
    }

    #[instrument(skip(self), fields(schema_name = schema_name.unwrap_or("<none>")))]
    async fn selected_schema_name(&self, schema_name: Option<&str>) -> Option<String> {
        let (forced_schema_name, additional_path) =
            self.forced_schema_and_additional_path(schema_name).await;

        if let Some(schema_name) = forced_schema_name {
            return Some(schema_name);
        }

        let schema_name = schema_name?;
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
                    info!(
                        schema_name = normalized_loaded,
                        path = %path.display(),
                        "loaded local schema documentation"
                    );
                }
                Some(normalized_loaded)
            }
            Err(error) => {
                warn!(error = %error, path = %path.display(), "failed to load local schema");
                None
            }
        }
    }

    /// Resolves a schema name to its loaded `SchemaDoc` under a single `schema_docs` lock
    /// acquisition, so a concurrent `apply_config` reload (which fully replaces the
    /// `schema_docs` map) can never swap the map out between "name exists" and "fetch by
    /// name" the way it could when those were two separate lock acquisitions. Use this
    /// instead of `selected_schema_name` wherever the schema doc itself is needed, not just
    /// its resolved name.
    #[instrument(skip(self), fields(schema_name = schema_name.unwrap_or("<none>")))]
    async fn resolve_schema(&self, schema_name: Option<&str>) -> Option<(String, Arc<SchemaDoc>)> {
        let (forced_schema_name, additional_path) =
            self.forced_schema_and_additional_path(schema_name).await;

        if let Some(forced_schema_name) = forced_schema_name {
            let schema_docs = self.schema_docs.read().await;
            return schema_docs
                .get_shared(&forced_schema_name)
                .map(|schema| (forced_schema_name, schema));
        }

        let schema_name = schema_name?;
        let normalized = normalize_name(schema_name);

        {
            let schema_docs = self.schema_docs.read().await;
            if let Some(schema) = schema_docs.get_shared(&normalized) {
                return Some((normalized, schema));
            }
        }

        let path = additional_path?;
        match load_local_schema(&path) {
            Ok((loaded_schema_name, schema)) => {
                let normalized_loaded = normalize_name(&loaded_schema_name);
                let mut schema_docs = self.schema_docs.write().await;
                if schema_docs.get(&normalized_loaded).is_none() {
                    schema_docs.insert(&loaded_schema_name, schema);
                    info!(
                        schema_name = normalized_loaded,
                        path = %path.display(),
                        "loaded local schema documentation"
                    );
                }
                let resolved = schema_docs.get_shared(&normalized_loaded)?;
                Some((normalized_loaded, resolved))
            }
            Err(error) => {
                warn!(error = %error, path = %path.display(), "failed to load local schema");
                None
            }
        }
    }

    #[instrument(skip(self, config), fields(ast_file_size_limit_bytes = config.ast_file_size_limit_bytes))]
    async fn apply_config(&self, config: ServerConfig) {
        let mut schema_docs = self.schema_docs.read().await.sharing_official();
        let mut next_state = {
            let current_state = self.config.read().await;
            config_state_with_client_capabilities(&config, &current_state)
        };

        if let Some(path) = config.overwrite_exp_schema_with_local.as_ref() {
            match load_local_schema(path) {
                Ok((schema_name, schema)) => {
                    next_state.forced_schema_name = Some(normalize_name(&schema_name));
                    schema_docs.insert(&schema_name, schema);
                    info!(
                        schema_name = next_state.forced_schema_name.as_deref().unwrap_or("<none>"),
                        path = %path.display(),
                        "configured forced local schema"
                    );
                }
                Err(error) => {
                    warn!(error = %error, path = %path.display(), "failed to load forced local schema");
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
                            warn!(
                                schema_name = normalized,
                                previous_path = %previous.display(),
                                candidate_path = %candidate.display(),
                                "duplicate local schema configuration; using latest path"
                            );
                        }
                    }
                    Err(error) => {
                        warn!(error = %error, path = %candidate.display(), "failed to inspect local schema");
                    }
                }
            }
        }

        info!(
            forced_schema = next_state.forced_schema_name.as_deref().unwrap_or("<none>"),
            additional_schema_count = next_state.additional_schema_paths.len(),
            semantic_tokens_enabled = next_state.semantic_tokens_enabled,
            "applied server configuration"
        );
        *self.schema_docs.write().await = schema_docs;
        *self.config.write().await = next_state;
    }
}

/// Builds the next `ConfigState` for a config reload. Only the reloadable/one-shot fields
/// below are explicitly reset or recomputed from `config`; everything else (negotiated-at-
/// `initialize()` client capabilities, and any field added later) is preserved by default via
/// `..current_state.clone()`, so a forgotten new negotiated field fails safe (stays as-is)
/// instead of silently resetting to its default on every reload.
fn config_state_with_client_capabilities(
    config: &ServerConfig,
    current_state: &ConfigState,
) -> ConfigState {
    ConfigState {
        forced_schema_name: None,
        additional_schema_paths: HashMap::new(),
        pending_init_config: None,
        ast_file_size_limit_bytes: config.ast_file_size_limit_bytes,
        semantic_tokens_enabled: config.semantic_tokens_enabled,
        ..current_state.clone()
    }
}

/// Drops AST-backed parse state from every open document except `active_uri`.
/// The backend keeps tree-sitter state for at most one document at a time.
fn unload_other_documents(documents: &mut HashMap<Url, Document>, active_uri: &Url) {
    for (document_uri, document) in documents.iter_mut() {
        if document_uri != active_uri {
            document.unload_parse_state();
        }
    }
}

fn new_parser() -> tree_sitter::Parser {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_ifc::LANGUAGE.into())
        .expect("Error loading IFC parser");
    parser
}

fn completion_snippet_support(params: &InitializeParams) -> bool {
    params
        .capabilities
        .text_document
        .as_ref()
        .and_then(|capabilities| capabilities.completion.as_ref())
        .and_then(|capabilities| capabilities.completion_item.as_ref())
        .and_then(|capabilities| capabilities.snippet_support)
        .unwrap_or(false)
}

fn completion_documentation_resolve_support(params: &InitializeParams) -> bool {
    params
        .capabilities
        .text_document
        .as_ref()
        .and_then(|capabilities| capabilities.completion.as_ref())
        .and_then(|capabilities| capabilities.completion_item.as_ref())
        .and_then(|capabilities| capabilities.resolve_support.as_ref())
        .is_some_and(|support| {
            support
                .properties
                .iter()
                .any(|property| property == "documentation")
        })
}

fn completion_trigger_characters() -> Vec<String> {
    ["!", "(", ",", "#", "."]
        .into_iter()
        .map(str::to_string)
        .collect()
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    #[instrument(skip(self, params))]
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let pending_init_config = params
            .initialization_options
            .as_ref()
            .map(parse_server_config)
            .unwrap_or_default();
        let completion_snippet_support = completion_snippet_support(&params);
        let completion_documentation_resolve_support =
            completion_documentation_resolve_support(&params);
        let semantic_tokens_enabled = pending_init_config.semantic_tokens_enabled;
        let semantic_tokens_provider = semantic_tokens_enabled.then(|| {
            SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                work_done_progress_options: WorkDoneProgressOptions::default(),
                legend: semantic_tokens::legend(),
                range: Some(true),
                full: None,
            })
        });

        let mut config = self.config.write().await;
        config.semantic_tokens_enabled = semantic_tokens_enabled;
        config.completion_snippet_support = completion_snippet_support;
        config.completion_documentation_resolve_support = completion_documentation_resolve_support;
        config.pending_init_config = Some(pending_init_config);
        info!("received initialize request");

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                definition_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: Some(vec![",".to_string()]),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                inlay_hint_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(true),
                    trigger_characters: Some(completion_trigger_characters()),
                    ..CompletionOptions::default()
                }),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            CodeActionKind::SOURCE,
                            CodeActionKind::new(related_entities::NAVIGATION_KIND),
                        ]),
                        ..CodeActionOptions::default()
                    },
                )),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![related_entities::GO_TO_RELATED_ENTITY_COMMAND.to_string()],
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                semantic_tokens_provider,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        info!("IFC LSP server initialized");

        let pending_init_config = {
            let mut config = self.config.write().await;
            config.pending_init_config.take()
        };

        if let Some(config) = pending_init_config {
            self.apply_config(config).await;
        }
    }

    async fn shutdown(&self) -> Result<()> {
        info!("received shutdown request");
        Ok(())
    }

    #[instrument(skip(self, params), fields(uri = %params.text_document.uri))]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;

        info!(text_len = text.len(), "document opened");

        let diagnostic_ticket = self.diagnostic_scheduler.begin(uri.clone()).await;
        let snapshot = self.load_text_as_active_document(&uri, text).await;
        self.schedule_diagnostics(diagnostic_ticket, snapshot).await;
    }

    #[instrument(skip(self, params), fields(uri = %params.text_document.uri))]
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let changes = params.content_changes;
        if !changes.is_empty() {
            debug!(change_count = changes.len(), "document changed");
            let diagnostic_ticket = self.diagnostic_scheduler.begin(uri.clone()).await;
            if let Some(snapshot) = self.apply_changes_to_active_document(&uri, changes).await {
                self.schedule_diagnostics(diagnostic_ticket, snapshot).await;
            } else {
                warn!("received document change for an unopened document");
            }
        }
    }

    #[instrument(skip(self, params), fields(uri = %params.text_document.uri))]
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;

        let mut documents = self.documents.write().await;
        documents.remove(&uri);
        drop(documents);

        self.ast_skip_warning_shown.write().await.remove(&uri);
        self.unknown_schema_warned.write().await.remove(&uri);
        self.diagnostic_scheduler.invalidate(&uri).await;
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
        info!("document closed");
    }

    #[instrument(skip(self, params), fields(uri = %params.text_document_position.text_document.uri))]
    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let (snippet_supported, resolve_documentation_supported) = {
            let config = self.config.read().await;
            (
                config.completion_snippet_support,
                config.completion_documentation_resolve_support,
            )
        };

        let documents = self.documents.read().await;
        let Some(document) = documents.get(&uri) else {
            return Ok(None);
        };
        let resolved_schema = self.resolve_schema(document.schema_name.as_deref()).await;
        let selected_schema_name = resolved_schema.as_ref().map(|(name, _)| name.clone());
        let schema = resolved_schema.as_ref().map(|(_, schema)| schema.as_ref());

        let mut items = Vec::new();
        let mut is_incomplete = false;
        append_completion_items(
            &mut items,
            &mut is_incomplete,
            scaffold_completions::completions(document, &uri, position, snippet_supported),
        );
        if let Some(schema_results) = completions::completions(
            document,
            &uri,
            position,
            selected_schema_name.as_deref(),
            schema,
            snippet_supported,
            !resolve_documentation_supported,
        ) {
            items.extend(schema_results.items);
            is_incomplete |= schema_results.is_incomplete;
        }
        let result = (!items.is_empty()).then_some(CompletionResponse::List(CompletionList {
            is_incomplete,
            items,
        }));
        debug!(
            has_result = result.is_some(),
            "completion request completed"
        );

        Ok(result)
    }

    #[instrument(skip(self, item))]
    async fn completion_resolve(&self, item: CompletionItem) -> Result<CompletionItem> {
        let uri = completions::resolution_uri(&item);
        let schema_name = completions::resolution_schema_name(&item).map(str::to_string);
        let documents = self.documents.read().await;
        let document = uri.as_ref().and_then(|uri| documents.get(uri));
        let schema_docs = self.schema_docs.read().await;
        let schema = schema_name
            .as_deref()
            .and_then(|schema_name| schema_docs.get(schema_name));
        Ok(completions::resolve_completion_item(item, document, schema))
    }

    #[instrument(skip(self, params), fields(uri = %params.text_document.uri))]
    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let position = params.range.start;
        let requested_kinds = params.context.only.as_deref();
        let forced_schema_name = self.config.read().await.forced_schema_name.clone();

        let documents = self.documents.read().await;
        let Some(document) = documents.get(&uri) else {
            return Ok(None);
        };
        let schema_docs = self.schema_docs.read().await;
        let schema_name = forced_schema_name
            .as_deref()
            .or(document.schema_name.as_deref());
        let schema = schema_name.and_then(|schema_name| schema_docs.get(schema_name));

        let mut actions =
            scaffold_completions::code_actions(document, &uri, requested_kinds).unwrap_or_default();
        if let Some(related) =
            related_entities::code_actions(document, &uri, position, requested_kinds, schema)
        {
            actions.extend(related);
        }
        let result = (!actions.is_empty()).then_some(actions);

        debug!(
            result_count = result.as_ref().map_or(0, Vec::len),
            "code action request completed"
        );

        Ok(result)
    }

    #[instrument(skip(self, params), fields(command = %params.command))]
    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        if params.command != related_entities::GO_TO_RELATED_ENTITY_COMMAND {
            return Ok(None);
        }

        let Some((uri, range)) = related_entities::parse_go_to_target(&params.arguments) else {
            warn!("received goToRelatedEntity command with unparseable arguments");
            return Ok(None);
        };

        if let Err(error) = self
            .client
            .show_document(ShowDocumentParams {
                uri,
                external: Some(false),
                take_focus: Some(true),
                selection: Some(range),
            })
            .await
        {
            warn!(error = ?error, "failed to show related entity document");
        }

        Ok(None)
    }

    #[instrument(skip(self, params), fields(uri = %params.text_document_position_params.text_document.uri))]
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let forced_schema_name = self.config.read().await.forced_schema_name.clone();

        let mut documents = self.documents.write().await;
        let diagnostic_snapshot = match self.ensure_document_loaded(&mut documents, &uri).await {
            Some(snapshot) => snapshot,
            None => return Ok(None),
        };
        let document = match documents.get(&uri) {
            Some(document) => document,
            None => return Ok(None),
        };

        if let Some(node) = document.node_at_position(position) {
            debug!(
                node_kind = node.kind(),
                node_text = node.utf8_text(document.text.as_bytes()).unwrap_or("?"),
                "resolved syntax node at hover position"
            );
        }
        let schema_docs = self.schema_docs.read().await;

        let result = hover::hover(
            document,
            position,
            &schema_docs,
            forced_schema_name.as_deref(),
        );
        drop(schema_docs);
        drop(documents);

        self.request_time_diagnostics(diagnostic_snapshot, &uri)
            .await;
        debug!(has_result = result.is_some(), "hover request completed");

        Ok(result)
    }

    #[instrument(skip(self, params), fields(uri = %params.text_document_position_params.text_document.uri))]
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

        let result = definition::goto_definition(&uri, document, position);
        drop(documents);
        if result.is_some() {
            debug!("go to definition resolved target");
        } else {
            debug!("go to definition found no target");
        }

        Ok(result)
    }

    #[instrument(skip(self, params), fields(uri = %params.text_document.uri))]
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let forced_schema_name = self.config.read().await.forced_schema_name.clone();

        let mut documents = self.documents.write().await;
        let diagnostic_snapshot = match self.ensure_document_loaded(&mut documents, &uri).await {
            Some(snapshot) => snapshot,
            None => return Ok(None),
        };
        let document = match documents.get(&uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        let schema_name = forced_schema_name.or_else(|| {
            document
                .schema_name
                .as_deref()
                .map(crate::schema::normalize_name)
        });
        let text = Arc::clone(&document.text);
        let tree = document.tree.clone();
        let ast_skipped = document.ast_skipped;
        drop(documents);

        let schema = if let Some(schema_name) = schema_name.as_deref() {
            self.schema_docs.read().await.get_shared(schema_name)
        } else {
            None
        };
        let Ok(permit) = self
            .diagnostic_scheduler
            .semantic_work()
            .acquire_owned()
            .await
        else {
            warn!("semantic work limiter closed");
            return Ok(None);
        };
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let instances = crate::document::build_entity_instances(tree.as_ref(), &text);
            document_symbols::document_symbols(&text, &instances, ast_skipped, schema.as_deref())
        })
        .await
        .unwrap_or_else(|error| {
            warn!(%error, "document symbols task failed");
            None
        });

        self.request_time_diagnostics(diagnostic_snapshot, &uri)
            .await;
        debug!(
            has_result = result.is_some(),
            "document symbols request completed"
        );

        Ok(result)
    }

    #[instrument(skip(self, params), fields(uri = %params.text_document_position.text_document.uri))]
    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let documents = self.documents.read().await;
        let document = match documents.get(&uri) {
            Some(document) => document,
            None => return Ok(None),
        };

        let result = references::find_references(&uri, document, position);
        drop(documents);
        debug!(
            result_count = result.as_ref().map_or(0, Vec::len),
            "find references request completed"
        );

        Ok(result)
    }

    #[instrument(skip(self, params), fields(uri = %params.text_document_position_params.text_document.uri))]
    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let documents = self.documents.read().await;
        let Some(document) = documents.get(&uri) else {
            return Ok(None);
        };

        let result = document_highlight::document_highlight(document, position);
        debug!(
            result_count = result.as_ref().map_or(0, Vec::len),
            "document highlight request completed"
        );

        Ok(result)
    }

    #[instrument(skip(self, params), fields(uri = %params.text_document_position_params.text_document.uri))]
    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let forced_schema_name = self.config.read().await.forced_schema_name.clone();

        let documents = self.documents.read().await;
        let Some(document) = documents.get(&uri) else {
            return Ok(None);
        };
        let schema_docs = self.schema_docs.read().await;

        let result = signature_help::signature_help(
            document,
            position,
            &schema_docs,
            forced_schema_name.as_deref(),
        );
        debug!(
            has_result = result.is_some(),
            "signature help request completed"
        );

        Ok(result)
    }

    #[instrument(skip(self, params), fields(uri = %params.text_document.uri))]
    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = params.text_document.uri;
        let forced_schema_name = self.config.read().await.forced_schema_name.clone();

        let documents = self.documents.read().await;
        let Some(document) = documents.get(&uri) else {
            return Ok(None);
        };
        let schema_docs = self.schema_docs.read().await;

        let result = inlay_hints::inlay_hints(
            document,
            params.range,
            &schema_docs,
            forced_schema_name.as_deref(),
        );
        debug!(
            result_count = result.as_ref().map_or(0, Vec::len),
            "inlay hint request completed"
        );

        Ok(result)
    }

    #[instrument(skip(self, params), fields(uri = %params.text_document.uri))]
    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        let uri = params.text_document.uri;
        if !self.config.read().await.semantic_tokens_enabled {
            return Ok(None);
        }

        let documents = self.documents.read().await;
        let Some(document) = documents.get(&uri) else {
            return Ok(None);
        };

        let result = semantic_tokens::semantic_tokens_range(document, params.range)
            .map(SemanticTokensRangeResult::Tokens);
        debug!(
            result_count = result.as_ref().map_or(0, |result| match result {
                SemanticTokensRangeResult::Tokens(tokens) => tokens.data.len(),
                SemanticTokensRangeResult::Partial(partial) => partial.data.len(),
            }),
            "semantic tokens range request completed"
        );

        Ok(result)
    }
}

fn append_completion_items(
    items: &mut Vec<CompletionItem>,
    is_incomplete: &mut bool,
    response: Option<CompletionResponse>,
) {
    match response {
        Some(CompletionResponse::Array(mut response_items)) => items.append(&mut response_items),
        Some(CompletionResponse::List(mut list)) => {
            *is_incomplete |= list.is_incomplete;
            items.append(&mut list.items);
        }
        None => {}
    }
}

/// Records the reason a document was last warned about and returns whether a new warning is
/// due: once per document, and again only when the reason changes (declared schema name, or
/// a bundled schema's load error appearing, clearing, or changing text).
fn should_warn_unknown_schema(
    warned: &mut HashMap<Url, Option<String>>,
    uri: &Url,
    schema_name: Option<&str>,
    schema_known: bool,
    load_error: Option<&str>,
) -> bool {
    if schema_known {
        warned.remove(uri);
        return false;
    }

    let current = match load_error {
        Some(error) => Some(format!("error:{}:{error}", schema_name.unwrap_or(""))),
        None => schema_name.map(str::to_owned),
    };
    if warned.get(uri) == Some(&current) {
        return false;
    }
    warned.insert(uri.clone(), current);
    true
}

#[cfg(test)]
mod tests {
    use tower_lsp::LspService;

    use super::*;
    use crate::schema::{IfcVersion, recording_official_schemas};

    fn warn_for(
        warned: &mut HashMap<Url, Option<String>>,
        uri: &Url,
        schema_name: Option<&str>,
    ) -> bool {
        should_warn_unknown_schema(warned, uri, schema_name, false, None)
    }

    #[test]
    fn unknown_schema_warning_is_sent_once_per_document() {
        let mut warned = HashMap::new();
        let uri = Url::parse("file:///tmp/a.ifc").unwrap();

        assert!(warn_for(&mut warned, &uri, Some("IFC4X3")));
        assert!(!warn_for(&mut warned, &uri, Some("IFC4X3")));
        assert!(!warn_for(&mut warned, &uri, Some("IFC4X3")));

        let other = Url::parse("file:///tmp/b.ifc").unwrap();
        assert!(warn_for(&mut warned, &other, Some("IFC4X3")));
    }

    #[test]
    fn unknown_schema_warning_repeats_when_the_schema_name_changes() {
        let mut warned = HashMap::new();
        let uri = Url::parse("file:///tmp/a.ifc").unwrap();

        assert!(warn_for(&mut warned, &uri, Some("IFC4X3")));
        assert!(warn_for(&mut warned, &uri, Some("IFC9")));
        assert!(warn_for(&mut warned, &uri, None));
        assert!(!warn_for(&mut warned, &uri, None));
    }

    #[test]
    fn known_schema_resets_the_unknown_schema_warning() {
        let mut warned = HashMap::new();
        let uri = Url::parse("file:///tmp/a.ifc").unwrap();

        assert!(warn_for(&mut warned, &uri, Some("IFC4X3")));
        assert!(!should_warn_unknown_schema(
            &mut warned,
            &uri,
            Some("IFC4"),
            true,
            None
        ));
        assert!(warn_for(&mut warned, &uri, Some("IFC4X3")));
    }

    #[test]
    fn load_error_warning_is_sent_once_and_repeats_if_the_error_text_changes() {
        let mut warned = HashMap::new();
        let uri = Url::parse("file:///tmp/a.ifc").unwrap();

        assert!(should_warn_unknown_schema(
            &mut warned,
            &uri,
            Some("IFC4"),
            false,
            Some("broken")
        ));
        assert!(!should_warn_unknown_schema(
            &mut warned,
            &uri,
            Some("IFC4"),
            false,
            Some("broken")
        ));
        assert!(should_warn_unknown_schema(
            &mut warned,
            &uri,
            Some("IFC4"),
            false,
            Some("still broken")
        ));
    }

    #[test]
    fn warning_repeats_when_switching_between_generic_and_load_error_reasons() {
        let mut warned = HashMap::new();
        let uri = Url::parse("file:///tmp/a.ifc").unwrap();

        assert!(warn_for(&mut warned, &uri, Some("IFC4")));
        assert!(should_warn_unknown_schema(
            &mut warned,
            &uri,
            Some("IFC4"),
            false,
            Some("broken")
        ));
        assert!(!should_warn_unknown_schema(
            &mut warned,
            &uri,
            Some("IFC4"),
            false,
            Some("broken")
        ));
    }

    #[tokio::test]
    async fn resolve_schema_returns_name_and_schema_from_one_lock_acquisition() {
        let (service, _socket) = LspService::new(Backend::new);
        let backend = service.inner();
        *backend.schema_docs.write().await = SchemaDocCollection::from_docs([(
            "IFC4".to_string(),
            SchemaDoc {
                entities: HashMap::new(),
                types: HashMap::new(),
            },
        )]);

        let (name, schema) = backend
            .resolve_schema(Some("IFC4"))
            .await
            .expect("schema resolves");
        assert_eq!(name, normalize_name("IFC4"));
        let expected = backend.schema_docs.read().await.get_shared(&name).unwrap();
        assert!(Arc::ptr_eq(&schema, &expected));
    }

    #[tokio::test]
    async fn resolve_schema_returns_none_for_unknown_schema_name() {
        let (service, _socket) = LspService::new(Backend::new);
        let backend = service.inner();
        *backend.schema_docs.write().await = SchemaDocCollection::empty();

        assert!(backend.resolve_schema(Some("Unknown")).await.is_none());
    }

    #[tokio::test]
    async fn resolve_schema_honors_forced_schema_name() {
        let (service, _socket) = LspService::new(Backend::new);
        let backend = service.inner();
        *backend.schema_docs.write().await = SchemaDocCollection::from_docs([(
            normalize_name("Forced"),
            SchemaDoc {
                entities: HashMap::new(),
                types: HashMap::new(),
            },
        )]);
        backend.config.write().await.forced_schema_name = Some(normalize_name("Forced"));

        let (name, _) = backend
            .resolve_schema(Some("SomethingElse"))
            .await
            .expect("forced schema resolves");
        assert_eq!(name, normalize_name("Forced"));
    }

    fn empty_schema() -> SchemaDoc {
        SchemaDoc {
            entities: HashMap::new(),
            types: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn applying_config_twice_keeps_official_schemas_parsed_once() {
        let (service, _socket) = LspService::new(Backend::new);
        let backend = service.inner();
        let (official, parsed) = recording_official_schemas(|_| Ok(empty_schema()));
        *backend.schema_docs.write().await = SchemaDocCollection::with_official(official);

        let (_, before_reload) = backend.resolve_schema(Some("IFC4")).await.unwrap();
        backend.apply_config(ServerConfig::default()).await;
        backend.apply_config(ServerConfig::default()).await;
        let (_, after_reload) = backend.resolve_schema(Some("IFC4")).await.unwrap();

        assert!(Arc::ptr_eq(&before_reload, &after_reload));
        assert_eq!(*parsed.lock().unwrap(), [IfcVersion::Ifc4Add2Tc1]);
    }

    #[tokio::test]
    async fn forced_local_schema_replaces_official_schema_of_same_name() {
        let path = std::env::temp_dir().join(format!("ifc-ls-{}.exp", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            "SCHEMA IFC4;\nENTITY IfcLocalOnly;\nEND_ENTITY;\nEND_SCHEMA;\n",
        )
        .unwrap();
        let (service, _socket) = LspService::new(Backend::new);
        let backend = service.inner();
        let (official, parsed) = recording_official_schemas(|_| Ok(empty_schema()));
        *backend.schema_docs.write().await = SchemaDocCollection::with_official(official);

        backend
            .apply_config(ServerConfig {
                overwrite_exp_schema_with_local: Some(path.clone()),
                ..ServerConfig::default()
            })
            .await;
        let resolved = backend.resolve_schema(Some("IFC4")).await;
        std::fs::remove_file(&path).unwrap();

        let (name, schema) = resolved.expect("local schema resolves");
        assert_eq!(name, "IFC4");
        assert!(schema.entity("IfcLocalOnly").is_some());
        assert!(parsed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_official_schema_is_not_retried_after_config_reload() {
        let (service, _socket) = LspService::new(Backend::new);
        let backend = service.inner();
        let (official, parsed) = recording_official_schemas(|_| Err("broken".to_string()));
        *backend.schema_docs.write().await = SchemaDocCollection::with_official(official);

        assert!(backend.resolve_schema(Some("IFC4")).await.is_none());
        backend.apply_config(ServerConfig::default()).await;
        assert!(backend.resolve_schema(Some("IFC4")).await.is_none());

        let schema_docs = backend.schema_docs.read().await;
        assert_eq!(schema_docs.official_schema_error("IFC4"), Some("broken"));
        assert_eq!(*parsed.lock().unwrap(), [IfcVersion::Ifc4Add2Tc1]);
    }

    #[test]
    fn applying_server_config_resets_reloadable_fields_and_preserves_negotiated_capabilities() {
        let current_state = ConfigState {
            forced_schema_name: Some("stale".to_string()),
            additional_schema_paths: HashMap::from([(
                "stale".to_string(),
                std::path::PathBuf::from("/stale"),
            )]),
            pending_init_config: Some(ServerConfig::default()),
            ast_file_size_limit_bytes: 1,
            semantic_tokens_enabled: false,
            completion_snippet_support: true,
            completion_documentation_resolve_support: true,
        };
        let new_config = ServerConfig {
            ast_file_size_limit_bytes: 999,
            semantic_tokens_enabled: true,
            ..ServerConfig::default()
        };

        let next_state = config_state_with_client_capabilities(&new_config, &current_state);

        assert_eq!(next_state.forced_schema_name, None);
        assert!(next_state.additional_schema_paths.is_empty());
        assert!(next_state.pending_init_config.is_none());
        assert_eq!(next_state.ast_file_size_limit_bytes, 999);
        assert!(next_state.semantic_tokens_enabled);
        assert!(next_state.completion_snippet_support);
        assert!(next_state.completion_documentation_resolve_support);
    }

    #[test]
    fn dot_triggers_enum_completion() {
        assert!(
            completion_trigger_characters()
                .iter()
                .any(|trigger| trigger == ".")
        );
    }
}
