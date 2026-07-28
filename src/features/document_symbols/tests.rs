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
        ("IFCOPENINGELEMENT", product.as_slice()),
        ("IFCWINDOW", product.as_slice()),
        ("IFCDOOR", product.as_slice()),
        ("IFCDISTRIBUTIONELEMENT", product.as_slice()),
        ("IFCDISTRIBUTIONPORT", product.as_slice()),
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
    let voids_attributes = [
        "A",
        "B",
        "C",
        "D",
        "RelatingBuildingElement",
        "RelatedOpeningElement",
    ];
    entities.insert(
        "IFCRELVOIDSELEMENT".into(),
        entity("IFCRELVOIDSELEMENT", &[], &voids_attributes),
    );
    let fills_attributes = [
        "A",
        "B",
        "C",
        "D",
        "RelatingOpeningElement",
        "RelatedBuildingElement",
    ];
    entities.insert(
        "IFCRELFILLSELEMENT".into(),
        entity("IFCRELFILLSELEMENT", &[], &fills_attributes),
    );
    let port_attributes = ["A", "B", "C", "D", "RelatingPort", "RelatedElement"];
    entities.insert(
        "IFCRELCONNECTSPORTTOELEMENT".into(),
        entity("IFCRELCONNECTSPORTTOELEMENT", &[], &port_attributes),
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
    let DocumentSymbolResponse::Nested(symbols) = document_symbols(doc, Some(&schema())).unwrap()
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
    assert!(all.iter().any(|n| n == "IFCSPACE #5"));
    assert!(all.iter().any(|n| n == "IFCELEMENTASSEMBLY #6"));
    assert!(all.iter().any(|n| n == "IFCWALL #7"));
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
    assert_eq!(out.iter().filter(|s| s.name.starts_with("IFC")).count(), 2);
    assert!(out.iter().any(|s| s.name == "IFCPROJECT #2"));
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
    assert!(all.iter().any(|n| n == "IFCSPACE #2"));
    assert_eq!(all.iter().filter(|n| *n == "IFCWALL #3").count(), 1);
    assert_eq!(all.iter().filter(|n| *n == "IFCWALL #4").count(), 1);
    let project = &out[0];
    assert_eq!(project.name, "IFCPROJECT #1");
    assert_eq!(project.detail.as_deref(), Some("😀"));
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
    assert_eq!(names(&out[0]), vec!["IFCPROJECT #1", "IFCSITE #2"]);
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
    let assembly = &space.children.as_ref().unwrap()[0];
    assert_eq!(assembly.name, "IFCELEMENTASSEMBLY #3");
    assert_eq!(assembly.children.as_ref().unwrap()[0].name, "IFCWALL #4");
    assert_eq!(
        names(&out[0])
            .iter()
            .filter(|name| *name == "IFCWALL #4")
            .count(),
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
            "IFCPROJECT #1",
            "  IFCBUILDINGSTOREY #2",
            "    IFCSPACE #3",
            "      IFCFURNISHINGELEMENT #5",
            "    IFCWALL #4",
        ]
    );
}

#[test]
fn openings_and_fillings_attach_to_their_host_element() {
    let doc = parse(
        "DATA;\n#1=IFCPROJECT('g',$,'Project',$,$,$,$,$,$);\n#2=IFCBUILDINGSTOREY('g',$,'Storey',$,$,$,$,$,$);\n#3=IFCWALL('g',$,'Wall',$,$,$,$,$,$);\n#4=IFCOPENINGELEMENT('g',$,'Opening',$,$,$,$,$,$);\n#5=IFCWINDOW('g',$,'Window',$,$,$,$,$,$);\n#10=IFCRELAGGREGATES('g',$,$,$,#1,(#2));\n#11=IFCRELCONTAINEDINSPATIALSTRUCTURE('g',$,$,$,(#3),#2);\n#12=IFCRELVOIDSELEMENT('g',$,$,$,#3,#4);\n#13=IFCRELFILLSELEMENT('g',$,$,$,#4,#5);\nENDSEC;",
    );
    let symbols = nested_symbols(&doc);
    assert_eq!(
        structure(&symbols),
        [
            "IFCPROJECT #1",
            "  IFCBUILDINGSTOREY #2",
            "    IFCWALL #3",
            "      IFCOPENINGELEMENT #4",
            "        IFCWINDOW #5",
        ]
    );
    assert!(
        !structure(&symbols)
            .iter()
            .any(|item| item.contains("Uncontained products"))
    );
}

