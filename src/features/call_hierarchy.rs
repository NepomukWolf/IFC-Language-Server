//! Call hierarchy over entity instances: an instance's incoming calls are the instances that
//! reference it, and its outgoing calls the instances it references, so objectified relationships
//! such as `IfcRelContainedInSpatialStructure` appear as expandable intermediate nodes.
//! Items are resolved from the text index and scan only the instances they report, without
//! tree-sitter state, so they also work on files above the AST parsing limit.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, Position, SymbolKind,
    Url,
};

use crate::document::Document;
use crate::schema::SchemaDoc;

use super::instance_queries::LabelReader;
use super::step_scan::{ScannedInstance, id_tokens_in_span, scan_instance, statement_end};

/// The `data` of every item the server returns.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct ItemData {
    pub id: u32,
    /// The entity name in upper case.
    pub entity: String,
    /// The attributes of the referring instance that hold the call's references, in argument
    /// order: on an incoming call's `from` item, the attributes of `from`; on an outgoing call's
    /// `to` item, the attributes of the instance whose outgoing calls were requested. Left out on
    /// prepared items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<ReferringAttribute>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct ReferringAttribute {
    /// One-based position of the argument that holds the reference.
    pub position: usize,
    /// The attribute's name in the document's schema; None when the schema or entity is unknown.
    pub name: Option<String>,
}

/// The instance at `position`: on a `#id` token the instance that id names, elsewhere the instance
/// whose text holds `position`. None when there is no such instance or it has no definition.
pub fn prepare(
    document: &Document,
    uri: &Url,
    schema: Option<&SchemaDoc>,
    position: Position,
) -> Option<Vec<CallHierarchyItem>> {
    let id = match document.id_token_at_position(position) {
        Some((id, _)) => id,
        None => instance_at_offset(document, document.position_to_offset(position)?)?,
    };
    let start = *document.definitions.get(&id)?;
    let mut items = ItemBuilder::new(document, uri, schema);
    let item = items.item(start, id, Vec::new())?;
    Some(vec![item])
}

/// The instances that reference `id`, in document order.
pub fn incoming_calls(
    document: &Document,
    uri: &Url,
    schema: Option<&SchemaDoc>,
    id: u32,
) -> Vec<CallHierarchyIncomingCall> {
    let target_definition = document.definitions.get(&id).copied();

    // Reference offsets are sorted, so the references one instance holds are adjacent; those inside
    // the current referrer's statement need no lookup.
    let text = document.text.as_str();
    let mut referrers: Vec<(usize, u32, Vec<usize>)> = Vec::new();
    let mut referrer_end = 0;
    for &offset in document.references.get(&id).into_iter().flatten() {
        if Some(offset) == target_definition {
            continue;
        }
        if let Some((_, _, offsets)) = referrers.last_mut()
            && offset < referrer_end
        {
            offsets.push(offset);
            continue;
        }
        let Some((start, referrer)) = document.enclosing_definition(offset) else {
            continue;
        };
        referrer_end = scan_instance(text, start, text.len())
            .map_or(offset + 1, |(_, arguments_end)| {
                statement_end(text.as_bytes(), arguments_end)
            });
        referrers.push((start, referrer, vec![offset]));
    }

    let mut items = ItemBuilder::new(document, uri, schema);
    referrers
        .into_iter()
        .filter_map(|(start, referrer, offsets)| {
            let (instance, _) = scan_instance(&document.text, start, document.text.len())?;
            let attributes = items.referring_attributes(&instance, &offsets);
            Some(CallHierarchyIncomingCall {
                from: items.item(start, referrer, attributes)?,
                from_ranges: document.id_ranges_at_offsets(&offsets),
            })
        })
        .collect()
}

/// The instances `id` references, in the order of their first reference. References to ids
/// without a definition are left out. None when `id` has no definition.
pub fn outgoing_calls(
    document: &Document,
    uri: &Url,
    schema: Option<&SchemaDoc>,
    id: u32,
) -> Option<Vec<CallHierarchyOutgoingCall>> {
    let text = document.text.as_str();
    let start = *document.definitions.get(&id)?;
    let (instance, arguments_end) = scan_instance(text, start, text.len())?;

    let mut callees: Vec<(u32, Vec<usize>)> = Vec::new();
    let mut callee_indexes: HashMap<u32, usize> = HashMap::new();
    for (offset, callee) in id_tokens_in_span(text, start, arguments_end) {
        if offset == start {
            continue;
        }
        let index = *callee_indexes.entry(callee).or_insert_with(|| {
            callees.push((callee, Vec::new()));
            callees.len() - 1
        });
        callees[index].1.push(offset);
    }

    let mut items = ItemBuilder::new(document, uri, schema);
    let calls = callees
        .into_iter()
        .filter_map(|(callee, offsets)| {
            let callee_start = *document.definitions.get(&callee)?;
            let attributes = items.referring_attributes(&instance, &offsets);
            Some(CallHierarchyOutgoingCall {
                to: items.item(callee_start, callee, attributes)?,
                from_ranges: document.id_ranges_at_offsets(&offsets),
            })
        })
        .collect();
    Some(calls)
}

