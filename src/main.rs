//! Binary entrypoint for the IFC language server.
//! It assembles the runtime modules, binds stdin/stdout to `tower-lsp`, and starts the single
//! backend instance that serves all editor requests for the process lifetime.

mod backend;
mod config;
mod diagnostics;
mod document;
mod features;
mod schema;
mod step;

use backend::Backend;
use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::build(Backend::new)
        .custom_method("ifc/openFromDisk", Backend::open_from_disk)
        .custom_method("ifc/closeFromDisk", Backend::close_from_disk)
        .custom_method("ifc/diagnostics", Backend::diagnostics)
        .custom_method("ifc/visibleDiagnostics", Backend::visible_diagnostics)
        .finish();

    Server::new(stdin, stdout, socket).serve(service).await;
}
