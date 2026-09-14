# MarkRust architecture

MarkRust is a **true WYSIWYG** Markdown editor: the user edits a rendered rich document; Markdown is the on-disk serialization. A **source** surface (Typora-style delimiter masking) and a **side-by-side** split are optional. All native Rust on GPUI.

The rope buffer is the single source of truth. A derived `RichTree` (comrak) is authoritative for interpretation and command targeting. Source-mode masking is a projection of that same grammar, not a second Markdown parser.

## Crate boundaries

| Crate | Responsibility | GUI deps |
|---|---|---|
| `markrust-core` | Rope buffer, undo, line index, comrak `RichTree` + source spans, revision tokens | None |
| `markrust-editor` | `HeadlessEditor` + source masking/layout; GPUI `MarkdownEditor` and `RichEditorView` | GPUI (view only) |
| `markrust-app` | `HeadlessWorkspace` session + GPUI window chrome | GPUI (Zed git pin) |
| `markrust` | CLI (`parse_args` / `run`) and desktop binary | GUI only for `--gui` |

**Rule:** `HeadlessEditor` (`markrust-editor::headless`) and `HeadlessWorkspace` (`markrust-app::session`) must not use GPUI types. The GPUI window maps keys/clicks to `WorkspaceCommand` / `EditorCommand`.

## Headless command layer

```
GPUI window / CLI
        ↓  WorkspaceCommand / EditorCommand
HeadlessWorkspace (tabs, drop routing, autosave clock, export)
        ↓  EditorCommand / RichCommand
HeadlessEditor / RichEngine (Document + caret/selection)
        ↓
source: compute_visibility / build_display_layout
WYSIWYG: RichEditorView over RichTree
export HTML (comrak, same extension set as import)
```

- `EditorCommand`: insert, backspace, delete, word/line delete, move/select caret, undo/redo, jump, wrap.
- `RichCommand`: WYSIWYG typing, marks, lists, tables, frontmatter fields — compiled to byte splices on the rope.
- `WorkspaceCommand`: save/open/export, drop files, theme, tabs, heading jump, `AdvanceTime` (fake clock), external-change reload, `SaveWithReview`.
- Drop classification stays pure in `drop.rs`.
- File-watcher policy is `classify_external_change` (ignore own saves; 3-way merge dirty tabs; prompt on conflict or clean-tab disk change).

Unit tests live next to the modules. Headless e2e lives in `crates/markrust-app/tests/e2e.rs` (no window). CLI e2e lives in `crates/markrust/tests/e2e.rs` (`assert_cmd`, never empty args / `--gui`).

## Data flow

