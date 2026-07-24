//! Feature-specific request handlers.
//! Each module operates on an already-parsed `Document` and returns plain LSP types.
//! This keeps protocol wiring in `backend.rs` and syntax/schema behavior close to each feature.

pub mod definition;
pub mod document_highlight;
pub mod hover;
pub mod inlay_hints;
pub mod references;
pub mod semantic_tokens;
pub mod signature_help;
