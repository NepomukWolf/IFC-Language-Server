//! Library root of the IFC language server.
//! It declares the runtime modules and exports the `tower-lsp` backend that the binary serves;
//! on wasm32 it also exports `serve` for running the server in a browser.

mod backend;
mod config;
mod diagnostics;
mod document;
mod document_index;
mod features;
mod rt;
mod schema;
pub mod step;
#[cfg(test)]
mod test_support;

pub use backend::Backend;
#[cfg(target_arch = "wasm32")]
mod wasm;