```
Keyboard/Mouse/IME/FileWatcher
        ↓
DocumentBuffer (ropey) + UndoStack
        ↓
BackgroundMarkdownParser (comrak on a worker thread)
        ↓
SyntaxNodeSpan map (revision-stamped)  — source-mode masking only
RichEngine::sync → RichTree              — WYSIWYG + commands
        ↓
Source: compute_visibility + layout + paint
WYSIWYG: virtualized block list + BlockTextElement; mixed text+image paragraphs are a wrapping flex line-box (`img(PathBuf)`, 1.5em height cap); standalone image paragraphs stay block-sized (max 720×480); every local, remote, or `data:` image reaches the background decoder only through a validated content-addressed cache file (bounded bytes, allowlisted raster format, and pixel budget; SVG also uses the strict shared validator); remote URLs are never fetched on open: an explicit per-tab Load images action starts only public HTTPS/443 fetches, resolves and pins each redirect hop, and writes only validated image bytes into versioned `cache/markrust/images` (everything else remains an alt placeholder); safe inline/block HTML is painted (phrasing tags hidden unless the caret intersects, phrasing marks applied; inline `style=` / HTML `color` and same-block CSS classes paint; safe `<svg>` is an image; script/iframe/`javascript:` dropped); HTML-block comments (`<!-- … -->`) and PI/CDATA (`<?…?>` / `<![CDATA[…]]>`, including an inner `>`) hide unless the caret intersects (then source; Comrak’s empty sourcepos is recovered from the literal); CommonMark Type-1 `<style>` / `<textarea>` hide unless the caret intersects (then source; tags skip as dest chrome); Type-1 `<script>` stays hidden and skips as a widget; GFM tagfilter `<iframe>` / `<noembed>` / `<noframes>` stay hidden widgets the same way; `<title>` / `<xmp>` / `<plaintext>` hide unless the caret intersects (then source, like Type-1 `<textarea>`) and skip/delete as one widget; Type-6 `<details>` skips/deletes as one widget and reveals source on intersect (not a disclosure UI); `<video>` / `<dialog>` / `<form>` / `<canvas>` / `<math>` skip the same way (source on intersect, not a player or form UI); `<button>` / `<select>` / `<input>` / `<label>` / `<option>` / `<fieldset>` / `<legend>` / `<output>` / `<progress>` / `<meter>` skip as dest-chrome widgets the same way (source on intersect, not a live form UI); `<noscript>` / `<template>` skip the same way (source on intersect, not a nested document); `<datalist>` is not a dest-chrome widget (inner `<option>` is); `<picture>` / `<summary>` / `<search>` / `<slot>` stay flow; `<object>` / `<embed>` stay hidden widgets like `<iframe>`; GFM `<pre>` inner text stays literal (not a nested Markdown parse); other HTML-block inner Markdown (`**bold**`, links, code) is a nested parse, not raw chrome (wrapper `<div>` / `</div>` hide unless the caret intersects); thematic `---` / `<hr>` paint as a rule unless the caret intersects (then source); `==highlight==` / `<mark>` paint a background; `<sub>`/`<sup>` and `~sub~`/`^sup^` map to Unicode (GPUI has no per-run baseline); footnote refs paint as superscripts unless the caret intersects (then `[^1]`); footnote definition `[^1]:` and definition-list `: ` hide unless the caret intersects; footnote definition and definition-list bodies are nested rich trees (`**bold**`, links, code); GFM `[ref]: url` / image-ref definitions are recovered as editable WYSIWYG blocks (comrak detaches them); GFM wrap marks (`*` / `**` / `_` / `~~` / ticks / `==`), ATX `#`, setext underlines, markdown link `[` `]`, list/task markers, quote `>`, fence ticks, table `|`, HTML phrasing tags, HTML-block `<div>` / `</div>`, thematic `---` / `<hr>` source, and reference-definition `[` `]` hide unless the caret or a selection intersects the node (link dest `(url)` stays hidden while the caret is only in the label; click on painted `bold` / `label` / item still maps into the word); images paint `![]()` when the image node is intersected; `$…$` / `$$…$$` hide the dollars unless the caret intersects, and paint the TeX in italic monospace (no TeX-to-glyphs); `[[wikilink]]` / `[[target|label]]` paint as in-place links (`[[ ]]` hidden unless the caret intersects); `[TOC]` / `[[toc]]` paint a generated heading list from the outline (marker shown when the caret intersects); GitHub `> [!NOTE]` / TIP / IMPORTANT / WARNING / CAUTION alerts paint as labeled callouts (left rule + label; `[!NOTE]` chrome hidden unless the caret intersects); IME origin from the focused widget or caret leaf (`wysiwyg/ime.rs`), pushed with `invalidate_character_coordinates` after caret/widget/composition changes (OS CJK candidate window still unverified)
```

Parse, the folder watcher `recv`, an explicitly approved remote-image fetch, and GPUI image decode stay off the GPUI UI thread. `Document::new` / `from_file` only *schedule* a parse; they do not fetch network images. The frame drains parse with `apply_pending_parse`. CI timeout tests (`crates/markrust-core/tests/perf_gates.rs`, `crates/markrust-editor/tests/perf_gates.rs`) fail if load+parse or source layout of a 256 KiB fixture exceeds a budget. Local numbers: `cargo bench -p markrust-core --bench parse` and `cargo bench -p markrust-editor --bench layout`.

## Document model

- **On disk:** plain UTF-8 `.md` / `.txt` — no proprietary container.
- **`DocumentProcessingMode`:** `MarkdownWysiwyg` parses with comrak; `PlainText` skips parsing.
- **`revision`:** monotonic counter on every buffer edit; parse results carry the revision they were computed for so stale updates are ignored.
- **`saved_content`:** last reconciled disk snapshot (load, save, or last merged disk bytes). Used as the 3-way merge base.

## Parser strategy

| Layer | Engine | When |
|---|---|---|
| Document structure + source spans | comrak (same options as HTML export: GFM + footnotes + description lists + math_dollars + alerts + wikilinks) | Every edit, background thread |
| WYSIWYG tree | `rich::import` (comrak → `RichTree`) | On `RichEngine::sync` |
| Fenced-code highlighting | tree-sitter rust/json/yaml/bash | Viewport paint of a code body |

tree-sitter-md is not used. Source-mode `SyntaxNodeSpan`s are extracted from the comrak AST so masking cannot disagree with the rich tree's grammar. `==highlight==` is paired in that same pass (comrak has no highlight node). `$…$` / `$$…$$` are comrak `math_dollars` nodes (multiline `$$\n…\n$$` wrapping newlines and quote/list prefixes on those lines are dest chrome like `$` / `$$`). `[[wikilink]]` / `[[target|label]]` are comrak `wikilinks_title_after_pipe` nodes. GitHub/Typora `:smile:` shortcodes are paired from a modest alias table (unknown `:foo:` stays text). GitHub alerts (`> [!NOTE]`, …) are comrak `alerts` nodes. `[TOC]` / `[[toc]]` are classified on import as a TOC block (not a comrak node). YAML frontmatter may close with Jekyll/Pandoc `...` as well as `---` (comrak's delimiter is `---`; import rewrites the closer for parse and keeps original bytes). CommonMark indented-code sourcepos that drops the opening 4 spaces / tab is recovered so Home/click skip that indent. CommonMark 0–3 spaces before ATX `#` / a setext title / a fence / a thematic break are recovered the same way (quoted keep `>`; four spaces stay indented code). Fenced bodies strip `fence_offset` spaces on content lines.

