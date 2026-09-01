# MarkRust architecture

MarkRust is a **true WYSIWYG** Markdown editor: the user edits a rendered rich document; Markdown is the on-disk serialization. A **source** surface (Typora-style delimiter masking) and a **side-by-side** split are optional. All native Rust on GPUI.

The rope buffer is the single source of truth. A derived `RichTree` (comrak) is authoritative for interpretation and command targeting. Source-mode masking is a projection of that same grammar, not a second Markdown parser.

## Crate boundaries

| Crate | Responsibility | GUI deps |
|---|---|---|
| `markrust-core` | Rope buffer, undo, line index, comrak `RichTree` + source spans, revision tokens | None |
| `markrust-editor` | `HeadlessEditor` + source masking/layout; GPUI `MarkdownEditor` and `RichEditorView` | GPUI (view only) |
| `markrust-app` | `HeadlessWorkspace` session + GPUI window chrome | GPUI (Zed git pin) |
| `markrust` | CLI (`parse_args` / `run`) and desktop binary | GUI only for `--gui` |

**Rule:** `HeadlessEditor` (`markrust-editor::headless`) and `HeadlessWorkspace` (`markrust-app::session`) must not use GPUI types. The GPUI window maps keys/clicks to `WorkspaceCommand` / `EditorCommand`.

## Headless command layer

```
GPUI window / CLI
        ↓  WorkspaceCommand / EditorCommand
HeadlessWorkspace (tabs, drop routing, autosave clock, export)
        ↓  EditorCommand / RichCommand
HeadlessEditor / RichEngine (Document + caret/selection)
        ↓
source: compute_visibility / build_display_layout
WYSIWYG: RichEditorView over RichTree
export HTML (comrak, same extension set as import)
```

- `EditorCommand`: insert, backspace, delete, move/select caret, undo/redo, jump, wrap.
- `RichCommand`: WYSIWYG typing, marks, lists, tables, frontmatter fields — compiled to byte splices on the rope.
- `WorkspaceCommand`: save/open/export, drop files, theme, tabs, heading jump, `AdvanceTime` (fake clock), external-change reload, `SaveWithReview`.
- Drop classification stays pure in `drop.rs`.
- File-watcher policy is `classify_external_change` (ignore own saves; 3-way merge dirty tabs; prompt on conflict or clean-tab disk change).

Unit tests live next to the modules. Headless e2e lives in `crates/markrust-app/tests/e2e.rs` (no window). CLI e2e lives in `crates/markrust/tests/e2e.rs` (`assert_cmd`, never empty args / `--gui`).

## Data flow

```
Keyboard/Mouse/IME/FileWatcher
        ↓
DocumentBuffer (ropey) + UndoStack
        ↓
BackgroundMarkdownParser (comrak on a worker thread)
        ↓
SyntaxNodeSpan map (revision-stamped)  — source-mode masking only
RichEngine::sync → RichTree              — WYSIWYG + commands
        ↓
Source: compute_visibility + layout + paint
WYSIWYG: virtualized block list + BlockTextElement; mixed text+image paragraphs are a wrapping flex line-box (`img(PathBuf)`, 1.5em height cap); standalone image paragraphs stay block-sized (max 720×480); local files and cached `http(s)` images decode on the background executor; remote URLs fetch off the UI thread into `cache/markrust/images` (timeout/failure → alt placeholder); safe inline/block HTML is painted (tags hidden, phrasing marks applied; script/iframe/`javascript:` dropped); HTML-block inner Markdown (`**bold**`, links, code) is a nested parse, not raw chrome; `==highlight==` / `<mark>` paint a background; `<sub>`/`<sup>` and `~sub~`/`^sup^` map to Unicode (GPUI has no per-run baseline); footnote refs paint as superscripts; footnote definition and definition-list bodies are nested rich trees (`**bold**`, links, code); `$…$` / `$$…$$` hide the dollars unless the caret intersects, and paint the TeX in italic monospace (no TeX-to-glyphs); `[[wikilink]]` / `[[target|label]]` paint as in-place links (`[[ ]]` hidden unless the caret intersects); `[TOC]` / `[[toc]]` paint a generated heading list from the outline (marker shown when the caret intersects); GitHub `> [!NOTE]` / TIP / IMPORTANT / WARNING / CAUTION alerts paint as labeled callouts (left rule + label; `[!NOTE]` chrome hidden unless the caret intersects); IME origin from the focused widget or caret leaf (`wysiwyg/ime.rs`), pushed with `invalidate_character_coordinates` after caret/widget paint
```

