//! Lightweight STEP and schema-aware completions.
//! Completion context is derived from document text and schema docs without loading AST state.

use std::collections::{HashMap, HashSet};

use tower_lsp::lsp_types::{
    Command, CompletionItem, CompletionItemKind, CompletionTextEdit, Documentation,
    InsertTextFormat, MarkupContent, MarkupKind, Position, Range, TextEdit, Url,
};

use crate::document::{Document, is_identifier_part};
use crate::schema::{
    AggregateKind, AggregateTypeRef, BoundValue, EntityDoc, NamedTypeKind, SchemaDoc, TypeDoc,
    TypeRef, normalize_name,
};

use super::hover::{render_entity_hover, render_reference_hover};
use super::ifc_guid::{IFC_GUID_ALPHABET, new_guid};
use super::scaffold_completions::starts_with_ignore_ascii_case;
use super::step_scan::{entity_argument_context, scan_definition_entity_name};

const MAX_REFERENCE_COMPLETIONS: usize = 100;
const TRIGGER_PARAMETER_HINTS_COMMAND: &str = "editor.action.triggerParameterHints";

pub struct CompletionResults {
    pub items: Vec<CompletionItem>,
    pub is_incomplete: bool,
}

pub fn completions(
    document: &Document,
    uri: &Url,
    position: Position,
    schema_name: Option<&str>,
    schema: Option<&SchemaDoc>,
    snippet_supported: bool,
    eager_documentation: bool,
) -> Option<CompletionResults> {
    let offset = document.position_to_offset(position)?;
    let mut items = Vec::new();
    let mut is_incomplete = false;

    if let Some(item) = next_id_completion(document, position, offset) {
        items.push(item);
    }

    if let Some(schema) = schema {
        items.extend(entity_name_completions(
            document,
            position,
            offset,
            schema_name,
            schema,
            snippet_supported,
            eager_documentation,
        ));
        let argument_results = argument_completions(
            document,
            uri,
            offset,
            schema,
            snippet_supported,
            eager_documentation,
        );
        items.extend(argument_results.items);
        is_incomplete |= argument_results.is_incomplete;
    }

    (!items.is_empty()).then_some(CompletionResults {
        items,
        is_incomplete,
    })
}

fn next_id_completion(
    document: &Document,
    position: Position,
    offset: usize,
) -> Option<CompletionItem> {
    if entity_argument_context(&document.text, &document.definitions, offset).is_some() {
        return None;
    }

    let line_start = *document.line_offsets.get(position.line as usize)?;
    let line_end = document.line_end_offset(position.line as usize)?;
    let line_prefix = document.text.get(line_start..offset)?;
    let line_suffix = document.text.get(offset..line_end)?;
    if !line_suffix.trim().is_empty() {
        return None;
    }
    let candidate = line_prefix.trim_start();
    let replace_start = if candidate.is_empty() {
        offset
    } else if candidate
        .strip_prefix('#')
        .is_some_and(|digits| digits.bytes().all(|byte| byte.is_ascii_digit()))
    {
        line_start + line_prefix.len().checked_sub(candidate.len())?
    } else {
        return None;
    };

    let next_id = document
        .definitions
        .keys()
        .copied()
        .max()
        .map_or(Some(1), |id| id.checked_add(1))?;
    let range = document.range_for_offsets(replace_start, offset)?;
    let mut item = completion_item(
        format!("#{next_id}="),
        "Next STEP instance id",
        CompletionItemKind::VALUE,
        format!("#{next_id}="),
        range,
        "000_next_id".to_string(),
    );
    item.filter_text = Some(format!("#{next_id}"));
    item.insert_text_format = Some(InsertTextFormat::PLAIN_TEXT);
    Some(item)
}

fn entity_name_completions(
    document: &Document,
    position: Position,
    offset: usize,
    schema_name: Option<&str>,
    schema: &SchemaDoc,
    snippet_supported: bool,
    eager_documentation: bool,
) -> Vec<CompletionItem> {
    let Some((prefix, range, can_insert_snippet)) = entity_name_request(document, position, offset)
    else {
        return Vec::new();
    };

    let mut names = schema
        .entities
        .keys()
        .filter(|name| starts_with_ignore_ascii_case(name, &prefix))
        .cloned()
        .collect::<Vec<_>>();
    names.sort();
    names
        .into_iter()
        .enumerate()
        .filter_map(|(index, name)| {
            let entity = schema.entity(&name)?;
            let use_snippet = snippet_supported && can_insert_snippet;
            let new_text = if use_snippet {
                render_entity_snippet(entity, schema)
            } else {
                name.clone()
            };
            let mut item = completion_item(
                name.clone(),
                &render_entity_signature(entity),
                CompletionItemKind::CLASS,
                new_text,
                range,
                format!("{index:06}_entity"),
            );
            item.filter_text = Some(name);
            item.insert_text_format = Some(if use_snippet {
                InsertTextFormat::SNIPPET
            } else {
                InsertTextFormat::PLAIN_TEXT
            });
            if use_snippet {
                item.command = Some(Command::new(
                    "Show IFC entity signature".to_string(),
                    TRIGGER_PARAMETER_HINTS_COMMAND.to_string(),
                    None,
                ));
            }
            if let Some(schema_name) = schema_name {
                item.data = Some(serde_json::json!({
                    "kind": "entity",
                    "schema": schema_name,
                    "entity": entity.name,
                }));
            }
            if eager_documentation {
                item.documentation = Some(markdown_documentation(render_entity_hover(entity)));
            }
            Some(item)
        })
        .collect()
}

