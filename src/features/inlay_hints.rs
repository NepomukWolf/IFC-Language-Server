//! Schema-backed inlay hints for positional STEP entity arguments.
//! A lightweight range scanner finds entity instances and argument starts without requiring
//! tree-sitter AST state.

use tower_lsp::lsp_types::{InlayHint, InlayHintKind, Range};

use crate::document::Document;
use crate::schema::SchemaDocCollection;

pub fn inlay_hints(
    document: &Document,
    range: Range,
    schema_docs: &SchemaDocCollection,
    selected_schema_name: Option<&str>,
) -> Option<Vec<InlayHint>> {
    let emit_start = document.position_to_offset(range.start)?;
    let emit_end = document.position_to_offset(range.end)?;
    if emit_end < emit_start {
        return None;
    }

    let schema_name = selected_schema_name.or(document.schema_name.as_deref());
    let Some(schema_name) = schema_name else {
        return Some(Vec::new());
    };
    let Some(schema) = schema_docs.get(schema_name) else {
        return Some(Vec::new());
    };

    let scan_start = document
        .line_offsets
        .get(range.start.line as usize)
        .copied()
        .unwrap_or(emit_start);
    let instances = scan_instances(document, scan_start, emit_end);
    let mut hints = Vec::new();

    for instance in instances {
        let Some(entity) = schema.entity(&instance.entity_name) else {
            continue;
        };

        for (argument_start, attribute) in
            instance.argument_starts.into_iter().zip(&entity.attributes)
        {
            if argument_start < emit_start || argument_start >= emit_end {
                continue;
            }

            hints.push(InlayHint {
                position: document.offset_to_position(argument_start)?,
                label: format!("{}:", attribute.name).into(),
                kind: Some(InlayHintKind::PARAMETER),
                text_edits: None,
                tooltip: None,
                padding_left: None,
                padding_right: Some(true),
                data: None,
            });
        }
    }

    Some(hints)
}

#[derive(Debug, PartialEq, Eq)]
struct ScannedInstance {
    entity_name: String,
    argument_starts: Vec<usize>,
}

fn scan_instances(document: &Document, scan_start: usize, scan_end: usize) -> Vec<ScannedInstance> {
    let bytes = document.text.as_bytes();
    let mut instances = Vec::new();
    let mut offset = scan_start;

    while offset < scan_end {
        if bytes[offset] == b'#'
            && let Some((id, _)) = scan_instance_id(bytes, offset, scan_end)
            && document.definitions.get(&id) == Some(&offset)
            && let Some((instance, end)) = scan_instance(&document.text, offset, scan_end)
        {
            instances.push(instance);
            offset = end;
        } else {
            offset += 1;
        }
    }

    instances
}

fn scan_instance(text: &str, start: usize, scan_end: usize) -> Option<(ScannedInstance, usize)> {
    let bytes = text.as_bytes();
    let (_, mut offset) = scan_instance_id(bytes, start, scan_end)?;
    offset = skip_trivia(bytes, offset, scan_end);
    if bytes.get(offset) != Some(&b'=') {
        return None;
    }

    offset = skip_trivia(bytes, offset + 1, scan_end);
    let entity_start = offset;
    offset = scan_identifier(bytes, offset, scan_end)?;
    let entity_name = text.get(entity_start..offset)?.to_ascii_uppercase();
    offset = skip_trivia(bytes, offset, scan_end);
    if bytes.get(offset) != Some(&b'(') {
        return None;
    }

    let (argument_starts, end) = scan_arguments(bytes, offset + 1, scan_end);
    Some((
        ScannedInstance {
            entity_name,
            argument_starts,
        },
        end,
    ))
}

