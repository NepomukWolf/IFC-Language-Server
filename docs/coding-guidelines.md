# IFC-Language-Server Coding Guidelines

**For Human Developers:** Reference this file in your agent instructions (`AGENTS.md`, `.claude/`, etc.).

## Goals

Code in this repository should be:

- correct
- small
- readable
- deterministic
- easy to test

Prefer the simplest implementation that matches the documented current scope in `requirements.md`.

## Architectural Discipline

- Keep changes aligned with the current `Backend` / `Document` / `document_index` / `SchemaDocCollection` split.
- Prefer feature logic that operates on `&Document` instead of pushing more logic into the LSP trait implementation.
- Preserve the current single-document model unless the work explicitly requires widening scope.
- Preserve incremental tree-sitter parsing for document changes; use full parsing when loading AST state from scratch or receiving a full-text replacement.
- Keep text indexing full-document unless measurements demonstrate that further incremental complexity is needed.
- Materialize complete entity-instance collections only in bounded background or explicit on-demand work; use targeted AST extraction for local feature requests.
- Keep per-document text indexes small and independent from tree-sitter.
- Prefer the lightweight text index for navigation and simple token lookup.
- Use tree-sitter only when syntax structure or parsed parameter values are required.
- Do not retain AST-backed parse state for every open document.
- New features must account for files above the configured AST parsing limit.

## Code Style

- Prefer explicit, readable code over compact but opaque code.
- Use descriptive names for types, functions, and variables.
- Keep functions narrowly scoped to one job.
- Reuse existing helpers before adding new ones.
- Add comments only when the intent is not obvious from the code itself.
- Avoid dead code, placeholder branches, and speculative abstractions.
- When creating a new module, add descriptive comments explaining its purpose.

## Approval Boundaries

- New crates require explicit approval before being added.
  - Avoid adding crates unless they are clearly necessary.
  - Prefer the standard library or existing dependencies for small tasks.
- Major architectural changes require approval.
- Large refactors require approval.
- Changes to the document memory strategy, AST loading/unloading behavior, or AST size-threshold behavior require explicit user approval.
- No modifications to `./docs` without specific instructions by the user.
- No deleting, resetting, or reverting unrelated work without approval.
- Ask the user when encountering ambiguous requirements, conflicting docs, or risky edits.

## Error Handling

- Handle errors deliberately; do not ignore them silently.
- Prefer graceful fallbacks for unsupported schema versions, missing docs, absent syntax nodes, and documents without loaded AST state.
- Use `expect` only when failure is truly unrecoverable or in tests.
- Avoid panics in normal LSP request handling paths.
- Prefer runtime logging through `tracing` on `stderr`; keep `stdout` reserved for the LSP protocol stream.

## Testing

- Prefer small unit tests close to the code they exercise.
- Test behavior, not implementation noise.
- When changing text indexing, parsing, AST loading/unloading, AST size limits, schema loading, version detection, or hover rendering, update tests in the same module.
- Use short IFC snippets in tests unless a repository sample file is clearly more useful.
- Note: Builds require internet access due to the EXPRESS schema pulling at compile time in `build.rs`.

## Scope Discipline

- Implement only what is required by the current issue or the documented current scope.
- Do not silently broaden the project into diagnostics, workspace indexing, or additional LSP features.
- If a design can be simpler, prefer the simpler version.

## Verification Gate

The following commands must pass for a change to be verified:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo audit
cargo test --locked
cargo build --release --locked
```

`cargo audit` requires `cargo-audit` 0.22.2 or newer.

If the tests fail repeatedly when implementing a new feature, the run should be halted and the issues presented to the user.
