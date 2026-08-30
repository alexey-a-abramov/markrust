# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Side-by-side editor mode: `cmd-shift-m` cycles Rich → Source → Split.
- Dirty-tab 3-way merge when a file changes on disk; carets are mapped across the merge.
- Normalize-on-save dialog shows a line hunk preview.
- Chip/caption/frontmatter IME origin uses the widget bounds, not the last text-leaf caret.

### Changed

- Source-mode syntax spans are extracted from comrak (same grammar as the WYSIWYG `RichTree`). tree-sitter-md is no longer a `markrust-core` dependency. Fenced-code highlighting still uses tree-sitter for rust/json/yaml/bash.
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
