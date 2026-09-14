# Delimiter masking

Source mode hides Markdown delimiter tokens (`**`, `` ` ``, `#`, link brackets, etc.) until the user focuses or selects inside the syntax node. **WYSIWYG** uses the same caret/selection intersect rule for GFM wrap marks (`*` / `**` / `_` / `~~` / ticks / `==`, including wrap marks around a whole `[hello](url)`, `![alt](url)`, HTML phrasing `<b>` / `<a href>`, and HTML `<img>`), ATX `#`, list/task markers, quote `>`, fence ticks, table `|` (unescaped cell boundaries, including compact GFM `foo|bar` with no wrapping pipes; escaped `\|` in a cell is a literal pipe), HTML phrasing tags (`<b>` / `<a href>` / comments), HTML-block Type-1 `<style>` / `<textarea>` (source when intersected; `<script>` stays a hidden widget), GFM tagfilter `<iframe>` (hidden widget) / `<title>` / `<xmp>` (source when intersected, skip as widgets), Type-6 `<details>` / `<dialog>` / `<form>` / `<fieldset>` / `<legend>` and Type-7 `<video>` / `<math>` / `<button>` / `<select>` / `<output>` / `<progress>` / `<meter>` / `<noscript>` / `<template>` (source when intersected, skip as a widget — not a player, form, nested document, or disclosure UI), `<object>` (hidden widget), GFM `[ref]: dest` definition brackets, definition-list `: `, footnote-definition `[^1]:`, and footnote-ref `[^1]`: those glyphs are omitted from the leaf until the caret or a selection hits the node (click on painted `bold` / `item` / details still maps into the word; footnote refs stay a superscript when the caret is outside). Click/Home skip wrapping `**[` onto the label of `**[hello](url)**` (and wrapping `**` around `![alt](url)` / `<b>hello</b>` / `<img>`), `[ref]:` opener chrome (`[` `]` `: `) onto the label/dest, GitHub alert `[!NOTE]` onto the title/body, `[TOC]` / `[[toc]]` brackets onto the name, CommonMark list-marker padding (a tab or 1–4 spaces after `-` / `1.`) onto the body, CommonMark 0–3 space indent before ATX `#` / a setext title / a fence / a thematic break onto Title / body / first `-`, and CommonMark code-span padding (a stripped leading/trailing space inside `` ` foo ` ``) onto the painted content. Math `$` / `$$` (including wrapping newlines of `$$\n…\n$$`) / wiki `[[ ]]` / emoji `:name:` / autolink `<>` / GFM `www.` / bare URL / alert `[!NOTE]` already followed that rule. Source mode keeps delimiter **width** when masked (transparent glyphs); WYSIWYG omits the glyphs so the line is the rendered body.

Syntax spans for source-mode masking are derived from the **same comrak AST** as `RichTree` import (`extract_syntax_spans`). There is no tree-sitter-md grammar on this path. Typora `==highlight==` is not a comrak node; the span extractor pairs it with the same rules as rich import so source masking matches WYSIWYG. Dollar math (`$` / `$$`) is a comrak node (`math_dollars`); currency like `$5` and `$` inside code are left as text. Wikilinks (`[[target]]` / `[[target|label]]`) mask `[[` / `]]` (and `target|` when a label is present) with the same caret/selection rule. Known GitHub/Typora emoji shortcodes (`:smile:`) paint as the Unicode glyph when the caret is outside and show `:name:` when it intersects; unmatched `:foo:` stays text. CommonMark character references (`A&amp;B`) paint the decoded glyph unless the caret intersects (then `&amp;`); last-in-line End stays after the glyph, not inside `amp;`; InsertText on dest chrome skips after the glyph (`A&amp;xB`), not `A&xamp;B`; code spans stay literal. CommonMark backslash escapes (`A\*B`) paint the decoded glyph unless the caret intersects (then `\*`); last-in-line End stays after the glyph, not on `*`; InsertText on the escaped char is `A\*xB`, not `A\x*B`; code spans stay literal.

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
| `$` / `$$` math, caret outside | Masked (formula body stays, italic monospace) |
| `$` / `$$` math, caret or selection inside | Visible |
| `$5`, `$ a $`, `` `$1+2$` `` | Not a math span |
| Known `:smile:` / `:heart:` / `:+1:` (etc.), caret outside | Glyph (shortcode hidden) |
| Known `:smile:`, caret or selection inside | Visible `:name:` |
| `A&amp;` last-in-line, End | After the painted glyph, not inside `amp;` |
| `A&amp;B`, caret or selection on the entity | Visible `&amp;` (intersect-reveal) |
| `` `A&amp;B` `` / fenced `A&amp;B` | Literal `&amp;` (not decoded) |
| Unmatched `:foo:`, `` `:smile:` `` | Not an emoji span (text stays) |
| `[!NOTE]` / TIP / IMPORTANT / WARNING / CAUTION in a GitHub alert, caret outside the tag line | Masked (callout body stays) |
| `[!NOTE]` (etc.), caret or selection on the tag line | Visible |
| `[label](url "title")` dest revealed, click/Home on `(` / `"` | Skip onto URL / title inner (wrapping is dest chrome) |
| InsertText on setext `===` / closed ATX trailing `#` / leading `|` / `[!NOTE]` | Skip onto title / first cell / alert body (`Titlex`, `| xa |`); cell-end `|` stays |
| InsertText on thematic `---` / `***` / `<hr>` | New paragraph above (`x\n\n---`), not `x---` / setext; leftover/EOF after; HTML `<hr>` / `<br>` keep a blank line |
| InsertText on two-space / `\` hard break | Next line's first visible char (`a  \nxb`); break start stays after the previous word |
| `<b>` / `<em>` / `<a href>` / `<mark>` / comments, caret outside | Masked (inner text stays, phrasing marks applied) |
| `<b>` (etc.), caret or selection inside the tag span | Visible |
| `<br>` / `<img>` / safe `<svg>` | Widgets (not this rule) |

## Example

Source: `**bold** plain`

- Caret at offset 0 (inside bold span) → both `**` pairs visible (source mode and WYSIWYG).
- Caret at offset 10 (in "plain") → bold delimiters masked / omitted.

## Related

- [Architecture](architecture.md) — rope / RichTree / source projection
- [WYSIWYG roadmap](roadmap.md) — phase status

