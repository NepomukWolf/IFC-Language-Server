//! IFC scaffold completion snippets and file-level code actions.
//! This module handles Emmet-like abbreviations such as `!ifc` and empty-file scaffold
//! actions without requiring AST state.

use crate::document::Document;
use crate::schema::IfcVersion;
use std::collections::HashMap;
use time::OffsetDateTime;
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionResponse, CompletionItem,
    CompletionItemKind, CompletionResponse, CompletionTextEdit, InsertTextFormat, Position, Range,
    TextEdit, Url, WorkspaceEdit,
};
use uuid::Uuid;

const DEFAULT_SCHEMA: IfcVersion = IfcVersion::Ifc4x3Add2;
const IFC_GUID_ALPHABET: &[u8; 64] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_$";
const METADATA_TEMPLATE: &str = include_str!("scaffold_templates/metadata.ifc.tpl");
const PROJECT_TEMPLATE: &str = include_str!("scaffold_templates/project.ifc.tpl");
const SPATIAL_TEMPLATE: &str = include_str!("scaffold_templates/spatial.ifc.tpl");

#[derive(Clone, Copy)]
enum ScaffoldLevel {
    Metadata,
    Project,
    Spatial,
}

const SCAFFOLD_LEVELS: [ScaffoldLevel; 3] = [
    ScaffoldLevel::Metadata,
    ScaffoldLevel::Project,
    ScaffoldLevel::Spatial,
];

struct ScaffoldCompletionRequest {
    text: String,
    range: Range,
}

struct RenderContext {
    file_name: String,
    snippet: bool,
    timestamp: OffsetDateTime,
}

impl RenderContext {
    fn from_uri(uri: &Url, snippet: bool) -> Self {
        Self {
            file_name: step_string(&file_name_from_uri(uri)),
            snippet,
            timestamp: OffsetDateTime::now_utc(),
        }
    }

    #[cfg(test)]
    fn for_test(file_name: &str, snippet: bool) -> Self {
        Self {
            file_name: step_string(file_name),
            snippet,
            timestamp: OffsetDateTime::from_unix_timestamp(1_731_578_976)
                .expect("test timestamp should be valid"),
        }
    }

    fn placeholder(&self, index: usize, text: &str) -> String {
        if self.snippet {
            format!("${{{index}:{text}}}")
        } else {
            text.to_string()
        }
    }

    fn final_tabstop(&self) -> &'static str {
        if self.snippet { "$0" } else { "" }
    }

    fn next_guid(&mut self) -> String {
        compress_uuid(Uuid::new_v4().as_u128())
    }
}

pub fn completions(
    document: &Document,
    uri: &Url,
    position: Position,
    snippet_supported: bool,
) -> Option<CompletionResponse> {
    let request = completion_request_at_position(document, position)?;
    let items = completion_candidates(&request.text)
        .into_iter()
        .enumerate()
        .map(|(index, level)| {
            let mut context = RenderContext::from_uri(uri, snippet_supported);
            let new_text = render_scaffold(level, &mut context);
            let mut item = CompletionItem::new_simple(
                level.detail().to_string(),
                "Insert IFC STEP boilerplate".to_string(),
            );

            item.kind = Some(CompletionItemKind::SNIPPET);
            item.filter_text = Some(level.trigger().to_string());
            item.sort_text = Some(format!("{index:03}_ifc_scaffold"));
            item.insert_text_format = Some(if snippet_supported {
                InsertTextFormat::SNIPPET
            } else {
                InsertTextFormat::PLAIN_TEXT
            });
            item.text_edit = Some(CompletionTextEdit::Edit(TextEdit::new(
                request.range,
                new_text,
            )));
            item
        })
        .collect::<Vec<_>>();

    (!items.is_empty()).then_some(CompletionResponse::Array(items))
}

pub fn code_actions(
    document: &Document,
    uri: &Url,
    requested_kinds: Option<&[CodeActionKind]>,
) -> Option<CodeActionResponse> {
    if !document.text.trim().is_empty() || !code_action_kind_requested(requested_kinds) {
        return None;
    }

    let replace_document_range = document.range_for_offsets(0, document.text.len())?;
    let actions = SCAFFOLD_LEVELS.into_iter().map(|level| {
        let mut context = RenderContext::from_uri(uri, false);
        let new_text = render_scaffold(level, &mut context);
        CodeActionOrCommand::CodeAction(CodeAction {
            title: format!("Insert {}", level.detail()),
            kind: Some(CodeActionKind::SOURCE),
            edit: Some(WorkspaceEdit::new(HashMap::from([(
                uri.clone(),
                vec![TextEdit::new(replace_document_range, new_text)],
            )]))),
            ..CodeAction::default()
        })
    });

    Some(actions.collect())
}