/// The instance id an item from this server names, read from its `data`.
pub fn item_id(item: &CallHierarchyItem) -> Option<u32> {
    let data = serde_json::from_value::<ItemData>(item.data.clone()?).ok()?;
    Some(data.id)
}

/// The id of the instance whose text, from `#id` through its closing `;`, holds `offset`. The
/// offset just past the `;` still counts, as a cursor at the end of the instance's line does.
fn instance_at_offset(document: &Document, offset: usize) -> Option<u32> {
    let (start, id) = document.enclosing_definition(offset)?;
    let text = document.text.as_str();
    let (_, arguments_end) = scan_instance(text, start, text.len())?;
    (offset <= statement_end(text.as_bytes(), arguments_end)).then_some(id)
}

/// Builds items, reading each entity's attribute names and label positions once.
struct ItemBuilder<'a> {
    document: &'a Document,
    uri: &'a Url,
    schema: Option<&'a SchemaDoc>,
    labels: LabelReader<'a>,
}

impl<'a> ItemBuilder<'a> {
    fn new(document: &'a Document, uri: &'a Url, schema: Option<&'a SchemaDoc>) -> Self {
        Self {
            document,
            uri,
            schema,
            labels: LabelReader::new(document, schema),
        }
    }

    /// The item for the instance defined at `start`, labelled with its Name, or else its GlobalId.
    fn item(
        &mut self,
        start: usize,
        id: u32,
        attributes: Vec<ReferringAttribute>,
    ) -> Option<CallHierarchyItem> {
        let text = self.document.text.as_str();
        let (instance, arguments_end) = scan_instance(text, start, text.len())?;
        let label = self.labels.read_scanned(&instance);
        let data = ItemData {
            id,
            entity: instance.entity_name,
            attributes,
        };
        Some(CallHierarchyItem {
            name: format!("#{id} {}", data.entity),
            kind: SymbolKind::OBJECT,
            tags: None,
            detail: label.name.or(label.global_id),
            uri: self.uri.clone(),
            range: self
                .document
                .range_for_offsets(start, statement_end(text.as_bytes(), arguments_end))?,
            selection_range: self.document.id_range_at_offset(start)?,
            data: Some(serde_json::to_value(data).expect("item data serializes to JSON")),
        })
    }

