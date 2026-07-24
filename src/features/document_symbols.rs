//! Curated IFC spatial hierarchy for Outline, breadcrumbs, and go-to-symbol.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::document::{Document, EntityInstanceInfo, ParameterValue};
use crate::schema::SchemaDoc;
use tower_lsp::lsp_types::{DocumentSymbol, DocumentSymbolResponse, Position, Range, SymbolKind};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Project,
    Spatial,
    Product,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EdgeKind {
    Aggregate,
    Nest,
    Containment,
}

#[derive(Clone, Copy)]
struct Edge {
    parent: usize,
    child: usize,
    kind: EdgeKind,
    order: usize,
}

pub fn document_symbols(
    document: &Document,
    schema: Option<&SchemaDoc>,
) -> Option<DocumentSymbolResponse> {
    if document.ast_skipped && document.instances.is_empty() {
        return Some(DocumentSymbolResponse::Nested(vec![message_symbol(
            "Outline unavailable: IFC AST was skipped because the file exceeds the configured AST size limit",
            "Increase `ifc.analysis.astFileSizeLimitMb` to enable the spatial outline",
        )]));
    }

    let lines: Vec<&str> = document.text.lines().collect();
    let by_id: HashMap<u32, usize> = document
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, item)| item.id.map(|id| (id, index)))
        .collect();
    let mut edges = Vec::new();
    for (order, relationship) in document.instances.iter().enumerate() {
        let Some((parent_parameter, children_parameter, kind)) =
            relationship_parameters(relationship, schema)
        else {
            continue;
        };
        let Some(parent_id) = first_reference(parent_parameter) else {
            continue;
        };
        let Some(&parent) = by_id.get(&parent_id) else {
            continue;
        };
        for child_id in references(children_parameter) {
            if let Some(&child) = by_id.get(&child_id) {
                if child != parent {
                    edges.push(Edge {
                        parent,
                        child,
                        kind,
                        order,
                    });
                }
            }
        }
    }

    let declared_classes: Vec<Option<Class>> = document
        .instances
        .iter()
        .map(|item| classify(item, schema))
        .collect();
    let classes = resolve_classes(&declared_classes, &edges);
    let relevant: HashSet<usize> = classes
        .iter()
        .enumerate()
        .filter_map(|(i, class)| class.map(|_| i))
        .collect();

    let mut candidates: HashMap<usize, Vec<Edge>> = HashMap::new();
    for edge in edges {
        if relevant.contains(&edge.parent)
            && relevant.contains(&edge.child)
            && valid_edge(edge, &classes)
        {
            candidates.entry(edge.child).or_default().push(edge);
        }
    }
    let mut parent = HashMap::new();
    for child in 0..document.instances.len() {
        let Some(class) = classes.get(child).copied().flatten() else {
            continue;
        };
        // Projects are always top-level outline anchors, even in malformed cyclic files.
        if class == Class::Project {
            continue;
        }
        let options = candidates.entry(child).or_default();
        options.sort_by_key(|edge| (priority(class, edge.kind), edge.order, edge.parent));
        for edge in options.iter() {
            if !would_cycle(child, edge.parent, &parent) {
                parent.insert(child, edge.parent);
                break;
            }
        }
    }
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    for (&child, &parent_index) in &parent {
        children.entry(parent_index).or_default().push(child);
    }
    for values in children.values_mut() {
        values.sort_unstable();
    }

    let mut roots: Vec<usize> = relevant
        .iter()
        .copied()
        .filter(|index| {
            matches!(classes[*index], Some(Class::Project | Class::Spatial))
                && !parent.contains_key(index)
        })
        .collect();
    roots.sort_unstable();
    let mut emitted = HashSet::new();
    let mut symbols = Vec::new();
    for root in roots {
        symbols.push(render_real(
            root,
            document,
            schema,
            &lines,
            &classes,
            &children,
            &mut emitted,
        ));
    }

    let mut uncontained: Vec<usize> = relevant
        .iter()
        .copied()
        .filter(|index| {
            classes[*index] == Some(Class::Product)
                && !emitted.contains(index)
                && !parent.contains_key(index)
        })
        .collect();
    uncontained.sort_unstable();
    if !uncontained.is_empty() {
        let grouped = render_product_groups(
            &uncontained,
            document,
            schema,
            &lines,
            &classes,
            &children,
            &mut emitted,
        );
        if !grouped.is_empty() {
            symbols.push(bucket_symbol("Uncontained products", grouped));
        }
    }
    Some(DocumentSymbolResponse::Nested(symbols))
}

