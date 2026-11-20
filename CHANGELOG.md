# Changelog


## [Unreleased]

### Changed

- **New STEP parser.** A hand-written parser replaces tree-sitter. Every feature now works on files
  up to 4 GiB; larger files get a single syntax diagnostic. Opening a 29 MB model is about 6× faster
  (2.2 s → 0.4 s to first diagnostics) and needs about 6× less memory (1.8 GB → 0.3 GB). (#166, #167)
- **Removed the `ifc.analysis.astFileSizeLimitMb` setting**, along with the size limit it controlled.
  Setting it no longer has any effect. (#167)
- **Diagnostic positions count UTF-16 code units**, as LSP requires. They used to count bytes, so
  ranges were off on lines with non-ASCII text such as umlauts. (#167)
- **Syntax errors are more focused.** An unclosed string or comment gives one error and the rest of
  the file keeps working. Binary values are accepted, values nested more than 256 levels deep are
  reported, and lowercase entity names are a syntax error. (#167)
- **Edits are checked incrementally.** An edit re-parses only the statements it touches and
  revalidates them and the statements that refer to them. (#168)
- **Diagnostics wait for a typing burst to end.** An isolated edit is still checked at once; during
  fast typing, diagnostics arrive 100 ms after the last edit. (#170)
- **Syntax diagnostics no longer depend on the schema.** They are published even when the file's
  schema is unknown or unsupported, and the warning about the unknown schema appears once per
  document. (#117)
- **Boilerplate for non-`file:` documents** (such as `inmemory://model/house.ifc`) now uses the
  document's name in `FILE_NAME`. (#119)

### Added

- **Completions:** the next instance id, entity names (as snippets with a generated GlobalId), GUIDs,
  enumeration values and references filtered to compatible entity types. (#115, #131)
- **Related-entity navigation:** code actions jump to the containing spatial structure, aggregation
  parent or parts, and host or opening elements. (#112)
- **Call hierarchy** over instance references, with relationship instances as intermediate nodes
  and items labelled with `Name` or `GlobalId`. Incoming calls list at most 1000 instances. (#125)
- **Find references from an entity name,** not only from an `#id`. (#157)
- **Browser build:** releases include `ifc-language-server-<version>-wasm32.tar.gz`, which runs the
  server in a Web Worker. (#120)
- **Editor setups** for Neovim, Helix, Emacs, Notepad++, Kate and Sublime Text, in
  [IFC-LS-integrations](https://github.com/Amixx/IFC-LS-integrations). (#152)

### Fixed

- The outline no longer crashes the server on very long aggregation chains; it nests up to 256
  levels. (#162)
- `semanticTokens/range` no longer hangs on very long lines. (#143)
- Inlay hints and semantic tokens are returned when the requested range ends past the document's
  end. (#123)
- An attribute redeclared as `DERIVE` must be `*`; `$` or a value there is reported. (#163)
- An unrecognized token is reported once instead of twice. (#116)
- A newline at the end of a file inside an unclosed comment no longer shifts positions on the last
  line. (#151)
- Hover and the outline no longer re-run diagnostics on documents that haven't changed. (#124)

### Performance

- The bundled schemas are parsed on first use: first diagnostics after start 911 ms → 200 ms. (#118)
- Faster edits and datatype validation on large models. (#134, #135)

## Earlier releases

See [GitHub Releases](https://github.com/NepomukWolf/IFC-Language-Server/releases) for v0.6.0
and before.

[Unreleased]: https://github.com/NepomukWolf/IFC-Language-Server/compare/v0.6.0...HEAD
