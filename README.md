# IFC-Language-Server

`ifc-language-server` is a lightweight Language Server Protocol server for [IFC STEP P21](https://technical.buildingsmart.org/standards/ifc/ifc-schema-specifications/) files (ISO 10303). It provides schema-aware IFC editing features, local STEP id navigation, diagnostics, semantic tokens, and editor assistance for IFC entity arguments.


## Current Capabilities

- Hover on IFC entity names such as `IFCWALL` and `IFCSPACE`
- Hover preview for entity references such as `#123`
- Derived `*` hover for `IfcSIUnit` and `IfcGeometricRepresentationSubContext`
- Go-to-definition for local entity references
- Find-references within the current document
- Document highlight for local STEP ids such as `#123`
- Signature help for IFC entity parameter lists
- Document symbols for editor Outline, breadcrumbs, and go-to-symbol navigation
- IFC STEP boilerplate via `!ifc` / `!!ifc` / `!!!ifc` completions and empty-document code actions
- Range-based semantic tokens for syntax highlighting
- Schema-aware diagnostics for:
  - invalid local reference targets
  - primitive datatype mismatches
  - invalid enumeration values
  - incorrect entity argument counts
  - invalid `$` / `*` usage for attributes

## Supported IFC Schema Versions

Bundled entity documentation is currently included for:

- IFC 2.3.0.1
- IFC 4.0.2.1
- IFC 4.3.2.0

## Current Limitations

- Single-document navigation only
- Full-document reparsing on open and change
- Diagnostics currently focus on entity-instance argument validation, not full EXPRESS rule evaluation
- No general completion, rename, or formatting support
- Code actions are currently limited to IFC boilerplate insertion for empty documents
- The LSP server provides range-based semantic tokens, but does not provide full-document semantic-token or TextMate grammar support

## Installation

### Editor integrations

Extensions for VSCode and Zed are available for local installation (though still under development):

- [VSCode](https://github.com/NepomukWolf/vscode-ifc)

- [Zed](https://github.com/Finradon/zed-ifc)

Other editors are set up with the files in [`editors/`](editors) and an `ifc-language-server` binary on your `PATH` (see [Manual](#manual)).

#### Neovim

Neovim 0.11 or later. Copy the syntax, file-type and LSP files into your config and enable the server in `init.lua`:

```sh
cp -R editors/neovim/. ~/.config/nvim/
```

```lua
vim.lsp.enable('ifc')
```

Colouring and all LSP features work on files of at least 180 MB. The file-type rule takes priority over size-based detection such as LazyVim's snacks.nvim `bigfile`. On lines of hundreds of thousands of characters, semantic colours take minutes to fill in, as Neovim's cost per token grows with line length.

#### Helix

Add to `~/.config/helix/languages.toml`:

```toml
[[language]]
name = "ifc"
scope = "source.ifc"
file-types = ["ifc"]
language-servers = ["ifc-language-server"]

[language-server.ifc-language-server]
command = "ifc-language-server"

[[grammar]]
name = "ifc"
source = { git = "https://github.com/NepomukWolf/tree-sitter-ifc", rev = "168e01eba32896cfbdacea38fdb068343d43073c" }
```

Then build the grammar and install its highlight query:

```sh
hx --grammar fetch && hx --grammar build
mkdir -p ~/.config/helix/runtime/queries/ifc
cp ~/.config/helix/runtime/grammars/sources/ifc/queries/highlights.scm ~/.config/helix/runtime/queries/ifc/
```

LSP features work at any file size. Helix stops parsing after a fixed 500 ms, so files above roughly 8 MB stay uncoloured.

#### Emacs

Emacs 29 or later (tested with Emacs 31.1 and its built-in Eglot). Put `editors/emacs/ifc-mode.el` in a directory on your `load-path` and add to `init.el`:

```elisp
(require 'ifc-mode)
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs
               '(ifc-mode . ("ifc-language-server"
                             :initializationOptions (:semanticTokensEnabled :json-false)))))
(add-hook 'ifc-mode-hook #'eglot-ensure)
```

Colouring, diagnostics, hover, go-to-definition, references and completion work on files of at least 56 MB. Eglot only requests full-document semantic tokens, which the server does not provide, so the snippet turns them off. The outline (`imenu`) takes seconds on a 13.6 MB file and minutes on a 56 MB one, as Eglot locates each symbol by counting lines from the top of the buffer. Emacs asks for confirmation before opening files above `large-file-warning-threshold` (10 MB by default).

#### Notepad++

In Notepad++, open **Language > User Defined Language > Define your language…**, click **Import…**, choose `editors/notepad++/IFC_STEP.udl.xml` and restart. `.ifc` files are coloured up to at least 56 MB. This is colouring only: NppLspClient 0.0.40, the available LSP client, does not load in Notepad++ 8.9.

### Manual

Release binaries are available on the [Releases](https://github.com/NepomukWolf/IFC-Language-Server/releases) page.

After downloading a release:

1. Place the `ifc-language-server` binary somewhere on your `PATH`, or configure your editor to point to the absolute binary path.
2. Register it as the language server for IFC STEP files in your editor or IDE.

## Development

### Prerequisites

- Rust (stable): https://www.rust-lang.org/tools/install
- An editor with LSP client support such as [Helix](https://docs.helix-editor.com/languages.html), [Neovim](https://neovim.io/doc/user/lsp.html), [VS Code](https://code.visualstudio.com/api/language-extensions/language-server-extension-guide), or Zed

### Build

Build a release binary with:

```bash
cargo build --release
```

### Configuration

The server accepts configuration through LSP `initializationOptions`.

Supported options:

- `overwriteExpSchemaWithLocal`
  - A single path to a local `.exp` file.
  - If set, the server always uses that schema for diagnostics and hover, regardless of the `FILE_SCHEMA(...)` declared in the IFC file.
  - If the forced schema does not match the schema declared in the IFC file, the server shows a warning and continues with the forced schema.

- `addLocalSchemaToSelection`
  - A list of local paths.
  - Each path may point to either:
    - a single `.exp` file
    - a directory containing `.exp` files
  - These schemas are added to the server's schema selection pool.
  - When an IFC file declares a schema name that is not bundled, the server checks the configured local schemas for an exact `SCHEMA ...;` name match and loads the matching schema on demand.

- `astFileSizeLimitMb`
  - Maximum file size in MiB for AST-backed features.
  - Defaults to `70`.
  - Files above this limit keep basic hover/navigation, document highlight, signature help, and semantic tokens available, but skip schema diagnostics and derived-value hover.

- `semanticTokensEnabled`
  - Enables range-based semantic tokens.
  - Defaults to `true`.

Configuration changes currently require restarting the server.

Example `initializationOptions`:

```json
{
  "overwriteExpSchemaWithLocal": "/Users/alice/dev/express/IFC4x2.exp",
  "astFileSizeLimitMb": 128,
  "semanticTokensEnabled": true,
  "addLocalSchemaToSelection": [
    "/Users/alice/dev/express/IFC4x1.exp",
    "/Users/alice/dev/express/custom-schemas"
  ]
}
```

### Grammar And Parser

This project depends on the published [`tree-sitter-ifc`](https://crates.io/crates/tree-sitter-ifc) crate for IFC parsing.

If you are developing the grammar itself, work in the [`tree-sitter-ifc`](https://github.com/NepomukWolf/tree-sitter-ifc) repository and publish a new crate version when needed. During local development, you can temporarily override the crates.io dependency with a `[patch.crates-io]` entry that points at a local checkout.

## Documentation

- [Architecture](./docs/architecture.md)
- [Coding Guidelines](./docs/coding-guidelines.md)
- [Diagnostics Capabilities](./docs/diagnostics-capabilities.md)
- [Requirements](./docs/requirements.md)

## Contributing

Contributions are welcome. Read [Architecture](./docs/architecture.md) and [Coding Guidelines](./docs/coding-guidelines.md) before submitting a PR.

## License

MIT
