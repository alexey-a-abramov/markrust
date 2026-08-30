# Delimiter masking (source mode)

Source mode — not the default WYSIWYG surface — hides Markdown delimiter tokens (`**`, `` ` ``, `#`, link brackets, etc.) until the user focuses or selects inside the syntax node. The default **Rich** mode edits the `RichTree` and never paints source delimiters.

Syntax spans for masking are derived from the **same comrak AST** as `RichTree` import (`extract_syntax_spans`). There is no tree-sitter-md grammar on this path. Typora `==highlight==` is not a comrak node; the span extractor pairs it with the same rules as rich import so source masking matches WYSIWYG.

## Visibility rule

## Visibility rule

For caret set **C** and syntax span **s** = `[s.start, s.end]` (inclusive byte offsets):

A delimiter in **s** is **Visible** when:

1. ∃ c ∈ **C** such that `s.start ≤ c ≤ s.end`, **or**
2. Any selection range `[sel.start, sel.end)` overlaps **s** (`sel.start < s.end && sel.end > s.start`)

Otherwise the delimiter is **Masked** — zero glyph advance and alpha 0 in the paint pass (typographic styles apply to content bytes only).

## API (`markrust-editor`)

```rust
pub fn compute_visibility(
    carets: &[Caret],
    selections: &[Selection],
    spans: &[SyntaxNodeSpan],
) -> Vec<VisibilityState>;
```

One `VisibilityState` is returned per delimiter in document order (flattened across spans).

## Reflow mitigation (Phase 2 GUI)

- Pre-calculate line height from **max** font metrics (regular / bold / italic / code) per line.
- Toggling mask state must not change line count or vertical layout.

## Edge cases covered by tests

| Scenario | Expected |
|---|---|
| Caret outside all spans | All delimiters masked |
| Caret at span boundary (`start` or `end`) | Visible |
| Selection overlaps span without caret inside | Visible |
| Selection adjacent but non-overlapping | Masked |
| Multiple carets, any inside span | Visible |
| Multiple spans | Per-span independent visibility |
| Empty selection | Does not reveal by itself |

## Example

Source: `**bold** plain`

- Caret at offset 0 (inside bold span) → both `**` pairs visible.
- Caret at offset 10 (in "plain") → bold delimiters masked.

## Related

- [Architecture](architecture.md) — rope / RichTree / source projection
- [WYSIWYG roadmap](roadmap.md) — phase status

