# MarkRust Roadmap

The live WYSIWYG tracker is [`docs/roadmap.md`](docs/roadmap.md). This file is the original v0.1 phase checklist, updated to match the current tree.

## v0.1.0 — Public alpha

- [x] Phase 0: Bootstrap (workspace, CI, core buffer)
- [x] Phase 1: Core engine (comrak spans + `RichTree`, background parse; tree-sitter-md retired)
- [x] Phase 2 (headless): Delimiter masking algorithm + tests
- [x] Phase 2 (GUI): GPUI editor surface, caret, selection, themes (default tab is WYSIWYG)
- [x] Phase 3: Workspace shell (folder, file tree, outline, palette)
- [x] Phase 4: GFM completeness (tables, task lists, export)
- [ ] Phase 5: Distribution (release binaries, Homebrew, crates.io)

Remaining for the Typora/WYSIWYG product goal (see `docs/roadmap.md` P5): the OS IME candidate window is not proven on a real CJK session.

## Explicitly deferred

- Split/source as the **primary** UX (they exist as optional `cmd-shift-m` modes; default is WYSIWYG)
- Git UI integration
- MCP / AI agent server
- Cloud sync and accounts
- Plugin marketplace
- Math/LaTeX rendering
- Windows port (target v0.2)

See the full product plan in the repository docs for rationale and milestones.
