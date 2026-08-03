//! Code-lens feature entry point.
//! Returns reference counts for local STEP instance definitions using the lightweight text index.

use tower_lsp::lsp_types::{CodeLens, Command, Location, Url};

use crate::document::Document;

const SHOW_REFERENCES_COMMAND: &str = "editor.action.showReferences";

// `Command::arguments` supplies the JSON value type without adding a direct serde_json dependency.
macro_rules! json_scalar {
    ($value:expr) => {{
        Command::new(String::new(), String::new(), Some(vec![$value.into()]))
            .arguments
            .expect("JSON scalar holder should contain arguments")
            .pop()
            .expect("JSON scalar holder should contain one argument")
    }};
}

macro_rules! json_object {
    ($($key:literal => $value:expr),* $(,)?) => {{
        Command::new(
            String::new(),
            String::new(),
            Some(vec![[ $(($key.to_string(), $value)),* ].into_iter().collect()]),
        )
        .arguments
        .expect("JSON object holder should contain arguments")
        .pop()
        .expect("JSON object holder should contain one argument")
    }};
}

macro_rules! json_array {
    ($values:expr) => {{
        Command::new(
            String::new(),
            String::new(),
            Some(vec![$values.into_iter().collect()]),
        )
        .arguments
        .expect("JSON array holder should contain arguments")
        .pop()
        .expect("JSON array holder should contain one argument")
    }};
}

pub fn code_lenses(uri: &Url, document: &Document) -> Option<Vec<CodeLens>> {
    let mut definitions: Vec<_> = document.definitions.iter().collect();
    definitions.sort_unstable_by_key(|(_, offset)| **offset);

    let mut lenses = Vec::with_capacity(definitions.len());
    for (id, definition_offset) in definitions {
        let definition_range = document.definition_range(*id)?;
        let reference_locations: Vec<_> = document
            .references
            .get(id)
            .into_iter()
            .flatten()
            .filter(|offset| *offset != definition_offset)
            .filter_map(|offset| {
                Some(Location {
                    uri: uri.clone(),
                    range: document.id_range_at_offset(*offset)?,
                })
            })
            .collect();
        let reference_count = reference_locations.len();
        let title = match reference_count {
            1 => "1 reference".to_string(),
            count => format!("{count} references"),
        };

        lenses.push(CodeLens {
            range: definition_range,
            command: Some(Command {
                title,
                command: SHOW_REFERENCES_COMMAND.to_string(),
                arguments: Some(vec![
                    json_scalar!(uri.to_string()),
                    json_object! {
                        "line" => json_scalar!(definition_range.start.line),
                        "character" => json_scalar!(definition_range.start.character),
                    },
                    json_array!(
                        reference_locations
                            .into_iter()
                            .map(|location| json_object! {
                                "uri" => json_scalar!(location.uri.to_string()),
                                "range" => json_object! {
                                    "start" => json_object! {
                                        "line" => json_scalar!(location.range.start.line),
                                        "character" => json_scalar!(location.range.start.character),
                                    },
                                    "end" => json_object! {
                                        "line" => json_scalar!(location.range.end.line),
                                        "character" => json_scalar!(location.range.end.character),
                                    },
                                },
                            })
                    ),
                ]),
            }),
            data: None,
        });
    }

    (!lenses.is_empty()).then_some(lenses)
}

#[cfg(test)]
mod tests {
    use tower_lsp::lsp_types::Position;

    use super::*;

    fn test_uri() -> Url {
        Url::parse("file:///model.ifc").expect("test URI should be valid")
    }

    #[test]
    fn returns_source_ordered_reference_counts_for_definitions() {
        let document = Document::new_unloaded(
            "#20=IFCWALL(#10);\n#10=IFCLOCALPLACEMENT($);\n#30=IFCDOOR(#10);".to_string(),
        );

        let lenses = code_lenses(&test_uri(), &document).expect("code lenses should exist");

        assert_eq!(lenses.len(), 3);
        assert_eq!(lenses[0].range.start, Position::new(0, 0));
        assert_eq!(lenses[1].range.start, Position::new(1, 0));
        assert_eq!(lenses[2].range.start, Position::new(2, 0));
        assert_eq!(lenses[0].command.as_ref().unwrap().title, "0 references");
        assert_eq!(lenses[1].command.as_ref().unwrap().title, "2 references");
        assert_eq!(lenses[2].command.as_ref().unwrap().title, "0 references");
    }

    #[test]
    fn uses_singular_title_and_show_references_command() {
        let document = Document::new_unloaded("#1=IFCWALL($);\n#2=IFCDOOR(#1);".to_string());

        let lenses = code_lenses(&test_uri(), &document).expect("code lenses should exist");
        let command = lenses[0].command.as_ref().expect("lens should be resolved");

        assert_eq!(command.title, "1 reference");
        assert_eq!(command.command, SHOW_REFERENCES_COMMAND);
        let arguments = command
            .arguments
            .as_ref()
            .expect("command should have arguments");
        assert_eq!(arguments[0].as_str(), Some("file:///model.ifc"));
        assert_eq!(arguments[1]["line"].as_u64(), Some(0));
        assert_eq!(arguments[1]["character"].as_u64(), Some(0));
        assert_eq!(arguments[2].as_array().map(Vec::len), Some(1));
        assert_eq!(arguments[2][0]["range"]["start"]["line"].as_u64(), Some(1));
        assert_eq!(
            arguments[2][0]["range"]["start"]["character"].as_u64(),
            Some(11)
        );
    }

    #[test]
    fn remains_available_without_ast_state() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("IFC parser language should load");
        let mut document = Document::new_unloaded("#1=IFCWALL($);".to_string());
        document.reload_parse_state(&mut parser, 0);
        assert!(document.ast_skipped);
        assert!(!document.has_ast());

        let lenses = code_lenses(&test_uri(), &document).expect("code lenses should exist");

        assert_eq!(lenses.len(), 1);
        assert_eq!(lenses[0].command.as_ref().unwrap().title, "0 references");
    }

    #[test]
    fn returns_none_without_definitions() {
        let document = Document::new_unloaded("#1".to_string());

        assert!(code_lenses(&test_uri(), &document).is_none());
    }
}
