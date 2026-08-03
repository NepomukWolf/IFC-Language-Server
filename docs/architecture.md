# IFC-Language-Server Architecture

**For Human Developers:** Reference this file in your agent instructions (`AGENTS.md`, `.claude/`, etc.).

## Goal

This project stays small and focused.

The implemented architecture is built around:

- full-text document sync
- one lightweight text index per open IFC document
- optional tree-sitter parse state for the active document
- generated in-memory EXPRESS schema documentation

The shipped LSP features are:

- hover
- go-to-definition
- find-references
- document highlight
- signature help
- inlay hints
- IFC boilerplate completions and code actions
- range-based semantic tokens
- schema-aware diagnostics

## Core Design

The runtime code revolves around three main concepts:

- `Backend`
  The `tower-lsp` entrypoint. It owns the open-document map, loaded schema docs, server configuration, and large-file warning state.
- `Document`
  The in-memory representation of one IFC file. It always stores source text and cheap text indexes. It may also store tree-sitter state when AST-backed features are enabled for that document.
- `SchemaDocCollection` and `SchemaDoc`
  In-memory lookup tables for EXPRESS entity and type documentation, keyed by normalized schema name.

Feature modules in `src/features/` stay thin and operate on `&Document`.
Diagnostics operate on `&Document` plus a selected `SchemaDoc`.

## Data Flow

On open or full-text change:

1. `Backend` unloads AST-backed parse state from other open documents.
2. The active document stores the new full text.
3. `Document::reload_parse_state` rebuilds the text index.
4. If the file is within the configured AST size limit, the document is parsed with `tree-sitter-ifc` and entity instances are rebuilt from the syntax tree.
5. If the file is above the AST size limit, the document is marked as `ast_skipped`.
6. Diagnostics are collected only when the active document has an AST.
7. Diagnostics are published to the LSP client.

On hover, definition, or references requests:

1. `Backend::ensure_document_loaded` reloads the requested document if its AST-backed state was previously unloaded.
2. Reloading one document unloads AST-backed state from the other open documents.
3. The feature handler reads the stored `Document`.
4. If request-time reloading produced diagnostics, they are published after the request.

On document-highlight, signature-help, or semantic-token range requests, the backend reads the
stored document text and text index only. These requests do not reload tree-sitter parse state or
publish diagnostics.

There is still no incremental parsing, background indexing, or cross-document indexing.

## Backend

`src/backend.rs` owns:

- `documents: Arc<RwLock<HashMap<Url, Document>>>`
- `schema_docs: Arc<RwLock<SchemaDocCollection>>`
- `config: Arc<RwLock<ConfigState>>`
- `ast_skip_warning_shown: Arc<RwLock<HashSet<Url>>>`

The backend creates a fresh tree-sitter parser through `new_parser()` when AST state must be rebuilt. It does not keep one long-lived parser.

The server advertises:

- `textDocument/hover`
- full text document sync
- `textDocument/definition`
- `textDocument/documentSymbol`
- `textDocument/references`
- `textDocument/documentHighlight`
- `textDocument/signatureHelp`
- `textDocument/inlayHint`
- `textDocument/completion`
- `textDocument/codeAction`
- `textDocument/semanticTokens/range`

Diagnostics are published with `textDocument/publishDiagnostics` on open, change, and request-time reloads.

## Logging

Runtime logs are written to `stderr` through `tracing`. The language server keeps `stdout`
reserved for the LSP protocol stream, while editor integrations are expected to decide whether
and how `stderr` should be captured into a log file. User-facing warnings still go through LSP
notifications such as `window/showMessage`.

## Document Model

`src/document.rs` is the central document representation:

```rust
pub struct Document {
    pub text: String,
    pub tree: Option<tree_sitter::Tree>,
    pub ast_skipped: bool,
    pub schema_name: Option<String>,
    pub line_offsets: Vec<usize>,
    pub definitions: HashMap<u32, usize>,
    pub references: HashMap<u32, Vec<usize>>,
    pub instances: Vec<EntityInstanceInfo>,
}
```

Important fields:

- `text`
  The full source text used by all features.
- `line_offsets`
  Byte offsets for line starts. Position conversion handles LSP UTF-16 columns.
- `schema_name`
  The `FILE_SCHEMA(...)` value detected by the text scanner.
- `definitions`
  Maps numeric ids such as `123` to the byte offset of the local `#123` definition.
- `references`
  Maps numeric ids to byte offsets of all local `#123` tokens, including the definition token.
- `tree`
  Optional tree-sitter syntax tree for AST-backed features.
- `ast_skipped`
  Marks a document whose text exceeded the configured AST parsing limit.
- `instances`
  Parsed entity instances and parameter values, rebuilt only when an AST is available.

`src/document_index.rs` provides the lightweight scanner used by every document. It records line starts, local ids, references, and `FILE_SCHEMA(...)` without requiring tree-sitter.

## Loading And Unloading

`Document::new_unloaded(text)` creates a document with source text and the lightweight text index only.

`Document::unload_parse_state()` drops:

- `tree`
- `instances`
- the private instance id lookup

It keeps:

- `text`
- `line_offsets`
- `schema_name`
- `definitions`
- `references`

`Document::reload_parse_state(parser, ast_file_size_limit_bytes)` always rebuilds the text index first. It then parses the document only if `text.len()` is within the AST limit. Files above the limit keep text-index-backed features available and set `ast_skipped = true` so the server does not repeatedly attempt to parse them.

The backend intentionally keeps AST-backed state for at most one active document at a time. Other open documents remain in memory as text plus the lightweight index.

