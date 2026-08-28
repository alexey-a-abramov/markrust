# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Monorepo bootstrap with MPL-2.0 licensing and CI workflows.
- `markrust-core`: rope-backed `DocumentBuffer`, undo/redo, line index, tree-sitter-md parser on a background thread, `SyntaxNodeSpan` extraction.
- `markrust-editor`: headless delimiter masking algorithm with unit tests.
- `markrust` CLI stub (`--version`).
- GPUI dependency pinned in `markrust-app` (shell stub for Phase 2).
