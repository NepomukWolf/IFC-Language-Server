//! Document symbol feature entry point.
//! Groups IFC STEP entity instances by type for VS Code's Outline, Breadcrumbs,
//! and "Go to Symbol in File" navigation.

use std::collections::BTreeMap;

use crate::document::{Document, EntityInstanceInfo, ParameterValue};
use crate::schema::SchemaDoc;
use tower_lsp::lsp_types::{DocumentSymbol, DocumentSymbolResponse, Position, Range, SymbolKind};

pub fn document_symbols(
    document: &Document,
    schema: Option<&SchemaDoc>,
    max_symbols: usize,
) -> Option<DocumentSymbolResponse> {
    if document.instances.len() > max_symbols {
        return Some(DocumentSymbolResponse::Nested(vec![limit_symbol(
            document,
            max_symbols,
        )]));
    }

    let lines: Vec<&str> = document.text.lines().collect();
    let mut instances_by_type: BTreeMap<&str, Vec<&EntityInstanceInfo>> = BTreeMap::new();
    for instance in &document.instances {
        instances_by_type
            .entry(&instance.entity_name)
            .or_default()
            .push(instance);
    }

    let symbols = instances_by_type
        .into_iter()
        .map(|(entity_name, instances)| group_symbol(&lines, schema, entity_name, &instances))
        .collect();

    Some(DocumentSymbolResponse::Nested(symbols))
}

fn group_symbol(
    lines: &[&str],
    schema: Option<&SchemaDoc>,
    entity_name: &str,
    instances: &[&EntityInstanceInfo],
) -> DocumentSymbol {
    let first = instances
        .first()
        .expect("entity type groups should not be empty");
    let last = instances
        .last()
        .expect("entity type groups should not be empty");

    DocumentSymbol {
        name: format!("{} ×{}", entity_name, instances.len()),
        detail: Some(format!("{} instances", instances.len())),
        kind: symbol_kind(entity_name),
        tags: None,
        #[allow(deprecated)]
        deprecated: None,
        range: lsp_range(
            lines,
            Range::new(first.entity_range.start, last.entity_range.end),
        ),
        selection_range: lsp_range(lines, first.entity_name_range),
        children: Some(
            instances
                .iter()
                .map(|instance| instance_symbol(lines, schema, instance))
                .collect(),
        ),
    }
}

fn instance_symbol(
    lines: &[&str],
    schema: Option<&SchemaDoc>,
    instance: &EntityInstanceInfo,
) -> DocumentSymbol {
    DocumentSymbol {
        name: instance_symbol_name(lines, schema, instance),
        detail: Some(instance.entity_name.clone()),
        kind: symbol_kind(&instance.entity_name),
        tags: None,
        #[allow(deprecated)]
        deprecated: None,
        range: lsp_range(lines, instance.entity_range),
        selection_range: lsp_range(
            lines,
            instance.id_range.unwrap_or(instance.entity_name_range),
        ),
        children: None,
    }
}

fn instance_symbol_name(
    lines: &[&str],
    schema: Option<&SchemaDoc>,
    instance: &EntityInstanceInfo,
) -> String {
    let name = schema
        .filter(|schema| schema.is_entity_compatible(&instance.entity_name, "IFCROOT"))
        .and_then(|_| instance_name(lines, instance));

    match (instance.id, name) {
        (Some(id), Some(name)) if !name.is_empty() => format!("#{} {}", id, name),
        (Some(id), _) => format!("#{}", id),
        (None, Some(name)) if !name.is_empty() => name,
        _ => instance.entity_name.clone(),
    }
}

fn limit_symbol(document: &Document, max_symbols: usize) -> DocumentSymbol {
    let byte_range = document
        .instances
        .first()
        .zip(document.instances.last())
        .map(|(first, last)| Range::new(first.entity_range.start, last.entity_range.end))
        .unwrap_or_else(|| Range::new(Position::new(0, 0), Position::new(0, 0)));
    let range = lsp_range_slow(&document.text, byte_range);

    DocumentSymbol {
        name: format!(
            "Outline unavailable: {} entities exceed the {} symbol limit",
            document.instances.len(),
            max_symbols
        ),
        detail: Some("Increase ifc.outline.maxSymbols to enable".to_string()),
        kind: SymbolKind::FILE,
        tags: None,
        #[allow(deprecated)]
        deprecated: None,
        range,
        selection_range: Range::new(range.start, range.start),
        children: None,
    }
}

