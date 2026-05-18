//! Parsed in-memory representation of one IFC document.
//! This module turns full IFC source text into a tree-sitter parse plus the small indexes the
//! language server needs for hover, definition, references, and schema-aware diagnostics.
//! It also stores structured parameter values so diagnostics can validate attribute arguments.

use std::collections::HashMap;
use std::time::Instant;

use tower_lsp::lsp_types::{Position, Range};
use tree_sitter::{Node, Parser, Point, Tree, TreeCursor};

#[derive(Debug)]
pub struct Document {
    pub text: String,
    pub tree: Option<Tree>,
    pub schema_name: Option<String>,
    pub parse_mode: DocumentParseMode,
    pub definitions: HashMap<u32, DefinitionInfo>,
    pub references: HashMap<u32, Vec<Range>>,
    pub instances: Vec<EntityInstanceInfo>,
    pub(crate) instance_indexes_by_id: HashMap<u32, usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentParseMode {
    Full,
    NavigationOnly,
}

impl DocumentParseMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::NavigationOnly => "navigation-only",
        }
    }

    fn should_parse_parameters(self) -> bool {
        matches!(self, Self::Full)
    }

    fn should_index_references(self) -> bool {
        matches!(self, Self::Full)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentParseMetrics {
    pub mode: DocumentParseMode,
    pub source_bytes: usize,
    pub tree_parse_ms: u128,
    pub schema_detect_ms: u128,
    pub index_build_ms: u128,
    pub definitions: usize,
    pub reference_groups: usize,
    pub reference_ranges: usize,
    pub instances: usize,
    pub parameter_values: usize,
}

impl DocumentParseMetrics {
    pub fn total_parse_ms(&self) -> u128 {
        self.tree_parse_ms + self.schema_detect_ms + self.index_build_ms
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefinitionInfo {
    pub id_range: Range,
    pub entity_range: Range,
    pub entity_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityInstanceInfo {
    pub id: Option<u32>,
    pub id_range: Option<Range>,
    pub entity_name: String,
    pub entity_name_range: Range,
    pub entity_range: Range,
    pub parameter_list_range: Option<Range>,
    pub parameters: Vec<ParameterValue>,
}

type DocumentIndexes = (
    HashMap<u32, DefinitionInfo>,
    HashMap<u32, Vec<Range>>,
    Vec<EntityInstanceInfo>,
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParameterValue {
    Reference {
        id: u32,
        range: Range,
    },
    Enumeration {
        value: String,
        range: Range,
    },
    String {
        range: Range,
    },
    Number {
        text: String,
        range: Range,
    },
    Null {
        range: Range,
    },
    Omitted {
        range: Range,
    },
    List {
        items: Vec<ParameterValue>,
        range: Range,
    },
    Typed {
        type_name: String,
        inner: Vec<ParameterValue>,
        range: Range,
    },
    Unknown {
        range: Range,
    },
}

impl ParameterValue {
    pub fn range(&self) -> Range {
        match self {
            ParameterValue::Reference { range, .. }
            | ParameterValue::Enumeration { range, .. }
            | ParameterValue::String { range }
            | ParameterValue::Number { range, .. }
            | ParameterValue::Null { range }
            | ParameterValue::Omitted { range }
            | ParameterValue::List { range, .. }
            | ParameterValue::Typed { range, .. }
            | ParameterValue::Unknown { range } => *range,
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            ParameterValue::Reference { .. } => "reference",
            ParameterValue::Enumeration { .. } => "enumeration",
            ParameterValue::String { .. } => "string",
            ParameterValue::Number { .. } => "number",
            ParameterValue::Null { .. } => "null",
            ParameterValue::Omitted { .. } => "omitted value",
            ParameterValue::List { .. } => "aggregate",
            ParameterValue::Typed { .. } => "typed value",
            ParameterValue::Unknown { .. } => "unknown value",
        }
    }
}

impl Document {
    #[cfg(test)]
    pub fn parse(parser: &mut Parser, text: String) -> Self {
        Self::parse_with_metrics(parser, text, DocumentParseMode::Full).0
    }

    pub fn parse_with_metrics(
        parser: &mut Parser,
        text: String,
        mode: DocumentParseMode,
    ) -> (Self, DocumentParseMetrics) {
        let source_bytes = text.len();

        let tree_started = Instant::now();
        let tree = parser.parse(&text, None);
        let tree_parse_ms = tree_started.elapsed().as_millis();

        let schema_started = Instant::now();
        let schema_name = detect_schema(&tree, &text);
        let schema_detect_ms = schema_started.elapsed().as_millis();

        let index_started = Instant::now();
        let (definitions, references, instances) = build_indexes(&tree, &text, mode);
        let index_build_ms = index_started.elapsed().as_millis();

        let reference_ranges = references.values().map(Vec::len).sum();
        let parameter_values = instances
            .iter()
            .map(|instance| count_parameter_values(&instance.parameters))
            .sum();
        let metrics = DocumentParseMetrics {
            mode,
            source_bytes,
            tree_parse_ms,
            schema_detect_ms,
            index_build_ms,
            definitions: definitions.len(),
            reference_groups: references.len(),
            reference_ranges,
            instances: instances.len(),
            parameter_values,
        };

        let instance_indexes_by_id = instances
            .iter()
            .enumerate()
            .filter_map(|(index, instance)| instance.id.map(|id| (id, index)))
            .collect();
        let document = Self {
            text,
            tree,
            schema_name,
            parse_mode: mode,
            definitions,
            references,
            instances,
            instance_indexes_by_id,
        };

        (document, metrics)
    }

    pub fn node_at_position(&self, position: Position) -> Option<Node<'_>> {
        let tree = self.tree.as_ref()?;
        let point = Point {
            row: position.line as usize,
            column: position.character as usize,
        };

        tree.root_node().descendant_for_point_range(point, point)
    }

    pub fn instance_by_id(&self, id: u32) -> Option<&EntityInstanceInfo> {
        self.instance_indexes_by_id
            .get(&id)
            .and_then(|index| self.instances.get(*index))
    }
}

fn count_parameter_values(parameters: &[ParameterValue]) -> usize {
    parameters
        .iter()
        .map(|parameter| match parameter {
            ParameterValue::List { items, .. } => 1 + count_parameter_values(items),
            ParameterValue::Typed { inner, .. } => 1 + count_parameter_values(inner),
            _ => 1,
        })
        .sum()
}

fn detect_schema(tree: &Option<Tree>, text: &str) -> Option<String> {
    extract_file_schema_name(tree, text)
}

fn extract_file_schema_name(tree: &Option<Tree>, text: &str) -> Option<String> {
    let tree = tree.as_ref()?;
    let mut cursor = tree.root_node().walk();

    find_file_schema_name(&mut cursor, text)
}

fn find_file_schema_name(cursor: &mut TreeCursor, text: &str) -> Option<String> {
    loop {
        let node = cursor.node();

        if node.kind() == "header_entry"
            && let Some(schema_name) = parse_file_schema_entry(node, text)
        {
            return Some(schema_name);
        }

        if cursor.goto_first_child() {
            if let Some(schema_name) = find_file_schema_name(cursor, text) {
                cursor.goto_parent();
                return Some(schema_name);
            }
            cursor.goto_parent();
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    None
}

fn parse_file_schema_entry(node: Node<'_>, text: &str) -> Option<String> {
    let mut cursor = node.walk();
    let mut entry_name = None;
    let mut parameter_list = None;

    for child in node.children(&mut cursor) {
        match child.kind() {
            "entity_name" => entry_name = child.utf8_text(text.as_bytes()).ok(),
            "parameter_list" => parameter_list = Some(child),
            _ => {}
        }
    }

    if entry_name? != "FILE_SCHEMA" {
        return None;
    }

    let parameter_list = parameter_list?;
    extract_first_string(parameter_list, text).map(|value| value.to_ascii_uppercase())
}

fn extract_first_string(node: Node<'_>, text: &str) -> Option<String> {
    if node.kind() == "string" {
        let raw = node.utf8_text(text.as_bytes()).ok()?;
        return Some(raw.trim_matches('\'').to_string());
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(value) = extract_first_string(child, text) {
            return Some(value);
        }
    }

    None
}

fn build_indexes(tree: &Option<Tree>, text: &str, mode: DocumentParseMode) -> DocumentIndexes {
    let mut definitions = HashMap::new();
    let mut references = HashMap::new();
    let mut instances = Vec::new();

    let tree = match tree {
        Some(t) => t,
        None => return (definitions, references, instances),
    };

    let mut cursor = tree.root_node().walk();
    traverse(
        &mut cursor,
        text,
        mode,
        &mut definitions,
        &mut references,
        &mut instances,
    );

    (definitions, references, instances)
}

fn traverse(
    cursor: &mut TreeCursor,
    text: &str,
    mode: DocumentParseMode,
    definitions: &mut HashMap<u32, DefinitionInfo>,
    references: &mut HashMap<u32, Vec<Range>>,
    instances: &mut Vec<EntityInstanceInfo>,
) {
    loop {
        let node = cursor.node();

        if node.kind() == "entity_instance" {
            if let Some(instance) = parse_entity_instance(node, text, mode) {
                if let Some(id) = instance.id {
                    definitions.insert(
                        id,
                        DefinitionInfo {
                            id_range: instance
                                .id_range
                                .expect("definition ids should have a range"),
                            entity_range: instance.entity_range,
                            entity_name: mode
                                .should_parse_parameters()
                                .then(|| instance.entity_name.clone()),
                        },
                    );
                }
                if mode.should_parse_parameters() {
                    instances.push(instance);
                }
            }
        } else if mode.should_index_references()
            && node.kind() == "reference"
            && let Ok(ref_text) = node.utf8_text(text.as_bytes())
            && let Ok(id) = ref_text.trim_start_matches('#').parse::<u32>()
        {
            references.entry(id).or_default().push(node_range(&node));
        }

        if cursor.goto_first_child() {
            traverse(cursor, text, mode, definitions, references, instances);
            cursor.goto_parent();
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn parse_entity_instance(
    node: Node<'_>,
    text: &str,
    mode: DocumentParseMode,
) -> Option<EntityInstanceInfo> {
    let mut child_cursor = node.walk();
    let mut id = None;
    let mut id_range = None;
    let mut entity_name = None;
    let mut entity_name_range = None;
    let mut parameter_list_range = None;
    let mut parameters = Vec::new();

    for child in node.children(&mut child_cursor) {
        match child.kind() {
            "instance_id" => {
                let id_text = child.utf8_text(text.as_bytes()).ok()?;
                id = id_text.trim_start_matches('#').parse::<u32>().ok();
                id_range = Some(node_range(&child));
            }
            "entity_name" => {
                let name = child.utf8_text(text.as_bytes()).ok()?;
                entity_name = Some(name.to_ascii_uppercase());
                entity_name_range = Some(node_range(&child));
            }
            "parameter_list" => {
                parameter_list_range = Some(node_range(&child));
                if mode.should_parse_parameters() {
                    parameters = parse_parameter_list(child, text);
                }
            }
            _ => {}
        }
    }

    Some(EntityInstanceInfo {
        id,
        id_range,
        entity_name: entity_name?,
        entity_name_range: entity_name_range?,
        entity_range: node_range(&node),
        parameter_list_range,
        parameters,
    })
}

fn parse_parameter_list(node: Node<'_>, text: &str) -> Vec<ParameterValue> {
    let mut cursor = node.walk();
    let Some(sequence) = node
        .children(&mut cursor)
        .find(|child| child.kind() == "parameter_sequence")
    else {
        return Vec::new();
    };

    let mut seq_cursor = sequence.walk();
    sequence
        .children(&mut seq_cursor)
        .filter(|child| child.kind() == "parameter")
        .map(|parameter| parse_parameter(parameter, text))
        .collect()
}

fn parse_parameter(node: Node<'_>, text: &str) -> ParameterValue {
    let mut cursor = node.walk();
    let Some(value_node) = node.children(&mut cursor).find(|child| child.is_named()) else {
        return ParameterValue::Unknown {
            range: node_range(&node),
        };
    };

    parse_parameter_value(value_node, text)
}

fn parse_parameter_value(node: Node<'_>, text: &str) -> ParameterValue {
    match node.kind() {
        "reference" => {
            let range = node_range(&node);
            let id = node
                .utf8_text(text.as_bytes())
                .ok()
                .and_then(|value| value.trim_start_matches('#').parse::<u32>().ok());
            match id {
                Some(id) => ParameterValue::Reference { id, range },
                None => ParameterValue::Unknown { range },
            }
        }
        "enumeration" => {
            let range = node_range(&node);
            let value = node
                .utf8_text(text.as_bytes())
                .ok()
                .map(|value| value.trim_matches('.').to_ascii_uppercase())
                .unwrap_or_default();
            ParameterValue::Enumeration { value, range }
        }
        "string" => ParameterValue::String {
            range: node_range(&node),
        },
        "number" => ParameterValue::Number {
            text: node
                .utf8_text(text.as_bytes())
                .unwrap_or_default()
                .to_string(),
            range: node_range(&node),
        },
        "null_value" => ParameterValue::Null {
            range: node_range(&node),
        },
        "omitted_value" => ParameterValue::Omitted {
            range: node_range(&node),
        },
        "list" => {
            let range = node_range(&node);
            let mut cursor = node.walk();
            let items = node
                .children(&mut cursor)
                .find(|child| child.kind() == "parameter_sequence")
                .map(|sequence| {
                    let mut seq_cursor = sequence.walk();
                    sequence
                        .children(&mut seq_cursor)
                        .filter(|child| child.kind() == "parameter")
                        .map(|parameter| parse_parameter(parameter, text))
                        .collect()
                })
                .unwrap_or_default();
            ParameterValue::List { items, range }
        }
        "typed_parameter" => {
            let range = node_range(&node);
            let mut cursor = node.walk();
            let mut type_name = None;
            let mut inner = Vec::new();

            for child in node.children(&mut cursor) {
                match child.kind() {
                    "entity_name" => {
                        type_name = child
                            .utf8_text(text.as_bytes())
                            .ok()
                            .map(|name| name.to_ascii_uppercase());
                    }
                    "parameter_list" => inner = parse_parameter_list(child, text),
                    _ => {}
                }
            }

            ParameterValue::Typed {
                type_name: type_name.unwrap_or_default(),
                inner,
                range,
            }
        }
        _ => ParameterValue::Unknown {
            range: node_range(&node),
        },
    }
}

fn node_range(node: &Node<'_>) -> Range {
    let start = node.start_position();
    let end = node.end_position();

    Range {
        start: Position::new(start.row as u32, start.column as u32),
        end: Position::new(end.row as u32, end.column as u32),
    }
}

//*----- TESTS BEGIN HERE -----*
#[cfg(test)]
mod tests {
    use super::*;

    /// Helper Function
    /// Parses the given text and returns a [`Document`] with the text and initial state set.
    fn parse_document(text: &str) -> Document {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("Error loading IFC parser");

        Document::parse(&mut parser, text.to_string())
    }

    /// Helper Function
    /// Returns the position of the first occurrence of `needle` in `text`.
    fn position_at(text: &str, needle: &str) -> Position {
        let offset = text.find(needle).expect("needle should exist") as u32;
        Position::new(0, offset)
    }

    /// Tests that [`Document::parse`] keeps the text and initial state set.
    #[test]
    fn parse_keeps_text_and_initial_state() {
        let text = "#1=IFCWALL($);";
        let document = parse_document(text);
        assert_eq!(document.text, text);
        assert!(document.tree.is_some());
        assert_eq!(document.schema_name, None);
        // definitions and references are now populated, not empty
    }

    /// Tests that [`Document::parse`] detects the schema name from the header.
    #[test]
    fn parse_detects_custom_schema_name_from_header() {
        let text = "ISO-10303-21;HEADER;FILE_SCHEMA(('IFC4X3_LOCAL_TEST'));ENDSEC;DATA;#1=IFCWALL($);ENDSEC;END-ISO-10303-21;";
        let document = parse_document(text);

        assert_eq!(document.schema_name.as_deref(), Some("IFC4X3_LOCAL_TEST"));
    }

    /// Tests that [`Document::node_at_position`] finds the entity name.
    #[test]
    fn node_at_position_finds_entity_name() {
        let text = "#1=IFCWALL($);";
        let document = parse_document(text);

        let node = document
            .node_at_position(position_at(text, "IFCWALL"))
            .expect("entity_name node should exist");

        assert_eq!(node.kind(), "entity_name");
        assert_eq!(
            node.utf8_text(document.text.as_bytes()).ok(),
            Some("IFCWALL")
        );
    }

    /// Tests that [`Document::node_at_position`] finds the reference.
    #[test]
    fn node_at_position_finds_reference() {
        let text = "#1=IFCWALL(#2);";
        let document = parse_document(text);

        let node = document
            .node_at_position(position_at(text, "#2"))
            .expect("reference node should exist");

        assert_eq!(node.kind(), "reference");
        assert_eq!(node.utf8_text(document.text.as_bytes()).ok(), Some("#2"));
    }

    /// Tests that [`Document::parse`] indexes the entity definition.
    #[test]
    fn parse_indexes_entity_definition() {
        let text = "#1=IFCWALL($);";
        let document = parse_document(text);
        assert!(document.definitions.contains_key(&1));
        assert!(document.references.is_empty());
        assert_eq!(
            document.definitions[&1].entity_name.as_deref(),
            Some("IFCWALL")
        );
        // "#1" starts at column 0, row 0 and ends at column 2, row 0
        assert_eq!(
            document.definitions[&1].id_range,
            Range {
                start: Position::new(0, 0),
                end: Position::new(0, 2),
            }
        );
        // "IFCWALL" starts at column 2, row 0 and ends at column 8, row 0
        assert_eq!(
            document.definitions[&1].entity_range,
            Range {
                start: Position::new(0, 0),
                end: Position::new(0, text.len() as u32),
            }
        );
    }

    /// Tests that [`Document::parse`] indexes multiple definitions.
    #[test]
    fn parse_indexes_multiple_definitions() {
        let text = "#1=IFCWALL($);\n#2=IFCDOOR($);";
        let document = parse_document(text);
        assert!(document.definitions.contains_key(&1));
        assert!(document.definitions.contains_key(&2));
        assert_eq!(document.definitions.len(), 2);
    }

    /// Tests that [`Document::parse`] indexes references.
    #[test]
    fn parse_indexes_references() {
        let text = "#1=IFCWALL(#2);";
        let document = parse_document(text);
        assert!(document.references.contains_key(&2));
        assert_eq!(document.references[&2].len(), 1);
    }

    /// Tests that [`Document::parse`] indexes multiple references to the same ID.
    #[test]
    fn parse_indexes_multiple_references_to_same_id() {
        let text = "#1=IFCWALL(#2);\n#3=IFCDOOR(#2);";
        let document = parse_document(text);
        assert_eq!(document.references[&2].len(), 2);
    }

    #[test]
    fn instance_by_id_returns_matching_instance() {
        let text = "#1=IFCWALL($);\n#2=IFCDOOR($);";
        let document = parse_document(text);

        let instance = document.instance_by_id(2).expect("instance should exist");

        assert_eq!(instance.entity_name, "IFCDOOR");
    }

    #[test]
    fn navigation_only_parse_skips_full_instances() {
        let text = "#1=IFCWALL(#2);\n#2=IFCDOOR($);";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("Error loading IFC parser");

        let (document, metrics) = Document::parse_with_metrics(
            &mut parser,
            text.to_string(),
            DocumentParseMode::NavigationOnly,
        );

        assert_eq!(document.parse_mode, DocumentParseMode::NavigationOnly);
        assert_eq!(document.definitions.len(), 2);
        assert!(document.references.is_empty());
        assert!(document.instances.is_empty());
        assert!(document.instance_indexes_by_id.is_empty());
        assert_eq!(document.definitions[&1].entity_name, None);
        assert_eq!(metrics.reference_groups, 0);
        assert_eq!(metrics.reference_ranges, 0);
        assert_eq!(metrics.instances, 0);
        assert_eq!(metrics.parameter_values, 0);
    }
}
