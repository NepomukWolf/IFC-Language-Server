use tree_sitter::{InputEdit, Node, Parser, Point, Range, Tree};

#[derive(Clone, Debug)]
pub struct DocumentTree {
    pub compact: Tree,
    pub structured: Option<Tree>,
    omitted: Vec<Range>,
    pub unclosed_strings: Vec<usize>,
}

impl DocumentTree {
    pub fn parse(parser: &mut Parser, text: &[u8], old: Option<&Self>) -> Option<Self> {
        parser.set_included_ranges(&[]).expect("whole document");
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("IFC ABI");
        let compact = parser.parse(text, old.map(|tree| &tree.compact))?;
        let mut result = Self {
            compact,
            structured: None,
            omitted: Vec::new(),
            unclosed_strings: Vec::new(),
        };
        if !result.compact.root_node().has_error() {
            return Some(result);
        }

        let mut errors = Vec::new();
        error_spans(result.compact.root_node(), &mut errors);
        for (start, end) in &mut errors {
            if let Some(quote) = unclosed_string_start(text, *start) {
                if !result.unclosed_strings.contains(&quote) {
                    result.unclosed_strings.push(quote);
                }
                *start = quote;
                *end = line_end(text, quote);
                continue;
            }
            for _ in 0..2 {
                *start = text[..*start]
                    .iter()
                    .rposition(|byte| *byte == b';')
                    .unwrap_or(0);
                *end = text[*end..]
                    .iter()
                    .position(|byte| *byte == b';')
                    .map_or(text.len(), |offset| *end + offset + 1);
            }
        }
        crate::document::visit_entity_nodes(&mut result.compact.root_node().walk(), &mut |node| {
            if !node.has_error()
                && !errors
                    .iter()
                    .any(|&(start, end)| node.start_byte() <= end && node.end_byte() >= start)
            {
                let range = node.range();
                if let Some(previous) = result.omitted.last_mut()
                    && text[previous.end_byte..range.start_byte]
                        .iter()
                        .all(u8::is_ascii_whitespace)
                {
                    previous.end_byte = range.end_byte;
                    previous.end_point = range.end_point;
                } else {
                    result.omitted.push(range);
                }
            }
        });
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE_STRUCTURED.into())
            .expect("IFC ABI");
        for attempt in 0..3 {
            let mut ranges = Vec::new();
            let mut start_byte = 0;
            let mut start_point = Point::new(0, 0);
            for range in &result.omitted {
                if start_byte < range.start_byte {
                    ranges.push(Range {
                        start_byte,
                        end_byte: range.start_byte,
                        start_point,
                        end_point: range.start_point,
                    });
                }
                start_byte = range.end_byte;
                start_point = range.end_point;
            }
            if start_byte < text.len() {
                ranges.push(Range {
                    start_byte,
                    end_byte: text.len(),
                    start_point,
                    end_point: result.compact.root_node().end_position(),
                });
            }
            parser
                .set_included_ranges(&ranges)
                .expect("ordered source ranges");
            let structured = parser.parse(text, None)?;
            if result.omitted.is_empty() {
                result.structured = Some(structured);
                break;
            }
            let mut crossing = Vec::new();
            crossing_spans(structured.root_node(), &mut crossing);
            let previous = result.omitted.len();
            result.omitted.retain(|range| {
                !crossing
                    .iter()
                    .any(|&(start, end)| start < range.end_byte && end > range.start_byte)
            });
            if previous == result.omitted.len() {
                result.structured = Some(structured);
                break;
            }
            if attempt == 1 {
                result.omitted.clear();
            }
        }
        parser.set_included_ranges(&[]).expect("whole document");
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
            .expect("IFC ABI");
        Some(result)
    }

    pub fn edit(&mut self, edit: &InputEdit) {
        self.compact.edit(edit);
    }

    pub fn root_node(&self) -> Node<'_> {
        self.structured
            .as_ref()
            .unwrap_or(&self.compact)
            .root_node()
    }

    pub fn node_at_offset(&self, offset: usize) -> Option<Node<'_>> {
        let tree = if self.is_omitted(offset) {
            &self.compact
        } else {
            self.structured.as_ref().unwrap_or(&self.compact)
        };
        tree.root_node().descendant_for_byte_range(offset, offset)
    }

    fn is_omitted(&self, offset: usize) -> bool {
        let index = self
            .omitted
            .partition_point(|range| range.end_byte <= offset);
        self.omitted
            .get(index)
            .is_some_and(|range| range.start_byte <= offset)
    }

    pub fn for_each_entity<'a>(&'a self, mut visit: impl FnMut(Node<'a>)) {
        let Some(structured) = &self.structured else {
            crate::document::visit_entity_nodes(&mut self.compact.root_node().walk(), &mut visit);
            return;
        };
        if self.omitted.is_empty() {
            crate::document::visit_entity_nodes(&mut structured.root_node().walk(), &mut visit);
            return;
        }
        let mut recovered = Vec::new();
        crate::document::visit_entity_nodes(&mut structured.root_node().walk(), &mut |node| {
            recovered.push(node)
        });
        let mut recovered = recovered.into_iter().peekable();
        crate::document::visit_entity_nodes(&mut self.compact.root_node().walk(), &mut |node| {
            if self.is_omitted(node.start_byte()) {
                while recovered
                    .peek()
                    .is_some_and(|next| next.start_byte() < node.start_byte())
                {
                    visit(recovered.next().unwrap());
                }
                visit(node);
            }
        });
        for node in recovered {
            visit(node);
        }
    }
}