fn entity_name_request(
    document: &Document,
    position: Position,
    offset: usize,
) -> Option<(String, Range, bool)> {
    let line_start = *document.line_offsets.get(position.line as usize)?;
    let bytes = document.text.as_bytes();
    let mut prefix_start = offset;
    while prefix_start > line_start && is_identifier_part(bytes[prefix_start - 1]) {
        prefix_start -= 1;
    }
    let prefix = document
        .text
        .get(prefix_start..offset)?
        .to_ascii_uppercase();

    let mut before = prefix_start;
    while before > line_start && bytes[before - 1].is_ascii_whitespace() {
        before -= 1;
    }
    if before == line_start || bytes[before - 1] != b'=' {
        return None;
    }
    before -= 1;
    while before > line_start && bytes[before - 1].is_ascii_whitespace() {
        before -= 1;
    }
    let digit_end = before;
    while before > line_start && bytes[before - 1].is_ascii_digit() {
        before -= 1;
    }
    if before == digit_end || before == line_start || bytes[before - 1] != b'#' {
        return None;
    }

    let line_end = document.line_end_offset(position.line as usize)?;
    let can_insert_snippet = document.text.get(offset..line_end)?.trim().is_empty();
    Some((
        prefix,
        document.range_for_offsets(prefix_start, offset)?,
        can_insert_snippet,
    ))
}

fn argument_completions(
    document: &Document,
    uri: &Url,
    offset: usize,
    schema: &SchemaDoc,
    snippet_supported: bool,
    eager_documentation: bool,
) -> CompletionResults {
    let Some(context) = entity_argument_context(&document.text, &document.definitions, offset)
    else {
        return empty_results();
    };
    let Some(entity) = schema.entity(&context.entity_name) else {
        return empty_results();
    };
    let Some(attribute) = entity.attributes.get(context.active_parameter) else {
        return empty_results();
    };
    let Some(token) = argument_token(document, context.value_start, offset) else {
        return empty_results();
    };

    if type_is_guid(
        schema,
        &attribute.ty,
        context.aggregate_depth,
        &mut HashSet::new(),
    ) {
        return complete_results(guid_completions(token));
    }

    if let Some(values) = enumeration_values(
        schema,
        &attribute.ty,
        context.aggregate_depth,
        &mut HashSet::new(),
    ) {
        return complete_results(enumeration_completions(values, token));
    }

    if context.aggregate_depth == 0
        && snippet_supported
        && token.text.is_empty()
        && let Some(aggregate) = resolve_aggregate(schema, &attribute.ty, &mut HashSet::new())
    {
        return complete_results(vec![aggregate_completion(aggregate, schema, token.range)]);
    }

    let mut expected_entities = HashSet::new();
    let mut accepts_any_entity = false;
    collect_entity_constraints(
        schema,
        &attribute.ty,
        context.aggregate_depth,
        &mut HashSet::new(),
        &mut expected_entities,
        &mut accepts_any_entity,
    );
    let (items, is_incomplete) = reference_completions(
        document,
        uri,
        schema,
        &expected_entities,
        accepts_any_entity,
        token,
        eager_documentation,
    );
    CompletionResults {
        items,
        is_incomplete,
    }
}

fn empty_results() -> CompletionResults {
    CompletionResults {
        items: Vec::new(),
        is_incomplete: false,
    }
}

fn complete_results(items: Vec<CompletionItem>) -> CompletionResults {
    CompletionResults {
        items,
        is_incomplete: false,
    }
}

#[derive(Clone, Copy)]
struct ArgumentToken<'a> {
    text: &'a str,
    range: Range,
}

fn argument_token(
    document: &Document,
    value_start: usize,
    cursor_offset: usize,
) -> Option<ArgumentToken<'_>> {
    let raw = document.text.get(value_start..cursor_offset)?;
    let leading = raw.len().checked_sub(raw.trim_start().len())?;
    let token_start = value_start + leading;
    let text = document.text.get(token_start..cursor_offset)?;
    if text.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return None;
    }

    Some(ArgumentToken {
        text,
        range: document.range_for_offsets(token_start, cursor_offset)?,
    })
}

fn guid_completions(token: ArgumentToken<'_>) -> Vec<CompletionItem> {
    let valid_partial = token.text.is_empty()
        || token.text.strip_prefix('\'').is_some_and(|value| {
            !value.contains('\'') && value.bytes().all(|byte| IFC_GUID_ALPHABET.contains(&byte))
        });
    if !valid_partial {
        return Vec::new();
    }

    let guid = format!("'{}'", new_guid());
    vec![completion_item(
        "Generate fresh IFC GUID".to_string(),
        "Fresh compressed UUID v4",
        CompletionItemKind::VALUE,
        guid,
        token.range,
        "000_guid".to_string(),
    )]
}

fn enumeration_completions(values: &[String], token: ArgumentToken<'_>) -> Vec<CompletionItem> {
    let prefix = if token.text.is_empty() {
        ""
    } else if let Some(value) = token.text.strip_prefix('.') {
        if value.contains('.') || !value.bytes().all(is_identifier_part) {
            return Vec::new();
        }
        value
    } else {
        return Vec::new();
    };

    let mut values = values.to_vec();
    values.sort();
    values
        .into_iter()
        .filter(|value| starts_with_ignore_ascii_case(value, prefix))
        .enumerate()
        .map(|(index, value)| {
            let step_value = format!(".{value}.");
            completion_item(
                step_value.clone(),
                "IFC enumeration value",
                CompletionItemKind::ENUM_MEMBER,
                step_value,
                token.range,
                format!("{index:06}_enum"),
            )
        })
        .collect()
}