fn instance_name(lines: &[&str], instance: &EntityInstanceInfo) -> Option<String> {
    instance
        .parameters
        .get(2)
        .and_then(|value| parameter_string(lines, value))
}

fn parameter_string(lines: &[&str], value: &ParameterValue) -> Option<String> {
    match value {
        ParameterValue::String { range } => {
            text_for_range(lines, *range).map(|text| text.trim_matches('\'').replace("''", "'"))
        }
        _ => None,
    }
}

fn text_for_range(lines: &[&str], range: tower_lsp::lsp_types::Range) -> Option<String> {
    if range.start.line != range.end.line {
        return None;
    }

    let line = lines.get(range.start.line as usize)?;
    let start = range.start.character as usize;
    let end = range.end.character as usize;
    if start > end || end > line.len() {
        return None;
    }

    Some(line[start..end].to_string())
}

fn lsp_range(lines: &[&str], byte_range: Range) -> Range {
    Range::new(
        lsp_position(lines, byte_range.start),
        lsp_position(lines, byte_range.end),
    )
}

fn lsp_position(lines: &[&str], byte_position: Position) -> Position {
    let Some(line) = lines.get(byte_position.line as usize) else {
        return byte_position;
    };
    let byte_column = byte_position.character as usize;
    let Some(prefix) = line.get(..byte_column) else {
        return byte_position;
    };

    Position::new(
        byte_position.line,
        prefix.encode_utf16().count().try_into().unwrap_or(u32::MAX),
    )
}

fn lsp_range_slow(text: &str, byte_range: Range) -> Range {
    let position = |byte_position: Position| {
        let Some(line) = text.lines().nth(byte_position.line as usize) else {
            return byte_position;
        };
        let Some(prefix) = line.get(..byte_position.character as usize) else {
            return byte_position;
        };

        Position::new(
            byte_position.line,
            prefix.encode_utf16().count().try_into().unwrap_or(u32::MAX),
        )
    };

    Range::new(position(byte_range.start), position(byte_range.end))
}

