use super::*;

#[test]
fn parsing_rejects_the_offset_boundary_with_an_explicit_error() {
    let text = b"#1=X(1);#2=Y(2);";
    for retain in [false, true] {
        assert_eq!(
            Document::parse_with_limit(text, retain, text.len() + 1),
            Document::parse(text, retain)
        );
        for limit in [text.len(), text.len() - 1] {
            let doc = Document::parse_with_limit(text, retain, limit);
            assert!(doc.statements.is_empty());
            assert!(doc.ids.is_empty());
            assert!(doc.params.is_empty());
            assert_eq!(
                doc.errors,
                [ParseError {
                    span: Span::default(),
                    message: DOCUMENT_TOO_LARGE,
                }]
            );
        }
    }
}

#[test]
fn edits_reject_the_offset_boundary_before_shifting_and_can_recover() {
    let original = b"#1=X(1);#2=Y(2);#3=Z(3);#4=W(4);";
    let start = 5;
    let limit = original.len() + 2;
    for retain in [false, true] {
        for insert in ["0", "00", "000"] {
            let mut text = original.to_vec();
            text.splice(start..start, insert.bytes());
            let mut doc = Document::parse(original, retain);
            let outcome = doc.edit_with_limit(
                b"",
                &text,
                Edit {
                    start,
                    delete_len: 0,
                    insert_len: insert.len(),
                },
                limit,
            );
            if text.len() < limit {
                assert!(matches!(outcome, EditOutcome::Local { .. }));
                assert_eq!(doc, Document::parse(&text, retain));
            } else {
                assert_eq!(outcome, EditOutcome::Rebuilt);
                assert!(doc.statements.is_empty());
                assert!(doc.ids.is_empty());
                assert!(doc.params.is_empty());
                assert_eq!(
                    doc.errors,
                    [ParseError {
                        span: Span::default(),
                        message: DOCUMENT_TOO_LARGE,
                    }]
                );
            }
            doc.edit_with_limit(
                insert.as_bytes(),
                original,
                Edit {
                    start,
                    delete_len: insert.len(),
                    insert_len: 0,
                },
                limit,
            );
            assert_eq!(doc, Document::parse(original, retain));
        }
    }
}

#[test]
fn section_keywords_require_a_terminator_without_extra_content() {
    for keyword in [
        "ISO-10303-21",
        "END-ISO-10303-21",
        "HEADER",
        "ENDSEC",
        "DATA",
        "ANCHOR",
        "REFERENCE",
        "SIGNATURE",
    ] {
        for retain in [false, true] {
            for suffix in [";", " /* ; #7=X(); */ ;"] {
                let text = format!("{keyword}{suffix}");
                assert!(
                    Document::parse(text.as_bytes(), retain).errors.is_empty(),
                    "{text}"
                );
            }
            for suffix in ["", " garbage;", "($);", "('part',('IFC4'));"] {
                let text = format!("{keyword}{suffix}");
                let doc = Document::parse(text.as_bytes(), retain);
                assert_eq!(doc.errors.len(), 1, "{text}: {:?}", doc.errors);
                assert_eq!(doc.errors[0].message, "expected ;", "{text}");
            }
        }
    }
}

#[test]
fn a_section_missing_its_terminator_recovers_a_minified_instance() {
    for keyword in ["DATA", "HEADER", "ENDSEC", "END-ISO-10303-21"] {
        for retain in [false, true] {
            let text = format!("{keyword} /* #8=X(); */ #1=IFCCARTESIANPOINT((0.,0.,0.));ENDSEC;");
            let doc = Document::parse(text.as_bytes(), retain);
            assert_eq!(doc.errors.len(), 1, "{text}: {:?}", doc.errors);
            assert_eq!(doc.errors[0].message, "expected ;");
            assert_eq!(doc.statements.len(), 1, "{text}");
            assert_eq!(doc.statements[0].id, 1);
            assert_eq!(&text[doc.statements[0].name.range()], "IFCCARTESIANPOINT");
            assert_eq!(
                doc.statements[0].span.start as usize,
                text.find("#1=").unwrap()
            );
            assert_eq!(doc.decode(text.as_bytes(), 0).len(), 5);
        }
    }
    let doc = Document::parse(b"DATA ENDSEC;", false);
    assert_eq!(doc.errors.len(), 1);
    assert!(!doc.unclosed_section);
}

