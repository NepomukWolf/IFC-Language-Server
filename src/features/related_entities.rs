//! Code-action navigation across the small set of IFC relationship entities named in issue #85:
//! containing spatial structure, aggregation parent/children, and host/opening element pairs.
//! Resolution starts from the entity under the cursor and only walks the handful of relationship
//! instances that reference it (via the existing text-index `references` map), so cost does not
//! grow with file size.

use std::collections::HashSet;

use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionResponse, Command, Position, Range,
    Url,
};

use crate::document::Document;
use crate::schema::{EntityDoc, SchemaDoc};

use super::step_scan::{ScannedInstance, ids_in_span, scan_instance};

pub const GO_TO_RELATED_ENTITY_COMMAND: &str = "ifc-language-server.goToRelatedEntity";
pub const NAVIGATION_KIND: &str = "navigation";

struct Relation {
    entity_name: &'static str,
    relating_attribute: &'static str,
    related_attribute: &'static str,
    relating_label: &'static str,
    related_label: &'static str,
}

const RELATIONS: &[Relation] = &[
    Relation {
        entity_name: "IFCRELCONTAINEDINSPATIALSTRUCTURE",
        relating_attribute: "RelatingStructure",
        related_attribute: "RelatedElements",
        relating_label: "Go to containing spatial structure",
        related_label: "Go to contained element",
    },
    Relation {
        entity_name: "IFCRELAGGREGATES",
        relating_attribute: "RelatingObject",
        related_attribute: "RelatedObjects",
        relating_label: "Go to parent aggregation",
        related_label: "Go to aggregated part",
    },
    Relation {
        entity_name: "IFCRELVOIDSELEMENT",
        relating_attribute: "RelatingBuildingElement",
        related_attribute: "RelatedOpeningElement",
        relating_label: "Go to host element",
        related_label: "Go to opening element",
    },
    Relation {
        entity_name: "IFCRELFILLSELEMENT",
        relating_attribute: "RelatingOpeningElement",
        related_attribute: "RelatedBuildingElement",
        relating_label: "Go to opening element",
        related_label: "Go to filling element",
    },
];

struct RelatedTarget {
    id: u32,
    label: &'static str,
}

pub fn code_actions(
    document: &Document,
    uri: &Url,
    position: Position,
    requested_kinds: Option<&[CodeActionKind]>,
    schema: Option<&SchemaDoc>,
) -> Option<CodeActionResponse> {
    if !kind_requested(requested_kinds) {
        return None;
    }
    let schema = schema?;

    let actions: Vec<CodeActionOrCommand> = related_targets(document, position, schema)
        .into_iter()
        .filter_map(|target| {
            let range = document.definition_range(target.id)?;
            Some(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("{} (#{})", target.label, target.id),
                kind: Some(CodeActionKind::new(NAVIGATION_KIND)),
                command: Some(Command {
                    title: target.label.to_string(),
                    command: GO_TO_RELATED_ENTITY_COMMAND.to_string(),
                    arguments: Some(vec![serde_json::json!({
                        "uri": uri.to_string(),
                        "range": range,
                    })]),
                }),
                ..CodeAction::default()
            }))
        })
        .collect();

    (!actions.is_empty()).then_some(actions)
}

/// Decodes the `(uri, range)` pair a `code_actions`-produced command was invoked with.
pub fn parse_go_to_target(arguments: &[serde_json::Value]) -> Option<(Url, Range)> {
    let payload = arguments.first()?;
    let uri = Url::parse(payload.get("uri")?.as_str()?).ok()?;
    let range = serde_json::from_value(payload.get("range")?.clone()).ok()?;
    Some((uri, range))
}

fn kind_requested(requested_kinds: Option<&[CodeActionKind]>) -> bool {
    requested_kinds.is_none_or(|kinds| {
        kinds.iter().any(|kind| {
            let requested = kind.as_str();
            requested.is_empty() || NAVIGATION_KIND.starts_with(requested)
        })
    })
}

