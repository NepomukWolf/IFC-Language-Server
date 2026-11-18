use super::*;
use crate::test_support::build_entity_instances;

fn change(range: Option<Range>, text: &str) -> TextDocumentContentChangeEvent {
    TextDocumentContentChangeEvent {
        range,
        range_length: None,
        text: text.to_string(),
    }
}

fn parse_document(text: &str) -> Document {
    Document::parse(text.to_string())
}

#[test]
fn numeric_aggregate_preserves_values_and_multiline_ranges() {
    let document = parse_document("#1=IFCCARTESIANPOINT(( 1.25,\n -2E-3, .75 ));");
    let instances = instances(&document);
    let ParameterValue::List { items, range } = &instances.instances[0].parameters[0] else {
        panic!("coordinate aggregate expected");
    };
    assert_eq!(
        *range,
        Range::new(Position::new(0, 21), Position::new(1, 13))
    );
    assert_eq!(
        items,
        &vec![
            ParameterValue::Number {
                text: "1.25".into(),
                range: Range::new(Position::new(0, 23), Position::new(0, 27))
            },
            ParameterValue::Number {
                text: "-2E-3".into(),
                range: Range::new(Position::new(1, 1), Position::new(1, 6))
            },
            ParameterValue::Number {
                text: ".75".into(),
                range: Range::new(Position::new(1, 8), Position::new(1, 11))
            },
        ]
    );
}

#[test]
fn reference_aggregate_preserves_ranges_and_overflow() {
    let document = parse_document("#1=IFCPOLYLINE((#12,\n #4294967296,#3));");
    let instances = instances(&document);
    let ParameterValue::List { items, .. } = &instances.instances[0].parameters[0] else {
        panic!("reference aggregate expected");
    };
    assert_eq!(
        items,
        &vec![
            ParameterValue::Reference {
                id: 12,
                range: Range::new(Position::new(0, 16), Position::new(0, 19))
            },
            ParameterValue::Unknown {
                range: Range::new(Position::new(1, 1), Position::new(1, 12))
            },
            ParameterValue::Reference {
                id: 3,
                range: Range::new(Position::new(1, 13), Position::new(1, 15))
            },
        ]
    );
}

fn instances(document: &Document) -> EntityInstanceCollection {
    build_entity_instances(document.statement_index.as_deref(), &document.text)
}

fn position_at(text: &str, needle: &str) -> Position {
    let offset = text.find(needle).expect("needle should exist");
    let document = Document::new_unloaded(text.to_string());
    document
        .offset_to_position(offset)
        .expect("needle offset should convert to a position")
}

#[test]
fn instance_cache_reuses_instances_only_for_the_same_text_and_schema() {
    let mut document = Document::parse("#1=IFCWALL($);".to_string());
    let cache = EntityInstanceCache::default();

    let build =
        |document: &Document| cache.get_or_build_with(None, &document.text, || instances(document));
    let first = build(&document);
    let repeated = build(&document);
    assert!(Arc::ptr_eq(&first, &repeated));

    document
        .apply_content_changes(&[TextDocumentContentChangeEvent {
            range: Some(Range::new(Position::new(0, 6), Position::new(0, 10))),
            range_length: None,
            text: "DOOR".to_string(),
        }])
        .expect("replacement should parse");
    let edited = build(&document);
    let schema = Arc::new(SchemaDoc {
        entities: Default::default(),
        types: Default::default(),
    });
    let with_schema = cache.get_or_build_with(Some(&schema), &document.text, || {
        build_entity_instances(None, &document.text)
    });

    assert_eq!(first.instances[0].entity_name, "IFCWALL");
    assert_eq!(edited.instances[0].entity_name, "IFCDOOR");
    assert!(with_schema.instances.is_empty());
}