#[test]
fn nested_values_decode_the_same_eagerly_and_on_demand() {
    let text = b"ISO-10303-21;HEADER;FILE_DESCRIPTION(('x''y'),'2;1');ENDSEC;DATA;#7=X((#1,$,(1,.T.)),typed('abc'),/*x*/-3.2E-4,*);ENDSEC;END-ISO-10303-21;";
    let tape = Document::parse(text, true);
    let index = Document::parse(text, false);
    assert!(tape.errors.is_empty(), "{:?}", tape.errors);
    assert_eq!(tape.statements.len(), 1);
    assert_eq!(tape.decode(text, 0), index.decode(text, 0));
    assert_eq!(tape.params[0].next as usize, tape.params.len());
    assert!(tape.params.iter().any(|p| p.kind == Kind::Typed));
}

#[test]
fn a_local_edit_orders_errors_like_a_full_parse() {
    let original = "#1321=IFCMATERIALLAYER(#1319=IFCAXIS2PLACEMENT3D('Concrete Masonry,10303-21;\nFILE_DESCRIPTION ((''), '2;\u{a0}1');\n";
    let text = original.replacen("(#", "(##", 1);
    let mut doc = Document::parse(original.as_bytes(), false);

    let outcome = doc.edit(
        b"",
        text.as_bytes(),
        Edit {
            start: 23,
            delete_len: 0,
            insert_len: 1,
        },
    );

    assert!(matches!(outcome, EditOutcome::Local { .. }), "{outcome:?}");
    assert_eq!(doc, Document::parse(text.as_bytes(), false));
}

#[test]
fn edits_and_repairs_match_full_parse() {
    for original in [
        b"DATA;\n#1=X(1);\n#2=Y('hello',(#1,3));\n#3=Z(3);\nENDSEC;".as_slice(),
        b"DATA;\n#1=X(1);\n#2=Y(2,\n#3=Z(3);\n#4=W(4);\nENDSEC;",
        b"DATA;\n#1=X(1);\n#2=Y(2);/\n#3=Z(3);\n#4=W(4);\nENDSEC;",
        b"DATA;\n#1=X(1);/* a */\n#2=Y('b/*',*,'*/c');\n/*\n#3=Z(3);\n*/#4=W(4);\nENDSEC;",
    ] {
        edits_match_full_parse(original);
    }
}

fn edits_match_full_parse(original: &[u8]) {
    for retain in [true, false] {
        for start in 6..original.len() - 8 {
            for insert in [
                b" ".as_slice(),
                b"X",
                b"'",
                b";",
                b"",
                b"/*",
                b"*/",
                b"*",
                b"/",
            ] {
                let mut text = original.to_vec();
                text.splice(start..start + 1, insert.iter().copied());
                let mut doc = Document::parse(original, retain);
                doc.edit(
                    &original[start..start + 1],
                    &text,
                    Edit {
                        start,
                        delete_len: 1,
                        insert_len: insert.len(),
                    },
                );
                assert_eq!(doc, Document::parse(&text, retain), "{start} {insert:?}");
                doc.edit(
                    &text[start..start + insert.len()],
                    original,
                    Edit {
                        start,
                        delete_len: insert.len(),
                        insert_len: 1,
                    },
                );
                assert_eq!(doc, Document::parse(original, retain));
            }
        }
    }
}

#[test]
fn a_comment_opening_cut_by_the_end_of_the_range_is_unclosed_within_it() {
    let text = b" /*x*/";
    assert_eq!(trivia(text, 0, 2, &mut Lookahead::default()), 2);
    assert_eq!(closed_trivia(text, 0, 2, &mut Lookahead::default()), Err(1));
}

#[test]
fn only_comments_reaching_past_the_reparsed_statements_rebuild() {
    let original = b"DATA;\n#1=X(1);\n#2=Y('a',$);\n#3=Z(3);\n#4=W(4);\nENDSEC;";
    let string = original.iter().position(|b| *b == b'a').unwrap();
    let dollar = original.iter().position(|b| *b == b'$').unwrap();
    let after_two = string + 6;
    for (start, delete_len, insert, local) in [
        (string, 0, b"*".as_slice(), true),
        (string, 0, b"/", true),
        (string, 0, b"/*", true),
        (string, 0, b"*/", true),
        (dollar, 1, b"*", true),
        (after_two, 0, b"/* c */", true),
        (after_two, 0, b"/*", false),
        (dollar, 1, b"*/*", false),
    ] {
        let mut text = original.to_vec();
        text.splice(start..start + delete_len, insert.iter().copied());
        let mut doc = Document::parse(original, false);
        let outcome = doc.edit(
            &original[start..start + delete_len],
            &text,
            Edit {
                start,
                delete_len,
                insert_len: insert.len(),
            },
        );
        assert_eq!(
            matches!(outcome, EditOutcome::Local { .. }),
            local,
            "{:?}",
            std::str::from_utf8(&text)
        );
        assert_eq!(doc, Document::parse(&text, false));
    }
}

