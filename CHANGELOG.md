# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- WYSIWYG paints GitHub/Typora emoji shortcodes (`:smile:`, `:heart:`, `:+1:`, `:rocket:`, …) as Unicode (a modest built-in alias table). `:name:` is hidden unless the caret intersects; unmatched `:foo:` stays visible; on-disk bytes stay `:name:`. Source mode substitutes the same glyphs when the caret is outside.
- WYSIWYG `[[wikilink]]` / `[[target|label]]` paint as in-place links (`[[ ]]` hidden unless the caret intersects). `[TOC]` / `[[toc]]` paint a generated heading list from the document outline (marker shown when the caret intersects). Source bytes stay in the file. Source mode masks wiki brackets with the same caret/selection rule as `**`.
- Parse/layout perf gates: 256 KiB mixed GFM load+parse timeout tests, 64 KiB source-layout timeout tests, plus criterion benches (`cargo bench -p markrust-core --bench parse`, `cargo bench -p markrust-editor --bench layout`).
- Contextual table toolbar when the caret is in a table (row/col insert and delete). Right-click still focuses the cell.
- Frontmatter panel edits title, description, tags, and the inner YAML body.
- Input rules: `[` / `]` / `<` type unescaped so task lists, links, and HTML can be typed; fence/thematic-break rules do not insert newlines inside table cells.
- Side-by-side editor mode: `cmd-shift-m` cycles Rich → Source → Split.
- Dirty-tab 3-way merge when a file changes on disk; carets are mapped across the merge.
- Normalize-on-save dialog shows a line hunk preview.
- Chip/caption/frontmatter overlays accept click/drag and Left/Right (Shift extends the inner selection; Home/End are line-local; YAML Up/Down moves between `\n` lines) to place an inner caret without moving the body caret. Delete in those overlays deletes in the field. Typing, Backspace, and Delete replace or delete a non-empty inner drag-selection. Cmd+A selects the overlay draft (not the document). Undo/Redo while the overlay is focused rewinds those draft edits; Escape still cancels and click-away still commits into document undo. Widget IME origin uses the widget bounds / inner caret, not the last text-leaf caret. Body/wrapped/table IME origin is the focused leaf's caret rect (not last-painted). Caret moves and widget focus push that origin via GPUI `invalidate_character_coordinates`. Composition is simulated through `EntityInputHandler` (preedit does not touch the model; origin follows insert / table Tab / caption-chip-frontmatter focus). The OS IME candidate window is still unverified.
- WYSIWYG local images render as pixels (filesystem path, background decode) with alt/caption still editable. Mixed text+image paragraphs lay out as a wrapping horizontal line-box (inline images capped at 1.5em); a standalone image paragraph stays block-sized. Remote `http(s)` images fetch off the UI thread into a URL-keyed cache, then use the same `PathBuf` pipeline; timeouts and failures keep the alt placeholder. Thematic breaks, autolink paint, strikethrough, and hard-break newlines match the rendered document instead of source chrome.
- WYSIWYG paints safe inline and block HTML instead of raw tags (phrasing marks, `<br>`, comments hidden, script/iframe/`javascript:` dropped). HTML-block inner Markdown (`**bold**`, links, code) paints as rich text. `==highlight==` / `<mark>` paint a background; `<sub>`/`<sup>` and `~sub~`/`^sup^` use Unicode (GPUI has no per-run baseline). Footnote references paint as superscripts; footnote definitions and definition lists paint as structured blocks whose bodies are nested rich trees (`**bold**`, links, code). `$…$` / `$$…$$` hide the dollars unless the caret intersects and paint the TeX in italic monospace (currency like `$5` and `$` in code stay text). GitHub `> [!NOTE]` / TIP / IMPORTANT / WARNING / CAUTION alerts paint as labeled callouts (left rule + label; `[!NOTE]` hidden unless the caret intersects). Parse still uses the background worker; the folder watcher still `recv`s off the UI thread. Mermaid fences stay code blocks (no small in-process SVG renderer without a large layout crate).

### Changed

- Source-mode syntax spans are extracted from comrak (same grammar as the WYSIWYG `RichTree`). tree-sitter-md is no longer a `markrust-core` dependency. Fenced-code highlighting still uses tree-sitter for rust/json/yaml/bash. Source mode masks `==highlight==` (and `~sub~` / `^sup^` / `$` / `$$` math / `[[wikilink]]` / known `:emoji:` shortcodes) with the same caret/selection rule as `**`.
- Architecture docs rewritten for the rope + RichTree + source-projection pipeline.

