//! Library root of the IFC language server.
//! It declares the runtime modules and exports the `tower-lsp` backend that the binary serves.

mod backend;
mod config;
mod diagnostics;
mod document;
mod document_index;
mod features;
pub mod handwritten;
mod rt;
mod schema;
pub use backend::Backend;
