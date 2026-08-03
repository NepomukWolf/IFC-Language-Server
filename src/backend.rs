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
    definition, document_highlight, document_symbols, hover, inlay_hints, references,
    semantic_tokens, signature_help,
};
use crate::schema::{
    SchemaDocCollection, inspect_local_schema_name, load_local_schema, normalize_name,
};

#[derive(Debug)]
struct ConfigState {
    forced_schema_name: Option<String>,
    additional_schema_paths: HashMap<String, std::path::PathBuf>,
    pending_init_config: Option<ServerConfig>,
    ast_file_size_limit_bytes: usize,
    semantic_tokens_enabled: bool,
}

impl Default for ConfigState {
    fn default() -> Self {
        Self {
            forced_schema_name: None,
            additional_schema_paths: HashMap::new(),
            pending_init_config: None,
            ast_file_size_limit_bytes: DEFAULT_AST_FILE_SIZE_LIMIT_BYTES,
            semantic_tokens_enabled: true,
        }
    }
}

pub struct Backend {
    client: Client,
    documents: Arc<RwLock<HashMap<Url, Document>>>,
    schema_docs: Arc<RwLock<SchemaDocCollection>>,
    config: Arc<RwLock<ConfigState>>,
    ast_skip_warning_shown: Arc<RwLock<HashSet<Url>>>,
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

    #[instrument(skip(self), fields(schema_name = schema_name.unwrap_or("<none>")))]
    async fn check_schema_support(&self, schema_name: Option<&str>) {
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

        if self.selected_schema_name(schema_name).await.is_none() {
            warn!("document schema is unknown or unsupported");
            self.client
                .show_message(
                    MessageType::WARNING,
                    "This IFC file uses an unknown or unsupported schema version. Schema-aware diagnostics and hover information may be incomplete.",
                )
                .await;
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
        for (document_uri, document) in documents.iter_mut() {
            if document_uri != uri {
                document.unload_parse_state();
            }
        }

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

        self.check_schema_support(snapshot.schema_name()).await;
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
        for (document_uri, document) in documents.iter_mut() {
            if document_uri != uri {
                document.unload_parse_state();
            }
        }

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
        self.check_schema_support(snapshot.schema_name()).await;
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

        for (document_uri, document) in documents.iter_mut() {
            if document_uri != uri {
                document.unload_parse_state();
            }
        }

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
        let selected_schema_name = self.selected_schema_name(snapshot.schema_name()).await;
        let schema = if let Some(schema_name) = selected_schema_name {
            self.schema_docs
                .read()
                .await
                .get_shared(&schema_name)
                .map(|schema| (schema_name, schema))
        } else {
            None
        };

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

    #[instrument(skip(self), fields(schema_name = schema_name.unwrap_or("<none>")))]
    async fn selected_schema_name(&self, schema_name: Option<&str>) -> Option<String> {
        let (forced_schema_name, additional_path) = {
            let config = self.config.read().await;
            let forced_schema_name = config.forced_schema_name.clone();
            let additional_path = schema_name.and_then(|schema_name| {
                config
                    .additional_schema_paths
                    .get(&normalize_name(schema_name))
                    .cloned()
            });
            (forced_schema_name, additional_path)
        };

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

    #[instrument(skip(self, config), fields(ast_file_size_limit_bytes = config.ast_file_size_limit_bytes))]
    async fn apply_config(&self, config: ServerConfig) {
        let mut schema_docs = SchemaDocCollection::new();
        let mut next_state = ConfigState {
            ast_file_size_limit_bytes: config.ast_file_size_limit_bytes,
            semantic_tokens_enabled: config.semantic_tokens_enabled,
            ..ConfigState::default()
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

fn new_parser() -> tree_sitter::Parser {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_ifc::LANGUAGE.into())
        .expect("Error loading IFC parser");
    parser
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
                semantic_tokens_provider,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        info!("IFC LSP server initialized");

        let load_errors = self.schema_docs.read().await.load_errors().to_vec();
        for error in load_errors {
            warn!(error, "failed to load bundled schema documentation");
        }

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
        self.diagnostic_scheduler.invalidate(&uri).await;
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
        info!("document closed");
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