#[tokio::test]
async fn instance_cache_forgets_only_instances_of_the_given_text() {
    let cache = EntityInstanceCache::default();
    let cached = Arc::new("#1=IFCWALL($);".to_string());
    let other = Arc::new("#1=IFCWALL($);".to_string());
    let build = || build_entity_instances(None, &cached);
    let first = cache.get_or_build_with(None, &cached, build);

    cache.forget(&other);
    assert!(Arc::ptr_eq(
        &first,
        &cache.get_or_build_with(None, &cached, build)
    ));
    cache.forget(&cached);
    assert!(!Arc::ptr_eq(
        &first,
        &cache.get_or_build_with(None, &cached, build)
    ));
}

#[test]
fn instance_cache_builds_for_different_texts_never_overlap() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let cache = EntityInstanceCache::default();
    let in_flight = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    let texts: Vec<Arc<String>> = (0..4)
        .map(|i| Arc::new(format!("#{i}=IFCWALL($);")))
        .collect();

    std::thread::scope(|scope| {
        for text in &texts {
            scope.spawn(|| {
                cache.get_or_build_with(None, text, || {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    EntityInstanceCollection {
                        instances: Vec::new(),
                    }
                });
            });
        }
    });

    assert_eq!(peak.load(Ordering::SeqCst), 1);
}

#[test]
fn parse_builds_text_index_and_statement_index() {
    let text = "#1=IFCWALL($);";
    let document = parse_document(text);

    assert_eq!(document.text.as_str(), text);
    assert!(document.statement_index.is_some());
    assert_eq!(document.schema_name, None);
    assert_eq!(document.definitions.get(&1), Some(&0));
    assert_eq!(document.references.get(&1), Some(&vec![0]));
}

#[test]
fn parse_detects_schema_name_from_text_index() {
    let text = "ISO-10303-21;HEADER;FILE_SCHEMA(('IFC4X3_LOCAL_TEST'));ENDSEC;DATA;#1=IFCWALL($);ENDSEC;END-ISO-10303-21;";
    let document = parse_document(text);

    assert_eq!(document.schema_name.as_deref(), Some("IFC4X3_LOCAL_TEST"));
}

#[test]
fn node_at_position_finds_entity_name() {
    let text = "#1=IFCWALL($);";
    let document = parse_document(text);

    let (name, range) = document
        .entity_name_at_position(position_at(text, "IFCWALL"))
        .expect("entity name should exist");
    assert_eq!(name, "IFCWALL");
    assert_eq!(range, Range::new(Position::new(0, 3), Position::new(0, 10)));
}

#[test]
fn instance_id_at_position_resolves_clicks_on_the_definition_entity_name() {
    let text = "#1=IFCWALL($);#2=IFCSLAB(#1);";
    let document = parse_document(text);

    assert_eq!(
        document.instance_id_at_position(position_at(text, "IFCWALL")),
        Some(1)
    );
    assert_eq!(
        document.instance_id_at_position(position_at(text, "IFCSLAB")),
        Some(2)
    );
    assert_eq!(
        document.instance_id_at_position(position_at(text, "#1)")),
        Some(1)
    );
    assert_eq!(
        document.id_token_at_position(position_at(text, "IFCWALL")),
        None
    );
}

#[test]
fn instance_id_at_position_ignores_a_definition_written_in_a_comment_or_string() {
    let text = "#1=IFCWALL($);/* #1=IFCDOOR($); */#2=IFCSLAB('#1=IFCROOF');";
    let document = parse_document(text);

    for name in ["IFCDOOR", "IFCROOF"] {
        assert_eq!(
            document.instance_id_at_position(position_at(text, name)),
            None,
            "{name}"
        );
    }
    assert_eq!(
        document.instance_id_at_position(position_at(text, "IFCWALL")),
        Some(1)
    );
}

#[test]
fn node_at_position_finds_reference() {
    let text = "#1=IFCWALL(#2,/* structured */$);";
    let document = parse_document(text);

    assert_eq!(
        document.id_token_at_position(position_at(text, "#2")),
        Some((2, 11))
    );
    assert_eq!(
        document.id_range_at_offset(11),
        Some(Range::new(Position::new(0, 11), Position::new(0, 13)))
    );
}

