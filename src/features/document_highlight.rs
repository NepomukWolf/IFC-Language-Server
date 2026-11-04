//! Document-highlight feature entry point.
//! Returns all same-document occurrences of the IFC instance id under the cursor using the
//! lightweight text index only.

use tower_lsp::lsp_types::{DocumentHighlight, DocumentHighlightKind, Position};

use crate::document::Document;

pub fn document_highlight(
    document: &Document,
    position: Position,
) -> Option<Vec<DocumentHighlight>> {
    let (id, _) = document.id_token_at_position(position)?;
    let offsets = document.references.get(&id)?;

    let mut highlights = Vec::new();
    for offset in offsets {
        highlights.push(DocumentHighlight {
            range: document.id_range_at_offset(*offset)?,
            kind: Some(DocumentHighlightKind::TEXT),
        });
    }

    if highlights.is_empty() {
        None
    } else {
        Some(highlights)
    }
}

#[cfg(test)]
mod tests {
    use tower_lsp::lsp_types::Position;

    use super::*;

    fn position_at(document: &Document, text: &str, needle: &str) -> Position {
        let offset = text.find(needle).expect("needle should exist");
        document
            .offset_to_position(offset)
            .expect("offset should convert to an LSP position")
    }

    fn position_at_last(document: &Document, text: &str, needle: &str) -> Position {
        let offset = text.rfind(needle).expect("needle should exist");
        document
            .offset_to_position(offset)
            .expect("offset should convert to an LSP position")
    }

    #[test]
    fn highlights_all_occurrences_when_cursor_is_on_reference() {
        let text = "#1=IFCWALL(#2);\n#2=IFCLOCALPLACEMENT($);\n#3=IFCDOOR(#2);";
        let document = Document::new_unloaded(text.to_string());

        let highlights = document_highlight(&document, position_at_last(&document, text, "#2"))
            .expect("highlights should exist");

        assert_eq!(highlights.len(), 3);
        assert_eq!(highlights[0].range.start, Position::new(0, 11));
        assert_eq!(highlights[1].range.start, Position::new(1, 0));
        assert_eq!(highlights[2].range.start, Position::new(2, 11));
        assert!(
            highlights
                .iter()
                .all(|highlight| highlight.kind == Some(DocumentHighlightKind::TEXT))
        );
    }

    #[test]
    fn highlights_all_occurrences_when_cursor_is_on_definition() {
        let text = "#1=IFCWALL(#2);\n#2=IFCLOCALPLACEMENT($);\n#3=IFCDOOR(#2);";
        let document = Document::new_unloaded(text.to_string());

        let highlights = document_highlight(&document, position_at(&document, text, "#2"))
            .expect("highlights should exist");

        assert_eq!(highlights.len(), 3);
    }

    #[test]
    fn highlights_all_occurrences_when_cursor_is_on_the_definitions_entity_name() {
        let text = "#1=IFCWALL(#2);\n#2=IFCLOCALPLACEMENT($);\n#3=IFCDOOR(#2);";
        let document = Document::new_unloaded(text.to_string());

        let highlights = document_highlight(&document, position_at(&document, text, "IFCWALL"))
            .expect("highlights should exist");

        assert_eq!(highlights.len(), 1);
        assert_eq!(highlights[0].range.start, Position::new(0, 0));
    }

    #[test]
    fn returns_none_when_cursor_is_outside_id_token() {
        let text = "#1=IFCLOCALPLACEMENT($);";
        let document = Document::new_unloaded(text.to_string());

        let highlights = document_highlight(&document, position_at(&document, text, "$"));

        assert!(highlights.is_none());
    }

    #[test]
    fn only_highlights_selected_id() {
        let text = "#1=IFCWALL(#2,#3);\n#2=IFCLOCALPLACEMENT($);\n#3=IFCDOOR(#2);";
        let document = Document::new_unloaded(text.to_string());

        let highlights = document_highlight(&document, position_at(&document, text, "#3"))
            .expect("highlights should exist");

        assert_eq!(highlights.len(), 2);
        assert_eq!(highlights[0].range.start, Position::new(0, 14));
        assert_eq!(highlights[1].range.start, Position::new(2, 0));
    }

    #[test]
    fn maps_positions_after_non_ascii_text() {
        let text = "/* Wänd */ #1=IFCWALL(#2);\n#2=IFCLOCALPLACEMENT($);";
        let document = Document::new_unloaded(text.to_string());

        let highlights = document_highlight(&document, position_at(&document, text, "#2"))
            .expect("highlights should exist");

        assert_eq!(highlights.len(), 2);
        assert_eq!(highlights[0].range.start, Position::new(0, 22));
        assert_eq!(highlights[1].range.start, Position::new(1, 0));
    }
}
