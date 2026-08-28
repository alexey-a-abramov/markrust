# MarkRust architecture

MarkRust uses a decoupled reactive pipeline: input mutates a rope-backed buffer, a background parser produces syntax spans, and the editor projects styled glyphs with delimiter masking.

## Crate boundaries

| Crate | Responsibility | GUI deps |
|---|---|---|
| `markrust-core` | Buffer, undo, line index, tree-sitter spans, revision tokens | None |
| `markrust-editor` | Delimiter masking, future layout + GPU paint hooks | None |
| `markrust-app` | GPUI window, file tree, palette, workspace chrome | GPUI (Zed git pin) |
| `markrust` | CLI and desktop binary entry point | Optional |

**Rule:** `markrust-core` and `markrust-editor` must remain free of GPUI so the engine is embeddable and testable headlessly.

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