fn code_action_kind_requested(requested_kinds: Option<&[CodeActionKind]>) -> bool {
    requested_kinds.is_none_or(|kinds| {
        kinds.iter().any(|kind| {
            let requested = kind.as_str();
            requested.is_empty() || CodeActionKind::SOURCE.as_str().starts_with(requested)
        })
    })
}

fn completion_request_at_position(
    document: &Document,
    position: Position,
) -> Option<ScaffoldCompletionRequest> {
    let offset = document.position_to_offset(position)?;
    let line_start = *document.line_offsets.get(position.line as usize)?;
    let line_prefix = document.text.get(line_start..offset)?;
    let token_start = line_prefix
        .rfind(|character: char| !is_abbreviation_character(character))
        .map(|index| index + 1)
        .unwrap_or(0);
    let token = line_prefix.get(token_start..)?;
    if token.is_empty() {
        return None;
    }

    let start_offset = line_start + token_start;

    Some(ScaffoldCompletionRequest {
        text: token.to_string(),
        range: document.range_for_offsets(start_offset, offset)?,
    })
}

fn is_abbreviation_character(character: char) -> bool {
    character == '!' || character.is_ascii_alphanumeric()
}

fn completion_candidates(token: &str) -> Vec<ScaffoldLevel> {
    SCAFFOLD_LEVELS
        .iter()
        .copied()
        .filter(|level| starts_with_ignore_ascii_case(level.trigger(), token))
        .collect()
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

impl ScaffoldLevel {
    fn trigger(self) -> &'static str {
        match self {
            Self::Metadata => "!ifc",
            Self::Project => "!!ifc",
            Self::Spatial => "!!!ifc",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Self::Metadata => "IFC metadata scaffold",
            Self::Project => "IFC project scaffold",
            Self::Spatial => "IFC spatial scaffold",
        }
    }
}

fn render_scaffold(level: ScaffoldLevel, context: &mut RenderContext) -> String {
    match level {
        ScaffoldLevel::Metadata => render_metadata_scaffold(context),
        ScaffoldLevel::Project => render_project_scaffold(context),
        ScaffoldLevel::Spatial => render_spatial_scaffold(context),
    }
}

fn render_metadata_scaffold(context: &RenderContext) -> String {
    render_template(
        METADATA_TEMPLATE,
        &common_bindings(context, context.final_tabstop()),
    )
}

fn render_project_scaffold(context: &mut RenderContext) -> String {
    let project_guid = context.next_guid();

    let mut bindings = common_bindings(context, context.final_tabstop());
    bindings.push(("project_guid", project_guid));
    bindings.push(("project_name", context.placeholder(3, "Project Name")));

    render_template(PROJECT_TEMPLATE, &bindings)
}

fn render_spatial_scaffold(context: &mut RenderContext) -> String {
    let project_guid = context.next_guid();
    let site_guid = context.next_guid();
    let building_guid = context.next_guid();
    let storey_guid = context.next_guid();
    let rel_project_site_guid = context.next_guid();
    let rel_site_building_guid = context.next_guid();
    let rel_building_storey_guid = context.next_guid();

    let mut bindings = common_bindings(context, context.final_tabstop());
    bindings.push(("project_guid", project_guid));
    bindings.push(("site_guid", site_guid));
    bindings.push(("building_guid", building_guid));
    bindings.push(("storey_guid", storey_guid));
    bindings.push(("rel_project_site_guid", rel_project_site_guid));
    bindings.push(("rel_site_building_guid", rel_site_building_guid));
    bindings.push(("rel_building_storey_guid", rel_building_storey_guid));
    bindings.push(("project_name", context.placeholder(3, "Project Name")));
    bindings.push(("site_name", context.placeholder(4, "Site Name")));
    bindings.push(("building_name", context.placeholder(5, "Building Name")));
    bindings.push(("storey_name", context.placeholder(6, "Storey Name")));

    render_template(SPATIAL_TEMPLATE, &bindings)
}

fn common_bindings(context: &RenderContext, final_tabstop: &str) -> Vec<(&'static str, String)> {
    vec![
        ("file_name", context.file_name.clone()),
        ("timestamp_iso", step_timestamp(context.timestamp)),
        (
            "timestamp_unix",
            context.timestamp.unix_timestamp().to_string(),
        ),
        ("author", context.placeholder(1, "Author")),
        ("organization", context.placeholder(2, "Organization")),
        ("originating_system", lsp_tool_name_with_version()),
        ("preprocessor_version", "ifc-language-server".to_string()),
        ("application_version", env!("CARGO_PKG_VERSION").to_string()),
        ("schema_name", DEFAULT_SCHEMA.schema_name().to_string()),
        ("final_tabstop", final_tabstop.to_string()),
    ]
}

fn lsp_tool_name_with_version() -> String {
    step_string(&format!(
        "ifc-language-server {}",
        env!("CARGO_PKG_VERSION")
    ))
}

