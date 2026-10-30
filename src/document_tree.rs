use tree_sitter::{InputEdit, Node, Parser, Point, Range, Tree, TreeCursor};

#[derive(Clone, Debug)]
pub struct DocumentTree {
    compact: Tree,
    structured: Option<Tree>,
    omitted: Vec<Range>,
}

impl DocumentTree {
    /// Parses with the compact grammar and, when that tree has errors, reparses only the text
    /// around them with the structured grammar, keeping error-free compact entities elsewhere.
    ///
    /// Each error is widened by two `;`-terminated statements on each side. A structured error,
    /// string, comment or entity that crosses a kept compact entity drops that entity and the
    /// reparse is repeated. The third attempt keeps no compact entities, so the worst case is
    /// two partial structured parses followed by one of the whole document.
    pub fn parse(parser: &mut Parser, text: &[u8], old: Option<Self>) -> Option<Self> {
        parser.set_included_ranges(&[]).expect("whole document");
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE_COMPACT.into())
            .expect("IFC ABI");
        let compact = parser.parse(text, old.as_ref().map(|tree| &tree.compact))?;
        drop(old);
        let mut result = Self {
            compact,
            structured: None,
            omitted: Vec::new(),
        };
        if !result.compact.root_node().has_error() {
            return Some(result);
        }

        let mut errors = Vec::new();
        let mut entities = Vec::new();
        collect_recovery_nodes(
            &mut result.compact.root_node().walk(),
            false,
            &mut errors,
            &mut entities,
        );
        for (start, end) in &mut errors {
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
        let errors = SortedSpans::new(errors);
        for node in entities {
            if !errors.any_touching(node.start_byte(), node.end_byte()) {
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
        }
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE.into())
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
            let crossing = SortedSpans::new(crossing);
            let previous = result.omitted.len();
            result
                .omitted
                .retain(|range| !crossing.any_crossing(range.start_byte, range.end_byte));
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
            .set_language(&tree_sitter_ifc::LANGUAGE_COMPACT.into())
            .expect("IFC ABI");
        Some(result)
    }

    #[cfg(test)]
    pub(crate) fn structured(&self) -> Option<&Tree> {
        self.structured.as_ref()
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

/// Collects the outermost error and missing nodes and, in document order, the error-free entity
/// instances.
fn collect_recovery_nodes<'a>(
    cursor: &mut TreeCursor<'a>,
    inside_error: bool,
    errors: &mut Vec<(usize, usize)>,
    entities: &mut Vec<Node<'a>>,
) {
    loop {
        let node = cursor.node();
        let is_error = node.is_error() || node.is_missing();
        if is_error && !inside_error {
            errors.push((node.start_byte(), node.end_byte()));
        }
        let is_entity = node.kind() == "entity_instance";
        let has_error = node.has_error();
        if is_entity && !has_error {
            entities.push(node);
        }
        if (!is_entity || has_error) && cursor.goto_first_child() {
            collect_recovery_nodes(cursor, inside_error || is_error, errors, entities);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// Byte spans sorted by start, with the furthest end among each prefix.
struct SortedSpans {
    spans: Vec<(usize, usize)>,
    furthest_ends: Vec<usize>,
}

impl SortedSpans {
    fn new(mut spans: Vec<(usize, usize)>) -> Self {
        spans.sort_unstable();
        let mut furthest = 0;
        let furthest_ends = spans
            .iter()
            .map(|&(_, end)| {
                furthest = furthest.max(end);
                furthest
            })
            .collect();
        Self {
            spans,
            furthest_ends,
        }
    }

    /// Whether a span starts before `end` and ends after `start`.
    fn any_crossing(&self, start: usize, end: usize) -> bool {
        let count = self.spans.partition_point(|span| span.0 < end);
        count > 0 && self.furthest_ends[count - 1] > start
    }

    /// Whether a span starts at or before `end` and ends at or after `start`.
    fn any_touching(&self, start: usize, end: usize) -> bool {
        let count = self.spans.partition_point(|span| span.0 <= end);
        count > 0 && self.furthest_ends[count - 1] >= start
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{DiagnosticSnapshot, syntax};
    use crate::document::Document;
    use crate::document::build_entity_instances;

    #[test]
    fn recovery_collects_only_outermost_errors_and_error_free_entities() {
        fn outermost_errors(node: Node<'_>, spans: &mut Vec<(usize, usize)>) {
            if node.is_error() || node.is_missing() {
                spans.push((node.start_byte(), node.end_byte()));
            } else {
                for child in node.children(&mut node.walk()) {
                    outermost_errors(child, spans);
                }
            }
        }
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ifc::LANGUAGE_COMPACT.into())
            .expect("IFC ABI");
        for text in [
            "DATA;\n#1=X((1,2);\n#2=Y('a);\nENDSEC;",
            "DATA;\n#1=X('a',$,IFCLABEL'x'),$);\n#2=Y(1);\nENDSEC;",
            "DATA;\n#1=X(#2=Y((1,2;\n#3=Z(1);\nENDSEC;",
        ] {
            let tree = parser.parse(text, None).expect("parse");
            let mut errors = Vec::new();
            let mut entities = Vec::new();
            collect_recovery_nodes(
                &mut tree.root_node().walk(),
                false,
                &mut errors,
                &mut entities,
            );

            let mut expected_errors = Vec::new();
            outermost_errors(tree.root_node(), &mut expected_errors);
            let mut expected_entities = Vec::new();
            crate::document::visit_entity_nodes(&mut tree.root_node().walk(), &mut |node| {
                if !node.has_error() {
                    expected_entities.push(node);
                }
            });
            assert_eq!(errors, expected_errors, "{text}");
            assert_eq!(entities, expected_entities, "{text}");
        }
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
                .set_language(&tree_sitter_ifc::LANGUAGE.into())
                .unwrap();
            let mut whole = Document::new_unloaded(text.clone());
            whole.tree = Some(DocumentTree {
                compact: parser.parse(text.as_bytes(), None).unwrap(),
                structured: None,
                omitted: Vec::new(),
            });
            assert_eq!(
                syntax::collect(&DiagnosticSnapshot::from_document(&local)),
                syntax::collect(&DiagnosticSnapshot::from_document(&whole)),
                "{broken}"
            );
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