    /// The attributes of `instance` that hold the references at `reference_offsets`, which are
    /// sorted.
    fn referring_attributes(
        &self,
        instance: &ScannedInstance,
        reference_offsets: &[usize],
    ) -> Vec<ReferringAttribute> {
        let attribute_names = self
            .schema
            .and_then(|schema| schema.entity(&instance.entity_name))
            .map(|entity| &entity.attributes);
        let mut indexes: Vec<usize> = reference_offsets
            .iter()
            .filter_map(|offset| {
                instance
                    .arguments
                    .partition_point(|&(start, _)| start <= *offset)
                    .checked_sub(1)
            })
            .collect();
        indexes.dedup();
        indexes
            .into_iter()
            .map(|index| ReferringAttribute {
                position: index + 1,
                name: attribute_names
                    .and_then(|attributes| attributes.get(index))
                    .map(|attribute| attribute.name.clone()),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::json;
    use tower_lsp::lsp_types::Range;

    use super::*;
    use crate::schema::{EntityAttributeDoc, EntityDoc, TypeRef};

    const TEXT: &str = "DATA;\n\
        #1=IFCBUILDINGSTOREY('0st',$,'Level 1',$);\n\
        #2=IFCWALL('1wa',$,'North wall',$);\n\
        #3=IFCRELCONTAINEDINSPATIALSTRUCTURE('2re',$,$,$,(#2,#4),#1);\n\
        #4=IFCWALL('3wb',$,$,$);\n\
        ENDSEC;\n";

    fn document() -> Document {
        Document::new_unloaded(TEXT.to_string())
    }

    fn uri() -> Url {
        Url::parse("file:///a.ifc").unwrap()
    }

    fn position_of(needle: &str) -> Position {
        let offset = TEXT.find(needle).expect("needle in text");
        document().offset_to_position(offset).expect("position")
    }

    fn range_of(needle: &str) -> Range {
        range_in(needle, needle)
    }

    /// The range of `part` within the first occurrence of `needle`.
    fn range_in(needle: &str, part: &str) -> Range {
        let start =
            TEXT.find(needle).expect("needle in text") + needle.find(part).expect("part in needle");
        document()
            .range_for_offsets(start, start + part.len())
            .expect("range")
    }

    fn schema() -> SchemaDoc {
        let attribute = |name: &str| EntityAttributeDoc {
            name: name.to_string(),
            type_name: "IfcLabel".to_string(),
            declared_in: "IfcRoot".to_string(),
            ty: TypeRef::Generic { label: None },
            optional: true,
            allows_omitted: false,
        };
        let entity = |name: &str, attributes: &[&str]| EntityDoc {
            name: name.to_string(),
            attributes: attributes.iter().map(|name| attribute(name)).collect(),
            url: String::new(),
            all_supertypes: HashSet::new(),
        };
        let rooted = ["GlobalId", "OwnerHistory", "Name", "Description"];
        SchemaDoc {
            entities: [
                ("IFCWALL".to_string(), entity("IfcWall", &rooted)),
                (
                    "IFCBUILDINGSTOREY".to_string(),
                    entity("IfcBuildingStorey", &rooted),
                ),
                (
                    "IFCRELCONTAINEDINSPATIALSTRUCTURE".to_string(),
                    entity(
                        "IfcRelContainedInSpatialStructure",
                        &[&rooted[..], &["RelatedElements", "RelatingStructure"]].concat(),
                    ),
                ),
            ]
            .into(),
            types: Default::default(),
        }
    }

    fn prepared(schema: Option<&SchemaDoc>, position: Position) -> CallHierarchyItem {
        let mut items = prepare(&document(), &uri(), schema, position).expect("an item");
        assert_eq!(items.len(), 1);
        items.remove(0)
    }

    fn data(item: &CallHierarchyItem) -> serde_json::Value {
        item.data.clone().expect("item data")
    }

    fn incoming_ids(id: u32) -> Vec<u32> {
        incoming_calls(&document(), &uri(), Some(&schema()), id)
            .iter()
            .map(|call| item_id(&call.from).unwrap())
            .collect()
    }

    fn outgoing_ids(id: u32) -> Vec<u32> {
        outgoing_calls(&document(), &uri(), Some(&schema()), id)
            .unwrap()
            .iter()
            .map(|call| item_id(&call.to).unwrap())
            .collect()
    }

    #[test]
    fn prepares_the_instance_a_reference_names() {
        let item = prepared(Some(&schema()), position_of("#2,#4"));

        assert_eq!(item.name, "#2 IFCWALL");
        assert_eq!(item.kind, SymbolKind::OBJECT);
        assert_eq!(item.detail.as_deref(), Some("North wall"));
        assert_eq!(item.uri, uri());
        assert_eq!(item.range, range_of("#2=IFCWALL('1wa',$,'North wall',$);"));
        assert_eq!(item.selection_range, range_in("#2=", "#2"));
        assert_eq!(data(&item), json!({ "id": 2, "entity": "IFCWALL" }));
    }

    #[test]
    fn prepares_the_instance_whose_text_holds_the_position() {
        assert_eq!(item_id(&prepared(None, position_of("North"))), Some(2));
        assert_eq!(item_id(&prepared(None, position_of("\nENDSEC"))), Some(4));
    }

    #[test]
    fn labels_an_item_with_its_global_id_when_it_has_no_name() {
        let item = prepared(Some(&schema()), position_of("#4="));

        assert_eq!(item.detail.as_deref(), Some("3wb"));
    }

    #[test]
    fn items_have_no_label_without_a_schema() {
        assert_eq!(prepared(None, position_of("#2=")).detail, None);
    }

    #[test]
    fn prepares_nothing_outside_every_instance() {
        let document = document();
        let prepare_at = |position| prepare(&document, &uri(), Some(&schema()), position);

        assert!(prepare_at(position_of("ENDSEC")).is_none());
        assert!(prepare_at(Position::new(0, 0)).is_none());
        assert!(prepare_at(Position::new(99, 0)).is_none());
    }

    #[test]
    fn prepares_nothing_for_an_undefined_id() {
        let document = Document::new_unloaded("DATA;\n#2=IFCX(#9);\nENDSEC;\n".to_string());

        assert!(prepare(&document, &uri(), None, Position::new(1, 8)).is_none());
    }

    #[test]
    fn incoming_calls_name_the_referring_attributes() {
        let calls = incoming_calls(&document(), &uri(), Some(&schema()), 2);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].from.name, "#3 IFCRELCONTAINEDINSPATIALSTRUCTURE");
        assert_eq!(calls[0].from_ranges, [range_in("#2,#4", "#2")]);
        assert_eq!(
            data(&calls[0].from),
            json!({
                "id": 3,
                "entity": "IFCRELCONTAINEDINSPATIALSTRUCTURE",
                "attributes": [{ "position": 5, "name": "RelatedElements" }],
            })
        );
    }

    #[test]
    fn referring_attributes_have_only_positions_without_a_schema() {
        let calls = incoming_calls(&document(), &uri(), None, 1);

        assert_eq!(
            data(&calls[0].from)["attributes"],
            json!([{ "position": 6, "name": null }])
        );
    }

    #[test]
    fn referring_attributes_count_empty_arguments() {
        let text = "DATA;\n#1=IFCBUILDINGSTOREY($,$,$,$);\n#2=IFCWALL($,$,$,$);\n\
            #3=IFCRELCONTAINEDINSPATIALSTRUCTURE('id',,$,$,(#2),#1);\nENDSEC;\n";
        let document = Document::new_unloaded(text.to_string());

        let calls = incoming_calls(&document, &uri(), Some(&schema()), 2);

        assert_eq!(
            data(&calls[0].from)["attributes"],
            json!([{ "position": 5, "name": "RelatedElements" }])
        );
    }

    #[test]
    fn labels_count_empty_arguments() {
        let document = Document::new_unloaded(
            "DATA;\n#1=IFCWALL('g',,'North wall',$);\nENDSEC;\n".to_string(),
        );

        let items = prepare(&document, &uri(), Some(&schema()), Position::new(1, 1)).unwrap();

        assert_eq!(items[0].detail.as_deref(), Some("North wall"));
    }

    #[test]
    fn one_referrer_lists_every_reference_and_attribute() {
        let text = "DATA;\n#1=IFCWALL($);\n#2=IFCX(#1,(#1,#1),$);\nENDSEC;\n";
        let document = Document::new_unloaded(text.to_string());

        let calls = incoming_calls(&document, &uri(), None, 1);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].from_ranges.len(), 3);
        assert_eq!(
            data(&calls[0].from)["attributes"],
            json!([{ "position": 1, "name": null }, { "position": 2, "name": null }])
        );
    }

    #[test]
    fn many_references_from_one_instance_form_one_call() {
        let repeats = 20_000;
        let points = vec!["#1"; repeats].join(",");
        let text = format!(
            "DATA;\n#1=IFCCARTESIANPOINT((0.,0.));\n#2=IFCPOLYLINE(({points}));\n\
             #3=IFCX(#1,'#1',#2);\nENDSEC;\n"
        );
        let document = Document::new_unloaded(text.clone());

        let calls = incoming_calls(&document, &uri(), None, 1);

        let ids: Vec<_> = calls
            .iter()
            .map(|call| item_id(&call.from).unwrap())
            .collect();
        assert_eq!(ids, [2, 3]);
        let ranges = &calls[0].from_ranges;
        assert_eq!(ranges.len(), repeats);
        let polyline = text.find("((#1").unwrap() + 2;
        for index in [0, repeats - 1] {
            let start = polyline + index * "#1,".len();
            assert_eq!(
                Some(ranges[index]),
                document.range_for_offsets(start, start + 2)
            );
        }
        assert_eq!(
            data(&calls[0].from)["attributes"],
            json!([{ "position": 1, "name": null }])
        );
        assert_eq!(calls[1].from_ranges.len(), 1);
    }

    #[test]
    fn outgoing_calls_group_references_by_instance_and_skip_undefined_ids() {
        let text = "DATA;\n#1=IFCWALL($);\n#2=IFCX(#9,#1,(#1));\nENDSEC;\n";
        let document = Document::new_unloaded(text.to_string());

        let calls = outgoing_calls(&document, &uri(), None, 2).expect("instance 2 is defined");

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to.name, "#1 IFCWALL");
        assert_eq!(calls[0].from_ranges.len(), 2);
        assert_eq!(
            data(&calls[0].to)["attributes"],
            json!([{ "position": 2, "name": null }, { "position": 3, "name": null }])
        );
        assert!(outgoing_calls(&document, &uri(), None, 9).is_none());
    }

    #[test]
    fn relationships_are_intermediate_nodes() {
        assert_eq!(incoming_ids(2), [3]);
        assert_eq!(outgoing_ids(3), [2, 4, 1]);
        assert_eq!(incoming_ids(1), [3]);
        assert!(incoming_ids(3).is_empty());
        assert!(outgoing_ids(2).is_empty());

        let calls = outgoing_calls(&document(), &uri(), Some(&schema()), 3).unwrap();
        let attributes: Vec<_> = calls
            .iter()
            .map(|call| data(&call.to)["attributes"][0]["name"].clone())
            .collect();
        assert_eq!(
            attributes,
            [
                json!("RelatedElements"),
                json!("RelatedElements"),
                json!("RelatingStructure")
            ]
        );
    }

    #[test]
    fn an_item_without_server_data_has_no_id() {
        let mut item = prepared(None, position_of("#2="));
        item.data = None;

        assert_eq!(item_id(&item), None);
    }
}
