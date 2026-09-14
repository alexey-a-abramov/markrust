# MarkRust

Fast, native, local-first Markdown workspace for developers.

MarkRust is a free, open-source editor that keeps documents as plain UTF-8 Markdown on disk while rendering a true WYSIWYG view (Typora-style: delimiters hidden unless the caret or a selection intersects the node). Source mode with delimiter masking and a side-by-side split are optional (`Cmd/Ctrl+Shift+M`).

## Status

**v0.1.0 alpha** — MVP editor with GPUI workspace shell, file tree, outline, command palette, autosave, and a broad automated Rust and website test suite.

## Screenshots

<!-- Add screenshots after the first public release build. -->
| Light | Dark |
|---|---|
| _Coming soon_ | _Coming soon_ |

## Install

### GitHub Releases

Prebuilt release archives are not published yet. Follow the
[releases page](https://github.com/alexey-a-abramov/markrust/releases) for the
first tagged alpha; until then, build from source below.

Windows builds are planned after the v0.1 alpha release gate.

### Homebrew

The Homebrew tap is not published yet. The formula template lives in
[`packaging/homebrew/markrust.rb`](packaging/homebrew/markrust.rb) and needs
release-archive SHA256 checksums before it can be installed.

### Cargo

`markrust-core` is crates.io-ready. The desktop binary currently depends on GPUI from a pinned Zed git revision, so install from the repository until GPUI is available on crates.io:

```bash
cargo install --git https://github.com/alexey-a-abramov/markrust markrust
```

After the package is published:

```bash
cargo install markrust
```

Requires **Rust 1.96+**.

## Build from source

```bash
git clone https://github.com/alexey-a-abramov/markrust.git
cd markrust
```

**Linux:** install GPUI system dependencies first:

```bash
bash scripts/install-linux-deps.sh
```

Then:

```bash
cargo build --release -p markrust
./target/release/markrust --gui
```

Run the full release checklist locally:

```bash
bash scripts/release.sh
```

## Keyboard shortcuts

| Shortcut | Action |
|---|---|
| `Cmd/Ctrl+N` | New document |
| `Cmd/Ctrl+O` | Open file |
| `Cmd/Ctrl+Shift+O` | Open folder (workspace) |
| `Cmd/Ctrl+S` | Save |
| `Cmd/Ctrl+W` | Close tab |
| `Cmd/Ctrl+Z` | Undo |
| `Cmd/Ctrl+Shift+Z` | Redo |
| `Cmd/Ctrl+P` | Command palette |
| `Cmd/Ctrl+Shift+M` | Cycle WYSIWYG / Source / Split |
| `Cmd/Ctrl+Shift+T` | Toggle light/dark theme |
| `Cmd/Ctrl+C` / `X` | Copy / cut as Markdown (empty caret copies/cuts the current block) |
| `Cmd/Ctrl+B` / `I` / `E` / `K` | Bold / italic / code / link |
| `Option+Left` / `Right` (or `Ctrl+Left` / `Right`) | Move by word |
| `Option+Shift+Left` / `Right` (or `Ctrl+Shift+Left` / `Right`) | Select by word |
| `Option+Backspace` / `Delete` (or `Ctrl+Backspace` / `Delete`) | Delete by word |
| `Cmd+Backspace` / `Delete` | Delete to line start / end |
| `Cmd+Up` / `Down` (or `Ctrl+Home` / `End`) | Document start / end |
| `Cmd+Shift+Up` / `Down` (or `Ctrl+Shift+Home` / `End`) | Select to document start / end |
| `Cmd+Left` / `Right` | Line start / end |
| `Page Up` / `Down` | Move by a viewport of lines (`Shift` extends) |

## Workspace crates

| Crate | Purpose |
|---|---|
| `markrust-core` | Rope buffer, undo/redo, comrak RichTree + source spans |
| `markrust-editor` | WYSIWYG view, source-mode masking, layout, GPUI elements |
| `markrust-app` | GPUI shell (workspace, file tree, palette) |
| `markrust` | CLI + desktop binary |

## Development

```bash
cargo build --workspace
cargo test --workspace
cargo bench -p markrust-core --bench parse
cargo bench -p markrust-editor --bench layout
cargo run -p markrust -- --version
cargo run -p markrust            # launches GUI
```

CI runs `fmt`, `clippy`, Rust tests, and website tests on every push/PR. A
`v*` tag builds release archives; publishing them remains part of the v0.1
release gate.

## Website

Product site and documentation live in [`website/`](website/). Built with Astro (static HTML, minimal JS).

```bash
pnpm install          # from repo root
pnpm dev              # http://localhost:4321
pnpm build            # output → website/dist/
```

Site: [markrust.org](https://markrust.org) (when deployed). The GitHub Pages
and custom-domain handoff is in [the deployment guide](docs/deployment.md).

## License

Mozilla Public License 2.0 — see [LICENSE-MPL-2.0](LICENSE-MPL-2.0).

**MarkRust** is a trademark of Alexey Abramov — see [TRADEMARK.md](TRADEMARK.md). Community forks and truthful references are welcome; do not imply official endorsement in product names.
