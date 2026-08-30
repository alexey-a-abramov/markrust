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

## Status

| Phase | Scope | Status |
|---|---|---|
| P0 | Repo hygiene: font-kit fix, GPUI shell restored, egui workaround removed (kept in history), masking editor → `src/source/`, references cloned | ✅ done |
| P1 | `rich/tree.rs` + `rich/import.rs` (comrak, sourcepos byte-columns pinned by test, fidelity capture, Opaque totality) | ✅ done |
| P2 | `rich/serialize.rs` + `rich/escape.rs` (delimiter-stack serializer), `rich/save.rs` (SaveCandidates + diff hunks), corpus test suite | ✅ done |
| P3 | `rich/engine.rs` (NodeId stability, splices, caret snap/step, line map, outline) + **read-only WYSIWYG view** (`markrust-editor::wysiwyg`) + per-tab mode toggle (cmd-shift-m / toolbar) | ✅ done |
| P4 | **Editing core** — `RichCommand` layer, Transaction undo upgrade (typing coalescing + caret restore), WYSIWYG caret/selection/hit-testing/IME | ✅ done |
| P5 | Lists/tasks UX (Enter/Tab, checkbox clicks); input rules; code language chip; image alt/caption editing; IME caret bounds; stable-width inline delimiter masking | 🚧 in progress (core behaviors in; polish remains) |
| P6 | Tables: cell editing, Tab nav, insert row/col commands (no context-menu chrome yet) | 🚧 in progress |
| P7 | Frontmatter panel hook + Normalize review decision fn (dialog UI still open) | 🚧 in progress |
| P8 | External-edit reconciliation (`map_offset_across_change` port, `apply_external_edit`, watcher/autosave wiring) | ⬜ |
| P9 | Side-by-side + polish + migration (delete tree-sitter-md/`parser.rs`/`spans.rs`/masking after source mode re-derives segments from `RichTree`; criterion perf gates; docs rewrite) | ⬜ |

### Proven invariants (enforced by tests — keep them green)

- **Preserve identity**: `serialize(import(x), Preserve) == x` byte-exact for **all 649** CommonMark spec examples + all corpus cases (`crates/markrust-core/tests/rich_roundtrip.rs`).
- **Normalize fixed point**: `n(x) == n(n(x))` in one step.
- **Meaning preservation**: `html(x) == html(n(x))` under the comrak-HTML oracle.
- 26 documented Normalize-only skips (nested same-mark emphasis — inherent to flat inline runs, Lexical shares it; comrak multi-line inline-HTML literal quirks; link-in-image-alt). Preserve identity still holds for every skip.
- Byte-exact save round-trip e2e (`task_table_fence_frontmatter_survive_save_roundtrip`) still passes.

### Key modules

- `crates/markrust-core/src/rich/` — `tree` (RichTree/Block/Inline + fidelity), `import` (comrak → tree), `serialize`/`escape` (Preserve/Normalize), `engine` (view contract), `command` (RichCommand → byte splices), `input_rules` (Typora-style `# ` / lists / fences / auto-close), `save` (SaveCandidates).
- `crates/markrust-editor/src/wysiwyg/` — `view` (RichEditorView over virtualized `list()`), `blocks` (per-kind renderers), `block_text` (caret/hit-testing/IME host).
- `crates/markrust-editor/src/source/` — the masking source editor (kept as always-working fallback and future source mode).
- `crates/markrust-app/src/crash.rs` — panic logger (see Crash handling).

## P4 shipped

Editing core is in: `rich/command.rs` (`InsertText`, `Backspace`, `Delete`, `SplitBlock`, `InsertLineBreak`, `ToggleMark`, `ToggleLink`, `SetBlockType`, `ToggleBlockquote`, `ToggleList`, `SetTaskChecked`, `IndentList`, `OutdentList`), `UndoStack` `Transaction` with typing coalescing and caret restore, WYSIWYG caret/selection/hit-testing via `block_text.rs`, IME (`EntityInputHandler`, preedit does not touch the model). Default tab mode is WYSIWYG. Source mode remains the fallback (`cmd-shift-m`).

Proven by tests: insert in one block leaves other blocks' bytes untouched; coalesced typing undo restores string + caret; backspace deletes the visible grapheme not `**` wrappers; split paragraph/list; toggle bold; wrap link; empty-item Enter outdents/exits; indent/outdent; task checkbox splice.

