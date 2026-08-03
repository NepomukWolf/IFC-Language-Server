# IFC-Language-Server Architecture

**For Human Developers:** Reference this file in your agent instructions (`AGENTS.md`, `.claude/`, etc.).

## Goal

This project stays small and focused.

The implemented architecture is built around:

- incremental document sync
- one lightweight text index per open IFC document
- optional tree-sitter parse state for the active document
- bounded background diagnostic processing
- generated in-memory EXPRESS schema documentation

The shipped LSP features are:

- hover
- go-to-definition
- find-references
- document highlight
- signature help
- inlay hints
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
Diagnostics operate on an immutable `DiagnosticSnapshot` plus a selected `SchemaDoc`.

## Data Flow

On open or document change:

1. `Backend` unloads AST-backed parse state from other open documents.
2. On open, the active document stores the full text and receives a full tree-sitter parse.
3. On change, the active document applies every LSP content change in order. Ranged changes edit both the source text and the existing tree-sitter tree before one incremental reparse; full-text changes fall back to a full parse.
4. The lightweight text index is rebuilt from the final document text.
5. If the file is within the configured AST size limit, the resulting syntax tree is retained without eagerly materializing every entity instance.
6. If the file is above the AST size limit, the document is marked as `ast_skipped`.
7. `Backend` creates a diagnostic snapshot containing the tree and a reference-counted source-text revision.
8. The snapshot is submitted to a bounded background scheduler and the notification handler returns.
9. Diagnostics transiently materialize entity instances off the async runtime and publish results only if the snapshot is still the newest generation for that URI.

On AST-backed hover or document-symbol requests:

1. `Backend::ensure_document_loaded` reloads the requested document if its AST-backed state was previously unloaded.
2. Reloading one document unloads AST-backed state from the other open documents.
3. Hover parses only the entity instances needed for the requested value. Document symbols build a transient full instance collection in bounded blocking work.
4. If request-time reloading produced a diagnostic snapshot, it is submitted to the same background scheduler after the feature result is computed.

Definition, references, document-highlight, signature-help, and semantic-token range requests read
the stored document text and text index only. These requests do not reload tree-sitter parse state
or publish diagnostics.

There is still no incremental text indexing, incremental entity-instance rebuilding, background
indexing, diagnostic caching, or cross-document indexing.

## Backend

`src/backend.rs` owns:

- `documents: Arc<RwLock<HashMap<Url, Document>>>`
- `schema_docs: Arc<RwLock<SchemaDocCollection>>`
- `config: Arc<RwLock<ConfigState>>`
- `ast_skip_warning_shown: Arc<RwLock<HashSet<Url>>>`
- `diagnostic_scheduler: DiagnosticScheduler`

The backend creates a fresh tree-sitter parser through `new_parser()` when AST state must be rebuilt. It does not keep one long-lived parser.

The server advertises:

- `textDocument/hover`
- incremental text document sync
- `textDocument/definition`
- `textDocument/documentSymbol`
- `textDocument/references`
- `textDocument/documentHighlight`
- `textDocument/signatureHelp`
- `textDocument/inlayHint`
- `textDocument/semanticTokens/range`

Diagnostics are published with `textDocument/publishDiagnostics` after background processing on
open, change, and request-time reloads. The scheduler runs one diagnostic computation at a time,
keeps only the newest pending snapshot per URI, and discards stale results by generation.

## Logging

Runtime logs are written to `stderr` through `tracing`. The language server keeps `stdout`
reserved for the LSP protocol stream, while editor integrations are expected to decide whether
and how `stderr` should be captured into a log file. User-facing warnings still go through LSP
notifications such as `window/showMessage`.

## Document Model

`src/document.rs` is the central document representation:

```rust
pub struct Document {
    pub text: Arc<String>,
    pub tree: Option<tree_sitter::Tree>,
    pub ast_skipped: bool,
    pub schema_name: Option<String>,
    pub line_offsets: Vec<usize>,
    pub definitions: HashMap<u32, usize>,
    pub references: HashMap<u32, Vec<usize>>,
}
```

Important fields:

- `text`
  The reference-counted source text used by all features and shared with background snapshots.
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

`EntityInstanceCollection` is a transient semantic representation of parsed entity instances,
parameter values, and the local instance-ID lookup. It is built only by background diagnostics or
an explicit document-symbol request. Hover parses individual instances directly from the tree.

`src/document_index.rs` provides the lightweight scanner used by every document. It records line starts, local ids, references, and `FILE_SCHEMA(...)` without requiring tree-sitter.

## Loading And Unloading

`Document::new_unloaded(text)` creates a document with source text and the lightweight text index only.

`Document::unload_parse_state()` drops:

- `tree`

It keeps:

- `text`
- `line_offsets`
- `schema_name`
- `definitions`
- `references`

`Document::reload_parse_state(parser, ast_file_size_limit_bytes)` always rebuilds the text index first and performs a full parse when AST state must be loaded from scratch. `Document::apply_content_changes` converts LSP UTF-16 ranges to byte offsets, applies matching tree-sitter `InputEdit` values, and reparses with the edited previous tree. It rebuilds the full text index but defers semantic entity extraction. Files above the limit keep text-index-backed features available and set `ast_skipped = true` so the server does not repeatedly attempt to parse them.

The backend intentionally keeps AST-backed state for at most one active document at a time. Other open documents remain in memory as text plus the lightweight index.

Background diagnostics may temporarily retain a reference-counted tree-sitter tree and shared
source-text revision after the live document unloads or changes its parse state. Full semantic
instance extraction is serialized across diagnostics and document-symbol work, so at most one
transient collection is built at a time. Snapshot retention remains bounded to one running job plus
the newest pending snapshot for each URI.

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

### Semantic Tokens

`src/features/semantic_tokens.rs` handles LSP semantic-token encoding for requested ranges.
The lexer in `src/features/semantic_tokens/lexer.rs` scans borrowed document text and emits
absolute byte ranges for STEP keywords, entity/type identifiers, instance ids, strings, numbers,
enumerations, operators, and block comments.

Semantic tokens do not use tree-sitter or schema docs.

### Diagnostics

`src/diagnostics/datatype.rs` validates transiently extracted entity instances against runtime
schema documentation. Diagnostics require a matching tree and source-text revision.

`src/diagnostics/scheduler.rs` owns the bounded background queue. Repeated edits replace pending
work for the same URI, while monotonically increasing generations prevent results from older edits
or closed documents from being published.

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
- general-purpose background worker systems beyond the diagnostics scheduler
- retaining ASTs for every open document
- large abstraction layers or service registries
- premature support for unimplemented LSP features

## Guiding Principle

Keep the implementation direct. If a new abstraction does not clearly support the current feature set or large-file behavior, it probably does not belong here yet.
