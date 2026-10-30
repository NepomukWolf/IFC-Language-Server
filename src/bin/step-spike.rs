use ifc_language_server::{
    document_tree::DocumentTree,
    handwritten::{Document, Edit, EditOutcome},
};
use serde_json::json;
use std::{env, fs, hint::black_box, time::Instant};
use tree_sitter::{InputEdit, Parser, Point, Tree};

unsafe extern "C" {
    fn dlopen(path: *const std::ffi::c_char, mode: i32) -> *mut std::ffi::c_void;
    fn dlsym(
        handle: *mut std::ffi::c_void,
        symbol: *const std::ffi::c_char,
    ) -> *mut std::ffi::c_void;
}

fn point(text: &[u8], end: usize) -> Point {
    let bytes = &text[..end];
    Point::new(
        bytes.iter().filter(|&&b| b == b'\n').count(),
        bytes
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(end, |n| end - n - 1),
    )
}

fn tree_errors(root: tree_sitter::Node<'_>, output: &mut Vec<serde_json::Value>) {
    if root.is_error() || root.is_missing() {
        output.push(json!({"start":root.start_byte(), "end":root.end_byte(), "kind":root.kind()}));
    } else if root.has_error() {
        for child in root.children(&mut root.walk()) {
            tree_errors(child, output);
        }
    }
}

