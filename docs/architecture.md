# IFC-Language-Server Architecture

**For Human Developers:** Reference this file in your agent instructions (`AGENTS.md`, `.claude/`, etc.).

## Goal

This project stays small and focused.

The implemented architecture is built around:

- incremental document sync
- one lightweight text index per open IFC document
- a STEP statement index and parameter parser for the active document
- bounded background diagnostic processing
- generated in-memory EXPRESS schema documentation

The shipped LSP features are:

- hover
- go-to-definition
- find-references
- document highlight
- signature help
- inlay hints
- IFC boilerplate completions and code actions
- related-entity navigation code actions
- range-based semantic tokens
- schema-aware diagnostics

## Core Design

The runtime code revolves around three main concepts:

- `Backend`
  The `tower-lsp` entrypoint. It owns the open-document map, loaded schema docs, server configuration, and the diagnostics scheduler.
- `Document`
  The in-memory representation of one IFC file. It always stores source text and cheap text indexes. The active document also stores the statement index (`step::Document`).
- `SchemaDocCollection` and `SchemaDoc`
  In-memory lookup tables for EXPRESS entity and type documentation, keyed by normalized schema name.

Feature modules in `src/features/` stay thin and operate on `&Document`.
Diagnostics operate on an immutable `DiagnosticSnapshot` plus a selected `SchemaDoc`.

## Data Flow

On open or document change:

1. `Backend` unloads the statement index of other open documents.
2. On open, the active document stores the full text and builds its statement index with `step::Document::parse`.
3. On change, the active document applies every LSP content change in order. A ranged change edits the source text and re-parses only the statements it touches (`step::Document::edit`), falling back to a full parse when the edited region no longer splits into whole statements. A full-text change parses from scratch.
4. The lightweight text index is edited in place, or rescanned when the edit cannot be applied locally.
5. The notification handler returns. A background task takes a diagnostic snapshot containing the statement index, the recorded edits and a reference-counted source-text revision: at once for an isolated change, or 100 ms after the change when it came less than 100 ms after the previous one. A newer change supersedes the task before it takes its snapshot.
6. The snapshot is submitted to a bounded background scheduler.
7. Diagnostics revalidate only the statements an edit touched, plus the statements referring to ids whose definition changed, and publish results only if the snapshot is still the newest generation for that URI.

On hover or document-symbol requests:

1. `Backend::ensure_document_loaded` rebuilds the statement index of the requested document if it was unloaded.
2. Reloading one document unloads the statement index of the other open documents.
3. Hover decodes only the statements it reads. Document symbols decode relationships and outline-classified entities in bounded blocking work.
4. If request-time reloading produced a diagnostic snapshot, it is submitted to the same background scheduler after the feature result is computed, unless the document changed since the snapshot was taken or the document's current diagnostics generation was already scheduled with the same text and schema.

Definition, references, document-highlight, signature-help, and semantic-token range requests read
the stored document text and text index only. These requests do not reload the statement index or
publish diagnostics.

Completion requests also use only stored text, the lightweight index, and the selected schema docs.

## Backend

`src/backend.rs` owns:

- `documents: Arc<RwLock<HashMap<Url, Document>>>`
- `schema_docs: Arc<RwLock<SchemaDocCollection>>`
- `config: Arc<RwLock<ConfigState>>`
- `diagnostic_scheduler: DiagnosticScheduler`

The server advertises:

- `textDocument/hover`
- incremental text document sync
- `textDocument/definition`
- `textDocument/documentSymbol`
- `textDocument/references`
- `textDocument/documentHighlight`
- `textDocument/signatureHelp`
- `textDocument/inlayHint`
- `textDocument/completion`
- `textDocument/codeAction`
- `workspace/executeCommand`
- `textDocument/semanticTokens/range`

Diagnostics are published with `textDocument/publishDiagnostics` after background processing on
open, change, and request-time reloads whose text or schema differs from the last scheduled run. The scheduler runs one diagnostic computation at a time,
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
    pub lazy: Option<Arc<step::Document>>,
    pub lazy_edits: LazyEdits,
    pub validation_cache: Arc<ValidationCache>,
    pub schema_name: Option<String>,
    pub line_offsets: Vec<usize>,
    pub definitions: HashMap<u32, usize>,
    pub references: HashMap<u32, Vec<usize>>,
    // ...
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
- `lazy`
  The statement index: one entry per `#id = NAME(...);` statement with its span, id, entity name and parse errors. Parameters are decoded on demand from the statement text.
- `lazy_edits` and `validation_cache`
  The statements changed since the last diagnostics run and their cached results, used for incremental revalidation.

`EntityInstanceInfo` is the decoded form of one statement: entity name, parameter values and their
ranges. Features decode only the statements they need.

`src/document_index.rs` provides the lightweight scanner used by every document. It records line starts, local ids, references, and `FILE_SCHEMA(...)`.

## Parsing

`src/step.rs` splits the text into statements and parses each one into a flat parameter tape.

- A statement ends at its `;`, or before a line that starts a new `#id =` definition, so an unclosed
  string or a missing `;` stays inside one statement.