fn classify(item: &EntityInstanceInfo, schema: Option<&SchemaDoc>) -> Option<Class> {
    if item.entity_name == "IFCPROJECT" {
        return Some(Class::Project);
    }
    let core = matches!(
        item.entity_name.as_str(),
        "IFCSITE" | "IFCBUILDING" | "IFCBUILDINGSTOREY" | "IFCSPACE"
    );
    if core
        || schema.is_some_and(|s| {
            s.is_entity_compatible(&item.entity_name, "IFCSPATIALELEMENT")
                || s.is_entity_compatible(&item.entity_name, "IFCSPATIALSTRUCTUREELEMENT")
        })
    {
        return Some(Class::Spatial);
    }
    if schema.is_some_and(|s| s.is_entity_compatible(&item.entity_name, "IFCPRODUCT")) {
        Some(Class::Product)
    } else {
        None
    }
}

fn resolve_classes(declared: &[Option<Class>], edges: &[Edge]) -> Vec<Option<Class>> {
    let mut spatial_decomposition = HashSet::new();
    let mut contained_children = HashSet::new();

    for edge in edges {
        match edge.kind {
            EdgeKind::Aggregate | EdgeKind::Nest
                if matches!(declared[edge.parent], Some(Class::Project | Class::Spatial))
                    && declared[edge.child] == Some(Class::Spatial) =>
            {
                spatial_decomposition.insert(edge.parent);
                spatial_decomposition.insert(edge.child);
            }
            EdgeKind::Containment => {
                contained_children.insert(edge.child);
            }
            _ => {}
        }
    }

    declared
        .iter()
        .enumerate()
        .map(|(index, class)| match class {
            Some(Class::Project) => Some(Class::Project),
            Some(Class::Spatial)
                if contained_children.contains(&index)
                    && !spatial_decomposition.contains(&index) =>
            {
                Some(Class::Product)
            }
            _ => *class,
        })
        .collect()
}

fn relationship_parameters<'a>(
    item: &'a EntityInstanceInfo,
    schema: Option<&SchemaDoc>,
) -> Option<(&'a ParameterValue, &'a ParameterValue, EdgeKind)> {
    let (parent_name, children_name, parent_fallback, children_fallback, kind) =
        match item.entity_name.as_str() {
            "IFCRELAGGREGATES" => (
                "RELATINGOBJECT",
                "RELATEDOBJECTS",
                4,
                5,
                EdgeKind::Aggregate,
            ),
            "IFCRELNESTS" => ("RELATINGOBJECT", "RELATEDOBJECTS", 4, 5, EdgeKind::Nest),
            "IFCRELCONTAINEDINSPATIALSTRUCTURE" => (
                "RELATINGSTRUCTURE",
                "RELATEDELEMENTS",
                5,
                4,
                EdgeKind::Containment,
            ),
            _ => return None,
        };
    let attribute_index = |name: &str, fallback| {
        schema
            .and_then(|s| s.entity(&item.entity_name))
            .and_then(|entity| {
                entity
                    .attributes
                    .iter()
                    .position(|attribute| attribute.name.eq_ignore_ascii_case(name))
            })
            .unwrap_or(fallback)
    };
    Some((
        item.parameters
            .get(attribute_index(parent_name, parent_fallback))?,
        item.parameters
            .get(attribute_index(children_name, children_fallback))?,
        kind,
    ))
}

fn references(value: &ParameterValue) -> Vec<u32> {
    fn visit(value: &ParameterValue, result: &mut Vec<u32>) {
        match value {
            ParameterValue::Reference { id, .. } => result.push(*id),
            ParameterValue::List { items, .. } => items.iter().for_each(|item| visit(item, result)),
            ParameterValue::Typed { inner, .. } => {
                inner.iter().for_each(|item| visit(item, result))
            }
            _ => {}
        }
    }
    let mut result = Vec::new();
    visit(value, &mut result);
    result
}
fn first_reference(value: &ParameterValue) -> Option<u32> {
    references(value).into_iter().next()
}
fn valid_edge(edge: Edge, classes: &[Option<Class>]) -> bool {
    let parent = classes[edge.parent];
    let child = classes[edge.child];
    match edge.kind {
        EdgeKind::Aggregate | EdgeKind::Nest => match child {
            Some(Class::Spatial) => matches!(parent, Some(Class::Project | Class::Spatial)),
            Some(Class::Product) => parent == Some(Class::Product),
            _ => false,
        },
        EdgeKind::Containment => parent == Some(Class::Spatial) && child == Some(Class::Product),
    }
}
fn priority(class: Class, kind: EdgeKind) -> u8 {
    match class {
        Class::Project | Class::Spatial => match kind {
            EdgeKind::Aggregate => 0,
            EdgeKind::Nest => 1,
            EdgeKind::Containment => 2,
        },
        Class::Product => match kind {
            EdgeKind::Aggregate => 0,
            EdgeKind::Nest => 1,
            EdgeKind::Containment => 2,
        },
    }
}
fn would_cycle(child: usize, mut candidate: usize, parent: &HashMap<usize, usize>) -> bool {
    loop {
        if candidate == child {
            return true;
        }
        let Some(next) = parent.get(&candidate) else {
            return false;
        };
        candidate = *next;
    }
}

