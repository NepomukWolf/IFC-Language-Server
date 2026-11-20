//! Curated IFC spatial hierarchy for Outline, breadcrumbs, and go-to-symbol.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::document::{EntityInstanceCollection, EntityInstanceInfo, ParameterValue};
use crate::schema::SchemaDoc;
use tower_lsp::lsp_types::{DocumentSymbol, DocumentSymbolResponse, Position, Range, SymbolKind};

/// The outline nests no deeper than this many levels, because clients and the JSON serialiser
/// recurse over it.
const MAX_OUTLINE_DEPTH: usize = 256;

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
    PortContainment,
    Containment,
}

#[derive(Clone, Copy)]
struct Edge {
    parent: usize,
    child: usize,
    kind: EdgeKind,
    order: usize,
}

#[derive(Clone, Copy)]
enum ProductRenderMode {
    Direct,
    GroupMultiOnly,
}

pub fn document_symbols(
    text: &str,
    instances: &EntityInstanceCollection,
    ast_skipped: bool,
    schema: Option<&SchemaDoc>,
) -> Option<DocumentSymbolResponse> {
    if ast_skipped && instances.instances.is_empty() {
        return Some(DocumentSymbolResponse::Nested(vec![message_symbol(
            "Outline unavailable: IFC AST was skipped because the file exceeds the configured AST size limit",
            "Increase `ifc.analysis.astFileSizeLimitMb` to enable the spatial outline",
        )]));
    }

    let lines: Vec<&str> = text.lines().collect();
    let by_id: HashMap<u32, usize> = instances
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, item)| item.id.map(|id| (id, index)))
        .collect();
    let mut edges = Vec::new();
    for (order, relationship) in instances.instances.iter().enumerate() {
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
            if let Some(&child) = by_id.get(&child_id)
                && child != parent
            {
                edges.push(Edge {
                    parent,
                    child,
                    kind,
                    order,
                });
            }
        }
    }

    let declared_classes: Vec<Option<Class>> = instances
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
    let mut trees = TreeRoots::new(instances.instances.len());
    for child in 0..instances.instances.len() {
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
            if trees.root(edge.parent) != child {
                parent.insert(child, edge.parent);
                trees.attach(child, edge.parent);
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
    let mut outline = OutlineBuilder {
        instances,
        classes: &classes,
        children: &children,
        nodes: Vec::new(),
        unexpanded: Vec::new(),
    };
    let mut top: Vec<usize> = roots
        .into_iter()
        .map(|root| outline.add_instance(root))
        .collect();

    let mut uncontained: Vec<usize> = relevant
        .iter()
        .copied()
        .filter(|index| classes[*index] == Some(Class::Product) && !parent.contains_key(index))
        .collect();
    uncontained.sort_unstable();
    if !uncontained.is_empty() {
        let bucket = outline.add(OutlineLabel::Bucket("Uncontained products".to_string()));
        outline.nodes[bucket].children =
            outline.add_product_groups(&uncontained, ProductRenderMode::GroupMultiOnly);
        top.push(bucket);
    }
    outline.expand();
    let symbols = outline_symbols(&outline.nodes, &top, |label, children| match label {
        OutlineLabel::Instance(index) => instance_symbol(
            &lines,
            schema,
            &instances.instances[*index],
            classes[*index],
            children,
        ),
        OutlineLabel::Bucket(name) => bucket_symbol(name, children),
    });
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
            "IFCRELCONNECTSPORTTOELEMENT" => (
                "RELATEDELEMENT",
                "RELATINGPORT",
                5,
                4,
                EdgeKind::PortContainment,
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
        EdgeKind::Void | EdgeKind::Fill | EdgeKind::PortContainment => {
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
            EdgeKind::PortContainment => 3,
            EdgeKind::Containment => 4,
        },
        Class::Product => match kind {
            EdgeKind::Aggregate => 0,
            EdgeKind::Nest => 1,
            EdgeKind::Void => 2,
            EdgeKind::Fill => 3,
            EdgeKind::PortContainment => 4,
            EdgeKind::Containment => 5,
        },
    }
}
/// Union-find over the chosen parent links: each set is one outline tree, and `root` holds that
/// tree's root at the set's representative.
struct TreeRoots {
    set: Vec<usize>,
    size: Vec<usize>,
    root: Vec<usize>,
}

impl TreeRoots {
    fn new(len: usize) -> Self {
        Self {
            set: (0..len).collect(),
            size: vec![1; len],
            root: (0..len).collect(),
        }
    }

    fn representative(&mut self, mut index: usize) -> usize {
        while self.set[index] != index {
            self.set[index] = self.set[self.set[index]];
            index = self.set[index];
        }
        index
    }

    fn root(&mut self, index: usize) -> usize {
        let representative = self.representative(index);
        self.root[representative]
    }

    /// `child` must be the root of its tree.
    fn attach(&mut self, child: usize, parent: usize) {
        let child_set = self.representative(child);
        let parent_set = self.representative(parent);
        let root = self.root[parent_set];
        let (small, large) = if self.size[child_set] < self.size[parent_set] {
            (child_set, parent_set)
        } else {
            (parent_set, child_set)
        };
        self.set[small] = large;
        self.size[large] += self.size[small];
        self.root[large] = root;
    }
}

enum OutlineLabel {
    Instance(usize),
    Bucket(String),
}

struct OutlineNode {
    label: OutlineLabel,
    children: Vec<usize>,
}

struct OutlineBuilder<'a> {
    instances: &'a EntityInstanceCollection,
    classes: &'a [Option<Class>],
    children: &'a HashMap<usize, Vec<usize>>,
    nodes: Vec<OutlineNode>,
    unexpanded: Vec<(usize, usize)>,
}