#[test]
fn compact_parameter_lists_match_structured_parsing() {
    let cases = [
        "#1=IFCPROPERTYSINGLEVALUE('Name',$,IFCLABEL~('it''s'),*);",
        "#2=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.~);",
        "#3=IFCX((1.,-2.5E-3,.5),(#1,#22),(),('a',IFCREAL(1.),*),~#4294967296,.T.,'/');",
        "#4=IFCX(\n  'a\nb',\n~#5,\n  (1,\n2), ( .A. , $ )\n);",
        "#5=IFCX(IFCBOOLEAN~(.T.),IFCINTEGER(-3),(IFCLABEL ( '' ),*));",
        "#6=IFCX(~);",
        "#7=IFCX(((1.,2.),(3.,\n4.)),((#1),('a',IFCLABEL('b'),*),()),~$);",
    ];
    let texts = cases.iter().flat_map(|case| {
        [
            case.to_string(),
            format!("DATA;\n{case}\n{case}\nENDSEC;\n"),
        ]
    });
    for case in texts {
        let compact = parse_document(&case.replace('~', " "));
        let structured = parse_document(&case.replace('~', "\u{c}"));
        assert!(
            compact.statement_index.as_ref().unwrap().errors.is_empty(),
            "{case}"
        );
        assert!(
            structured
                .statement_index
                .as_ref()
                .unwrap()
                .errors
                .is_empty(),
            "{case}"
        );
        assert_eq!(
            instances(&compact).instances,
            instances(&structured).instances,
            "{case}"
        );

        for offset in 0..=case.len() {
            let position = compact.offset_to_position(offset).unwrap();
            let lookup = |document: &Document| {
                let parsed = document.statement_index.as_ref().unwrap();
                let context = parsed.statements.iter().enumerate().find_map(|(index, s)| {
                    let tape = parsed.decode(document.text.as_bytes(), index);
                    let mut slot = 1;
                    let mut parameter_index = 0;
                    while slot < tape.len() {
                        let p = tape[slot];
                        if p.span.range().contains(&offset) {
                            return Some((
                                s.id,
                                document.text[s.name.range()].to_ascii_uppercase(),
                                parameter_index,
                            ));
                        }
                        slot = p.next as usize;
                        parameter_index += 1;
                    }
                    None
                });
                let mut omitted = None;
                let mut typed_name = None;
                for index in 0..parsed.statements.len() {
                    for p in parsed.decode(document.text.as_bytes(), index) {
                        if !p.span.range().contains(&offset) {
                            continue;
                        }
                        if p.kind == step::Kind::Omitted {
                            omitted = Some((p.span.start, p.span.end));
                        }
                        if p.kind == step::Kind::Typed {
                            let start = p.span.start as usize;
                            let source = &document.text[p.span.range()];
                            let end = start + source[..source.find('(').unwrap()].trim_end().len();
                            if offset < end {
                                typed_name = Some((start, end));
                            }
                        }
                    }
                }
                Some((
                    context,
                    omitted,
                    typed_name,
                    document.entity_name_at_position(position),
                    document.id_token_at_position(position),
                    instances(document)
                        .instances
                        .into_iter()
                        .flat_map(|i| i.parameters)
                        .filter(|p| p.range().start <= position && position < p.range().end)
                        .collect::<Vec<_>>(),
                ))
            };
            assert_eq!(lookup(&compact), lookup(&structured), "{case} at {offset}");
        }
    }
}

#[test]
fn lowercase_names_are_accepted_only_for_typed_values() {
    let document =
        parse_document("DATA;#1=IFCX(ifclabel('a'),ifcCountMeasure(3),typed_1(.T.));ENDSEC;");
    assert!(document.statement_index.as_ref().unwrap().errors.is_empty());
    let collection = instances(&document);
    let names: Vec<_> = collection.instances[0]
        .parameters
        .iter()
        .map(|value| match value {
            ParameterValue::Typed { type_name, .. } => type_name.as_str(),
            other => panic!("expected typed value, got {other:?}"),
        })
        .collect();
    assert_eq!(names, ["IFCLABEL", "IFCCOUNTMEASURE", "TYPED_1"]);
    for text in [
        "DATA;#1=x(1.);ENDSEC;",
        "DATA;#1=X(.lower.);ENDSEC;",
        "DATA;#1=X(_typed(.T.));ENDSEC;",
    ] {
        let rejected = parse_document(text);
        assert!(
            !rejected.statement_index.as_ref().unwrap().errors.is_empty(),
            "{text}"
        );
    }
}