fn reference_completions(
    document: &Document,
    uri: &Url,
    schema: &SchemaDoc,
    expected_entities: &HashSet<String>,
    accepts_any_entity: bool,
    token: ArgumentToken<'_>,
    eager_documentation: bool,
) -> (Vec<CompletionItem>, bool) {
    if !accepts_any_entity && expected_entities.is_empty() {
        return (Vec::new(), false);
    }
    let prefix = if token.text.is_empty() {
        ""
    } else if let Some(value) = token.text.strip_prefix('#') {
        if !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return (Vec::new(), false);
        }
        value
    } else {
        return (Vec::new(), false);
    };

    let mut definitions = document
        .definitions
        .iter()
        .map(|(id, offset)| (*id, *offset))
        .filter(|(id, _)| id.to_string().starts_with(prefix))
        .collect::<Vec<_>>();
    definitions.sort_by_key(|(id, _)| *id);

    let mut compatible_by_name = HashMap::<&str, bool>::new();
    let mut matching = definitions
        .into_iter()
        .filter_map(|(id, offset)| {
            let entity_name =
                scan_definition_entity_name(&document.text, offset, document.text.len())?;
            let compatible = accepts_any_entity
                || *compatible_by_name.entry(entity_name).or_insert_with(|| {
                    expected_entities
                        .iter()
                        .any(|expected| schema.is_entity_compatible(entity_name, expected))
                });
            if !compatible {
                return None;
            }

            let entity_name = normalize_name(entity_name);
            let reference = format!("#{id}");
            let mut item = completion_item(
                reference.clone(),
                &entity_name,
                CompletionItemKind::REFERENCE,
                reference,
                token.range,
                format!("{id:010}_reference"),
            );
            item.data = Some(serde_json::json!({
                "kind": "reference",
                "uri": uri,
                "id": id,
            }));
            if eager_documentation && let Some(documentation) = render_reference_hover(document, id)
            {
                item.documentation = Some(markdown_documentation(documentation));
            }
            Some(item)
        })
        .take(MAX_REFERENCE_COMPLETIONS + 1)
        .collect::<Vec<_>>();
    let is_incomplete = matching.len() > MAX_REFERENCE_COMPLETIONS;
    matching.truncate(MAX_REFERENCE_COMPLETIONS);
    (matching, is_incomplete)
}

