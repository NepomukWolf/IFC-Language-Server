//! Find-references feature entry point.
//! This module returns same-document usages of IFC instance ids and includes the local definition
//! when the cursor is on either a definition or a reference.
//! It does not attempt cross-file indexing or workspace-wide lookup.

use tower_lsp::lsp_types::{Location, Position, Url};

use crate::document::Document;

pub fn find_references(
    uri: &Url,
    document: &Document,
    position: Position,
) -> Option<Vec<Location>> {
    let id = document.instance_id_at_position(position)?;

    let mut locations = Vec::new();

    if let Some(offsets) = document.references.get(&id) {
        for offset in offsets {
            locations.push(Location {
                uri: uri.clone(),
                range: document.id_range_at_offset(*offset)?,
            });
        }
    }

    if locations.is_empty() {
        None
    } else {
        Some(locations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_references_when_cursor_is_on_the_definitions_entity_name() {
        let text = "#1=IFCWALL(#2);\n#2=IFCLOCALPLACEMENT($);\n#3=IFCDOOR(#2);";
        let document = Document::new_unloaded(text.to_string());
        let uri = Url::parse("file:///model.ifc").expect("uri should parse");

        let locations =
            find_references(&uri, &document, Position::new(1, 5)).expect("references should exist");

        let starts: Vec<_> = locations
            .iter()
            .map(|location| location.range.start)
            .collect();
        assert_eq!(
            starts,
            [
                Position::new(0, 11),
                Position::new(1, 0),
                Position::new(2, 11)
            ]
        );
    }
}