#[allow(clippy::too_many_arguments)]
fn render_real(
    index: usize,
    document: &Document,
    schema: Option<&SchemaDoc>,
    lines: &[&str],
    classes: &[Option<Class>],
    children: &HashMap<usize, Vec<usize>>,
    emitted: &mut HashSet<usize>,
) -> DocumentSymbol {
    emitted.insert(index);
    let item = &document.instances[index];
    let all_children = children.get(&index).cloned().unwrap_or_default();
    let mut nested = Vec::new();
    for child in all_children
        .iter()
        .copied()
        .filter(|child| matches!(classes[*child], Some(Class::Project | Class::Spatial)))
    {
        if !emitted.contains(&child) {
            nested.push(render_real(
                child, document, schema, lines, classes, children, emitted,
            ));
        }
    }
    let products: Vec<usize> = all_children
        .into_iter()
        .filter(|child| classes[*child] == Some(Class::Product))
        .collect();
    nested.extend(render_product_groups(
        &products, document, schema, lines, classes, children, emitted,
    ));
    instance_symbol(lines, schema, item, classes[index], nested)
}

#[allow(clippy::too_many_arguments)]
fn render_product_groups(
    indices: &[usize],
    document: &Document,
    schema: Option<&SchemaDoc>,
    lines: &[&str],
    classes: &[Option<Class>],
    children: &HashMap<usize, Vec<usize>>,
    emitted: &mut HashSet<usize>,
) -> Vec<DocumentSymbol> {
    let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for &index in indices {
        if !emitted.contains(&index) {
            groups
                .entry(&document.instances[index].entity_name)
                .or_default()
                .push(index);
        }
    }
    let mut result = Vec::new();
    for (entity, group) in groups {
        let total = group.len();
        let mut items = Vec::new();
        for index in group {
            items.push(render_real(
                index, document, schema, lines, classes, children, emitted,
            ));
        }
        if !items.is_empty() {
            result.push(bucket_symbol(&format!("{entity} ({total})"), items));
        }
    }
    result
}