#[test]
fn id_lookups_after_an_edit_match_a_full_parse() {
    let original = b"DATA;\n#1=X(1);\n#2=Y(#1);\n#3=Z(#2);\n#4=W(4);\nENDSEC;".as_slice();
    for (from, to) in [
        (b"#2=Y".as_slice(), b"#9=Y".as_slice()),
        (b"#2=Y(#1);", b"#2=Y(#1);\n#5=V(5);"),
        (b"\n#2=Y(#1);", b""),
        (b"#2=Y(#1);\n#3", b"#3=Y(#1);\n#2"),
        (b"Y(#1)", b"Y(#3)"),
    ] {
        let start = original
            .windows(from.len())
            .position(|w| w == from)
            .unwrap();
        let mut text = original.to_vec();
        text.splice(start..start + from.len(), to.iter().copied());
        let mut doc = Document::parse(original, false);
        let outcome = doc.edit(
            from,
            &text,
            Edit {
                start,
                delete_len: from.len(),
                insert_len: to.len(),
            },
        );
        assert!(matches!(outcome, EditOutcome::Local { .. }));
        let fresh = Document::parse(&text, false);
        for id in 0..12 {
            assert_eq!(
                doc.find(id),
                fresh.find(id),
                "#{id} after {:?}",
                String::from_utf8_lossy(to)
            );
        }
    }
}

#[test]
fn an_edit_that_keeps_ids_costs_the_same_on_a_larger_document() {
    let document = |statements: u32| {
        let mut text = b"DATA;\n".to_vec();
        for id in 1..=statements {
            text.extend_from_slice(format!("#{id}=X(1);\n").as_bytes());
        }
        text.extend_from_slice(b"ENDSEC;\n");
        let digit = text.len() - b"1);\nENDSEC;\n".len();
        let doc = Document::parse(&text, false);
        (text, digit, doc)
    };
    let mut documents = [document(10_000), document(160_000)];
    let mut fastest = [std::time::Duration::MAX; 2];
    for n in 0..200 {
        let (old, new) = if n % 2 == 0 {
            (b"1", b"2")
        } else {
            (b"2", b"1")
        };
        for ((text, digit, doc), fastest) in documents.iter_mut().zip(&mut fastest) {
            text[*digit] = new[0];
            let start = std::time::Instant::now();
            doc.edit(
                old,
                text,
                Edit {
                    start: *digit,
                    delete_len: 1,
                    insert_len: 1,
                },
            );
            *fastest = start.elapsed().min(*fastest);
        }
    }
    assert_eq!(documents[1].2.find(160_000), Some(159_999));
    let [small, large] = fastest;
    assert!(
        large < small * 4,
        "16x the statements took {large:?} against {small:?}"
    );
}

#[test]
fn a_statement_wrapped_over_many_lines_parses_in_linear_time() {
    let growth = crate::test_support::growth_on_sixteenfold(
        2_500,
        |lines| {
            let mut text = b"DATA;\n#1=IFCCARTESIANPOINTLIST3D((\n".to_vec();
            for _ in 1..lines {
                text.extend_from_slice(b"(0.,0.,0.),\n");
            }
            text.extend_from_slice(b"(0.,0.,0.)\n));\nENDSEC;\n");
            text
        },
        |text| {
            let doc = Document::parse(text, false);
            assert!(doc.errors.is_empty(), "{:?}", doc.errors);
        },
    );
    assert!(
        growth < 4.0,
        "16x the lines took {growth:.1}x as long as 16 inputs"
    );
}

#[test]
fn comments_opening_again_and_again_inside_strings_parse_in_linear_time() {
    let growths = crate::test_support::growth_on_repeated_comment_openings(1_000, |text| {
        std::hint::black_box(Document::parse(text.as_bytes(), false));
    });
    assert!(
        growths.iter().all(|&(_, growth)| growth < 4.0),
        "16x the units took this many times as long as 16 inputs: {growths:?}"
    );
}