#[test]
fn documents_with_syntax_errors_recover_after_edit_and_regress_after_undo() {
    let invalid = "DATA;\n#1=IFCPROPERTYSINGLEVALUE('Name',$,IFCLABEL'x'),$);\n#2=IFCPROPERTYSET('g',#3,'n',$,(#1));\nENDSEC;\n";
    let mut document = parse_document(invalid);
    assert!(!document.statement_index.as_ref().unwrap().errors.is_empty());

    let label = Position::new(1, 43);
    let edit = |range: Range, text: &str| vec![change(Some(range), text)];
    document
        .apply_content_changes(&edit(Range::new(label, label), "("))
        .unwrap();
    assert!(document.statement_index.as_ref().unwrap().errors.is_empty());
    assert_eq!(instances(&document).instances.len(), 2);

    document
        .apply_content_changes(&edit(Range::new(label, Position::new(1, 44)), ""))
        .unwrap();
    assert_eq!(document.text.as_str(), invalid);
    assert!(!document.statement_index.as_ref().unwrap().errors.is_empty());
    assert_eq!(
        document.statement_index.as_deref(),
        Some(&step::Document::parse(invalid.as_bytes(), false))
    );
}

#[test]
fn text_index_records_definition_and_reference_occurrences() {
    let text = "#1=IFCWALL(#2);\n#2=IFCDOOR(#1);";
    let document = parse_document(text);

    assert_eq!(document.definitions.get(&1), Some(&0));
    assert_eq!(document.definitions.get(&2), Some(&16));
    assert_eq!(document.references.get(&1), Some(&vec![0, 27]));
    assert_eq!(document.references.get(&2), Some(&vec![11, 16]));
}

#[test]
fn position_conversion_uses_utf16_columns() {
    let document = Document::new_unloaded("a😀b\n#1=IFCWALL($);".to_string());

    assert_eq!(document.position_to_offset(Position::new(0, 3)), Some(5));
    assert_eq!(document.offset_to_position(5), Some(Position::new(0, 3)));
    assert_eq!(document.position_to_offset(Position::new(1, 0)), Some(7));
}

#[test]
fn instance_by_id_returns_matching_instance() {
    let text = "#1=IFCWALL($);\n#2=IFCDOOR($);";
    let document = parse_document(text);

    let instance = document
        .entity_instance_by_id(2)
        .expect("instance should exist");

    assert_eq!(instance.entity_name, "IFCDOOR");
    assert_eq!(
        instance.entity_range,
        Range::new(Position::new(1, 0), Position::new(1, 14))
    );
}

#[test]
fn unload_parse_state_preserves_text_index_and_clears_statement_index() {
    let text = "#1=IFCWALL(#2);";
    let mut document = parse_document(text);

    document.unload_parse_state();

    assert_eq!(document.text.as_str(), text);
    assert!(!document.is_parse_state_loaded());
    assert_eq!(document.schema_name, None);
    assert_eq!(document.definitions.get(&1), Some(&0));
    assert_eq!(document.references.get(&2), Some(&vec![11]));
}

#[test]
fn reload_parse_state_rebuilds_derived_state_from_existing_text() {
    let text = "#1=IFCWALL(#2);\n#2=IFCDOOR($);";
    let mut document = parse_document(text);
    document.unload_parse_state();
    document.reload_parse_state();

    assert_eq!(document.text.as_str(), text);
    assert!(document.is_parse_state_loaded());
    assert!(document.definitions.contains_key(&1));
    assert!(document.definitions.contains_key(&2));
    assert_eq!(document.references[&2], vec![11, 16]);
    assert_eq!(
        document
            .entity_instance_by_id(2)
            .expect("instance should be indexed")
            .entity_name,
        "IFCDOOR"
    );
}

