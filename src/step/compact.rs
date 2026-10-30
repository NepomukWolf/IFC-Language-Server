//! Values inside a `compact_parameter_list` token. The grammar only emits that token for a
//! well-formed parameter list whose parameters are atoms, typed atoms such as `IFCLABEL('x')`,
//! or flat lists of those, so the scanner below never sees malformed input.

#[derive(Debug, PartialEq, Eq)]
pub enum CompactKind {
    String,
    Number,
    Reference,
    Enumeration,
    Null,
    Omitted,
    Typed {
        name_end: usize,
        inner: Vec<CompactValue>,
    },
    List(Vec<CompactValue>),
}

/// One value with absolute byte offsets into the document text.
#[derive(Debug, PartialEq, Eq)]
pub struct CompactValue {
    pub start: usize,
    pub end: usize,
    pub kind: CompactKind,
}

impl CompactValue {
    fn contains(&self, offset: usize) -> bool {
        self.start <= offset && offset < self.end
    }
}

/// Byte spans of a whole-entity token `#ID=NAME(...);`.
pub struct CompactEntity {
    pub id: (usize, usize),
    pub name: (usize, usize),
    pub parameters: (usize, usize),
}

/// A leaf `entity_instance` is a whole-entity token; structured instances always have children.
pub fn is_compact_entity(node: tree_sitter::Node<'_>) -> bool {
    node.kind() == "entity_instance" && node.child_count() == 0
}

pub fn entity(text: &str, start: usize) -> CompactEntity {
    let mut scanner = Scanner {
        bytes: text.as_bytes(),
        pos: start + 1,
    };
    scanner.skip_while(|byte| byte.is_ascii_digit());
    let id = (start, scanner.pos);
    scanner.skip_whitespace();
    scanner.pos += 1;
    scanner.skip_whitespace();
    let name_start = scanner.pos;
    scanner.skip_while(is_name_byte);
    let name = (name_start, scanner.pos);
    scanner.skip_whitespace();
    let parameters_start = scanner.pos;
    scanner.sequence();
    CompactEntity {
        id,
        name,
        parameters: (parameters_start, scanner.pos),
    }
}

/// Start of the compact parameter list held by `node`, which is either that token or a
/// whole-entity token.
pub fn parameters_start(node: tree_sitter::Node<'_>, text: &str) -> Option<usize> {
    if node.kind() == "compact_parameter_list" {
        Some(node.start_byte())
    } else if is_compact_entity(node) {
        Some(entity(text, node.start_byte()).parameters.0)
    } else {
        None
    }
}

/// Top-level parameters of the token starting at `start`.
pub fn parameters(text: &str, start: usize) -> Vec<CompactValue> {
    Scanner {
        bytes: text.as_bytes(),
        pos: start,
    }
    .sequence()
}

/// The values containing `offset`, outermost first.
pub fn values_at(parameters: &[CompactValue], offset: usize) -> Vec<&CompactValue> {
    let mut path = Vec::new();
    let mut values = parameters;
    while let Some(value) = values.iter().find(|value| value.contains(offset)) {
        path.push(value);
        values = match &value.kind {
            CompactKind::Typed { inner, .. } => inner,
            CompactKind::List(items) => items,
            _ => &[],
        };
    }
    path
}

struct Scanner<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Scanner<'_> {
    fn peek(&self) -> u8 {
        self.bytes[self.pos]
    }

    fn skip_while(&mut self, predicate: impl Fn(u8) -> bool) {
        while self.pos < self.bytes.len() && predicate(self.peek()) {
            self.pos += 1;
        }
    }

    fn skip_whitespace(&mut self) {
        self.skip_while(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'));
    }

    fn sequence(&mut self) -> Vec<CompactValue> {
        self.pos += 1;
        self.skip_whitespace();
        let mut values = Vec::new();
        if self.peek() == b')' {
            self.pos += 1;
            return values;
        }
        loop {
            values.push(self.value());
            self.skip_whitespace();
            let separator = self.peek();
            self.pos += 1;
            if separator == b')' {
                return values;
            }
            self.skip_whitespace();
        }
    }

    fn value(&mut self) -> CompactValue {
        let start = self.pos;
        let kind = match self.peek() {
            b'(' => CompactKind::List(self.sequence()),
            b'\'' => {
                self.string();
                CompactKind::String
            }
            b'#' => {
                self.pos += 1;
                self.skip_while(|byte| byte.is_ascii_digit());
                CompactKind::Reference
            }
            b'$' => {
                self.pos += 1;
                CompactKind::Null
            }
            b'*' => {
                self.pos += 1;
                CompactKind::Omitted
            }
            b'A'..=b'Z' => {
                self.skip_while(is_name_byte);
                let name_end = self.pos;
                self.skip_whitespace();
                let inner = self.sequence();
                CompactKind::Typed { name_end, inner }
            }
            b'.' if self.enumeration() => CompactKind::Enumeration,
            _ => {
                self.number();
                CompactKind::Number
            }
        };
        CompactValue {
            start,
            end: self.pos,
            kind,
        }
    }

    fn string(&mut self) {
        self.pos += 1;
        loop {
            if self.peek() == b'\'' {
                self.pos += 1;
                if self.pos == self.bytes.len() || self.peek() != b'\'' {
                    return;
                }
            }
            self.pos += 1;
        }
    }

    /// Lexes `.NAME.` the way the grammar's longest match does, leaving `.5` to `number`.
    fn enumeration(&mut self) -> bool {
        let name_start = self.pos + 1;
        let mut end = name_start;
        while end < self.bytes.len() && is_name_byte(self.bytes[end]) {
            end += 1;
        }
        if end == name_start || self.bytes.get(end) != Some(&b'.') {
            return false;
        }
        self.pos = end + 1;
        true
    }

    fn number(&mut self) {
        if self.peek() == b'-' {
            self.pos += 1;
        }
        self.skip_while(|byte| byte.is_ascii_digit());
        if self.peek() == b'.' {
            self.pos += 1;
            self.skip_while(|byte| byte.is_ascii_digit());
        }
        if matches!(self.peek(), b'E' | b'e') {
            self.pos += 1;
            if matches!(self.peek(), b'+' | b'-') {
                self.pos += 1;
            }
            self.skip_while(|byte| byte.is_ascii_digit());
        }
    }
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_'
}
