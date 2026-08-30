# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- WYSIWYG editing core: typing, backspace, bold/italic/code/link wrap, list split/indent/outdent, task-checkbox toggle.
- Source-mode Cmd/Ctrl+B/I/E/K wrap that keeps delimiter masking working (caret stays inside the span).
- Precise source-mode click-to-caret (byte offset from x position, not line-only).
- Source-mode tables render padded columns until focused, then show raw pipes.
- Monorepo bootstrap with MPL-2.0 licensing and CI workflows.
- `markrust-core`: rope-backed `DocumentBuffer`, undo/redo, line index, tree-sitter-md parser on a background thread, `SyntaxNodeSpan` extraction.
- `markrust-editor`: headless delimiter masking algorithm with unit tests.
- `markrust` CLI stub (`--version`).
- GPUI dependency pinned in `markrust-app` (shell stub for Phase 2).

### Fixed

- Opening a Markdown file no longer freezes the window: the folder watcher no longer blocks GPUI's UI thread, and tree-sitter parse stays on the background worker.
- Empty list-item Enter outdents or exits the list instead of inserting a blank paragraph in place.
- Source-mode line height for a heading vs body line is derived from syntax spans, not mask visibility.