fn related_targets(
    document: &Document,
    position: Position,
    schema: &SchemaDoc,
) -> Vec<RelatedTarget> {
    let mut targets = Vec::new();

    let Some(self_id) = self_entity_id(document, position) else {
        return targets;
    };
    let Some(reference_offsets) = document.references.get(&self_id) else {
        return targets;
    };
    let self_def_offset = document.definitions.get(&self_id).copied();
    if reference_offsets
        .iter()
        .all(|&offset| Some(offset) == self_def_offset)
    {
        return targets;
    }

    let sorted_defs = sorted_definitions(document);
    let statement_scan_end = document.text.len();
    let mut seen = HashSet::new();

    for &reference_offset in reference_offsets {
        if Some(reference_offset) == self_def_offset {
            continue;
        }

        let Some(enclosing_id) = enclosing_definition_id(&sorted_defs, reference_offset) else {
            continue;
        };
        if enclosing_id == self_id {
            continue;
        }
        let Some(&enclosing_start) = document.definitions.get(&enclosing_id) else {
            continue;
        };
        let Some((instance, statement_end)) =
            scan_instance(&document.text, enclosing_start, statement_scan_end)
        else {
            continue;
        };
        let Some(relation) = RELATIONS
            .iter()
            .find(|relation| relation.entity_name == instance.entity_name)
        else {
            continue;
        };
        let Some(entity_doc) = schema.entity(&instance.entity_name) else {
            continue;
        };
        let Some(relating_span) = argument_span(
            &instance,
            entity_doc,
            relation.relating_attribute,
            statement_end,
        ) else {
            continue;
        };
        let Some(related_span) = argument_span(
            &instance,
            entity_doc,
            relation.related_attribute,
            statement_end,
        ) else {
            continue;
        };

        let (target_span, label) =
            if reference_offset >= relating_span.0 && reference_offset < relating_span.1 {
                (related_span, relation.related_label)
            } else if reference_offset >= related_span.0 && reference_offset < related_span.1 {
                (relating_span, relation.relating_label)
            } else {
                continue;
            };

        for target_id in ids_in_span(&document.text, target_span.0, target_span.1) {
            if target_id == self_id {
                continue;
            }
            if seen.insert((target_id, label)) {
                targets.push(RelatedTarget {
                    id: target_id,
                    label,
                });
            }
        }
    }

    targets
}

fn argument_span(
    instance: &ScannedInstance,
    entity_doc: &EntityDoc,
    attribute_name: &str,
    statement_end: usize,
) -> Option<(usize, usize)> {
    let index = entity_doc
        .attributes
        .iter()
        .position(|attribute| attribute.name == attribute_name)?;
    let start = *instance.argument_starts.get(index)?;
    let end = instance
        .argument_starts
        .get(index + 1)
        .copied()
        .unwrap_or(statement_end.saturating_sub(1));
    Some((start, end))
}

fn self_entity_id(document: &Document, position: Position) -> Option<u32> {
    if let Some((id, _)) = document.id_token_at_position(position) {
        return Some(id);
    }

    let offset = document.position_to_offset(position)?;
    let sorted_defs = sorted_definitions(document);
    enclosing_definition_id(&sorted_defs, offset)
}

fn sorted_definitions(document: &Document) -> Vec<(usize, u32)> {
    let mut defs: Vec<(usize, u32)> = document
        .definitions
        .iter()
        .map(|(&id, &offset)| (offset, id))
        .collect();
    defs.sort_unstable();
    defs
}

