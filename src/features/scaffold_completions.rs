//! IFC scaffold completion snippets.
//! This module handles Emmet-like abbreviations such as `!ifc:4x3` without requiring AST state.

use crate::document::Document;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, InsertTextFormat,
    Position, Range, TextEdit,
};

const DEFAULT_SCHEMA: IfcSchema = IfcSchema::Ifc4x3Add2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScaffoldLevel {
    Metadata,
    Project,
    Spatial,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IfcSchema {
    Ifc2x3,
    Ifc4,
    Ifc4x3Add2,
}

impl IfcSchema {
    fn schema_name(self) -> &'static str {
        match self {
            Self::Ifc2x3 => "IFC2X3",
            Self::Ifc4 => "IFC4",
            Self::Ifc4x3Add2 => "IFC4X3_ADD2",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Self::Ifc2x3 => "IFC 2x3 TC1",
            Self::Ifc4 => "IFC 4 ADD2 TC1",
            Self::Ifc4x3Add2 => "IFC 4x3 ADD2",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ScaffoldAbbreviation {
    text: String,
    range: Range,
    level: ScaffoldLevel,
    schema: IfcSchema,
}

pub fn completions(
    document: &Document,
    position: Position,
    snippet_supported: bool,
) -> Option<CompletionResponse> {
    let abbreviation = abbreviation_at_position(document, position)?;
    let new_text = render_scaffold(abbreviation.level, abbreviation.schema, snippet_supported);
    let mut item = CompletionItem::new_simple(
        abbreviation.text.clone(),
        format!(
            "{} {}",
            abbreviation.level.detail(),
            abbreviation.schema.detail()
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

fn parse_abbreviation(token: &str) -> Option<(ScaffoldLevel, IfcSchema)> {
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

fn parse_schema_selector(selector: &str) -> Option<IfcSchema> {
    if selector.is_empty() {
        return None;
    }

    match selector.to_ascii_lowercase().as_str() {
        "2x3" => Some(IfcSchema::Ifc2x3),
        "4" => Some(IfcSchema::Ifc4),
        "4x3" => Some(IfcSchema::Ifc4x3Add2),
        _ => None,
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

fn render_scaffold(level: ScaffoldLevel, schema: IfcSchema, snippet: bool) -> String {
    match level {
        ScaffoldLevel::Metadata => render_metadata_scaffold(schema, snippet),
        ScaffoldLevel::Project => render_project_scaffold(schema, snippet),
        ScaffoldLevel::Spatial => render_spatial_scaffold(schema, snippet),
    }
}

fn render_metadata_scaffold(schema: IfcSchema, snippet: bool) -> String {
    format!(
        "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');\nFILE_NAME('{}','',(),(),'', '', '');\nFILE_SCHEMA(('{}'));\nENDSEC;\n\nDATA;\n{}\nENDSEC;\nEND-ISO-10303-21;\n",
        placeholder(1, "model.ifc", snippet),
        schema.schema_name(),
        final_tabstop(snippet)
    )
}

fn render_project_scaffold(schema: IfcSchema, snippet: bool) -> String {
    let omitted = omitted_value(snippet);
    format!(
        "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');\nFILE_NAME('{}','',(),(),'', '', '');\nFILE_SCHEMA(('{}'));\nENDSEC;\n\nDATA;\n#1=IFCPROJECT('0000000000000000000000',{omitted},'{}',{omitted},{omitted},{omitted},{omitted},{omitted},{omitted});\n{}\nENDSEC;\nEND-ISO-10303-21;\n",
        placeholder(1, "model.ifc", snippet),
        schema.schema_name(),
        placeholder(2, "Project Name", snippet),
        final_tabstop(snippet)
    )
}

fn render_spatial_scaffold(schema: IfcSchema, snippet: bool) -> String {
    let omitted = omitted_value(snippet);
    format!(
        "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');\nFILE_NAME('{}','',(),(),'', '', '');\nFILE_SCHEMA(('{}'));\nENDSEC;\n\nDATA;\n#1=IFCPROJECT('0000000000000000000000',{omitted},'{}',{omitted},{omitted},{omitted},{omitted},{omitted},{omitted});\n#2=IFCSITE('0000000000000000000001',{omitted},'{}',{omitted},{omitted},{omitted},{omitted},{omitted},{omitted},{omitted},{omitted},{omitted},{omitted},{omitted});\n#3=IFCBUILDING('0000000000000000000002',{omitted},'{}',{omitted},{omitted},{omitted},{omitted},{omitted},{omitted},{omitted},{omitted});\n#4=IFCBUILDINGSTOREY('0000000000000000000003',{omitted},'{}',{omitted},{omitted},{omitted},{omitted},{omitted},{omitted},{omitted});\n#5=IFCRELAGGREGATES('0000000000000000000004',{omitted},{omitted},{omitted},#1,(#2));\n#6=IFCRELAGGREGATES('0000000000000000000005',{omitted},{omitted},{omitted},#2,(#3));\n#7=IFCRELAGGREGATES('0000000000000000000006',{omitted},{omitted},{omitted},#3,(#4));\n{}\nENDSEC;\nEND-ISO-10303-21;\n",
        placeholder(1, "model.ifc", snippet),
        schema.schema_name(),
        placeholder(2, "Project Name", snippet),
        placeholder(3, "Site Name", snippet),
        placeholder(4, "Building Name", snippet),
        placeholder(5, "Storey Name", snippet),
        final_tabstop(snippet)
    )
}

fn placeholder(index: usize, text: &str, snippet: bool) -> String {
    if snippet {
        format!("${{{index}:{text}}}")
    } else {
        text.to_string()
    }
}

fn omitted_value(snippet: bool) -> &'static str {
    if snippet { "\\$" } else { "$" }
}

fn final_tabstop(snippet: bool) -> &'static str {
    if snippet { "$0" } else { "" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &str) -> Document {
        Document::new_unloaded(text.to_string())
    }

    fn completion_text(
        text: &str,
        position: Position,
        snippet_supported: bool,
    ) -> Option<(CompletionItem, String)> {
        let document = document(text);
        let CompletionResponse::Array(items) = completions(&document, position, snippet_supported)?
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

    #[test]
    fn parses_supported_scaffold_abbreviations() {
        assert_eq!(
            parse_abbreviation("!ifc"),
            Some((ScaffoldLevel::Metadata, IfcSchema::Ifc4x3Add2))
        );
        assert_eq!(
            parse_abbreviation("!!ifc:2x3"),
            Some((ScaffoldLevel::Project, IfcSchema::Ifc2x3))
        );
        assert_eq!(
            parse_abbreviation("!!!ifc:4"),
            Some((ScaffoldLevel::Spatial, IfcSchema::Ifc4))
        );
        assert_eq!(
            parse_abbreviation("!!!ifc:4x3"),
            Some((ScaffoldLevel::Spatial, IfcSchema::Ifc4x3Add2))
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
    fn renders_snippet_completion_when_supported() {
        let (item, new_text) =
            completion_text("!ifc:2x3", Position::new(0, 8), true).expect("expected completion");

        assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        assert!(new_text.contains("FILE_SCHEMA(('IFC2X3'))"));
        assert!(new_text.contains("${1:model.ifc}"));
        assert!(new_text.contains("$0"));
    }

    #[test]
    fn renders_plain_text_completion_without_snippet_support() {
        let (item, new_text) =
            completion_text("!!ifc:4", Position::new(0, 7), false).expect("expected completion");

        assert_eq!(item.insert_text_format, Some(InsertTextFormat::PLAIN_TEXT));
        assert!(new_text.contains("FILE_SCHEMA(('IFC4'))"));
        assert!(new_text.contains("'Project Name'"));
        assert!(!new_text.contains("${"));
        assert!(!new_text.contains("\\$"));
    }

    #[test]
    fn renders_spatial_scaffold_for_ifc4x3() {
        let (_, new_text) =
            completion_text("!!!ifc:4x3", Position::new(0, 10), true).expect("expected completion");

        assert!(new_text.contains("FILE_SCHEMA(('IFC4X3_ADD2'))"));
        assert!(new_text.contains("IFCBUILDINGSTOREY"));
        assert!(new_text.contains("IFCRELAGGREGATES"));
    }
}