impl OutlineBuilder<'_> {
    fn add(&mut self, label: OutlineLabel) -> usize {
        self.nodes.push(OutlineNode {
            label,
            children: Vec::new(),
        });
        self.nodes.len() - 1
    }

    fn add_instance(&mut self, index: usize) -> usize {
        let node = self.add(OutlineLabel::Instance(index));
        self.unexpanded.push((node, index));
        node
    }

    fn expand(&mut self) {
        let classes = self.classes;
        while let Some((node, index)) = self.unexpanded.pop() {
            let all_children = self.children.get(&index).map_or(&[][..], Vec::as_slice);
            let mut nested: Vec<usize> = all_children
                .iter()
                .copied()
                .filter(|child| matches!(classes[*child], Some(Class::Project | Class::Spatial)))
                .map(|child| self.add_instance(child))
                .collect();
            let products: Vec<usize> = all_children
                .iter()
                .copied()
                .filter(|child| classes[*child] == Some(Class::Product))
                .collect();
            nested.extend(self.add_product_groups(&products, product_render_mode(classes[index])));
            self.nodes[node].children = nested;
        }
    }

    fn add_product_groups(&mut self, indices: &[usize], mode: ProductRenderMode) -> Vec<usize> {
        if matches!(mode, ProductRenderMode::Direct) {
            return indices
                .iter()
                .map(|&index| self.add_instance(index))
                .collect();
        }
        let instances = self.instances;
        let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for &index in indices {
            groups
                .entry(&instances.instances[index].entity_name)
                .or_default()
                .push(index);
        }
        let mut result = Vec::new();
        for (entity, group) in groups {
            if let [index] = group[..] {
                result.push(self.add_instance(index));
            } else {
                let bucket = self.add(OutlineLabel::Bucket(format!("{entity} ({})", group.len())));
                self.nodes[bucket].children = group
                    .into_iter()
                    .map(|index| self.add_instance(index))
                    .collect();
                result.push(bucket);
            }
        }
        result
    }
}

/// Symbols at `MAX_OUTLINE_DEPTH` have no children: when a node one level above it has deeper
/// descendants, all its descendant instances become its direct children in document order.
fn outline_symbols(
    nodes: &[OutlineNode],
    top: &[usize],
    symbol: impl Fn(&OutlineLabel, Vec<DocumentSymbol>) -> DocumentSymbol,
) -> Vec<DocumentSymbol> {
    let mut visit = Vec::new();
    let mut stack: Vec<(usize, usize)> = top.iter().map(|&node| (node, 1)).collect();
    while let Some((node, level)) = stack.pop() {
        visit.push((node, level));
        if level + 1 < MAX_OUTLINE_DEPTH {
            stack.extend(nodes[node].children.iter().map(|&child| (child, level + 1)));
        }
    }
    let mut built: Vec<Option<DocumentSymbol>> = (0..nodes.len()).map(|_| None).collect();
    for &(node, level) in visit.iter().rev() {
        let children = &nodes[node].children;
        let symbols = if level + 1 < MAX_OUTLINE_DEPTH {
            children
                .iter()
                .map(|&child| built[child].take().expect("children are built first"))
                .collect()
        } else if children
            .iter()
            .all(|&child| nodes[child].children.is_empty())
        {
            children
                .iter()
                .map(|&child| symbol(&nodes[child].label, Vec::new()))
                .collect()
        } else {
            let mut descendants = Vec::new();
            let mut pending = children.clone();
            while let Some(descendant) = pending.pop() {
                if let OutlineLabel::Instance(index) = nodes[descendant].label {
                    descendants.push((index, descendant));
                }
                pending.extend(&nodes[descendant].children);
            }
            descendants.sort_unstable();
            descendants
                .into_iter()
                .map(|(_, descendant)| symbol(&nodes[descendant].label, Vec::new()))
                .collect()
        };
        built[node] = Some(symbol(&nodes[node].label, symbols));
    }
    top.iter()
        .map(|&node| built[node].take().expect("top-level nodes are built"))
        .collect()
}

fn product_render_mode(parent_class: Option<Class>) -> ProductRenderMode {
    match parent_class {
        Some(Class::Product) => ProductRenderMode::Direct,
        _ => ProductRenderMode::GroupMultiOnly,
    }
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