## Editing surfaces

| Mode | Default | How |
|---|---|---|
| **Wysiwyg** | yes | `RichEditorView` — Typora in-place: wrap marks, ATX `#`, setext underlines, link `[` `]`, list/task markers, quote `>`, fence ticks, indented-code indent, table `|`, HTML phrasing tags, HTML-block `<div>` / `</div>` / comments / Type-1 `<style>` / `<textarea>`, thematic `---` / `<hr>`, definition-list `: `, footnote-definition `[^1]:`, and footnote-ref `[^1]` hidden unless the caret or a selection intersects the node (dest `(url)` hidden while the caret is only in the label; a thematic break is a rule when the caret is outside; footnote refs are a superscript when the caret is outside; `<pre>` inner text is literal; `<script>` / `<iframe>` / `<object>` stay hidden as widgets; `<title>` / `<xmp>` / Type-6 `<details>` / `<fieldset>` / `<video>` / `<dialog>` / `<button>` / `<select>` / `<output>` / `<noscript>` reveal source on intersect and skip as widgets); Markdown is serialization |
| **Source** | `cmd-shift-m` | Existing delimiter-masking editor; spans from comrak |
| **Split** | cycle `cmd-shift-m` again | Source left, Wysiwyg right; shared `Document` |

Caret/selection are source byte offsets. `RichEngine` provides delimiter-skipping snap/step for WYSIWYG, including a clickable blank on a standard `\n\n` between top-level blocks and on a trailing blank after the last block (`hello\n\n`). Quote and list-indent prefixes on nested fences, HTML, quoted paragraphs, and list items are skipped the same way (click, IME, and Left/Right stay on the painted body, not on `>` / `- ` / `1. `; CommonMark tab / 1–4 space padding after the list marker skips the same way). CommonMark code-span stripped padding spaces (`` ` foo ` ``) skip like ticks. CommonMark indented-code opening 4 spaces / tab skip like fence ticks (comrak sourcepos that drops the indent is recovered). CommonMark 0–3 spaces before ATX `#` / a setext title / a fence / a thematic break skip the same way (quoted `>  # Title` keeps `>`; four spaces stay indented code). Fenced content lines strip `fence_offset`. GFM table `|` and the alignment `|---|` row skip onto the painted cell the same way (Home/click do not sit on a hidden pipe), including compact spec tables that omit wrapping pipes (`foo|bar`). An escaped `\|` in a cell is a literal pipe, not dest chrome (typing next to it does not split the row). GitHub alert `[!NOTE]` skips onto the title/body; `[TOC]` / `[[toc]]` skip brackets; GFM `[ref]: url` skips `[` onto the label and `: ` onto dest. Inline chrome (`[` / `](url)` / `[ref]`, dest wrapping `(url "title")` / `'title'` / `(title)` so click/Home land on the URL or title inner not `(` `"` `)`, `**` / ticks, wrapping `**` around a whole `[hello](url)`, URL and email autolink `<>`, `$math$` / multiline `$$\n…\n$$` wrapping newlines / `[[wiki]]` / `:emoji:`, wrapping dest around a linked image, HTML phrasing tags `<b>` / `<a href>` / comments) is skipped the same way (Left at a link label does not sit on `[` or wrapping `*`; Backspace there does not nibble dest, `$` / `[[` / `:`, wrapping marks, or HTML `>`). End on a last-in-line HTML `<a href>label</a>` / `<b>` / comment stays on the inner insert home (before `</a>` / `</b>` / `<!--`), not dest `href`. Two-space / backslash hard breaks (`a  \nb`) skip like `<br>`. Empty-caret wrap wraps dest-chrome widgets (`**<br>**`, wrap around a hard break, `**[^1]**`, `**<https://…>**`) instead of splicing `****` / `[]()` in front; leftover below a last-block widget still opens `[]()`. Empty wrap on ATX `#`, setext underlines, table `|`, and 0–3 space heading indent uses the same Home/click skip so the pair sits in the body (`# **x**Title`, `| **x**a |`), not splice `**x**#` / `**x**|`. Empty wrap / InsertText on markdown-link `[`, HTML phrasing `<b>` / `<a href>`, and GFM alignment dashes skip onto inner text the same way (`[**x**hello](url)`, `<b>**x**hello</b>`, not `|x---|`). InsertText on revealed list/quote/task/`[ref]:` prefixes skips onto the body (`> xhello`, `- [x] xdone`, `[xref]: url`), not splice `x> hello` / `x[ref]:`. InsertText on setext underline / closed ATX trailing hashes / leading table `|` / GitHub `[!NOTE]` matches that skip (`Titlex\n===`, `# Titlex #`, `| xa |`, `> [!NOTE]\n> xbody`), not glue `Title\n===x` / `# Title #x` / `x| a |` / `x[!NOTE]` (cell-end `|` and open ATX `# Titlex` stay; document EOF on underline / trailing hashes still opens a body line). InsertText on a thematic `---` / `***` / `<hr>` widget opens a paragraph above (`x\n\n---`, not `x---` / a setext heading; quoted keep `>`; leftover/EOF after; HTML `<hr>` / `<br>` keep a blank line). InsertText on two-space / `\` hard-break chrome lands on the next line's first visible char (`a  \nxb`); the break start still extends the previous word. GFM extended autolinks (`www.`, `https://`, bare email) recover the URL literal when comrak reports `0..1` / marker sourcepos. CommonMark character references (`A&amp;B`) paint the decoded glyph and skip `amp;` dest chrome (click/Home on `&`; Left/Right/Backspace/Delete one step; InsertText on dest chrome skips after the glyph (`A&amp;xB`), not `A&xamp;B`; End on last-in-line `A&amp;` after the glyph, not inside `amp;`; End on last-in-line `[^1]` after the widget, not the label start; linked `[![alt](img)](url)` End stays on wrapping `]`, not dest; code spans stay literal). CommonMark backslash escapes (`A\*B`) paint the decoded glyph and skip the escaped char the same way (click/Home on `\`; InsertText on `*` is `A\*xB`, not `A\x*B`; End on last-in-line `A\*` after the glyph, not on `*`; code spans walk `\` and `*` as literals). Autolink `<>` stay hidden unless the caret or a selection intersects the span. Wrap marks (`*` / `**` / `_` / `~~` / ticks / `==`), ATX heading hashes, setext underlines, markdown link `[` `]`, list/task markers (`- ` / `1. ` / `[ ] `, including tab / 1–4 space padding), quote `>`, fence ticks (and the info string), table `|`, HTML phrasing tags (`<b>` / `<a href>` / comments), HTML-block `<div>` / `</div>` / `<!-- … -->`, thematic `---` / `<hr>` source, definition-list `: `, footnote-definition `[^1]:`, and footnote-ref `[^1]` use that same intersect rule (hidden outside; painted when the caret or a selection hits the node — list markers per item, quote `>` for the whole quote, fence ticks for that block, table pipes for the whole table, `: ` per details block, `[^1]:` per definition; a thematic break is a rule widget when the caret is outside; a footnote ref is a superscript when the caret is outside). Link dest `(url)` stays hidden while the caret is only in the label and paints when the caret is in dest or a selection overlaps the node. Images paint `![]()` when the image node is intersected. Left/Right/Delete still treat a thematic break / `<hr>` as one unit. Backspace/Delete that empties a marked run unwraps surrounding `**` / `==` / `~~` / ticks (no leftover `====`). Empty `> ` / `- ` lines sit after the prefix so typing is `> x` / `- x`. InsertText / wrap / block commands at EOF on a closing fence, last-line fence opener / info-string, setext underline, closed ATX trailing `#`, HTML-block `</div>` / single-line `<pre>…</pre>`, last-block `<svg>…</svg>` / `<img>` / `<br>`, or `[TOC]` (no following newline) insert a newline first (`===\nx`, `# Title #\nx`, `</div>\nx`, `<pre>…</pre>\nx`, `<svg></svg>\nx`, ` ```rust\nx`, `[TOC]\nx`). Enter inside `[hello](url)` / `[hello][ref]` / `![alt](url)` / GFM autolink stays one node (label/title wrap with `\n`; dest/autolink split after the node; ATX headings split after the heading line). Enter inside wrap marks / inline code / HTML phrasing stays one node (`**bo\nld**`, `` `co\nde` ``, `<b>he\nllo</b>`; ATX `# **hello**` splits after the heading). Clicking leftover viewport below the last painted line places the caret on that trailing blank, or opens one when the file has none (`hello` then type `x` is two paragraphs). Click on the last line of the last block still sits in that paragraph. A document that is only newlines still hosts a caret. A lone terminator `\n` is not an empty paragraph.

## Save and external edits

- Default save writes the buffer verbatim (untouched blocks are untouched bytes).
- When house-style Normalize would change the file, Save offers Keep original / Normalize / Cancel with a hunk preview.
- Autosave writes the buffer as-is (no Normalize).
- External disk changes: if the buffer still matches the last snapshot, prompt to reload; if the tab is dirty, 3-way line-merge disjoint edits (carets mapped with `map_offset_across_change`); overlapping edits prompt before discarding.

## Related

- [WYSIWYG roadmap](roadmap.md) — phase status and handoff
- [Delimiter masking](delimiter-masking.md) — source-mode visibility algorithm
