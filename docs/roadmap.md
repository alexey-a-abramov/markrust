# MarkRust WYSIWYG Roadmap — status & handoff

_Last updated: 2026-08-30. This is the working roadmap for the "real WYSIWYG editor" effort and a self-contained handoff for any agent continuing the work._

## Vision (locked decisions)

MarkRust becomes a **true WYSIWYG markdown editor**: the user edits a rendered rich document (bold is bold, no `**` visible); Markdown is the on-disk serialization format. Additionally: a **pure source** mode (the existing Typora-style delimiter-masking editor) and a **side-by-side** mode. All native Rust on **GPUI**, fast.

- v1 scope: core rich text + tables (cell editing, row/col ops) + inline images + frontmatter panel. Mermaid WYSIWYG is future (renders as inert code block; extension point kept).
- Save flow: **block-preserving by default** (untouched blocks keep exact original bytes). A diff dialog (Keep original / Normalize / Cancel) appears only when formatting beyond the user's edits would change — with the source-primary architecture that means only on an explicit Normalize action.
- Reuse: MIT/BSD sources fine with attribution (see `THIRD-PARTY-NOTICES.md`); no GPL. References cloned at `~/Projects/github_sources/{tui.editor,nimbalyst}`.

## Architecture: source-primary, tree-projected (the key decision)

**The rope buffer stays the single source of truth.** A derived `RichTree` (in `markrust-core::rich`) is authoritative for interpretation and command targeting. Every rich command compiles down to byte splices on the source, produced by re-serializing only the touched top-level block(s). Consequences:

- Block-preserving save is free: untouched blocks are untouched bytes. Default save writes the buffer verbatim.
- Undo stays byte-based (`UndoStack`/`EditOperation` keep working for both modes).
- Caret/selection stay source byte offsets; `RichEngine` provides delimiter-skipping snap/step and source↔visible mapping.
- Fidelity layering: untouched blocks byte-exact automatically; touched blocks keep delimiter fidelity via captured attrs (`*` vs `_`, list markers, ATX/setext, fence char/len, break style) and `raw` slices on inline runs; explicit Normalize ignores fidelity.
- Source-mode masking is a **projection of the same comrak AST**, not a second Markdown parser. tree-sitter-md is gone. Masking + `spans.rs` stay as the source-mode renderer; they are not the WYSIWYG primary.

## Status

| Phase | Scope | Status |
|---|---|---|
| P0 | Repo hygiene: font-kit fix, GPUI shell restored, egui workaround removed (kept in history), masking editor → `src/source/`, references cloned | ✅ done |
| P1 | `rich/tree.rs` + `rich/import.rs` (comrak, sourcepos byte-columns pinned by test, fidelity capture, Opaque totality) | ✅ done |
| P2 | `rich/serialize.rs` + `rich/escape.rs` (delimiter-stack serializer), `rich/save.rs` (SaveCandidates + diff hunks), corpus test suite | ✅ done |
| P3 | `rich/engine.rs` (NodeId stability, splices, caret snap/step, line map, outline) + **read-only WYSIWYG view** (`markrust-editor::wysiwyg`) + per-tab mode toggle (cmd-shift-m / toolbar) | ✅ done |
| P4 | **Editing core** — `RichCommand` layer, Transaction undo upgrade (typing coalescing + caret restore), WYSIWYG caret/selection/hit-testing/IME | ✅ done |
| P5 | Lists/tasks UX (Enter/Tab, checkbox clicks); input rules; code language chip; image alt/caption editing; IME caret bounds; stable-width inline delimiter masking | 🚧 in progress (IME origin is derived from the focused widget/leaf and pushed via `invalidate_character_coordinates` after caret/widget paint; composition is simulated in cargo tests; OS candidate window not proven) |
| P6 | Tables: cell editing, Tab nav, insert/delete row/col, header constraints, right-click menu | ✅ done (contextual table toolbar when the caret is in a table; right-click focuses the cell; GPUI has no OS-native pixel-accurate context menu) |
| P7 | Frontmatter in-place title/tags; Normalize review dialog on save | ✅ done (title, description, tags, and a YAML body field; hunk preview in the save prompt) |
| P8 | External-edit reconciliation (`map_offset_across_change`, `apply_external_edit` / `apply_merged_edit`, 3-way dirty-tab merge) | ✅ done (proven by `three_way_merge` + headless dirty-tab merge e2e; overlapping edits still prompt) |
| P9 | Side-by-side + polish + migration (source spans from comrak; delete tree-sitter-md; docs rewrite; criterion perf gates; optionally retire masking) | ✅ done (masking/`spans.rs` kept on purpose for source mode — the retire item was optional) |

