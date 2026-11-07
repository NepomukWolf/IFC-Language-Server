//! Binary entrypoint for the IFC language server.
//! It binds stdin/stdout to `tower-lsp` and starts the single backend instance that serves all
//! editor requests for the process lifetime.
//! On wasm32 the library's `serve` is the entrypoint and this binary is empty.

#![cfg_attr(target_arch = "wasm32", no_main)]
#![cfg(not(target_arch = "wasm32"))]

use ifc_language_server::Backend;
use tower_lsp::Server;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    init_logging();
    info!("starting IFC language server");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = Backend::service();

    Server::new(stdin, stdout, socket).serve(service).await;

    info!("IFC language server stopped");
}

fn init_logging() {
    let env_filter = EnvFilter::try_from_env("IFC_LSP_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("ifc_language_server=info,tower_lsp=warn"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}