fn scan_arguments(bytes: &[u8], mut offset: usize, scan_end: usize) -> (Vec<usize>, usize) {
    let mut argument_starts = Vec::new();
    let mut expecting_argument = true;
    let mut depth = 0usize;

    while offset < scan_end {
        if expecting_argument {
            offset = skip_trivia(bytes, offset, scan_end);
            match bytes.get(offset) {
                Some(b')') if depth == 0 => return (argument_starts, offset + 1),
                Some(b',') if depth == 0 => {
                    offset += 1;
                    continue;
                }
                None => break,
                _ => {
                    argument_starts.push(offset);
                    expecting_argument = false;
                }
            }
        }

        match bytes[offset] {
            b'\'' => offset = scan_string(bytes, offset, scan_end),
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                offset = scan_block_comment(bytes, offset, scan_end);
            }
            b'(' => {
                depth += 1;
                offset += 1;
            }
            b')' if depth == 0 => return (argument_starts, offset + 1),
            b')' => {
                depth -= 1;
                offset += 1;
            }
            b',' if depth == 0 => {
                expecting_argument = true;
                offset += 1;
            }
            _ => offset += 1,
        }
    }

    (argument_starts, scan_end)
}

fn skip_trivia(bytes: &[u8], mut offset: usize, scan_end: usize) -> usize {
    loop {
        while offset < scan_end && bytes[offset].is_ascii_whitespace() {
            offset += 1;
        }

        if bytes.get(offset) == Some(&b'/') && bytes.get(offset + 1) == Some(&b'*') {
            offset = scan_block_comment(bytes, offset, scan_end);
            continue;
        }

        return offset;
    }
}

fn scan_instance_id(bytes: &[u8], mut offset: usize, scan_end: usize) -> Option<(u32, usize)> {
    offset += 1;
    let digit_start = offset;
    while offset < scan_end && bytes[offset].is_ascii_digit() {
        offset += 1;
    }
    let id = std::str::from_utf8(bytes.get(digit_start..offset)?)
        .ok()?
        .parse()
        .ok()?;
    Some((id, offset))
}

fn scan_identifier(bytes: &[u8], mut offset: usize, scan_end: usize) -> Option<usize> {
    if offset >= scan_end || !(bytes[offset].is_ascii_alphabetic() || bytes[offset] == b'_') {
        return None;
    }

    offset += 1;
    while offset < scan_end && (bytes[offset].is_ascii_alphanumeric() || bytes[offset] == b'_') {
        offset += 1;
    }
    Some(offset)
}

fn scan_string(bytes: &[u8], mut offset: usize, scan_end: usize) -> usize {
    offset += 1;
    while offset < scan_end {
        if bytes[offset] == b'\'' {
            if bytes.get(offset + 1) == Some(&b'\'') && offset + 1 < scan_end {
                offset += 2;
            } else {
                return offset + 1;
            }
        } else {
            offset += 1;
        }
    }
    scan_end
}