- A malformed scalar value (a stray token, a bad number, reference or enumeration) is skipped up to
  its next `,`, `)` or `;` and reported; the statement keeps its other parameters.
- A statement missing only its outer `)` before `;` keeps its parameters.
- Values nest at most 256 levels deep, which keeps the recursive consumers of decoded values within
  a wasm stack. Deeper nesting is reported as unsupported.
- Any other error, such as an unclosed string, drops the statement's parameters. Its id and name
  stay indexed.

Syntax diagnostics come from these parse errors. An unbalanced quote in a statement is reported at its
opening quote.

## Loading And Unloading

`Document::new_unloaded(text)` creates a document with source text, the lightweight text index and
a statement index. `Document::unload_parse_state()` drops the statement index and keeps the text and
text index. `Document::reload_parse_state()` rebuilds both indexes from the text.

The backend keeps a statement index for at most one active document at a time. Other open documents
remain in memory as text plus the lightweight index.

Background diagnostics may retain a reference-counted statement index and source-text revision after
the live document unloads or changes. Snapshot retention remains bounded to one running job plus the
newest pending snapshot for each URI.

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

Document highlight does not use the statement index or schema docs.

### Signature Help

`src/features/signature_help.rs` provides IFC entity parameter signatures from the selected schema
docs. It uses a lightweight text scanner to find the current STEP entity argument list and count
top-level commas before the cursor to choose the active parameter.

Signature help ignores commas inside nested parameter lists, strings, and block comments. It does
not use the statement index.

### Inlay Hints

`src/features/inlay_hints.rs` labels positional STEP arguments with schema attribute names using a
range-scoped text scanner. It includes inherited attributes and does not use the statement index.

### IFC Boilerplate

`src/features/scaffold_completions.rs` provides minimal IFC STEP boilerplate without the statement
index. It is surfaced as snippet-aware completions for `!ifc`, `!!ifc`, and `!!!ifc`, and as plain
text code actions for empty or whitespace-only documents.

Boilerplate generation targets the current default schema. Older schema versions are not generated
directly.

### Completions

`src/features/completions.rs` provides next-id and schema-aware completions without the statement index. It
uses the shared STEP argument scanner to locate the active flattened attribute, resolves aliases,
aggregates, and entity selects through `SchemaDoc`, and scans indexed local definitions on demand
when compatible reference suggestions are requested. IFC GUID generation is shared with scaffold
rendering. Snippet-capable clients receive complete entity constructions and schema-shaped aggregate
values; entity snippets request the client's parameter-hints command after insertion so signature help
opens at the first placeholder. Other clients retain plain entity-name insertion. Entity and reference
documentation is resolved through `completionItem/resolve` when supported and attached eagerly
otherwise. Reference lists are capped at 100 results and returned as incomplete when additional
matches exist. The `#` trigger is interpreted by context: it completes the next definition prefix on
an otherwise empty line and local references inside entity arguments. No semantic entity-instance
collection is retained for completion.

### Related Entity Navigation

`src/features/related_entities.rs` offers code actions that jump between an entity and the small,
fixed set of IFC relationship entities that reference it: `IfcRelContainedInSpatialStructure`,
`IfcRelAggregates`, `IfcRelVoidsElement`, and `IfcRelFillsElement`. Resolution starts from the
entity under the cursor and only walks the relationship instances already indexed against it via
`Document::references`, so cost does not grow with file size. Relationship attribute positions are
resolved from the selected `SchemaDoc` rather than hardcoded, so it stays correct across schema
versions.

`src/features/step_scan.rs` is a small shared STEP argument-list scanner (entity name, positional
argument boundaries) used by both this feature and inlay hints.

Each action carries a `Command` executed via `workspace/executeCommand`, which calls back into the
client with `window/showDocument` to move the cursor. This depends on the client declaring
`window.showDocument.support`; clients that do not implement it (e.g. Zed before its
`window/showDocument` support landed) will show the actions but the jump will not occur.

### Semantic Tokens

`src/features/semantic_tokens.rs` handles LSP semantic-token encoding for requested ranges.
The lexer in `src/features/semantic_tokens/lexer.rs` scans borrowed document text and emits
absolute byte ranges for STEP keywords, entity/type identifiers, instance ids, strings, numbers,
enumerations, operators, and block comments.

Semantic tokens do not use the statement index or schema docs.

### Diagnostics

`src/diagnostics/datatype.rs` validates transiently extracted entity instances against runtime
schema documentation. Diagnostics require a matching statement index and source-text revision.

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
  lib.rs
  main.rs
  rt.rs
  backend.rs
  config.rs
  document.rs
  document_index.rs
  diagnostics/
  features/
  schema/
  step.rs
samples/
  *.ifc
```

## Non-Goals

Avoid these unless explicitly requested:

- cross-file indexing
- general-purpose background worker systems beyond the diagnostics scheduler
- retaining a statement index for every open document
- large abstraction layers or service registries
- premature support for unimplemented LSP features

## Guiding Principle

Keep the implementation direct. If a new abstraction does not clearly support the current feature set or large-file behavior, it probably does not belong here yet.
