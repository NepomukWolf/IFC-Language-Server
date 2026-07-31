//! IFC scaffold completion snippets and file-level code actions.
//! This module handles Emmet-like abbreviations such as `!ifc:4x3` and empty-file scaffold
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScaffoldLevel {
    Metadata,
    Project,
    Spatial,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ScaffoldAbbreviation {
    text: String,
    range: Range,
    level: ScaffoldLevel,
    schema: IfcVersion,
}

#[derive(Clone, Debug)]
struct RenderContext {
    file_name: String,
    snippet: bool,
    timestamp: OffsetDateTime,
    guid_seed: Option<u128>,
}

impl RenderContext {
    fn from_uri(uri: &Url, snippet: bool) -> Self {
        Self {
            file_name: step_string(&file_name_from_uri(uri)),
            snippet,
            timestamp: OffsetDateTime::now_utc(),
            guid_seed: None,
        }
    }

    #[cfg(test)]
    fn for_test(file_name: &str, snippet: bool) -> Self {
        Self {
            file_name: step_string(file_name),
            snippet,
            timestamp: OffsetDateTime::from_unix_timestamp(1_731_578_976)
                .expect("test timestamp should be valid"),
            guid_seed: Some(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210),
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
        if let Some(seed) = self.guid_seed.as_mut() {
            let value = *seed;
            *seed = seed.wrapping_add(1);
            return compress_uuid(value);
        }

        compress_uuid(Uuid::new_v4().as_u128())
    }
}

pub fn completions(
    document: &Document,
    uri: &Url,
    position: Position,
    snippet_supported: bool,
) -> Option<CompletionResponse> {
    let abbreviation = abbreviation_at_position(document, position)?;
    let mut context = RenderContext::from_uri(uri, snippet_supported);
    let new_text = render_scaffold(abbreviation.level, abbreviation.schema, &mut context);
    let mut item = CompletionItem::new_simple(
        abbreviation.text.clone(),
        format!(
            "{} {}",
            abbreviation.level.detail(),
            schema_detail(abbreviation.schema)
        ),
    );

    item.kind = Some(CompletionItemKind::SNIPPET);
    item.filter_text = Some(abbreviation.text.clone());
    item.sort_text = Some("000_ifc_scaffold".to_string());
    item.insert_text_format = Some(if snippet_supported {
        InsertTextFormat::SNIPPET
    } else {
        InsertTextFormat::PLAIN_TEXT
    });
    item.text_edit = Some(CompletionTextEdit::Edit(TextEdit::new(
        abbreviation.range,
        new_text,
    )));

    Some(CompletionResponse::Array(vec![item]))
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
    let actions = [
        (ScaffoldLevel::Metadata, "Insert IFC metadata scaffold"),
        (ScaffoldLevel::Project, "Insert IFC project scaffold"),
        (ScaffoldLevel::Spatial, "Insert IFC spatial scaffold"),
    ]
    .into_iter()
    .map(|(level, title)| {
        let mut context = RenderContext::from_uri(uri, false);
        let new_text = render_scaffold(level, DEFAULT_SCHEMA, &mut context);
        CodeActionOrCommand::CodeAction(CodeAction {
            title: title.to_string(),
            kind: Some(CodeActionKind::SOURCE),
            edit: Some(workspace_edit(
                uri.clone(),
                TextEdit::new(replace_document_range, new_text),
            )),
            ..CodeAction::default()
        })
    })
    .collect();

    Some(actions)
}

fn code_action_kind_requested(requested_kinds: Option<&[CodeActionKind]>) -> bool {
    requested_kinds.is_none_or(|kinds| {
        kinds.iter().any(|kind| {
            let requested = kind.as_str();
            requested.is_empty() || CodeActionKind::SOURCE.as_str().starts_with(requested)
        })
    })
}

fn workspace_edit(uri: Url, edit: TextEdit) -> WorkspaceEdit {
    WorkspaceEdit {
        changes: Some(HashMap::from([(uri, vec![edit])])),
        document_changes: None,
        change_annotations: None,
    }
}

fn abbreviation_at_position(
    document: &Document,
    position: Position,
) -> Option<ScaffoldAbbreviation> {
    let offset = document.position_to_offset(position)?;
    let line_start = *document.line_offsets.get(position.line as usize)?;
    let line_prefix = document.text.get(line_start..offset)?;
    let token_start = line_prefix
        .rfind(|character: char| !is_abbreviation_character(character))
        .map(|index| index + 1)
        .unwrap_or(0);
    let token = line_prefix.get(token_start..)?;
    let (level, schema) = parse_abbreviation(token)?;
    let start_offset = line_start + token_start;

    Some(ScaffoldAbbreviation {
        text: token.to_string(),
        range: document.range_for_offsets(start_offset, offset)?,
        level,
        schema,
    })
}

fn is_abbreviation_character(character: char) -> bool {
    character == '!' || character == ':' || character.is_ascii_alphanumeric()
}

fn parse_abbreviation(token: &str) -> Option<(ScaffoldLevel, IfcVersion)> {
    let bang_count = token.bytes().take_while(|byte| *byte == b'!').count();
    let level = match bang_count {
        1 => ScaffoldLevel::Metadata,
        2 => ScaffoldLevel::Project,
        3 => ScaffoldLevel::Spatial,
        _ => return None,
    };

    let rest = token.get(bang_count..)?;
    if rest.len() < 3 || !rest[..3].eq_ignore_ascii_case("ifc") {
        return None;
    }

    let schema = match rest.get(3..) {
        Some("") => DEFAULT_SCHEMA,
        Some(selector) if selector.starts_with(':') => parse_schema_selector(&selector[1..])?,
        _ => return None,
    };

    Some((level, schema))
}

fn parse_schema_selector(selector: &str) -> Option<IfcVersion> {
    if selector.is_empty() {
        return None;
    }

    match selector.to_ascii_lowercase().as_str() {
        "2x3" => Some(IfcVersion::Ifc2x3Tc1),
        "4" => Some(IfcVersion::Ifc4Add2Tc1),
        "4x3" => Some(IfcVersion::Ifc4x3Add2),
        _ => None,
    }
}

fn schema_detail(schema: IfcVersion) -> &'static str {
    match schema {
        IfcVersion::Ifc2x3Tc1 => "IFC 2x3 TC1",
        IfcVersion::Ifc4Add2Tc1 => "IFC 4 ADD2 TC1",
        IfcVersion::Ifc4x3Add2 => "IFC 4x3 ADD2",
    }
}

impl ScaffoldLevel {
    fn detail(self) -> &'static str {
        match self {
            Self::Metadata => "IFC metadata scaffold",
            Self::Project => "IFC project scaffold",
            Self::Spatial => "IFC spatial scaffold",
        }
    }
}

