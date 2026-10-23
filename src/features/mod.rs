//! Feature-specific request handlers.
//! Each module operates on an already-parsed `Document` and returns plain LSP types.
//! This keeps protocol wiring in `backend.rs` and syntax/schema behavior close to each feature.

pub mod call_hierarchy;
pub mod completions;
pub mod definition;
pub mod document_highlight;
pub mod document_symbols;
pub mod hover;
mod ifc_guid;
pub mod inlay_hints;
pub mod instance_queries;
pub mod instances;
pub mod references;
pub mod related_entities;
pub mod scaffold_completions;
pub mod semantic_tokens;
pub mod signature_help;
mod step_scan;