Parse, the folder watcher `recv`, remote image HTTP, and GPUI image decode stay off the GPUI UI thread. `Document::new` / `from_file` only *schedule* a parse; they do not fetch network images. The frame drains parse with `apply_pending_parse`. CI timeout tests (`crates/markrust-core/tests/perf_gates.rs`, `crates/markrust-editor/tests/perf_gates.rs`) fail if load+parse or source layout of a 256 KiB fixture exceeds a budget. Local numbers: `cargo bench -p markrust-core --bench parse` and `cargo bench -p markrust-editor --bench layout`.

## Document model

- **On disk:** plain UTF-8 `.md` / `.txt` — no proprietary container.
- **`DocumentProcessingMode`:** `MarkdownWysiwyg` parses with comrak; `PlainText` skips parsing.
- **`revision`:** monotonic counter on every buffer edit; parse results carry the revision they were computed for so stale updates are ignored.
- **`saved_content`:** last reconciled disk snapshot (load, save, or last merged disk bytes). Used as the 3-way merge base.

## Parser strategy

| Layer | Engine | When |
|---|---|---|
| Document structure + source spans | comrak (same options as HTML export: GFM + footnotes + description lists + math_dollars + alerts + wikilinks) | Every edit, background thread |
| WYSIWYG tree | `rich::import` (comrak → `RichTree`) | On `RichEngine::sync` |
| Fenced-code highlighting | tree-sitter rust/json/yaml/bash | Viewport paint of a code body |

tree-sitter-md is not used. Source-mode `SyntaxNodeSpan`s are extracted from the comrak AST so masking cannot disagree with the rich tree's grammar. `==highlight==` is paired in that same pass (comrak has no highlight node). `$…$` / `$$…$$` are comrak `math_dollars` nodes. `[[wikilink]]` / `[[target|label]]` are comrak `wikilinks_title_after_pipe` nodes. GitHub/Typora `:smile:` shortcodes are paired from a modest alias table (unknown `:foo:` stays text). GitHub alerts (`> [!NOTE]`, …) are comrak `alerts` nodes. `[TOC]` / `[[toc]]` are classified on import as a TOC block (not a comrak node).

## Editing surfaces

| Mode | Default | How |
|---|---|---|
| **Rich** (WYSIWYG) | yes | `RichEditorView` — bold is bold; Markdown is serialization |
| **Source** | `cmd-shift-m` | Existing delimiter-masking editor; spans from comrak |
| **Split** | cycle `cmd-shift-m` again | Source left, Rich right; shared `Document` |

Caret/selection are source byte offsets. `RichEngine` provides delimiter-skipping snap/step for WYSIWYG, including a clickable blank on a standard `\n\n` between top-level blocks and on a trailing blank after the last block (`hello\n\n`). Quote and list-indent prefixes on nested fences, HTML, quoted paragraphs, and list items are skipped the same way (click, IME, and Left/Right stay on the painted body, not on `>` / `- ` / `1. `). Empty `> ` / `- ` lines sit after the prefix so typing is `> x` / `- x`. Clicking leftover viewport below the last painted line places the caret on that trailing blank, or opens one when the file has none (`hello` then type `x` is two paragraphs). Click on the last line of the last block still sits in that paragraph. A document that is only newlines still hosts a caret. A lone terminator `\n` is not an empty paragraph.

## Save and external edits

- Default save writes the buffer verbatim (untouched blocks are untouched bytes).
- When house-style Normalize would change the file, Save offers Keep original / Normalize / Cancel with a hunk preview.
- Autosave writes the buffer as-is (no Normalize).
- External disk changes: if the buffer still matches the last snapshot, prompt to reload; if the tab is dirty, 3-way line-merge disjoint edits (carets mapped with `map_offset_across_change`); overlapping edits prompt before discarding.

## Related

- [WYSIWYG roadmap](roadmap.md) — phase status and handoff
- [Delimiter masking](delimiter-masking.md) — source-mode visibility algorithm