fn render_entity_signature(entity: &EntityDoc) -> String {
    format!(
        "{}({})",
        entity.name,
        entity
            .attributes
            .iter()
            .map(|attribute| attribute.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn render_entity_snippet(entity: &EntityDoc, schema: &SchemaDoc) -> String {
    let mut next_tabstop = 1usize;
    let arguments = entity
        .attributes
        .iter()
        .map(|attribute| {
            if type_is_guid(schema, &attribute.ty, 0, &mut HashSet::new()) {
                format!("'{}'", new_guid())
            } else if attribute.allows_omitted {
                "*".to_string()
            } else if attribute.optional {
                snippet_tabstop(&mut next_tabstop, Some("$"))
            } else {
                render_type_snippet(
                    schema,
                    &attribute.ty,
                    &mut next_tabstop,
                    &mut HashSet::new(),
                )
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{}({arguments});$0", entity.name.to_ascii_uppercase())
}

fn aggregate_completion(
    aggregate: &AggregateTypeRef,
    schema: &SchemaDoc,
    range: Range,
) -> CompletionItem {
    let mut next_tabstop = 1usize;
    let new_text =
        render_aggregate_snippet(schema, aggregate, &mut next_tabstop, &mut HashSet::new());
    let mut item = completion_item(
        format!("Insert {} value", aggregate_kind_name(aggregate.kind)),
        &format_aggregate_detail(aggregate),
        CompletionItemKind::SNIPPET,
        new_text,
        range,
        "000_aggregate".to_string(),
    );
    item.insert_text_format = Some(InsertTextFormat::SNIPPET);
    item.documentation = Some(markdown_documentation(format!(
        "Schema type: `{}`",
        format_aggregate_detail(aggregate)
    )));
    item
}

/// Marks `name` as visited for alias-cycle detection; un-marks it automatically on drop
/// (including on early return), replacing a hand-copied insert/match/remove pattern.
struct CycleGuard<'a> {
    visiting: &'a mut HashSet<String>,
    name: String,
}

impl<'a> CycleGuard<'a> {
    /// Returns `None` if `name` is already being visited (cycle detected).
    fn enter(visiting: &'a mut HashSet<String>, name: String) -> Option<Self> {
        visiting
            .insert(name.clone())
            .then(|| Self { visiting, name })
    }

    fn visiting(&mut self) -> &mut HashSet<String> {
        self.visiting
    }
}

impl Drop for CycleGuard<'_> {
    fn drop(&mut self) {
        self.visiting.remove(&self.name);
    }
}

fn render_type_snippet(
    schema: &SchemaDoc,
    ty: &TypeRef,
    next_tabstop: &mut usize,
    visiting: &mut HashSet<String>,
) -> String {
    match ty {
        TypeRef::Aggregate(aggregate) => {
            render_aggregate_snippet(schema, aggregate, next_tabstop, visiting)
        }
        TypeRef::Named(named) if named.kind != NamedTypeKind::Entity => {
            let name = normalize_name(&named.name);
            let Some(mut guard) = CycleGuard::enter(visiting, name.clone()) else {
                return snippet_tabstop(next_tabstop, None);
            };
            match schema.type_decl(&name) {
                Some(TypeDoc::Alias(alias)) => {
                    render_type_snippet(schema, &alias.target, next_tabstop, guard.visiting())
                }
                _ => snippet_tabstop(next_tabstop, None),
            }
        }
        _ => snippet_tabstop(next_tabstop, None),
    }
}

fn render_aggregate_snippet(
    schema: &SchemaDoc,
    aggregate: &AggregateTypeRef,
    next_tabstop: &mut usize,
    visiting: &mut HashSet<String>,
) -> String {
    let count = aggregate_placeholder_count(aggregate);
    let values = (0..count)
        .map(|_| render_type_snippet(schema, &aggregate.item, next_tabstop, visiting))
        .collect::<Vec<_>>()
        .join(",");
    format!("({values})")
}

fn aggregate_placeholder_count(aggregate: &AggregateTypeRef) -> usize {
    let required = match (&aggregate.kind, &aggregate.bounds) {
        (AggregateKind::Array, Some(bounds)) => match (&bounds.lower, &bounds.upper) {
            (BoundValue::Integer(lower), BoundValue::Integer(upper)) if upper >= lower => {
                upper.saturating_sub(*lower).saturating_add(1) as usize
            }
            _ => 1,
        },
        (_, Some(bounds)) => match bounds.lower {
            BoundValue::Integer(lower) => lower as usize,
            BoundValue::Unbounded | BoundValue::UnsupportedExpression => 1,
        },
        (_, None) => 1,
    };
    required.clamp(1, 8)
}

fn snippet_tabstop(next_tabstop: &mut usize, default: Option<&str>) -> String {
    let index = *next_tabstop;
    *next_tabstop += 1;
    match default {
        Some(default) => format!("${{{index}:\\{default}}}"),
        None => format!("${{{index}}}"),
    }
}

fn resolve_aggregate<'a>(
    schema: &'a SchemaDoc,
    ty: &'a TypeRef,
    visiting: &mut HashSet<String>,
) -> Option<&'a AggregateTypeRef> {
    match ty {
        TypeRef::Aggregate(aggregate) => Some(aggregate),
        TypeRef::Named(named) if named.kind != NamedTypeKind::Entity => {
            let name = normalize_name(&named.name);
            let mut guard = CycleGuard::enter(visiting, name.clone())?;
            match schema.type_decl(&name) {
                Some(TypeDoc::Alias(alias)) => {
                    resolve_aggregate(schema, &alias.target, guard.visiting())
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn aggregate_kind_name(kind: AggregateKind) -> &'static str {
    match kind {
        AggregateKind::Set => "SET",
        AggregateKind::Bag => "BAG",
        AggregateKind::List => "LIST",
        AggregateKind::Array => "ARRAY",
    }
}

fn format_aggregate_detail(aggregate: &AggregateTypeRef) -> String {
    let bounds = aggregate
        .bounds
        .as_ref()
        .map_or_else(String::new, |bounds| {
            format!(
                " [{}:{}]",
                format_bound(&bounds.lower),
                format_bound(&bounds.upper)
            )
        });
    let unique = if aggregate.unique { " UNIQUE" } else { "" };
    format!("{}{bounds}{unique}", aggregate_kind_name(aggregate.kind))
}

fn format_bound(bound: &BoundValue) -> String {
    match bound {
        BoundValue::Integer(value) => value.to_string(),
        BoundValue::Unbounded | BoundValue::UnsupportedExpression => "?".to_string(),
    }
}

fn markdown_documentation(value: String) -> Documentation {
    Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value,
    })
}

fn type_is_guid(
    schema: &SchemaDoc,
    ty: &TypeRef,
    aggregate_depth: usize,
    visiting: &mut HashSet<String>,
) -> bool {
    match ty {
        TypeRef::Aggregate(aggregate) if aggregate_depth > 0 => {
            type_is_guid(schema, &aggregate.item, aggregate_depth - 1, visiting)
        }
        TypeRef::Aggregate(_) => false,
        TypeRef::Named(named) => {
            let name = normalize_name(&named.name);
            if aggregate_depth == 0 && name == "IFCGLOBALLYUNIQUEID" {
                return true;
            }
            let Some(mut guard) = CycleGuard::enter(visiting, name.clone()) else {
                return false;
            };
            match schema.type_decl(&name) {
                Some(TypeDoc::Alias(alias)) => {
                    type_is_guid(schema, &alias.target, aggregate_depth, guard.visiting())
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn enumeration_values<'a>(
    schema: &'a SchemaDoc,
    ty: &'a TypeRef,
    aggregate_depth: usize,
    visiting: &mut HashSet<String>,
) -> Option<&'a [String]> {
    match ty {
        TypeRef::Aggregate(aggregate) if aggregate_depth > 0 => {
            enumeration_values(schema, &aggregate.item, aggregate_depth - 1, visiting)
        }
        TypeRef::Aggregate(_) => None,
        TypeRef::Named(named) if named.kind != NamedTypeKind::Entity => {
            let name = normalize_name(&named.name);
            let mut guard = CycleGuard::enter(visiting, name.clone())?;
            match schema.type_decl(&name) {
                Some(TypeDoc::Alias(alias)) => {
                    enumeration_values(schema, &alias.target, aggregate_depth, guard.visiting())
                }
                Some(TypeDoc::Enumeration(enumeration)) if aggregate_depth == 0 => {
                    Some(enumeration.items.as_slice())
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn collect_entity_constraints(
    schema: &SchemaDoc,
    ty: &TypeRef,
    aggregate_depth: usize,
    visiting: &mut HashSet<String>,
    expected: &mut HashSet<String>,
    accepts_any: &mut bool,
) {
    match ty {
        TypeRef::Aggregate(aggregate) if aggregate_depth > 0 => collect_entity_constraints(
            schema,
            &aggregate.item,
            aggregate_depth - 1,
            visiting,
            expected,
            accepts_any,
        ),
        TypeRef::Aggregate(_) => {}
        TypeRef::GenericEntity { .. } if aggregate_depth == 0 => *accepts_any = true,
        TypeRef::Named(named) if aggregate_depth == 0 => {
            let name = normalize_name(&named.name);
            if named.kind == NamedTypeKind::Entity || schema.entity(&name).is_some() {
                expected.insert(name);
                return;
            }
            let Some(mut guard) = CycleGuard::enter(visiting, name.clone()) else {
                return;
            };
            match schema.type_decl(&name) {
                Some(TypeDoc::Alias(alias)) => collect_entity_constraints(
                    schema,
                    &alias.target,
                    0,
                    guard.visiting(),
                    expected,
                    accepts_any,
                ),
                Some(TypeDoc::Select(select)) => {
                    if select.generic_entity {
                        *accepts_any = true;
                    }
                    for option in &select.options {
                        collect_entity_constraints(
                            schema,
                            option,
                            0,
                            guard.visiting(),
                            expected,
                            accepts_any,
                        );
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn completion_item(
    label: String,
    detail: &str,
    kind: CompletionItemKind,
    new_text: String,
    range: Range,
    sort_text: String,
) -> CompletionItem {
    let mut item = CompletionItem::new_simple(label, detail.to_string());
    item.kind = Some(kind);
    item.sort_text = Some(sort_text);
    item.text_edit = Some(CompletionTextEdit::Edit(TextEdit::new(range, new_text)));
    item
}

pub fn resolution_uri(item: &CompletionItem) -> Option<Url> {
    item.data.as_ref()?.get("uri")?.as_str()?.parse().ok()
}

pub fn resolution_schema_name(item: &CompletionItem) -> Option<&str> {
    item.data.as_ref()?.get("schema")?.as_str()
}

pub fn resolve_completion_item(
    mut item: CompletionItem,
    document: Option<&Document>,
    schema: Option<&SchemaDoc>,
) -> CompletionItem {
    let Some(data) = item.data.as_ref() else {
        return item;
    };
    match data.get("kind").and_then(serde_json::Value::as_str) {
        Some("entity") => {
            let Some(entity_name) = data.get("entity").and_then(serde_json::Value::as_str) else {
                return item;
            };
            if let Some(entity) = schema.and_then(|schema| schema.entity(entity_name)) {
                item.documentation = Some(markdown_documentation(render_entity_hover(entity)));
            }
        }
        Some("reference") => {
            let Some(id) = data
                .get("id")
                .and_then(serde_json::Value::as_u64)
                .and_then(|id| u32::try_from(id).ok())
            else {
                return item;
            };
            if let Some(documentation) =
                document.and_then(|document| render_reference_hover(document, id))
            {
                item.documentation = Some(markdown_documentation(documentation));
            }
        }
        _ => {}
    }
    item
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use crate::schema::{
        AggregateKind, AggregateTypeRef, AliasTypeDef, EntityAttributeDoc, EntityDoc,
        EnumerationTypeDef, NamedTypeRef, SelectTypeDef,
    };

    use super::*;

    fn named(name: &str, kind: NamedTypeKind) -> TypeRef {
        TypeRef::Named(NamedTypeRef {
            name: name.to_string(),
            kind,
        })
    }

    fn attribute(name: &str, ty: TypeRef) -> EntityAttributeDoc {
        EntityAttributeDoc {
            name: name.to_string(),
            type_name: name.to_string(),
            declared_in: "IfcWall".to_string(),
            ty,
            optional: false,
            allows_omitted: false,
        }
    }

    fn entity(name: &str, attributes: Vec<EntityAttributeDoc>, supers: &[&str]) -> EntityDoc {
        EntityDoc {
            name: name.to_string(),
            attributes,
            url: String::new(),
            all_supertypes: supers.iter().map(|name| normalize_name(name)).collect(),
        }
    }

    fn schema() -> SchemaDoc {
        let aggregate = TypeRef::Aggregate(AggregateTypeRef {
            kind: AggregateKind::Set,
            bounds: None,
            item: Box::new(named("IfcRoot", NamedTypeKind::Entity)),
            unique: false,
            optional: false,
        });
        SchemaDoc {
            entities: HashMap::from([
                (
                    "IFCWALL".to_string(),
                    entity(
                        "IfcWall",
                        vec![
                            attribute(
                                "GlobalId",
                                named("IfcGloballyUniqueId", NamedTypeKind::Type),
                            ),
                            attribute(
                                "OwnerHistory",
                                named("IfcOwnerHistory", NamedTypeKind::Entity),
                            ),
                            attribute(
                                "PredefinedType",
                                named("IfcWallTypeEnum", NamedTypeKind::Type),
                            ),
                            attribute("Related", aggregate),
                            attribute("Choice", named("IfcRootSelect", NamedTypeKind::Type)),
                        ],
                        &["IfcRoot"],
                    ),
                ),
                (
                    "IFCWALLSTANDARDCASE".to_string(),
                    entity("IfcWallStandardCase", Vec::new(), &["IfcWall", "IfcRoot"]),
                ),
                (
                    "IFCDOOR".to_string(),
                    entity("IfcDoor", Vec::new(), &["IfcRoot"]),
                ),
                (
                    "IFCOWNERHISTORY".to_string(),
                    entity("IfcOwnerHistory", Vec::new(), &[]),
                ),
            ]),
            types: HashMap::from([
                (
                    "IFCGLOBALLYUNIQUEID".to_string(),
                    TypeDoc::Alias(AliasTypeDef {
                        name: "IfcGloballyUniqueId".to_string(),
                        target: TypeRef::Primitive(crate::schema::PrimitiveType::String {
                            width: Some(22),
                            fixed: true,
                        }),
                    }),
                ),
                (
                    "IFCWALLTYPEENUM".to_string(),
                    TypeDoc::Enumeration(EnumerationTypeDef {
                        name: "IfcWallTypeEnum".to_string(),
                        items: vec!["USERDEFINED".to_string(), "SOLIDWALL".to_string()],
                        extensible: true,
                    }),
                ),
                (
                    "IFCROOTSELECT".to_string(),
                    TypeDoc::Select(SelectTypeDef {
                        name: "IfcRootSelect".to_string(),
                        options: vec![named("IfcWall", NamedTypeKind::Entity)],
                        extensible: false,
                        generic_entity: false,
                    }),
                ),
            ]),
        }
    }

    fn items(text: &str, schema: Option<&SchemaDoc>) -> Vec<CompletionItem> {
        results(text, schema, false, true).items
    }

    fn results(
        text: &str,
        schema: Option<&SchemaDoc>,
        snippet_supported: bool,
        eager_documentation: bool,
    ) -> CompletionResults {
        let document = Document::new_unloaded(text.to_string());
        let position = document
            .offset_to_position(text.len())
            .expect("end position should be valid");
        completions(
            &document,
            &Url::parse("file:///test.ifc").unwrap(),
            position,
            Some("IFC4"),
            schema,
            snippet_supported,
            eager_documentation,
        )
        .unwrap_or_else(empty_results)
    }

    fn new_text(item: &CompletionItem) -> &str {
        match item.text_edit.as_ref().expect("expected text edit") {
            CompletionTextEdit::Edit(edit) => &edit.new_text,
            CompletionTextEdit::InsertAndReplace(_) => panic!("expected plain text edit"),
        }
    }

    #[test]
    fn suggests_next_definition_prefix_on_empty_line() {
        let results = items("#1=IFCWALL($);\n#4=IFCDOOR($);\n", None);
        assert_eq!(new_text(&results[0]), "#5=");
    }

    #[test]
    fn suggests_first_definition_prefix_without_definitions() {
        let results = items("", None);
        assert_eq!(new_text(&results[0]), "#1=");
    }

    #[test]
    fn replaces_typed_id_prefix_without_adding_whitespace() {
        for prefix in ["#", "#1"] {
            let text = format!("#1=IFCWALL($);\n#16=IFCDOOR($);\n  {prefix}");
            let results = items(&text, None);

            assert_eq!(results.len(), 1);
            assert_eq!(new_text(&results[0]), "#17=");
            assert!(!new_text(&results[0]).contains(' '));
            let range = match results[0].text_edit.as_ref().unwrap() {
                CompletionTextEdit::Edit(edit) => edit.range,
                CompletionTextEdit::InsertAndReplace(_) => panic!("expected plain text edit"),
            };
            assert_eq!(range.start.character, 2);
        }
    }

    #[test]
    fn completes_entity_names_in_definition_position() {
        let results = items("#1=ifcw", Some(&schema()));
        let edits = results.iter().map(new_text).collect::<Vec<_>>();
        assert_eq!(edits, ["IFCWALL", "IFCWALLSTANDARDCASE"]);
    }

    #[test]
    fn entity_completion_edit_range_uses_utf16_positions() {
        let text = "/* 😀 */ #1=IFCW";
        let results = items(text, Some(&schema()));
        let range = match results[0].text_edit.as_ref().unwrap() {
            CompletionTextEdit::Edit(edit) => edit.range,
            CompletionTextEdit::InsertAndReplace(_) => panic!("expected plain text edit"),
        };

        assert_eq!(range.start.character, 12);
        assert_eq!(range.end.character, 16);
    }

    #[test]
    fn generates_quoted_guid_for_empty_and_partial_values() {
        for text in ["#1=IFCWALL(", "#1=IFCWALL('abc"] {
            let results = items(text, Some(&schema()));
            let value = new_text(&results[0]);
            assert_eq!(value.len(), 24);
            assert!(value.starts_with('\'') && value.ends_with('\''));
            assert!(
                value[1..23]
                    .bytes()
                    .all(|byte| IFC_GUID_ALPHABET.contains(&byte))
            );
        }
    }

    #[test]
    fn does_not_replace_complete_or_null_guid_values() {
        assert!(items("#1=IFCWALL('0123456789012345678901'", Some(&schema())).is_empty());
        assert!(items("#1=IFCWALL($", Some(&schema())).is_empty());
    }

    #[test]
    fn completes_enumeration_prefix() {
        let results = items("#1=IFCWALL('done',$,.SO", Some(&schema()));
        assert_eq!(results.len(), 1);
        assert_eq!(new_text(&results[0]), ".SOLIDWALL.");
    }

    #[test]
    fn completes_after_a_leading_argument_comment() {
        let results = items(
            "#1=IFCWALL('done',$, /* explanation */ .SO",
            Some(&schema()),
        );
        assert_eq!(results.len(), 1);
        assert_eq!(new_text(&results[0]), ".SOLIDWALL.");
    }

    #[test]
    fn completes_compatible_references_inside_aggregate() {
        let text = "#1=IFCWALL($);#2=IFCDOOR($);#3=IFCOWNERHISTORY($);#4=IFCWALL($,$,$,(#";
        let results = items(text, Some(&schema()));
        let labels = results
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, ["#1", "#2", "#4"]);
    }

    #[test]
    fn filters_select_references_to_compatible_entities() {
        let text = "#1=IFCWALL($);#2=IFCDOOR($);#3=IFCWALL($,$,$,$,#";
        let results = items(text, Some(&schema()));
        assert_eq!(
            results.iter().map(new_text).collect::<Vec<_>>(),
            ["#1", "#3"]
        );
    }

    #[test]
    fn suggests_owner_history_after_comma_and_hash_trigger() {
        for suffix in ["#17=IFCWALL('fresh',", "#17=IFCWALL('fresh', #"] {
            let text = format!("#1=IFCPROJECT($);#2=IFCOWNERHISTORY($);#3=IFCDOOR($);{suffix}");
            let results = items(&text, Some(&schema()));

            assert_eq!(results.len(), 1);
            assert_eq!(results[0].label, "#2");
            assert_eq!(new_text(&results[0]), "#2");
            assert!(!new_text(&results[0]).contains(' '));
        }
    }

    #[test]
    fn bundled_ifc4x3_schema_suggests_owner_history_for_wall() {
        let schemas = crate::schema::SchemaDocCollection::new();
        let schema = schemas
            .get("IFC4X3_ADD2")
            .expect("bundled IFC4X3 schema should load");
        let text = "#2=IFCOWNERHISTORY($,$,$,$,$,$,$,$);#17=IFCWALL('fresh', #";
        let results = items(text, Some(schema));

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].label, "#2");
        assert_eq!(new_text(&results[0]), "#2");
    }

    #[test]
    fn snippet_capable_client_gets_complete_entity_construction() {
        let results = results("#18=IFCW", Some(&schema()), true, false);
        let wall = results
            .items
            .iter()
            .find(|item| item.label == "IFCWALL")
            .expect("expected wall completion");
        let text = new_text(wall);

        assert!(text.starts_with("IFCWALL('") && text.ends_with(");$0"));
        assert!(text.contains("',${1},${2},(${3}),${4})"));
        assert_eq!(wall.insert_text_format, Some(InsertTextFormat::SNIPPET));
        assert_eq!(
            wall.command
                .as_ref()
                .map(|command| command.command.as_str()),
            Some(TRIGGER_PARAMETER_HINTS_COMMAND)
        );
        assert!(wall.documentation.is_none());
        assert!(wall.data.is_some());
    }

    #[test]
    fn entity_completion_stays_plain_when_text_follows_cursor() {
        let text = "#18=IFCWALL();";
        let document = Document::new_unloaded(text.to_string());
        let position = document
            .offset_to_position("#18=IFCWALL".len())
            .expect("cursor position should be valid");
        let results = completions(
            &document,
            &Url::parse("file:///test.ifc").unwrap(),
            position,
            Some("IFC4"),
            Some(&schema()),
            true,
            false,
        )
        .expect("expected completion results");
        let wall = results
            .items
            .iter()
            .find(|item| item.label == "IFCWALL")
            .expect("expected wall completion");

        assert_eq!(new_text(wall), "IFCWALL");
        assert_eq!(wall.insert_text_format, Some(InsertTextFormat::PLAIN_TEXT));
        assert!(wall.command.is_none());
    }

    #[test]
    fn entity_snippet_uses_optional_and_omitted_defaults() {
        let mut schema = schema();
        let wall = schema.entities.get_mut("IFCWALL").unwrap();
        wall.attributes[1].optional = true;
        wall.attributes[2].allows_omitted = true;

        let results = results("#18=IFCWALL", Some(&schema), true, true);
        let text = new_text(&results.items[0]);

        assert!(text.contains("${1:\\$},*"));
        assert!(results.items[0].documentation.is_some());
    }

    #[test]
    fn aggregate_completion_uses_schema_minimum_and_nested_shapes() {
        let nested = AggregateTypeRef {
            kind: AggregateKind::List,
            bounds: Some(crate::schema::AggregateBounds {
                lower: BoundValue::Integer(2),
                upper: BoundValue::Unbounded,
            }),
            item: Box::new(TypeRef::Aggregate(AggregateTypeRef {
                kind: AggregateKind::Array,
                bounds: Some(crate::schema::AggregateBounds {
                    lower: BoundValue::Integer(1),
                    upper: BoundValue::Integer(3),
                }),
                item: Box::new(named("IfcRoot", NamedTypeKind::Entity)),
                unique: false,
                optional: false,
            })),
            unique: true,
            optional: false,
        };
        let mut schema = schema();
        schema.entities.get_mut("IFCWALL").unwrap().attributes[3].ty = TypeRef::Aggregate(nested);

        let results = results("#18=IFCWALL('id',#2,.X.,", Some(&schema), true, true);
        assert_eq!(results.items.len(), 1);
        assert_eq!(
            new_text(&results.items[0]),
            "((${1},${2},${3}),(${4},${5},${6}))"
        );
        assert!(
            results.items[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("LIST [2:?] UNIQUE")
        );
    }

    #[test]
    fn aggregate_placeholder_count_is_capped_at_eight() {
        let aggregate = AggregateTypeRef {
            kind: AggregateKind::List,
            bounds: Some(crate::schema::AggregateBounds {
                lower: BoundValue::Integer(20),
                upper: BoundValue::Unbounded,
            }),
            item: Box::new(named("IfcRoot", NamedTypeKind::Entity)),
            unique: false,
            optional: false,
        };

        assert_eq!(aggregate_placeholder_count(&aggregate), 8);
    }

    #[test]
    fn resolve_adds_entity_and_reference_documentation() {
        let schema = schema();
        let entity_results = results("#18=IFCW", Some(&schema), false, false);
        let entity_item = entity_results
            .items
            .into_iter()
            .find(|item| item.label == "IFCWALL")
            .unwrap();
        let resolved_entity = resolve_completion_item(entity_item, None, Some(&schema));
        assert!(matches!(
            resolved_entity.documentation,
            Some(Documentation::MarkupContent(_))
        ));

        let text = "#2=IFCOWNERHISTORY($);#18=IFCWALL('id', #";
        let document = Document::new_unloaded(text.to_string());
        let reference_item = results(text, Some(&schema), false, false)
            .items
            .into_iter()
            .next()
            .unwrap();
        let resolved_reference = resolve_completion_item(reference_item, Some(&document), None);
        let Some(Documentation::MarkupContent(documentation)) = resolved_reference.documentation
        else {
            panic!("expected markdown documentation");
        };
        assert!(documentation.value.contains("#2=IFCOWNERHISTORY($);"));
    }

    #[test]
    fn resolve_leaves_stale_item_unchanged() {
        let schema = schema();
        let item = results(
            "#2=IFCOWNERHISTORY($);#18=IFCWALL('id', #",
            Some(&schema),
            false,
            false,
        )
        .items
        .into_iter()
        .next()
        .unwrap();

        let resolved = resolve_completion_item(item.clone(), None, None);
        assert_eq!(resolved, item);
    }

    #[test]
    fn reference_results_are_bounded_and_marked_incomplete() {
        let definitions = (1..=101)
            .map(|id| format!("#{id}=IFCWALL($);"))
            .collect::<String>();
        let text = format!("{definitions}#200=IFCWALL($,$,$,(#");
        let response = results(&text, Some(&schema()), false, false);

        assert_eq!(response.items.len(), MAX_REFERENCE_COMPLETIONS);
        assert!(response.is_incomplete);

        let narrowed = format!("{definitions}#200=IFCWALL($,$,$,(#100");
        let narrowed_results = results(&narrowed, Some(&schema()), false, false);
        assert_eq!(narrowed_results.items.len(), 1);
        assert_eq!(narrowed_results.items[0].label, "#100");
        assert!(!narrowed_results.is_incomplete);
    }

    #[test]
    fn reference_results_ignore_entity_name_case_and_follow_numeric_id_order() {
        let text = "#9=IfcWall($);#3=ifcdoor($);#12=IfcOwnerHistory($);#5=IFCWALL($);\
                    #7=ifcWallStandardCase($);#20=IFCWALL($,$,$,(#";
        let results = items(text, Some(&schema()));

        assert_eq!(
            results
                .iter()
                .map(|item| (item.label.as_str(), item.detail.as_deref().unwrap()))
                .collect::<Vec<_>>(),
            [
                ("#3", "IFCDOOR"),
                ("#5", "IFCWALL"),
                ("#7", "IFCWALLSTANDARDCASE"),
                ("#9", "IFCWALL"),
                ("#20", "IFCWALL"),
            ]
        );
    }

    #[test]
    fn reference_results_take_first_hundred_compatible_ids_among_incompatible_ones() {
        let definitions = |ids: &mut dyn Iterator<Item = u32>| {
            ids.map(|id| {
                let entity = if id % 2 == 0 {
                    "IfcWall"
                } else {
                    "IFCOWNERHISTORY"
                };
                format!("#{id}={entity}($);")
            })
            .collect::<String>()
        };
        let text = format!("{}#300=IFCWALL($,$,$,(#", definitions(&mut (1..=250).rev()));
        let response = results(&text, Some(&schema()), false, false);

        let expected = (1..=100).map(|n| format!("#{}", n * 2)).collect::<Vec<_>>();
        assert_eq!(
            response
                .items
                .iter()
                .map(|item| item.label.clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(response.is_incomplete);

        let response = results(
            &format!("{}#300=IFCWALL($,$,$,(#", definitions(&mut (1..=198))),
            Some(&schema()),
            false,
            false,
        );
        assert_eq!(response.items.len(), MAX_REFERENCE_COMPLETIONS);
        assert_eq!(response.items.last().unwrap().label, "#300");
        assert!(!response.is_incomplete);
    }

    #[test]
    fn reference_prefix_filters_ids_before_compatibility() {
        let definitions = (1..=120)
            .map(|id| {
                let entity = if id % 3 == 0 {
                    "IFCOWNERHISTORY"
                } else {
                    "IfcDoor"
                };
                format!("#{id}={entity}($);")
            })
            .collect::<String>();
        let text = format!("{definitions}#300=IFCWALL($,$,$,(#1");
        let labels = items(&text, Some(&schema()))
            .into_iter()
            .map(|item| item.label)
            .collect::<Vec<_>>();

        assert_eq!(
            labels,
            [
                "#1", "#10", "#11", "#13", "#14", "#16", "#17", "#19", "#100", "#101", "#103",
                "#104", "#106", "#107", "#109", "#110", "#112", "#113", "#115", "#116", "#118",
                "#119"
            ]
        );
    }

    #[test]
    fn bundled_schemas_filter_spatial_references_by_their_own_hierarchy() {
        let schemas = crate::schema::SchemaDocCollection::new();
        let text = "#1=IFCBUILDINGSTOREY($);#2=IfcSpatialZone($);#3=IFCROAD($);#4=IFCWALL($);\
                    #5=IFCSPACE($);#9=IFCRELCONTAINEDINSPATIALSTRUCTURE('g',$,$,$,(#4),#";
        for (schema_name, expected) in [
            ("IFC2X3", vec!["#1", "#5"]),
            ("IFC4", vec!["#1", "#2", "#5"]),
            ("IFC4X3_ADD2", vec!["#1", "#2", "#3", "#5"]),
        ] {
            let schema = schemas
                .get(schema_name)
                .expect("bundled schema should load");
            let labels = items(text, Some(schema))
                .into_iter()
                .map(|item| item.label)
                .collect::<Vec<_>>();
            assert_eq!(labels, expected, "{schema_name}");
        }
    }

    #[test]
    fn next_id_not_offered_inside_open_multiline_statement() {
        let text = "#1=IFCWALL(\n  ";
        let results = items(text, Some(&schema()));

        assert!(results.iter().map(new_text).all(|text| text != "#2="));
    }

    #[test]
    fn hash_inside_argument_does_not_offer_next_definition_id() {
        let text = "#2=IFCOWNERHISTORY($);\n#17=IFCWALL('fresh', #";
        let results = items(text, Some(&schema()));
        let edits = results.iter().map(new_text).collect::<Vec<_>>();

        assert_eq!(edits, ["#2"]);
        assert!(!edits.contains(&"#18="));
    }

    #[test]
    fn schema_aware_completions_do_not_require_ast() {
        let mut document = Document::new_unloaded("#1=IFCWALL(".to_string());
        document.ast_skipped = true;
        let position = document.offset_to_position(document.text.len()).unwrap();
        assert!(
            completions(
                &document,
                &Url::parse("file:///test.ifc").unwrap(),
                position,
                Some("IFC4"),
                Some(&schema()),
                false,
                true,
            )
            .is_some()
        );
    }

    #[test]
    fn ignores_ids_in_strings_when_computing_next_id() {
        let results = items("'#99='\n", None);
        assert_eq!(new_text(&results[0]), "#1=");
    }

    #[test]
    fn self_referential_alias_does_not_recurse_infinitely() {
        let mut schema = schema();
        schema.types.insert(
            "IFCSELFALIAS".to_string(),
            TypeDoc::Alias(AliasTypeDef {
                name: "IfcSelfAlias".to_string(),
                target: named("IfcSelfAlias", NamedTypeKind::Type),
            }),
        );
        let ty = named("IfcSelfAlias", NamedTypeKind::Type);

        assert!(!type_is_guid(&schema, &ty, 0, &mut HashSet::new()));
        assert!(enumeration_values(&schema, &ty, 0, &mut HashSet::new()).is_none());
        assert!(resolve_aggregate(&schema, &ty, &mut HashSet::new()).is_none());
    }

    #[test]
    fn test_schema_has_no_accidental_duplicate_supertypes() {
        let schema = schema();
        let supers = &schema.entity("IfcWall").unwrap().all_supertypes;
        assert_eq!(supers, &HashSet::from(["IFCROOT".to_string()]));
    }
}
