# MarkRust architecture

MarkRust uses a decoupled reactive pipeline: input mutates a rope-backed buffer, a background parser produces syntax spans, and the editor projects styled glyphs with delimiter masking.

## Crate boundaries

| Crate | Responsibility | GUI deps |
|---|---|---|
| `markrust-core` | Buffer, undo, line index, tree-sitter spans, revision tokens | None |
| `markrust-editor` | `HeadlessEditor` + masking/layout; GPUI `MarkdownEditor` adapter | GPUI (view only) |
| `markrust-app` | `HeadlessWorkspace` session + GPUI window chrome | GPUI (Zed git pin) |
| `markrust` | CLI (`parse_args` / `run`) and desktop binary | GUI only for `--gui` |

**Rule:** `HeadlessEditor` (`markrust-editor::headless`) and `HeadlessWorkspace` (`markrust-app::session`) must not use GPUI types. The GPUI window maps keys/clicks to `WorkspaceCommand` / `EditorCommand`.

## Headless command layer

```
GPUI window / CLI
        ↓  WorkspaceCommand / EditorCommand
HeadlessWorkspace (tabs, drop routing, autosave clock, export)
        ↓  EditorCommand
HeadlessEditor (Document + caret/selection)
        ↓
compute_visibility / build_display_layout / export HTML
```

- `EditorCommand`: insert, backspace, delete, move/select caret, undo/redo, jump.
- `WorkspaceCommand`: save/open/export, drop files, theme, tabs, heading jump, `AdvanceTime` (fake clock), external-change reload.
- Drop classification stays pure in `drop.rs`.
- File-watcher reload policy is `reload_decision` (skip dirty tabs).

Unit tests live next to the modules. Headless e2e lives in `crates/markrust-app/tests/e2e.rs` (no window). CLI e2e lives in `crates/markrust/tests/e2e.rs` (`assert_cmd`, never empty args / `--gui`).


## Data flow

```
Keyboard/Mouse/IME/FileWatcher
        ↓
DocumentBuffer (ropey) + UndoStack
        ↓
BackgroundMarkdownParser (tree-sitter-md thread)
        ↓
SyntaxNodeSpan map (revision-stamped)
        ↓
compute_visibility (carets, selections, spans)
        ↓
Layout + Paint (Phase 2 GPUI)
```

## Document model

- **On disk:** plain UTF-8 `.md` / `.txt` — no proprietary container.
- **`DocumentProcessingMode`:** `MarkdownWysiwyg` runs tree-sitter; `PlainText` skips parsing.
- **`revision`:** monotonic counter on every buffer edit; parse results carry the revision they were computed for so stale updates are ignored.

## Parser strategy

| Layer | Engine | When |
|---|---|---|
| Hot path | tree-sitter-md | Every edit (background thread) |
| Export (later) | comrak | On demand for HTML |

## Future trait boundaries (stubs in later phases)

- `TextLayoutEngine` — measure, line break, glyph runs
- `SyntaxHighlighter` — viewport-local highlight spans
- `FileSystemAdapter` — read, write, watch with workspace sandbox

See [delimiter-masking.md](delimiter-masking.md) for the WYSIWYG visibility algorithm.