## P5–P7 design notes (for the next agent)

Shipped this pass:
- Input rules as pure functions in `rich/input_rules.rs` (`# `, `- `/`* `/`+ `, `1. `/`1) `, `> `, fences, `---`/`***`/`___`, auto-close `*`/`**`/` `/`~~`), disabled in code/raw, heading+space is one undo group.
- `SetCodeInfo` / `SetImageAlt` / `SetFrontmatter` commands. WYSIWYG: clickable language chip and image caption (click → type → Enter commits).
- Fenced code bodies are editable `BlockTextElement`s (not inert `StyledText`).
- IME: `bounds_for_range` uses the painted caret rect; `character_index_for_point` hit-tests the focused leaf.
- Source-mode inline delimiters keep glyph width when masked (transparent paint) so unmasking does not wrap the line. Block markers (`#`, list bullets) still collapse to visual chrome.
- P6: `TableTab` (Tab/Shift-Tab in a table), `InsertTableRow` / `InsertTableColumn` (Tab on the last cell inserts a row). Cells were already `BlockTextElement`s.
- P7: frontmatter panel (title + “edit in source” hook → `WorkspaceCommand::EditFrontmatter`). `normalize_review_decision` / `should_offer_normalize_review` in `session.rs` (headless-tested). No dialog chrome yet.
- File-open hang fix remains: watcher `recv` on a background task; parse via `apply_pending_parse` off the frame.

Still open (do not shrink the goal):
- Input-rule polish: `_` italic, nested auto-close, list-item-local `# ` / fences, undo grouping when the prefix was not the last Typing tx.
- Language chip / caption: IME composition inside the chip, click-away vs Enter, empty-alt placeholder copy.
- IME: preedit underline in the leaf; `character_index_for_point` for wrapped lines other than the last focused leaf.
- P6: delete row/col, header-row constraints, context-menu / toolbar for insert; caret after insert-row is approximate.
- P7: Normalize review dialog UI (Keep original / Normalize / Cancel) wired to save; in-place YAML key editing (title/tags) instead of source-mode hook.
- P8–P9 unchanged.

Earlier P5 (still in):
- Checkbox clicks → `SetTaskChecked`; Tab/Shift-Tab → `IndentList`/`OutdentList` outside tables; empty-item Enter outdents or exits.
- Cmd/Ctrl+B/I/E/K wrap in both modes.
- Source click-to-caret uses glyph x; source tables padded when blurred.

## Dev workflow — hard-won gotchas

- **Build env**: `export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer` — `xcode-select` points at CommandLineTools, which lacks the Metal shader compiler (`xcrun: unable to find utility "metal"`).
- **Always `cargo build --workspace`** before running `target/debug/markrust`: building `-p markrust-app` rebuilds only the lib — the `markrust` bin package stays stale (this burned a debugging hour).
- **gpui font-kit**: `gpui_platform` must keep `features = ["font-kit"]` or macOS gets a silent `NoopTextSystem` and renders no text at all.
- **gpui run invariants**: `TextRun`s passed to `shape_line`/`StyledText::with_runs` must exactly tile the text on char boundaries; tree-sitter highlight spans nest/overlap and must be sanitized first (see `wysiwyg/blocks.rs::code_runs` and `source/element.rs::build_runs_for_line`).
- **Visual verification**: run the app, `screencapture -x out.png` (needs sandbox disabled → permission prompt), read the PNG. Launching the app steals focus — batch checks and kill instances promptly; every panic-abort also spawns a macOS crash dialog for the user.
- **Panic analysis**: panics append structured reports (message, location, full backtrace) to `~/Library/Logs/MarkRust/panics.log` via `markrust_app::crash::install_panic_logger()`. Check that file first when the app dies; it is written before the crash dialog appears.
- `cargo test/build` piped to `grep`/`tail` masks exit codes — check `pipestatus` or run unpiped.

## Historical note

The pre-WYSIWYG roadmap lives in `ROADMAP.md` (repo root). The delimiter-masking design (`docs/delimiter-masking.md`) describes the source mode; `docs/architecture.md` needs its parser/data-flow sections rewritten in P9 (tree-sitter-md removal).
