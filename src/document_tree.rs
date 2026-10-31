use tree_sitter::{InputEdit, Node, Parser, Point, Range, Tree, TreeCursor};

#[derive(Clone, Debug)]
pub struct DocumentTree {
    compact: Tree,
    structured: Option<Tree>,
    omitted: Vec<Range>,
    pub(crate) unclosed_strings: Vec<UnclosedString>,
}

/// A string whose closing quote is missing, reported at `quote`. Errors from `quote` to `end`
/// follow from the text it swallowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UnclosedString {
    pub(crate) quote: usize,
    pub(crate) end: usize,
}

impl DocumentTree {
    /// Parses with the compact grammar and, when that tree has errors, reparses only the text
    /// around them with the structured grammar, keeping error-free compact entities elsewhere.
    ///
    /// An error caused by an unclosed string is bounded to the lines of that string; any other
    /// error is widened by two `;`-terminated statements on each side. A structured error,
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
            unclosed_strings: Vec::new(),
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
        let odd_quotes = odd_quote_entities(text);
        for (start, end) in &mut errors {
            let causing = causing_strings(text, &odd_quotes, (*start, *end));
            let causes = &odd_quotes[causing.clone()];
            if let (Some(first), Some(last)) = (causes.first(), causes.last()) {
                result
                    .unclosed_strings
                    .extend(causing.clone().map(|index| UnclosedString {
                        quote: odd_quotes[index].first_quote,
                        end: swallowed_end(text, &odd_quotes, index),
                    }));
                *start = first.first_quote;
                *end = line_end(text, last.last_quote);
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
        result
            .unclosed_strings
            .sort_unstable_by_key(|string| string.quote);
        result.unclosed_strings.dedup();
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

fn line_end(text: &[u8], start: usize) -> usize {
    text[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(text.len(), |offset| start + offset)
}

/// An entity line whose quotes do not pair up.
struct OddQuoteEntity {
    start: usize,
    first_quote: usize,
    last_quote: usize,
}

fn odd_quote_entities(text: &[u8]) -> Vec<OddQuoteEntity> {
    let mut entities = Vec::new();
    let mut current = None;
    let mut quote_count = 0;
    let mut offset = 0;
    let mut finish = |current: Option<OddQuoteEntity>, quote_count: usize| {
        if quote_count % 2 == 1 {
            entities.extend(current);
        }
    };
    while offset < text.len() {
        if is_entity_start(text, offset) {
            finish(current.take(), quote_count);
            current = Some(OddQuoteEntity {
                start: offset,
                first_quote: usize::MAX,
                last_quote: usize::MAX,
            });
            quote_count = 0;
        }
        if quote_count % 2 == 0 && text[offset..].starts_with(b"/*") {
            offset = memchr::memmem::find(&text[offset + 2..], b"*/")
                .map_or(text.len(), |end| offset + end + 4);
        } else if text[offset] == b'\'' {
            if text.get(offset + 1) == Some(&b'\'') {
                offset += 2;
            } else {
                if let Some(entity) = &mut current {
                    if quote_count == 0 {
                        entity.first_quote = offset;
                    }
                    entity.last_quote = offset;
                }
                quote_count += 1;
                offset += 1;
            }
        } else if text[offset] == b'\n' {
            offset += 1;
        } else {
            offset += 1;
            offset +=
                memchr::memchr3(b'\n', b'\'', b'/', &text[offset..]).unwrap_or(text.len() - offset);
        }
    }
    finish(current, quote_count);
    entities
}

/// The entities in `odd_quotes` whose unclosed string can explain an error spanning
/// `error_start..error_end`: those starting inside it, the nearest one starting at or before it,
/// and the earlier ones whose string swallows text up to it.
fn causing_strings(
    text: &[u8],
    odd_quotes: &[OddQuoteEntity],
    (error_start, error_end): (usize, usize),
) -> std::ops::Range<usize> {
    let reach = error_end.saturating_sub(1).max(error_start);
    let last = odd_quotes.partition_point(|entity| entity.start <= reach);
    let mut first = last;
    while first > 0 && swallowed_end(text, odd_quotes, first - 1) >= error_start {
        first -= 1;
    }
    first..last
}

/// End of the text affected by the unclosed string of the entity at `index`. Each entity in
/// `odd_quotes` flips which quotes open strings, so the text from an entity at an even index to
/// the end of the next one's line is read out of phase; with no next one, everything up to the
/// last quote is. Error recovery resumes only after the statement that follows.
fn swallowed_end(text: &[u8], odd_quotes: &[OddQuoteEntity], index: usize) -> usize {
    let entity = &odd_quotes[index];
    let last_quote = if index % 2 == 1 {
        entity.last_quote
    } else if let Some(next) = odd_quotes.get(index + 1) {
        next.last_quote
    } else {
        entity.last_quote
            + memchr::memrchr(b'\'', &text[entity.last_quote..])
                .expect("the entity's own last quote")
    };
    let out_of_phase = line_end(text, last_quote);
    memchr::memchr(b';', &text[out_of_phase..]).map_or(text.len(), |semicolon| {
        line_end(text, out_of_phase + semicolon)
    })
}

/// Whether a data entity (`#1=`) or a header entity (`FILE_NAME(`) starts at `offset`.
fn is_entity_start(text: &[u8], offset: usize) -> bool {
    if offset > 0 && text[offset - 1] != b'\n' {
        return false;
    }
    let (follower, continues): (u8, fn(&u8) -> bool) = match text.get(offset) {
        Some(b'#') => (b'=', u8::is_ascii_digit),
        Some(byte) if byte.is_ascii_uppercase() => (b'(', |byte| {
            byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_'
        }),
        _ => return false,
    };
    let mut end = offset + 1;
    while text.get(end).is_some_and(continues) {
        end += 1;
    }
    text.get(end) == Some(&follower)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{DiagnosticSnapshot, syntax};
    use crate::document::Document;
    use crate::document::build_entity_instances;

    fn unclosed_quotes(text: &[u8], error_start: usize) -> Vec<usize> {
        let odd_quotes = odd_quote_entities(text);
        odd_quotes[causing_strings(text, &odd_quotes, (error_start, error_start))]
            .iter()
            .map(|entity| entity.first_quote)
            .collect()
    }

    #[test]
    fn finds_the_opening_quote_of_an_unclosed_string() {
        assert_eq!(unclosed_quotes(b"#1=IFCWALL('Anna's wall');", 0), [11]);
        assert!(unclosed_quotes(b"#1=IFCWALL('line\none');", 0).is_empty());
        assert!(unclosed_quotes(b"#1=IFCWALL('escaped '' quote');", 0).is_empty());
        assert_eq!(unclosed_quotes(b"#1=X('a/b');\n#2=X('c);", 14), [18]);
        assert!(unclosed_quotes(b"/* it's\n#9=X(' */\n#1=X('a');", 0).is_empty());
        assert_eq!(unclosed_quotes(b"/* it's\n#9=X(' */\n#1=X('a);", 25), [23]);
        assert_eq!(unclosed_quotes(b"#1=X('a' /x/ 'b);\n#2=X('');", 0), [5]);
        assert_eq!(unclosed_quotes(b"#1=X('a);\n/* x */#2=X('b');", 0), [5]);
        assert_eq!(unclosed_quotes(b"#1=X('a /* b');\n#2=X('c);", 22), [21]);
    }

    #[test]
    fn a_comment_opener_inside_a_string_does_not_hide_quotes_up_to_a_later_closer() {
        let text = b"#1=X('a /* b');\n#2=X('c);\n#3=X('d */');\n";
        assert_eq!(unclosed_quotes(text, 21), [21]);
    }

    #[test]
    fn finds_every_unclosed_string_that_explains_an_error() {
        let text = b"#1=X('a);\n#2=X(1);\n#3=X('b);\n";
        assert_eq!(unclosed_quotes(text, 0), [5]);
        assert_eq!(unclosed_quotes(text, 25), [5, 24]);
        let far = b"#1=X('a);\n#2=X(1);\n#3=X(2)\n#4=X(3);\n";
        assert!(unclosed_quotes(far, 27).is_empty());
    }

    #[test]
    fn an_unclosed_string_affects_text_up_to_the_statement_after_the_next_one() {
        let text = "#1=X('a);\n#2=X('b','c');\n#3=X('d');\n#4=X('e);\n#5=X(1);\n#6=X('f');\n";
        let odd_quotes = odd_quote_entities(text.as_bytes());
        let ends: Vec<_> = (0..odd_quotes.len())
            .map(|index| swallowed_end(text.as_bytes(), &odd_quotes, index))
            .collect();
        let line_end = |entity: &str| {
            text.find(entity).unwrap() + text[text.find(entity).unwrap()..].find('\n').unwrap()
        };
        assert_eq!(ends, [line_end("#5="), line_end("#5=")]);
    }

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