fn error_spans(node: Node<'_>, spans: &mut Vec<(usize, usize)>) {
    if node.is_error() || node.is_missing() {
        spans.push((node.start_byte(), node.end_byte()));
    } else if node.has_error() {
        for child in node.children(&mut node.walk()) {
            error_spans(child, spans);
        }
    }
}

fn crossing_spans(node: Node<'_>, spans: &mut Vec<(usize, usize)>) {
    if node.is_error()
        || node.is_missing()
        || matches!(node.kind(), "string" | "comment" | "entity_instance")
    {
        spans.push((node.start_byte(), node.end_byte()));
    }
    for child in node.children(&mut node.walk()) {
        crossing_spans(child, spans);
    }
}

fn line_end(text: &[u8], start: usize) -> usize {
    text[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(text.len(), |offset| start + offset)
}

fn unclosed_string_start(text: &[u8], error_start: usize) -> Option<usize> {
    let mut entity_start = None;
    let mut first_quote = None;
    let mut quote_count = 0;
    let mut candidate = None;
    let mut candidate_entity_start = None;
    let mut offset = 0;
    while offset < text.len() {
        if is_entity_start(text, offset) {
            if quote_count % 2 == 1 {
                candidate = first_quote;
                candidate_entity_start = entity_start;
            }
            entity_start = Some(offset);
            first_quote = None;
            quote_count = 0;
        }
        if text[offset..].starts_with(b"/*") {
            offset = text[offset + 2..]
                .windows(2)
                .position(|window| window == b"*/")
                .map_or(text.len(), |end| offset + end + 4);
        } else if text[offset] == b'\'' {
            if text.get(offset + 1) == Some(&b'\'') {
                offset += 2;
            } else {
                first_quote.get_or_insert(offset);
                quote_count += 1;
                offset += 1;
            }
        } else {
            offset += 1;
        }
    }
    if quote_count % 2 == 1 {
        candidate = first_quote;
        candidate_entity_start = entity_start;
    }
    candidate.filter(|_| candidate_entity_start.is_some_and(|entity| entity <= error_start))
}

fn is_entity_start(text: &[u8], offset: usize) -> bool {
    if offset > 0 && text[offset - 1] != b'\n' {
        return false;
    }
    if text.get(offset) != Some(&b'#') {
        return false;
    }
    let mut end = offset + 1;
    while text.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    text.get(end) == Some(&b'=')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{DiagnosticSnapshot, syntax};
    use crate::document::{Document, build_entity_instances};

    #[test]
    fn finds_the_opening_quote_of_an_unclosed_string() {
        assert_eq!(
            unclosed_string_start(b"#1=IFCWALL('Anna's wall');", 0),
            Some(11)
        );
        assert_eq!(unclosed_string_start(b"#1=IFCWALL('line\none');", 0), None);
        assert_eq!(
            unclosed_string_start(b"#1=IFCWALL('escaped '' quote');", 0),
            None
        );
    }

    #[test]
    fn local_recovery_matches_whole_document_recovery() {
        let header = "ISO-10303-21;\nHEADER;\nFILE_SCHEMA(('IFC4'));\nENDSEC;\nDATA;\n";
        for broken in [
            "#25=IFCX(1x);",
            "#25=IFCX(1)",
            "#25=IFCX((1,2);",
            "#25=IFCX('unclosed);",
            "#25=IFCX('Anna's wall');",
            "#25=IFCX('a',$,IFCLABEL'x'),$);",
            "#25 IFCX($);",
            "#=IFCX($);",
            "#25=IFCX('é',IFCLABEL(2x),$);",
        ] {
            let text = format!(
                "{header}{}ENDSEC;\nEND-ISO-10303-21;\n",
                (1..=50)
                    .map(|id| {
                        if id == 25 {
                            format!("{broken}\n")
                        } else {
                            format!("#{id}=IFCX('v',IFCLABEL('x'),$);\n")
                        }
                    })
                    .collect::<String>()
            );
            let mut parser = Parser::new();
            let local = Document::parse(&mut parser, text.clone());
            parser
                .set_language(&tree_sitter_ifc::LANGUAGE_STRUCTURED.into())
                .unwrap();
            let mut whole = Document::new_unloaded(text.clone());
            whole.tree = Some(DocumentTree {
                compact: parser.parse(text.as_bytes(), None).unwrap(),
                structured: None,
                omitted: Vec::new(),
                unclosed_strings: Vec::new(),
            });
            let local_diagnostics = syntax::collect(&DiagnosticSnapshot::from_document(&local));
            if broken.contains("unclosed") || broken.contains("Anna") {
                assert_eq!(local_diagnostics.len(), 1, "{local_diagnostics:?}");
                assert_eq!(
                    local_diagnostics[0].message,
                    "Unclosed string; expected `'`"
                );
            } else {
                assert_eq!(
                    local_diagnostics,
                    syntax::collect(&DiagnosticSnapshot::from_document(&whole)),
                    "{broken}"
                );
            }
            assert_eq!(
                build_entity_instances(local.tree.as_ref(), &text).instances,
                build_entity_instances(whole.tree.as_ref(), &text).instances,
                "{broken}"
            );
            for offset in (0..text.len()).filter(|&offset| text.is_char_boundary(offset)) {
                let context = |document: &Document| {
                    let node = document
                        .tree
                        .as_ref()
                        .unwrap()
                        .node_at_offset(offset)
                        .unwrap();
                    (
                        crate::step::ast::parameter_context(node, offset, &text),
                        crate::step::ast::omitted_value_span(node, offset, &text),
                        crate::step::ast::typed_value_name_span(node, offset, &text),
                    )
                };
                assert_eq!(context(&local), context(&whole), "{broken} at {offset}");
            }
        }
    }
}