#[test]
fn statements_all_lacking_their_semicolon_parse_in_linear_time() {
    let growth = crate::test_support::growth_on_sixteenfold(
        1_000,
        |lines| {
            let mut text = b"DATA;\n".to_vec();
            for id in 1..=lines {
                text.extend_from_slice(format!("#{id}=IFCX(1)\n").as_bytes());
            }
            text
        },
        |text| {
            let doc = Document::parse(text, false);
            assert_eq!(doc.errors.len(), doc.statements.len());
        },
    );
    assert!(
        growth < 4.0,
        "16x the statements took {growth:.1}x as long as 16 inputs"
    );
}

#[test]
fn reports_an_unclosed_comment_at_its_opening() {
    let text = b"DATA;\n#1=X(1);\n/* never closed\n#2=Y(#3);\nENDSEC;\n";
    let doc = Document::parse(text, true);
    assert_eq!(
        doc.errors,
        [ParseError {
            span: Span { start: 15, end: 17 },
            message: "unclosed comment",
        }]
    );
    assert_eq!(doc.statements.len(), 1);
    edits_match_full_parse(text);
}

#[test]
fn reports_an_unclosed_comment_inside_a_statement_at_its_opening() {
    for (text, ids) in [
        (
            b"DATA;\n#1=X(1);\n#2=Y(1, /* never closed\n#3=Z(#2);\nENDSEC;\n".as_slice(),
            [1, 2].as_slice(),
        ),
        (
            b"HEADER;\nFILE_NAME('a', /* never closed\nENDSEC;\nDATA;\n#1=X(1);\nENDSEC;\n",
            [].as_slice(),
        ),
    ] {
        let doc = Document::parse(text, true);
        let comment = memchr::memmem::find(text, b"/*").unwrap() as u32;
        assert_eq!(
            doc.errors,
            [ParseError {
                span: Span {
                    start: comment,
                    end: comment + 2,
                },
                message: "unclosed comment",
            }]
        );
        assert!(!doc.unclosed_section);
        assert_eq!(doc.statements.iter().map(|s| s.id).collect::<Vec<_>>(), ids);
        edits_match_full_parse(text);
    }
}

#[test]
fn flags_a_section_left_open_where_the_text_ends() {
    for (text, open) in [
        (b"HEADER;\nENDSEC;\nDATA;\n#1=X(1);\n".as_slice(), true),
        (b"HEADER;\nFILE_SCHEMA(('IFC4'));\n", true),
        (b"HEADER;\nENDSEC;\nDATA;\n", true),
        (b"HEADER;\nENDSEC;\nDATA;\n#1=X(1);\nENDSEC;\n", false),
        (b"HEADER;\nENDSEC;\n", false),
        (b"", false),
        (b"DATA;\n#1=X('a);\nENDSEC;\n", false),
        (b"DATA;\n#1=X(1\n  endsec ;\n", false),
        (
            b"HEADER;\nFILE_NAME('a);\nENDSEC;\nDATA;\n#1=X(1);\nENDSEC;\n",
            false,
        ),
        (b"DATA;\n#1=X(1);\n/* ENDSEC;\n", false),
    ] {
        assert_eq!(
            Document::parse(text, false).unclosed_section,
            open,
            "{:?}",
            std::str::from_utf8(text)
        );
    }
    edits_match_full_parse(b"DATA;\n#1=X(1);\n#2=Y('a',2);\n#3=Z(3);\n#4=W(4);\n");
    edits_match_full_parse(b"DATA;\n#1=X(1);\n#2=Y('a',2);\nENDSEC;\n#3=Z(3);\n#4=W(4);\n");
}

#[test]
fn reports_a_section_opened_before_the_previous_one_ended() {
    let reopened = |text: &[u8]| {
        let doc = Document::parse(text, true);
        let keyword = memchr::memmem::rfind(text, b"DATA").unwrap() as u32;
        assert_eq!(
            doc.errors,
            [ParseError {
                span: Span {
                    start: keyword,
                    end: keyword + 4,
                },
                message: SECTION_REOPENED,
            }]
        );
        assert!(!doc.unclosed_section);
        assert_eq!(doc.statements.len(), 1);
        edits_match_full_parse(text);
    };
    reopened(b"HEADER;\nFILE_SCHEMA(('IFC4'));\nDATA;\n#1=X(1);\nENDSEC;\n");
    reopened(b"DATA;\nDATA;\n#1=X(1);\nENDSEC;\n");
    for text in [
        b"HEADER;\nENDSEC;\nDATA;\n#1=X(1);\nENDSEC;\n".as_slice(),
        b"DATA;\n#1=X(1);\nENDSEC;\nDATA;\n#2=X(2);\nENDSEC;\n",
        b"HEADER;\nENDSEC;\nDATA;\nENDSEC;\nDATA;\n#2=X(2);\nENDSEC;\n",
    ] {
        let doc = Document::parse(text, true);
        assert!(doc.errors.is_empty(), "{:?}", doc.errors);
        assert!(!doc.unclosed_section);
        edits_match_full_parse(text);
    }
}

