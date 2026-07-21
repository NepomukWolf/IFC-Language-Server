//! IFC scaffold completion snippets.
//! This module handles Emmet-like abbreviations such as `!ifc:4x3` without requiring AST state.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::document::Document;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, InsertTextFormat,
    Position, Range, TextEdit, Url,
};
use uuid::Uuid;

const DEFAULT_SCHEMA: IfcSchema = IfcSchema::Ifc4x3Add2;
const IFC_GUID_ALPHABET: &[u8; 64] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_$";

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

#[derive(Clone, Debug)]
struct RenderContext {
    file_name: String,
    snippet: bool,
    timestamp: Timestamp,
    guid_seed: Option<u128>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Timestamp {
    unix_seconds: u64,
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

impl RenderContext {
    fn from_uri(uri: &Url, snippet: bool) -> Self {
        Self {
            file_name: step_string(&file_name_from_uri(uri)),
            snippet,
            timestamp: Timestamp::now(),
            guid_seed: None,
        }
    }

    #[cfg(test)]
    fn for_test(file_name: &str, snippet: bool) -> Self {
        Self {
            file_name: step_string(file_name),
            snippet,
            timestamp: Timestamp::from_unix_seconds(1_731_578_976),
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

    fn header(&self, schema: IfcSchema) -> String {
        format!(
            "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('ViewDefinition [ReferenceView]'),'2;1');\nFILE_NAME('{}','{}',('{}'),('{}'),'{}','{}','');\nFILE_SCHEMA(('{}'));\nENDSEC;\n\nDATA;\n",
            self.file_name,
            self.timestamp.iso_string(),
            self.placeholder(1, "Author"),
            self.placeholder(2, "Organization"),
            step_string(&format!(
                "ifc-language-server {}",
                env!("CARGO_PKG_VERSION")
            )),
            "ifc-language-server",
            schema.schema_name()
        )
    }
}

impl Timestamp {
    fn now() -> Self {
        let unix_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        Self::from_unix_seconds(unix_seconds)
    }

    fn from_unix_seconds(unix_seconds: u64) -> Self {
        let days = (unix_seconds / 86_400) as i64;
        let seconds_of_day = (unix_seconds % 86_400) as u32;
        let (year, month, day) = civil_from_days(days);

        Self {
            unix_seconds,
            year,
            month,
            day,
            hour: seconds_of_day / 3_600,
            minute: (seconds_of_day % 3_600) / 60,
            second: seconds_of_day % 60,
        }
    }

    fn iso_string(self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
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

fn render_scaffold(level: ScaffoldLevel, schema: IfcSchema, context: &mut RenderContext) -> String {
    match level {
        ScaffoldLevel::Metadata => render_metadata_scaffold(schema, context),
        ScaffoldLevel::Project => render_project_scaffold(schema, context),
        ScaffoldLevel::Spatial => render_spatial_scaffold(schema, context),
    }
}

fn render_metadata_scaffold(schema: IfcSchema, context: &RenderContext) -> String {
    format!(
        "{}{}\nENDSEC;\nEND-ISO-10303-21;\n",
        context.header(schema),
        context.final_tabstop()
    )
}

fn render_project_scaffold(schema: IfcSchema, context: &mut RenderContext) -> String {
    let project_guid = context.next_guid();
    let owner_history = support_entities(context, 2);
    let context_and_units = context_and_units(8);
    let project = ifc_project(
        schema,
        1,
        &project_guid,
        "#2",
        &context.placeholder(3, "Project Name"),
        "#12",
        "#16",
    );

    format!(
        "{}{project}{owner_history}{context_and_units}{}\nENDSEC;\nEND-ISO-10303-21;\n",
        context.header(schema),
        context.final_tabstop()
    )
}

fn render_spatial_scaffold(schema: IfcSchema, context: &mut RenderContext) -> String {
    let project_guid = context.next_guid();
    let site_guid = context.next_guid();
    let building_guid = context.next_guid();
    let storey_guid = context.next_guid();
    let rel_project_site_guid = context.next_guid();
    let rel_site_building_guid = context.next_guid();
    let rel_building_storey_guid = context.next_guid();
    let owner_history = support_entities(context, 8);
    let context_and_units = context_and_units(14);

    let project = ifc_project(
        schema,
        1,
        &project_guid,
        "#8",
        &context.placeholder(3, "Project Name"),
        "#18",
        "#22",
    );
    let site = ifc_site(
        2,
        &site_guid,
        "#8",
        &context.placeholder(4, "Site Name"),
        "#17",
    );
    let building = ifc_building(
        3,
        &building_guid,
        "#8",
        &context.placeholder(5, "Building Name"),
        "#17",
    );
    let storey = ifc_building_storey(
        4,
        &storey_guid,
        "#8",
        &context.placeholder(6, "Storey Name"),
        "#17",
    );
    let rel_project_site = ifc_rel_aggregates(
        5,
        &rel_project_site_guid,
        "#8",
        "Project aggregation",
        "#1",
        "#2",
    );
    let rel_site_building = ifc_rel_aggregates(
        6,
        &rel_site_building_guid,
        "#8",
        "Site aggregation",
        "#2",
        "#3",
    );
    let rel_building_storey = ifc_rel_aggregates(
        7,
        &rel_building_storey_guid,
        "#8",
        "Building aggregation",
        "#3",
        "#4",
    );

    format!(
        "{}{project}{site}{building}{storey}{rel_project_site}{rel_site_building}{rel_building_storey}{owner_history}{context_and_units}{}\nENDSEC;\nEND-ISO-10303-21;\n",
        context.header(schema),
        context.final_tabstop()
    )
}

fn support_entities(context: &mut RenderContext, first_id: u32) -> String {
    let person = first_id + 2;
    let organization = first_id + 3;
    let application = first_id + 4;
    let application_organization = first_id + 5;

    format!(
        "#{first_id}=IFCOWNERHISTORY(#{person_and_organization},#{application},$,.ADDED.,{timestamp},#{person_and_organization},#{application},{timestamp});\n\
         #{person_and_organization}=IFCPERSONANDORGANIZATION(#{person},#{organization},$);\n\
         #{person}=IFCPERSON($,'{}',$,$,$,$,$,$);\n\
         #{organization}=IFCORGANIZATION($,'{}',$,$,$);\n\
         #{application}=IFCAPPLICATION(#{application_organization},'{}','ifc-language-server','ifc-language-server');\n\
         #{application_organization}=IFCORGANIZATION($,'ifc-language-server',$,$,$);\n",
        context.placeholder(1, "Author"),
        context.placeholder(2, "Organization"),
        env!("CARGO_PKG_VERSION"),
        person_and_organization = first_id + 1,
        timestamp = context.timestamp.unix_seconds
    )
}

fn context_and_units(first_id: u32) -> String {
    format!(
        "#{first_id}=IFCCARTESIANPOINT((0.,0.,0.));\n\
         #{z_direction}=IFCDIRECTION((0.,0.,1.));\n\
         #{x_direction}=IFCDIRECTION((1.,0.,0.));\n\
         #{placement}=IFCAXIS2PLACEMENT3D(#{first_id},#{z_direction},#{x_direction});\n\
         #{representation_context}=IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,$,#{placement},$);\n\
         #{length_unit}=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);\n\
         #{area_unit}=IFCSIUNIT(*,.AREAUNIT.,$,.SQUARE_METRE.);\n\
         #{volume_unit}=IFCSIUNIT(*,.VOLUMEUNIT.,$,.CUBIC_METRE.);\n\
         #{unit_assignment}=IFCUNITASSIGNMENT((#{length_unit},#{area_unit},#{volume_unit}));\n",
        z_direction = first_id + 1,
        x_direction = first_id + 2,
        placement = first_id + 3,
        representation_context = first_id + 4,
        length_unit = first_id + 5,
        area_unit = first_id + 6,
        volume_unit = first_id + 7,
        unit_assignment = first_id + 8,
    )
}

fn ifc_project(
    schema: IfcSchema,
    id: u32,
    guid: &str,
    owner_history: &str,
    name: &str,
    representation_context: &str,
    unit_assignment: &str,
) -> String {
    match schema {
        IfcSchema::Ifc2x3 => format!(
            "#{id}=IFCPROJECT('{guid}',{owner_history},'{name}',$,$,$,$,({representation_context}),{unit_assignment});\n"
        ),
        IfcSchema::Ifc4 | IfcSchema::Ifc4x3Add2 => format!(
            "#{id}=IFCPROJECT('{guid}',{owner_history},'{name}',$,$,$,$,({representation_context}),{unit_assignment});\n"
        ),
    }
}

fn ifc_site(id: u32, guid: &str, owner_history: &str, name: &str, placement: &str) -> String {
    format!(
        "#{id}=IFCSITE('{guid}',{owner_history},'{name}',$,$,{placement},$,$,.ELEMENT.,$,$,$,$,$);\n"
    )
}

fn ifc_building(id: u32, guid: &str, owner_history: &str, name: &str, placement: &str) -> String {
    format!(
        "#{id}=IFCBUILDING('{guid}',{owner_history},'{name}',$,$,{placement},$,$,.ELEMENT.,$,$,$);\n"
    )
}

fn ifc_building_storey(
    id: u32,
    guid: &str,
    owner_history: &str,
    name: &str,
    placement: &str,
) -> String {
    format!(
        "#{id}=IFCBUILDINGSTOREY('{guid}',{owner_history},'{name}',$,$,{placement},$,$,.ELEMENT.,$);\n"
    )
}

fn ifc_rel_aggregates(
    id: u32,
    guid: &str,
    owner_history: &str,
    name: &str,
    relating_object: &str,
    related_object: &str,
) -> String {
    format!(
        "#{id}=IFCRELAGGREGATES('{guid}',{owner_history},'{name}',$,{relating_object},({related_object}));\n"
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

fn civil_from_days(days_since_unix_epoch: i64) -> (i32, u32, u32) {
    let days = days_since_unix_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };

    (year as i32, month as u32, day as u32)
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

    fn render_for_test(level: ScaffoldLevel, schema: IfcSchema, file_name: &str) -> String {
        let mut context = RenderContext::for_test(file_name, true);
        render_scaffold(level, schema, &mut context)
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
    fn uses_file_name_from_uri_in_completion() {
        let (_, new_text) =
            completion_text("!ifc:2x3", &file_uri("test.ifc"), Position::new(0, 8), true)
                .expect("expected completion");

        assert!(new_text.contains("FILE_NAME('test.ifc'"));
    }

    #[test]
    fn escapes_file_name_for_step_strings() {
        let output = render_for_test(ScaffoldLevel::Metadata, IfcSchema::Ifc4, "owner's.ifc");

        assert!(output.contains("FILE_NAME('owner''s.ifc'"));
    }

    #[test]
    fn renders_lsp_tool_metadata_in_header() {
        let output = render_for_test(ScaffoldLevel::Metadata, IfcSchema::Ifc4x3Add2, "test.ifc");

        assert!(output.contains("'2024-11-14T10:09:36'"));
        assert!(output.contains("ifc-language-server 0.4.1"));
        assert!(output.contains("'ifc-language-server'"));
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
    fn compressed_guids_have_ifc_shape() {
        let guid = compress_uuid(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210);

        assert_eq!(guid.len(), 22);
        assert!(guid.bytes().all(|byte| IFC_GUID_ALPHABET.contains(&byte)));
    }

    #[test]
    fn project_scaffold_includes_owner_history_and_application() {
        let output = render_for_test(ScaffoldLevel::Project, IfcSchema::Ifc4, "test.ifc");

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
        let output = render_for_test(ScaffoldLevel::Spatial, IfcSchema::Ifc4x3Add2, "test.ifc");
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
}