fn main() {
    let args: Vec<_> = env::args().collect();
    let mode = args.get(1).expect("tape|index|dev|perf").as_str();
    let mut text = fs::read(args.get(2).expect("input path")).unwrap();
    let action = args.get(3).map_or("parse", String::as_str);
    let mut parser = Parser::new();
    if mode == "dev" {
        let path = std::ffi::CString::new(env::var("DEV_GRAMMAR").expect("DEV_GRAMMAR dylib path"))
            .unwrap();
        let language = unsafe {
            let handle = dlopen(path.as_ptr(), 2);
            assert!(!handle.is_null(), "load dev grammar");
            let pointer = dlsym(handle, c"tree_sitter_ifc".as_ptr());
            assert!(!pointer.is_null());
            let function = std::mem::transmute::<
                *mut std::ffi::c_void,
                unsafe extern "C" fn() -> *const (),
            >(pointer);
            tree_sitter::Language::new(tree_sitter_language::LanguageFn::from_raw(function))
        };
        parser.set_language(&language).unwrap();
    }
    let t = Instant::now();
    let mut hand = None;
    let mut dev: Option<Tree> = None;
    let mut perf = None;
    match mode {
        "tape" | "index" => hand = Some(Document::parse(&text, mode == "tape")),
        "dev" => dev = parser.parse(&text, None),
        "perf" => perf = DocumentTree::parse(&mut parser, &text, None),
        _ => panic!("unknown mode"),
    }
    let parse_ms = t.elapsed().as_secs_f64() * 1000.;
    let mut errors = Vec::new();
    if let Some(doc) = &hand {
        errors.extend(
            doc.errors
                .iter()
                .map(|e| json!({"start":e.span.start,"end":e.span.end,"message":e.message})),
        );
    }
    if let Some(tree) = &dev {
        tree_errors(tree.root_node(), &mut errors);
    }
    if let Some(tree) = &perf {
        tree_errors(tree.root_node(), &mut errors);
    }
    println!(
        "{}",
        json!({"mode":mode,"action":action,"bytes":text.len(),"parse_ms":parse_ms,"errors":errors.len(),"statements":hand.as_ref().map(|d|d.statements.len()),"param_count":hand.as_ref().map(|d|d.params.len()),"error_ranges":errors.iter().take(12).collect::<Vec<_>>()})
    );
    if action == "parse" {
        black_box((&hand, &dev, &perf));
        return;
    }
    if action == "decode" {
        let doc = hand.as_ref().unwrap();
        let mut timings = Vec::new();
        let mut checksum = 0u64;
        for n in 0..1000 {
            let (id, _) = doc.ids[n * doc.ids.len() / 1000];
            let t = Instant::now();
            let index = doc.find(id).unwrap();
            checksum ^= consume(&doc.decode(&text, index));
            timings.push(t.elapsed().as_secs_f64() * 1e6);
        }
        timings.sort_by(f64::total_cmp);
        let t = Instant::now();
        if doc.retain {
            checksum = checksum.wrapping_add(consume(&doc.params));
        } else {
            for index in 0..doc.statements.len() {
                checksum = checksum.wrapping_add(consume(&doc.decode(&text, index)));
            }
        }
        println!(
            "{}",
            json!({"decode_p50_us":timings[500],"decode_p95_us":timings[950],"full_decode_ms":t.elapsed().as_secs_f64()*1000.,"checksum":black_box(checksum),"id_index_bytes":doc.ids.capacity()*8})
        );
        return;
    }
    let anchors = Document::parse(&text, false);
    let middle = anchors.statements.len() / 2;
    let selected = anchors
        .statements
        .iter()
        .enumerate()
        .filter(|(_, s)| text[s.name.end as usize..s.span.end as usize].contains(&b'\''))
        .min_by_key(|(index, _)| index.abs_diff(middle))
        .unwrap()
        .0;
    let s = anchors.statements[selected];
    let open = s.name.end as usize
        + text[s.name.end as usize..s.span.end as usize]
            .iter()
            .position(|&b| b == b'(')
            .unwrap();
    let first_quote = open
        + text[open..s.span.end as usize]
            .iter()
            .position(|&b| b == b'\'')
            .unwrap();
    let mut close_quote = first_quote + 1;
    while close_quote < s.span.end as usize {
        if text[close_quote] == b'\'' {
            if text.get(close_quote + 1) == Some(&b'\'') {
                close_quote += 2;
            } else {
                break;
            }
        } else {
            close_quote += 1;
        }
    }
    drop(anchors);
    let (start, deleted, replacement) = match action {
        "valid" => (open + 1, 0, b" ".as_slice()),
        "syntax" => (open, 1, b"X".as_slice()),
        "quote" => (close_quote, 1, b"".as_slice()),
        _ => panic!("unknown action"),
    };
    let mut new = text.clone();
    new.splice(start..start + deleted, replacement.iter().copied());
    let input = InputEdit {
        start_byte: start,
        old_end_byte: start + deleted,
        new_end_byte: start + replacement.len(),
        start_position: point(&text, start),
        old_end_position: point(&text, start + deleted),
        new_end_position: point(&new, start + replacement.len()),
    };
    let t = Instant::now();
    let mut reparsed_bytes = None;
    if let Some(doc) = &mut hand {
        reparsed_bytes = Some(
            match doc.edit(
                &text[start..start + deleted],
                &new,
                Edit {
                    start,
                    delete_len: deleted,
                    insert_len: replacement.len(),
                },
            ) {
                EditOutcome::Rebuilt => new.len(),
                EditOutcome::Local { reparsed_bytes, .. } => reparsed_bytes,
            },
        );
    }
    if let Some(tree) = &mut dev {
        tree.edit(&input);
        dev = parser.parse(&new, Some(tree));
    }
    if let Some(tree) = &mut perf {
        tree.edit(&input);
        perf = DocumentTree::parse(&mut parser, &new, perf.take());
    }
    let edit_ms = t.elapsed().as_secs_f64() * 1000.;
    errors.clear();
    if let Some(doc) = &hand {
        errors.extend(
            doc.errors
                .iter()
                .map(|e| json!({"start":e.span.start,"end":e.span.end,"message":e.message})),
        );
    }
    if let Some(tree) = &dev {
        tree_errors(tree.root_node(), &mut errors);
    }
    if let Some(tree) = &perf {
        tree_errors(tree.root_node(), &mut errors);
    }
    println!(
        "{}",
        json!({"edit_ms":edit_ms,"reparsed_bytes":reparsed_bytes,"errors":errors.len(),"edit_start":start,"error_ranges":errors.iter().take(12).collect::<Vec<_>>()})
    );
    if let Some(doc) = &hand {
        assert!(
            doc == &Document::parse(&new, mode == "tape"),
            "incremental/full parity"
        );
    }
    text.clear();
}

fn consume(params: &[ifc_language_server::handwritten::Param]) -> u64 {
    params.iter().fold(0, |n, p| {
        n.wrapping_add(p.span.start as u64)
            .wrapping_add(p.kind as u64)
    })
}