fn render_scaffold(
    level: ScaffoldLevel,
    schema: IfcVersion,
    context: &mut RenderContext,
) -> String {
    match level {
        ScaffoldLevel::Metadata => render_metadata_scaffold(schema, context),
        ScaffoldLevel::Project => render_project_scaffold(schema, context),
        ScaffoldLevel::Spatial => render_spatial_scaffold(schema, context),
    }
}

fn render_metadata_scaffold(schema: IfcVersion, context: &RenderContext) -> String {
    render_template(
        METADATA_TEMPLATE,
        &common_bindings(schema, context, context.final_tabstop()),
    )
}

fn render_project_scaffold(schema: IfcVersion, context: &mut RenderContext) -> String {
    let project_guid = context.next_guid();

    let mut bindings = common_bindings(schema, context, context.final_tabstop());
    bindings.push(("project_guid", project_guid));
    bindings.push(("project_name", context.placeholder(3, "Project Name")));

    render_template(PROJECT_TEMPLATE, &bindings)
}

fn render_spatial_scaffold(schema: IfcVersion, context: &mut RenderContext) -> String {
    let project_guid = context.next_guid();
    let site_guid = context.next_guid();
    let building_guid = context.next_guid();
    let storey_guid = context.next_guid();
    let rel_project_site_guid = context.next_guid();
    let rel_site_building_guid = context.next_guid();
    let rel_building_storey_guid = context.next_guid();

    let mut bindings = common_bindings(schema, context, context.final_tabstop());
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

fn common_bindings(
    schema: IfcVersion,
    context: &RenderContext,
    final_tabstop: &str,
) -> Vec<(&'static str, String)> {
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
        ("schema_name", schema.schema_name().to_string()),
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
        let document = document(text);
        let CompletionResponse::Array(items) =
            completions(&document, uri, position, snippet_supported)?
        else {
            panic!("expected completion item array");
        };
        let item = items.into_iter().next()?;
        let new_text = match item.text_edit.as_ref()? {
            CompletionTextEdit::Edit(edit) => edit.new_text.clone(),
            CompletionTextEdit::InsertAndReplace(_) => panic!("expected plain text edit"),
        };
        Some((item, new_text))
    }

    fn render_for_test(level: ScaffoldLevel, schema: IfcVersion, file_name: &str) -> String {
        let mut context = RenderContext::for_test(file_name, true);
        render_scaffold(level, schema, &mut context)
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
    fn parses_supported_scaffold_abbreviations() {
        assert_eq!(
            parse_abbreviation("!ifc"),
            Some((ScaffoldLevel::Metadata, IfcVersion::Ifc4x3Add2))
        );
        assert_eq!(
            parse_abbreviation("!!ifc:2x3"),
            Some((ScaffoldLevel::Project, IfcVersion::Ifc2x3Tc1))
        );
        assert_eq!(
            parse_abbreviation("!!!ifc:4"),
            Some((ScaffoldLevel::Spatial, IfcVersion::Ifc4Add2Tc1))
        );
        assert_eq!(
            parse_abbreviation("!!!ifc:4x3"),
            Some((ScaffoldLevel::Spatial, IfcVersion::Ifc4x3Add2))
        );
    }

    #[test]
    fn rejects_unsupported_or_incomplete_abbreviations() {
        assert_eq!(parse_abbreviation("!!!!ifc"), None);
        assert_eq!(parse_abbreviation("!ifc:"), None);
        assert_eq!(parse_abbreviation("!ifc:4x2"), None);
        assert_eq!(parse_abbreviation("!wall"), None);
    }

    #[test]
    fn replaces_only_the_typed_abbreviation() {
        let document = document("prefix !ifc:4");
        let abbreviation = abbreviation_at_position(&document, Position::new(0, 13))
            .expect("expected scaffold abbreviation");

        assert_eq!(abbreviation.text, "!ifc:4");
        assert_eq!(abbreviation.range.start, Position::new(0, 7));
        assert_eq!(abbreviation.range.end, Position::new(0, 13));
    }

    #[test]
    fn uses_file_name_from_uri_in_completion() {
        let (_, new_text) =
            completion_text("!ifc:2x3", &file_uri("test.ifc"), Position::new(0, 8), true)
                .expect("expected completion");

        assert!(new_text.contains("FILE_NAME('test.ifc'"));
    }

    #[test]
    fn escapes_file_name_for_step_strings() {
        let output = render_for_test(
            ScaffoldLevel::Metadata,
            IfcVersion::Ifc4Add2Tc1,
            "owner's.ifc",
        );

        assert!(output.contains("FILE_NAME('owner''s.ifc'"));
    }

    #[test]
    fn renders_lsp_tool_metadata_in_header() {
        let output = render_for_test(ScaffoldLevel::Metadata, IfcVersion::Ifc4x3Add2, "test.ifc");

        assert!(output.contains("'2024-11-14T10:09:36'"));
        assert!(output.contains("ifc-language-server 0.4.1"));
        assert!(output.contains("'ifc-language-server'"));
    }

    #[test]
    fn scaffold_templates_render_without_unresolved_placeholders() {
        for level in [
            ScaffoldLevel::Metadata,
            ScaffoldLevel::Project,
            ScaffoldLevel::Spatial,
        ] {
            let output = render_for_test(level, IfcVersion::Ifc4x3Add2, "test.ifc");

            assert!(!output.contains("{{"));
            assert!(!output.contains("}}"));
        }
    }

    #[test]
    fn renders_snippet_completion_when_supported() {
        let (item, new_text) =
            completion_text("!ifc:2x3", &file_uri("test.ifc"), Position::new(0, 8), true)
                .expect("expected completion");

        assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        assert!(new_text.contains("FILE_SCHEMA(('IFC2X3'))"));
        assert!(new_text.contains("${1:Author}"));
        assert!(new_text.contains("$0"));
    }

    #[test]
    fn renders_plain_text_completion_without_snippet_support() {
        let (item, new_text) =
            completion_text("!!ifc:4", &file_uri("test.ifc"), Position::new(0, 7), false)
                .expect("expected completion");

        assert_eq!(item.insert_text_format, Some(InsertTextFormat::PLAIN_TEXT));
        assert!(new_text.contains("FILE_SCHEMA(('IFC4'))"));
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

    #[test]
    fn project_scaffold_includes_owner_history_and_application() {
        let output = render_for_test(ScaffoldLevel::Project, IfcVersion::Ifc4Add2Tc1, "test.ifc");

        assert!(output.contains("IFCOWNERHISTORY"));
        assert!(output.contains("IFCPERSONANDORGANIZATION"));
        assert!(output.contains("IFCPERSON"));
        assert!(output.contains("IFCORGANIZATION"));
        assert!(output.contains("IFCAPPLICATION"));
        assert!(output.contains("ifc-language-server"));
        assert!(output.contains("#1=IFCPROJECT("));
        assert!(output.contains(",#2,'${3:Project Name}'"));
        assert!(!output.contains("0000000000000000000000"));
    }

    #[test]
    fn spatial_scaffold_for_ifc4x3_has_valid_building_arity() {
        let output = render_for_test(ScaffoldLevel::Spatial, IfcVersion::Ifc4x3Add2, "test.ifc");
        let building_line = output
            .lines()
            .find(|line| line.contains("=IFCBUILDING("))
            .expect("expected building line");

        assert_eq!(building_line.matches(',').count() + 1, 12);
        assert!(output.contains("FILE_SCHEMA(('IFC4X3_ADD2'))"));
        assert!(output.contains("IFCBUILDINGSTOREY"));
        assert!(output.contains("IFCRELAGGREGATES"));
        assert!(output.contains("IFCUNITASSIGNMENT"));
        assert!(output.contains("IFCGEOMETRICREPRESENTATIONCONTEXT"));
    }

    #[test]
    fn spatial_scaffold_uses_object_placements_for_spatial_elements() {
        let output = render_for_test(ScaffoldLevel::Spatial, IfcVersion::Ifc4x3Add2, "test.ifc");

        assert!(output.contains("#17=IFCAXIS2PLACEMENT3D(#14,#15,#16);"));
        assert!(output.contains("#18=IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,$,#17,$);"));
        assert!(output.contains("#23=IFCLOCALPLACEMENT($,#17);"));
        assert!(output.contains("#24=IFCLOCALPLACEMENT(#23,#17);"));
        assert!(output.contains("#25=IFCLOCALPLACEMENT(#24,#17);"));
        assert!(output.contains(",#23,$,$,.ELEMENT.,$,$,$,$,$);"));
        assert!(output.contains(",#24,$,$,.ELEMENT.,$,$,$);"));
        assert!(output.contains(",#25,$,$,.ELEMENT.,$);"));
    }
}