**The Typora WYSIWYG GOAL is not complete.** P0–P4 and P6–P9 have test evidence. P5 still lacks a real-OS IME candidate-window proof (origin rectangles and an `EntityInputHandler` composition session are unit-tested, and the platform cursor is invalidated after caret/widget paint; GPUI's test window does not record `set_ime_cursor_position`, and no CJK candidate window has been observed).

### Proven invariants (enforced by tests — keep them green)

- **Preserve identity**: `serialize(import(x), Preserve) == x` byte-exact for **all 649** CommonMark spec examples + all corpus cases (`crates/markrust-core/tests/rich_roundtrip.rs`).
- **Normalize fixed point**: `n(x) == n(n(x))` in one step.
- **Meaning preservation**: `html(x) == html(n(x))` under the comrak-HTML oracle.
- 26 documented Normalize-only skips (nested same-mark emphasis — inherent to flat inline runs, Lexical shares it; comrak multi-line inline-HTML literal quirks; link-in-image-alt). Preserve identity still holds for every skip.
- Byte-exact save round-trip e2e (`task_table_fence_frontmatter_survive_save_roundtrip`) still passes.
- Source spans share kinds with the rich tree on `showcase.md` (`source_spans_share_comrak_grammar_with_rich_tree`).
- Dirty-tab disjoint external edits merge (`dirty_tab_merges_disjoint_external_edits`).
- Load+parse of a 256 KiB mixed GFM fixture stays under 2.5s (`large_document_load_and_parse_stays_under_budget`); source layout of a 64 KiB fixture under 2.5s (`large_document_layout_stays_under_budget`). `cargo bench -p markrust-core --bench parse` and `cargo bench -p markrust-editor --bench layout` for local criterion numbers.

### Key modules

- `crates/markrust-core/src/rich/` — `tree` (RichTree/Block/Inline + fidelity), `import` (comrak → tree), `serialize`/`escape` (Preserve/Normalize), `engine` (view contract), `command` (RichCommand → byte splices), `input_rules` (Typora-style `# ` / lists / fences / auto-close), `save` (SaveCandidates + hunk preview).
- `crates/markrust-core/src/parser.rs` — background comrak span extraction for source-mode masking (same grammar as import).
- `crates/markrust-core/src/offset_map.rs` — `map_offset_across_change` (nimbalyst port).
- `crates/markrust-core/src/merge.rs` — `three_way_merge` for dirty-tab disk reconciliation.
- `crates/markrust-core/src/document.rs` — `saved_content`, `apply_external_edit`, `apply_merged_edit`, `peel_typing_range`.
- `crates/markrust-core/src/html_visual.rs` — safe HTML / footnote-ref / definition-list paint projection (no script, no CSS).
- `crates/markrust-editor/src/wysiwyg/` — `view` (RichEditorView over virtualized `list()`), `blocks` (per-kind renderers), `block_text` (caret/hit-testing/IME host), `ime` (IME origin from the focused widget/leaf).
- `crates/markrust-editor/src/source/` — the masking source editor (fallback and source/split panes).
- `crates/markrust-core/src/perf_fixture.rs` — 256 KiB mixed GFM used by parse/layout budget tests and criterion benches.
- `crates/markrust-app/src/crash.rs` — panic logger (see Crash handling).

## P4 shipped

Editing core is in: `rich/command.rs` (`InsertText`, `Backspace`, `Delete`, `SplitBlock`, `InsertLineBreak`, `ToggleMark`, `ToggleLink`, `SetBlockType`, `ToggleBlockquote`, `ToggleList`, `SetTaskChecked`, `IndentList`, `OutdentList`), `UndoStack` `Transaction` with typing coalescing and caret restore, WYSIWYG caret/selection/hit-testing via `block_text.rs`, IME (`EntityInputHandler`, preedit does not touch the model). Default tab mode is WYSIWYG (`default_editor_mode_is_wysiwyg_not_split`). `cmd-shift-m` cycles Rich → Source → Split.

Proven by tests: insert in one block leaves other blocks' bytes untouched; coalesced typing undo restores string + caret; backspace deletes the visible grapheme not `**` wrappers; split paragraph/list; toggle bold; wrap link; empty-item Enter outdents/exits; indent/outdent; task checkbox splice.

## P5–P9 design notes (for the next agent)

Shipped this pass (keep previous bullets; this pass added):
- **Emoji shortcodes:** GitHub/Typora `:smile:` / `:heart:` / `:+1:` / `:rocket:` (modest built-in alias table) paint as Unicode in WYSIWYG. `:name:` is hidden unless the caret or a selection intersects; unmatched `:foo:` stays visible; source bytes stay `:name:`. Source mode substitutes the same glyphs when the caret is outside. Not a full gemoji dump.
- **Wikilinks / TOC:** `[[target]]` / `[[target|label]]` parse via comrak `wikilinks_title_after_pipe`. WYSIWYG paints them as in-place links (`[[ ]]` hidden unless the caret or a selection intersects); a piped label hides the target too. Source mode masks the same chrome. `[TOC]` / `[[toc]]` (case-insensitive, whole paragraph) paint a generated heading list from `RichTree::outline()`; the marker is shown when the caret intersects so it can be edited. Untouched source bytes stay in the file.
- **GitHub alerts:** `> [!NOTE]` / TIP / IMPORTANT / WARNING / CAUTION parse via comrak `alerts`. WYSIWYG paints a labeled callout (left rule + title); `[!NOTE]` chrome is hidden unless the caret or a selection intersects that first-line span. Untouched source bytes stay in the file. Source mode masks the same `[!NOTE]` token. Ordinary `>` quotes are unchanged.
- **Dollar math:** `$…$` / `$$…$$` parse via comrak `math_dollars` (pandoc heuristics so `$5` and `$` in code stay text). WYSIWYG hides the dollars unless the caret or a selection intersects the span, and paints the TeX in italic monospace (no TeX-to-glyphs; no browser). Source mode masks the same delimiters. Untouched `$` bytes stay in the file.
- **P9:** tree-sitter-md removed from `markrust-core`. `extract_syntax_spans` walks the comrak AST (shared `parse_options` / `LineStarts` with `rich::import`). Background parser still off the UI thread. Source mode remains masking, now grammar-aligned with WYSIWYG. Split mode in the window (source | rich). Criterion benches plus CI timeout tests gate load+parse (`extract_syntax_spans` / `import_markdown`) and source-mode `build_display_layout` on a 256 KiB fixture.
- P8: dirty tabs 3-way merge (`three_way_merge` + `Document::apply_merged_edit`); own-save watcher events ignored (`ours == theirs`); conflict still prompts. Autosave no longer spuriously prompts reload after it writes.
- P7: Normalize save prompt includes a hunk preview (`SaveCandidates::hunk_preview`). Frontmatter panel edits title, description, tags, and the inner YAML body.
- Chip/caption/frontmatter IME: `WidgetImeSink` reports widget bounds and takes `handle_input` while focused so body leaves cannot steal the IME handler. Origin is the widget overlay's trailing caret, not the last painted text leaf.
- IME origin (`wysiwyg/ime.rs`): `bounds_for_range` uses `ImeOriginState` — focused widget, else the leaf whose source range contains the document caret (wrapped-line caret rect, table cell, body). After that origin is painted, `Window::invalidate_character_coordinates` pushes it to the platform (GPUI equivalent of `set_ime_cursor_position`) so the OS does not keep a stale candidate window. Unit tests drive `EntityInputHandler` composition (`replace_and_mark_text_in_range` / `selected_text_range` / `bounds_for_range` / `replace_text_in_range`) and assert that during preedit the origin tracks the caret after insert, after `TableTab` into a cell, and after focusing a caption / language chip / frontmatter field, including `take_platform_push` (GPUI's test platform does not record `update_ime_position`). **Composition simulation is not an OS candidate window.**
- Input-rule polish: `_`/`__` italic/bold, nested italic inside bold, list-item-local `# ` / fences, `Document::peel_typing_range` so heading conversion is one undo even when the prefix was a slice of a longer Typing tx. `[` / `]` / `<` type as raw (task lists, links, HTML); fences/thematic breaks do not insert newlines inside table cells.
- P6: delete row/col (keep ≥1 of each); first row stays header; insert caret lands in the new cell; contextual table toolbar when the caret is in a table; right-click focuses the cell (toolbar follows).
- **Inline images as pixels:** local Markdown images (`PNG`/`JPEG`/`GIF`/`WebP`, plus anything else GPUI can decode) resolve relative to the document directory and load through `img(PathBuf)` so GPUI treats them as filesystem `Resource::Path`. Decode stays on the background executor. Mixed paragraphs (`hello ![x](a.png) world`) are a wrapping flex row of hug-width text + 1.5em-tall images — GPUI cannot mix `Image` and glyphs in one shaped `TextRun`. Standalone image paragraphs stay block-sized (natural size, max 720×480) with a caption. Click an inline image (or a block caption) to edit alt. A `String` source was a bundled-asset lookup, so the previous `img(path_string)` never showed file pixels.
- **Safe HTML paint:** inline tags (`<b>`, `<i>`, `<code>`, `<kbd>`, `<mark>`, `<br>`, `<a>`, `<sub>`, `<sup>`, …) are hidden chrome; inner text keeps Markdown marks plus HTML phrasing. HTML blocks strip tags and parse inner Markdown (`**bold**`, links, code) so it is not source chrome (or a rule / `<img>`). `script` / `iframe` / `javascript:` URLs are dropped, not executed. Caret skips tag bytes like `**` delimiters.
- **Highlight / sub / sup / math:** Typora `==highlight==` (comrak has no node; applied on import) and HTML `<mark>` paint a background. Comrak `^sup^` / `~sub~` and HTML `<sub>`/`<sup>` map through Unicode super/subscripts. GPUI `TextRun` has no per-run font size or baseline offset, so unmapped letters stay the same size. Source mode masks `==` / `~` / `^` / `$` / `$$` with the same caret/selection visibility rule as `**`. `$…$` / `$$…$$` paint as italic monospace TeX (no TeX-to-glyphs crate); currency like `$5` and `$` in code stay text. Source `$` is preserved on disk.
- **Footnotes / definition lists:** comrak extensions on (Typora extras, not GFM). `[^1]` paints as a superscript; `[^1]:` defs and `term` / `: details` are structured containers. Nested Markdown in those bodies (`**bold**`, links, code) is a rich tree and paints as WYSIWYG.
- Thematic breaks paint as a full-width rule (not an 8%-opacity hairline). Autolinks keep link color + underline. Strikethrough inherits the run color. Hard breaks (`  ` / `\`) are a visible newline in the leaf layout.
- **Remote `http(s)` images:** WYSIWYG maps the URL to a cache path under the user cache dir (`markrust/images`, SHA-256 of the URL). Fetch runs on GPUI's background executor via reqwest (timeout + size cap); success writes the file and the existing `PathBuf` pipeline paints pixels. Timeouts, HTTP errors, and non-image bodies leave the cache empty so the alt placeholder stays. File-open / `Document::from_file` does not wait on the network. `data:` URLs are unchanged.

Previously shipped:
- Input rules as pure functions in `rich/input_rules.rs` (`# `, `- `/`* `/`+ `, `1. `/`1) `, `> `, fences, `---`/`***`/`___`, auto-close `*`/`**`/`_`/`__`/` `/`~~`), disabled in code/raw, heading+space is one undo group.
- `SetCodeInfo` / `SetImageAlt` / `SetFrontmatter` commands. WYSIWYG: clickable language chip and image caption (click → type → Enter commits).
- Fenced code bodies are editable `BlockTextElement`s (not inert `StyledText`).
- IME: `bounds_for_range` / `character_index_for_point` go through `ImeOriginState` (superseded by the origin resolver above).
- Source-mode inline delimiters keep glyph width when masked (transparent paint) so unmasking does not wrap the line. Block markers (`#`, list bullets) still collapse to visual chrome.
- P6: `TableTab` (Tab/Shift-Tab in a table), `InsertTableRow` / `InsertTableColumn` (Tab on the last cell inserts a row). Cells were already `BlockTextElement`s.
- File-open hang fix remains: watcher `recv` on a background task; parse via `apply_pending_parse` off the frame.

Still open (do not shrink the goal):
- IME candidate window: origin rectangles are unit-tested for body caret, wrapped lines, table cells, language chip, image caption, and frontmatter fields, including after an edit then a caret move into a table cell or caption. A simulated IME session (`EntityInputHandler` preedit + GPUI `compute_ime_candidate_bounds`) asserts the same origin during composition, and `take_platform_push` fires with that rect (`invalidate_character_coordinates` is what the view calls; the test window does not record `set_ime_cursor_position`). A human must still enable a CJK IME in the GUI (Hiragana / Pinyin / etc.) and confirm the OS candidate window follows the caret in each of those surfaces. Russian/Colemak layouts on this Mac are not a candidate-window proof. **This is the remaining P5 gap; the GOAL is not complete.**
- Mixed paragraphs are a flex line-box, not CSS `display:inline` wrap-around inside one shaped line — a long text run still wraps as a unit rather than flowing around the image mid-line. Missing local files fall back to an alt placeholder.
- Mermaid fences stay inert code blocks. Headless Rust SVG crates exist (`merman`, `mermaid-rs-renderer`) but they pull a large layout stack (ELK/cytoscape or ~70 extra crates) and GPUI has no SVG element — not a small in-process path, and not bundled.
- HTML `<svg>` / `style=` / CSS classes are not painted. GPUI `TextRun` has no per-run baseline or font size, so `<sub>`/`<sup>` / `~` / `^` use a Unicode map (unmapped letters stay body size). Dollar math paints TeX source in italic monospace; there is no small in-process TeX-to-glyphs path (no browser).
- Masking/`parser.rs`/`spans.rs` are not deleted (source mode still uses them; they project comrak, not tree-sitter-md). Fenced-code highlighting still uses tree-sitter rust/json/yaml/bash — that stays.

Earlier P5 (still in):
- Checkbox clicks → `SetTaskChecked`; Tab/Shift-Tab → `IndentList`/`OutdentList` outside tables; empty-item Enter outdents or exits.
- Cmd/Ctrl+B/I/E/K wrap in both modes.
- Source click-to-caret uses glyph x; source tables padded when blurred.

## Dev workflow — hard-won gotchas

- **Build env**: `export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer` — `xcode-select` points at CommandLineTools, which lacks the Metal shader compiler (`xcrun: unable to find utility "metal"`).
- **Always `cargo build --workspace`** before running `target/debug/markrust`: building `-p markrust-app` rebuilds only the lib — the `markrust` bin package stays stale (this burned a debugging hour).
- **gpui font-kit**: `gpui_platform` must keep `features = ["font-kit"]` or macOS gets a silent `NoopTextSystem` and renders no text at all.
- **gpui run invariants**: `TextRun`s passed to `shape_line`/`StyledText::with_runs` must exactly tile the text on char boundaries; tree-sitter *code* highlight spans nest/overlap and must be sanitized first (see `wysiwyg/blocks.rs::code_runs` and `source/element.rs::build_runs_for_line`).
- **Visual verification**: run the app, `screencapture -x out.png` (needs sandbox disabled → permission prompt), read the PNG. Launching the app steals focus — batch checks and kill instances promptly; every panic-abort also spawns a macOS crash dialog for the user.
- **Panic analysis**: panics append structured reports (message, location, full backtrace) to `~/Library/Logs/MarkRust/panics.log` via `markrust_app::crash::install_panic_logger()`. Check that file first when the app dies; it is written before the crash dialog appears.
- `cargo test/build` piped to `grep`/`tail` masks exit codes — check `pipestatus` or run unpiped.

## Historical note

The pre-WYSIWYG roadmap lives in `ROADMAP.md` (repo root). The delimiter-masking design (`docs/delimiter-masking.md`) describes **source mode**. `docs/architecture.md` matches the comrak / RichTree / source-projection pipeline.
