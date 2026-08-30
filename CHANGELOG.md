# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- WYSIWYG paints GitHub/Typora emoji shortcodes (`:smile:`, `:heart:`, `:+1:`, `:rocket:`, …) as Unicode (a modest built-in alias table). `:name:` is hidden unless the caret intersects; unmatched `:foo:` stays visible; on-disk bytes stay `:name:`. Source mode substitutes the same glyphs when the caret is outside.
- WYSIWYG `[[wikilink]]` / `[[target|label]]` paint as in-place links (`[[ ]]` hidden unless the caret intersects). `[TOC]` / `[[toc]]` paint a generated heading list from the document outline (marker shown when the caret intersects). Source bytes stay in the file. Source mode masks wiki brackets with the same caret/selection rule as `**`.
- Parse/layout perf gates: 256 KiB mixed GFM load+parse timeout tests, 64 KiB source-layout timeout tests, plus criterion benches (`cargo bench -p markrust-core --bench parse`, `cargo bench -p markrust-editor --bench layout`).
- Contextual table toolbar when the caret is in a table (row/col insert and delete). Right-click still focuses the cell.
- Frontmatter panel edits title, description, tags, and the inner YAML body.
- Input rules: `[` / `]` / `<` type unescaped so task lists, links, and HTML can be typed; fence/thematic-break rules do not insert newlines inside table cells.
- Side-by-side editor mode: `cmd-shift-m` cycles Rich → Source → Split.
- Dirty-tab 3-way merge when a file changes on disk; carets are mapped across the merge.
- Normalize-on-save dialog shows a line hunk preview.
- Chip/caption/frontmatter IME origin uses the widget bounds, not the last text-leaf caret. Body/wrapped/table IME origin is the focused leaf's caret rect (not last-painted). Caret moves and widget focus push that origin via GPUI `invalidate_character_coordinates`. The OS IME candidate window is still unverified.
- WYSIWYG local images render as pixels (filesystem path, background decode) with alt/caption still editable. Mixed text+image paragraphs lay out as a wrapping horizontal line-box (inline images capped at 1.5em); a standalone image paragraph stays block-sized. Remote `http(s)` images fetch off the UI thread into a URL-keyed cache, then use the same `PathBuf` pipeline; timeouts and failures keep the alt placeholder. Thematic breaks, autolink paint, strikethrough, and hard-break newlines match the rendered document instead of source chrome.
- WYSIWYG paints safe inline and block HTML instead of raw tags (phrasing marks, `<br>`, comments hidden, script/iframe/`javascript:` dropped). HTML-block inner Markdown (`**bold**`, links, code) paints as rich text. `==highlight==` / `<mark>` paint a background; `<sub>`/`<sup>` and `~sub~`/`^sup^` use Unicode (GPUI has no per-run baseline). Footnote references paint as superscripts; footnote definitions and definition lists paint as structured blocks whose bodies are nested rich trees (`**bold**`, links, code). `$…$` / `$$…$$` hide the dollars unless the caret intersects and paint the TeX in italic monospace (currency like `$5` and `$` in code stay text). GitHub `> [!NOTE]` / TIP / IMPORTANT / WARNING / CAUTION alerts paint as labeled callouts (left rule + label; `[!NOTE]` hidden unless the caret intersects). Parse still uses the background worker; the folder watcher still `recv`s off the UI thread. Mermaid fences stay code blocks (no small in-process SVG renderer without a large layout crate).

### Changed

- Source-mode syntax spans are extracted from comrak (same grammar as the WYSIWYG `RichTree`). tree-sitter-md is no longer a `markrust-core` dependency. Fenced-code highlighting still uses tree-sitter for rust/json/yaml/bash. Source mode masks `==highlight==` (and `~sub~` / `^sup^` / `$` / `$$` math / `[[wikilink]]` / known `:emoji:` shortcodes) with the same caret/selection rule as `**`.
- Architecture docs rewritten for the rope + RichTree + source-projection pipeline.

### Added (earlier this cycle)

- WYSIWYG editing core: typing, backspace, bold/italic/code/link wrap, list split/indent/outdent, task-checkbox toggle.
- Typora-style input rules (`# `, lists, quotes, fences, `---`, auto-close `*`/`**`/`_`/`__`/` `/`~~`), including inside list items.
- Editable fenced-code language chip and image alt/caption in WYSIWYG (IME, click-away, empty-alt placeholder).
- Table Tab navigation, insert/delete row and column, header-row constraints, and a right-click table menu.
- Frontmatter panel with in-place title/tags editing in WYSIWYG and a Keep original / Normalize / Cancel save dialog.
- External-edit caret mapping (`map_offset_across_change`) when reloading a file changed on disk.
- Source-mode Cmd/Ctrl+B/I/E/K wrap that keeps delimiter masking working (caret stays inside the span).
- Precise source-mode click-to-caret (byte offset from x position, not line-only).
- Source-mode tables render padded columns until focused, then show raw pipes.
- Monorepo bootstrap with MPL-2.0 licensing and CI workflows.
- `markrust-core`: rope-backed `DocumentBuffer`, undo/redo, line index, background Markdown parse, `SyntaxNodeSpan` extraction.
- `markrust-editor`: headless delimiter masking algorithm with unit tests.
- `markrust` CLI stub (`--version`).
- GPUI dependency pinned in `markrust-app` (shell stub for Phase 2).

### Fixed

- Opening a Markdown file no longer freezes the window: the folder watcher no longer blocks GPUI's UI thread, and Markdown parse stays on the background worker.
- Empty list-item Enter outdents or exits the list instead of inserting a blank paragraph in place.
- Source-mode line height for a heading vs body line is derived from syntax spans, not mask visibility.
