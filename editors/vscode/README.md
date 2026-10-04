# Cogito for Visual Studio Code

Support for [Cogito](../../README.md) (`.cog` files):

- syntax highlighting: keywords, contracts, tests, string interpolation, raw
  and triple-quoted strings, numbers and operators;
- through the Cogito language server (`cogito lsp`): errors and warnings as
  you type (syntax, names, exhaustiveness, types), **quick fixes** for
  habits from other languages (`&&`, `null`, `xs.length()`, ...) and
  **Fix All**, **Format Document**
  (`cogito fmt`), documentation on hover for built-ins and your own
  functions (and the inferred type of a variable), completion (names in scope, built-ins, a module's functions
  after `module.`), the outline view, **Go to Definition** (local variables
  too), **Find All References**, and **Rename Symbol** (it refuses a new
  name that would change what another name refers to).

To install locally, put `cogito` on your `PATH` (or set `cogito.path` in the
settings), then install the extension's one dependency and link this folder
into your extensions directory:

```console
$ npm install
$ ln -s "$(pwd)" ~/.vscode/extensions/cogito-lang
```

and reload the window.

## Other editors

Any editor with a language-server client can use `cogito lsp`, which speaks
the Language Server Protocol on standard input and output.

Neovim (0.11 or later):

```lua
vim.filetype.add({ extension = { cog = "cogito" } })
vim.lsp.config("cogito", { cmd = { "cogito", "lsp" }, filetypes = { "cogito" } })
vim.lsp.enable("cogito")
```

Helix (`languages.toml`):

```toml
[language-server.cogito]
command = "cogito"
args = ["lsp"]

[[language]]
name = "cogito"
scope = "source.cogito"
file-types = ["cog"]
comment-token = "#"
indent = { tab-width = 2, unit = "  " }
language-servers = ["cogito"]
formatter = { command = "cogito", args = ["fmt", "-"] }
```