fn symbol_kind(entity_name: &str) -> SymbolKind {
    match entity_name {
        "IFCPROJECT" | "IFCSITE" | "IFCBUILDING" | "IFCBUILDINGSTOREY" | "IFCSPACE" => {
            SymbolKind::NAMESPACE
        }
        name if name.starts_with("IFCREL") => SymbolKind::FIELD,
        name if name.contains("PROPERTY") || name.contains("QUANTITY") => SymbolKind::PROPERTY,
        name if name.contains("MATERIAL") => SymbolKind::CONSTANT,
        name if name.contains("TYPE") => SymbolKind::CLASS,
        _ => SymbolKind::OBJECT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::EntityDoc;
    use std::collections::{HashMap, HashSet};
    use tower_lsp::lsp_types::{DocumentSymbolResponse, Position};
    use tree_sitter::Parser;

    fn parse_document(text: &str) -> Document {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("IFC grammar should load");
        Document::parse(&mut parser, text.to_string())
    }

    fn test_schema() -> SchemaDoc {
        let entity = |name: &str, is_root: bool| EntityDoc {
            name: name.to_string(),
            attributes: Vec::new(),
            url: String::new(),
            all_supertypes: if is_root {
                HashSet::from(["IFCROOT".to_string()])
            } else {
                HashSet::new()
            },
        };

        SchemaDoc {
            entities: HashMap::from([
                ("IFCPROJECT".to_string(), entity("IFCPROJECT", true)),
                ("IFCWALL".to_string(), entity("IFCWALL", true)),
                (
                    "IFCORGANIZATION".to_string(),
                    entity("IFCORGANIZATION", false),
                ),
            ]),
            types: HashMap::new(),
        }
    }

    #[test]
    fn groups_entity_instances_by_type() {
        let document = parse_document(
            "ISO-10303-21;\n\
             DATA;\n\
             #1=IFCPROJECT('guid',$,'Project A',$,$,$,$,$,$);\n\
             #2=IFCWALL('guid',$,'Wall A',$,$,$,$,$,$);\n\
             #3=IFCWALL('guid',$,'Wall B',$,$,$,$,$,$);\n\
             ENDSEC;\n\
             END-ISO-10303-21;",
        );

        let schema = test_schema();
        let Some(DocumentSymbolResponse::Nested(symbols)) =
            document_symbols(&document, Some(&schema), 100)
        else {
            panic!("expected nested document symbols");
        };

        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "IFCPROJECT ×1");
        assert_eq!(symbols[0].kind, SymbolKind::NAMESPACE);
        assert_eq!(
            symbols[0].children.as_ref().unwrap()[0].name,
            "#1 Project A"
        );
        assert_eq!(symbols[1].name, "IFCWALL ×2");
        assert_eq!(symbols[1].kind, SymbolKind::OBJECT);
        assert_eq!(symbols[1].children.as_ref().unwrap()[0].name, "#2 Wall A");
        assert_eq!(symbols[1].children.as_ref().unwrap()[1].name, "#3 Wall B");
    }

    #[test]
    fn selects_instance_id_inside_full_entity_range() {
        let document = parse_document(
            "ISO-10303-21;\n\
             DATA;\n\
             #42=IFCWALL('guid',$,'Wall A',$,$,$,$,$,$);\n\
             ENDSEC;\n\
             END-ISO-10303-21;",
        );

        let Some(DocumentSymbolResponse::Nested(symbols)) = document_symbols(&document, None, 100)
        else {
            panic!("expected nested document symbols");
        };

        let symbol = &symbols[0].children.as_ref().unwrap()[0];
        assert_eq!(symbol.range.start, Position::new(2, 0));
        assert_eq!(symbol.selection_range.start, Position::new(2, 0));
        assert_eq!(symbol.selection_range.end, Position::new(2, 3));
    }

    #[test]
    fn returns_explanation_instead_of_an_oversized_symbol_tree() {
        let document = parse_document(
            "ISO-10303-21;\n\
             DATA;\n\
             #1=IFCWALL('guid',$,'Wall A',$,$,$,$,$,$);\n\
             #2=IFCWALL('guid',$,'Wall B',$,$,$,$,$,$);\n\
             ENDSEC;\n\
             END-ISO-10303-21;",
        );

        let Some(DocumentSymbolResponse::Nested(symbols)) = document_symbols(&document, None, 1)
        else {
            panic!("expected a limit explanation");
        };

        assert_eq!(symbols.len(), 1);
        assert!(
            symbols[0]
                .name
                .contains("2 entities exceed the 1 symbol limit")
        );
        assert_eq!(symbols[0].kind, SymbolKind::FILE);
        assert_eq!(symbols[0].children, None);
    }

    #[test]
    fn does_not_label_a_non_root_attribute_as_name() {
        let document = parse_document(
            "ISO-10303-21;\n\
             DATA;\n\
             #1=IFCORGANIZATION($,'Organization A','Description A',$,$);\n\
             ENDSEC;\n\
             END-ISO-10303-21;",
        );
        let schema = test_schema();

        let Some(DocumentSymbolResponse::Nested(symbols)) =
            document_symbols(&document, Some(&schema), 100)
        else {
            panic!("expected nested document symbols");
        };

        assert_eq!(symbols[0].children.as_ref().unwrap()[0].name, "#1");
    }

    #[test]
    fn converts_statement_end_columns_to_lsp_utf16() {
        let document = parse_document(
            "ISO-10303-21;\n\
             DATA;\n\
             #1=IFCWALL('guid',$,'Café 😀',$,$,$,$,$,$);\n\
             ENDSEC;\n\
             END-ISO-10303-21;",
        );

        let Some(DocumentSymbolResponse::Nested(symbols)) = document_symbols(&document, None, 100)
        else {
            panic!("expected nested document symbols");
        };
        let symbol = &symbols[0].children.as_ref().unwrap()[0];
        let statement = "#1=IFCWALL('guid',$,'Café 😀',$,$,$,$,$,$);";

        assert_eq!(
            symbol.range.end.character,
            statement.encode_utf16().count() as u32
        );
        assert_ne!(symbol.range.end.character, statement.len() as u32);
    }
}
