//! STEP-specific support code shared across language-server features.

#[cfg(test)]
pub mod ast;
pub mod compact;

pub(crate) fn parse_recovery(text: &str, range: tree_sitter::Range) -> Option<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_ifc::LANGUAGE_STRUCTURED.into())
        .expect("IFC ABI");
    parser
        .set_included_ranges(&[range])
        .expect("statement range");
    parser.parse(text, None)
}
