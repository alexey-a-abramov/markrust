# Delimiter masking and stable hints

## Projection policy

| Surface | Syntax visibility | Geometry |
|---|---|---|
| WYSIWYG | Delimiters stay hidden; optional context tint, floating badge and status-bar hint | Stable across caret, selection and hint changes |
| Source | Every Markdown byte remains visible | Uniform monospace font and row metrics; colors only |
| Split source | Every Markdown byte remains visible | Literal one-to-one byte projection |

**View → Show Markup Hints** affects only WYSIWYG context tint, its floating
syntax badge and status-bar label. It does not insert Markdown into the text flow, change
source bytes or reset scrolling. **View → Highlight Colors** selects Native,
Ocean or Forest colors in both editing surfaces without changing fonts or
line metrics. Use Source or Split to inspect and edit actual markup.

## Internal masking API

The masking code remains available for internal projections and its regression
tests. Source and Split do not use it: both show complete literal Markdown.

Syntax spans come from the same comrak AST as rich import; there is no second
Markdown grammar. A delimiter is visible if a caret lies inclusively inside
its syntax span, or a nonempty selection overlaps that span. Otherwise its
paint is transparent while its layout width is retained.

```rust
pub fn compute_visibility(
    carets: &[Caret],
    selections: &[Selection],
    spans: &[SyntaxNodeSpan],
) -> Vec<VisibilityState>;
```

One visibility state is returned per delimiter in document order.
Both user-facing source surfaces bypass this masking rule.

## Rich caret ownership

WYSIWYG omits syntax glyphs from the stable display projection. Native
shaping maps visible text back to source offsets; a separate logical caret
range covers hidden delimiters, empty list items and blank EOF paragraphs.
Formatting commands still serialize Markdown and preserve untouched bytes.
Widget drafts retain their own focus and caret.

For `**bold** plain`, moving into bold changes the context hint to
`Bold · **…**`, not the position of `bold` or `plain`. Turning hints off
removes tint, badge and label. Source and Split always display both `**` pairs.

## Regression contracts

- Hint toggles and caret moves preserve shaped text, source maps and row bounds.
- Selection and palette changes cannot change line metrics.
- Empty bullet, ordered and task continuations paint a legal insertion caret.
- Enter on an empty item exits the list; following typing creates body text.
- Deep editing preserves the viewport anchor rather than jumping to block zero.

See [GUI testing](gui-testing.md) for native action traces and fault oracles.

## Related

- [Architecture](architecture.md) — document, projection and recovery boundaries
- [WYSIWYG engineering notes](roadmap.md) — syntax edge-case history
