//! Library root of the IFC language server.
//! It declares the runtime modules and exports the `tower-lsp` backend that the binary serves.

mod backend;
mod config;
mod diagnostics;
mod document;
mod document_index;
pub mod document_tree;
mod features;
pub mod handwritten;
mod rt;
mod schema;
mod step;

pub use backend::Backend;