#[test]
fn statement_index_stays_in_step_with_edits() {
    let mut document =
        Document::new_unloaded("DATA;\n#1=IFCWALL('a');\n#2=IFCSLAB($);\nENDSEC;\n".into());
    document.reload_parse_state();

    let edits = [
        (Position::new(1, 11), Position::new(1, 13), "'b"),
        (Position::new(2, 10), Position::new(2, 10), "X"),
        (Position::new(0, 5), Position::new(1, 0), "\n\n"),
        (Position::new(3, 10), Position::new(3, 11), ""),
    ];
    for (start, end, text) in edits {
        document
            .apply_content_changes(&[change(Some(Range::new(start, end)), text)])
            .expect("change should be valid");
        assert_eq!(
            document.statement_index.as_deref(),
            Some(&step::Document::parse(document.text.as_bytes(), false)),
            "{:?}",
            document.text
        );
    }

    document
        .apply_content_changes(&[change(None, "#1=IFCWALL($);")])
        .expect("change should be valid");
    assert_eq!(instances(&document).instances[0].entity_name, "IFCWALL");
}

#[test]
fn incremental_changes_are_applied_sequentially() {
    let mut document = parse_document("#1=IFCWALL($);");
    let changes = [
        change(
            Some(Range::new(Position::new(0, 6), Position::new(0, 10))),
            "SLAB",
        ),
        change(
            Some(Range::new(Position::new(0, 11), Position::new(0, 11))),
            "#2,",
        ),
    ];

    document
        .apply_content_changes(&changes)
        .expect("changes should be valid");

    assert_eq!(document.text.as_str(), "#1=IFCSLAB(#2,$);");
    assert_eq!(document.references.get(&2), Some(&vec![11]));
    assert_eq!(instances(&document).instances[0].entity_name, "IFCSLAB");
    assert!(document.is_parse_state_loaded());
}

#[test]
fn incremental_change_ranges_use_utf16_columns() {
    let mut document = parse_document("#1=IFCWALL('😀');");
    let changes = [change(
        Some(Range::new(Position::new(0, 12), Position::new(0, 14))),
        "door",
    )];

    document
        .apply_content_changes(&changes)
        .expect("change should be valid");

    assert_eq!(document.text.as_str(), "#1=IFCWALL('door');");
    assert!(document.is_parse_state_loaded());
}

#[test]
fn full_text_change_falls_back_to_full_parse() {
    let mut document = parse_document("#1=IFCWALL($);");

    document
        .apply_content_changes(&[change(None, "#2=IFCDOOR($);")])
        .expect("change should be valid");

    assert_eq!(document.text.as_str(), "#2=IFCDOOR($);");
    assert!(document.definitions.contains_key(&2));
    assert_eq!(instances(&document).instances[0].entity_name, "IFCDOOR");
}

#[test]
fn clamped_position_to_offset_clamps_past_line_and_document_end() {
    let document = parse_document("ab😀\ncd\n");

    assert_eq!(
        document.clamped_position_to_offset(Position::new(0, 1)),
        Some(1)
    );
    assert_eq!(
        document.clamped_position_to_offset(Position::new(0, 99)),
        Some(6)
    );
    assert_eq!(
        document.clamped_position_to_offset(Position::new(1, 99)),
        Some(9)
    );
    assert_eq!(
        document.clamped_position_to_offset(Position::new(99, 0)),
        Some(document.text.len())
    );
    assert_eq!(document.position_to_offset(Position::new(0, 99)), None);
}