### Added (earlier this cycle)

- WYSIWYG editing core: typing, backspace, bold/italic/code/link wrap, list split/indent/outdent, task-checkbox toggle.
- Typora-style input rules (`# `, lists, quotes, fences, `---`, auto-close `*`/`**`/`_`/`__`/` `/`~~`), including inside list items.
- Editable fenced-code language chip and image alt/caption in WYSIWYG (IME, click-away, empty-alt placeholder).
- Table Tab navigation, insert/delete row and column, header-row constraints, and a right-click table menu.
- Frontmatter panel with in-place title/tags editing in WYSIWYG and a Keep original / Normalize / Cancel save dialog.
- External-edit caret mapping (`map_offset_across_change`) when reloading a file changed on disk.
- Source-mode Cmd/Ctrl+B/I/E/K wrap that keeps delimiter masking working (caret stays inside the span).
- Precise source-mode click-to-caret (byte offset from x position, not line-only).
- Source-mode tables render padded columns until focused, then show raw pipes.
- Monorepo bootstrap with MPL-2.0 licensing and CI workflows.
- `markrust-core`: rope-backed `DocumentBuffer`, undo/redo, line index, background Markdown parse, `SyntaxNodeSpan` extraction.
- `markrust-editor`: headless delimiter masking algorithm with unit tests.
- `markrust` CLI stub (`--version`).
- GPUI dependency pinned in `markrust-app` (shell stub for Phase 2).

### Fixed