fn enclosing_definition_id(sorted_defs: &[(usize, u32)], offset: usize) -> Option<u32> {
    match sorted_defs.binary_search_by_key(&offset, |&(def_offset, _)| def_offset) {
        Ok(index) => Some(sorted_defs[index].1),
        Err(0) => None,
        Err(index) => Some(sorted_defs[index - 1].1),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet as StdHashSet;

    use tower_lsp::lsp_types::CodeActionKind;

    use super::*;
    use crate::schema::{EntityAttributeDoc, TypeRef};

    fn attribute(name: &str) -> EntityAttributeDoc {
        EntityAttributeDoc {
            name: name.to_string(),
            type_name: "IfcLabel".to_string(),
            declared_in: "IfcRoot".to_string(),
            ty: TypeRef::Generic { label: None },
            optional: false,
            allows_omitted: false,
        }
    }

    fn entity(name: &str, attribute_names: &[&str]) -> EntityDoc {
        EntityDoc {
            name: name.to_string(),
            attributes: attribute_names.iter().map(|name| attribute(name)).collect(),
            url: String::new(),
            all_supertypes: StdHashSet::new(),
        }
    }

    fn schema() -> SchemaDoc {
        SchemaDoc {
            entities: [
                (
                    "IFCRELCONTAINEDINSPATIALSTRUCTURE".to_string(),
                    entity(
                        "IfcRelContainedInSpatialStructure",
                        &[
                            "GlobalId",
                            "OwnerHistory",
                            "Name",
                            "Description",
                            "RelatedElements",
                            "RelatingStructure",
                        ],
                    ),
                ),
                (
                    "IFCRELVOIDSELEMENT".to_string(),
                    entity(
                        "IfcRelVoidsElement",
                        &[
                            "GlobalId",
                            "OwnerHistory",
                            "Name",
                            "Description",
                            "RelatingBuildingElement",
                            "RelatedOpeningElement",
                        ],
                    ),
                ),
            ]
            .into(),
            types: Default::default(),
        }
    }

    fn document(text: &str) -> Document {
        Document::new_unloaded(text.to_string())
    }

    fn position_of(text: &str, needle: &str) -> Position {
        let offset = text.find(needle).expect("needle should be present");
        let document = document(text);
        document
            .offset_to_position(offset)
            .expect("offset should convert to a position")
    }

    #[test]
    fn finds_containing_spatial_structure_from_related_element() {
        let text = "#10=IFCWALL($);\n\
                     #20=IFCBUILDINGSTOREY($);\n\
                     #30=IFCRELCONTAINEDINSPATIALSTRUCTURE($,$,$,$,(#10),#20);";
        let document = document(text);
        let position = position_of(text, "IFCWALL");

        let targets = related_targets(&document, position, &schema());

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, 20);
        assert_eq!(targets[0].label, "Go to containing spatial structure");
    }

    #[test]
    fn finds_contained_elements_from_spatial_structure() {
        let text = "#10=IFCWALL($);\n\
                     #11=IFCDOOR($);\n\
                     #20=IFCBUILDINGSTOREY($);\n\
                     #30=IFCRELCONTAINEDINSPATIALSTRUCTURE($,$,$,$,(#10,#11),#20);";
        let document = document(text);
        let position = position_of(text, "IFCBUILDINGSTOREY");

        let mut ids: Vec<u32> = related_targets(&document, position, &schema())
            .into_iter()
            .map(|target| target.id)
            .collect();
        ids.sort_unstable();

        assert_eq!(ids, vec![10, 11]);
    }

    #[test]
    fn finds_host_element_from_opening() {
        let text = "#10=IFCWALL($);\n\
                     #11=IFCOPENINGELEMENT($);\n\
                     #40=IFCRELVOIDSELEMENT($,$,$,$,#10,#11);";
        let document = document(text);
        let position = position_of(text, "IFCOPENINGELEMENT");

        let targets = related_targets(&document, position, &schema());

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, 10);
        assert_eq!(targets[0].label, "Go to host element");
    }

    #[test]
    fn returns_nothing_for_entity_with_no_relations() {
        let text = "#10=IFCWALL($);";
        let document = document(text);
        let position = position_of(text, "IFCWALL");

        assert!(related_targets(&document, position, &schema()).is_empty());
    }

    #[test]
    fn code_actions_are_filtered_by_requested_kind() {
        let text = "#10=IFCWALL($);\n\
                     #20=IFCBUILDINGSTOREY($);\n\
                     #30=IFCRELCONTAINEDINSPATIALSTRUCTURE($,$,$,$,(#10),#20);";
        let document = document(text);
        let position = position_of(text, "IFCWALL");
        let uri = Url::parse("file:///model.ifc").expect("uri should parse");
        let schema = schema();

        assert!(
            code_actions(&document, &uri, position, None, Some(&schema)).is_some(),
            "unfiltered request should return navigation actions"
        );
        assert!(
            code_actions(
                &document,
                &uri,
                position,
                Some(&[CodeActionKind::QUICKFIX]),
                Some(&schema)
            )
            .is_none(),
            "quickfix-only request should not return navigation actions"
        );
    }

    #[test]
    fn parses_go_to_target_from_command_arguments() {
        let range = Range::new(Position::new(1, 0), Position::new(1, 5));
        let arguments = vec![serde_json::json!({
            "uri": "file:///model.ifc",
            "range": range,
        })];

        let (uri, parsed_range) = parse_go_to_target(&arguments).expect("should parse target");

        assert_eq!(uri.as_str(), "file:///model.ifc");
        assert_eq!(parsed_range, range);
    }
}
