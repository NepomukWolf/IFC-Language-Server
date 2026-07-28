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
    Void,
    Fill,
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
            "IFCRELVOIDSELEMENT" => (
                "RELATINGBUILDINGELEMENT",
                "RELATEDOPENINGELEMENT",
                4,
                5,
                EdgeKind::Void,
            ),
            "IFCRELFILLSELEMENT" => (
                "RELATINGOPENINGELEMENT",
                "RELATEDBUILDINGELEMENT",
                4,
                5,
                EdgeKind::Fill,
            ),
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
        EdgeKind::Void | EdgeKind::Fill => {
            parent == Some(Class::Product) && child == Some(Class::Product)
        }
        EdgeKind::Containment => parent == Some(Class::Spatial) && child == Some(Class::Product),
    }
}
fn priority(class: Class, kind: EdgeKind) -> u8 {
    match class {
        Class::Project | Class::Spatial => match kind {
            EdgeKind::Aggregate => 0,
            EdgeKind::Nest => 1,
            EdgeKind::Void | EdgeKind::Fill => 2,
            EdgeKind::Containment => 3,
        },
        Class::Product => match kind {
            EdgeKind::Aggregate => 0,
            EdgeKind::Nest => 1,
            EdgeKind::Void => 2,
            EdgeKind::Fill => 3,
            EdgeKind::Containment => 4,
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
        name: instance_symbol_name(item),
        detail: instance_symbol_detail(lines, schema, item),
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
fn instance_symbol_name(item: &EntityInstanceInfo) -> String {
    match item.id {
        Some(id) => format!("{} #{id}", item.entity_name),
        None => item.entity_name.clone(),
    }
}
fn instance_symbol_detail(
    lines: &[&str],
    schema: Option<&SchemaDoc>,
    item: &EntityInstanceInfo,
) -> Option<String> {
    schema
        .filter(|s| s.is_entity_compatible(&item.entity_name, "IFCROOT"))
        .and_then(|_| item.parameters.get(2))
        .and_then(|v| {
            if let ParameterValue::String { range } = v {
                text_for_range(lines, *range).map(|s| s.trim_matches('\'').replace("''", "'"))
            } else {
                None
            }
        })
        .filter(|name| !name.is_empty())
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
mod tests;
