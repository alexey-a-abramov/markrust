# MarkRust

Fast, native, local-first Markdown notepad.

MarkRust is a free, open-source editor that keeps documents as plain UTF-8 Markdown on disk while rendering a true WYSIWYG view. Optional context hints are separate from shaped text, so words stay still. Source is a literal monospace Markdown editor; Split places it beside an editable visual view. Table row/column controls appear only in the active cell. Cycle modes with `Cmd/Ctrl+Shift+M`. Choose Native, Ocean or Forest highlight colors, or disable hints, in View.

## Status

**v0.8.1 alpha** — Native Markdown notepad with WYSIWYG, Source and Split,
document search, private draft recovery, 20 live UI languages and macOS release updates.

About MarkRust shows the version and compiled build date/time in UTC. The same
identity is available without opening the editor through `markrust --build-info`.
See [build identity](docs/architecture.md#build-identity) for timestamp semantics.

On macOS, the application menu checks GitHub Releases automatically or on demand.
Download and restart require confirmation; every window's private drafts must be
checkpointed before replacement. See [in-app updates](docs/deployment.md#in-app-macos-updates)
for platform, integrity and signing limits.

Save preserves the current Markdown without a normalization prompt or a
normalization menu beside Save. Concurrent external
edits merge when unambiguous; conflicts offer scrollable side-by-side review
and preserve the other version as an unsaved draft. See
[concurrent editing](docs/concurrent-editing.md) for guarantees and limits.

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

Prepared GitHub Actions build macOS Apple Silicon/Intel, Linux x86_64 and
experimental Windows x86_64 archives after verification on each push. Successful
run artifacts expire after 14 days; permanent Releases require an explicitly
chosen version tag. These workflows are not yet remotely verified or published.
See [binary distribution](docs/deployment.md#binary-distribution) for signing,
platform acceptance and download details.

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
| `Cmd/Ctrl+T` | New document tab |
| `Cmd/Ctrl+Shift+N` | New editor window |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous document tab |
| `Cmd+Shift+]` / `[` | Next / previous document tab |
| `Cmd/Ctrl+O` | Open file |
| `Cmd/Ctrl+Shift+O` | Open folder (workspace) |
| `Cmd/Ctrl+Shift+L` | Open a typed file or folder path |
| `Cmd/Ctrl+F` | Find in the current document |
| `Cmd/Ctrl+G` / `F3` | Next match |
| `Cmd/Ctrl+Shift+G` / `Shift+F3` | Previous match |
| `Cmd/Ctrl+S` | Save |
| `Cmd/Ctrl+Shift+S` | Save As |
| `Cmd/Ctrl+W` | Close tab |
| `Cmd/Ctrl+Z` | Undo |
| `Cmd/Ctrl+Shift+Z` | Redo |
| `Cmd/Ctrl+P` | Command palette |
| `Cmd/Ctrl+Shift+M` | Cycle WYSIWYG / Source / Split |
| `Cmd/Ctrl+Option+4` | Toggle WYSIWYG markup hints |
| `Cmd/Ctrl+Option+Shift+T` | Toggle light/dark theme (`Shift+T` is reserved for future tab reopening) |
| `Cmd/Ctrl+C` / `X` | Copy / cut as Markdown (empty caret copies/cuts the current block) |
| `Cmd/Ctrl+B` / `I` / `K` | Bold / italic / link |
| `Cmd/Ctrl+Option+K` | Inline code |
| `Cmd/Ctrl+Option+C` | Code block |
| `Cmd/Ctrl+1` … `6` | Heading 1 … 6 (mode picker is `Cmd/Ctrl+Option+1` … `3`) |
| `Cmd/Ctrl+Option+0` | Paragraph |
| `Cmd/Ctrl+Shift+7` / `8` / `9` | Numbered / bulleted / task list |
| `Cmd/Ctrl+Shift+.` | Blockquote |
| `Cmd/Ctrl+Shift+-` | Horizontal rule |
| `Cmd/Ctrl+Shift+X` | Strikethrough |
| `Cmd/Ctrl+Shift+I` | Image |
| `Cmd/Ctrl+Option+T` | Table |
| `Tab` / `Shift+Tab` (rich surface) | Indent / outdent |
| `Option+Left` / `Right` (or `Ctrl+Left` / `Right`) | Move by word |
| `Option+Shift+Left` / `Right` (or `Ctrl+Shift+Left` / `Right`) | Select by word |
| `Option+Backspace` / `Delete` (or `Ctrl+Backspace` / `Delete`) | Delete by word |
| `Cmd+Backspace` / `Delete` | Delete to line start / end |
| `Cmd+Up` / `Down` (or `Ctrl+Home` / `End`) | Document start / end |
| `Cmd+Shift+Up` / `Down` (or `Ctrl+Shift+Home` / `End`) | Select to document start / end |
| `Cmd+Left` / `Right` | Line start / end |
| `Page Up` / `Down` | Move by a viewport of lines (`Shift` extends) |

Open Location accepts absolute, relative, quoted and `~/` paths. Relative paths
resolve against the open folder, otherwise the active file's directory, then
the user's Documents directory (or the launch directory if unavailable).
The dialog always displays this base. The document header shows the current full path; the
sidebar names the open folder and separates files outside it. Recent files keep
their basenames and full-path tooltips.

Choose **View → Language** to switch menus and common chrome immediately in all
editor windows. The 20 embedded catalogs include English, Russian, Spanish,
French, German, Portuguese (Brazil), Italian, Dutch, Polish, Ukrainian, Turkish,
Arabic, Hebrew, Hindi, Bengali, Simplified Chinese, Japanese, Korean, Indonesian
and Vietnamese. Document text, paths and Markdown syntax never change. Initial
translations still need native-speaker review; Arabic/Hebrew text is stored in
logical Unicode order, not manually reversed. Full RTL layout and OS
accessibility/IME acceptance remain separate gates. Settings stay lean: language,
appearance, syntax colors, optional hints and panel visibility.

## Session recovery

Open tabs and unsaved buffers are checkpointed locally, including editor mode,
selection and the active Split pane. Recovery never silently overwrites a file
that changed while the app was closed; such buffers require an explicit save.
Background autosave protects private drafts rather than publishing changes to
the source file. Finder opens documents in the most recently active window.
Storage failures and recovery limits are shown in the window. Recovery is a
bounded safety net, not a backup service. See [everyday notepad behavior](docs/notepad-experience.md)
and [architecture](docs/architecture.md).

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
cargo run -p markrust -- --build-info
cargo run -p markrust            # launches GUI
```

CI runs `fmt`, `clippy`, Rust tests, website tests and native macOS GUI contracts
before the four-platform binary matrix. A matching `v*` version tag runs the
release gates and publishes checksum-backed assets only after all jobs succeed.
Public release approval, signing and native cross-platform acceptance remain
part of the [release gate](docs/deployment.md#repository-prerequisites-and-decisions).

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
