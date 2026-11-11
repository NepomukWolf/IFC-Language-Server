//! In-process LSP session over `Backend`, shared by the benches and the peak-memory example.

use std::path::Path;

use futures::StreamExt;
use ifc_language_server::Backend;
use serde_json::json;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tower::{Service, ServiceExt};
use tower_lsp::jsonrpc::Request;
use tower_lsp::lsp_types::*;
use tower_lsp::{LanguageServer, LspService};

pub struct Model {
    pub name: String,
    pub text: String,
}

impl Model {
    pub fn load(path: &Path) -> Self {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("cannot read model {}: {error}", path.display()));
        let name = path
            .file_name()
            .expect("model path has a file name")
            .to_string_lossy()
            .into_owned();
        Self { name, text }
    }

    pub fn uri(&self) -> Url {
        Url::parse(&format!("file:///bench/{}", self.name)).expect("model name forms a valid URI")
    }

    pub fn position(&self, offset: usize) -> Position {
        let line_start = self.text[..offset].rfind('\n').map_or(0, |index| index + 1);
        let line = self.text[..line_start].matches('\n').count();
        let character = self.text[line_start..offset].encode_utf16().count();
        Position::new(line as u32, character as u32)
    }

    /// Offset of the first digit of the first `IFCCARTESIANPOINT((` past the middle of the file,
    /// or of the first one in the file.
    pub fn cartesian_point_digit(&self) -> usize {
        let needle = "IFCCARTESIANPOINT((";
        let middle = self.text.len() / 2;
        let mut points = self.text.match_indices(needle).map(|(index, _)| index);
        let first = points
            .next()
            .unwrap_or_else(|| panic!("{} has no IFCCARTESIANPOINT", self.name));
        let start = std::iter::once(first)
            .chain(points)
            .find(|&index| index >= middle)
            .unwrap_or(first)
            + needle.len();
        start
            + self.text[start..]
                .find(|c: char| c.is_ascii_digit())
                .expect("cartesian point has a digit")
    }
}

pub struct Session {
    runtime: Runtime,
    service: LspService<Backend>,
    published: mpsc::UnboundedReceiver<PublishDiagnosticsParams>,
    version: i32,
}

impl Session {
    pub fn new() -> Self {
        let runtime = Runtime::new().expect("tokio runtime starts");
        let (published_tx, published) = mpsc::unbounded_channel();
        let mut service = runtime.block_on(async {
            let (mut service, mut socket) = LspService::new(Backend::new);
            tokio::spawn(async move {
                while let Some(request) = socket.next().await {
                    if request.method() == "textDocument/publishDiagnostics" {
                        let params = serde_json::from_value(
                            request
                                .params()
                                .cloned()
                                .expect("publishDiagnostics has params"),
                        )
                        .expect("publishDiagnostics params deserialize");
                        if published_tx.send(params).is_err() {
                            break;
                        }
                    }
                }
            });
            let initialize = Request::build("initialize")
                .params(json!({ "capabilities": {} }))
                .id(1)
                .finish();
            call(&mut service, initialize).await;
            call(
                &mut service,
                Request::build("initialized").params(json!({})).finish(),
            )
            .await;
            service
        });
        runtime.block_on(service.ready()).expect("service is ready");
        Self {
            runtime,
            service,
            published,
            version: 0,
        }
    }

    pub fn backend(&self) -> &Backend {
        self.service.inner()
    }

    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    pub fn open(&mut self, uri: &Url, text: String) -> Vec<Diagnostic> {
        self.version += 1;
        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                uri.clone(),
                "ifc".to_string(),
                self.version,
                text,
            ),
        };
        self.runtime.block_on(self.service.inner().did_open(params));
        self.next_diagnostics(uri)
    }

    pub fn change(&mut self, uri: &Url, range: Range, text: &str) -> Vec<Diagnostic> {
        self.version += 1;
        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier::new(uri.clone(), self.version),
            content_changes: vec![TextDocumentContentChangeEvent {
                range: Some(range),
                range_length: None,
                text: text.to_string(),
            }],
        };
        self.runtime
            .block_on(self.service.inner().did_change(params));
        self.next_diagnostics(uri)
    }

    fn next_diagnostics(&mut self, uri: &Url) -> Vec<Diagnostic> {
        loop {
            let params = self
                .published
                .blocking_recv()
                .expect("diagnostics channel stays open");
            if &params.uri == uri {
                return params.diagnostics;
            }
        }
    }
}

async fn call(service: &mut LspService<Backend>, request: Request) {
    service
        .ready()
        .await
        .expect("service is ready")
        .call(request)
        .await
        .expect("request succeeds");
}