#[test]
fn invalid_incremental_range_keeps_derived_state_consistent() {
    let mut document = parse_document("#1=IFCWALL($);");
    let invalid_range = Range::new(Position::new(2, 0), Position::new(2, 1));

    let result = document.apply_content_changes(&[change(Some(invalid_range), "x")]);

    assert_eq!(result, Err(ApplyChangeError::InvalidRange(invalid_range)));
    assert_eq!(document.text.as_str(), "#1=IFCWALL($);");
    assert!(document.definitions.contains_key(&1));
    assert_eq!(instances(&document).instances[0].entity_name, "IFCWALL");
    assert!(document.is_parse_state_loaded());
}

#[test]
fn applies_an_edit_on_the_empty_last_line_inside_an_unterminated_comment() {
    let mut document = parse_document("#1=X(1);/*\n");
    let last_line = Range::new(Position::new(1, 0), Position::new(1, 0));

    let result = document.apply_content_changes(&[change(Some(last_line), "*/#2=X(#1);")]);

    assert_eq!(result, Ok(()));
    assert_eq!(document.text.as_str(), "#1=X(1);/*\n*/#2=X(#1);");
    assert!(document.definitions.contains_key(&2));
}

#[test]
fn edits_match_full_scan_at_token_boundaries() {
    let fixtures = [
        "FILE_SCHEMA(('IFC4'));\r\n#1 = IFCWALL('a''b😀\\X2\\00E4\\X0\\',12.34,#2); /* ab */\r\n#2=IFCDOOR(#1);",
        "FILE_SCHEMA /* x; */ (('IFC''4'));\n#1=ABC(#42949672960,1ABC,1FILE_SCHEMA,'a');#2=X(#1);",
        "FILE_SCHEMA #1=ABC('IFC4');\n#2=ABC(12,#1);",
        "FILE_SCHEMA;\n#1=ABC('IFC4');\n#2=ABC(12,#1);",
        "FILE_SCHEMA(('IFC4'));\n/*\n#1=ABC('fake');\n*/\n#1=ABC('real');\n#1=ABC('last');",
        "FILE_SCHEMA(('IFC4'));\n#1=ABC('unterminated\n#2=ABC('x');",
        "FILE_SCHEMA(('IFC4'));\n#1=ABC(12);/* unterminated\n#2=ABC(3);\n",
        "FILE_SCHEMA(('IFC4'));\n#1=ABC(12);/* a*x/ #5=ABC(#1); */\n",
        "FILE_SCHEMA(('IFC4'));\n#1=ABC#2=XYZ#1;\n#3=ABC(12#2);",
        "FILE_SCHEMA(('IFC4'));\n#1=ABC('','''abc',#2);\n#2=ABC(1);",
    ];
    let replacements = [
        "", "x", "9", "_", " ", "\t", "\n", "'", "''", "/*", "*/", "#12=", ";", "😀", "\\X2\\",
    ];
    for text in fixtures {
        let original = Document::new_unloaded(text.to_string());
        let boundaries: Vec<_> = text
            .char_indices()
            .map(|(i, _)| i)
            .chain([text.len()])
            .collect();
        for (i, &start) in boundaries.iter().enumerate() {
            for &end in boundaries[i..].iter().take(4) {
                let range = Range::new(
                    original.offset_to_position(start).unwrap(),
                    original.offset_to_position(end).unwrap(),
                );
                if original.position_to_offset(range.start) != Some(start)
                    || original.position_to_offset(range.end) != Some(end)
                {
                    continue;
                }
                for replacement in replacements {
                    let mut document = Document::new_unloaded(text.to_string());
                    document
                        .apply_content_changes(&[change(Some(range), replacement)])
                        .unwrap();
                    document.assert_text_index_matches_scan();
                }
            }
        }
    }
}