fn render_template(template: &str, bindings: &[(&str, String)]) -> String {
    let mut output = template.to_string();
    for (name, value) in bindings {
        output = output.replace(&format!("{{{{{name}}}}}"), value);
    }
    output
}

fn step_timestamp(timestamp: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        timestamp.year(),
        u8::from(timestamp.month()),
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute(),
        timestamp.second()
    )
}

fn file_name_from_uri(uri: &Url) -> String {
    uri.to_file_path()
        .ok()
        .and_then(|path| path.file_name().map(|file_name| file_name.to_owned()))
        .and_then(|file_name| file_name.into_string().ok())
        .filter(|file_name| !file_name.is_empty())
        .unwrap_or_else(|| "model.ifc".to_string())
}

fn step_string(value: &str) -> String {
    value.replace('\'', "''")
}

fn compress_uuid(mut value: u128) -> String {
    let mut output = [b'0'; 22];
    for character in output.iter_mut().rev() {
        let index = (value & 0b11_1111) as usize;
        *character = IFC_GUID_ALPHABET[index];
        value >>= 6;
    }

    String::from_utf8(output.to_vec()).expect("IFC GUID alphabet should be ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &str) -> Document {
        Document::new_unloaded(text.to_string())
    }

    fn file_uri(name: &str) -> Url {
        Url::from_file_path(format!("/tmp/{name}")).expect("file URI should be valid")
    }

    fn completion_text(
        text: &str,
        uri: &Url,
        position: Position,
        snippet_supported: bool,
    ) -> Option<(CompletionItem, String)> {
        let item = completion_items(text, uri, position, snippet_supported)?
            .into_iter()
            .next()?;
        let new_text = match item.text_edit.as_ref()? {
            CompletionTextEdit::Edit(edit) => edit.new_text.clone(),
            CompletionTextEdit::InsertAndReplace(_) => panic!("expected plain text edit"),
        };
        Some((item, new_text))
    }

    fn completion_items(
        text: &str,
        uri: &Url,
        position: Position,
        snippet_supported: bool,
    ) -> Option<Vec<CompletionItem>> {
        let document = document(text);
        let CompletionResponse::Array(items) =
            completions(&document, uri, position, snippet_supported)?
        else {
            panic!("expected completion item array");
        };
        Some(items)
    }

    fn code_action_edits(text: &str, uri: &Url) -> Option<Vec<(String, TextEdit)>> {
        let document = document(text);
        let actions = code_actions(&document, uri, None)?;

        Some(
            actions
                .into_iter()
                .map(|action| {
                    let CodeActionOrCommand::CodeAction(action) = action else {
                        panic!("expected code action");
                    };
                    let edit = action
                        .edit
                        .and_then(|edit| edit.changes)
                        .and_then(|mut changes| changes.remove(uri))
                        .and_then(|mut edits| {
                            assert_eq!(edits.len(), 1);
                            edits.pop()
                        })
                        .expect("expected text edit");

                    (action.title, edit)
                })
                .collect(),
        )
    }

    #[test]
    fn replaces_only_the_typed_abbreviation() {
        let document = document("prefix !ifc");
        let request = completion_request_at_position(&document, Position::new(0, 11))
            .expect("expected scaffold completion request");

        assert_eq!(request.text, "!ifc");
        assert_eq!(request.range.start, Position::new(0, 7));
        assert_eq!(request.range.end, Position::new(0, 11));
    }

    #[test]
    fn offers_default_scaffold_levels_from_single_bang() {
        let items = completion_items("!", &file_uri("test.ifc"), Position::new(0, 1), true)
            .expect("expected scaffold completions");
        let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();

        assert_eq!(
            labels,
            [
                "IFC metadata scaffold",
                "IFC project scaffold",
                "IFC spatial scaffold"
            ]
        );
    }

    #[test]
    fn narrows_default_scaffold_levels_from_repeated_bangs() {
        let items = completion_items("!!", &file_uri("test.ifc"), Position::new(0, 2), true)
            .expect("expected scaffold completions");
        let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();

        assert_eq!(labels, ["IFC project scaffold", "IFC spatial scaffold"]);
    }

    #[test]
    fn does_not_offer_schema_suffix_completions() {
        for (text, position) in [
            ("!ifc:", Position::new(0, 5)),
            ("!ifc:2x3", Position::new(0, 8)),
            ("!ifc:4", Position::new(0, 6)),
        ] {
            let document = document(text);

            assert_eq!(
                completions(&document, &file_uri("test.ifc"), position, true),
                None
            );
        }
    }

    #[test]
    fn partial_completion_replaces_only_the_typed_token() {
        let items = completion_items(
            "prefix !!",
            &file_uri("test.ifc"),
            Position::new(0, 9),
            true,
        )
        .expect("expected scaffold completions");
        let edit = match items
            .first()
            .and_then(|item| item.text_edit.as_ref())
            .expect("expected text edit")
        {
            CompletionTextEdit::Edit(edit) => edit,
            CompletionTextEdit::InsertAndReplace(_) => panic!("expected plain text edit"),
        };

        assert_eq!(edit.range.start, Position::new(0, 7));
        assert_eq!(edit.range.end, Position::new(0, 9));
    }

    #[test]
    fn uses_file_name_from_uri_in_completion() {
        let (_, new_text) =
            completion_text("!ifc", &file_uri("test.ifc"), Position::new(0, 4), true)
                .expect("expected completion");

        assert!(new_text.contains("FILE_NAME('test.ifc'"));
    }

    #[test]
    fn escapes_file_name_for_step_strings() {
        let mut context = RenderContext::for_test("owner's.ifc", true);
        let output = render_scaffold(ScaffoldLevel::Metadata, &mut context);

        assert!(output.contains("FILE_NAME('owner''s.ifc'"));
    }

    #[test]
    fn renders_lsp_tool_metadata_in_header() {
        let mut context = RenderContext::for_test("test.ifc", true);
        let output = render_scaffold(ScaffoldLevel::Metadata, &mut context);

        assert!(output.contains("'2024-11-14T10:09:36'"));
        assert!(output.contains(&format!(
            "ifc-language-server {}",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(output.contains("'ifc-language-server'"));
    }

    #[test]
    fn renders_snippet_completion_when_supported() {
        let (item, new_text) =
            completion_text("!ifc", &file_uri("test.ifc"), Position::new(0, 4), true)
                .expect("expected completion");

        assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        assert!(new_text.contains("FILE_SCHEMA(('IFC4X3_ADD2'))"));
        assert!(new_text.contains("${1:Author}"));
        assert!(new_text.contains("$0"));
    }

    #[test]
    fn renders_plain_text_completion_without_snippet_support() {
        let (item, new_text) =
            completion_text("!!ifc", &file_uri("test.ifc"), Position::new(0, 5), false)
                .expect("expected completion");

        assert_eq!(item.insert_text_format, Some(InsertTextFormat::PLAIN_TEXT));
        assert!(new_text.contains("FILE_SCHEMA(('IFC4X3_ADD2'))"));
        assert!(new_text.contains("'Project Name'"));
        assert!(!new_text.contains("${"));
        assert!(!new_text.contains("\\$"));
    }

    #[test]
    fn offers_three_code_actions_for_empty_document() {
        let edits = code_action_edits("", &file_uri("empty.ifc")).expect("expected code actions");
        let titles: Vec<_> = edits.iter().map(|(title, _)| title.as_str()).collect();

        assert_eq!(
            titles,
            [
                "Insert IFC metadata scaffold",
                "Insert IFC project scaffold",
                "Insert IFC spatial scaffold"
            ]
        );
        assert!(
            edits
                .iter()
                .all(|(_, edit)| edit.range == Range::new(Position::new(0, 0), Position::new(0, 0)))
        );
    }

    #[test]
    fn code_actions_replace_whitespace_only_document() {
        let edits =
            code_action_edits(" \n\t", &file_uri("blank.ifc")).expect("expected code actions");

        assert!(
            edits
                .iter()
                .all(|(_, edit)| edit.range == Range::new(Position::new(0, 0), Position::new(1, 1)))
        );
    }

    #[test]
    fn code_actions_insert_plain_default_schema_scaffolds() {
        let edits = code_action_edits("", &file_uri("plain.ifc")).expect("expected code actions");

        for (_, edit) in edits {
            assert!(edit.new_text.contains("FILE_SCHEMA(('IFC4X3_ADD2'))"));
            assert!(!edit.new_text.contains("${"));
            assert!(!edit.new_text.contains("$0"));
        }
    }

    #[test]
    fn does_not_offer_code_actions_for_non_empty_documents() {
        let document = document("ISO-10303-21;\nEND-ISO-10303-21;");

        assert_eq!(code_actions(&document, &file_uri("model.ifc"), None), None);
    }

    #[test]
    fn filters_code_actions_by_requested_kind() {
        let document = document("");

        assert!(
            code_actions(
                &document,
                &file_uri("model.ifc"),
                Some(&[CodeActionKind::SOURCE])
            )
            .is_some()
        );
        assert!(
            code_actions(
                &document,
                &file_uri("model.ifc"),
                Some(&[CodeActionKind::QUICKFIX])
            )
            .is_none()
        );
    }

    #[test]
    fn compressed_guids_have_ifc_shape() {
        let guid = compress_uuid(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210);

        assert_eq!(guid.len(), 22);
        assert!(guid.bytes().all(|byte| IFC_GUID_ALPHABET.contains(&byte)));
    }
}
