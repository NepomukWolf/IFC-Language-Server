//! Go-to-definition feature entry point.
//! Resolves local `#id` references to their defining entity instances in the same document.
//! Cross-file lookup and schema-level symbol navigation are intentionally not handled here.

use crate::document::Document;
use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, Url};

pub fn goto_definition(
    uri: &Url,
    document: &Document,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    let (id, offset) = document.id_token_at_position(position)?;
    if document.definitions.get(&id) == Some(&offset) {
        return None;
    }

    Some(GotoDefinitionResponse::Scalar(Location {
        uri: uri.clone(),
        range: document.definition_range(id)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entity_name_of_a_duplicated_definition_has_no_definition_to_go_to() {
        let text = "#1=IFCCARTESIANPOINT((0.,0.));#1=IFCWALL(#1);";
        let document = Document::parse(text.to_string());
        let uri = Url::parse("file:///model.ifc").unwrap();
        let at = |needle: &str| Position::new(0, text.find(needle).unwrap() as u32);

        assert!(goto_definition(&uri, &document, at("IFCCARTESIANPOINT")).is_none());
        assert!(goto_definition(&uri, &document, at("#1)")).is_some());
    }
}