#[test]
fn random_edit_sequences_match_full_scan() {
    let text = "FILE_SCHEMA(('IFC4'));\n#1 = IFCWALL('Name😀 a''b \\X2\\00E4\\X0\\',12.34,#2); /* comment */\n#2=IFCDOOR(#1);\n";
    let replacements = [
        "a", "Name", "IFCDOOR", "1", "99", "#2", "#12=", " ", "\t", "\n", "", ";", "'", "''", "/*",
        "*/", "😀", "\\X2\\",
    ];
    for seed in 1..=128u64 {
        let mut state = seed;
        let mut random = |limit: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as usize % limit
        };
        let mut document = Document::new_unloaded(text.to_string());
        for step in 0..128 {
            if step % 16 == 0 {
                document = Document::new_unloaded(text.to_string());
            }
            let mut changes = Vec::new();
            let before = document.text.to_string();
            for _ in 0..1 + random(3) {
                let boundaries: Vec<_> = document
                    .text
                    .char_indices()
                    .map(|(i, _)| i)
                    .chain([document.text.len()])
                    .collect();
                let i = random(boundaries.len());
                let start = boundaries[i];
                let end = boundaries[(i + random(4)).min(boundaries.len() - 1)];
                let range = Range::new(
                    document.offset_to_position(start).unwrap(),
                    document.offset_to_position(end).unwrap(),
                );
                if document.position_to_offset(range.start) != Some(start)
                    || document.position_to_offset(range.end) != Some(end)
                {
                    continue;
                }
                let replacement = replacements[random(replacements.len())];
                let event = change(Some(range), replacement);
                document
                    .apply_content_changes(std::slice::from_ref(&event))
                    .unwrap();
                document.assert_text_index_matches_scan();
                changes.push(event);
            }
            let mut batched = Document::new_unloaded(before);
            batched.apply_content_changes(&changes).unwrap();
            assert_eq!(batched.text, document.text, "seed {seed}");
            batched.assert_text_index_matches_scan();
        }
    }
}

#[test]
fn discovers_entities_inside_error_recovery() {
    let document = parse_document(concat!(
        "DATA;\n",
        "#1=IFCCARTESIANPOINT((0.,1.,2.));\n",
        "#2=IFCPOLYLINE((#1,#4))\n",
        "#3=IFCDIRECTION((1.,0.,0.));\n",
        "#4=IFCWALL('a',(#2,#3);\n",
        "#5=IFCDIRECTION((1.,0.,0.));\n",
        "#6=IFCWALL('b',$) stray;\n",
        "#7=IFCCARTESIANPOINT((3.,4.,5.));\n",
        "#8=IFCWALL(g',$,.T.,IFCLABEL('x'),(#1,#2),*);\n",
        "#9=IFCDIRECTION((1.,0.,0.));\n",
        "ENDSEC;\n",
    ));
    let instances = instances(&document);
    let discovered: Vec<_> = instances
        .instances
        .iter()
        .map(|instance| {
            (
                instance.id,
                instance.entity_name.as_str(),
                instance.parameters.len(),
            )
        })
        .collect();

    assert_eq!(
        discovered,
        [
            (Some(1), "IFCCARTESIANPOINT", 1),
            (Some(2), "IFCPOLYLINE", 1),
            (Some(3), "IFCDIRECTION", 1),
            (Some(4), "IFCWALL", 2),
            (Some(5), "IFCDIRECTION", 1),
            (Some(6), "IFCWALL", 2),
            (Some(7), "IFCCARTESIANPOINT", 1),
            (Some(9), "IFCDIRECTION", 1),
        ]
    );
}

#[test]
fn discovers_entities_after_malformed_syntax() {
    let document = parse_document(concat!(
        "DATA;\n",
        "#1=IFCCARTESIANPOINT((0.,1.,2.));\n",
        "#2=IFCPOLYLINE((#1,#4))\n",
        "#3=IFCDIRECTION((1.,0.,0.));\n",
        "#4=IFCWALL('a',IFCLABEL('x',(#2,#3));\n",
        "#5=IFCDIRECTION((1.,0.,0.));\n",
        "#6=IFCWALL('b',$,%);\n",
        "#7=IFCCARTESIANPOINT((3.,4.,5.));\n",
        "ENDSEC;\n",
    ));
    let ids: Vec<_> = instances(&document)
        .instances
        .iter()
        .map(|instance| instance.id)
        .collect();

    assert_eq!(ids, (1..=7).map(Some).collect::<Vec<_>>());
}
