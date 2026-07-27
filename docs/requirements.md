# IFC-Language-Server Requirements

**For Human Developers:** Reference this file in your agent instructions (`AGENTS.md`, `.claude/`, etc.)

## Scope

### Included Support for IFC Schema Versions:

See [IFC Schema Specifications](https://technical.buildingsmart.org/standards/ifc/ifc-schema-specifications/):
- IFC 2.3.0.1  
  https://standards.buildingsmart.org/IFC/RELEASE/IFC2x3/TC1/EXPRESS/IFC2X3_TC1.exp
- IFC 4.0.2.1  
  https://standards.buildingsmart.org/IFC/RELEASE/IFC4/ADD2_TC1/EXPRESS/IFC4.exp
- IFC 4.3.2.0  
  https://standards.buildingsmart.org/IFC/RELEASE/IFC4_3/HTML/IFC4X3_ADD2.exp

For these officially supported versions:

- the repository should not store local EXPRESS files
- the official EXPRESS definitions should be fetched at compile time
- the fetched EXPRESS source should be embedded into the binary
- language-server startup and restart should not require network access to load official schemas

## Features

### Hover

#### Entity Definition Hover

Hovering over an entity definition such as `IFCWALL` should display documentation for that entity.

Preferably, the hover should include:

- a short description of the entity
- a list or table of attributes, including:
  name, type, and description
- a link to the official web documentation for the entity

#### Entity ID Hover

Hovering over an entity id such as `#1234` should display a preview of the line in the file where that entity is defined.

#### Derived Attribute Hover

Hovering over an omitted value (`*`) should provide derived-value information only for the IFC cases represented in STEP instance arguments:

- `IfcSIUnit.Dimensions`, resolved from `Name`
- inherited `IfcGeometricRepresentationSubContext` attributes, resolved from `ParentContext`

General EXPRESS `DERIVE` evaluation and hover information for other omitted values are out of scope.

### Go to Definition

When a user invokes Go to Definition, for example through an editor context menu or keyboard shortcut, the language server should:

- identify the IFC entity reference at the cursor position, such as `#123`
- resolve that reference to its corresponding entity definition within the same document
- return the precise target location, including URI and range

### Find References

The language server should implement find references functionality for IFC symbols.

For the current scope, this should be limited to a single document. Cross-file indexing is not required.

### Document Highlight

The language server should implement document highlight functionality for local IFC STEP ids.

When the cursor is on a local id such as `#123`, the server should return all same-document
occurrences of that id, including the definition token and reference tokens.

Cross-file highlights are out of scope.

### Signature Help

The language server should provide signature help for IFC entity parameter lists.

When the cursor is inside a STEP entity instance argument list, the server should:

- identify the selected IFC entity, such as `IFCWALL`
- show the entity parameter signature from the selected schema docs
- highlight the active parameter based on the cursor position
- include concise parameter metadata where available, such as type information and optionality

Signature help should not require tree-sitter AST state.

### Inlay Hints

The language server should label positional STEP entity arguments with schema attribute names.
Hints should be range-scoped, include inherited attributes, and remain available without tree-sitter
AST state.

### Semantic Tokens

The language server should provide range-based semantic tokens for IFC STEP syntax highlighting.

Semantic tokens should:

- be enabled by default
- be configurable through `initializationOptions.semanticTokensEnabled`
- remain available without tree-sitter AST state
- support large files through range requests rather than requiring full-document tokenization

### Large IFC Files

The language server should remain usable for large IFC files without retaining tree-sitter ASTs for every open document.

For files above the configured AST parsing limit:

- local reference hover should remain available
- entity definition hover should remain available when schema docs are available
- go-to-definition should remain available for local `#id` references
- find-references should remain available for local `#id` tokens
- document highlight should remain available for local `#id` tokens
- signature help should remain available for IFC entity parameter lists when schema docs are available
- schema-backed inlay hints should remain available
- range-based semantic tokens should remain available
- AST-backed schema diagnostics may be disabled
- derived `*` hover may be disabled

The AST parsing limit should be configurable through `initializationOptions.astFileSizeLimitMb`.

### Schema Diagnostics

The LS should provide schema-aware diagnostics for things like:

- Datatype checking (e.g., a reference points to IFCOWNERHITORY, even though it should point to an IFCWALL)
- Unsupported IfcVersions (e.g. display message to user)
- Entity Schema Compliance (e.g., IFCALIGNMENT is not part of IFC2x3)

Appropriate user-facing information (underlining, hover text) is part of the diagnostics.

### Custom EXPRESS Schemas

Next to the built-in support for the three major versions of IFC, the user should be able to add support for more schema versions via EXPRESS definition files. This functionality is exposed via the `initializationOptions`. Both forcing a specific schema (`overwriteExpSchemaWithLocal`) and adding to the existing list of schemas (`addLocalSchemaToSelection`) are supported. Examples Configuration:

```json
{
  "overwriteExpSchemaWithLocal": "/Users/alice/dev/express/IFC4x2.exp",
  "addLocalSchemaToSelection": [
    "/Users/alice/dev/express/IFC4x1.exp",
    "/Users/alice/dev/express/custom-schemas"
  ]
}
```
