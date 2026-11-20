//! Find-references feature entry point.
//! This module returns same-document usages of IFC instance ids when the cursor is on either a
//! definition or a reference, and the local definition too when the client asks for it.
//! It does not attempt cross-file indexing or workspace-wide lookup.

use tower_lsp::lsp_types::{Location, Position, Url};

use crate::document::Document;

pub fn find_references(
    uri: &Url,
    document: &Document,
    position: Position,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let id = document.instance_id_at_position(position)?;
    let declaration = document.definitions.get(&id);

    let mut locations = Vec::new();

    if let Some(offsets) = document.references.get(&id) {
        for offset in offsets {
            if !include_declaration && Some(offset) == declaration {
                continue;
            }
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

    const TEXT: &str = "#1=IFCWALL(#2);\n#2=IFCLOCALPLACEMENT($);\n#3=IFCDOOR(#2);";

    fn reference_starts(position: Position, include_declaration: bool) -> Option<Vec<Position>> {
        let document = Document::new_unloaded(TEXT.to_string());
        let uri = Url::parse("file:///model.ifc").expect("uri should parse");

        let locations = find_references(&uri, &document, position, include_declaration)?;
        Some(
            locations
                .iter()
                .map(|location| location.range.start)
                .collect(),
        )
    }

    #[test]
    fn finds_references_when_cursor_is_on_the_definitions_entity_name() {
        assert_eq!(
            reference_starts(Position::new(1, 5), true).expect("references should exist"),
            [
                Position::new(0, 11),
                Position::new(1, 0),
                Position::new(2, 11)
            ]
        );
    }

    #[test]
    fn leaves_out_the_definition_unless_the_client_asks_for_it() {
        let usages = [Position::new(0, 11), Position::new(2, 11)];

        // From the definition's entity name and from a usage of `#2`.
        for position in [Position::new(1, 5), Position::new(2, 12)] {
            assert_eq!(
                reference_starts(position, false).expect("usages should exist"),
                usages
            );
        }
    }

    #[test]
    fn an_unreferenced_instance_has_only_its_definition() {
        assert_eq!(
            reference_starts(Position::new(2, 1), true),
            Some(vec![Position::new(2, 0)])
        );
        assert_eq!(reference_starts(Position::new(2, 1), false), None);
    }
}