## AST Size Limit

The default AST parsing limit is `70 MiB` (`DEFAULT_AST_FILE_SIZE_LIMIT_BYTES`).

Clients can override it with the LSP initialization option:

```json
{
  "astFileSizeLimitMb": 128
}
```

When a file is larger than the limit:

- the server shows one warning per URI
- tree-sitter parsing is skipped
- schema diagnostics are disabled
- derived `*` hover is disabled
- basic hover, navigation, document highlight, signature help, inlay hints, and range-based semantic tokens remain available from the text index/source text

## Feature AST Usage

These features do not require an AST:

- local reference hover for `#123`
- entity definition hover for names such as `IFCWALL`, when schema docs are available
- go-to-definition for local `#id` references
- find-references for local `#id` tokens
- document highlight for local `#id` tokens
- signature help for IFC entity parameter lists, when schema docs are available
- schema-backed inlay hints for STEP entity arguments
- schema name detection from `FILE_SCHEMA(...)`
- range-based semantic tokens

These features require an AST:

- syntax diagnostics
- schema-aware datatype diagnostics
- parsed entity argument validation
- derived `*` hover for `IfcSIUnit` and `IfcGeometricRepresentationSubContext`

New features should explicitly choose the cheapest data source that is sufficient. Prefer the text index for navigation and simple token lookup. Use tree-sitter only when syntax structure or parsed parameter values are required.

## Schema Documentation

Official EXPRESS definitions for supported IFC versions are fetched by `build.rs` at compile time, written into Cargo's `OUT_DIR`, and embedded into the binary with `include_str!`.

At runtime, startup parses those bundled EXPRESS strings into a `SchemaDocCollection`. Custom EXPRESS schemas can also be loaded from paths supplied in `initializationOptions`.

`SchemaDocCollection` is keyed by normalized schema name, not by open document. If an official schema fails to load, the backend logs a warning and schema-aware hover or diagnostics for that schema are unavailable.

## Feature Behavior

### Hover

`src/features/hover.rs` supports:

- reference hover by rendering the local defining entity instance as an IFC code block
- entity definition hover from the selected schema docs
- explicit derived `*` hover for `IfcSIUnit.Dimensions`, resolved from `Name`
- explicit derived `*` hover for inherited `IfcGeometricRepresentationSubContext` attributes, resolved from `ParentContext`

Derived hover uses the fixed STEP parameter layouts for these two entities. It does not evaluate general EXPRESS `DERIVE` expressions or consult schema metadata to discover additional derived attributes.

### Go To Definition

`src/features/definition.rs` resolves local `#id` references to their same-document definition offset. It does not perform cross-file lookup.

### Find References

`src/features/references.rs` returns all same-document text-indexed `#id` locations, including the definition token.

### Document Highlight

`src/features/document_highlight.rs` returns all same-document text-indexed `#id` ranges for the
id under the cursor. All occurrences are returned as textual highlights.

Document highlight does not use tree-sitter or schema docs.

### Signature Help

`src/features/signature_help.rs` provides IFC entity parameter signatures from the selected schema
docs. It uses a lightweight text scanner to find the current STEP entity argument list and count
top-level commas before the cursor to choose the active parameter.

Signature help ignores commas inside nested parameter lists, strings, and block comments. It does
not use tree-sitter AST state.

### Inlay Hints

`src/features/inlay_hints.rs` labels positional STEP arguments with schema attribute names using a
range-scoped text scanner. It includes inherited attributes and does not use tree-sitter AST state.

### IFC Boilerplate

`src/features/scaffold_completions.rs` provides minimal IFC STEP boilerplate without tree-sitter AST
state. It is surfaced as snippet-aware completions for `!ifc`, `!!ifc`, and `!!!ifc`, and as plain
text code actions for empty or whitespace-only documents.

Boilerplate generation targets the current default schema. Older schema versions are not generated
directly.

### Semantic Tokens

`src/features/semantic_tokens.rs` handles LSP semantic-token encoding for requested ranges.
The lexer in `src/features/semantic_tokens/lexer.rs` scans borrowed document text and emits
absolute byte ranges for STEP keywords, entity/type identifiers, instance ids, strings, numbers,
enumerations, operators, and block comments.

Semantic tokens do not use tree-sitter or schema docs.

### Diagnostics

`src/diagnostics/datatype.rs` validates parsed IFC entity instances against runtime schema documentation. Diagnostics currently require AST-backed `Document::instances`.

The provider supports:

- schema non-compliant entity names
- wrong local reference target types
- unresolved local references
- primitive datatype mismatches
- enumeration mismatches
- argument-count mismatches
- invalid `$` usage for required attributes
- invalid `*` usage except where an inherited attribute is derived in a subtype
- aggregate cardinality/type mismatches
- `SELECT` branch validation
- inline typed values such as `IFCLABEL('Name')`

It does not evaluate general EXPRESS `WHERE` rules.

## Project Structure

```text
build.rs
src/
  main.rs
  backend.rs
  config.rs
  document.rs
  document_index.rs
  diagnostics/
  features/
  schema/
  step/
samples/
  *.ifc
```

## Non-Goals

Avoid these unless explicitly requested:

- cross-file indexing
- incremental parsing infrastructure
- background worker systems
- retaining ASTs for every open document
- large abstraction layers or service registries
- premature support for unimplemented LSP features

## Guiding Principle

Keep the implementation direct. If a new abstraction does not clearly support the current feature set or large-file behavior, it probably does not belong here yet.