fn instance_symbol(
    lines: &[&str],
    schema: Option<&SchemaDoc>,
    item: &EntityInstanceInfo,
    class: Option<Class>,
    children: Vec<DocumentSymbol>,
) -> DocumentSymbol {
    DocumentSymbol {
        name: instance_symbol_name(lines, schema, item),
        detail: Some(item.entity_name.clone()),
        kind: symbol_kind(class),
        tags: None,
        #[allow(deprecated)]
        deprecated: None,
        range: lsp_range(lines, item.entity_range),
        selection_range: lsp_range(lines, item.id_range.unwrap_or(item.entity_name_range)),
        children: (!children.is_empty()).then_some(children),
    }
}
fn bucket_symbol(name: &str, children: Vec<DocumentSymbol>) -> DocumentSymbol {
    let range = extent(children.iter().map(|child| child.range));
    let selection_range = children
        .first()
        .map(|child| child.selection_range)
        .unwrap_or(range);
    DocumentSymbol {
        name: name.to_string(),
        detail: None,
        kind: SymbolKind::ARRAY,
        tags: None,
        #[allow(deprecated)]
        deprecated: None,
        range,
        selection_range,
        children: Some(children),
    }
}
fn extent(ranges: impl Iterator<Item = Range>) -> Range {
    let ranges: Vec<_> = ranges.collect();
    let start = ranges
        .iter()
        .map(|range| range.start)
        .min_by_key(|position| (position.line, position.character))
        .unwrap_or(Position::new(0, 0));
    let end = ranges
        .iter()
        .map(|range| range.end)
        .max_by_key(|position| (position.line, position.character))
        .unwrap_or(start);
    Range::new(start, end)
}
fn message_symbol(name: &str, detail: &str) -> DocumentSymbol {
    DocumentSymbol {
        name: name.to_string(),
        detail: Some(detail.to_string()),
        kind: SymbolKind::NULL,
        tags: None,
        #[allow(deprecated)]
        deprecated: None,
        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
        selection_range: Range::new(Position::new(0, 0), Position::new(0, 0)),
        children: None,
    }
}
fn instance_symbol_name(
    lines: &[&str],
    schema: Option<&SchemaDoc>,
    item: &EntityInstanceInfo,
) -> String {
    let name = schema
        .filter(|s| s.is_entity_compatible(&item.entity_name, "IFCROOT"))
        .and_then(|_| item.parameters.get(2))
        .and_then(|v| {
            if let ParameterValue::String { range } = v {
                text_for_range(lines, *range).map(|s| s.trim_matches('\'').replace("''", "'"))
            } else {
                None
            }
        });
    match (item.id, name) {
        (Some(id), Some(name)) if !name.is_empty() => format!("#{id} {name}"),
        (Some(id), _) => format!("#{id}"),
        _ => item.entity_name.clone(),
    }
}
fn text_for_range(lines: &[&str], range: Range) -> Option<String> {
    if range.start.line != range.end.line {
        return None;
    }
    let line = lines.get(range.start.line as usize)?;
    Some(
        line.get(range.start.character as usize..range.end.character as usize)?
            .to_string(),
    )
}
fn lsp_range(lines: &[&str], range: Range) -> Range {
    Range::new(
        lsp_position(lines, range.start),
        lsp_position(lines, range.end),
    )
}
fn lsp_position(lines: &[&str], position: Position) -> Position {
    let Some(prefix) = lines
        .get(position.line as usize)
        .and_then(|line| line.get(..position.character as usize))
    else {
        return position;
    };
    Position::new(
        position.line,
        prefix.encode_utf16().count().try_into().unwrap_or(u32::MAX),
    )
}
fn symbol_kind(class: Option<Class>) -> SymbolKind {
    match class {
        Some(Class::Project | Class::Spatial) => SymbolKind::NAMESPACE,
        _ => SymbolKind::OBJECT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{EntityAttributeDoc, EntityDoc, TypeRef};
    use std::collections::HashMap;
    use tree_sitter::Parser;

    fn parse(text: &str) -> Document {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .unwrap();
        Document::parse(&mut parser, text.to_string())
    }
    fn entity(name: &str, supers: &[&str], attrs: &[&str]) -> EntityDoc {
        EntityDoc {
            name: name.into(),
            attributes: attrs
                .iter()
                .map(|name| EntityAttributeDoc {
                    name: (*name).into(),
                    type_name: String::new(),
                    declared_in: String::new(),
                    ty: TypeRef::Generic { label: None },
                    optional: false,
                    allows_omitted: false,
                })
                .collect(),
            url: String::new(),
            all_supertypes: supers.iter().map(|s| (*s).into()).collect(),
        }
    }
    fn schema() -> SchemaDoc {
        let root = ["IFCROOT"];
        let product = ["IFCROOT", "IFCPRODUCT"];
        let spatial = ["IFCROOT", "IFCSPATIALELEMENT", "IFCPRODUCT"];
        let spatial_structure = ["IFCROOT", "IFCSPATIALSTRUCTUREELEMENT", "IFCPRODUCT"];
        let mut entities: HashMap<String, EntityDoc> = [
            ("IFCPROJECT", root.as_slice()),
            ("IFCSITE", spatial_structure.as_slice()),
            ("IFCBUILDING", spatial_structure.as_slice()),
            ("IFCBUILDINGSTOREY", spatial_structure.as_slice()),
            ("IFCSPACE", spatial.as_slice()),
            ("IFCWALL", product.as_slice()),
            ("IFCFURNISHINGELEMENT", product.as_slice()),
            ("IFCELEMENTASSEMBLY", product.as_slice()),
            ("IFCFACILITY", spatial_structure.as_slice()),
            ("IFCCARTESIANPOINT", &["IFCREPRESENTATIONITEM"]),
        ]
        .into_iter()
        .map(|(name, supers)| (name.into(), entity(name, supers, &[])))
        .collect();

        let object_attributes = ["A", "B", "C", "D", "RelatingObject", "RelatedObjects"];
        for name in ["IFCRELAGGREGATES", "IFCRELNESTS"] {
            entities.insert(name.into(), entity(name, &[], &object_attributes));
        }
        let containment_attributes = ["A", "B", "C", "D", "RelatedElements", "RelatingStructure"];
        entities.insert(
            "IFCRELCONTAINEDINSPATIALSTRUCTURE".into(),
            entity(
                "IFCRELCONTAINEDINSPATIALSTRUCTURE",
                &[],
                &containment_attributes,
            ),
        );

        SchemaDoc {
            entities,
            types: HashMap::new(),
        }
    }
    fn names(symbol: &DocumentSymbol) -> Vec<String> {
        let mut out = vec![symbol.name.clone()];
        if let Some(children) = &symbol.children {
            for child in children {
                out.extend(names(child));
            }
        }
        out
    }
    fn structure(symbols: &[DocumentSymbol]) -> Vec<String> {
        fn visit(symbol: &DocumentSymbol, depth: usize, out: &mut Vec<String>) {
            out.push(format!("{}{}", "  ".repeat(depth), symbol.name));
            for child in symbol.children.iter().flatten() {
                visit(child, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        for symbol in symbols {
            visit(symbol, 0, &mut out);
        }
        out
    }
    fn nested_symbols(doc: &Document) -> Vec<DocumentSymbol> {
        let DocumentSymbolResponse::Nested(symbols) =
            document_symbols(doc, Some(&schema())).unwrap()
        else {
            panic!("expected nested document symbols")
        };
        symbols
    }

    #[test]
    fn builds_spatial_tree_buckets_and_nested_assemblies() {
        let doc = parse(
            "DATA;\n#1=IFCPROJECT('g',$,'P',$,$,$,$,$,$);\n#2=IFCSITE('g',$,'S',$,$,$,$,$,$);\n#3=IFCBUILDING('g',$,'B',$,$,$,$,$,$);\n#4=IFCBUILDINGSTOREY('g',$,'L',$,$,$,$,$,$);\n#5=IFCSPACE('g',$,'R',$,$,$,$,$,$);\n#6=IFCELEMENTASSEMBLY('g',$,'A',$,$,$,$,$,$);\n#7=IFCWALL('g',$,'W',$,$,$,$,$,$);\n#10=IFCRELAGGREGATES('g',$,$,$,#1,(#2));\n#11=IFCRELAGGREGATES('g',$,$,$,#2,(#3));\n#12=IFCRELAGGREGATES('g',$,$,$,#3,(#4));\n#13=IFCRELAGGREGATES('g',$,$,$,#4,(#5));\n#14=IFCRELCONTAINEDINSPATIALSTRUCTURE('g',$,$,$,(#6),#5);\n#15=IFCRELNESTS('g',$,$,$,#6,(#7));\nENDSEC;",
        );
        let DocumentSymbolResponse::Nested(out) = document_symbols(&doc, Some(&schema())).unwrap()
        else {
            panic!()
        };
        let all = names(&out[0]);
        assert!(all.iter().any(|n| n == "#5 R"));
        assert!(all.iter().any(|n| n == "IFCELEMENTASSEMBLY (1)"));
        assert!(all.iter().any(|n| n == "#7 W"));
        assert_eq!(
            out[0].range,
            lsp_range(
                &doc.text.lines().collect::<Vec<_>>(),
                doc.instances[0].entity_range
            )
        );
    }
    #[test]
    fn handles_duplicates_cycles_missing_refs_and_multiple_roots() {
        let doc = parse(
            "DATA;\n#1=IFCPROJECT('g',$,'A',$,$,$,$,$,$);\n#2=IFCPROJECT('g',$,'B',$,$,$,$,$,$);\n#3=IFCSITE('g',$,'S',$,$,$,$,$,$);\n#4=IFCWALL('g',$,'W',$,$,$,$,$,$);\n#10=IFCRELAGGREGATES('g',$,$,$,#1,(#3,#99,#3));\n#11=IFCRELAGGREGATES('g',$,$,$,#3,(#1));\nENDSEC;",
        );
        let DocumentSymbolResponse::Nested(out) = document_symbols(&doc, Some(&schema())).unwrap()
        else {
            panic!()
        };
        assert_eq!(out.iter().filter(|s| s.name.starts_with('#')).count(), 2);
        assert!(out.iter().any(|s| s.name == "#2 B"));
        assert!(
            out.iter()
                .any(|s| s.name.starts_with("Uncontained products"))
        );
    }
    #[test]
    fn emits_all_products_and_preserves_utf16_ranges() {
        let doc = parse(
            "DATA;\n#1=IFCPROJECT('g',$,'😀',$,$,$,$,$,$);\n#2=IFCSPACE('g',$,'S',$,$,$,$,$,$);\n#3=IFCWALL('g',$,'😀',$,$,$,$,$,$);\n#4=IFCWALL('g',$,'B',$,$,$,$,$,$);\n#10=IFCRELAGGREGATES('g',$,$,$,#1,(#2));\n#11=IFCRELCONTAINEDINSPATIALSTRUCTURE('g',$,$,$,(#3,#4),#2);\nENDSEC;",
        );
        let DocumentSymbolResponse::Nested(out) = document_symbols(&doc, Some(&schema())).unwrap()
        else {
            panic!()
        };
        let all = names(&out[0]);
        assert!(all.iter().any(|n| n == "#2 S"));
        assert_eq!(all.iter().filter(|n| n.starts_with("#3 ")).count(), 1);
        assert_eq!(all.iter().filter(|n| n.starts_with("#4 ")).count(), 1);
        let project = &out[0];
        let statement = "#1=IFCPROJECT('g',$,'😀',$,$,$,$,$,$);";
        assert_eq!(
            project.range.end.character,
            statement.encode_utf16().count() as u32
        );
    }
    #[test]
    fn explains_ast_skip() {
        let mut doc = Document::new_unloaded("x".into());
        doc.ast_skipped = true;
        let DocumentSymbolResponse::Nested(out) = document_symbols(&doc, None).unwrap() else {
            panic!()
        };
        assert!(out[0].name.contains("AST was skipped"));
        assert_eq!(
            out[0].detail.as_deref(),
            Some("Increase `ifc.analysis.astFileSizeLimitMb` to enable the spatial outline")
        );
    }

    #[test]
    fn schema_less_outline_keeps_core_backbone_and_excludes_unknown_endpoints() {
        let doc = parse(
            "DATA;\n#1=IFCPROJECT('g',$,'P',$,$,$,$,$,$);\n#2=IFCSITE('g',$,'S',$,$,$,$,$,$);\n#3=IFCTASK('g',$,'Parent',$,$,$,$,$,$);\n#4=IFCTASK('g',$,'Child',$,$,$,$,$,$);\n#10=IFCRELAGGREGATES('g',$,$,$,#1,(#2));\n#11=IFCRELNESTS('g',$,$,$,#3,(#4));\nENDSEC;",
        );
        let DocumentSymbolResponse::Nested(out) = document_symbols(&doc, None).unwrap() else {
            panic!()
        };
        assert_eq!(out.len(), 1);
        assert_eq!(names(&out[0]), vec!["#1", "#2"]);
    }

    #[test]
    fn product_whole_part_precedes_spatial_containment() {
        let doc = parse(
            "DATA;\n#1=IFCPROJECT('g',$,'P',$,$,$,$,$,$);\n#2=IFCSPACE('g',$,'S',$,$,$,$,$,$);\n#3=IFCELEMENTASSEMBLY('g',$,'A',$,$,$,$,$,$);\n#4=IFCWALL('g',$,'W',$,$,$,$,$,$);\n#10=IFCRELAGGREGATES('g',$,$,$,#1,(#2));\n#11=IFCRELCONTAINEDINSPATIALSTRUCTURE('g',$,$,$,(#3,#4),#2);\n#12=IFCRELAGGREGATES('g',$,$,$,#3,(#4));\nENDSEC;",
        );
        let DocumentSymbolResponse::Nested(out) = document_symbols(&doc, Some(&schema())).unwrap()
        else {
            panic!()
        };
        let space = &out[0].children.as_ref().unwrap()[0];
        let assembly_bucket = &space.children.as_ref().unwrap()[0];
        let assembly = &assembly_bucket.children.as_ref().unwrap()[0];
        let wall_bucket = &assembly.children.as_ref().unwrap()[0];
        assert_eq!(wall_bucket.children.as_ref().unwrap()[0].name, "#4 W");
        assert_eq!(
            names(&out[0]).iter().filter(|name| *name == "#4 W").count(),
            1
        );
    }

    #[test]
    fn contained_products_attach_to_the_correct_spatial_container_and_type_bucket() {
        let doc = parse(
            "DATA;\n#1=IFCPROJECT('g',$,'P',$,$,$,$,$,$);\n#2=IFCBUILDINGSTOREY('g',$,'Storey',$,$,$,$,$,$);\n#3=IFCSPACE('g',$,'Space',$,$,$,$,$,$);\n#4=IFCWALL('g',$,'Wall',$,$,$,$,$,$);\n#5=IFCFURNISHINGELEMENT('g',$,'Chair',$,$,$,$,$,$);\n#10=IFCRELAGGREGATES('g',$,$,$,#1,(#2));\n#11=IFCRELAGGREGATES('g',$,$,$,#2,(#3));\n#12=IFCRELCONTAINEDINSPATIALSTRUCTURE('g',$,$,$,(#4),#2);\n#13=IFCRELCONTAINEDINSPATIALSTRUCTURE('g',$,$,$,(#5),#3);\nENDSEC;",
        );
        assert_eq!(
            structure(&nested_symbols(&doc)),
            [
                "#1 P",
                "  #2 Storey",
                "    #3 Space",
                "      IFCFURNISHINGELEMENT (1)",
                "        #5 Chair",
                "    IFCWALL (1)",
                "      #4 Wall",
            ]
        );
    }

    #[test]
    fn breaks_a_non_project_product_cycle_deterministically_and_emits_each_product_once() {
        let doc = parse(
            "DATA;\n#1=IFCELEMENTASSEMBLY('g',$,'First',$,$,$,$,$,$);\n#2=IFCELEMENTASSEMBLY('g',$,'Second',$,$,$,$,$,$);\n#10=IFCRELAGGREGATES('g',$,$,$,#2,(#1));\n#11=IFCRELAGGREGATES('g',$,$,$,#1,(#2));\nENDSEC;",
        );
        let symbols = nested_symbols(&doc);
        assert_eq!(
            structure(&symbols),
            [
                "Uncontained products",
                "  IFCELEMENTASSEMBLY (1)",
                "    #2 Second",
                "      IFCELEMENTASSEMBLY (1)",
                "        #1 First",
            ]
        );
        for name in ["#1 First", "#2 Second"] {
            assert_eq!(
                structure(&symbols)
                    .iter()
                    .filter(|item| item.ends_with(name))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn excludes_schema_known_low_level_support_entities() {
        let doc = parse(
            "DATA;\n#1=IFCPROJECT('g',$,'P',$,$,$,$,$,$);\n#2=IFCCARTESIANPOINT((1.,2.,3.));\nENDSEC;",
        );
        assert_eq!(structure(&nested_symbols(&doc)), ["#1 P"]);
    }

    #[test]
    fn keeps_multiple_projects_and_orphan_space_roots_in_source_order() {
        let doc = parse(
            "DATA;\n#1=IFCPROJECT('g',$,'First',$,$,$,$,$,$);\n#2=IFCSPACE('g',$,'Orphan',$,$,$,$,$,$);\n#3=IFCPROJECT('g',$,'Second',$,$,$,$,$,$);\nENDSEC;",
        );
        assert_eq!(
            structure(&nested_symbols(&doc)),
            ["#1 First", "#2 Orphan", "#3 Second"]
        );
    }

    #[test]
    fn includes_schema_compatible_ifc4x3_spatial_subtypes_as_spatial_nodes() {
        let doc = parse(
            "DATA;\n#1=IFCPROJECT('g',$,'P',$,$,$,$,$,$);\n#2=IFCFACILITY('g',$,'Rail Facility',$,$,$,$,$,$);\n#10=IFCRELAGGREGATES('g',$,$,$,#1,(#2));\nENDSEC;",
        );
        assert_eq!(
            structure(&nested_symbols(&doc)),
            ["#1 P", "  #2 Rail Facility"]
        );
    }

    #[test]
    fn contained_vendor_mapped_sites_are_products_but_decomposed_site_remains_spatial() {
        let doc = parse(
            "DATA;\n#1=IFCPROJECT('g',$,'Project',$,$,$,$,$,$);\n#2=IFCSITE('g',$,'Real site',$,$,$,$,$,$);\n#3=IFCBUILDING('g',$,'Building',$,$,$,$,$,$);\n#4=IFCBUILDINGSTOREY('g',$,'Storey',$,$,$,$,$,$);\n#5=IFCSITE('g',$,'Bench A',$,$,$,$,$,$);\n#6=IFCSITE('g',$,'Bench B',$,$,$,$,$,$);\n#10=IFCRELAGGREGATES('g',$,$,$,#1,(#2));\n#11=IFCRELAGGREGATES('g',$,$,$,#2,(#3));\n#12=IFCRELAGGREGATES('g',$,$,$,#3,(#4));\n#13=IFCRELCONTAINEDINSPATIALSTRUCTURE('g',$,$,$,(#5,#6),#4);\nENDSEC;",
        );
        let symbols = nested_symbols(&doc);
        assert_eq!(
            structure(&symbols),
            [
                "#1 Project",
                "  #2 Real site",
                "    #3 Building",
                "      #4 Storey",
                "        IFCSITE (2)",
                "          #5 Bench A",
                "          #6 Bench B",
            ]
        );
        let storey = &symbols[0].children.as_ref().unwrap()[0]
            .children
            .as_ref()
            .unwrap()[0]
            .children
            .as_ref()
            .unwrap()[0];
        let benches = &storey.children.as_ref().unwrap()[0]
            .children
            .as_ref()
            .unwrap();
        assert_eq!(
            symbols[0].children.as_ref().unwrap()[0].kind,
            SymbolKind::NAMESPACE
        );
        assert!(benches.iter().all(|bench| bench.kind == SymbolKind::OBJECT));
    }

    #[test]
    fn decomposition_wins_when_a_spatial_entity_is_also_contained() {
        let doc = parse(
            "DATA;\n#1=IFCPROJECT('g',$,'Project',$,$,$,$,$,$);\n#2=IFCSITE('g',$,'Site',$,$,$,$,$,$);\n#3=IFCBUILDINGSTOREY('g',$,'Storey',$,$,$,$,$,$);\n#10=IFCRELAGGREGATES('g',$,$,$,#1,(#2,#3));\n#11=IFCRELCONTAINEDINSPATIALSTRUCTURE('g',$,$,$,(#2),#3);\nENDSEC;",
        );
        assert_eq!(
            structure(&nested_symbols(&doc)),
            ["#1 Project", "  #2 Site", "  #3 Storey"]
        );
    }

    #[test]
    fn schema_less_contained_core_site_is_contextually_a_product() {
        let doc = parse(
            "DATA;\n#1=IFCPROJECT('g',$,'Project',$,$,$,$,$,$);\n#2=IFCBUILDINGSTOREY('g',$,'Storey',$,$,$,$,$,$);\n#3=IFCSITE('g',$,'Bench',$,$,$,$,$,$);\n#4=IFCVENDORUNKNOWN($);\n#10=IFCRELAGGREGATES('g',$,$,$,#1,(#2));\n#11=IFCRELCONTAINEDINSPATIALSTRUCTURE('g',$,$,$,(#3,#4),#2);\nENDSEC;",
        );
        let DocumentSymbolResponse::Nested(symbols) = document_symbols(&doc, None).unwrap() else {
            panic!()
        };
        assert_eq!(
            structure(&symbols),
            ["#1", "  #2", "    IFCSITE (1)", "      #3"]
        );
        assert_eq!(
            symbols[0].children.as_ref().unwrap()[0]
                .children
                .as_ref()
                .unwrap()[0]
                .children
                .as_ref()
                .unwrap()[0]
                .kind,
            SymbolKind::OBJECT
        );
    }

    #[test]
    fn real_selection_ranges_are_local_and_buckets_select_their_first_child() {
        fn contains(outer: Range, inner: Range) -> bool {
            outer.start <= inner.start && inner.end <= outer.end
        }
        fn verify(symbol: &DocumentSymbol) {
            if symbol
                .detail
                .as_deref()
                .is_some_and(|detail| detail.starts_with("IFC"))
                && symbol.kind != SymbolKind::NULL
            {
                assert!(
                    contains(symbol.range, symbol.selection_range),
                    "selection for {} is outside its statement range",
                    symbol.name
                );
            }
            if symbol.detail.is_none() {
                let first = &symbol.children.as_ref().unwrap()[0];
                assert_eq!(
                    symbol.selection_range, first.selection_range,
                    "bucket {} must select its first displayed child",
                    symbol.name
                );
            }
            for child in symbol.children.iter().flatten() {
                verify(child);
            }
        }

        let doc = parse(
            "DATA;\n#1=IFCPROJECT('g',$,'P',$,$,$,$,$,$);\n#2=IFCSPACE('g',$,'Room',$,$,$,$,$,$);\n#3=IFCWALL('g',$,'Wall',$,$,$,$,$,$);\n#4=IFCFURNISHINGELEMENT('g',$,'Desk',$,$,$,$,$,$);\n#10=IFCRELAGGREGATES('g',$,$,$,#1,(#2));\n#11=IFCRELCONTAINEDINSPATIALSTRUCTURE('g',$,$,$,(#3),#2);\nENDSEC;",
        );
        let symbols = nested_symbols(&doc);
        assert_eq!(
            structure(&symbols),
            [
                "#1 P",
                "  #2 Room",
                "    IFCWALL (1)",
                "      #3 Wall",
                "Uncontained products",
                "  IFCFURNISHINGELEMENT (1)",
                "    #4 Desk",
            ]
        );
        for symbol in &symbols {
            verify(symbol);
        }
    }
}