- WYSIWYG Backspace/Delete that empties the last inner character of `==highlight==` or GFM `~~strikethrough~~` unwraps the marks (same as empty `****` after deleting the last bold character). It no longer leaves `====` or `~~~~` painted as chrome. Empty-caret Cmd-B still inserts `****` with the caret inside.
- WYSIWYG Left at the start of a markdown link label no longer sits on `[`. Backspace there deletes the previous visible character, not `[` (which used to leave `see label](url)`). Delete at the end of the label does not swallow `](url)`. The same skip covers `**` / ticks / URL and email autolink `<>`, GFM `[label][ref]`, and wrapping dest around a linked inline image (`[![alt](img)](url)`). Option-Backspace at the start of a label does not eat `[`.
- Click/IME on a markdown soft wrap (`hello\nworld` paints `hello world`) or a hard break (`a  \nb` / `a\\\nb`) maps onto the break bytes, not the paragraph start. Quoted and list-wrapped lines map onto the newline, not `>` / `-`.
- The gap between two top-level blocks (a standard `\n\n` paragraph separator) is a clickable empty line: click places the caret there instead of snapping to the next heading/paragraph, typing inserts a new paragraph in the gap, and Down from the previous block lands on that one empty line then the next block (extra unused newlines are not extra steps). A trailing blank after the last block (`hello\n\n`) is the same: click below the last paragraph, type a new one. Clicking leftover viewport below the last painted line also places the caret on that trailing blank — and if the file has none (`hello`), leftover click opens one so typing starts a new paragraph (`hello` then `x`, not `hellox`). Click on the last line of the last paragraph still sits in that paragraph. Clicks on a last-block image or thematic rule are not leftover (the image is selected; inline still opens alt). A document that is only newlines still hosts a caret. A single trailing `\n` is only the block terminator and is not painted.
- Click/IME/Home on an empty quoted paragraph (`> `) or empty list item (`- `, `1. `, `- [ ] `) sit after the prefix so typing is `> x` / `- x`, not chrome. Nested quotes (`> > hello`) paint and click `h`; Left at that start leaves the body (does not sit on `>`). Ordered lists (`1. hello`) and unchecked tasks (`- [ ] hello`) match `- hello`. Right at the end of a wrapped list item (`- hello` / `  world`) skips continuation indent onto `w`.
- Click/IME on a quoted paragraph (`> hello` paints `hello`) or list item (`- hello` / `- [x] done`) maps the first painted character onto the body, not `>` / `- `. Left at the start of that visible body leaves the block (does not sit on chrome). Right at a wrap inside a quote (`> hello` / `> world`) skips the next line’s `>` onto `w` and does not skip the letter. Home/snap on quote and list containers use descendant body ranges so they do not treat the whole `> …` / `- …` span as a caret home. The same `clamp_raw_prefix` helper now applies to those leaves (list markers included) and skips HTML-block tag bytes (`<div>`) after the quote prefix. Unquoted paragraphs stay 1:1; heading click, table cells, and fence prefix-skip tests are unchanged.
- Click/IME on quoted or list-nested HTML (and indented code) maps painted body text onto source offsets that skip `>` / list indent — the same `code_body_source_map` fenced code already uses. The first painted HTML body character is not the `>` byte. Left/Right (and Up/Down / Home) in those raw blocks stay on editable body offsets; they do not walk prefix bytes. Unquoted fences stay 1:1; table and leftover-below click are unchanged.
- Click/IME on a quoted or list-nested fenced code body maps the painted `code` onto source offsets that skip `>` / list indent (the same prefix Tab/Enter keep). Clicking the first painted character no longer lands on `>`. Unquoted fences stay 1:1.
- Cmd+A in a language chip, image caption, or frontmatter overlay selects that field's draft instead of the whole document. Escape still cancels the overlay; click-away still commits.
- Typing, Backspace, and Delete in those overlays replace or delete a non-empty inner drag-selection instead of inserting or deleting one character at the caret.
- Undo/Redo while a chip/caption/frontmatter overlay is focused rewinds overlay draft edits first (a per-overlay stack). Committing the overlay still folds into document undo.
- Left / Right / Shift-Left / Right / Home / End in a language chip, image caption, or frontmatter overlay move the inner caret (Shift extends the inner selection) instead of the document body. Delete in those overlays deletes in the field.
- WYSIWYG (and source) Shift-Up / Shift-Down / Shift-Home / Shift-End were unbound, so only Shift-Left/Right extended the selection. Those keys now extend; Cmd-Left/Right are line start/end on macOS (laptops have no Home/End); Page-Up/Down move by a viewport of lines. Overlay YAML Shift-Up/Down/Home/End extend the inner draft the same way.
- Option-Left/Right (Ctrl-Left/Right) move by word; Option-Shift / Ctrl-Shift extend the selection. Cmd-Up/Down (Ctrl-Home/End) jump to document start/end; Cmd-Shift-Up/Down and Ctrl-Shift-Home/End extend. Shift-Page-Up/Down extend by a viewport. Overlay drafts use the same word and document keys. Cmd-Left/Right stay line Home/End.
- Option-Backspace/Delete (or Ctrl-Backspace/Delete) delete by word; Cmd-Backspace/Delete delete to the current line start/end (not the whole document). A non-empty selection is deleted like Backspace.
- Images are one keyboard/click unit: Left/Right skip `![…](url)` instead of walking `!` / `[` / `)`; Backspace after or Delete before removes the whole image; click selects the image source (inline still opens alt).
- WYSIWYG Copy/Cut (`Cmd/Ctrl+C` / `Cmd/Ctrl+X`) were unbound, so the clipboard stayed empty. Copy now writes source markdown of the visible selection (Typora): a fully selected bold/italic/code/link run includes `**` / `*` / `` ` `` / `[…](url)`, a fully selected heading/list/quote includes `# ` / `- ` / `>`, and a selected image is `![alt](url)`. A partial selection inside a run stays inner text. An empty caret copies the current block (heading `# `, list item `- `, quote `>`, fence ticks, paragraph source, image `![…](url)`; table cell is cell text, not `|`). Cut deletes that same block (collapsed table Cut is a no-op). Source mode copies the selected source bytes. Overlay Copy/Cut with a non-empty inner selection copies/cuts the draft; an empty overlay caret copies the whole chip/caption/frontmatter draft (Cut stays a no-op).
- Cmd/Ctrl+B/I/E/K on an empty image caption or frontmatter field inserts wrap marks with the caret inside, so typing becomes `**x**` rather than `****x`.
- Cmd/Ctrl+B/I/E/K while an image caption or frontmatter field is focused wrap that field instead of committing and toggling the document body. The fenced-code language chip ignores wrap (does not bold the body).
- Tab in a fenced-code language chip, image caption, or frontmatter overlay commits that field and no longer indents the document body.
- Opening a Markdown file no longer freezes the window: the folder watcher no longer blocks GPUI's UI thread, and Markdown parse stays on the background worker.
- Empty list-item Enter outdents or exits the list instead of inserting a blank paragraph in place.
- Source-mode line height for a heading vs body line is derived from syntax spans, not mask visibility.