#[test]
fn distribution_ports_connect_to_their_related_element() {
    let doc = parse(
        "DATA;\n#1=IFCPROJECT('g',$,'Project',$,$,$,$,$,$);\n#2=IFCBUILDINGSTOREY('g',$,'Storey',$,$,$,$,$,$);\n#3=IFCDISTRIBUTIONELEMENT('g',$,'Terminal',$,$,$,$,$,$);\n#4=IFCDISTRIBUTIONPORT('g',$,'Supply',$,$,$,$,$,$,.SOURCE.,.DUCT.,.AIRCONDITIONING.);\n#10=IFCRELAGGREGATES('g',$,$,$,#1,(#2));\n#11=IFCRELCONTAINEDINSPATIALSTRUCTURE('g',$,$,$,(#3),#2);\n#12=IFCRELCONNECTSPORTTOELEMENT('g',$,$,$,#4,#3);\nENDSEC;",
    );
    let symbols = nested_symbols(&doc);
    assert_eq!(
        structure(&symbols),
        [
            "IFCPROJECT #1",
            "  IFCBUILDINGSTOREY #2",
            "    IFCDISTRIBUTIONELEMENT #3",
            "      IFCDISTRIBUTIONPORT #4",
        ]
    );
    assert!(
        !structure(&symbols)
            .iter()
            .any(|item| item.contains("Uncontained products"))
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
            "  IFCELEMENTASSEMBLY #2",
            "    IFCELEMENTASSEMBLY #1",
        ]
    );
    for name in ["IFCELEMENTASSEMBLY #1", "IFCELEMENTASSEMBLY #2"] {
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
    assert_eq!(structure(&nested_symbols(&doc)), ["IFCPROJECT #1"]);
}

#[test]
fn keeps_multiple_projects_and_orphan_space_roots_in_source_order() {
    let doc = parse(
        "DATA;\n#1=IFCPROJECT('g',$,'First',$,$,$,$,$,$);\n#2=IFCSPACE('g',$,'Orphan',$,$,$,$,$,$);\n#3=IFCPROJECT('g',$,'Second',$,$,$,$,$,$);\nENDSEC;",
    );
    assert_eq!(
        structure(&nested_symbols(&doc)),
        ["IFCPROJECT #1", "IFCSPACE #2", "IFCPROJECT #3"]
    );
}

#[test]
fn includes_schema_compatible_ifc4x3_spatial_subtypes_as_spatial_nodes() {
    let doc = parse(
        "DATA;\n#1=IFCPROJECT('g',$,'P',$,$,$,$,$,$);\n#2=IFCFACILITY('g',$,'Rail Facility',$,$,$,$,$,$);\n#10=IFCRELAGGREGATES('g',$,$,$,#1,(#2));\nENDSEC;",
    );
    assert_eq!(
        structure(&nested_symbols(&doc)),
        ["IFCPROJECT #1", "  IFCFACILITY #2"]
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
            "IFCPROJECT #1",
            "  IFCSITE #2",
            "    IFCBUILDING #3",
            "      IFCBUILDINGSTOREY #4",
            "        IFCSITE (2)",
            "          IFCSITE #5",
            "          IFCSITE #6",
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
        ["IFCPROJECT #1", "  IFCSITE #2", "  IFCBUILDINGSTOREY #3"]
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
        ["IFCPROJECT #1", "  IFCBUILDINGSTOREY #2", "    IFCSITE #3"]
    );
    assert_eq!(
        symbols[0].children.as_ref().unwrap()[0]
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
        if symbol.kind != SymbolKind::ARRAY && symbol.kind != SymbolKind::NULL {
            assert!(
                contains(symbol.range, symbol.selection_range),
                "selection for {} is outside its statement range",
                symbol.name
            );
        }
        if symbol.kind == SymbolKind::ARRAY {
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
            "IFCPROJECT #1",
            "  IFCSPACE #2",
            "    IFCWALL #3",
            "Uncontained products",
            "  IFCFURNISHINGELEMENT #4",
        ]
    );
    for symbol in &symbols {
        verify(symbol);
    }
}