fn scan_block_comment(bytes: &[u8], mut offset: usize, scan_end: usize) -> usize {
    offset += 2;
    while offset + 1 < scan_end {
        if bytes[offset] == b'*' && bytes[offset + 1] == b'/' {
            return offset + 2;
        }
        offset += 1;
    }
    scan_end
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tower_lsp::lsp_types::{InlayHintLabel, Position};

    use super::*;
    use crate::schema::{EntityAttributeDoc, EntityDoc, SchemaDoc, SchemaDocCollection, TypeRef};

    fn schema_docs_with_wall() -> SchemaDocCollection {
        let entity = EntityDoc {
            name: "IfcWall".to_string(),
            attributes: vec![
                attribute("GlobalId", "IfcRoot"),
                attribute("OwnerHistory", "IfcRoot"),
                attribute("Name", "IfcRoot"),
                attribute("Representation", "IfcProduct"),
            ],
            url: String::new(),
            all_supertypes: HashSet::new(),
        };
        SchemaDocCollection::from_docs([(
            "IFC4".to_string(),
            SchemaDoc {
                entities: [("IFCWALL".to_string(), entity)].into(),
                types: Default::default(),
            },
        )])
    }

    fn attribute(name: &str, declared_in: &str) -> EntityAttributeDoc {
        EntityAttributeDoc {
            name: name.to_string(),
            type_name: "IfcLabel".to_string(),
            declared_in: declared_in.to_string(),
            ty: TypeRef::Generic { label: None },
            optional: false,
            allows_omitted: false,
        }
    }

    fn full_range(document: &Document) -> Range {
        Range {
            start: Position::new(0, 0),
            end: document
                .offset_to_position(document.text.len())
                .expect("document end should convert to a position"),
        }
    }

    fn labels(hints: &[InlayHint]) -> Vec<&str> {
        hints
            .iter()
            .map(|hint| match &hint.label {
                InlayHintLabel::String(label) => label.as_str(),
                InlayHintLabel::LabelParts(_) => panic!("expected simple hint label"),
            })
            .collect()
    }

    #[test]
    fn labels_arguments_with_flattened_schema_attributes() {
        let document = Document::new_unloaded(
            "#1=IFCWALL('id', #2, $, IFCPRODUCTREPRESENTATION($));".to_string(),
        );

        let hints = inlay_hints(
            &document,
            full_range(&document),
            &schema_docs_with_wall(),
            Some("IFC4"),
        )
        .expect("range should be valid");

        assert_eq!(
            labels(&hints),
            vec!["GlobalId:", "OwnerHistory:", "Name:", "Representation:"]
        );
        assert_eq!(hints[0].kind, Some(InlayHintKind::PARAMETER));
        assert_eq!(hints[0].padding_right, Some(true));
    }

    #[test]
    fn ignores_nested_commas_strings_and_comments() {
        let document = Document::new_unloaded(
            "#1=IFCWALL(IFCLABEL('a,b'), /* x,y */ #2, $, (1,2));".to_string(),
        );

        let hints = inlay_hints(
            &document,
            full_range(&document),
            &schema_docs_with_wall(),
            Some("IFC4"),
        )
        .expect("range should be valid");

        assert_eq!(
            labels(&hints),
            vec!["GlobalId:", "OwnerHistory:", "Name:", "Representation:"]
        );
    }

    #[test]
    fn emits_only_hints_inside_requested_range() {
        let text = "#1=IFCWALL('first',#2,$,*);\n#3=IFCWALL('second',#4,$,*);";
        let document = Document::new_unloaded(text.to_string());
        let range = Range {
            start: Position::new(1, 0),
            end: document
                .offset_to_position(text.len())
                .expect("document end should convert to a position"),
        };

        let hints = inlay_hints(&document, range, &schema_docs_with_wall(), Some("IFC4"))
            .expect("range should be valid");

        assert_eq!(hints.len(), 4);
        assert!(hints.iter().all(|hint| hint.position.line == 1));
    }

    #[test]
    fn returns_no_hints_for_unknown_schema_or_entity() {
        let document = Document::new_unloaded("#1=IFCDOOR($);".to_string());

        let unknown_entity = inlay_hints(
            &document,
            full_range(&document),
            &schema_docs_with_wall(),
            Some("IFC4"),
        )
        .expect("range should be valid");
        let unknown_schema = inlay_hints(
            &document,
            full_range(&document),
            &schema_docs_with_wall(),
            Some("IFC2X3"),
        )
        .expect("range should be valid");

        assert!(unknown_entity.is_empty());
        assert!(unknown_schema.is_empty());
    }

    #[test]
    fn ignores_fake_instance_inside_comment_when_range_starts_mid_comment() {
        let text = "/* start\n#1=IFCWALL('fake',#2,$,*);\n*/\n#3=IFCWALL('real',#4,$,*);";
        let document = Document::new_unloaded(text.to_string());
        let range = Range {
            start: Position::new(1, 0),
            end: document
                .offset_to_position(text.len())
                .expect("document end should convert to a position"),
        };

        let hints = inlay_hints(&document, range, &schema_docs_with_wall(), Some("IFC4"))
            .expect("range should be valid");

        assert_eq!(hints.len(), 4);
        assert!(hints.iter().all(|hint| hint.position.line == 3));
    }

    #[test]
    fn maps_hint_positions_after_non_ascii_text() {
        let text = "/* Wänd */ #1=IFCWALL('id',#2,$,*);";
        let document = Document::new_unloaded(text.to_string());

        let hints = inlay_hints(
            &document,
            full_range(&document),
            &schema_docs_with_wall(),
            Some("IFC4"),
        )
        .expect("range should be valid");

        assert_eq!(
            hints[0].position,
            document
                .offset_to_position(text.find("'id'").expect("argument should exist"))
                .expect("argument position should convert")
        );
    }
}
