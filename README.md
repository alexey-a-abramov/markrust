# MarkRust

Fast, native, local-first Markdown workspace for developers.

MarkRust is a free, open-source editor that keeps documents as plain UTF-8 Markdown on disk while rendering a true WYSIWYG view (bold is bold). Source mode with Typora-style delimiter masking and a side-by-side split are optional (`Cmd/Ctrl+Shift+M`).

## Status

**v0.1.0** — MVP editor with GPUI workspace shell, file tree, outline, command palette, autosave, and 29+ unit tests.

## Screenshots

<!-- Add screenshots after the first public release build. -->
| Light | Dark |
|---|---|
| _Coming soon_ | _Coming soon_ |

## Install

### GitHub Releases (recommended)

Download a prebuilt binary for your platform from the [latest release](https://github.com/alexey-a-abramov/markrust/releases/latest):

| Platform | Archive |
|---|---|
| macOS (Apple Silicon) | `markrust-macos-aarch64.tar.gz` |
| macOS (Intel) | `markrust-macos-x86_64.tar.gz` |
| Linux (x86_64) | `markrust-linux-x86_64.tar.gz` |

```bash
tar xzf markrust-*.tar.gz
install -m 755 markrust ~/.local/bin/   # or /usr/local/bin
```

Windows builds are planned for v0.2.

### Homebrew

```bash
brew tap alexey-a-abramov/markrust
brew install markrust
```

The formula template lives in [`packaging/homebrew/markrust.rb`](packaging/homebrew/markrust.rb). After the first release, copy it into the [`homebrew-markrust`](https://github.com/alexey-a-abramov/homebrew-markrust) tap and update the SHA256 checksums.

### Cargo

`markrust-core` is crates.io-ready. The desktop binary currently depends on GPUI from a pinned Zed git revision, so install from the repository until GPUI is available on crates.io:

```bash
cargo install --git https://github.com/alexey-a-abramov/markrust markrust
```

Once published:

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
| `Cmd/Ctrl+Shift+M` | Cycle Rich / Source / Split |
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

CI runs `fmt`, `clippy`, and `tests` on every push/PR. Pushing a `v*` tag triggers a GitHub Release with macOS and Linux binaries.

## Website

Product site and documentation live in [`website/`](website/). Built with Astro (static HTML, minimal JS).

```bash
pnpm install          # from repo root
pnpm dev              # http://localhost:4321
pnpm build            # output → website/dist/
```

Site: [markrust.org](https://markrust.org) (when deployed).

## License

Mozilla Public License 2.0 — see [LICENSE-MPL-2.0](LICENSE-MPL-2.0).

**MarkRust** is a trademark of Alexey Abramov — see [TRADEMARK.md](TRADEMARK.md). Community forks and truthful references are welcome; do not imply official endorsement in product names.