#[test]
fn a_semicolon_and_a_definition_on_one_line_stay_inside_a_string() {
    let text = b"DATA;\n#1=X('b',$,IFCLABEL(';#9='),$);\n#2=X('a; #8 =');\nENDSEC;\n";
    let doc = Document::parse(text, true);
    assert!(doc.errors.is_empty(), "{:?}", doc.errors);
    assert_eq!(
        doc.statements.iter().map(|s| s.id).collect::<Vec<_>>(),
        [1, 2]
    );
    edits_match_full_parse(text);
}

#[test]
fn an_edit_ending_a_recovered_header_string_reparses_the_header() {
    let original = b"HEADER;\nFILE_NAME('a\n#4=b');\n#5=Y(2);\n#6=Z(3);\n";
    let equals = original.iter().position(|b| *b == b'=').unwrap();
    let mut text = original.to_vec();
    text.remove(equals);
    let mut doc = Document::parse(original, false);
    assert!(!doc.errors.is_empty());
    doc.edit(
        b"=",
        &text,
        Edit {
            start: equals,
            delete_len: 1,
            insert_len: 0,
        },
    );
    assert!(doc.errors.is_empty(), "{:?}", doc.errors);
    assert_eq!(doc, Document::parse(&text, false));
    edits_match_full_parse(original);
}

#[test]
fn skips_a_leading_byte_order_mark() {
    let text = b"\xEF\xBB\xBFDATA;\n#1=X(1);\n#2=Y('a',2);\n#3=Z(3);\nENDSEC;\n";
    let doc = Document::parse(text, true);
    assert!(doc.errors.is_empty(), "{:?}", doc.errors);
    assert_eq!(doc.statements.len(), 3);
    edits_match_full_parse(text);
}

#[test]
fn recovers_two_strings_and_an_independent_error() {
    let text = b"DATA;\n#1=X('bad);\n#2=X(1);\n#3=X('bad);\n#4=X(2x);\n#5=X('ok');\nENDSEC;";
    let doc = Document::parse(text, true);
    assert_eq!(doc.errors.len(), 3);
    assert_eq!(
        doc.errors
            .iter()
            .filter(|e| e.message == "unclosed string")
            .count(),
        2
    );
    assert_eq!(doc.statements.len(), 5);
    assert!(!doc.decode(text, 4).is_empty());
}

#[test]
fn corpus_parses_without_errors_and_both_modes_decode_alike() {
    fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for item in std::fs::read_dir(dir).unwrap() {
            let path = item.unwrap().path();
            if path.is_dir() {
                files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "ifc") {
                out.push(path);
            }
        }
    }
    let mut paths = Vec::new();
    files(std::path::Path::new("samples"), &mut paths);
    if let Some(dir) = std::env::var_os("STEP_CORPUS_DIR") {
        for name in [
            "055.867908MB__uni-project-house-50mb.ifc",
            "013.646137MB__Ifc4_Revit_ARC.ifc",
        ] {
            paths.push(std::path::Path::new(&dir).join(name));
        }
    }
    assert!(!paths.is_empty());
    for path in paths {
        let text = std::fs::read_to_string(&path).unwrap();
        let tape = Document::parse(text.as_bytes(), true);
        let index = Document::parse(text.as_bytes(), false);
        assert!(
            tape.errors.is_empty(),
            "{}: {:?}",
            path.display(),
            tape.errors.iter().take(5).collect::<Vec<_>>()
        );
        assert!(!tape.statements.is_empty(), "{}", path.display());
        for statement in 0..tape.statements.len() {
            let decoded = tape.decode(text.as_bytes(), statement);
            assert!(!decoded.is_empty(), "{}", path.display());
            assert_eq!(decoded, index.decode(text.as_bytes(), statement));
        }
    }
}
