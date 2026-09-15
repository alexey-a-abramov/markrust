// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rich editing commands. Each command compiles to a byte splice on the
//! source buffer (the single source of truth) and is one undo transaction.

use std::ops::Range;

use crate::document::Document;
use crate::undo::{SelectionSnapshot, TransactionKind};

use super::engine::{
    atomic_character_reference_range, atomic_delete_range, atomic_hard_break_range,
    atomic_html_break_range, blank_caret_gap_after_last, blank_caret_gap_at, blank_caret_gaps,
    caret_for_click_below_content, definition_details_marker_on_line, expand_mark_delimiters,
    expand_range_over_escaped_table_pipe, footnote_def_marker_on_line, frontmatter_body_start,
    html_block_atomic_range, html_block_break_range, html_block_image_range, html_tag_bounds,
    insert_offset_escaping_table_pipe, is_unescaped_pipe, link_ref_def_marker_on_line,
    list_marker_on_line, quote_list_prefix_on_line, raw_body_range, raw_container_prefix,
    skip_line_prefix_and_fence, step_right_in_slice, thematic_break_range, Bias, RichEngine,
    TablePos,
};

pub use super::engine::{code_body_source_map, html_block_literal_source_map};
use super::escape::{escape_text, EscapeContext};
use super::input_rules::{input_rule_breaks_table, match_input_rule_with, InputRule};
use super::serialize::serialize_block;
use super::tree::{
    code_span_visible_range, expand_around_markdown_link, expand_link_and_html_chrome,
    is_toc_marker, link_reference_def_chrome, markdown_link_chrome, markdown_link_dest_parts,
    Block, BlockKind, ColumnAlign, Frontmatter, HeadingStyle, Inline, LinkAttrs, MarkSet,
    MarkdownLinkChrome, NodeId, RichTree,
};

/// Caret/selection in source byte offsets.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaretState {
    pub range: Range<usize>,
    pub reversed: bool,
}

impl CaretState {
    pub fn collapsed(offset: usize) -> Self {
        Self {
            range: offset..offset,
            reversed: false,
        }
    }

    pub fn cursor(&self) -> usize {
        if self.reversed {
            self.range.start
        } else {
            self.range.end
        }
    }

    pub fn snapshot(&self) -> SelectionSnapshot {
        SelectionSnapshot {
            start: self.range.start,
            end: self.range.end,
            reversed: self.reversed,
        }
    }

    pub fn restore(&mut self, snap: SelectionSnapshot) {
        let len_ok_start = snap.start;
        let len_ok_end = snap.end;
        self.range = if len_ok_start <= len_ok_end {
            len_ok_start..len_ok_end
        } else {
            len_ok_end..len_ok_start
        };
        self.reversed = snap.reversed;
    }

    pub fn collapse_to(&mut self, offset: usize) {
        self.range = offset..offset;
        self.reversed = false;
    }

    fn clamp(&mut self, len: usize) {
        let start = self.range.start.min(len);
        let end = self.range.end.min(len);
        if start <= end {
            self.range = start..end;
        } else {
            self.range = end..start;
            self.reversed = !self.reversed;
        }
    }
}

/// Block-type change for [`RichCommand::SetBlockType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockType {
    Paragraph,
    Heading(u8),
}

/// Commands the WYSIWYG surface issues. Movement stays on [`crate` editor commands].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RichCommand {
    InsertText(String),
    Backspace,
    Delete,
    /// Option-Backspace / Ctrl-Backspace: delete to the previous word start.
    DeleteWordLeft,
    /// Option-Delete / Ctrl-Delete: delete to the next word end.
    DeleteWordRight,
    /// Cmd-Backspace: delete from the caret to the current source line start.
    DeleteToLineStart,
    /// Cmd-Delete: delete from the caret to the current source line end.
    DeleteToLineEnd,
    SplitBlock,
    InsertLineBreak,
    ToggleMark(MarkSet),
    ToggleLink,
    SetBlockType(BlockType),
    ToggleBlockquote,
    ToggleList {
        ordered: bool,
    },
    SetTaskChecked {
        id: NodeId,
        checked: bool,
    },
    IndentList,
    OutdentList,
    SetCodeInfo {
        id: NodeId,
        info: String,
    },
    SetImageAlt {
        source_range: Range<usize>,
        alt: String,
    },
    SetFrontmatter {
        raw: String,
    },
    SetFrontmatterField {
        key: String,
        value: String,
    },
    TableTab {
        reverse: bool,
    },
    InsertTableRow {
        after: bool,
    },
    InsertTableColumn {
        after: bool,
    },
    DeleteTableRow,
    DeleteTableColumn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RichOutcome {
    Changed,
    Noop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RichError {
    InvalidRange,
    /// A frontmatter edit did not pass the YAML validation gate, so the
    /// document was left unchanged.
    InvalidFrontmatter(crate::FrontmatterError),
}

/// Apply `command` to `doc`, keeping `engine` and `caret` in sync.
pub fn apply_rich_command(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    command: RichCommand,
) -> Result<RichOutcome, RichError> {
    engine.sync(doc);
    caret.clamp(doc.buffer.len_bytes());
    // YAML is the frontmatter panel. Body commands must not splice into it
    // (a caret at 0 on a file with `---` would otherwise type `x---`).
    if !matches!(
        command,
        RichCommand::SetFrontmatter { .. } | RichCommand::SetFrontmatterField { .. }
    ) {
        clamp_caret_out_of_frontmatter(engine, caret, &doc.buffer.content());
    }
    match command {
        RichCommand::InsertText(text) => insert_text(doc, engine, caret, &text),
        RichCommand::Backspace => backspace(doc, engine, caret),
        RichCommand::Delete => delete_forward(doc, engine, caret),
        RichCommand::DeleteWordLeft => delete_to_bound(doc, engine, caret, DeleteBound::WordLeft),
        RichCommand::DeleteWordRight => delete_to_bound(doc, engine, caret, DeleteBound::WordRight),
        RichCommand::DeleteToLineStart => {
            delete_to_bound(doc, engine, caret, DeleteBound::LineStart)
        }
        RichCommand::DeleteToLineEnd => delete_to_bound(doc, engine, caret, DeleteBound::LineEnd),
        RichCommand::SplitBlock => split_block(doc, engine, caret),
        RichCommand::InsertLineBreak => insert_line_break(doc, engine, caret),
        RichCommand::ToggleMark(mark) => toggle_mark(doc, engine, caret, mark),
        RichCommand::ToggleLink => toggle_link(doc, engine, caret),
        RichCommand::SetBlockType(kind) => set_block_type(doc, engine, caret, kind),
        RichCommand::ToggleBlockquote => toggle_blockquote(doc, engine, caret),
        RichCommand::ToggleList { ordered } => toggle_list(doc, engine, caret, ordered),
        RichCommand::SetTaskChecked { id, checked } => {
            set_task_checked(doc, engine, caret, id, checked)
        }
        RichCommand::IndentList => indent_list(doc, engine, caret),
        RichCommand::OutdentList => outdent_list(doc, engine, caret),
        RichCommand::SetCodeInfo { id, info } => set_code_info(doc, engine, caret, id, &info),
        RichCommand::SetImageAlt { source_range, alt } => {
            set_image_alt(doc, engine, caret, source_range, &alt)
        }
        RichCommand::SetFrontmatter { raw } => set_frontmatter(doc, engine, caret, &raw),
        RichCommand::SetFrontmatterField { key, value } => {
            set_frontmatter_field(doc, engine, caret, &key, &value)
        }
        RichCommand::TableTab { reverse } => table_tab(doc, engine, caret, reverse),
        RichCommand::InsertTableRow { after } => insert_table_row(doc, engine, caret, after),
        RichCommand::InsertTableColumn { after } => insert_table_column(doc, engine, caret, after),
        RichCommand::DeleteTableRow => delete_table_row(doc, engine, caret),
        RichCommand::DeleteTableColumn => delete_table_column(doc, engine, caret),
    }
}

/// Leftover viewport click below the last painted block: open a trailing
/// blank if the file has none, then sit the caret on that empty paragraph
/// (Typora: `hello` then type `x` is two paragraphs, not `hellox`).
///
/// No-op when a trailing blank (or a newlines-only document) already hosts
/// a caret. Click **on** the last line of the last block must not call this
/// — that path hit-tests the leaf and may land at EOF inside the paragraph.
pub fn place_caret_for_click_below(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> RichOutcome {
    engine.sync(doc);
    caret.clamp(doc.buffer.len_bytes());
    let source = doc.buffer.content();
    if let Some(gap) = blank_caret_gap_after_last(engine.tree()) {
        // A degenerate empty gap at EOF on a closing `---` is not a painted
        // blank — leftover must open a body paragraph (wrap/type must not
        // glue `****` / `x` onto the fence).
        if gap.start < source.len()
            || !needs_newline_after_frontmatter_fence(&source, engine, source.len())
        {
            caret.collapse_to(caret_for_click_below_content(engine.tree()));
            return RichOutcome::Noop;
        }
    }
    let at = source.len();
    let (insert, caret_after) = if source.ends_with('\n') {
        ("\n", at)
    } else {
        ("\n\n", at + 1)
    };
    let before = caret.snapshot();
    let after = CaretState::collapsed(caret_after);
    doc.replace_range_tx(
        at,
        at,
        insert,
        TransactionKind::Command,
        before,
        after.snapshot(),
    );
    *caret = after;
    engine.sync(doc);
    caret.collapse_to(caret_for_click_below_content(engine.tree()));
    RichOutcome::Changed
}

fn insert_text(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    text: &str,
) -> Result<RichOutcome, RichError> {
    if caret.range.is_empty() || text.is_empty() {
        return insert_text_inner(doc, engine, caret, text);
    }
    // Deletion must run through Markdown-aware selection expansion before
    // insertion can resolve its new context (table cells, links, lists).
    // Keep those splices, but expose the replacement as one user undo step.
    // Prevent coalescing even when selection clamping makes deletion empty.
    let undo_group = doc.begin_undo_group(caret.snapshot());
    let result = insert_text_inner(doc, engine, caret, text);
    doc.finish_undo_group(undo_group, caret.snapshot());
    result
}

fn insert_text_inner(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    text: &str,
) -> Result<RichOutcome, RichError> {
    if text == "\n" || text == "\r\n" || text == "\r" {
        return split_block(doc, engine, caret);
    }
    if text.is_empty() {
        return Ok(RichOutcome::Noop);
    }
    if !caret.range.is_empty() {
        delete_range(doc, engine, caret, TransactionKind::Command)?;
        engine.sync(doc);
    }
    if text.contains('\n') || text.contains('\r') {
        return insert_multiline_text(doc, engine, caret, text);
    }
    let source = doc.buffer.content();
    let mut offset = insert_offset_escaping_table_pipe(engine, &source, caret.cursor());
    // 0–3 space CommonMark indent before ATX `#`: type into the title, not
    // the orphan indent (`x # Title` would become a paragraph).
    if let Some(heading) = heading_at(engine, offset) {
        let body = heading_body_start(&source, heading);
        if offset < body {
            offset = body;
        }
    }
    // Thematic `---` / `***` / `___` / `<hr>` is a widget, not an insert
    // home: type-at-start opens a paragraph above (`x\n\n---`, not `x---` /
    // a setext heading); the close edge / leftover-below opens after.
    // Quoted keep `>`. Wrap still wraps the rule (`[---]()`).
    let mut thematic_after_newline = false;
    if let Some(plan) = thematic_break_insert_plan(engine, &source, offset) {
        match plan {
            ThematicInsert::Relocate(home) => offset = home,
            ThematicInsert::OpenAfter(at) => {
                offset = at;
                thematic_after_newline = true;
            }
            ThematicInsert::OpenAbove {
                at,
                opener,
                caret: c,
            } => {
                if !opener.is_empty() {
                    let before = caret.snapshot();
                    let after = CaretState::collapsed(at + c);
                    doc.replace_range_tx(
                        at,
                        at,
                        &opener,
                        TransactionKind::Command,
                        before,
                        after.snapshot(),
                    );
                    *caret = after;
                    engine.sync(doc);
                    return insert_text(doc, engine, caret, text);
                }
                offset = at + c;
            }
        }
    } else if let Some(ThematicInsert::OpenAfter(at)) =
        html_break_insert_plan(engine, &source, offset)
    {
        offset = at;
        thematic_after_newline = true;
    }
    // Revealed dest chrome (link `[`, HTML `<b>`, GFM alignment dashes, ATX
    // `#` / closed trailing hashes, setext underlines, leading table `|`,
    // GitHub `[!NOTE]` tags, list/quote/task/`[ref]:` prefixes, HTML-block
    // `<div>`, autolink `<>`, `$math$`, `[[wiki]]`, dest `(` / `"title"`,
    // hard-break interiors, `&amp;` / `\*` dest suffixes, inline `<br>`)
    // is not an insert home.
    // Empty wrap shares the link/HTML/prefix skip; autolink / math / wiki
    // dest openers are InsertText-only so wrap can still wrap widgets.
    // Cell-end `|` stays the End insert home (`hellox|`).
    if !thematic_after_newline {
        if let Some(home) = dest_chrome_insert_home(engine, &source, offset, false) {
            offset = home;
        }
    }
    // Opening-fence ticks / indented-code indent: pull forward into the body.
    // Closing-fence leftover-EOF must stay put so the newline-prefix path
    // can open a body line (` ```\ncode\n```x ` is glue). Use the dest-chrome
    // home (not `caret.min(offset)`): HTML-block `<div>` skip onto inner
    // must not be pulled back onto the tag (`x<div>`).
    if !needs_newline_after_frontmatter_fence(&source, engine, caret.cursor())
        && (in_raw_block(engine, offset) || in_raw_block(engine, caret.cursor()))
    {
        offset = clamp_to_raw_edit(engine, &source, offset);
    }
    if caret.range.is_empty() {
        caret.collapse_to(offset);
    }
    let raw = engine.in_raw_context(offset);
    // Last `|` at EOF is table chrome, not a cell: input rules / GFM paste
    // must open a body line (`| 1 | 2 |\n# Title`), not stay literal on the pipe.
    let in_table =
        engine.in_table(offset) && !needs_newline_after_frontmatter_fence(&source, engine, offset);
    if !raw {
        if let Some(rule) = match_input_rule_with(&source, offset, text, false, in_table) {
            // Headings / lists / quotes / fences would rewrite a GFM row;
            // splices that insert a newline or `|` would too.
            if !(in_table && input_rule_breaks_table(&rule)) {
                return apply_input_rule(doc, engine, caret, rule);
            }
        }
    }
    // Single-line clipboard/IME paste of a complete GFM block (`# Title`,
    // `- world`) must not go through `escape_text` (`\# Title`) or glue onto
    // the current list item (`- hello- world`). Typed `#` / `-` keystrokes
    // stay input-rule driven (they are not a complete block line).
    if !raw && !in_table {
        if let Some(result) = try_insert_gfm_block_paste(doc, engine, caret, text) {
            return result;
        }
    }
    // Clipboard/IME paste of markdown (`[hello](url)`, `**bold**`, a compact
    // GFM table, `---`) must stay GFM. Copy already writes source markdown;
    // escaping that paste is `\[hello](url)` / `\*\*bold\*\*` / `\---` and
    // drops the construct. Typed keystrokes (`*`, `[`, `-`) still escape.
    let mut inserted = if raw || (!in_table && !is_keystroke_insert(text)) {
        text.to_string()
    } else {
        let ctx = EscapeContext {
            in_table,
            at_line_start: implied_line_start(&source, engine, offset),
        };
        escape_text(text, ctx)
    };
    // Typora empty quotes/lists are `> ` / `- `, not `>`. InsertText at
    // that home fills in the missing marker space so typing is `> x`.
    if empty_prefix_home_needs_marker_space(engine.tree(), &source, offset, &inserted) {
        inserted.insert(0, ' ');
    }
    if thematic_after_newline {
        inserted.insert(0, '\n');
    }
    inserted.insert_str(0, frontmatter_eof_body_prefix(&source, engine, offset));
    let kind = if is_coalescable_insert(&inserted) {
        TransactionKind::Typing
    } else {
        TransactionKind::Command
    };
    let before = caret.snapshot();
    // `[hello](<>)` leftover dest: typing must replace `<>`, not insert inside.
    if !raw {
        if let Some(angle) = empty_angle_destination(&source, offset) {
            let after = CaretState::collapsed(angle.start + inserted.len());
            doc.replace_range_tx(
                angle.start,
                angle.end,
                &inserted,
                kind,
                before,
                after.snapshot(),
            );
            *caret = after;
            engine.sync(doc);
            return Ok(RichOutcome::Changed);
        }
    }
    let after = CaretState::collapsed(offset + inserted.len());
    doc.replace_range_tx(offset, offset, &inserted, kind, before, after.snapshot());
    *caret = after;
    engine.sync(doc);
    Ok(RichOutcome::Changed)
}

/// IME/clipboard paste (`InsertText` with embedded newlines). A lone `"\n"`
/// stays Enter (`SplitBlock`). Typora-ish: real source newlines (not `&#10;`);
/// paragraph soft-wrap vs `\n\n` paragraph; later lines stay source markdown
/// (ATX / list / quote / fence / table / setext / thematic / HTML / inlines)
/// instead of escaped text; list marker lines become sibling items; quote/list
/// prefixes kept for wraps; raw blocks stay inside. Single-line complete GFM
/// pastes (`# Title` with no `\n`) are rewritten onto this path by
/// `try_insert_gfm_block_paste`. Keystroke inserts still go through
/// `escape_text`.
fn insert_multiline_text(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    text: &str,
) -> Result<RichOutcome, RichError> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let source = doc.buffer.content();
    let offset = insert_offset_escaping_table_pipe(engine, &source, caret.cursor());
    if caret.range.is_empty() {
        caret.collapse_to(offset);
    }
    if engine.in_table(offset) && !needs_newline_after_frontmatter_fence(&source, engine, offset) {
        let inserted = format_table_paste(&text, &source, offset);
        return commit_inserted_text(doc, engine, caret, offset, inserted, false);
    }
    if in_raw_block(engine, offset)
        && !needs_newline_after_frontmatter_fence(&source, engine, offset)
    {
        let at = clamp_to_raw_edit(engine, &source, offset);
        let prefix = raw_block_at(engine, offset)
            .map(|block| raw_container_prefix(&source, block))
            .unwrap_or_default();
        let inserted = join_prefixed_lines(&text, &prefix);
        return commit_inserted_text(doc, engine, caret, at, inserted, false);
    }
    let wrap_prefix = paste_line_prefix(&source, engine, offset);
    let in_list = engine
        .block_at(offset)
        .is_some_and(|id| ancestor_list_item(engine, id).is_some());
    let in_quote = engine
        .block_at(offset)
        .is_some_and(|id| ancestor_is_quote(engine, id));
    let quote_pfx = if in_quote {
        quote_marker_prefix(current_line(&source, offset)).unwrap_or_else(|| "> ".to_string())
    } else {
        String::new()
    };
    let first_at_start = implied_line_start(&source, engine, offset);
    let mut inserted = String::new();
    for (i, line) in text.split('\n').enumerate() {
        let at_line_start = i == 0 && first_at_start;
        if i > 0 {
            inserted.push('\n');
        }
        if (i > 0 || at_line_start) && keep_markdown_paste_line(line, in_list) {
            inserted.push_str(&with_container_quote(line, &quote_pfx));
            continue;
        }
        if i > 0 {
            inserted.push_str(&wrap_prefix);
        }
        // Multiline clipboard is paste-as-markdown (Typora). Escaping here
        // turned compact tables (`---|---`), setext `===`, `---`, HTML, and
        // `[ref]:` into prose (`\---|---`, `\===`, `\---`, `\<div>`).
        inserted.push_str(line);
    }
    commit_inserted_text(doc, engine, caret, offset, inserted, true)
}

fn format_table_paste(text: &str, source: &str, offset: usize) -> String {
    let first_at_start = offset == 0 || source.as_bytes().get(offset - 1) == Some(&b'\n');
    let mut out = String::new();
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push_str("<br>");
        }
        out.push_str(&escape_text(
            line,
            EscapeContext {
                in_table: true,
                at_line_start: i == 0 && first_at_start,
            },
        ));
    }
    out
}

fn join_prefixed_lines(text: &str, prefix: &str) -> String {
    let mut out = String::new();
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
            out.push_str(prefix);
        }
        out.push_str(line);
    }
    out
}

/// Quote `>` (and nested `> > `) or list continuation indent, matching Enter
/// continuing the block so a paste cannot drop prefixes and split the quote.
fn paste_line_prefix(source: &str, engine: &RichEngine, offset: usize) -> String {
    let Some(leaf_id) = engine.block_at(offset) else {
        return String::new();
    };
    if let Some(item) = ancestor_list_item(engine, leaf_id) {
        return list_continuation_prefix(source, item, offset);
    }
    if ancestor_is_quote(engine, leaf_id) {
        let line = current_line(source, offset);
        return quote_marker_prefix(line).unwrap_or_else(|| "> ".to_string());
    }
    String::new()
}

fn list_continuation_prefix(source: &str, item: &Block, offset: usize) -> String {
    let line = current_line(source, offset);
    let quote = quote_prefix(line).to_string();
    let after = after_quote(line);
    if let Some(marker) = list_marker_prefix(after) {
        return format!("{quote}{}", " ".repeat(marker.len()));
    }
    let indent = after
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    if indent > 0 {
        return format!("{quote}{}", &after[..indent]);
    }
    let slice = source.get(item.source_range.clone()).unwrap_or_default();
    let first = slice.split('\n').next().unwrap_or(slice);
    let q = quote_prefix(first);
    let marker = list_marker_prefix(after_quote(first)).unwrap_or_else(|| "- ".to_string());
    format!("{q}{}", " ".repeat(marker.len()))
}

/// List items keep sibling markers; everywhere else, ATX / list / quote /
/// fence lines stay GFM instead of `\#` / `\-` / escaped ticks.
fn keep_markdown_paste_line(line: &str, in_list: bool) -> bool {
    if in_list {
        looks_like_list_item_line(line)
    } else {
        looks_like_gfm_block_start(line)
    }
}

fn looks_like_list_item_line(line: &str) -> bool {
    list_marker_prefix(after_quote(line)).is_some()
}

fn looks_like_gfm_block_start(line: &str) -> bool {
    quote_marker_prefix(line).is_some()
        || atx_marker_prefix(after_quote(line)).is_some()
        || list_marker_prefix(after_quote(line)).is_some()
        || is_fence_line(after_quote(line))
}

fn is_fence_line(line: &str) -> bool {
    let indent = line
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let ticks = rest.bytes().take_while(|b| *b == b'`').count();
    if ticks >= 3 {
        return true;
    }
    rest.bytes().take_while(|b| *b == b'~').count() >= 3
}

/// True when `text` is one typed grapheme (`*`, `[`, `-`), not a clipboard
/// paste. Multi-character InsertText is paste-as-markdown.
fn is_keystroke_insert(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if c != '\n' && c != '\r' => chars.next().is_none(),
        _ => false,
    }
}

/// Clipboard/IME `InsertText` of a complete GFM block line with no embedded
/// newline (`# Title`, `- world`). Not a typed `#` / `-` keystroke.
fn try_insert_gfm_block_paste(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    text: &str,
) -> Option<Result<RichOutcome, RichError>> {
    if !is_complete_gfm_block_paste(text) {
        return None;
    }
    let offset = caret.cursor();
    let (in_list, at_line_start, can_start) = {
        let source = doc.buffer.content();
        let in_list = engine
            .block_at(offset)
            .is_some_and(|id| ancestor_list_item(engine, id).is_some());
        let at_line_start = implied_line_start(&source, engine, offset);
        let can_start = caret_can_start_gfm_block(engine, &source, offset);
        (in_list, at_line_start, can_start)
    };
    if in_list && looks_like_list_item_line(text) {
        // Mid-item paste without a leading `\n` must still start a sibling
        // (`- hello` + `- world` → two items, not `- hello- world`).
        let paste = if at_line_start {
            text.to_string()
        } else {
            format!("\n{text}")
        };
        return Some(insert_multiline_text(doc, engine, caret, &paste));
    }
    if can_start {
        return Some(insert_multiline_text(doc, engine, caret, text));
    }
    None
}

/// True when `text` is a whole ATX / list / task / quote / fence-opener line,
/// not a typical keystroke. `# Title` has a space after the marker; a fence
/// opener is longer than two bytes; a lone `#` / `-` / `>` is typing.
fn is_complete_gfm_block_paste(text: &str) -> bool {
    looks_like_gfm_block_start(text) && gfm_block_paste_not_keystroke(text)
}

fn gfm_block_paste_not_keystroke(text: &str) -> bool {
    text.len() > 2 || gfm_marker_has_separator_space(text)
}

fn gfm_marker_has_separator_space(text: &str) -> bool {
    let body = after_quote(text);
    if let Some(prefix) = atx_marker_prefix(body) {
        return prefix.contains(' ') || prefix.contains('\t');
    }
    if let Some(prefix) = list_marker_prefix(body) {
        return prefix.contains(' ') || prefix.contains('\t');
    }
    quote_marker_prefix(text).is_some_and(|prefix| prefix.contains(' ') || prefix.contains('\t'))
}

/// Empty paragraph (Comrak gap / empty doc), line start of a paragraph, or
/// after `\n\n`. Not a table cell, fence/HTML body, or list item (list-item
/// paste uses the sibling path).
fn caret_can_start_gfm_block(engine: &RichEngine, source: &str, offset: usize) -> bool {
    if needs_newline_after_frontmatter_fence(source, engine, offset) {
        return true;
    }
    if engine.in_table(offset) || in_raw_block(engine, offset) {
        return false;
    }
    if engine
        .block_at(offset)
        .is_some_and(|id| ancestor_list_item(engine, id).is_some())
    {
        return false;
    }
    if blank_caret_gap_at(engine.tree(), offset).is_some() {
        return true;
    }
    if !implied_line_start(source, engine, offset) {
        return false;
    }
    match engine.block_at(offset).and_then(|id| engine.block(id)) {
        None => true,
        Some(block) => matches!(block.kind, BlockKind::Paragraph),
    }
}

/// Re-apply the current quote prefix without doubling `>` when the paste
/// already carries the same depth. Extra `>` in the paste stay nested.
fn with_container_quote(line: &str, quote_pfx: &str) -> String {
    let rest = strip_matching_quote_prefix(line, quote_pfx);
    if quote_pfx.is_empty() {
        rest.to_string()
    } else {
        format!("{quote_pfx}{rest}")
    }
}

fn quote_depth(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut i = bytes
        .iter()
        .take_while(|b| **b == b' ' || **b == b'\t')
        .count();
    let mut depth = 0;
    while bytes.get(i) == Some(&b'>') {
        depth += 1;
        i += 1;
        if bytes.get(i) == Some(&b' ') || bytes.get(i) == Some(&b'\t') {
            i += 1;
        }
    }
    depth
}

fn strip_matching_quote_prefix<'a>(line: &'a str, current_quote: &str) -> &'a str {
    let n = quote_depth(current_quote).min(quote_depth(line));
    if n == 0 {
        return line;
    }
    let bytes = line.as_bytes();
    let mut i = bytes
        .iter()
        .take_while(|b| **b == b' ' || **b == b'\t')
        .count();
    let mut stripped = 0;
    while stripped < n && bytes.get(i) == Some(&b'>') {
        i += 1;
        if bytes.get(i) == Some(&b' ') || bytes.get(i) == Some(&b'\t') {
            i += 1;
        }
        stripped += 1;
    }
    &line[i.min(line.len())..]
}

fn commit_inserted_text(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    offset: usize,
    mut inserted: String,
    apply_homes_and_angles: bool,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    if apply_homes_and_angles
        && empty_prefix_home_needs_marker_space(engine.tree(), &source, offset, &inserted)
    {
        inserted.insert(0, ' ');
    }
    if needs_newline_after_frontmatter_fence(&source, engine, offset) {
        inserted.insert(0, '\n');
    }
    let kind = if is_coalescable_insert(&inserted) {
        TransactionKind::Typing
    } else {
        TransactionKind::Command
    };
    let before = caret.snapshot();
    if apply_homes_and_angles {
        if let Some(angle) = empty_angle_destination(&source, offset) {
            let after = CaretState::collapsed(angle.start + inserted.len());
            doc.replace_range_tx(
                angle.start,
                angle.end,
                &inserted,
                kind,
                before,
                after.snapshot(),
            );
            *caret = after;
            engine.sync(doc);
            return Ok(RichOutcome::Changed);
        }
    }
    let after = CaretState::collapsed(offset + inserted.len());
    doc.replace_range_tx(offset, offset, &inserted, kind, before, after.snapshot());
    *caret = after;
    engine.sync(doc);
    Ok(RichOutcome::Changed)
}

fn apply_input_rule(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    rule: InputRule,
) -> Result<RichOutcome, RichError> {
    match rule {
        InputRule::InsertRaw(text) => {
            let offset = caret.cursor();
            let source = doc.buffer.content();
            let prefix = frontmatter_eof_body_prefix(&source, engine, offset);
            let inserted = format!("{prefix}{text}");
            let kind = if is_coalescable_insert(&inserted) {
                TransactionKind::Typing
            } else {
                TransactionKind::Command
            };
            let before = caret.snapshot();
            let after = CaretState::collapsed(offset + inserted.len());
            doc.replace_range_tx(offset, offset, &inserted, kind, before, after.snapshot());
            *caret = after;
            engine.sync(doc);
            Ok(RichOutcome::Changed)
        }
        InputRule::Replace {
            range,
            insert,
            caret: new_caret,
        } => {
            let offset = caret.cursor();
            let source = doc.buffer.content();
            if needs_newline_after_frontmatter_fence(&source, engine, offset) {
                // Do not replace the closing fence / atomic chrome line.
                let inserted = format!("\n{insert}");
                let before = caret.snapshot();
                let after = CaretState::collapsed(offset + inserted.len());
                doc.replace_range_tx(
                    offset,
                    offset,
                    &inserted,
                    TransactionKind::Command,
                    before,
                    after.snapshot(),
                );
                *caret = after;
                engine.sync(doc);
                caret.clamp(doc.buffer.len_bytes());
                return Ok(RichOutcome::Changed);
            }
            let absorbed = doc.peel_typing_range(range.clone());
            let before = absorbed.unwrap_or_else(|| caret.snapshot());
            let (start, end) = if absorbed.is_some() {
                (range.start, range.start)
            } else {
                (
                    range.start.min(doc.buffer.len_bytes()),
                    range.end.min(doc.buffer.len_bytes()),
                )
            };
            let after = CaretState::collapsed(new_caret.min(start.saturating_add(insert.len())));
            doc.replace_range_tx(
                start,
                end,
                &insert,
                TransactionKind::Command,
                before,
                after.snapshot(),
            );
            *caret = after;
            engine.sync(doc);
            caret.clamp(doc.buffer.len_bytes());
            Ok(RichOutcome::Changed)
        }
    }
}

fn is_coalescable_insert(text: &str) -> bool {
    crate::undo::is_typing_burst(text)
}

/// Empty `>` / `-` / `1.` (no trailing space) still host a caret after the
/// marker. InsertText there must become `> x` / `- x`, not `>x`.
///
/// Only real quote/list nodes (not a `-` / `*` paragraph waiting for an
/// input-rule space or a third `-` for `---`).
fn empty_prefix_home_needs_marker_space(
    tree: &RichTree,
    source: &str,
    offset: usize,
    inserted: &str,
) -> bool {
    if inserted.is_empty() || inserted.starts_with([' ', '\t']) {
        return false;
    }
    tree.empty_prefix_homes.iter().any(|blank| {
        if blank.home != offset {
            return false;
        }
        let line = source.get(blank.line.clone()).unwrap_or("");
        let last = line.as_bytes().last();
        if line.is_empty() || last == Some(&b' ') || last == Some(&b'\t') {
            return false;
        }
        // Unquoted `-` / `*` / `+` are paragraphs or empty items waiting for
        // an input-rule space (or a third `-` for `---`, or `*hi*` italic).
        // Only fill the Typora marker space on a real quote (and quoted lists).
        if quote_marker_prefix(line).is_some() && deepest_is_quote_or_list(&tree.blocks, blank.home)
        {
            return true;
        }
        // Empty `[^1]:` (no trailing space) still types as `[^1]: x`.
        let after = after_quote(line);
        !footnote_def_marker_on_line(after).is_empty()
            && deepest_is_footnote_definition(&tree.blocks, blank.home)
    })
}

fn deepest_is_footnote_definition(blocks: &[Block], byte: usize) -> bool {
    ancestor_kind_at(blocks, byte, |b| {
        matches!(b.kind, BlockKind::FootnoteDefinition { .. })
    })
}

fn ancestor_kind_at(blocks: &[Block], byte: usize, pred: impl Fn(&Block) -> bool) -> bool {
    fn walk(blocks: &[Block], byte: usize, pred: &impl Fn(&Block) -> bool, hit: &mut bool) {
        for b in blocks {
            if b.source_range.start <= byte && byte <= b.source_range.end {
                if pred(b) {
                    *hit = true;
                }
                walk(&b.children, byte, pred, hit);
            }
        }
    }
    let mut hit = false;
    walk(blocks, byte, &pred, &mut hit);
    hit
}

fn deepest_is_quote_or_list(blocks: &[Block], byte: usize) -> bool {
    let mut best = None;
    fn walk<'a>(blocks: &'a [Block], byte: usize, best: &mut Option<&'a Block>) {
        for b in blocks {
            if b.source_range.start <= byte && byte <= b.source_range.end {
                *best = Some(b);
                walk(&b.children, byte, best);
            }
        }
    }
    walk(blocks, byte, &mut best);
    best.is_some_and(|b| {
        matches!(
            b.kind,
            BlockKind::BlockQuote
                | BlockKind::Alert { .. }
                | BlockKind::BulletList { .. }
                | BlockKind::OrderedList { .. }
                | BlockKind::ListItem { .. }
        )
    })
}

/// Body caret/selection must sit at or after the closing frontmatter fence.
fn clamp_caret_out_of_frontmatter(engine: &RichEngine, caret: &mut CaretState, source: &str) {
    let tree_end = frontmatter_body_start(engine.tree());
    let parsed_end = crate::parse_frontmatter(source)
        .map(|info| info.end_byte)
        .unwrap_or(0);
    let end = tree_end.max(parsed_end).min(source.len());
    if end == 0 || caret.range.start >= end {
        return;
    }
    if caret.range.end <= end {
        caret.collapse_to(end);
        return;
    }
    caret.range.start = end;
}

/// Closing frontmatter `---` / `...`, thematic `---` / HTML `<hr>`, fence ticks
/// (close _or_ last-line opener / info-string), setext `===` / `---`
/// underlines, closed ATX trailing `#`, an HTML-block close tag (including
/// a single-line `<pre>…</pre>` / `<div>…</div>`), a last-block HTML widget
/// paragraph (`<svg>…</svg>` / `<img>` / `<br>`), a `[TOC]` / `[[toc]]`
/// marker, or a GFM table's last `|` at EOF with no following newline: body
/// splices must not glue onto that chrome (`---x`, `===x`, `===# Title`,
/// `# Title #x`, `</div>x`, `<pre>…</pre>x`, `[TOC]x`, `| 1 | 2 |x`,
/// `` ```x ``, `` ```rustx ``, `<svg></svg>x`). A trailing newline after
/// the chrome, or a body after it, is already a valid insert offset.
/// Comrak may omit a FrontMatter node for empty `---\n---`;
/// `parse_frontmatter` still sees it.
fn needs_newline_after_frontmatter_fence(source: &str, engine: &RichEngine, offset: usize) -> bool {
    if offset != source.len() || source.ends_with('\n') {
        return false;
    }
    let tree_end = frontmatter_body_start(engine.tree());
    let parsed_end = crate::parse_frontmatter(source)
        .map(|info| info.end_byte)
        .unwrap_or(0);
    let fm_end = tree_end.max(parsed_end).min(source.len());
    if fm_end > 0 && offset == fm_end {
        return true;
    }
    atomic_chrome_at_eof(source, engine)
}

fn atomic_chrome_at_eof(source: &str, engine: &RichEngine) -> bool {
    fn walk(blocks: &[Block], source: &str) -> bool {
        for b in blocks {
            if walk(&b.children, source) {
                return true;
            }
            // Trailing spaces after the last `|` sit past comrak's table
            // sourcepos; still treat that as EOF chrome (`| 1 | 2 |  x`).
            if table_closing_pipe_at_eof(source, b) {
                return true;
            }
            if b.source_range.end < source.len() {
                continue;
            }
            if thematic_break_range(b).is_some() || html_block_atomic_range(b).is_some() {
                return true;
            }
            if closing_fence_line_at_eof(source, b)
                || setext_underline_at_eof(source, b)
                || closed_atx_trailing_hashes_at_eof(source, b)
                || html_close_tag_line_at_eof(source, b)
                || html_widget_line_at_eof(source, b)
                || toc_marker_at_eof(source, b)
            {
                return true;
            }
        }
        false
    }
    walk(&engine.tree().blocks, source)
}

/// Last line of a GFM table is a row (or alignment row) that ends with `|`
/// (quoted `> | 1 | 2 |` too). EOF on that chrome must not type `| 1 | 2 |x`.
/// Pipeless last cells (`1 | 2`) are body, not closing-pipe chrome.
fn table_closing_pipe_at_eof(source: &str, block: &Block) -> bool {
    if !matches!(block.kind, BlockKind::Table { .. }) {
        return false;
    }
    if block.source_range.end < source.len() {
        let rest = &source[block.source_range.end..];
        if rest.contains('\n') || !rest.bytes().all(|b| b == b' ' || b == b'\t') {
            return false;
        }
    }
    let line_start = source.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = after_quote(&source[line_start..]).trim_end();
    !line.is_empty() && is_unescaped_pipe(line, line.len() - 1)
}

fn closing_fence_line_at_eof(source: &str, block: &Block) -> bool {
    let BlockKind::CodeBlock { fence: Some(_), .. } = &block.kind else {
        return false;
    };
    if block.source_range.end < source.len() {
        return false;
    }
    let line_start = source.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = &source[line_start..];
    // Opening info-string (` ```rust ` as the last line) is language-chip
    // chrome, same as closing ticks: do not glue ` ```rustx `.
    is_fence_line(line) || is_fence_line(after_quote(line))
}

/// Last line of a setext heading is the `===` / `---` underline (quoted
/// `> ===` too). EOF on that chrome must not type `===x` / `===# Title`.
fn setext_underline_at_eof(source: &str, block: &Block) -> bool {
    let BlockKind::Heading {
        style: HeadingStyle::Setext,
        ..
    } = block.kind
    else {
        return false;
    };
    if block.source_range.end < source.len() {
        return false;
    }
    let line_start = source.rfind('\n').map(|i| i + 1).unwrap_or(0);
    if line_start <= block.source_range.start {
        return false;
    }
    let line = &source[line_start..];
    is_setext_underline(after_quote(line)) || is_setext_underline(line)
}

/// Closed ATX trailing ` #` at EOF (`# Title #` with no following newline).
/// Open ATX `# Title` is not chrome — typing there extends the title.
fn closed_atx_trailing_hashes_at_eof(source: &str, block: &Block) -> bool {
    let BlockKind::Heading {
        style: HeadingStyle::Atx,
        ..
    } = block.kind
    else {
        return false;
    };
    if block.source_range.end < source.len() {
        return false;
    }
    // Quoted `# Title #` starts after `>`; the last line still begins at 0.
    let line_start = source.rfind('\n').map(|i| i + 1).unwrap_or(0);
    atx_line_has_closing_hashes(after_quote(&source[line_start..]))
}

fn atx_line_has_closing_hashes(line: &str) -> bool {
    let Some(prefix) = atx_marker_prefix(line) else {
        return false;
    };
    let rest = &line[prefix.len()..];
    strip_closing_atx(rest) != rest
}

/// Last line of a flow HTML block is a close tag (`</div>` / `</p>`), or a
/// single-line block that ends with one (`<pre>…</pre>` / `<div>hello</div>`).
/// Atomic widgets (`<hr>` / `<img>` / `<br>`) already match above.
fn html_close_tag_line_at_eof(source: &str, block: &Block) -> bool {
    if !matches!(block.kind, BlockKind::Opaque { .. }) {
        return false;
    }
    if block.source_range.end < source.len() || html_block_atomic_range(block).is_some() {
        return false;
    }
    let line_start = source.rfind('\n').map(|i| i + 1).unwrap_or(0);
    if line_start >= block.source_range.end {
        return false;
    }
    is_html_close_tag_line(after_quote(&source[line_start..]))
}

/// Last-block paragraph/list/quote whose last line is only an HTML widget
/// (`<svg>…</svg>`, `<img>`, `<br>`). Comrak treats complete `<svg>` as
/// inline HTML in a paragraph, so it never hits `html_block_atomic_range`.
/// Mixed `hello <svg></svg>` is still paragraph body (like `hello ![alt](url)`).
fn html_widget_line_at_eof(source: &str, block: &Block) -> bool {
    if matches!(
        block.kind,
        BlockKind::CodeBlock { .. } | BlockKind::Opaque { .. }
    ) {
        return false;
    }
    if block.source_range.end < source.len() {
        return false;
    }
    let line_start = source.rfind('\n').map(|i| i + 1).unwrap_or(0);
    if line_start >= block.source_range.end {
        return false;
    }
    let line = after_quote(&source[line_start..]);
    let marker = list_marker_on_line(line);
    let rest = if marker.is_empty() {
        line.trim()
    } else {
        line[marker.len()..].trim()
    };
    crate::html_visual::html_inline_image(rest).is_some()
        || crate::html_visual::html_inline_break(rest)
        || tagfilter_inline_covers_last_line(source, block, line_start)
}

fn tagfilter_inline_covers_last_line(source: &str, block: &Block, line_start: usize) -> bool {
    let line = after_quote(&source[line_start..]);
    let quote_bytes = source[line_start..].len() - line.len();
    let marker = list_marker_on_line(line);
    let after_marker = &line[marker.len()..];
    let pad = after_marker.len() - after_marker.trim_start().len();
    let content_off = line_start + quote_bytes + marker.len() + pad;
    crate::rich::engine::tagfilter_inline_widget_ranges(block)
        .iter()
        .any(|r| r.start <= content_off && r.end >= block.source_range.end)
}

fn is_html_close_tag_line(line: &str) -> bool {
    let t = line.trim();
    if t.len() > 3 && t.starts_with("</") && t.ends_with('>') {
        return true;
    }
    line_ends_with_html_close_tag(t)
}

/// `<pre>code</pre>` / `<div>hello</div>` / `hello</div>` — the line ends on
/// a close tag, so EOF typing must not glue `x` onto `>`.
fn line_ends_with_html_close_tag(t: &str) -> bool {
    let Some(close) = t.rfind("</") else {
        return false;
    };
    let rest = &t[close..];
    if rest.len() < 4 || !rest.ends_with('>') {
        return false;
    }
    let name = rest[2..rest.len() - 1].trim();
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `[TOC]` / `[[toc]]` at EOF is marker chrome: typing must not glue `[TOC]x`.
/// Quoted `> [TOC]` may still be a paragraph inside the quote, not `Toc`.
fn toc_marker_at_eof(source: &str, block: &Block) -> bool {
    if block.source_range.end < source.len() {
        return false;
    }
    if matches!(block.kind, BlockKind::Toc { .. }) {
        return true;
    }
    if !matches!(
        block.kind,
        BlockKind::Paragraph | BlockKind::BlockQuote | BlockKind::Alert { .. }
    ) {
        return false;
    }
    let line_start = source.rfind('\n').map(|i| i + 1).unwrap_or(0);
    if line_start >= block.source_range.end {
        return false;
    }
    is_toc_marker(after_quote(&source[line_start..]))
}

/// Physical line start, or EOF on closing fence / atomic chrome that will
/// get a body newline first (paste/input then sit on that new line).
fn implied_line_start(source: &str, engine: &RichEngine, offset: usize) -> bool {
    offset == 0
        || source.as_bytes().get(offset - 1) == Some(&b'\n')
        || needs_newline_after_frontmatter_fence(source, engine, offset)
}

/// Prefix a body splice at EOF on closing fence / atomic chrome that has no
/// following newline. InsertText already prepends this; wrap and block
/// opens (heading / list / quote) must too.
fn frontmatter_eof_body_prefix(source: &str, engine: &RichEngine, offset: usize) -> &'static str {
    if !needs_newline_after_frontmatter_fence(source, engine, offset) {
        return "";
    }
    // Type-6 `<hr>` / type-7 `<br>` continue until a blank line
    // (`<hr>\nx` / `<br>\nx` swallow `x`).
    if html_void_block_at_eof(engine) {
        "\n\n"
    } else {
        "\n"
    }
}

fn html_void_block_at_eof(engine: &RichEngine) -> bool {
    fn last_void(blocks: &[Block]) -> bool {
        let Some(last) = blocks.last() else {
            return false;
        };
        if last_void(&last.children) {
            return true;
        }
        html_block_break_range(last).is_some()
            || (matches!(last.kind, BlockKind::Opaque { .. })
                && thematic_break_range(last).is_some())
    }
    last_void(&engine.tree().blocks)
}

/// Empty gap (frontmatter-only, trailing blank, empty doc) or EOF sitting on
/// atomic chrome: do not rewrite the chrome; open a new body block after it.
fn should_open_new_block_at_caret(source: &str, engine: &RichEngine, caret: &CaretState) -> bool {
    if !caret.range.is_empty() {
        return false;
    }
    let offset = caret.cursor();
    needs_newline_after_frontmatter_fence(source, engine, offset)
        || engine.top_level_at(offset).is_none()
}

fn insert_block_open_at_caret(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    marker: &str,
) -> Result<RichOutcome, RichError> {
    if marker.is_empty() {
        let source = doc.buffer.content();
        let offset = caret.cursor();
        let prefix = frontmatter_eof_body_prefix(&source, engine, offset);
        if prefix.is_empty() {
            return Ok(RichOutcome::Noop);
        }
        splice(doc, caret, offset, offset, prefix, TransactionKind::Command);
        caret.collapse_to(offset + prefix.len());
        engine.sync(doc);
        caret.clamp(doc.buffer.len_bytes());
        return Ok(RichOutcome::Changed);
    }
    let source = doc.buffer.content();
    let offset = caret.cursor();
    let prefix = frontmatter_eof_body_prefix(&source, engine, offset);
    let text = format!("{prefix}{marker}");
    splice(doc, caret, offset, offset, &text, TransactionKind::Command);
    caret.collapse_to(offset + text.len());
    engine.sync(doc);
    caret.clamp(doc.buffer.len_bytes());
    Ok(RichOutcome::Changed)
}

fn backspace(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    if !caret.range.is_empty() {
        return delete_range(doc, engine, caret, TransactionKind::Command);
    }
    let source = doc.buffer.content();
    let to = caret.cursor();
    let fm_end = frontmatter_body_start(engine.tree());
    if fm_end > 0 && to <= fm_end {
        return Ok(RichOutcome::Noop);
    }
    if to == 0 {
        if at_heading_body_start(&source, engine, to) {
            return convert_heading_to_paragraph(doc, engine, caret);
        }
        if at_list_item_body_start(&source, engine, to) {
            return outdent_current_list_line(doc, engine, caret);
        }
        if at_definition_details_body_start(&source, engine, to) {
            return strip_definition_details_marker(doc, engine, caret);
        }
        if at_footnote_def_body_start(&source, engine, to) {
            return strip_footnote_def_marker(doc, engine, caret);
        }
        return Ok(RichOutcome::Noop);
    }
    // Typora: Backspace at the first body byte of a fenced code / HTML block
    // must not nibble opening ticks, list/quote prefixes, or the previous
    // block. Later body lines join without eating `>` / list indent.
    if let Some(outcome) = backspace_in_raw_block(doc, engine, caret, &source, to)? {
        return Ok(outcome);
    }
    // Typora: Backspace at the first visible character / heading-body start
    // strips `#` / setext underline (same idea as list-marker strip). Inner
    // heading chrome is stripped before a surrounding list marker, so
    // `- # Title` becomes `- Title` rather than `# Title`.
    if at_heading_body_start(&source, engine, to) {
        match convert_heading_to_paragraph(doc, engine, caret)? {
            RichOutcome::Changed => return Ok(RichOutcome::Changed),
            RichOutcome::Noop => {}
        }
    }
    // Typora: Backspace at the start of a list item (first visible character /
    // start of the item body) removes the list marker rather than a grapheme.
    // Quoted lists keep their `>` prefixes (same as outermost OutdentList).
    if at_list_item_body_start(&source, engine, to) {
        match outdent_current_list_line(doc, engine, caret)? {
            RichOutcome::Changed => return Ok(RichOutcome::Changed),
            RichOutcome::Noop => {}
        }
    }
    // Typora: Backspace at the start of definition details strips `: `
    // (same idea as list-marker strip) instead of joining into `Termdetails`.
    if at_definition_details_body_start(&source, engine, to) {
        match strip_definition_details_marker(doc, engine, caret)? {
            RichOutcome::Changed => return Ok(RichOutcome::Changed),
            RichOutcome::Noop => {}
        }
    }
    // Typora: Backspace at the start of a footnote definition strips `[^1]: `
    // instead of deleting the previous `[^1]` ref (atomic widget).
    if at_footnote_def_body_start(&source, engine, to) {
        match strip_footnote_def_marker(doc, engine, caret)? {
            RichOutcome::Changed => return Ok(RichOutcome::Changed),
            RichOutcome::Noop => {}
        }
    }
    let from = engine.prev_caret(&source, to);
    let grapheme_end = engine.next_caret(&source, from);
    // Prefer deleting only the previous visible grapheme, not delimiter gaps.
    let (del_start, del_end) = if from < grapheme_end && grapheme_end <= to {
        (from, grapheme_end)
    } else {
        (from, to)
    };
    if del_start >= del_end {
        return Ok(RichOutcome::Noop);
    }
    let range = prepare_delete_range(engine, &source, del_start..del_end);
    if range.start >= range.end {
        return Ok(RichOutcome::Noop);
    }
    caret.range = range;
    caret.reversed = true;
    delete_range(doc, engine, caret, TransactionKind::DeleteBack)
}

fn delete_forward(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    if !caret.range.is_empty() {
        return delete_range(doc, engine, caret, TransactionKind::Command);
    }
    let source = doc.buffer.content();
    let from = caret.cursor();
    if from >= source.len() {
        return Ok(RichOutcome::Noop);
    }
    // Typora: Delete at the end of a fence / HTML body must not nibble
    // closing ticks or `>` (pair of Backspace-at-start). Mid-body Delete
    // still removes one grapheme, clamped to the editable range.
    if let Some(outcome) = delete_forward_in_raw_block(doc, engine, caret, &source, from)? {
        return Ok(outcome);
    }
    let to = engine.next_caret(&source, from);
    if to <= from {
        return Ok(RichOutcome::Noop);
    }
    let range = prepare_delete_range(engine, &source, from..to);
    if range.start >= range.end {
        return Ok(RichOutcome::Noop);
    }
    caret.range = range;
    caret.reversed = false;
    delete_range(doc, engine, caret, TransactionKind::Command)
}

#[derive(Clone, Copy)]
enum DeleteBound {
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
}

/// Option/Ctrl word-delete and Cmd line-delete. A non-empty selection is
/// removed like Backspace. Collapsed carets stay inside a table cell, a
/// fence/HTML body, and out of YAML. Word-delete-left at the start of a
/// heading/list/quote matches grapheme Backspace (convert / strip marker /
/// outdent) instead of splicing the previous block. Word-delete-right at a
/// paragraph/heading/list/quote end does not eat the next block's `# ` /
/// `- ` / `>` (no-op, or join the next *paragraph* only).
fn delete_to_bound(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    bound: DeleteBound,
) -> Result<RichOutcome, RichError> {
    if !caret.range.is_empty() {
        return delete_range(doc, engine, caret, TransactionKind::Command);
    }
    if matches!(bound, DeleteBound::WordLeft) {
        match word_delete_left_at_block_start(doc, engine, caret)? {
            RichOutcome::Changed => return Ok(RichOutcome::Changed),
            RichOutcome::Noop => {}
        }
    }
    let source = doc.buffer.content();
    let cursor = caret.cursor();
    let target = match bound {
        DeleteBound::WordLeft => engine.prev_word_caret(&source, cursor),
        DeleteBound::WordRight => engine.next_word_caret(&source, cursor),
        DeleteBound::LineStart => engine.line_start_caret(&source, cursor),
        DeleteBound::LineEnd => engine.line_end_caret(&source, cursor),
    };
    let window = delete_edit_window(engine, &source, cursor);
    let cursor = cursor.clamp(window.start, window.end);
    let target = target.clamp(window.start, window.end);
    let (start, end, reversed) = if target < cursor {
        (target, cursor, true)
    } else {
        (cursor, target, false)
    };
    if start >= end {
        return Ok(RichOutcome::Noop);
    }
    let range = prepare_delete_range(engine, &source, start..end);
    if range.start >= range.end {
        return Ok(RichOutcome::Noop);
    }
    caret.range = range;
    caret.reversed = reversed;
    delete_range(doc, engine, caret, TransactionKind::Command)
}

/// Byte range a collapsed word/line delete may cover from `cursor`.
fn delete_edit_window(engine: &RichEngine, source: &str, cursor: usize) -> Range<usize> {
    let fm = frontmatter_body_start(engine.tree()).min(source.len());
    let mut lo = fm;
    let mut hi = source.len();
    if let Some(cell) = cell_edit_range_near(engine, source, cursor) {
        return cell.start.max(lo)..cell.end.min(hi);
    }
    if let Some(block) = raw_block_at(engine, cursor) {
        let body = raw_body_range(block, source);
        lo = lo.max(body.start);
        hi = hi.min(body.end);
        let prefix = raw_container_prefix(source, block);
        let first_line_start = line_start(source, body.start);
        let first_content = (first_line_start + prefix.len()).clamp(lo, hi);
        lo = lo.max(first_content);
        if !prefix.is_empty() {
            let at = cursor.clamp(lo, hi);
            let line_s = line_start(source, at);
            let line_e = line_end_exclusive(source, at);
            let content = (line_s + prefix.len()).clamp(lo, hi);
            lo = lo.max(content);
            hi = hi.min(line_e.max(content));
        }
        return if lo > hi { hi..hi } else { lo..hi };
    }
    // Headings/lists/quotes are not raw: still refuse to delete `# ` / `- ` /
    // `>` of this or a neighbor block. Interior word-delete stays in the
    // body. At a paragraph start, joining the previous *paragraph* is OK;
    // at a paragraph end, joining the next *paragraph* is OK. Heading/list/
    // quote/alert chrome is never stolen.
    let body = editable_body_start(engine, source, cursor);
    if cursor > body {
        lo = lo.max(body);
    } else if let Some(prev) = previous_plain_paragraph_body_start(engine, source, cursor) {
        lo = lo.max(prev);
    } else {
        lo = lo.max(body);
    }
    let body_end = editable_body_end(engine, source, cursor);
    if cursor < body_end {
        hi = hi.min(body_end);
    } else if is_plain_paragraph_leaf(engine, cursor) {
        if let Some(next) = next_plain_paragraph_body_end(engine, source, cursor) {
            hi = hi.min(next);
        } else {
            hi = hi.min(body_end);
        }
    } else {
        hi = hi.min(body_end);
    }
    if lo > hi {
        hi..hi
    } else {
        lo..hi
    }
}

/// Option-Backspace at the first visible body character of a heading, list
/// item, or quoted paragraph: same structural edit as grapheme Backspace,
/// not a word delete into the previous block.
fn word_delete_left_at_block_start(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let cursor = caret.cursor();
    let fm_end = frontmatter_body_start(engine.tree());
    if fm_end > 0 && cursor <= fm_end {
        return Ok(RichOutcome::Noop);
    }
    if at_heading_body_start(&source, engine, cursor) {
        match convert_heading_to_paragraph(doc, engine, caret)? {
            RichOutcome::Changed => return Ok(RichOutcome::Changed),
            RichOutcome::Noop => {}
        }
    }
    if at_list_item_body_start(&doc.buffer.content(), engine, caret.cursor()) {
        match outdent_current_list_line(doc, engine, caret)? {
            RichOutcome::Changed => return Ok(RichOutcome::Changed),
            RichOutcome::Noop => {}
        }
    }
    if at_definition_details_body_start(&doc.buffer.content(), engine, caret.cursor()) {
        match strip_definition_details_marker(doc, engine, caret)? {
            RichOutcome::Changed => return Ok(RichOutcome::Changed),
            RichOutcome::Noop => {}
        }
    }
    if at_footnote_def_body_start(&doc.buffer.content(), engine, caret.cursor()) {
        match strip_footnote_def_marker(doc, engine, caret)? {
            RichOutcome::Changed => return Ok(RichOutcome::Changed),
            RichOutcome::Noop => {}
        }
    }
    if at_quote_body_start(&doc.buffer.content(), engine, caret.cursor()) {
        match outdent_current_quote_line(doc, engine, caret)? {
            RichOutcome::Changed => return Ok(RichOutcome::Changed),
            RichOutcome::Noop => {}
        }
    }
    Ok(RichOutcome::Noop)
}

/// First visible body byte of the leaf at `offset` (after `# ` / `- ` / `>`).
fn editable_body_start(engine: &RichEngine, source: &str, offset: usize) -> usize {
    if let Some(heading) = heading_at(engine, offset) {
        return heading_body_start(source, heading);
    }
    if let Some(details) = ancestor_definition_details(engine, offset) {
        return definition_details_body_start(source, details);
    }
    if let Some(def) = ancestor_footnote_definition(engine, offset) {
        return footnote_def_body_start(source, def);
    }
    let Some(id) = engine.block_at(offset) else {
        return offset;
    };
    let Some(block) = engine.block(id) else {
        return offset;
    };
    let first = line_start(source, block.source_range.start);
    let line = current_line(source, first);
    let quote = quote_prefix(line).len();
    let marker = list_marker_prefix(after_quote(line))
        .map(|p| p.len())
        .unwrap_or(0);
    first + quote + marker
}

/// Last visible body byte of the leaf at `offset` (before the next block's
/// `# ` / `- ` / `>` / `[!NOTE]` chrome).
fn editable_body_end(engine: &RichEngine, source: &str, offset: usize) -> usize {
    let Some(probe) = probe_leaf_offset(engine, offset) else {
        return offset;
    };
    if let Some(heading) = heading_at(engine, probe) {
        return last_visible_body_end(heading)
            .unwrap_or_else(|| heading_body_start(source, heading));
    }
    let Some(id) = engine.block_at(probe) else {
        return offset;
    };
    let Some(block) = engine.block(id) else {
        return offset;
    };
    last_visible_body_end(block).unwrap_or_else(|| editable_body_start(engine, source, probe))
}

fn last_visible_body_end(block: &Block) -> Option<usize> {
    let mut end = None;
    fn consider(block: &Block, end: &mut Option<usize>) {
        for inline in &block.inlines {
            if let Inline::OpaqueInline { raw, .. } = inline {
                if crate::html_visual::opaque_inline_is_caret_chrome(raw) {
                    continue;
                }
            }
            let e = inline.source_range().end;
            *end = Some(end.map_or(e, |cur| cur.max(e)));
        }
        for child in &block.children {
            consider(child, end);
        }
    }
    consider(block, &mut end);
    end
}

/// Offset inside a leaf, or the previous byte when `offset` sits in a
/// Comrak-less gap (separator / trailing blank).
fn probe_leaf_offset(engine: &RichEngine, offset: usize) -> Option<usize> {
    if engine.block_at(offset).is_some() {
        Some(offset)
    } else if offset > 0 && engine.block_at(offset - 1).is_some() {
        Some(offset - 1)
    } else {
        None
    }
}

/// Unquoted, unlisted paragraph (not a heading, quote, alert, table, or raw).
fn is_plain_paragraph_leaf(engine: &RichEngine, offset: usize) -> bool {
    let Some(probe) = probe_leaf_offset(engine, offset) else {
        return false;
    };
    if engine.in_table(probe) || engine.in_raw_context(probe) {
        return false;
    }
    if heading_at(engine, probe).is_some() {
        return false;
    }
    let Some(id) = engine.block_at(probe) else {
        return false;
    };
    if ancestor_list_item(engine, id).is_some() || ancestor_is_quote(engine, id) {
        return false;
    }
    matches!(
        engine.block(id).map(|b| &b.kind),
        Some(BlockKind::Paragraph)
    )
}

/// Body start of the previous leaf when it is an unquoted, unlisted
/// paragraph (Typora: Option-Backspace at a paragraph start may join it).
/// Refuses when the caret is already in a heading/list/quote/alert/raw so
/// a previous paragraph is not spliced into that chrome (`> [!NOTE]`).
fn previous_plain_paragraph_body_start(
    engine: &RichEngine,
    source: &str,
    cursor: usize,
) -> Option<usize> {
    if !is_plain_paragraph_leaf(engine, cursor) {
        return None;
    }
    let mut i = cursor;
    while i > 0 {
        let b = source.as_bytes()[i - 1];
        if matches!(b, b'\n' | b' ' | b'\t') {
            i -= 1;
            continue;
        }
        break;
    }
    if i == 0 {
        return None;
    }
    let prev = i - 1;
    if !is_plain_paragraph_leaf(engine, prev) {
        return None;
    }
    Some(editable_body_start(engine, source, prev))
}

/// Body end of the next leaf when it is an unquoted, unlisted paragraph
/// (Typora: Option-Delete at a paragraph end may join it). Heading/list/
/// quote/alert chrome is never part of this window.
fn next_plain_paragraph_body_end(
    engine: &RichEngine,
    source: &str,
    cursor: usize,
) -> Option<usize> {
    let mut i = cursor;
    let bytes = source.as_bytes();
    while i < bytes.len() && matches!(bytes[i], b'\n' | b' ' | b'\t') {
        i += 1;
    }
    if i >= source.len() || !is_plain_paragraph_leaf(engine, i) {
        return None;
    }
    Some(editable_body_end(engine, source, i))
}

/// Drop `[` / `](url)` / `**` / ticks / `$math$` / `[[wiki]]` / `:emoji:` /
/// HTML phrasing tags from a delete range so Backspace at the start of a
/// link label (or Delete at the end) does not nibble dest chrome.
fn trim_inline_chrome(engine: &RichEngine, source: &str, mut range: Range<usize>) -> Range<usize> {
    while range.start < range.end && engine.byte_is_inline_chrome(source, range.start) {
        range.start += 1;
    }
    while range.end > range.start && engine.byte_is_inline_chrome(source, range.end - 1) {
        range.end -= 1;
    }
    range
}

/// Trim dest chrome, unwrap empty marks, then grow onto overlapping atomic
/// widgets (`<br>`, hard break `  \n` / `\\\n`, `<img>`, `[^1]`, rules) so
/// Delete cannot nibble them.
/// All-chrome ranges (`](url)`, autolink `>`) stay empty (Noop) unless they
/// overlap a widget.
fn prepare_delete_range(engine: &RichEngine, source: &str, original: Range<usize>) -> Range<usize> {
    let original = expand_range_over_escaped_table_pipe(engine, source, original);
    if !engine.range_is_inside_link_dest(source, original.clone()) {
        let expanded = engine.expand_atomic_widget_range(original.clone());
        if expanded.start < original.start
            || expanded.end > original.end
            || engine.is_exact_atomic_widget(&expanded)
        {
            return expanded;
        }
    }
    let mut range = trim_inline_chrome(engine, source, original);
    extend_empty_mark_wrappers(source, engine, &mut range);
    if range.start >= range.end {
        return range;
    }
    if engine.range_is_inside_link_dest(source, range.clone()) {
        return range;
    }
    let grown = engine.expand_atomic_widget_range(range.clone());
    if grown.start < range.start || grown.end > range.end {
        grown
    } else {
        range
    }
}

/// If a deletion empties a marked run, swallow the surrounding delimiters too.
fn extend_empty_mark_wrappers(source: &str, engine: &RichEngine, range: &mut Range<usize>) {
    let Some(id) = engine.block_at(range.start) else {
        return;
    };
    let Some(block) = engine.block(id) else {
        return;
    };
    // Fenced-code inlines carry CODE marks; swallowing surrounding backticks
    // would delete the fence when the last body character is removed.
    if matches!(
        block.kind,
        BlockKind::CodeBlock { .. } | BlockKind::Opaque { .. }
    ) {
        return;
    }
    for inline in &block.inlines {
        let Inline::Run {
            source_range,
            marks,
            text,
            ..
        } = inline
        else {
            continue;
        };
        if marks.is_empty() {
            continue;
        }
        let overlap_start = range.start.max(source_range.start);
        let overlap_end = range.end.min(source_range.end);
        if overlap_start >= overlap_end {
            continue;
        }
        let remaining = text
            .len()
            .saturating_sub(overlap_end.saturating_sub(overlap_start));
        if remaining > 0 {
            continue;
        }
        // Same delimiter set as caret chrome (`=` / `~~` / `^`, not only `*` / ticks).
        let expanded = expand_mark_delimiters(source, block, source_range);
        range.start = range.start.min(expanded.start);
        range.end = range.end.max(expanded.end);
    }
}

fn delete_range(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    kind: TransactionKind,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    clamp_selection_to_table_cell(engine, caret, &source);
    caret.range = expand_range_over_escaped_table_pipe(engine, &source, caret.range.clone());
    let start = caret.range.start;
    let end = caret.range.end;
    if start == end {
        return Ok(RichOutcome::Noop);
    }
    let expanded = engine.expand_atomic_widget_range(start..end);
    caret.range = expanded.clone();
    let start = expanded.start;
    let end = expanded.end;
    if start == end {
        return Ok(RichOutcome::Noop);
    }
    if start > doc.buffer.len_bytes() || end > doc.buffer.len_bytes() {
        return Err(RichError::InvalidRange);
    }
    let before = caret.snapshot();
    let after = CaretState::collapsed(start);
    doc.replace_range_tx(start, end, "", kind, before, after.snapshot());
    *caret = after;
    Ok(RichOutcome::Changed)
}

/// Drag-select across GFM `|` must not merge cells. Clamp the range to the
/// cell that contains the start (or the caret), or collapse if the range is
/// only separators.
fn clamp_selection_to_table_cell(engine: &RichEngine, caret: &mut CaretState, source: &str) {
    if caret.range.is_empty() {
        return;
    }
    let Some(slice) = source.get(caret.range.clone()) else {
        return;
    };
    if !slice.contains('|') {
        return;
    }
    let end_probe = caret.range.end.saturating_sub(1).max(caret.range.start);
    if !engine.in_table(caret.range.start)
        && !engine.in_table(end_probe)
        && !engine.in_table(caret.cursor())
    {
        return;
    }
    let Some(cell) = engine
        .cell_edit_range(caret.range.start, source)
        .or_else(|| engine.cell_edit_range(caret.cursor(), source))
        .or_else(|| engine.cell_edit_range(end_probe, source))
    else {
        caret.collapse_to(caret.range.start);
        return;
    };
    let start = caret.range.start.clamp(cell.start, cell.end);
    let end = caret.range.end.clamp(cell.start, cell.end);
    if start >= end {
        caret.collapse_to(start);
    } else {
        caret.range = start..end;
    }
}

/// Editable cell containing `byte`, or the cell next to a GFM `|` the caret
/// is sitting on (pipes are not themselves editable).
fn cell_edit_range_near(engine: &RichEngine, source: &str, byte: usize) -> Option<Range<usize>> {
    engine.cell_edit_range(byte, source).or_else(|| {
        // Escaped `\|` is cell text, not a separator to hop across.
        if !is_unescaped_pipe(source, byte) {
            return None;
        }
        engine
            .cell_edit_range(byte.saturating_add(1).min(source.len()), source)
            .or_else(|| engine.cell_edit_range(byte.saturating_sub(1), source))
    })
}

/// Wrap (ToggleMark / ToggleLink) must not splice delimiters across GFM `|`.
///
/// Only when the caret or selection *starts* in a table — a document-wide
/// selection that merely overlaps a table (Cmd-A from a paragraph) still
/// wraps the paragraph, not a cell. Returns `false` when wrap should no-op
/// (in a table but not in an editable cell).
fn clamp_wrap_to_table_cell(engine: &RichEngine, caret: &mut CaretState, source: &str) -> bool {
    // Last `|` at EOF is not a wrap target: Cmd-B/I/E/K must open a body
    // paragraph after the table, not clamp into the last cell.
    if needs_newline_after_frontmatter_fence(source, engine, caret.cursor()) {
        return true;
    }
    if !engine.in_table(caret.range.start) && !engine.in_table(caret.cursor()) {
        return true;
    }
    let Some(cell) = cell_edit_range_near(engine, source, caret.range.start)
        .or_else(|| cell_edit_range_near(engine, source, caret.cursor()))
    else {
        return false;
    };
    if caret.range.is_empty() {
        let at = insert_offset_escaping_table_pipe(engine, source, caret.cursor());
        caret.collapse_to(at.clamp(cell.start, cell.end));
        return true;
    }
    let start = caret.range.start.clamp(cell.start, cell.end);
    let end = caret.range.end.clamp(cell.start, cell.end);
    if start >= end {
        caret.collapse_to(start);
    } else {
        caret.range = start..end;
    }
    true
}

/// First Cmd-A in a table selects the current cell's editable text (Typora).
/// Already selecting that cell, already selecting the whole document, or not
/// in a table: `None` so the caller can take the document.
///
/// `prior_cell` is the range the previous Cmd-A selected (if it was a cell).
/// Empty cells need it: their body range is collapsed and equals the caret,
/// so range equality alone cannot tell first Cmd-A ("select the cell") from
/// second Cmd-A (document).
pub fn table_select_all_range(
    engine: &RichEngine,
    source: &str,
    current: &Range<usize>,
    prior_cell: Option<&Range<usize>>,
) -> Option<Range<usize>> {
    if current.start == 0 && current.end == source.len() && !current.is_empty() {
        return None;
    }
    let start = current.start.min(source.len());
    let end = current.end.min(source.len());
    let end_inside = end.saturating_sub(1).max(start);
    if !engine.in_table(start) && !engine.in_table(end_inside) && !engine.in_table(end) {
        return None;
    }
    let cell = cell_edit_range_near(engine, source, start)
        .or_else(|| cell_edit_range_near(engine, source, end_inside))
        .or_else(|| cell_edit_range_near(engine, source, end))?;
    // Non-empty cell already selected, or empty cell latched by the last Cmd-A.
    if prior_cell == Some(&cell) || (*current == cell && !current.is_empty()) {
        None
    } else {
        Some(cell)
    }
}

/// True when the caret or the selection start sits in a GFM table.
/// Heading/list/quote have no cell-local meaning and must not rewrite `|`.
fn selection_in_table(engine: &RichEngine, caret: &CaretState) -> bool {
    engine.in_table(caret.range.start) || engine.in_table(caret.cursor())
}

fn split_block(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    if !caret.range.is_empty() {
        delete_range(doc, engine, caret, TransactionKind::Command)?;
        engine.sync(doc);
    }
    let offset = caret.cursor();
    let source = doc.buffer.content();
    // Fenced/indented code and HTML blocks keep Enter inside the block.
    // Check before empty-list/quote so a `- ` or `> ` line of code is not
    // treated as a list item or quote exit (and so a caret on fence chrome
    // cannot SplitBlock the ticks in half).
    if in_raw_block(engine, offset) {
        return insert_raw_newline(doc, engine, caret, &source, offset);
    }
    if empty_list_line(&source, offset) {
        return outdent_current_list_line(doc, engine, caret);
    }
    // Last `|` at EOF is table chrome: Enter must open a body line, not
    // splice `<br>` onto the pipe (`| 1 | 2 |<br>`).
    if needs_newline_after_frontmatter_fence(&source, engine, offset) {
        splice(doc, caret, offset, offset, "\n", TransactionKind::Command);
        engine.sync(doc);
        return Ok(RichOutcome::Changed);
    }
    // Enter in a table is a cell line-break (`<br>`), not a paragraph split.
    // A raw newline would split the GFM row the same way Tab used to indent.
    if engine.in_table(offset) {
        return table_cell_break(doc, engine, caret);
    }
    // Empty quote + Enter leaves the quote (Typora / GFM), like an empty list item.
    if empty_quote_line(&source, offset) {
        if let Some(leaf_id) = engine.block_at(offset) {
            if ancestor_is_quote(engine, leaf_id) {
                return outdent_or_exit_quote(doc, engine, caret);
            }
        }
    }
    // Typora: Enter on an empty footnote definition drops `[^1]: `
    // (quoted keep `>`), like an empty list item / heading.
    if empty_footnote_def_line(&source, engine, offset) {
        return strip_footnote_def_marker(doc, engine, caret);
    }
    // Typora: Enter on an empty `[ref]: ` drops the opener (quoted keep `>`),
    // like an empty list item / footnote def. Mid-dest Enter must not `\n\n`
    // split the dest into prose.
    if empty_link_ref_def_line(&source, offset) {
        return strip_link_ref_def_marker(doc, engine, caret);
    }
    // Typora: Enter on an empty ATX/setext heading drops heading chrome
    // (becomes a paragraph). Enter at the start of a non-empty heading
    // inserts a blank paragraph above and keeps the heading. Mid/end
    // still splits like a paragraph.
    if empty_heading_at(engine, offset) {
        return convert_heading_to_paragraph(doc, engine, caret);
    }
    if at_heading_body_start(&source, engine, offset) {
        return insert_paragraph_above_heading(doc, engine, caret);
    }
    // Typora/pandoc: Enter at the end of a definition term places or
    // creates a `: ` details opener. A generic `\n\n` split turns
    // `Term\n\n: details` into `Term\n\n\n\n: details`. A paragraph
    // immediately after a deflist is the next term (Enter creates `: `).
    if at_definition_term_end(engine, offset) {
        return split_at_definition_term_end(doc, engine, caret);
    }
    if at_pending_definition_term_end(engine, offset) {
        return insert_definition_details_opener(doc, engine, caret);
    }
    if at_definition_details_end(engine, offset) {
        return split_at_definition_details_end(doc, engine, caret);
    }
    if at_definition_term_body_start(&source, engine, offset) {
        return insert_paragraph_above_definition_term(doc, engine, caret);
    }
    // Typora: Enter inside `$x^2$` / `$$\nE=mc^2\n$$` stays in the TeX.
    // A paragraph `\n\n` split (or a list sibling split) breaks the dollars.
    // Quoted `> $$` already used a single `>` line via ancestor_is_quote;
    // unquoted must match. Shift-Enter shares this (not `\\\n` in the TeX).
    if math_inline_at(engine, offset) {
        return insert_math_newline(doc, engine, caret, &source, offset);
    }
    // GFM `[hello](url)` / `![alt](url)` / autolink: a paragraph `\n\n`
    // (or a list sibling) splits dest/label into prose. Label/title wrap
    // with `\n`; dest/autolink split after the node (URLs cannot wrap).
    if let Some(hit) = markdown_link_split_at(&source, engine, offset) {
        return apply_markdown_link_split(doc, engine, caret, &source, offset, hit, false);
    }
    // Typora: Enter inside `**bold**` / `` `code` `` / `<b>hello</b>` stays
    // one node. A paragraph `\n\n` split orphans the closer as prose.
    if let Some((hit, _)) = wrap_span_split_at(&source, engine, offset) {
        return apply_markdown_link_split(doc, engine, caret, &source, offset, hit, false);
    }
    let Some(leaf_id) = engine.block_at(offset) else {
        splice(doc, caret, offset, offset, "\n\n", TransactionKind::Command);
        engine.sync(doc);
        return Ok(RichOutcome::Changed);
    };
    let Some(leaf) = engine.block(leaf_id) else {
        return Ok(RichOutcome::Noop);
    };
    let insert = match &leaf.kind {
        BlockKind::ListItem { .. } => list_split_text(&source, engine, leaf, offset),
        BlockKind::LinkReferenceDefinition { .. } => {
            if in_link_ref_dest(&source, engine, offset) {
                link_ref_def_split_text(&source, engine, offset)
            } else {
                String::new()
            }
        }
        _ => {
            if ancestor_link_reference_definition(engine, offset).is_some()
                && in_link_ref_dest(&source, engine, offset)
            {
                // Mid-dest Enter must stay one `[ref]:` definition. A
                // paragraph `\n\n` (or a list sibling) orphans the rest as
                // prose. Bare `\n` also breaks CommonMark dest, so this is
                // `\\\n` (quoted keep `>`).
                link_ref_def_split_text(&source, engine, offset)
            } else if let Some(item) = ancestor_list_item(engine, leaf_id) {
                list_split_text(&source, engine, item, offset)
            } else if ancestor_definition_term(engine, offset).is_some() {
                // Mid-term Enter must stay in the list (`Te\nrm\n: details`),
                // not a paragraph `\n\n` split that turns `Te` into prose.
                let line = current_line(&source, offset);
                format!("\n{}", quote_prefix(line))
            } else if ancestor_definition_details(engine, offset).is_some() {
                // Mid-details Enter continues as more details (`: de\n: tails`),
                // not a paragraph `\n\n` split that orphans the rest as prose.
                details_split_text(&source, engine, offset)
            } else if ancestor_footnote_definition(engine, offset).is_some() {
                // Mid-def Enter is a lazy continuation (`the no\nte` stays
                // inside `[^1]:`). A paragraph `\n\n` split orphans the rest
                // as prose. End-of-def Enter still exits (`\n\n`; quoted keep
                // `>` so a following empty quoted line can leave the quote).
                footnote_def_split_text(&source, engine, offset)
            } else if ancestor_is_quote(engine, leaf_id) {
                let line = current_line(&source, offset);
                let prefix = quote_marker_prefix(line).unwrap_or_else(|| "> ".to_string());
                format!("\n{prefix}")
            } else {
                "\n\n".to_string()
            }
        }
    };
    if insert.is_empty() {
        return Ok(RichOutcome::Noop);
    }
    splice(
        doc,
        caret,
        offset,
        offset,
        &insert,
        TransactionKind::Command,
    );
    engine.sync(doc);
    Ok(RichOutcome::Changed)
}

/// Continue a `[ref]:` dest with a backslash hard break (quote-prefixed).
/// Bare `\n` / `\n\n` split dest into prose (CommonMark dest cannot wrap).
/// Do not copy `[ref]: ` — that would open a second definition.
fn link_ref_def_split_text(source: &str, engine: &RichEngine, offset: usize) -> String {
    format!(
        "\\\n{}",
        link_ref_def_continuation_prefix(source, engine, offset)
    )
}

/// Quote `>` or list continuation indent. Comrak detaches `[ref]:` from the
/// AST; recovered quoted defs may sit outside `ancestor_is_quote`.
fn link_ref_def_continuation_prefix(source: &str, engine: &RichEngine, offset: usize) -> String {
    let prefix = paste_line_prefix(source, engine, offset);
    if !prefix.is_empty() {
        return prefix;
    }
    quote_prefix(current_line(source, offset)).to_string()
}

fn in_link_ref_dest(source: &str, engine: &RichEngine, offset: usize) -> bool {
    let Some(def) = ancestor_link_reference_definition(engine, offset) else {
        return false;
    };
    if let Some(chrome) = link_reference_def_chrome(source, def) {
        return offset >= chrome.colon.start;
    }
    true
}

/// Empty `[ref]: ` / `> [ref]: ` opener line (no dest yet).
fn empty_link_ref_def_line(source: &str, offset: usize) -> bool {
    let line = current_line(source, offset);
    let after = after_quote(line);
    let marker = link_ref_def_marker_on_line(after);
    !marker.is_empty() && after[marker.len()..].trim().is_empty()
}

fn strip_link_ref_def_marker(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let offset = caret.cursor();
    let start = line_start(&source, offset);
    let line = current_line(&source, offset);
    let quote = quote_prefix(line);
    let after = after_quote(line);
    let marker = link_ref_def_marker_on_line(after);
    if marker.is_empty() {
        return Ok(RichOutcome::Noop);
    }
    let rest = after.get(marker.len()..).unwrap_or("");
    let new_line = format!("{quote}{rest}");
    if new_line == line {
        return Ok(RichOutcome::Noop);
    }
    rewrite_range(doc, engine, caret, start..start + line.len(), &new_line)
}

/// Continue a footnote definition with a lazy newline (quote-prefixed).
/// Unquoted Enter at the last body byte exits with `\n\n` so the next
/// paragraph is not swallowed as `[^1]: note\nx`.
fn footnote_def_split_text(source: &str, engine: &RichEngine, offset: usize) -> String {
    let quote = footnote_def_continuation_prefix(source, offset);
    if quote.is_empty() && at_footnote_def_body_end(engine, offset) {
        return "\n\n".to_string();
    }
    format!("\n{quote}")
}

/// Quote markers on the current footnote-def line (`>` / `> >`). Comrak
/// lifts footnote definitions out of the quote tree, so `paste_line_prefix`
/// cannot see `ancestor_is_quote`. Do not copy `[^1]: ` — that would open
/// a second definition.
fn footnote_def_continuation_prefix(source: &str, offset: usize) -> String {
    quote_prefix(current_line(source, offset)).to_string()
}

/// True when the caret is at or after the last visible character of a
/// footnote definition (including empty `[^1]: `).
fn at_footnote_def_body_end(engine: &RichEngine, offset: usize) -> bool {
    if engine.in_table(offset) || engine.in_raw_context(offset) {
        return false;
    }
    let Some(def) = ancestor_footnote_definition(engine, offset) else {
        return false;
    };
    match last_visible_body_end(def) {
        Some(end) => offset >= end,
        None => true,
    }
}

/// Empty `[^1]: ` / `> [^1]: ` opener line (no body yet).
fn empty_footnote_def_line(source: &str, engine: &RichEngine, offset: usize) -> bool {
    if ancestor_footnote_definition(engine, offset).is_none() {
        return false;
    }
    let line = current_line(source, offset);
    let after = after_quote(line);
    let marker = footnote_def_marker_on_line(after);
    !marker.is_empty() && after[marker.len()..].trim().is_empty()
}

/// Continue definition details with a `: ` sibling line (quote-prefixed),
/// copying the opener marker so the rest stays in the list.
fn details_split_text(source: &str, engine: &RichEngine, offset: usize) -> String {
    format!("\n{}", details_continuation_prefix(source, engine, offset))
}

/// `: ` (and any quote prefix) copied from the current details line, or
/// from the details opener when the caret is on a continuation.
fn details_continuation_prefix(source: &str, engine: &RichEngine, offset: usize) -> String {
    let line = current_line(source, offset);
    let quote = quote_prefix(line);
    if let Some(marker) = definition_details_marker_prefix(after_quote(line)) {
        return format!("{quote}{marker}");
    }
    if let Some(details) = ancestor_definition_details(engine, offset) {
        let start = line_start(source, details.source_range.start);
        let opener = current_line(source, start);
        let opener_quote = quote_prefix(opener);
        if let Some(marker) = definition_details_marker_prefix(after_quote(opener)) {
            return format!("{opener_quote}{marker}");
        }
    }
    format!("{quote}: ")
}

/// Container chrome for `InsertLineBreak`. Details keep `: `; footnote
/// defs keep quote markers (lazy / indented body, not a second `[^1]: `);
/// `[ref]:` dest keeps quote / list continuation (not a second `[ref]: `);
/// lists/quotes/tasks/alerts share paste's prefixes.
fn line_break_prefix(source: &str, engine: &RichEngine, offset: usize) -> String {
    if ancestor_definition_details(engine, offset).is_some() {
        return details_continuation_prefix(source, engine, offset);
    }
    if ancestor_link_reference_definition(engine, offset).is_some() {
        return link_ref_def_continuation_prefix(source, engine, offset);
    }
    let prefix = paste_line_prefix(source, engine, offset);
    if prefix.is_empty() && ancestor_footnote_definition(engine, offset).is_some() {
        return footnote_def_continuation_prefix(source, offset);
    }
    prefix
}

fn list_split_text(source: &str, _engine: &RichEngine, item: &Block, offset: usize) -> String {
    let line = current_line(source, offset);
    let quote = quote_prefix(line);
    let marker = list_marker_prefix(after_quote(line)).or_else(|| {
        let slice = source.get(item.source_range.clone()).unwrap_or_default();
        let first = slice.split('\n').next().unwrap_or(slice);
        list_marker_prefix(after_quote(first))
    });
    let marker = marker.unwrap_or_else(|| "- ".to_string());
    // Typora: Enter on a checked GFM task opens an unchecked item.
    format!("\n{quote}{}", continue_list_marker(&marker))
}

/// Keep the list marker (and an unchecked `[ ] ` slot) but do not copy `[x]` / `[X]`.
fn continue_list_marker(marker: &str) -> String {
    for checked in ["[x] ", "[X] ", "[x]\t", "[X]\t"] {
        if let Some(prefix) = marker.strip_suffix(checked) {
            let pad = if checked.ends_with('\t') { "\t" } else { " " };
            return format!("{prefix}[ ]{pad}");
        }
    }
    marker.to_string()
}

fn empty_list_line(source: &str, offset: usize) -> bool {
    let line = current_line(source, offset);
    let after = after_quote(line);
    match list_marker_prefix(after) {
        Some(prefix) => after[prefix.len()..].trim().is_empty(),
        None => false,
    }
}

/// Bytes of a leading `>` chain (optional space after each), or empty.
fn quote_prefix(line: &str) -> &str {
    match quote_marker_prefix(line) {
        Some(prefix) => &line[..prefix.len()],
        None => "",
    }
}

fn after_quote(line: &str) -> &str {
    &line[quote_prefix(line).len()..]
}

fn empty_quote_line(source: &str, offset: usize) -> bool {
    is_empty_quote_line(current_line(source, offset))
}

fn is_empty_quote_line(line: &str) -> bool {
    match quote_marker_prefix(line) {
        Some(prefix) => line[prefix.len()..].trim().is_empty(),
        None => false,
    }
}

/// Leading indent plus one or more `>` markers (optional space after each).
fn quote_marker_prefix(line: &str) -> Option<String> {
    let indent_len = line
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    let bytes = line.as_bytes();
    if bytes.get(indent_len) != Some(&b'>') {
        return None;
    }
    let mut i = indent_len;
    while bytes.get(i) == Some(&b'>') {
        i += 1;
        if bytes.get(i) == Some(&b' ') || bytes.get(i) == Some(&b'\t') {
            i += 1;
        }
    }
    Some(line[..i].to_string())
}

fn outdent_or_exit_quote(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let offset = caret.cursor();
    let start = line_start(&source, offset);
    let line = current_line(&source, offset);
    let Some(prefix) = quote_marker_prefix(line) else {
        return Ok(RichOutcome::Noop);
    };
    if let Some(outdented) = outdent_quote_prefix(&prefix) {
        let rest = line.get(prefix.len()..).unwrap_or("");
        let new_line = format!("{outdented}{rest}");
        return rewrite_range(doc, engine, caret, start..start + line.len(), &new_line);
    }
    let mut from = start;
    let mut to = start + line.len();
    if source.as_bytes().get(to) == Some(&b'\n') {
        to += 1;
    }
    // Drop a blank `>` separator left by a previous continue so two Enters
    // leave the quote instead of a trailing empty quoted line.
    loop {
        if from == 0 || source.as_bytes().get(from - 1) != Some(&b'\n') {
            break;
        }
        let prev_end = from - 1;
        let prev_start = line_start(&source, prev_end);
        let prev = &source[prev_start..prev_end];
        if is_empty_quote_line(prev) {
            from = prev_start;
            continue;
        }
        break;
    }
    let mut replacement = String::new();
    if from > 0 && source.as_bytes()[from - 1] == b'\n' {
        from -= 1;
        replacement = "\n\n".to_string();
    }
    rewrite_range(doc, engine, caret, from..to, &replacement)
}

/// Strip one `>` from the current quoted line, keeping the body (Typora:
/// Option-Backspace at the start of a quoted paragraph). Nested quotes
/// outdent one level; a single `>` becomes an unquoted paragraph.
fn outdent_current_quote_line(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let offset = caret.cursor();
    let start = line_start(&source, offset);
    let line = current_line(&source, offset);
    let Some(prefix) = quote_marker_prefix(line) else {
        return Ok(RichOutcome::Noop);
    };
    let rest = line.get(prefix.len()..).unwrap_or("");
    let new_line = match outdent_quote_prefix(&prefix) {
        Some(kept) => format!("{kept}{rest}"),
        None => rest.to_string(),
    };
    if new_line == line {
        return Ok(RichOutcome::Noop);
    }
    rewrite_range(doc, engine, caret, start..start + line.len(), &new_line)
}

fn outdent_quote_prefix(prefix: &str) -> Option<String> {
    let last = prefix.rfind('>')?;
    if !prefix[..last].contains('>') {
        return None;
    }
    Some(prefix[..last].to_string())
}

fn current_line(source: &str, offset: usize) -> &str {
    let start = line_start(source, offset);
    let end = line_end_exclusive(source, offset);
    &source[start..end]
}

fn line_start(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0)
}

fn line_end_exclusive(source: &str, offset: usize) -> usize {
    let offset = offset.min(source.len());
    match source[offset..].find('\n') {
        Some(i) => offset + i,
        None => source.len(),
    }
}

/// True when the caret is on the item's first line at or before the first
/// visible body character (WYSIWYG start). Quote / list-marker chrome counts
/// as "start" so Backspace does not nibble `>` one byte at a time.
fn at_list_item_body_start(source: &str, engine: &RichEngine, offset: usize) -> bool {
    if engine.in_table(offset) || engine.in_raw_context(offset) {
        return false;
    }
    let start = line_start(source, offset);
    let line = current_line(source, offset);
    let Some(marker) = list_marker_prefix(after_quote(line)) else {
        return false;
    };
    let body_start = start + quote_prefix(line).len() + marker.len();
    let visual_start = engine.snap_caret(body_start, Bias::Right);
    offset <= visual_start.max(body_start)
}

/// True when the caret is at or before the first visible heading character
/// (WYSIWYG start of the heading body). Hash / setext chrome counts as start
/// so Backspace does not no-op or nibble `#` one byte at a time.
fn at_heading_body_start(source: &str, engine: &RichEngine, offset: usize) -> bool {
    if engine.in_table(offset) || engine.in_raw_context(offset) {
        return false;
    }
    let Some(heading) = heading_at(engine, offset) else {
        return false;
    };
    let body_start = heading_body_start(source, heading);
    let visual_start = engine.snap_caret(body_start, Bias::Right);
    offset <= visual_start.max(body_start)
}

/// True when the caret is on the first line of a quoted leaf at or before
/// the first visible body character (not a heading or list — those convert
/// / strip first). Quote chrome counts as start so word-delete does not
/// nibble `>` or eat the previous block.
fn at_quote_body_start(source: &str, engine: &RichEngine, offset: usize) -> bool {
    if engine.in_table(offset) || engine.in_raw_context(offset) {
        return false;
    }
    if heading_at(engine, offset).is_some() || at_list_item_body_start(source, engine, offset) {
        return false;
    }
    let Some(id) = engine.block_at(offset) else {
        return false;
    };
    if !ancestor_is_quote(engine, id) {
        return false;
    }
    let Some(block) = engine.block(id) else {
        return false;
    };
    let first = line_start(source, block.source_range.start);
    if line_start(source, offset) != first {
        return false;
    }
    let line = current_line(source, first);
    let Some(prefix) = quote_marker_prefix(line) else {
        return false;
    };
    let body_start = first + prefix.len();
    let visual_start = engine.snap_caret(body_start, Bias::Right);
    offset <= visual_start.max(body_start)
}

fn heading_at(engine: &RichEngine, offset: usize) -> Option<&Block> {
    let id = engine.block_at(offset)?;
    let block = engine.block(id)?;
    matches!(block.kind, BlockKind::Heading { .. }).then_some(block)
}

fn ancestor_block<F>(engine: &RichEngine, offset: usize, pred: F) -> Option<&Block>
where
    F: Fn(&Block) -> bool,
{
    let id = engine.block_at(offset)?;
    fn walk<'t>(
        blocks: &'t [Block],
        id: NodeId,
        pred: &impl Fn(&Block) -> bool,
        current: Option<&'t Block>,
    ) -> Option<&'t Block> {
        for b in blocks {
            let next = if pred(b) { Some(b) } else { current };
            if b.id == id {
                return next;
            }
            if let Some(found) = walk(&b.children, id, pred, next) {
                return Some(found);
            }
        }
        None
    }
    walk(&engine.tree().blocks, id, &pred, None)
}

fn parent_block(engine: &RichEngine, child_id: NodeId) -> Option<&Block> {
    fn walk(blocks: &[Block], child_id: NodeId) -> Option<&Block> {
        for b in blocks {
            if b.children.iter().any(|c| c.id == child_id) {
                return Some(b);
            }
            if let Some(found) = walk(&b.children, child_id) {
                return Some(found);
            }
        }
        None
    }
    walk(&engine.tree().blocks, child_id)
}

fn sibling_index(engine: &RichEngine, id: NodeId) -> Option<(&[Block], usize)> {
    fn find(blocks: &[Block], id: NodeId) -> Option<(&[Block], usize)> {
        if let Some(i) = blocks.iter().position(|b| b.id == id) {
            return Some((blocks, i));
        }
        for b in blocks {
            if let Some(found) = find(&b.children, id) {
                return Some(found);
            }
        }
        None
    }
    find(&engine.tree().blocks, id)
}

fn previous_sibling(engine: &RichEngine, id: NodeId) -> Option<&Block> {
    let (blocks, i) = sibling_index(engine, id)?;
    i.checked_sub(1).and_then(|j| blocks.get(j))
}

fn next_sibling(engine: &RichEngine, id: NodeId) -> Option<&Block> {
    let (blocks, i) = sibling_index(engine, id)?;
    blocks.get(i + 1)
}

fn ancestor_definition_term(engine: &RichEngine, offset: usize) -> Option<&Block> {
    ancestor_block(engine, offset, |b| {
        matches!(b.kind, BlockKind::DefinitionTerm)
    })
}

fn ancestor_definition_details(engine: &RichEngine, offset: usize) -> Option<&Block> {
    ancestor_block(engine, offset, |b| {
        matches!(b.kind, BlockKind::DefinitionDetails)
    })
}

fn ancestor_definition_list(engine: &RichEngine, offset: usize) -> Option<&Block> {
    ancestor_block(engine, offset, |b| {
        matches!(b.kind, BlockKind::DefinitionList)
    })
}

fn ancestor_footnote_definition(engine: &RichEngine, offset: usize) -> Option<&Block> {
    ancestor_block(engine, offset, |b| {
        matches!(b.kind, BlockKind::FootnoteDefinition { .. })
    })
}

fn ancestor_link_reference_definition(engine: &RichEngine, offset: usize) -> Option<&Block> {
    ancestor_block(engine, offset, |b| {
        matches!(b.kind, BlockKind::LinkReferenceDefinition { .. })
    })
}

/// `[[target]]` containing `offset`. Exclusive end so wrap-mark Enter does
/// not rewrite wiki (wrap-inside-wiki stays a leftover no-op).
fn wiki_inline_at(engine: &RichEngine, offset: usize) -> bool {
    fn walk(blocks: &[Block], offset: usize) -> bool {
        for b in blocks {
            if walk(&b.children, offset) {
                return true;
            }
            for inline in &b.inlines {
                if let Inline::WikiLink { source_range, .. } = inline {
                    if source_range.start <= offset && offset < source_range.end {
                        return true;
                    }
                }
            }
        }
        false
    }
    walk(&engine.tree().blocks, offset)
}

/// `$…$` / `$$…$$` span containing `offset`. Exclusive end so a caret after
/// the closer (`$x^2$|`) is a following insert, not inside the TeX.
fn math_inline_at(engine: &RichEngine, offset: usize) -> bool {
    fn walk(blocks: &[Block], offset: usize) -> bool {
        for b in blocks {
            if walk(&b.children, offset) {
                return true;
            }
            for inline in &b.inlines {
                if let Inline::Math { source_range, .. } = inline {
                    if source_range.start <= offset && offset < source_range.end {
                        return true;
                    }
                }
            }
        }
        false
    }
    walk(&engine.tree().blocks, offset)
}

/// Raw newline plus quote/list continuation, like a fence body. Not `\\\n`
/// (TeX) and not `\n\n` (which splits the dollars into prose).
fn insert_math_newline(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    source: &str,
    offset: usize,
) -> Result<RichOutcome, RichError> {
    let prefix = paste_line_prefix(source, engine, offset);
    splice(
        doc,
        caret,
        offset,
        offset,
        &format!("\n{prefix}"),
        TransactionKind::Command,
    );
    engine.sync(doc);
    Ok(RichOutcome::Changed)
}

/// Enter / Shift-Enter inside a GFM markdown link, image, or autolink.
enum MarkdownLinkSplit {
    /// Label / alt / title: a soft wrap (or hard break) stays inside the node.
    Wrap,
    /// Dest URL / autolink cannot contain a line ending; insert after `end`.
    After { end: usize },
}

/// Innermost markdown link/image/autolink containing `offset`. `[ref]:`
/// definition blocks have their own dest-wrap path.
fn markdown_link_split_at(
    source: &str,
    engine: &RichEngine,
    offset: usize,
) -> Option<MarkdownLinkSplit> {
    if ancestor_link_reference_definition(engine, offset).is_some() {
        return None;
    }
    let mut best: Option<(usize, MarkdownLinkSplit)> = None;
    collect_markdown_link_split(&engine.tree().blocks, source, offset, &mut best);
    let (_, mut hit) = best?;
    if atx_heading_at(engine, offset) {
        let end = match &hit {
            MarkdownLinkSplit::After { end } => *end,
            MarkdownLinkSplit::Wrap => line_end_exclusive(source, offset),
        };
        hit = MarkdownLinkSplit::After {
            end: end.max(line_end_exclusive(source, offset)),
        };
    }
    Some(hit)
}

fn collect_markdown_link_split(
    blocks: &[Block],
    source: &str,
    offset: usize,
    best: &mut Option<(usize, MarkdownLinkSplit)>,
) {
    for block in blocks {
        if matches!(block.kind, BlockKind::LinkReferenceDefinition { .. }) {
            continue;
        }
        collect_markdown_link_split(&block.children, source, offset, best);
        for inline in &block.inlines {
            match inline {
                Inline::Run {
                    source_range,
                    link: Some(link),
                    ..
                }
                | Inline::Emoji {
                    source_range,
                    link: Some(link),
                    ..
                } => consider_markdown_link_split(
                    source,
                    block,
                    source_range.clone(),
                    Some(link),
                    offset,
                    best,
                ),
                Inline::Image {
                    source_range, link, ..
                } => {
                    consider_markdown_link_split(
                        source,
                        block,
                        source_range.clone(),
                        None,
                        offset,
                        best,
                    );
                    if let Some(link) = link.as_ref() {
                        consider_markdown_link_split(
                            source,
                            block,
                            source_range.clone(),
                            Some(link),
                            offset,
                            best,
                        );
                    }
                }
                _ => {}
            }
        }
    }
}

fn consider_markdown_link_split(
    source: &str,
    block: &Block,
    inner: Range<usize>,
    link: Option<&LinkAttrs>,
    offset: usize,
    best: &mut Option<(usize, MarkdownLinkSplit)>,
) {
    let lo = block.source_range.start;
    let hi = block.source_range.end.min(source.len());
    let mut outer = expand_link_and_html_chrome(source, inner, link, lo, hi);
    if link.is_none() {
        outer = expand_around_markdown_link(source, outer);
        outer.start = outer.start.max(lo);
        outer.end = outer.end.min(hi);
    }
    if offset < outer.start || offset >= outer.end {
        return;
    }
    let size = outer.end.saturating_sub(outer.start);
    if best
        .as_ref()
        .is_some_and(|(best_size, _)| size >= *best_size)
    {
        return;
    }
    let hit = if let Some(chrome) = markdown_link_chrome(source, outer.clone()) {
        markdown_link_split_kind(source, &chrome, offset, outer.end)
    } else if link.is_some_and(|l| l.autolink || l.angle) {
        MarkdownLinkSplit::After { end: outer.end }
    } else if link.is_some() {
        // GFM www/email: dest is `http://` / `mailto:` so `autolink` stays
        // false (Normalize would emit `<>`). No `[…]()` chrome.
        let slice = source.get(outer.clone()).unwrap_or("");
        if slice.starts_with('[') || slice.starts_with("![") {
            MarkdownLinkSplit::Wrap
        } else {
            MarkdownLinkSplit::After { end: outer.end }
        }
    } else {
        return;
    };
    *best = Some((size, hit));
}

fn markdown_link_split_kind(
    source: &str,
    chrome: &MarkdownLinkChrome,
    offset: usize,
    outer_end: usize,
) -> MarkdownLinkSplit {
    if let Some(parts) = markdown_link_dest_parts(source, chrome.dest.clone()) {
        if let Some(title) = &parts.title {
            if offset >= title.start && offset < title.end {
                return MarkdownLinkSplit::Wrap;
            }
        }
        if offset >= parts.outer.start && offset < parts.outer.end {
            return MarkdownLinkSplit::After { end: outer_end };
        }
    } else if chrome.dest.start < chrome.dest.end
        && offset >= chrome.dest.start
        && offset < chrome.dest.end
    {
        return MarkdownLinkSplit::After { end: outer_end };
    }
    MarkdownLinkSplit::Wrap
}

fn atx_heading_at(engine: &RichEngine, offset: usize) -> bool {
    ancestor_block(engine, offset, |b| {
        matches!(
            b.kind,
            BlockKind::Heading {
                style: HeadingStyle::Atx,
                ..
            }
        )
    })
    .is_some()
}

fn apply_markdown_link_split(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    source: &str,
    offset: usize,
    hit: MarkdownLinkSplit,
    hard_break: bool,
) -> Result<RichOutcome, RichError> {
    let prefix = paste_line_prefix(source, engine, offset);
    let (at, insert) = match hit {
        MarkdownLinkSplit::Wrap => {
            let insert = if hard_break {
                format!("\\\n{prefix}")
            } else {
                format!("\n{prefix}")
            };
            (offset, insert)
        }
        MarkdownLinkSplit::After { end } => {
            let insert = if prefix.is_empty() {
                if ancestor_footnote_definition(engine, offset).is_some() {
                    "\n".to_string()
                } else {
                    "\n\n".to_string()
                }
            } else {
                format!("\n{prefix}")
            };
            (end.min(source.len()), insert)
        }
    };
    splice(doc, caret, at, at, &insert, TransactionKind::Command);
    engine.sync(doc);
    Ok(RichOutcome::Changed)
}

/// GFM wrap marks (`*` / `**` / `_` / `~~` / ticks) and HTML phrasing
/// (`<b>` / `<a href>`). `raw_newline` is code / HTML: Shift-Enter must not
/// insert a visible `\` inside the span.
fn wrap_span_split_at(
    source: &str,
    engine: &RichEngine,
    offset: usize,
) -> Option<(MarkdownLinkSplit, bool)> {
    if wiki_inline_at(engine, offset) {
        return None;
    }
    let mut best: Option<(usize, MarkdownLinkSplit, bool)> = None;
    collect_wrap_span_split(&engine.tree().blocks, source, offset, &mut best);
    let (_, mut hit, raw_newline) = best?;
    if atx_heading_at(engine, offset) {
        let end = match &hit {
            MarkdownLinkSplit::After { end } => *end,
            MarkdownLinkSplit::Wrap => line_end_exclusive(source, offset),
        };
        hit = MarkdownLinkSplit::After {
            end: end.max(line_end_exclusive(source, offset)),
        };
    }
    Some((hit, raw_newline))
}

fn collect_wrap_span_split(
    blocks: &[Block],
    source: &str,
    offset: usize,
    best: &mut Option<(usize, MarkdownLinkSplit, bool)>,
) {
    for block in blocks {
        collect_wrap_span_split(&block.children, source, offset, best);
        collect_html_phrasing_split(&block.inlines, offset, best);
        for inline in &block.inlines {
            let Inline::Run {
                text,
                source_range,
                marks,
                ..
            } = inline
            else {
                continue;
            };
            if !is_gfm_wrap_mark(*marks) {
                continue;
            }
            let inner = if marks.contains(MarkSet::CODE) {
                code_span_visible_range(source, source_range.clone(), text)
            } else {
                source_range.clone()
            };
            let outer = expand_mark_delimiters(source, block, source_range);
            consider_wrap_span(inner, outer, offset, marks.contains(MarkSet::CODE), best);
        }
    }
}

fn collect_html_phrasing_split(
    inlines: &[Inline],
    offset: usize,
    best: &mut Option<(usize, MarkdownLinkSplit, bool)>,
) {
    let mut stack: Vec<(String, usize, usize)> = Vec::new();
    for inline in inlines {
        let Inline::OpaqueInline {
            raw, source_range, ..
        } = inline
        else {
            continue;
        };
        match crate::html_visual::html_reveal_kind(raw) {
            crate::html_visual::HtmlRevealKind::Open(name) => {
                stack.push((name, source_range.start, source_range.end));
            }
            crate::html_visual::HtmlRevealKind::Close(name) => {
                while let Some((open_name, open_start, open_end)) = stack.pop() {
                    if open_name == name {
                        consider_wrap_span(
                            open_end..source_range.start,
                            open_start..source_range.end,
                            offset,
                            true,
                            best,
                        );
                        break;
                    }
                }
            }
            _ => {}
        }
    }
}

fn is_gfm_wrap_mark(marks: MarkSet) -> bool {
    marks.contains(MarkSet::BOLD)
        || marks.contains(MarkSet::ITALIC)
        || marks.contains(MarkSet::STRIKE)
        || marks.contains(MarkSet::CODE)
}

fn consider_wrap_span(
    inner: Range<usize>,
    outer: Range<usize>,
    offset: usize,
    raw_newline: bool,
    best: &mut Option<(usize, MarkdownLinkSplit, bool)>,
) {
    if outer.end <= outer.start || offset < outer.start || offset >= outer.end {
        return;
    }
    let size = outer.end.saturating_sub(outer.start);
    if best
        .as_ref()
        .is_some_and(|(best_size, _, _)| size >= *best_size)
    {
        return;
    }
    let hit = if inner.end >= inner.start && offset >= inner.start && offset <= inner.end {
        MarkdownLinkSplit::Wrap
    } else {
        MarkdownLinkSplit::After { end: outer.end }
    };
    *best = Some((size, hit, raw_newline));
}

/// Leading indent plus `:` and an optional following space/tab (the PHP-Extra
/// / Typora details marker, after any quote prefix).
fn definition_details_marker_prefix(line: &str) -> Option<String> {
    let marker = definition_details_marker_on_line(line);
    (!marker.is_empty()).then(|| marker.to_string())
}

fn definition_details_body_start(source: &str, details: &Block) -> usize {
    let start = line_start(source, details.source_range.start);
    let line = current_line(source, start);
    let quote_len = quote_prefix(line).len();
    let after = after_quote(line);
    if let Some(marker) = definition_details_marker_prefix(after) {
        return start + quote_len + marker.len();
    }
    first_visible_body_start(details).unwrap_or(details.source_range.start)
}

fn footnote_def_body_start(source: &str, def: &Block) -> usize {
    let start = line_start(source, def.source_range.start);
    let line = current_line(source, start);
    let quote_len = quote_prefix(line).len();
    let after = after_quote(line);
    let marker = footnote_def_marker_on_line(after);
    if !marker.is_empty() {
        return start + quote_len + marker.len();
    }
    first_visible_body_start(def).unwrap_or(def.source_range.start)
}

fn first_visible_body_start(block: &Block) -> Option<usize> {
    let mut start = None;
    fn consider(block: &Block, start: &mut Option<usize>) {
        for inline in &block.inlines {
            if let Inline::OpaqueInline { raw, .. } = inline {
                if crate::html_visual::opaque_inline_is_caret_chrome(raw) {
                    continue;
                }
            }
            let s = inline.source_range().start;
            *start = Some(start.map_or(s, |cur| cur.min(s)));
        }
        for child in &block.children {
            consider(child, start);
        }
    }
    consider(block, &mut start);
    start
}

fn following_definition_details(engine: &RichEngine, term_id: NodeId) -> Option<&Block> {
    let item = parent_block(engine, term_id)?;
    let mut seen = false;
    for child in &item.children {
        if child.id == term_id {
            seen = true;
            continue;
        }
        if !seen {
            continue;
        }
        match child.kind {
            BlockKind::DefinitionDetails => return Some(child),
            BlockKind::DefinitionTerm => return None,
            _ => {}
        }
    }
    None
}

/// True when the caret is at or after the last visible character of a
/// definition term, still before the following details (if any).
fn at_definition_term_end(engine: &RichEngine, offset: usize) -> bool {
    if engine.in_table(offset) || engine.in_raw_context(offset) {
        return false;
    }
    if ancestor_definition_details(engine, offset).is_some() {
        return false;
    }
    let Some(term) = ancestor_definition_term(engine, offset) else {
        return false;
    };
    match last_visible_body_end(term) {
        Some(end) => offset >= end,
        None => true,
    }
}

/// True when the caret is at the first visible character of a definition
/// term (WYSIWYG start). Quote chrome counts as start so Enter does not
/// splice `\n\n` after `>` and break a quoted list.
fn at_definition_term_body_start(source: &str, engine: &RichEngine, offset: usize) -> bool {
    if engine.in_table(offset) || engine.in_raw_context(offset) {
        return false;
    }
    if ancestor_definition_details(engine, offset).is_some() {
        return false;
    }
    let Some(term) = ancestor_definition_term(engine, offset) else {
        return false;
    };
    let start = first_visible_body_start(term).unwrap_or_else(|| {
        let ls = line_start(source, term.source_range.start);
        ls + quote_prefix(current_line(source, ls)).len()
    });
    let visual_start = engine.snap_caret(start, Bias::Right);
    offset <= visual_start.max(start)
}

/// True when the caret is at or after the last visible character of
/// definition details (including empty `: `).
fn at_definition_details_end(engine: &RichEngine, offset: usize) -> bool {
    if engine.in_table(offset) || engine.in_raw_context(offset) {
        return false;
    }
    let Some(details) = ancestor_definition_details(engine, offset) else {
        return false;
    };
    match last_visible_body_end(details) {
        Some(end) => offset >= end,
        None => true,
    }
}

/// A paragraph immediately after a definition list is the next term.
fn at_pending_definition_term_end(engine: &RichEngine, offset: usize) -> bool {
    if engine.in_table(offset) || engine.in_raw_context(offset) {
        return false;
    }
    if ancestor_definition_term(engine, offset).is_some()
        || ancestor_definition_details(engine, offset).is_some()
    {
        return false;
    }
    let Some(id) = engine.block_at(offset) else {
        return false;
    };
    let Some(prev) = previous_sibling(engine, id) else {
        return false;
    };
    if !matches!(prev.kind, BlockKind::DefinitionList) {
        return false;
    }
    let Some(block) = engine.block(id) else {
        return false;
    };
    match last_visible_body_end(block) {
        Some(end) => offset >= end,
        None => true,
    }
}

/// True when the caret is on the details opener line at or before the first
/// visible body character (WYSIWYG start of `: details`).
fn at_definition_details_body_start(source: &str, engine: &RichEngine, offset: usize) -> bool {
    if engine.in_table(offset) || engine.in_raw_context(offset) {
        return false;
    }
    let Some(details) = ancestor_definition_details(engine, offset) else {
        return false;
    };
    let first = line_start(source, details.source_range.start);
    if line_start(source, offset) != first {
        return false;
    }
    let body = definition_details_body_start(source, details);
    let visual_start = engine.snap_caret(body, Bias::Right);
    offset <= visual_start.max(body)
}

/// True when the caret is on the first line of a footnote definition at or
/// before the first visible body character (WYSIWYG start of `[^1]: note`).
fn at_footnote_def_body_start(source: &str, engine: &RichEngine, offset: usize) -> bool {
    if engine.in_table(offset) || engine.in_raw_context(offset) {
        return false;
    }
    let Some(def) = ancestor_footnote_definition(engine, offset) else {
        return false;
    };
    let first = line_start(source, def.source_range.start);
    if line_start(source, offset) != first {
        return false;
    }
    let body = footnote_def_body_start(source, def);
    let visual_start = engine.snap_caret(body, Bias::Right);
    offset <= visual_start.max(body)
}

fn split_at_definition_term_end(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let offset = caret.cursor();
    let Some(term) = ancestor_definition_term(engine, offset) else {
        return Ok(RichOutcome::Noop);
    };
    let term_id = term.id;
    if let Some(details) = following_definition_details(engine, term_id) {
        let body = definition_details_body_start(&source, details);
        let empty = first_visible_body_start(details).is_none();
        let at = if empty {
            body
        } else {
            engine.snap_caret(body, Bias::Right)
        };
        caret.collapse_to(at.min(source.len()));
        return Ok(RichOutcome::Noop);
    }
    insert_definition_details_opener(doc, engine, caret)
}

fn insert_definition_details_opener(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let offset = caret.cursor();
    let line = current_line(&source, offset);
    let insert = format!("\n{}: ", quote_prefix(line));
    splice(
        doc,
        caret,
        offset,
        offset,
        &insert,
        TransactionKind::Command,
    );
    engine.sync(doc);
    Ok(RichOutcome::Changed)
}

fn split_at_definition_details_end(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let offset = caret.cursor();
    if let Some(home) = pending_term_home_after_details(engine, &source, offset) {
        caret.collapse_to(home.min(source.len()));
        return Ok(RichOutcome::Noop);
    }
    let Some(details) = ancestor_definition_details(engine, offset) else {
        return Ok(RichOutcome::Noop);
    };
    let body_end = last_visible_body_end(details).unwrap_or(offset);
    let line_at = if body_end == 0 { 0 } else { body_end - 1 };
    let line = current_line(&source, line_at);
    let quote = quote_prefix(line);
    let insert = if quote.is_empty() {
        "\n\n".to_string()
    } else {
        format!("\n{}\n{quote}", quote.trim_end())
    };
    splice(
        doc,
        caret,
        body_end,
        body_end,
        &insert,
        TransactionKind::Command,
    );
    engine.sync(doc);
    Ok(RichOutcome::Changed)
}

fn pending_term_home_after_details(
    engine: &RichEngine,
    source: &str,
    offset: usize,
) -> Option<usize> {
    let list = ancestor_definition_list(engine, offset)?;
    if let Some(next) = next_sibling(engine, list.id) {
        if matches!(next.kind, BlockKind::Paragraph) {
            let start = line_start(source, next.source_range.start);
            let line = current_line(source, start);
            return Some(start + quote_prefix(line).len());
        }
    }
    let last_top = engine.tree().blocks.last()?;
    let in_last =
        last_top.id == list.id || ancestor_block(engine, offset, |b| b.id == last_top.id).is_some();
    if in_last {
        return blank_caret_gap_after_last(engine.tree()).map(|gap| gap.start);
    }
    None
}

fn insert_paragraph_above_definition_term(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let offset = caret.cursor();
    let Some(term) = ancestor_definition_term(engine, offset) else {
        return Ok(RichOutcome::Noop);
    };
    let insert_at = line_start(&source, term.source_range.start);
    let line = current_line(&source, insert_at);
    let (text, new_cursor) = if let Some(prefix) = quote_marker_prefix(line) {
        (format!("{prefix}\n"), insert_at + prefix.len())
    } else {
        ("\n\n".to_string(), insert_at)
    };
    let before = caret.snapshot();
    let after = CaretState::collapsed(new_cursor);
    doc.replace_range_tx(
        insert_at,
        insert_at,
        &text,
        TransactionKind::Command,
        before,
        after.snapshot(),
    );
    *caret = after;
    engine.sync(doc);
    caret.clamp(doc.buffer.len_bytes());
    Ok(RichOutcome::Changed)
}

fn strip_definition_details_marker(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let offset = caret.cursor();
    let Some(details) = ancestor_definition_details(engine, offset) else {
        return Ok(RichOutcome::Noop);
    };
    let start = line_start(&source, details.source_range.start);
    let line = current_line(&source, start);
    let quote = quote_prefix(line);
    let after = after_quote(line);
    let Some(marker) = definition_details_marker_prefix(after) else {
        return Ok(RichOutcome::Noop);
    };
    let rest = after.get(marker.len()..).unwrap_or("");
    let new_line = format!("{quote}{rest}");
    if new_line == line {
        return Ok(RichOutcome::Noop);
    }
    rewrite_range(doc, engine, caret, start..start + line.len(), &new_line)
}

fn strip_footnote_def_marker(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let offset = caret.cursor();
    let Some(def) = ancestor_footnote_definition(engine, offset) else {
        return Ok(RichOutcome::Noop);
    };
    let start = line_start(&source, def.source_range.start);
    let line = current_line(&source, start);
    let quote = quote_prefix(line);
    let after = after_quote(line);
    let marker = footnote_def_marker_on_line(after);
    if marker.is_empty() {
        return Ok(RichOutcome::Noop);
    }
    let rest = after.get(marker.len()..).unwrap_or("");
    let new_line = format!("{quote}{rest}");
    if new_line == line {
        return Ok(RichOutcome::Noop);
    }
    rewrite_range(doc, engine, caret, start..start + line.len(), &new_line)
}

fn empty_heading_at(engine: &RichEngine, offset: usize) -> bool {
    heading_at(engine, offset).is_some_and(heading_body_empty)
}

fn heading_body_empty(heading: &Block) -> bool {
    heading.inlines.iter().all(|inline| match inline {
        Inline::Run { text, .. } => text.trim().is_empty(),
        Inline::SoftBreak { .. } | Inline::HardBreak { .. } => true,
        _ => false,
    })
}

fn heading_body_start(source: &str, heading: &Block) -> usize {
    let start = line_start(source, heading.source_range.start);
    let line = current_line(source, start);
    let quote_len = quote_prefix(line).len();
    let after = after_quote(line);
    match heading.kind {
        BlockKind::Heading {
            style: HeadingStyle::Setext,
            ..
        } => {
            let indent = after
                .bytes()
                .take_while(|b| *b == b' ' || *b == b'\t')
                .count();
            start + quote_len + indent
        }
        BlockKind::Heading { .. } => {
            let marker_len = list_marker_prefix(after).map(|p| p.len()).unwrap_or(0);
            let rest = after.get(marker_len..).unwrap_or("");
            let prefix = atx_marker_prefix(rest).map(|p| p.len()).unwrap_or(0);
            start + quote_len + marker_len + prefix
        }
        _ => start,
    }
}

fn heading_rewrite_range(source: &str, heading: &Block) -> Range<usize> {
    let start = line_start(source, heading.source_range.start);
    let end_anchor = heading.source_range.end.max(start);
    let end = line_end_exclusive(source, end_anchor.saturating_sub(1).max(start));
    start..end
}

fn convert_heading_to_paragraph(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let offset = caret.cursor();
    let Some(heading) = heading_at(engine, offset).cloned() else {
        return Ok(RichOutcome::Noop);
    };
    let range = heading_rewrite_range(&source, &heading);
    let stripped = strip_heading_chrome(&source, &heading);
    if stripped == source.get(range.clone()).unwrap_or("") {
        return Ok(RichOutcome::Noop);
    }
    rewrite_range(doc, engine, caret, range, &stripped)
}

/// Insert a blank paragraph (or blank list item) before a non-empty heading
/// (Typora: Enter at the first visible character). Splitting at the body
/// caret would leave an empty `# ` line and turn the rest into a paragraph.
fn insert_paragraph_above_heading(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let offset = caret.cursor();
    let Some(heading) = heading_at(engine, offset) else {
        return Ok(RichOutcome::Noop);
    };
    let insert_at = line_start(&source, heading.source_range.start);
    let line = current_line(&source, insert_at);
    let quote = quote_prefix(line);
    let (text, new_cursor) = if let Some(marker) = list_marker_prefix(after_quote(line)) {
        let prefix = format!("{quote}{marker}");
        (format!("{prefix}\n"), insert_at + prefix.len())
    } else if let Some(prefix) = quote_marker_prefix(line) {
        (format!("{prefix}\n"), insert_at + prefix.len())
    } else {
        ("\n\n".to_string(), insert_at)
    };
    let before = caret.snapshot();
    let after = CaretState::collapsed(new_cursor);
    doc.replace_range_tx(
        insert_at,
        insert_at,
        &text,
        TransactionKind::Command,
        before,
        after.snapshot(),
    );
    *caret = after;
    engine.sync(doc);
    caret.clamp(doc.buffer.len_bytes());
    Ok(RichOutcome::Changed)
}

fn strip_heading_chrome(source: &str, heading: &Block) -> String {
    let range = heading_rewrite_range(source, heading);
    let slice = source.get(range).unwrap_or("");
    match heading.kind {
        BlockKind::Heading {
            style: HeadingStyle::Setext,
            ..
        } => strip_setext_underline(slice),
        BlockKind::Heading { .. } => map_item_lines(slice, strip_atx_from_line),
        _ => slice.to_string(),
    }
}

fn strip_setext_underline(slice: &str) -> String {
    let trailing_nl = slice.ends_with('\n');
    let body = slice.strip_suffix('\n').unwrap_or(slice);
    let Some((head, last)) = body.rsplit_once('\n') else {
        return slice.to_string();
    };
    if is_setext_underline(after_quote(last)) || is_setext_underline(last) {
        let mut out = strip_cm_opening_indent_after_quote(head);
        if trailing_nl {
            out.push('\n');
        }
        return out;
    }
    slice.to_string()
}

/// Drop CommonMark 0–3 opening spaces after a quote/list prefix so
/// converting a setext heading to a paragraph does not leave an orphan
/// indent (` Title` after stripping `===`).
fn strip_cm_opening_indent_after_quote(head: &str) -> String {
    let trailing = head.ends_with('\n');
    let body = head.strip_suffix('\n').unwrap_or(head);
    let mut out = body
        .split('\n')
        .map(|line| {
            let quote = quote_prefix(line);
            let after = after_quote(line);
            let n = after.bytes().take_while(|&b| b == b' ').take(3).count();
            format!("{quote}{}", &after[n..])
        })
        .collect::<Vec<_>>()
        .join("\n");
    if trailing {
        out.push('\n');
    }
    out
}

fn is_setext_underline(line: &str) -> bool {
    let t = line.trim_end();
    let indent = t.bytes().take_while(|&b| b == b' ').count();
    if indent > 3 {
        return false;
    }
    let rest = t[indent..].trim_end();
    !rest.is_empty() && (rest.bytes().all(|b| b == b'=') || rest.bytes().all(|b| b == b'-'))
}

fn strip_atx_from_line(line: &str) -> String {
    let quote = quote_prefix(line);
    let after = after_quote(line);
    let marker = list_marker_prefix(after).unwrap_or_default();
    let rest = after.get(marker.len()..).unwrap_or("");
    let Some(prefix) = atx_marker_prefix(rest) else {
        return line.to_string();
    };
    format!(
        "{quote}{marker}{}",
        strip_closing_atx(rest.get(prefix.len()..).unwrap_or(""))
    )
}

/// Opening ATX marker: 0–3 spaces, 1–6 `#`, optional separator space/tab.
fn atx_marker_prefix(after_quote: &str) -> Option<String> {
    let indent = after_quote
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    if indent > 3 {
        return None;
    }
    let rest = &after_quote[indent..];
    let hashes = rest.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after_hashes = &rest[hashes..];
    let extra = if after_hashes.starts_with(' ') || after_hashes.starts_with('\t') {
        1
    } else if after_hashes.is_empty()
        || after_hashes
            .bytes()
            .all(|b| b == b'#' || b == b' ' || b == b'\t')
    {
        0
    } else {
        return None;
    };
    Some(after_quote[..indent + hashes + extra].to_string())
}

fn strip_closing_atx(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut end = body.len();
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
        end -= 1;
    }
    let trimmed = end;
    while end > 0 && bytes[end - 1] == b'#' {
        end -= 1;
    }
    if end == trimmed {
        return body.to_string();
    }
    if end == 0 {
        return String::new();
    }
    if bytes[end - 1] == b' ' || bytes[end - 1] == b'\t' {
        while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
            end -= 1;
        }
        return body[..end].to_string();
    }
    body.to_string()
}

fn list_marker_prefix(line: &str) -> Option<String> {
    let marker = list_marker_on_line(line);
    if marker.is_empty() {
        None
    } else {
        Some(marker.to_string())
    }
}

fn ancestor_list_item(engine: &RichEngine, leaf_id: NodeId) -> Option<&Block> {
    let mut found = None;
    fn walk<'t>(
        blocks: &'t [Block],
        leaf_id: NodeId,
        current_item: Option<&'t Block>,
        found: &mut Option<&'t Block>,
    ) -> bool {
        for b in blocks {
            let item = if matches!(b.kind, BlockKind::ListItem { .. }) {
                Some(b)
            } else {
                current_item
            };
            if b.id == leaf_id {
                *found = item;
                return true;
            }
            if walk(&b.children, leaf_id, item, found) {
                return true;
            }
        }
        false
    }
    walk(engine.tree().blocks.as_slice(), leaf_id, None, &mut found);
    found
}

fn ancestor_is_quote(engine: &RichEngine, leaf_id: NodeId) -> bool {
    fn walk(blocks: &[Block], leaf_id: NodeId, in_quote: bool) -> Option<bool> {
        for b in blocks {
            let q = in_quote || matches!(b.kind, BlockKind::BlockQuote | BlockKind::Alert { .. });
            if b.id == leaf_id {
                return Some(q);
            }
            if let Some(v) = walk(&b.children, leaf_id, q) {
                return Some(v);
            }
        }
        None
    }
    walk(&engine.tree().blocks, leaf_id, false).unwrap_or(false)
}

fn insert_line_break(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    if !caret.range.is_empty() {
        delete_range(doc, engine, caret, TransactionKind::Command)?;
        engine.sync(doc);
    }
    let offset = caret.cursor();
    let source = doc.buffer.content();
    if needs_newline_after_frontmatter_fence(&source, engine, offset) {
        splice(doc, caret, offset, offset, "\n", TransactionKind::Command);
        engine.sync(doc);
        return Ok(RichOutcome::Changed);
    }
    if engine.in_table(offset) {
        return table_cell_break(doc, engine, caret);
    }
    if in_raw_block(engine, offset) {
        return insert_raw_newline(doc, engine, caret, &source, offset);
    }
    if math_inline_at(engine, offset) {
        return insert_math_newline(doc, engine, caret, &source, offset);
    }
    if let Some(hit) = markdown_link_split_at(&source, engine, offset) {
        return apply_markdown_link_split(doc, engine, caret, &source, offset, hit, true);
    }
    // Shift-Enter: emphasis is a hard break inside the span; code / HTML
    // phrasing stay a raw newline (`\` would paint in the span).
    if let Some((hit, raw_newline)) = wrap_span_split_at(&source, engine, offset) {
        return apply_markdown_link_split(doc, engine, caret, &source, offset, hit, !raw_newline);
    }
    if ancestor_link_reference_definition(engine, offset).is_some()
        && !in_link_ref_dest(&source, engine, offset)
        && !empty_link_ref_def_line(&source, offset)
    {
        // A newline in the label would drop `[hello][ref]` resolution.
        return Ok(RichOutcome::Noop);
    }
    // Quote `>` / list continuation indent match Enter and paste so a hard
    // break cannot drop container chrome (`> hello\` then `x` is `> x`,
    // not a lazy unquoted line; `- hello\` continues as indented body).
    // Definition details keep `: ` the same way Enter continues details
    // (`details_split_text`); a bare `\\\n` is a lazy continuation that
    // can break the list. Footnote defs keep quote markers (comrak lifts
    // them out of the quote tree) so `> [^1]: note\` continues as `> x`;
    // unquoted stays lazy. Do not copy `[^1]: `. `[ref]:` dest keeps quote
    // / list continuation (not a second `[ref]: `; dest cannot wrap with
    // a bare `\n`). Terms keep quote prefixes only (do not insert `: `,
    // which would turn a term into details).
    let prefix = line_break_prefix(&source, engine, offset);
    splice(
        doc,
        caret,
        offset,
        offset,
        &format!("\\\n{prefix}"),
        TransactionKind::Command,
    );
    engine.sync(doc);
    Ok(RichOutcome::Changed)
}

/// GFM table cells cannot contain a source newline. Typora encodes an
/// in-cell line break as HTML `<br>` so the row stays one line.
fn table_cell_break(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let offset = caret.cursor();
    splice(doc, caret, offset, offset, "<br>", TransactionKind::Command);
    engine.sync(doc);
    Ok(RichOutcome::Changed)
}

fn toggle_mark(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    mark: MarkSet,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    if !clamp_wrap_to_table_cell(engine, caret, &source) {
        return Ok(RichOutcome::Noop);
    }
    // `` `![alt](url)` `` re-parses as a code span, not an image. Second Cmd-E
    // must strip those ticks rather than no-op as wrap-immune.
    if caret.range.is_empty() && mark == MarkSet::CODE {
        if let Some(atom) = wrap_immune_range(engine, caret.cursor()) {
            if let Some((outer, inner)) = wrapping_code_span_to_unwrap(&source, atom) {
                splice(
                    doc,
                    caret,
                    outer.start,
                    outer.end,
                    &inner,
                    TransactionKind::Command,
                );
                caret.collapse_to(outer.start);
                engine.sync(doc);
                caret.clamp(doc.buffer.len_bytes());
                return Ok(RichOutcome::Changed);
            }
        }
    }
    // Typora: Cmd-B/I/E inside `$math$` / `` `code` `` / `[[wiki]]` / `:emoji:`
    // must not splice `**` into the span (fenced bodies already no-op). Dest-
    // chrome widgets (image / HTML img / `<br>` / hard break / footnote ref /
    // thematic) wrap as a unit instead.
    if wrap_is_immune(engine, caret)
        && wrap_widget_range_at(engine, &source, caret.cursor()).is_none()
        && !needs_newline_after_frontmatter_fence(&source, engine, caret.cursor())
    {
        return Ok(RichOutcome::Noop);
    }
    if caret.range.is_empty() {
        let offset = caret.cursor();
        let prefix = frontmatter_eof_body_prefix(&source, engine, offset);
        if prefix.is_empty() {
            if let Some(widget) = wrap_widget_range_at(engine, &source, offset) {
                caret.range = widget;
                caret.reversed = false;
                if !clamp_wrap_to_table_cell(engine, caret, &source) {
                    return Ok(RichOutcome::Noop);
                }
            } else {
                // Prefix chrome (`- [x] `, `> `, `[^1]: `, ATX `#`, setext
                // underline, table `|`, markdown-link `[`, HTML `<b>`) and
                // wrap-mark openers (`~~` / `**` / `==`) are not insert homes.
                // Sit in the body / inner text instead of splicing `****[x]` /
                // `****#` / `****[` / `****~~`.
                relocate_empty_wrap_caret(engine, &source, caret);
                // Typora / source wrap: empty Cmd-B/I/E inserts `****` / `**` /
                // `` ` ` `` with the caret inside so the next insert is wrapped.
                return toggle_mark_collapsed(doc, engine, caret, mark);
            }
        } else {
            return toggle_mark_collapsed(doc, engine, caret, mark);
        }
    }
    let sel = caret.range.clone();
    let Some(top) = engine.top_level_at(sel.start).cloned() else {
        return Ok(RichOutcome::Noop);
    };
    let leaf_id = engine.block_at(sel.start).unwrap_or(top.id);
    let mut rewritten = top.clone();
    let Some(leaf) = find_block_mut(&mut rewritten, leaf_id) else {
        return Ok(RichOutcome::Noop);
    };
    let src = doc.buffer.content();
    if wrap_widget_range_at(engine, &src, sel.start).as_ref() == Some(&sel) {
        // Widgets reparse as neighboring text (`**[^1]**`) or a code span
        // (`` `<br>` ``). Source splice keeps second-press unwrap.
        return source_toggle_mark_around(doc, engine, caret, sel, mark);
    }
    if !inlines_can_tree_wrap_mark(&leaf.inlines, &sel) {
        return source_toggle_mark_around(doc, engine, caret, sel, mark);
    }
    toggle_mark_inlines(&mut leaf.inlines, &sel, mark);
    let source = doc.buffer.content();
    let new_md = serialize_block(&rewritten, &source);
    let before = caret.snapshot();
    let range = top.source_range.clone();
    let after = CaretState {
        range: range.start + (sel.start.saturating_sub(range.start))
            ..range.start + (sel.end.saturating_sub(range.start)).min(new_md.len()),
        reversed: caret.reversed,
    };
    doc.replace_range_tx(
        range.start,
        range.end,
        &new_md,
        TransactionKind::Command,
        before,
        after.snapshot(),
    );
    *caret = after;
    caret.clamp(doc.buffer.len_bytes());
    engine.sync(doc);
    Ok(RichOutcome::Changed)
}

fn toggle_mark_collapsed(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    mark: MarkSet,
) -> Result<RichOutcome, RichError> {
    if mark_wrap_in_raw_block(engine, caret.cursor()) {
        let source = doc.buffer.content();
        if !needs_newline_after_frontmatter_fence(&source, engine, caret.cursor()) {
            return Ok(RichOutcome::Noop);
        }
    }
    let Some((open, close)) = mark_wrap_delimiters(mark) else {
        return Ok(RichOutcome::Noop);
    };
    let offset = caret.cursor();
    let source = doc.buffer.content();
    if sitting_in_empty_mark_wrappers(&source, offset, open, close, mark) {
        let start = offset - open.len();
        let end = offset + close.len();
        splice(doc, caret, start, end, "", TransactionKind::Command);
        engine.sync(doc);
        caret.clamp(doc.buffer.len_bytes());
        return Ok(RichOutcome::Changed);
    }
    let prefix = frontmatter_eof_body_prefix(&source, engine, offset);
    let pair = format!("{prefix}{open}{close}");
    splice(doc, caret, offset, offset, &pair, TransactionKind::Command);
    caret.collapse_to(offset + prefix.len() + open.len());
    engine.sync(doc);
    caret.clamp(doc.buffer.len_bytes());
    Ok(RichOutcome::Changed)
}

fn mark_wrap_delimiters(mark: MarkSet) -> Option<(&'static str, &'static str)> {
    if mark == MarkSet::BOLD {
        Some(("**", "**"))
    } else if mark == MarkSet::ITALIC {
        Some(("*", "*"))
    } else if mark == MarkSet::CODE {
        Some(("`", "`"))
    } else if mark == MarkSet::STRIKE {
        Some(("~~", "~~"))
    } else if mark == MarkSet::HIGHLIGHT {
        Some(("==", "=="))
    } else if mark == MarkSet::SUP {
        Some(("^", "^"))
    } else if mark == MarkSet::SUB {
        Some(("~", "~"))
    } else {
        None
    }
}

fn mark_wrap_in_raw_block(engine: &RichEngine, offset: usize) -> bool {
    wrap_immune_range(engine, offset).is_some()
}

/// Fence / HTML bodies, inline code, `$math$`, `[[wiki]]`, and `:emoji:` are
/// not markdown-wrap targets. A selection that extends *outside* the atom
/// still wraps (Cmd-B on `see $x$ here`).
fn wrap_immune_range(engine: &RichEngine, byte: usize) -> Option<Range<usize>> {
    let block = engine.block(engine.block_at(byte)?)?;
    match &block.kind {
        BlockKind::CodeBlock { .. } | BlockKind::LinkReferenceDefinition { .. } => {
            Some(block.source_range.clone())
        }
        BlockKind::Opaque { .. } => {
            // HTML-block `<img>` / `<svg>` / `<hr>` / `<br>` wrap as widgets
            // (`**<br>**`, `[<hr>]()`). Other Opaque (script, comments,
            // `<style>`) stay immune.
            if html_block_image_range(block).is_some()
                || html_block_break_range(block).is_some()
                || thematic_break_range(block).is_some()
            {
                None
            } else {
                Some(block.source_range.clone())
            }
        }
        _ => {
            for inline in &block.inlines {
                match inline {
                    Inline::Run {
                        source_range,
                        marks,
                        ..
                    } if marks.contains(MarkSet::CODE)
                        && source_range.start <= byte
                        && byte <= source_range.end =>
                    {
                        return Some(source_range.clone());
                    }
                    Inline::Math { source_range, .. }
                    | Inline::WikiLink { source_range, .. }
                    | Inline::Emoji { source_range, .. }
                        if source_range.start <= byte && byte <= source_range.end =>
                    {
                        return Some(source_range.clone());
                    }
                    _ => {}
                }
            }
            None
        }
    }
}

fn wrap_is_immune(engine: &RichEngine, caret: &CaretState) -> bool {
    let Some(atom) = wrap_immune_range(engine, caret.cursor()).or_else(|| {
        if caret.range.is_empty() {
            None
        } else {
            wrap_immune_range(engine, caret.range.start)
        }
    }) else {
        return false;
    };
    caret.range.is_empty() || (caret.range.start >= atom.start && caret.range.end <= atom.end)
}

/// True when the caret is between a matching empty delimiter pair (`**|**`).
/// Italic `*` must not unwrap the inside of an empty bold `****`.
fn sitting_in_empty_mark_wrappers(
    source: &str,
    offset: usize,
    open: &str,
    close: &str,
    mark: MarkSet,
) -> bool {
    if offset < open.len() || offset + close.len() > source.len() {
        return false;
    }
    if &source[offset - open.len()..offset] != open {
        return false;
    }
    if &source[offset..offset + close.len()] != close {
        return false;
    }
    if mark == MarkSet::ITALIC && open == "*" {
        let star_before = offset >= 2 && source.as_bytes()[offset - 2] == b'*';
        let star_after = offset + 1 < source.len() && source.as_bytes()[offset + 1] == b'*';
        if star_before || star_after {
            return false;
        }
    }
    true
}

fn toggle_mark_inlines(inlines: &mut Vec<Inline>, sel: &Range<usize>, mark: MarkSet) {
    let mut out = Vec::with_capacity(inlines.len() + 2);
    for inline in inlines.drain(..) {
        match inline {
            Inline::Run {
                text,
                raw: _,
                source_range,
                marks,
                link,
                fidelity,
            } if ranges_overlap(&source_range, sel) => {
                let pieces = split_run_text(&text, &source_range, sel);
                for (piece, piece_range, covered) in pieces {
                    let mut new_marks = marks;
                    if covered {
                        new_marks = if marks.contains(mark) {
                            marks.without(mark)
                        } else {
                            marks.with(mark)
                        };
                    }
                    out.push(Inline::Run {
                        text: piece,
                        raw: None,
                        source_range: piece_range,
                        marks: new_marks,
                        link: link.clone(),
                        fidelity,
                    });
                }
            }
            Inline::Image {
                alt,
                url,
                title,
                source_range,
                marks,
                link,
            } if ranges_overlap(&source_range, sel) => {
                let new_marks = if marks.contains(mark) {
                    marks.without(mark)
                } else {
                    marks.with(mark)
                };
                out.push(Inline::Image {
                    alt,
                    url,
                    title,
                    source_range,
                    marks: new_marks,
                    link,
                });
            }
            Inline::OpaqueInline {
                raw,
                source_range,
                marks,
            } if ranges_overlap(&source_range, sel) => {
                let new_marks = if marks.contains(mark) {
                    marks.without(mark)
                } else {
                    marks.with(mark)
                };
                out.push(Inline::OpaqueInline {
                    raw,
                    source_range,
                    marks: new_marks,
                });
            }
            other => out.push(other),
        }
    }
    *inlines = out;
}

fn ranges_overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
        || (a.start == a.end && b.start <= a.start && a.start <= b.end)
        || (b.start == b.end && a.start <= b.start && b.start <= a.end)
}

fn split_run_text(
    text: &str,
    source_range: &Range<usize>,
    sel: &Range<usize>,
) -> Vec<(String, Range<usize>, bool)> {
    let sel_start = sel.start.max(source_range.start);
    let sel_end = sel.end.min(source_range.end);
    if sel_start >= sel_end && sel.start != sel.end {
        return vec![(text.to_string(), source_range.clone(), false)];
    }
    if source_range.len() == text.len() {
        let rel_a = sel_start.saturating_sub(source_range.start);
        let rel_b = sel_end.saturating_sub(source_range.start).min(text.len());
        let mut parts = Vec::new();
        if rel_a > 0 {
            parts.push((
                text[..rel_a].to_string(),
                source_range.start..source_range.start + rel_a,
                false,
            ));
        }
        if rel_a < rel_b || sel.start == sel.end {
            let end = rel_b.max(rel_a);
            parts.push((
                text[rel_a..end].to_string(),
                source_range.start + rel_a..source_range.start + end,
                true,
            ));
        }
        if rel_b < text.len() {
            parts.push((
                text[rel_b..].to_string(),
                source_range.start + rel_b..source_range.end,
                false,
            ));
        }
        return parts;
    }
    // Escaped run: toggle the whole run rather than split mid-escape.
    vec![(text.to_string(), source_range.clone(), true)]
}

fn set_block_type(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    kind: BlockType,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    if should_open_new_block_at_caret(&source, engine, caret) {
        let marker = match kind {
            BlockType::Paragraph => "",
            BlockType::Heading(level) => {
                return insert_block_open_at_caret(
                    doc,
                    engine,
                    caret,
                    &format!("{} ", "#".repeat(level.clamp(1, 6) as usize)),
                );
            }
        };
        return insert_block_open_at_caret(doc, engine, caret, marker);
    }
    if selection_in_table(engine, caret) {
        // GFM cells are not headings; rewriting the table eats `|`.
        return Ok(RichOutcome::Noop);
    }
    if engine
        .block_at(caret.cursor())
        .and_then(|id| engine.block(id))
        .is_some_and(|b| matches!(b.kind, BlockKind::LinkReferenceDefinition { .. }))
    {
        // Rewriting `[ref]: url` as a heading would drop the definition.
        return Ok(RichOutcome::Noop);
    }
    let Some(top) = engine.top_level_at(caret.cursor()).cloned() else {
        return Ok(RichOutcome::Noop);
    };
    if top.is_container() && !matches!(top.kind, BlockKind::BlockQuote | BlockKind::Alert { .. }) {
        return Ok(RichOutcome::Noop);
    }
    let mut rewritten = top.clone();
    let target = match kind {
        BlockType::Paragraph => BlockKind::Paragraph,
        BlockType::Heading(level) => BlockKind::Heading {
            level: level.clamp(1, 6),
            style: HeadingStyle::Atx,
        },
    };
    if rewritten.kind == target {
        return Ok(RichOutcome::Noop);
    }
    rewritten.kind = target;
    splice_serialized(doc, engine, caret, &top, &rewritten)
}

fn toggle_blockquote(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    if should_open_new_block_at_caret(&source, engine, caret) {
        return insert_block_open_at_caret(doc, engine, caret, "> ");
    }
    if selection_in_table(engine, caret) {
        return Ok(RichOutcome::Noop);
    }
    let Some(top) = engine.top_level_at(caret.cursor()).cloned() else {
        return Ok(RichOutcome::Noop);
    };
    if matches!(top.kind, BlockKind::BlockQuote | BlockKind::Alert { .. }) {
        let inner = if top.children.len() == 1 {
            top.children[0].clone()
        } else {
            Block {
                id: top.id,
                source_range: top.source_range.clone(),
                content_hash: top.content_hash,
                kind: BlockKind::Paragraph,
                children: top.children.clone(),
                inlines: top.inlines.clone(),
            }
        };
        return splice_serialized(doc, engine, caret, &top, &inner);
    }
    let wrapped = Block {
        id: top.id,
        source_range: top.source_range.clone(),
        content_hash: top.content_hash,
        kind: BlockKind::BlockQuote,
        children: vec![top.clone()],
        inlines: Vec::new(),
    };
    splice_serialized(doc, engine, caret, &top, &wrapped)
}

fn toggle_list(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    ordered: bool,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    if should_open_new_block_at_caret(&source, engine, caret) {
        let marker = if ordered { "1. " } else { "- " };
        return insert_block_open_at_caret(doc, engine, caret, marker);
    }
    if selection_in_table(engine, caret) {
        return Ok(RichOutcome::Noop);
    }
    let Some(top) = engine.top_level_at(caret.cursor()).cloned() else {
        return Ok(RichOutcome::Noop);
    };
    match &top.kind {
        BlockKind::BulletList { .. } if !ordered => {
            let inner = unwrap_list(&top);
            return splice_serialized(doc, engine, caret, &top, &inner);
        }
        BlockKind::OrderedList { .. } if ordered => {
            let inner = unwrap_list(&top);
            return splice_serialized(doc, engine, caret, &top, &inner);
        }
        BlockKind::BulletList { .. } | BlockKind::OrderedList { .. } => {
            // Convert list flavor by rewriting markers via serialize after kind change.
            let mut rewritten = top.clone();
            rewritten.kind = if ordered {
                BlockKind::OrderedList {
                    start: 1,
                    tight: true,
                    delimiter: b'.',
                }
            } else {
                BlockKind::BulletList {
                    tight: true,
                    marker: b'-',
                }
            };
            return splice_serialized(doc, engine, caret, &top, &rewritten);
        }
        _ => {}
    }
    let item = Block {
        id: top.id,
        source_range: top.source_range.clone(),
        content_hash: top.content_hash,
        kind: BlockKind::ListItem { task: None },
        children: vec![top.clone()],
        inlines: Vec::new(),
    };
    let list = Block {
        id: top.id,
        source_range: top.source_range.clone(),
        content_hash: top.content_hash,
        kind: if ordered {
            BlockKind::OrderedList {
                start: 1,
                tight: true,
                delimiter: b'.',
            }
        } else {
            BlockKind::BulletList {
                tight: true,
                marker: b'-',
            }
        },
        children: vec![item],
        inlines: Vec::new(),
    };
    splice_serialized(doc, engine, caret, &top, &list)
}

fn unwrap_list(list: &Block) -> Block {
    if list.children.len() == 1 {
        let item = &list.children[0];
        if item.children.len() == 1 {
            return item.children[0].clone();
        }
        return Block {
            id: list.id,
            source_range: list.source_range.clone(),
            content_hash: list.content_hash,
            kind: BlockKind::Paragraph,
            children: Vec::new(),
            inlines: item.inlines.clone(),
        };
    }
    list.clone()
}

fn set_task_checked(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    id: NodeId,
    checked: bool,
) -> Result<RichOutcome, RichError> {
    let Some(item) = engine.block(id) else {
        return Ok(RichOutcome::Noop);
    };
    if !matches!(item.kind, BlockKind::ListItem { task: Some(_) }) {
        return Ok(RichOutcome::Noop);
    }
    let source = doc.buffer.content();
    let slice = source.get(item.source_range.clone()).unwrap_or("");
    let (needle, replacement) = if checked {
        ("[ ]", "[x]")
    } else {
        ("[x]", "[ ]")
    };
    let Some(rel) = slice.find(needle).or_else(|| slice.find("[X]")) else {
        return Ok(RichOutcome::Noop);
    };
    let abs = item.source_range.start + rel;
    let before = caret.snapshot();
    doc.replace_range_tx(
        abs,
        abs + 3,
        replacement,
        TransactionKind::Command,
        before,
        caret.snapshot(),
    );
    engine.sync(doc);
    Ok(RichOutcome::Changed)
}

fn toggle_link(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    if !clamp_wrap_to_table_cell(engine, caret, &source) {
        return Ok(RichOutcome::Noop);
    }
    // `[[^1]]()` reparses as a wikilink. Second Cmd-K must unwrap to `[^1]`.
    if caret.range.is_empty() {
        if let Some(atom) = wrap_immune_range(engine, caret.cursor()) {
            if let Some((outer, inner)) = wiki_wrapping_footnote_link(&source, atom) {
                return splice_unwrap_wrapping_link(doc, engine, caret, outer, inner);
            }
        }
    }
    if wrap_is_immune(engine, caret)
        && wrap_widget_range_at(engine, &source, caret.cursor()).is_none()
        && !needs_newline_after_frontmatter_fence(&source, engine, caret.cursor())
    {
        return Ok(RichOutcome::Noop);
    }
    if caret.range.is_empty() {
        let offset = caret.cursor();
        let prefix = frontmatter_eof_body_prefix(&source, engine, offset);
        if prefix.is_empty() {
            // Dest-chrome widgets (`!`, `<img>`, `<br>`, `***`, `[^1]`,
            // `&amp;`, two-space / `\` hard breaks) are not word bytes, so
            // word_range is empty and Cmd-K used to splice `[]()` in front.
            // Wrap the widget like a selected word.
            if let Some(widget) = wrap_widget_range_at(engine, &source, offset) {
                caret.range = widget;
                caret.reversed = false;
                if !clamp_wrap_to_table_cell(engine, caret, &source) {
                    return Ok(RichOutcome::Noop);
                }
            } else {
                // Task `[x]` / wrap-mark `~~` / footnote-def `[` are prefix or
                // delimiter chrome. Sit in the body / inner word so Cmd-K does
                // not splice `[]()[x]` / `[]()~~`.
                relocate_empty_wrap_caret(engine, &source, caret);
                let at = caret.cursor();
                let word = word_range(&source, at);
                if !word.is_empty() {
                    caret.range = word;
                    caret.reversed = false;
                    // Word bounds cannot include `|`, but clamp if the caret sat on a
                    // pipe and snapped into a cell.
                    if !clamp_wrap_to_table_cell(engine, caret, &source) {
                        return Ok(RichOutcome::Noop);
                    }
                } else if let Some(label) = linked_label_range_at(engine, at) {
                    // `[***]()` / punctuation labels: `*` is not a word byte.
                    caret.range = label;
                    caret.reversed = false;
                    if !clamp_wrap_to_table_cell(engine, caret, &source) {
                        return Ok(RichOutcome::Noop);
                    }
                }
            }
        }
        if caret.range.is_empty() {
            let at = caret.cursor();
            splice(
                doc,
                caret,
                at,
                at,
                &format!("{prefix}[]()"),
                TransactionKind::Command,
            );
            // Caret inside the label (`[` after an optional fence newline).
            caret.collapse_to(at + prefix.len() + 1);
            engine.sync(doc);
            return Ok(RichOutcome::Changed);
        }
    }
    let sel = caret.range.clone();
    let Some(top) = engine.top_level_at(sel.start).cloned() else {
        return Ok(RichOutcome::Noop);
    };
    let leaf_id = engine.block_at(sel.start).unwrap_or(top.id);
    let mut rewritten = top.clone();
    let Some(leaf) = find_block_mut(&mut rewritten, leaf_id) else {
        return Ok(RichOutcome::Noop);
    };
    let already_link = leaf.inlines.iter().any(|inline| match inline {
        Inline::Run {
            source_range,
            link: Some(_),
            ..
        }
        | Inline::Image {
            source_range,
            link: Some(_),
            ..
        } => ranges_overlap(source_range, &sel),
        _ => false,
    });
    let source = doc.buffer.content();
    if let Some(outer) = wrapping_markdown_link_outer(&source, &sel) {
        return splice_unwrap_wrapping_link(doc, engine, caret, outer, sel);
    }
    if !inlines_can_tree_wrap_link(&leaf.inlines, &sel) {
        return splice_wrap_link_around(doc, engine, caret, sel);
    }
    toggle_link_inlines(&mut leaf.inlines, &sel, !already_link);
    let source = doc.buffer.content();
    let inner = source.get(sel.clone()).unwrap_or("").to_string();
    let new_md = serialize_block(&rewritten, &source);
    let before = caret.snapshot();
    let range = top.source_range.clone();
    // Source wrap leaves the caret in the URL `()` after wrapping a
    // selection (or a word). Keep that so Cmd-K can type the destination.
    let after = if !already_link {
        if let Some(rel) = link_url_caret_in(&new_md, &inner) {
            CaretState::collapsed(range.start + rel)
        } else {
            CaretState {
                range: range.start + (sel.start.saturating_sub(range.start))
                    ..range.start + (sel.end.saturating_sub(range.start)).min(new_md.len()),
                reversed: caret.reversed,
            }
        }
    } else {
        CaretState {
            range: range.start + (sel.start.saturating_sub(range.start))
                ..range.start + (sel.end.saturating_sub(range.start)).min(new_md.len()),
            reversed: caret.reversed,
        }
    };
    doc.replace_range_tx(
        range.start,
        range.end,
        &new_md,
        TransactionKind::Command,
        before,
        after.snapshot(),
    );
    *caret = after;
    caret.clamp(doc.buffer.len_bytes());
    engine.sync(doc);
    Ok(RichOutcome::Changed)
}

fn toggle_link_inlines(inlines: &mut [Inline], sel: &Range<usize>, wrap: bool) {
    let link = if wrap {
        Some(LinkAttrs {
            url: String::new(),
            title: None,
            autolink: false,
            angle: false,
            group: 1,
        })
    } else {
        None
    };
    for inline in inlines.iter_mut() {
        match inline {
            Inline::Run {
                source_range,
                link: slot,
                raw,
                ..
            } if ranges_overlap(source_range, sel) => {
                *slot = link.clone();
                *raw = None;
            }
            Inline::Image {
                source_range,
                link: slot,
                ..
            } if ranges_overlap(source_range, sel) => {
                *slot = link.clone();
            }
            _ => {}
        }
    }
}

/// Dest-chrome widget containing `offset` (not wrapping dest after `)` — that
/// is a leftover/EOF home). Empty-caret wrap uses this range as a unit.
fn wrap_widget_range_at(engine: &RichEngine, source: &str, offset: usize) -> Option<Range<usize>> {
    fn consider(widget: &Range<usize>, offset: usize) -> bool {
        widget.start <= offset && offset < widget.end
    }
    fn walk(blocks: &[Block], source: &str, offset: usize) -> Option<Range<usize>> {
        for b in blocks {
            if let Some(found) = walk(&b.children, source, offset) {
                return Some(found);
            }
            for inline in &b.inlines {
                if let Some(r) = atomic_delete_range(inline) {
                    if consider(&r, offset) {
                        // Two-space / `\` hard-break ranges include the
                        // newline; stripping it would wrap only `  ` / `\`
                        // and destroy the break.
                        if atomic_hard_break_range(inline).is_some() {
                            return Some(r);
                        }
                        return Some(inner_widget_wrap_range(source, b, r));
                    }
                }
                if let Inline::OpaqueInline {
                    raw, source_range, ..
                } = inline
                {
                    if crate::html_visual::html_solo_markup_inner_range(raw, source_range.clone())
                        .is_some()
                    {
                        let inner = inner_widget_wrap_range(source, b, source_range.clone());
                        if consider(&inner, offset) {
                            return Some(inner);
                        }
                    }
                }
                if let Some(r) = angle_autolink_wrap_range(source, inline, offset) {
                    return Some(r);
                }
            }
            if let Some(r) = html_block_image_range(b) {
                let inner = inner_widget_wrap_range(source, b, r);
                if consider(&inner, offset) {
                    return Some(inner);
                }
            }
            if let Some(r) = html_block_break_range(b) {
                let inner = inner_widget_wrap_range(source, b, r);
                if consider(&inner, offset) {
                    return Some(inner);
                }
            }
            if let Some(r) = thematic_break_range(b) {
                let inner = inner_widget_wrap_range(source, b, r);
                if consider(&inner, offset) {
                    return Some(inner);
                }
            }
        }
        None
    }
    walk(&engine.tree().blocks, source, offset)
}

/// Empty wrap on dest chrome must not splice `****` / `[]()` in front.
/// Sit on the Home/click insert home: list/quote/task/footnote-def prefixes,
/// wrap-mark openers, ATX `#`, setext underlines, table `|`, 0–3 space
/// heading indent, markdown-link `[`, HTML phrasing tags, and HTML-block
/// wrapper tags (`<div>`). Inline comments wrap as widgets instead.
fn relocate_empty_wrap_caret(engine: &RichEngine, source: &str, caret: &mut CaretState) -> bool {
    if !caret.range.is_empty() {
        return false;
    }
    let Some(home) = wrap_empty_caret_home(engine, source, caret.cursor()) else {
        return false;
    };
    caret.collapse_to(home);
    true
}

fn wrap_empty_caret_home(engine: &RichEngine, source: &str, offset: usize) -> Option<usize> {
    let home = dest_chrome_insert_home(engine, source, offset, true)?;
    wrap_immune_range(engine, home).is_none().then_some(home)
}

/// InsertText on a thematic widget: splice a paragraph above the rule, or
/// relocate / open a body line after it (close edge / leftover-below). Wrap
/// still uses `wrap_widget_range_at` and must not take this path.
enum ThematicInsert {
    Relocate(usize),
    /// Splice `opener` then type (type-at-start paragraph above).
    OpenAbove {
        at: usize,
        opener: String,
        caret: usize,
    },
    /// Insert at `at`, prepending a newline so following content is not eaten.
    OpenAfter(usize),
}

/// Type-at-start on `---` / `***` / `___` / `<hr>` must not rewrite the
/// rule into `x---` / a setext heading. Quoted keep `>`. Close-edge /
/// leftover-below open after the rule without nibbling dashes or the tag.
fn thematic_break_insert_plan(
    engine: &RichEngine,
    source: &str,
    offset: usize,
) -> Option<ThematicInsert> {
    let offset = offset.min(source.len());
    let (block, marker) = thematic_break_for_insert(engine, source, offset)?;
    // Empty Cmd-B/I inserts `****` / `____` on a new line; that parses as a
    // thematic rule. Filling the pair must stay `**x**`, not open a paragraph
    // above the wrap.
    if thematic_marker_is_empty_wrap(source, &marker, offset) {
        return None;
    }
    if offset < marker.end {
        let (at, opener, caret) = thematic_paragraph_above_opener(source, block);
        return Some(ThematicInsert::OpenAbove { at, opener, caret });
    }
    let home = thematic_after_home(source, &marker);
    let html_hr = matches!(block.kind, BlockKind::Opaque { .. });
    // CommonMark type-6 `<hr>` continues until a blank line. Typing in the
    // leftover gap (`<hr>\n\n`) would become `<hr>\nx` and swallow `x`.
    if html_hr && home < source.len() {
        return Some(ThematicInsert::OpenAfter(home));
    }
    if home >= source.len() || blank_caret_gap_at(engine.tree(), home).is_some() {
        return (home != offset).then_some(ThematicInsert::Relocate(home));
    }
    // Following content: open a body line after the rule (`---\nx\nworld`),
    // matching leftover-below (do not type into `world`).
    Some(ThematicInsert::OpenAfter(home))
}

/// Leftover / close-edge InsertText after HTML-block `<br>` must keep a blank
/// line (type-7 continues until a blank). Type-at-start on the tag is unchanged.
fn html_break_insert_plan(
    engine: &RichEngine,
    source: &str,
    offset: usize,
) -> Option<ThematicInsert> {
    let offset = offset.min(source.len());
    let (_, marker) = html_break_for_insert(engine, source, offset)?;
    if offset < marker.end {
        return None;
    }
    let home = thematic_after_home(source, &marker);
    if home < source.len() {
        Some(ThematicInsert::OpenAfter(home))
    } else {
        None
    }
}

fn html_break_for_insert<'a>(
    engine: &'a RichEngine,
    source: &str,
    offset: usize,
) -> Option<(&'a Block, Range<usize>)> {
    fn walk<'a>(
        blocks: &'a [Block],
        source: &str,
        offset: usize,
    ) -> Option<(&'a Block, Range<usize>)> {
        for b in blocks {
            if let Some(found) = walk(&b.children, source, offset) {
                return Some(found);
            }
            if html_block_break_range(b).is_none() {
                continue;
            }
            let marker = thematic_insert_marker_range(source, b);
            if marker.start >= marker.end {
                continue;
            }
            let home = thematic_after_home(source, &marker);
            if offset >= marker.end && offset <= home {
                return Some((b, marker));
            }
        }
        None
    }
    walk(&engine.tree().blocks, source, offset)
}

fn thematic_break_for_insert<'a>(
    engine: &'a RichEngine,
    source: &str,
    offset: usize,
) -> Option<(&'a Block, Range<usize>)> {
    fn walk<'a>(
        blocks: &'a [Block],
        source: &str,
        offset: usize,
    ) -> Option<(&'a Block, Range<usize>)> {
        for b in blocks {
            if let Some(found) = walk(&b.children, source, offset) {
                return Some(found);
            }
            if thematic_break_range(b).is_none() {
                continue;
            }
            let marker = thematic_insert_marker_range(source, b);
            if marker.start >= marker.end {
                continue;
            }
            let line_s = line_start(source, marker.start);
            let home = thematic_after_home(source, &marker);
            let on_prefix = offset >= line_s && offset < marker.start;
            let on_marker = offset >= marker.start && offset < marker.end;
            let on_close = offset >= marker.end && offset <= home;
            if on_prefix || on_marker || on_close {
                return Some((b, marker));
            }
        }
        None
    }
    walk(&engine.tree().blocks, source, offset)
}

/// Marker bytes (`---` / `***` / `<hr>`) without a trailing newline. List
/// prefix is stripped only for HTML-block `<hr>` in a list (`- <hr>`);
/// markdown `* * *` is a rule, not a list marker.
fn thematic_insert_marker_range(source: &str, block: &Block) -> Range<usize> {
    let mut start = block.source_range.start.min(source.len());
    let mut end = block.source_range.end.min(source.len());
    let bytes = source.as_bytes();
    while end > start && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    let line_s = line_start(source, start);
    let line = current_line(source, line_s);
    let prefix_end = if matches!(block.kind, BlockKind::Opaque { .. }) {
        line_s + quote_list_prefix_on_line(line).len()
    } else {
        line_s + quote_prefix(line).len()
    };
    if prefix_end > start && prefix_end <= end {
        start = prefix_end;
    }
    start..end
}

/// `****` / `____` on a line (empty wrap) with the caret strictly inside.
fn thematic_marker_is_empty_wrap(source: &str, marker: &Range<usize>, offset: usize) -> bool {
    if offset <= marker.start || offset >= marker.end {
        return false;
    }
    let slice = source.get(marker.clone()).unwrap_or("").trim();
    slice.len() >= 4 && (slice.bytes().all(|b| b == b'*') || slice.bytes().all(|b| b == b'_'))
}

fn thematic_after_home(source: &str, marker: &Range<usize>) -> usize {
    let le = line_end_exclusive(source, marker.start);
    if source.as_bytes().get(le) == Some(&b'\r') && source.as_bytes().get(le + 1) == Some(&b'\n') {
        le + 2
    } else if source.as_bytes().get(le) == Some(&b'\n') {
        le + 1
    } else {
        le
    }
}

/// Blank paragraph (or blank list item / quoted blank) before the rule so
/// `x` + `---` cannot parse as setext. Quoted insert `> x\n>\n> ---`.
fn thematic_paragraph_above_opener(source: &str, block: &Block) -> (usize, String, usize) {
    let insert_at = line_start(source, block.source_range.start);
    let line = current_line(source, insert_at);
    let quote = quote_prefix(line);
    let after = after_quote(line);
    if matches!(block.kind, BlockKind::Opaque { .. }) {
        if let Some(marker) = list_marker_prefix(after) {
            let prefix = format!("{quote}{marker}");
            return (insert_at, format!("{prefix}\n"), prefix.len());
        }
    }
    if let Some(prefix) = quote_marker_prefix(line) {
        let blank = prefix.trim_end();
        (insert_at, format!("{prefix}\n{blank}\n"), prefix.len())
    } else {
        (insert_at, "\n\n".to_string(), 0)
    }
}

/// Interior of atomic dest chrome is not an insert home: two-space / `\`
/// hard breaks, CommonMark `&amp;` / `\*` dest suffixes, and inline `<br>`.
/// The widget start stays "after the previous word" (`ax  \nb`, `Ax&amp;B`,
/// `ax<br>b`). Interior click/IME types on the next visible char.
fn atomic_dest_chrome_insert_home(
    engine: &RichEngine,
    source: &str,
    offset: usize,
) -> Option<usize> {
    fn interior(range: &Range<usize>, offset: usize) -> bool {
        offset > range.start && offset < range.end
    }
    fn walk(blocks: &[Block], offset: usize) -> Option<Range<usize>> {
        for b in blocks {
            for inline in &b.inlines {
                if let Some(r) = atomic_hard_break_range(inline) {
                    if interior(&r, offset) {
                        return Some(r);
                    }
                }
                if let Some(r) = atomic_character_reference_range(inline) {
                    if interior(&r, offset) {
                        return Some(r);
                    }
                }
                if let Some(r) = atomic_html_break_range(inline) {
                    if interior(&r, offset) {
                        return Some(r);
                    }
                }
            }
            if let Some(found) = walk(&b.children, offset) {
                return Some(found);
            }
        }
        None
    }
    let chrome = walk(&engine.tree().blocks, offset)?;
    let home =
        engine.clamp_raw_prefix(source, engine.next_caret(source, chrome.start), Bias::Right);
    (home != offset).then_some(home)
}

/// Insert / empty-wrap home when the caret sits on dest chrome. Does not
/// refuse wrap-immune homes so InsertText can still enter a `[ref]:` dest
/// or a cell; wrap callers drop immune homes themselves.
///
/// `include_wrap_marks` skips `*` / `~~` openers (empty wrap). InsertText
/// must not: a caret inside a fresh `**|**` pair would skip onto the
/// following letter (`[****xhello](url)`). HTML-block wrapper tags skip
/// onto inner markdown (close tags open a body paragraph after the block).
/// InsertText also skips autolink `<>` / `$math$` / `[[wiki]]` / dest `(`
/// openers, setext underlines, closed ATX trailing hashes, a leading
/// table `|`, GitHub `[!NOTE]` tags, hard-break interiors, `&amp;` / `\*`
/// dest suffixes, and inline `<br>` interiors; empty wrap still wraps those
/// widgets. Cell-end `|` stays the End insert home. Empty wrap splice
/// onto `[!NOTE]` / `[TOC]` is unchanged.
fn dest_chrome_insert_home(
    engine: &RichEngine,
    source: &str,
    offset: usize,
    include_wrap_marks: bool,
) -> Option<usize> {
    let offset = offset.min(source.len());
    // Only snap when the caret is on dest chrome. Blind Home-skip from a
    // trailing space / EOF would wrap the previous word (`hello []()` →
    // `[hello]()`).
    if let Some(home) = engine.link_or_html_phrasing_prefix_home(source, offset) {
        if home != offset {
            return Some(home);
        }
    }
    if let Some(home) = html_block_wrapper_insert_home(engine, source, offset) {
        if home != offset {
            return Some(home);
        }
    }
    if include_wrap_marks {
        if caret_on_empty_wrap_skip_chrome(engine, source, offset) {
            let home = engine.clamp_raw_prefix(
                source,
                engine.snap_caret(offset, Bias::Right),
                Bias::Right,
            );
            if home != offset {
                return Some(home);
            }
        }
        if let Some(home) = prefix_chrome_body_home(source, offset) {
            if home != offset {
                return Some(home);
            }
        }
        if let Some(home) = wrap_mark_opener_inner_home(engine, source, offset) {
            if home != offset {
                return Some(home);
            }
        }
    } else {
        // InsertText-only: autolink `<>` / `$math$` / `[[wiki]]` / dest `(`
        // openers. Empty wrap still wraps those widgets.
        if let Some(home) = engine.inline_dest_chrome_insert_home(source, offset) {
            if home != offset {
                return Some(home);
            }
        }
        // Two-space / `\` hard-break interior, `&amp;` / `\*` dest chrome,
        // and inline `<br>` interiors are not insert homes. Click/IME can
        // sit on those bytes when revealed; type on the next visible char
        // (`A&amp;xB`, `A\*xB`, `a<br>xb`). The widget start still extends
        // the previous word (`Ax&amp;B`, `ax<br>b`). Empty wrap still wraps.
        if let Some(home) = atomic_dest_chrome_insert_home(engine, source, offset) {
            if home != offset {
                return Some(home);
            }
        }
        // InsertText: list/quote/task/`[ref]:` prefixes are not insert homes
        // (same as empty wrap / Home/click). Snap so `[ref]:` `[` lands on
        // the label, not dest.
        if prefix_chrome_body_home(source, offset).is_some() {
            if let Some(home) = snap_right_insert_home(engine, source, offset) {
                return Some(home);
            }
        }
        // Setext `===` / closed ATX trailing `#`: Home/click skip onto the
        // title. Mid-document click on revealed underline used to glue
        // `Title\n===x`. Document EOF on that chrome still opens a body
        // line (`===\nx` / `# Title #\nx`). Open ATX `# Titlex` stays.
        if caret_on_heading_dest_chrome(engine, source, offset) {
            if let Some(home) = snap_right_insert_home(engine, source, offset) {
                return Some(home);
            }
        }
        // GitHub `[!NOTE]` / TIP / … tag is dest chrome (Home/click already
        // skip onto the title or body). InsertText used to splice `x[!NOTE]`.
        // Empty wrap splice onto the tag is unchanged.
        if caret_on_alert_tag(engine, offset) {
            if let Some(home) = snap_right_insert_home(engine, source, offset) {
                return Some(home);
            }
        }
        if engine.in_table(offset) {
            let outside = engine.cell_edit_range(offset, source).is_none();
            let alignment = outside && matches!(source.as_bytes().get(offset), Some(b'-' | b':'));
            // Leading `|` of `| a | b |` is Home chrome (`| xa |`). Exclusive
            // cell-end `|` stays the End insert home (`hellox|`) — do not
            // skip onto the next cell.
            if alignment || caret_on_leading_table_pipe(source, offset) {
                if let Some(home) = snap_right_insert_home(engine, source, offset) {
                    return Some(home);
                }
            }
        }
    }
    None
}

fn snap_right_insert_home(engine: &RichEngine, source: &str, offset: usize) -> Option<usize> {
    let home = engine.clamp_raw_prefix(source, engine.snap_caret(offset, Bias::Right), Bias::Right);
    (home != offset).then_some(home)
}

/// Insert home when the caret sits on an HTML-block wrapper tag (`<div>` /
/// `</div>`). Open tags skip onto inner markdown; close tags open a body
/// paragraph after the block (leftover-below / `</div>` EOF newline). Do
/// not nibble tags. Quoted keep `>`.
fn html_block_wrapper_insert_home(
    engine: &RichEngine,
    source: &str,
    offset: usize,
) -> Option<usize> {
    let block = raw_block_at(engine, offset)?;
    let BlockKind::Opaque { raw } = &block.kind else {
        return None;
    };
    if html_block_atomic_range(block).is_some() {
        return None;
    }
    let (open, close) = crate::html_visual::html_block_wrapper_tag_ranges(raw)?;
    if let Some(close) = close {
        if let Some(close_src) = html_wrapper_tag_in_source(source, block, raw, &close, true) {
            if offset >= close_src.start && offset < close_src.end {
                let home = html_block_after_close_home(source, block);
                return (home != offset).then_some(home);
            }
        }
    }
    if let Some(open_src) = html_wrapper_tag_in_source(source, block, raw, &open, false) {
        if offset >= open_src.start && offset < open_src.end {
            return (open_src.end != offset).then_some(open_src.end.min(source.len()));
        }
    }
    // Inner tags (`<span>` inside `<div>`) skip onto following inner text.
    // One tag only — do not walk onto the wrapper close.
    let body = raw_body_range(block, source);
    if let Some(tag) = html_tag_bounds(source, offset, body) {
        return (tag.end != offset).then_some(tag.end.min(source.len()));
    }
    None
}

fn html_wrapper_tag_in_source(
    source: &str,
    block: &Block,
    raw: &str,
    tag: &Range<usize>,
    last: bool,
) -> Option<Range<usize>> {
    let needle = raw.get(tag.clone()).filter(|s| !s.is_empty())?;
    let lo = block.source_range.start.min(source.len());
    let hi = block.source_range.end.min(source.len()).max(lo);
    let body = source.get(lo..hi)?;
    let rel = if last {
        body.rfind(needle)?
    } else {
        body.find(needle)?
    };
    let start = lo + rel;
    Some(start..start + needle.len())
}

fn html_block_after_close_home(source: &str, block: &Block) -> usize {
    let end = block.source_range.end.min(source.len());
    if source.as_bytes().get(end) == Some(&b'\r') && source.as_bytes().get(end + 1) == Some(&b'\n')
    {
        end + 2
    } else if source.as_bytes().get(end) == Some(&b'\n') {
        end + 1
    } else {
        end
    }
}

/// List/quote/task/`[^1]:` prefixes, ATX `#` / setext underlines / 0–3 space
/// heading indent, table `|` (plus alignment chrome), and markdown-link /
/// HTML phrasing open tags are not empty-wrap insert homes. Trailing
/// paragraph space is.
fn caret_on_empty_wrap_skip_chrome(engine: &RichEngine, source: &str, offset: usize) -> bool {
    if prefix_chrome_body_home(source, offset).is_some() {
        return true;
    }
    if engine
        .link_or_html_phrasing_prefix_home(source, offset)
        .is_some()
    {
        return true;
    }
    if offset < source.len() && engine.in_table(offset) {
        if is_unescaped_pipe(source, offset) {
            return true;
        }
        if engine.cell_edit_range(offset, source).is_none() {
            return true;
        }
    }
    caret_on_heading_dest_chrome(engine, source, offset)
}

fn caret_on_heading_dest_chrome(engine: &RichEngine, source: &str, offset: usize) -> bool {
    let Some(block) = engine.block_at(offset).and_then(|id| engine.block(id)) else {
        return false;
    };
    let BlockKind::Heading { style, .. } = &block.kind else {
        return false;
    };
    if offset >= block.source_range.end {
        return false;
    }
    if source.as_bytes().get(offset) == Some(&b'\n') {
        return false;
    }
    let start = line_start(source, offset);
    let line = current_line(source, start);
    let container = quote_list_prefix_on_line(line);
    let after = &line[container.len()..];
    match style {
        HeadingStyle::Setext => {
            is_setext_underline(after)
                || is_setext_underline(line)
                || cm_heading_opening_indent_at(start, container, after, offset)
        }
        HeadingStyle::Atx => caret_on_atx_marker_or_closer(start, container, after, offset),
    }
}

/// First unescaped `|` on a GFM table row (after quote/list prefix and
/// optional spaces). Typing here must skip onto the first cell (`| xa |`),
/// not glue `x| a |`. Later `|` bytes are cell End homes.
fn caret_on_leading_table_pipe(source: &str, offset: usize) -> bool {
    if !is_unescaped_pipe(source, offset) {
        return false;
    }
    let start = line_start(source, offset);
    let line = current_line(source, start);
    let prefix = quote_list_prefix_on_line(line);
    let mut i = start + prefix.len();
    while i < offset && matches!(source.as_bytes().get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    i == offset
}

/// GitHub alert `[!NOTE]` / `[!TIP]` / … tag bytes. Custom title after `]`
/// is a caret home; the tag itself is not.
fn caret_on_alert_tag(engine: &RichEngine, offset: usize) -> bool {
    fn walk(blocks: &[Block], offset: usize) -> bool {
        for b in blocks {
            if let BlockKind::Alert { tag_range, .. } = &b.kind {
                if offset >= tag_range.start && offset < tag_range.end {
                    return true;
                }
            }
            if walk(&b.children, offset) {
                return true;
            }
        }
        false
    }
    walk(&engine.tree().blocks, offset)
}

fn cm_heading_opening_indent_at(
    line_start: usize,
    container: &str,
    after: &str,
    offset: usize,
) -> bool {
    let indent = after.bytes().take_while(|&b| b == b' ').count();
    if indent == 0 || indent > 3 {
        return false;
    }
    let abs = line_start + container.len();
    offset >= abs && offset < abs + indent
}

fn caret_on_atx_marker_or_closer(
    line_start: usize,
    container: &str,
    after: &str,
    offset: usize,
) -> bool {
    let Some(marker) = atx_marker_prefix(after) else {
        return false;
    };
    let marker_start = line_start + container.len();
    let marker_end = marker_start + marker.len();
    if offset >= marker_start && offset < marker_end {
        return true;
    }
    if !atx_line_has_closing_hashes(after) {
        return false;
    }
    let rest = &after[marker.len()..];
    let stripped = strip_closing_atx(rest);
    let trail_start = marker_end + stripped.len();
    let line_end = marker_start + after.len();
    offset >= trail_start && offset < line_end
}

fn prefix_chrome_body_home(source: &str, offset: usize) -> Option<usize> {
    let start = line_start(source, offset);
    let line = current_line(source, start);
    let prefix = quote_list_prefix_on_line(line);
    if prefix.is_empty() {
        return None;
    }
    let body = start + prefix.len();
    if offset >= start && offset < body {
        Some(body.min(source.len()))
    } else {
        None
    }
}

/// `~~strike~~` / `**bold**` / `==mark==` opener (and closer) bytes are dest
/// chrome. Skip onto inner text so empty wrap does not splice `****~~`.
fn wrap_mark_opener_inner_home(engine: &RichEngine, source: &str, offset: usize) -> Option<usize> {
    let b = *source.as_bytes().get(offset)?;
    if !matches!(b, b'*' | b'_' | b'~' | b'=' | b'^' | b'`') {
        return None;
    }
    if !engine.byte_is_inline_chrome(source, offset) {
        return None;
    }
    let home = engine.clamp_raw_prefix(source, engine.snap_caret(offset, Bias::Right), Bias::Right);
    (home != offset).then_some(home)
}

/// Skip list/quote prefixes and a trailing newline so wrap is `[---]()` /
/// `[<img>]()`, not `[> ---]()`.
fn inner_widget_wrap_range(source: &str, block: &Block, mut range: Range<usize>) -> Range<usize> {
    range.end = range.end.min(source.len());
    range.start = range.start.min(range.end);
    while range.end > range.start && source.as_bytes()[range.end - 1] == b'\n' {
        range.end -= 1;
    }
    let prefix = raw_container_prefix(source, block);
    if !prefix.is_empty() {
        if let Some(head) = source.get(range.start..range.start + prefix.len()) {
            if head == prefix {
                let next = range.start + prefix.len();
                let rest = source.get(next..range.end).unwrap_or("");
                // `* * *` looks like a `* ` list marker plus `* *`. Only skip a
                // prefix when the remainder is still the widget.
                if rest.starts_with('<')
                    || crate::html_visual::html_inline_image(rest.trim()).is_some()
                    || is_thematic_break_source(rest)
                {
                    range.start = next;
                }
            }
        }
    }
    while range.start < range.end && matches!(source.as_bytes()[range.start], b' ' | b'\t') {
        range.start += 1;
    }
    range
}

fn inlines_can_tree_wrap_link(inlines: &[Inline], sel: &Range<usize>) -> bool {
    inlines.iter().any(|inline| match inline {
        Inline::Run { source_range, .. } | Inline::Image { source_range, .. } => {
            ranges_overlap(source_range, sel)
        }
        _ => false,
    })
}

fn inlines_can_tree_wrap_mark(inlines: &[Inline], sel: &Range<usize>) -> bool {
    inlines.iter().any(|inline| match inline {
        Inline::Run { source_range, .. }
        | Inline::Image { source_range, .. }
        | Inline::OpaqueInline { source_range, .. } => ranges_overlap(source_range, sel),
        _ => false,
    })
}

fn linked_label_range_at(engine: &RichEngine, offset: usize) -> Option<Range<usize>> {
    let block = engine.block(engine.block_at(offset)?)?;
    for inline in &block.inlines {
        match inline {
            Inline::Run {
                source_range,
                link: Some(link),
                ..
            }
            | Inline::Image {
                source_range,
                link: Some(link),
                ..
            } if !link.autolink && source_range.start <= offset && offset < source_range.end => {
                return Some(source_range.clone());
            }
            _ => {}
        }
    }
    None
}

fn wrapping_markdown_link_outer(source: &str, inner: &Range<usize>) -> Option<Range<usize>> {
    let outer = expand_around_markdown_link(source, inner.clone());
    if outer.start < inner.start
        && source.as_bytes().get(outer.start) == Some(&b'[')
        && outer.end > inner.end
    {
        Some(outer)
    } else {
        None
    }
}

/// `[[^1]]` / `[[^1]]()` is a wikilink after Cmd-K wraps a footnote ref.
/// Unwrap back to `[^1]`.
fn wiki_wrapping_footnote_link(
    source: &str,
    span: Range<usize>,
) -> Option<(Range<usize>, Range<usize>)> {
    let slice = source.get(span.clone())?;
    let body = slice.strip_prefix("[[")?.strip_suffix("]]")?;
    if !body.starts_with('^') {
        return None;
    }
    let footnote = format!("[{body}]");
    crate::html_visual::footnote_ref_label(&footnote)?;
    let inner = span.start + 1..span.end - 1;
    let mut outer = span;
    let rest = source.get(outer.end..).unwrap_or("");
    if rest.starts_with("()") {
        outer.end += 2;
    } else if let Some(after) = rest.strip_prefix('(') {
        if let Some(close) = after.find(')') {
            outer.end += 1 + close + 1;
        }
    }
    Some((outer, inner))
}

fn splice_unwrap_wrapping_link(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    outer: Range<usize>,
    inner: Range<usize>,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let widget = source.get(inner.clone()).unwrap_or("").to_string();
    splice(
        doc,
        caret,
        outer.start,
        outer.end,
        &widget,
        TransactionKind::Command,
    );
    caret.collapse_to(outer.start);
    engine.sync(doc);
    caret.clamp(doc.buffer.len_bytes());
    Ok(RichOutcome::Changed)
}

fn splice_wrap_link_around(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    sel: Range<usize>,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let inner = source.get(sel.clone()).unwrap_or("").to_string();
    let wrapped = format!("[{inner}]()");
    splice(
        doc,
        caret,
        sel.start,
        sel.end,
        &wrapped,
        TransactionKind::Command,
    );
    caret.collapse_to(sel.start + 1 + inner.len() + 2);
    engine.sync(doc);
    caret.clamp(doc.buffer.len_bytes());
    Ok(RichOutcome::Changed)
}

fn source_toggle_mark_around(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    range: Range<usize>,
    mark: MarkSet,
) -> Result<RichOutcome, RichError> {
    let Some((open, close)) = mark_wrap_delimiters(mark) else {
        return Ok(RichOutcome::Noop);
    };
    let source = doc.buffer.content();
    if range.start >= open.len()
        && range.end + close.len() <= source.len()
        && source.get(range.start - open.len()..range.start) == Some(open)
        && source.get(range.end..range.end + close.len()) == Some(close)
    {
        let inner = source[range.clone()].to_string();
        let outer_start = range.start - open.len();
        splice(
            doc,
            caret,
            outer_start,
            range.end + close.len(),
            &inner,
            TransactionKind::Command,
        );
        caret.collapse_to(outer_start);
        engine.sync(doc);
        caret.clamp(doc.buffer.len_bytes());
        return Ok(RichOutcome::Changed);
    }
    let inner = source.get(range.clone()).unwrap_or("").to_string();
    let wrapped = format!("{open}{inner}{close}");
    splice(
        doc,
        caret,
        range.start,
        range.end,
        &wrapped,
        TransactionKind::Command,
    );
    caret.collapse_to(range.start + open.len());
    engine.sync(doc);
    caret.clamp(doc.buffer.len_bytes());
    Ok(RichOutcome::Changed)
}

fn wrapping_code_span_to_unwrap(
    source: &str,
    span: Range<usize>,
) -> Option<(Range<usize>, String)> {
    if let Some(inner) = code_span_wrapping_widget(source, &span) {
        return Some((span, inner));
    }
    if span.start == 0 || source.as_bytes().get(span.start - 1) != Some(&b'`') {
        return None;
    }
    let mut tick_start = span.start;
    while tick_start > 0 && source.as_bytes()[tick_start - 1] == b'`' {
        tick_start -= 1;
    }
    let mut tick_end = span.end;
    while tick_end < source.len() && source.as_bytes()[tick_end] == b'`' {
        tick_end += 1;
    }
    let outer = tick_start..tick_end;
    let inner = code_span_wrapping_widget(source, &outer)?;
    Some((outer, inner))
}

fn code_span_wrapping_widget(source: &str, span: &Range<usize>) -> Option<String> {
    let slice = source.get(span.clone())?;
    let (body, nl) = slice
        .strip_suffix('\n')
        .map(|b| (b, true))
        .unwrap_or((slice, false));
    let inner = unwrap_matching_ticks(body)?;
    if looks_like_wrap_widget_markdown(inner) {
        Some(if nl {
            format!("{inner}\n")
        } else {
            inner.to_string()
        })
    } else {
        None
    }
}

fn unwrap_matching_ticks(slice: &str) -> Option<&str> {
    let bytes = slice.as_bytes();
    let mut n = 0;
    while n < bytes.len() && bytes[n] == b'`' {
        n += 1;
    }
    if n == 0 || slice.len() < 2 * n {
        return None;
    }
    if !slice[slice.len() - n..].bytes().all(|b| b == b'`') {
        return None;
    }
    Some(&slice[n..slice.len() - n])
}

fn looks_like_wrap_widget_markdown(s: &str) -> bool {
    let t = s.trim();
    if t.starts_with("![") && t.contains("](") {
        return true;
    }
    if crate::html_visual::html_inline_image(t).is_some() {
        return true;
    }
    if crate::html_visual::html_inline_break(t) {
        return true;
    }
    if crate::html_visual::footnote_ref_label(t).is_some() {
        return true;
    }
    if t.len() >= 3 && t[..3].eq_ignore_ascii_case("<hr") {
        return true;
    }
    if crate::rich::escape::looks_like_entity(t) {
        return true;
    }
    if t.starts_with('\\') && t.chars().count() == 2 {
        return true;
    }
    let hb = s.trim_end_matches(['\n', '\r']);
    if hb == "\\" || (hb.len() >= 2 && hb.bytes().all(|b| b == b' ')) {
        return true;
    }
    if t.starts_with('<') && t.ends_with('>') && t.len() > 2 {
        return true;
    }
    is_thematic_break_source(t)
}

/// Angle-wrapped autolink `<>` dest chrome. Empty wrap on `<` / `>` wraps
/// the whole `<https://…>` / `<user@host>` (GFM www/email without `<>` is a
/// word, so empty Cmd-B still inserts `****`).
fn angle_autolink_wrap_range(source: &str, inline: &Inline, offset: usize) -> Option<Range<usize>> {
    let Inline::Run {
        source_range,
        link: Some(link),
        ..
    } = inline
    else {
        return None;
    };
    let bytes = source.as_bytes();
    let mut outer = source_range.clone();
    if outer.start > 0
        && bytes[outer.start - 1] == b'<'
        && outer.end < bytes.len()
        && bytes[outer.end] == b'>'
    {
        outer.start -= 1;
        outer.end += 1;
    } else if !link.angle && !link.autolink {
        return None;
    }
    if source.as_bytes().get(outer.start) != Some(&b'<')
        || outer.end == 0
        || source.as_bytes().get(outer.end - 1) != Some(&b'>')
    {
        return None;
    }
    if offset == outer.start || offset == outer.end - 1 {
        Some(outer)
    } else {
        None
    }
}

fn is_thematic_break_source(t: &str) -> bool {
    let t = t.trim();
    let mut kind = None;
    let mut n = 0usize;
    for c in t.chars() {
        if c == ' ' || c == '\t' {
            continue;
        }
        match kind {
            None if matches!(c, '-' | '*' | '_') => {
                kind = Some(c);
                n = 1;
            }
            Some(k) if c == k => n += 1,
            _ => return false,
        }
    }
    n >= 3
}

/// Byte offset of the empty URL slot in `[label]()` (after `](`).
fn link_url_caret_in(md: &str, inner: &str) -> Option<usize> {
    let needle = format!("[{inner}](");
    if let Some(at) = md.find(&needle) {
        return Some(at + needle.len());
    }
    md.find("]()").map(|i| i + 2)
}

/// If `offset` sits on an empty `<>` link/image destination, the range of
/// those two bytes so InsertText can replace them (not type inside).
fn empty_angle_destination(source: &str, offset: usize) -> Option<Range<usize>> {
    let start = if source.get(offset..).is_some_and(|s| s.starts_with("<>")) {
        offset
    } else if offset > 0
        && source
            .get(offset - 1..)
            .is_some_and(|s| s.starts_with("<>"))
    {
        offset - 1
    } else {
        return None;
    };
    let before = source.get(..start)?;
    let dest_open = before.trim_end_matches([' ', '\t']);
    if dest_open.ends_with("](") {
        Some(start..start + 2)
    } else {
        None
    }
}

fn word_range(source: &str, offset: usize) -> Range<usize> {
    let offset = offset.min(source.len());
    let bytes = source.as_bytes();
    let mut start = offset;
    while start > 0 && is_word_byte(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = offset;
    while end < source.len() && is_word_byte(bytes[end]) {
        end += 1;
    }
    start..end
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn indent_list(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    engine.sync(doc);
    // Tab in a table is cell navigation (Typora), not indent. Check before
    // raw-context so inline code inside a cell still TableTabs.
    if engine.in_table(caret.cursor()) {
        return table_tab(doc, engine, caret, false);
    }
    if engine.in_raw_context(caret.cursor()) {
        if in_raw_block(engine, caret.cursor()) {
            let source = doc.buffer.content();
            let at = clamp_to_raw_edit(engine, &source, caret.cursor());
            caret.collapse_to(at);
        }
        return insert_text(doc, engine, caret, "  ");
    }
    let source = doc.buffer.content();
    let Some(id) = engine.block_at(caret.cursor()) else {
        return insert_text(doc, engine, caret, "  ");
    };
    let Some(item) = ancestor_list_item(engine, id).cloned() else {
        return insert_text(doc, engine, caret, "  ");
    };
    let range = item_visual_range(&source, &item);
    let slice = source.get(range.clone()).unwrap_or("");
    rewrite_range(doc, engine, caret, range, &prefix_item_lines(slice, 2))
}

fn outdent_list(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    engine.sync(doc);
    if engine.in_table(caret.cursor()) {
        return table_tab(doc, engine, caret, true);
    }
    if in_raw_block(engine, caret.cursor()) {
        return unindent_raw_line(doc, engine, caret);
    }
    outdent_current_list_line(doc, engine, caret)
}

/// Shift-Tab inside a fence / HTML block: strip one tab or up to two leading
/// spaces on the current **body** line, after the list/quote prefix. Never
/// treat a code line as a list item, and never strip the indent that keeps a
/// nested fence inside `- ` / `>`.
fn unindent_raw_line(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    engine.sync(doc);
    let source = doc.buffer.content();
    let offset = caret.cursor();
    let Some(block) = raw_block_at(engine, offset) else {
        return Ok(RichOutcome::Noop);
    };
    let body = raw_body_range(block, &source);
    let start = line_start(&source, offset);
    let line = current_line(&source, offset).to_string();
    let line_end = start + line.len();
    // Opening/closing fence (or HTML) chrome is not body indent.
    if line_end <= body.start || start > body.end {
        return Ok(RichOutcome::Noop);
    }
    let prefix = raw_container_prefix(&source, block);
    if !line.starts_with(&prefix) {
        return Ok(RichOutcome::Noop);
    }
    let rest = &line[prefix.len()..];
    let stripped = if let Some(r) = rest.strip_prefix('\t') {
        r.to_string()
    } else {
        let n = rest.bytes().take(2).take_while(|b| *b == b' ').count();
        if n == 0 {
            return Ok(RichOutcome::Noop);
        }
        rest[n..].to_string()
    };
    let from = start + prefix.len();
    rewrite_range(doc, engine, caret, from..start + line.len(), &stripped)
}

fn in_raw_block(engine: &RichEngine, offset: usize) -> bool {
    raw_block_at(engine, offset).is_some()
}

fn raw_block_at(engine: &RichEngine, offset: usize) -> Option<&Block> {
    let id = engine.block_at(offset)?;
    let block = engine.block(id)?;
    matches!(
        block.kind,
        BlockKind::CodeBlock { .. } | BlockKind::Opaque { .. }
    )
    .then_some(block)
}

/// Clamp into the raw body and past the list/quote prefix on that line.
///
/// Only pulls the caret *forward* (indented-code indent, opening fence
/// ticks). Never pulls it back from closing-fence / leftover-EOF dest
/// chrome, or from a wrap pair that was just spliced after an unclosed
/// opener (` ```rust\n**|** `) — those must stay so typing opens a body
/// line instead of splicing into the fence.
fn clamp_to_raw_edit(engine: &RichEngine, source: &str, offset: usize) -> usize {
    let Some(block) = raw_block_at(engine, offset) else {
        return offset;
    };
    let body = raw_body_range(block, source);
    if offset > body.end {
        return offset;
    }
    let at = offset.max(body.start);
    let prefix = raw_container_prefix(source, block);
    let fence_offset = match &block.kind {
        BlockKind::CodeBlock { fence: Some(f), .. } => f.fence_offset,
        _ => 0,
    };
    let ls = line_start(source, at);
    let le = line_end_exclusive(source, at);
    let line = &source[ls..le];
    let skip = skip_line_prefix_and_fence(line, &prefix, fence_offset);
    let content = (ls + skip).clamp(body.start, body.end);
    if at < content {
        content
    } else {
        at
    }
}

fn insert_raw_newline(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    source: &str,
    offset: usize,
) -> Result<RichOutcome, RichError> {
    let at = clamp_to_raw_edit(engine, source, offset);
    let prefix = raw_block_at(engine, offset)
        .map(|block| raw_container_prefix(source, block))
        .unwrap_or_default();
    splice(
        doc,
        caret,
        at,
        at,
        &format!("\n{prefix}"),
        TransactionKind::Command,
    );
    engine.sync(doc);
    Ok(RichOutcome::Changed)
}

fn backspace_in_raw_block(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    source: &str,
    to: usize,
) -> Result<Option<RichOutcome>, RichError> {
    let Some(block) = raw_block_at(engine, to) else {
        return Ok(None);
    };
    if html_block_atomic_range(block).is_some() {
        return Ok(None);
    }
    let body = raw_body_range(block, source);
    let prefix = raw_container_prefix(source, block);
    let fence_offset = match &block.kind {
        BlockKind::CodeBlock { fence: Some(f), .. } => f.fence_offset,
        _ => 0,
    };
    let first_line_start = line_start(source, body.start);
    let first_line_end = line_end_exclusive(source, body.start);
    let first_line = &source[first_line_start..first_line_end];
    let first_skip = skip_line_prefix_and_fence(first_line, &prefix, fence_offset);
    let first_content = (first_line_start + first_skip).clamp(body.start, body.end);
    if to <= first_content {
        return Ok(Some(RichOutcome::Noop));
    }
    let line_s = line_start(source, to);
    let line_e = line_end_exclusive(source, to);
    let line = &source[line_s..line_e];
    let content = line_s + skip_line_prefix_and_fence(line, &prefix, fence_offset);
    if line_s > first_line_start && to <= content {
        if source.as_bytes().get(line_s - 1) == Some(&b'\n') {
            let del_end = content.min(line_end_exclusive(source, to)).max(line_s);
            caret.range = (line_s - 1)..del_end;
            caret.reversed = true;
            delete_range(doc, engine, caret, TransactionKind::DeleteBack)?;
            return Ok(Some(RichOutcome::Changed));
        }
        return Ok(Some(RichOutcome::Noop));
    }
    Ok(None)
}

/// Delete at/past the end of a fence or HTML body is a no-op (does not nibble
/// closing ticks or `>`). Opening chrome is the same. Inside the body, Delete
/// removes one grapheme and will not cross `body.end`.
fn delete_forward_in_raw_block(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    source: &str,
    from: usize,
) -> Result<Option<RichOutcome>, RichError> {
    let Some(block) = raw_block_at(engine, from) else {
        return Ok(None);
    };
    if html_block_atomic_range(block).is_some() {
        return Ok(None);
    }
    let body = raw_body_range(block, source);
    if from < body.start || from >= body.end {
        return Ok(Some(RichOutcome::Noop));
    }
    // One source grapheme, not prefix-skipping `next_caret` (that would eat
    // `>` / list indent when Delete is at a line break).
    let to = step_right_in_slice(source, from, body.end).min(body.end);
    if to <= from {
        return Ok(Some(RichOutcome::Noop));
    }
    caret.range = from..to;
    caret.reversed = false;
    delete_range(doc, engine, caret, TransactionKind::Command)?;
    Ok(Some(RichOutcome::Changed))
}

fn outdent_current_list_line(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let offset = caret.cursor();
    let start = line_start(&source, offset);
    let line = current_line(&source, offset);
    let quote = quote_prefix(line).to_string();
    let after = after_quote(line).to_string();
    if list_marker_prefix(&after).is_none() {
        let Some(id) = engine.block_at(offset) else {
            return Ok(RichOutcome::Noop);
        };
        let Some(item) = ancestor_list_item(engine, id).cloned() else {
            return Ok(RichOutcome::Noop);
        };
        let range = item_visual_range(&source, &item);
        let slice = source.get(range.clone()).unwrap_or("");
        let first = slice.split('\n').next().unwrap_or(slice);
        let indent = after_quote(first)
            .bytes()
            .take_while(|b| *b == b' ' || *b == b'\t')
            .count();
        if indent >= 2 {
            return rewrite_range(doc, engine, caret, range, &unprefix_item_lines(slice, 2));
        }
        return Ok(RichOutcome::Noop);
    }
    let indent = after
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    if indent >= 2 {
        let new_line = format!("{quote}{}", unprefix_item_lines(&after, 2));
        return rewrite_range(doc, engine, caret, start..start + line.len(), &new_line);
    }
    // Exit the list: drop this empty (or top-level) item line.
    let prefix = list_marker_prefix(&after).unwrap_or_default();
    let rest = after.get(prefix.len()..).unwrap_or("");
    if rest.trim().is_empty() {
        if !quote.is_empty() {
            // Typora: empty quoted list item becomes an empty quoted paragraph.
            return rewrite_range(doc, engine, caret, start..start + line.len(), &quote);
        }
        let mut from = start;
        let mut to = start + line.len();
        if source.as_bytes().get(to) == Some(&b'\n') {
            to += 1;
        }
        let mut replacement = String::new();
        if from > 0 && source.as_bytes()[from - 1] == b'\n' {
            from -= 1;
            replacement = "\n\n".to_string();
        }
        return rewrite_range(doc, engine, caret, from..to, &replacement);
    }
    let new_line = format!("{quote}{rest}");
    rewrite_range(doc, engine, caret, start..start + line.len(), &new_line)
}

fn item_visual_range(source: &str, item: &Block) -> Range<usize> {
    let start = line_start(source, item.source_range.start);
    start..item.source_range.end.max(start)
}

fn rewrite_range(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    range: Range<usize>,
    new_md: &str,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let slice = source.get(range.clone()).unwrap_or("");
    if new_md == slice {
        return Ok(RichOutcome::Noop);
    }
    let before = caret.snapshot();
    let delta = new_md.len() as isize - slice.len() as isize;
    let new_cursor = ((caret.cursor() as isize) + delta).max(range.start as isize) as usize;
    let after = CaretState::collapsed(new_cursor.min(range.start + new_md.len()));
    doc.replace_range_tx(
        range.start,
        range.end,
        new_md,
        TransactionKind::Command,
        before,
        after.snapshot(),
    );
    *caret = after;
    engine.sync(doc);
    caret.clamp(doc.buffer.len_bytes());
    Ok(RichOutcome::Changed)
}

fn prefix_item_lines(slice: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    map_item_lines(slice, |line| {
        if line.is_empty() {
            return String::new();
        }
        // Indent after `>` so Tab on a quoted list becomes `>   - item`,
        // not a leading space before the quote (`  > - item`).
        let quote = quote_prefix(line);
        format!("{quote}{pad}{}", after_quote(line))
    })
}

fn unprefix_item_lines(slice: &str, n: usize) -> String {
    map_item_lines(slice, |line| {
        let quote = quote_prefix(line);
        format!("{quote}{}", strip_line_indent(after_quote(line), n))
    })
}

fn map_item_lines(slice: &str, mut map: impl FnMut(&str) -> String) -> String {
    let trailing_nl = slice.ends_with('\n');
    let body = slice.strip_suffix('\n').unwrap_or(slice);
    let mut out = String::new();
    for (i, line) in body.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&map(line));
    }
    if trailing_nl {
        out.push('\n');
    }
    out
}

fn strip_line_indent(line: &str, n: usize) -> String {
    if let Some(rest) = line.strip_prefix('\t') {
        return rest.to_string();
    }
    let mut take = 0usize;
    for (idx, b) in line.bytes().enumerate() {
        if b == b' ' && idx < n {
            take += 1;
        } else {
            break;
        }
    }
    line[take..].to_string()
}

fn splice_serialized(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    original: &Block,
    rewritten: &Block,
) -> Result<RichOutcome, RichError> {
    let source = doc.buffer.content();
    let new_md = serialize_block(rewritten, &source);
    let before = caret.snapshot();
    let range = original.source_range.clone();
    let rel = caret.cursor().saturating_sub(range.start).min(new_md.len());
    let after = CaretState::collapsed(range.start + rel);
    doc.replace_range_tx(
        range.start,
        range.end,
        &new_md,
        TransactionKind::Command,
        before,
        after.snapshot(),
    );
    *caret = after;
    engine.sync(doc);
    caret.clamp(doc.buffer.len_bytes());
    Ok(RichOutcome::Changed)
}

fn splice(
    doc: &mut Document,
    caret: &mut CaretState,
    start: usize,
    end: usize,
    text: &str,
    kind: TransactionKind,
) {
    let before = caret.snapshot();
    let after = CaretState::collapsed(start + text.len());
    doc.replace_range_tx(start, end, text, kind, before, after.snapshot());
    *caret = after;
}

fn find_block_mut(block: &mut Block, id: NodeId) -> Option<&mut Block> {
    if block.id == id {
        return Some(block);
    }
    for child in &mut block.children {
        if let Some(found) = find_block_mut(child, id) {
            return Some(found);
        }
    }
    None
}

fn set_code_info(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    id: NodeId,
    info: &str,
) -> Result<RichOutcome, RichError> {
    let Some(block) = engine.block(id) else {
        return Ok(RichOutcome::Noop);
    };
    if !matches!(block.kind, BlockKind::CodeBlock { .. }) {
        return Ok(RichOutcome::Noop);
    }
    let Some(top) = engine.top_level_at(block.source_range.start).cloned() else {
        return Ok(RichOutcome::Noop);
    };
    let mut rewritten = top.clone();
    let Some(target) = find_block_mut(&mut rewritten, id) else {
        return Ok(RichOutcome::Noop);
    };
    let BlockKind::CodeBlock { info: slot, .. } = &mut target.kind else {
        return Ok(RichOutcome::Noop);
    };
    let cleaned = sanitize_info(info);
    if *slot == cleaned {
        return Ok(RichOutcome::Noop);
    }
    *slot = cleaned;
    splice_serialized(doc, engine, caret, &top, &rewritten)
}

fn sanitize_info(info: &str) -> String {
    info.chars()
        .map(|c| {
            if matches!(c, '\n' | '\r' | '`') {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

fn set_image_alt(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    image_range: Range<usize>,
    alt: &str,
) -> Result<RichOutcome, RichError> {
    let Some(top) = engine.top_level_at(image_range.start).cloned() else {
        return Ok(RichOutcome::Noop);
    };
    let leaf_id = engine.block_at(image_range.start).unwrap_or(top.id);
    let mut rewritten = top.clone();
    let Some(leaf) = find_block_mut(&mut rewritten, leaf_id) else {
        return Ok(RichOutcome::Noop);
    };
    let mut found = false;
    for inline in &mut leaf.inlines {
        if let Inline::Image {
            source_range,
            alt: slot,
            ..
        } = inline
        {
            if *source_range == image_range || source_range.start == image_range.start {
                *slot = alt.replace(['\n', '\r'], " ");
                found = true;
                break;
            }
        }
    }
    if !found {
        return Ok(RichOutcome::Noop);
    }
    splice_serialized(doc, engine, caret, &top, &rewritten)
}

fn set_frontmatter(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    raw: &str,
) -> Result<RichOutcome, RichError> {
    engine.sync(doc);
    let wrapped = crate::normalize_frontmatter(raw).map_err(RichError::InvalidFrontmatter)?;
    let existing = engine.tree().frontmatter.clone();
    let source = doc.buffer.content();
    let (range, insert) = match existing {
        Some(Frontmatter { source_range, .. }) => {
            let mut to = source_range.end.min(source.len());
            if wrapped.is_empty() {
                while to < source.len() && matches!(source.as_bytes()[to], b'\n' | b'\r') {
                    to += 1;
                    if source.as_bytes().get(to - 1) == Some(&b'\n') {
                        break;
                    }
                }
                (source_range.start..to, String::new())
            } else {
                (source_range, wrapped)
            }
        }
        None => {
            if wrapped.is_empty() {
                return Ok(RichOutcome::Noop);
            }
            let insert = if source.is_empty() || source.starts_with('\n') {
                wrapped
            } else {
                format!("{wrapped}\n")
            };
            (0..0, insert)
        }
    };
    rewrite_range(doc, engine, caret, range, &insert)
}

fn set_frontmatter_field(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    key: &str,
    value: &str,
) -> Result<RichOutcome, RichError> {
    engine.sync(doc);
    let raw = engine
        .tree()
        .frontmatter
        .as_ref()
        .map(|fm| fm.raw.clone())
        .unwrap_or_default();
    let updated =
        crate::upsert_yaml_key(&raw, key, value).map_err(RichError::InvalidFrontmatter)?;
    set_frontmatter(doc, engine, caret, &updated)
}

fn table_tab(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    reverse: bool,
) -> Result<RichOutcome, RichError> {
    engine.sync(doc);
    let Some(pos) = engine.table_pos(caret.cursor()) else {
        return Ok(RichOutcome::Noop);
    };
    let (row, col) = if reverse {
        if pos.col > 0 {
            (pos.row, pos.col - 1)
        } else if pos.row > 0 {
            (pos.row - 1, pos.n_cols.saturating_sub(1))
        } else {
            // Typora-class: last-cell Tab inserts a row; first-cell Shift-Tab
            // leaves the table (does not rewrite pipes). Sit on the blank
            // before it, else the previous block, else open a blank above.
            return exit_first_table_cell(doc, engine, caret, pos);
        }
    } else if pos.col + 1 < pos.n_cols {
        (pos.row, pos.col + 1)
    } else if pos.row + 1 < pos.n_rows {
        (pos.row + 1, 0)
    } else {
        insert_table_row(doc, engine, caret, true)?;
        engine.sync(doc);
        let Some(pos) = engine.table_pos(caret.cursor()) else {
            return Ok(RichOutcome::Changed);
        };
        caret.collapse_to(
            engine
                .cell_caret(pos.table_id, pos.n_rows.saturating_sub(1), 0)
                .unwrap_or(caret.cursor()),
        );
        return Ok(RichOutcome::Changed);
    };
    if let Some(offset) = engine.cell_caret(pos.table_id, row, col) {
        caret.collapse_to(offset);
    }
    Ok(RichOutcome::Changed)
}

/// Shift-Tab in the first cell: leave the table without rewriting GFM pipes.
/// A painted blank immediately before the table is the home; otherwise the
/// previous sibling; otherwise insert a blank paragraph above (quoted/list
/// lines keep their prefix, like Enter at the start of a heading).
fn exit_first_table_cell(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    pos: TablePos,
) -> Result<RichOutcome, RichError> {
    let Some(table) = engine.block(pos.table_id) else {
        return Ok(RichOutcome::Noop);
    };
    let table_id = pos.table_id;
    let table_start = table.source_range.start;
    if let Some(home) = caret_home_before_table(engine, table_id, table_start) {
        place_table_exit_caret(engine, caret, &doc.buffer.content(), home);
        return Ok(RichOutcome::Changed);
    }
    let source = doc.buffer.content();
    let insert_at = line_start(&source, table_start).max(frontmatter_body_start(engine.tree()));
    let line = current_line(&source, insert_at);
    let quote = quote_prefix(line);
    let (text, new_cursor) = if let Some(marker) = list_marker_prefix(after_quote(line)) {
        let prefix = format!("{quote}{marker}");
        (format!("{prefix}\n"), insert_at + prefix.len())
    } else if let Some(prefix) = quote_marker_prefix(line) {
        (format!("{prefix}\n"), insert_at + prefix.len())
    } else {
        ("\n\n".to_string(), insert_at)
    };
    let before = caret.snapshot();
    let after = CaretState::collapsed(new_cursor);
    doc.replace_range_tx(
        insert_at,
        insert_at,
        &text,
        TransactionKind::Command,
        before,
        after.snapshot(),
    );
    *caret = after;
    engine.sync(doc);
    let source = doc.buffer.content();
    let probe = (table_start + text.len()).min(source.len().saturating_sub(1));
    if let Some(pos) = engine.table_pos(probe) {
        if let Some(table) = engine.block(pos.table_id) {
            if let Some(home) =
                caret_home_before_table(engine, pos.table_id, table.source_range.start)
            {
                place_table_exit_caret(engine, caret, &source, home);
                return Ok(RichOutcome::Changed);
            }
        }
    }
    place_table_exit_caret(engine, caret, &source, caret.cursor());
    if engine.in_table(caret.cursor()) {
        open_blank_before_table(doc, engine, caret);
    }
    Ok(RichOutcome::Changed)
}

/// YAML can swallow a leading body `\n` after `---`. Open one more blank so
/// the caret has a body home that is not a table cell.
fn open_blank_before_table(doc: &mut Document, engine: &mut RichEngine, caret: &mut CaretState) {
    let at = frontmatter_body_start(engine.tree()).min(doc.buffer.len_bytes());
    if !engine.in_table(at) {
        place_table_exit_caret(engine, caret, &doc.buffer.content(), at);
        return;
    }
    let before = caret.snapshot();
    let after = CaretState::collapsed(at);
    doc.replace_range_tx(
        at,
        at,
        "\n",
        TransactionKind::Command,
        before,
        after.snapshot(),
    );
    *caret = after;
    engine.sync(doc);
    let source = doc.buffer.content();
    let home = frontmatter_body_start(engine.tree());
    place_table_exit_caret(engine, caret, &source, home);
}

fn caret_home_before_table(
    engine: &RichEngine,
    table_id: NodeId,
    table_start: usize,
) -> Option<usize> {
    if let Some(gap) = blank_caret_gaps(engine.tree())
        .into_iter()
        .find(|gap| gap.end == table_start)
    {
        return Some(gap.start);
    }
    let prev = previous_sibling_block(engine, table_id)?;
    let home = last_visible_body_end(prev).unwrap_or(prev.source_range.end);
    if engine.in_table(home) {
        None
    } else {
        Some(home)
    }
}

fn previous_sibling_block(engine: &RichEngine, id: NodeId) -> Option<&Block> {
    fn walk(blocks: &[Block], id: NodeId) -> Result<Option<&Block>, ()> {
        let mut prev = None;
        for b in blocks {
            if b.id == id {
                return Ok(prev);
            }
            if let Ok(found) = walk(&b.children, id) {
                return Ok(found);
            }
            prev = Some(b);
        }
        Err(())
    }
    walk(&engine.tree().blocks, id).ok().flatten()
}

fn place_table_exit_caret(engine: &RichEngine, caret: &mut CaretState, source: &str, home: usize) {
    let fm_end = frontmatter_body_start(engine.tree());
    let at = home.min(source.len()).max(fm_end);
    let snapped = engine
        .clamp_raw_prefix(source, engine.snap_caret(at, Bias::Right), Bias::Right)
        .max(fm_end);
    caret.collapse_to(if engine.in_table(snapped) {
        at
    } else {
        snapped
    });
}

fn insert_table_row(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    after: bool,
) -> Result<RichOutcome, RichError> {
    let Some(pos) = engine.table_pos(caret.cursor()) else {
        return Ok(RichOutcome::Noop);
    };
    let new_row = if after { pos.row + 1 } else { pos.row };
    let col = pos.col;
    let table_start = engine
        .block(pos.table_id)
        .map(|t| t.source_range.start)
        .unwrap_or(caret.cursor());
    rewrite_table(doc, engine, caret, pos, |table, pos| {
        let cols = table
            .children
            .first()
            .map(|r| r.children.len())
            .unwrap_or(0)
            .max(1);
        let row = empty_row(false, cols);
        let idx = if after { pos.row + 1 } else { pos.row };
        table.children.insert(idx.min(table.children.len()), row);
        normalize_table_headers(table);
    })?;
    place_table_caret(engine, caret, table_start, new_row, col);
    Ok(RichOutcome::Changed)
}

fn insert_table_column(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    after: bool,
) -> Result<RichOutcome, RichError> {
    let Some(pos) = engine.table_pos(caret.cursor()) else {
        return Ok(RichOutcome::Noop);
    };
    let new_col = if after { pos.col + 1 } else { pos.col };
    let row = pos.row;
    let table_start = engine
        .block(pos.table_id)
        .map(|t| t.source_range.start)
        .unwrap_or(caret.cursor());
    rewrite_table(doc, engine, caret, pos, |table, pos| {
        let idx = if after { pos.col + 1 } else { pos.col };
        if let BlockKind::Table { alignments } = &mut table.kind {
            let at = idx.min(alignments.len());
            alignments.insert(at, ColumnAlign::None);
        }
        for row in &mut table.children {
            let at = idx.min(row.children.len());
            row.children.insert(at, empty_cell());
        }
        normalize_table_headers(table);
    })?;
    place_table_caret(engine, caret, table_start, row, new_col);
    Ok(RichOutcome::Changed)
}

fn delete_table_row(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let Some(pos) = engine.table_pos(caret.cursor()) else {
        return Ok(RichOutcome::Noop);
    };
    if pos.n_rows <= 1 {
        return Ok(RichOutcome::Noop);
    }
    let next_row = pos.row.min(pos.n_rows.saturating_sub(2));
    let col = pos.col;
    let table_start = engine
        .block(pos.table_id)
        .map(|t| t.source_range.start)
        .unwrap_or(caret.cursor());
    rewrite_table(doc, engine, caret, pos, |table, pos| {
        if pos.row < table.children.len() {
            table.children.remove(pos.row);
        }
        normalize_table_headers(table);
    })?;
    place_table_caret(engine, caret, table_start, next_row, col);
    Ok(RichOutcome::Changed)
}

fn delete_table_column(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
) -> Result<RichOutcome, RichError> {
    let Some(pos) = engine.table_pos(caret.cursor()) else {
        return Ok(RichOutcome::Noop);
    };
    if pos.n_cols <= 1 {
        return Ok(RichOutcome::Noop);
    }
    let next_col = pos.col.min(pos.n_cols.saturating_sub(2));
    let row = pos.row;
    let table_start = engine
        .block(pos.table_id)
        .map(|t| t.source_range.start)
        .unwrap_or(caret.cursor());
    rewrite_table(doc, engine, caret, pos, |table, pos| {
        if let BlockKind::Table { alignments } = &mut table.kind {
            if pos.col < alignments.len() {
                alignments.remove(pos.col);
            }
        }
        for row in &mut table.children {
            if pos.col < row.children.len() {
                row.children.remove(pos.col);
            }
        }
        normalize_table_headers(table);
    })?;
    place_table_caret(engine, caret, table_start, row, next_col);
    Ok(RichOutcome::Changed)
}

fn normalize_table_headers(table: &mut Block) {
    for (i, row) in table.children.iter_mut().enumerate() {
        if let BlockKind::TableRow { header } = &mut row.kind {
            *header = i == 0;
        }
    }
}

fn place_table_caret(
    engine: &RichEngine,
    caret: &mut CaretState,
    table_start: usize,
    row: usize,
    col: usize,
) {
    let probe = table_start.min(
        engine
            .tree()
            .blocks
            .last()
            .map_or(0, |b| b.source_range.end),
    );
    let Some(pos) = engine
        .table_pos(probe)
        .or_else(|| engine.table_pos(caret.cursor()))
    else {
        return;
    };
    let row = row.min(pos.n_rows.saturating_sub(1));
    let col = col.min(pos.n_cols.saturating_sub(1));
    if let Some(offset) = engine.cell_caret(pos.table_id, row, col) {
        caret.collapse_to(offset);
    }
}

fn rewrite_table(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    pos: TablePos,
    mutate: impl FnOnce(&mut Block, TablePos),
) -> Result<RichOutcome, RichError> {
    let Some(table) = engine.block(pos.table_id) else {
        return Ok(RichOutcome::Noop);
    };
    let Some(top) = engine.top_level_at(table.source_range.start).cloned() else {
        return Ok(RichOutcome::Noop);
    };
    let mut rewritten = top.clone();
    let Some(target) = find_block_mut(&mut rewritten, pos.table_id) else {
        return Ok(RichOutcome::Noop);
    };
    mutate(target, pos);
    splice_serialized(doc, engine, caret, &top, &rewritten)
}

fn empty_row(header: bool, cols: usize) -> Block {
    Block {
        id: NodeId(0),
        source_range: 0..0,
        content_hash: 0,
        kind: BlockKind::TableRow { header },
        children: (0..cols).map(|_| empty_cell()).collect(),
        inlines: Vec::new(),
    }
}

fn empty_cell() -> Block {
    Block {
        id: NodeId(0),
        source_range: 0..0,
        content_hash: 0,
        kind: BlockKind::TableCell,
        children: Vec::new(),
        inlines: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rich::blank_caret_gap_after_last;
    use crate::rich::blank_caret_gap_before;
    use crate::rich::engine::{html_block_break_range, thematic_break_range, Bias, RichEngine};
    use crate::rich::place_caret_for_click_below;
    use crate::Document;

    fn setup(source: &str) -> (Document, RichEngine, CaretState) {
        let doc = Document::new(source);
        let mut engine = RichEngine::new();
        engine.sync(&doc);
        (doc, engine, CaretState::collapsed(0))
    }

    fn apply(
        doc: &mut Document,
        engine: &mut RichEngine,
        caret: &mut CaretState,
        cmd: RichCommand,
    ) -> String {
        apply_rich_command(doc, engine, caret, cmd).unwrap();
        doc.buffer.content()
    }

    #[test]
    fn insert_text_escapes_emphasis_and_preserves_other_blocks() {
        let source = "hello\n\nworld\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let second = engine.tree().blocks[1].source_range.clone();
        // Mid-word `*` is a literal, not an input-rule opener.
        caret.collapse_to(second.start + 3);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("*".into()),
        );
        assert!(after.starts_with("hello\n\n"), "prefix kept: {after:?}");
        assert!(
            after.contains("wor\\*ld") || after.contains("\\*"),
            "star escaped: {after:?}"
        );
    }

    #[test]
    fn insert_text_newline_in_paragraph_is_soft_wrap_not_ncr() {
        let (mut doc, mut engine, mut caret) = setup("hello");
        caret.collapse_to(5);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\nworld".into()),
        );
        assert!(
            !after.contains("&#10;"),
            "paste must not HTML-encode newlines, got {after:?}"
        );
        assert_eq!(after, "hello\nworld");
    }

    #[test]
    fn insert_text_lone_newline_in_paragraph_still_splits() {
        let (mut doc, mut engine, mut caret) = setup("hello");
        caret.collapse_to(5);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n".into()),
        );
        assert!(
            after.contains("hello\n\n") || after == "hello\n\n",
            "IME Enter (InsertText newline) must still split, got {after:?}"
        );
    }

    #[test]
    fn insert_text_double_newline_in_paragraph_starts_new_paragraph() {
        let (mut doc, mut engine, mut caret) = setup("hello");
        caret.collapse_to(5);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("a\n\nb".into()),
        );
        assert!(!after.contains("&#10;"), "{after:?}");
        assert_eq!(after, "helloa\n\nb");
        engine.sync(&doc);
        assert!(
            engine.tree().blocks.len() >= 2,
            "\\n\\n must start a new paragraph, got {} blocks in {after:?}",
            engine.tree().blocks.len()
        );
    }

    #[test]
    fn insert_text_multiline_in_quote_keeps_quote_prefix() {
        let source = "> hello";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("a\nb".into()),
        );
        assert!(!after.contains("&#10;"), "{after:?}");
        assert!(
            after.contains("> helloa") && after.contains("> b"),
            "each pasted line must keep `>`, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::BlockQuote),
            "quote must not split, got {:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn insert_text_multiline_in_list_keeps_continuation_indent() {
        let source = "- hello";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("a\nb".into()),
        );
        assert!(!after.contains("&#10;"), "{after:?}");
        assert!(
            after.contains("- helloa") && after.contains("\n  b"),
            "pasted wrap must keep list continuation indent, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::BulletList { .. }),
            "list must survive paste, got {:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn insert_text_paste_list_marker_lines_become_sibling_items() {
        for (source, paste, second) in [
            ("- hello", "\n- world", "- world"),
            ("* hello", "\n* world", "* world"),
            ("1. hello", "\n2. world", "2. world"),
            ("- [ ] hello", "\n- [ ] world", "- [ ] world"),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText(paste.into()),
            );
            assert!(
                !after.contains("\\-") && !after.contains("\\*") && !after.contains("\\."),
                "list marker must not be escaped, got {after:?}"
            );
            assert!(
                after.contains(second) && !after.contains(&format!("  {second}")),
                "pasted marker line must be a sibling item, not continuation, got {after:?}"
            );
            engine.sync(&doc);
            assert_eq!(
                count_list_items(&engine.tree().blocks),
                2,
                "expected two list items after pasting {paste:?} into {source:?}, got {after:?} {:?}",
                engine.tree().blocks[0].kind
            );
        }
    }

    #[test]
    fn insert_text_paste_list_marker_in_quoted_list_stays_quoted_sibling() {
        let source = "> - hello";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n- world".into()),
        );
        assert!(
            after.contains("> - hello") && after.contains("> - world"),
            "quoted sibling list paste, got {after:?}"
        );
        assert!(
            !after.contains(">   - world") && !after.contains(">>"),
            "must not continuation-indent or double quote, got {after:?}"
        );
        engine.sync(&doc);
        assert_eq!(count_list_items(&engine.tree().blocks), 2, "{after:?}");
    }

    #[test]
    fn insert_text_paste_quoted_line_does_not_double_gt() {
        let source = "> hello";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n> world".into()),
        );
        assert!(
            after.contains("> hello") && after.contains("> world") && !after.contains("> > world"),
            "pasted `>` must not nest, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::BlockQuote),
            "must remain a quote, got {:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn insert_text_paste_atx_line_in_paragraph_becomes_heading() {
        let (mut doc, mut engine, mut caret) = setup("hello");
        caret.collapse_to(5);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n# Title".into()),
        );
        assert!(
            !after.contains("\\#"),
            "ATX paste must not be escaped, got {after:?}"
        );
        assert_eq!(after, "hello\n# Title");
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Paragraph),
            "first line stays a paragraph, got {:?}",
            engine.tree().blocks[0].kind
        );
        assert!(
            matches!(
                engine.tree().blocks.get(1).map(|b| &b.kind),
                Some(BlockKind::Heading { level: 1, .. })
            ),
            "later `# Title` must be a heading, got {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn insert_text_single_line_atx_on_empty_doc_becomes_heading() {
        let (mut doc, mut engine, mut caret) = setup("");
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("# Title".into()),
        );
        assert!(
            !after.contains("\\#"),
            "single-line ATX paste must not be escaped, got {after:?}"
        );
        assert!(
            after.starts_with("# Title"),
            "empty doc paste must stay markdown, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(
                engine.tree().blocks.first().map(|b| &b.kind),
                Some(BlockKind::Heading { level: 1, .. })
            ),
            "empty doc + `# Title` must be a heading, got {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn insert_text_single_line_atx_at_paragraph_start_becomes_heading() {
        let (mut doc, mut engine, mut caret) = setup("hello");
        caret.collapse_to(0);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("# Title".into()),
        );
        assert!(
            !after.contains("\\#"),
            "paragraph-start ATX paste must not be escaped, got {after:?}"
        );
        assert!(
            after.starts_with("# Title"),
            "must insert as markdown at paragraph start, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(
                engine.tree().blocks.first().map(|b| &b.kind),
                Some(BlockKind::Heading { level: 1, .. })
            ),
            "paragraph start + `# Title` must be a heading, got {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn insert_text_single_line_atx_after_blank_becomes_heading() {
        let source = "hello\n\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let gap = blank_caret_gap_after_last(engine.tree()).expect("trailing blank");
        caret.collapse_to(gap.start);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("# Title".into()),
        );
        assert!(
            !after.contains("\\#"),
            "ATX paste after \\n\\n must not be escaped, got {after:?}"
        );
        assert!(
            after.contains("hello") && after.contains("# Title"),
            "must keep the paragraph and insert a heading, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Paragraph),
            "first block stays a paragraph, got {:?}",
            engine.tree().blocks[0].kind
        );
        assert!(
            engine
                .tree()
                .blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Heading { level: 1, .. })),
            "after \\n\\n, `# Title` must be a heading, got {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn insert_text_single_line_atx_mid_paragraph_stays_text() {
        let (mut doc, mut engine, mut caret) = setup("hello");
        caret.collapse_to(5);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("# Title".into()),
        );
        assert_eq!(after, "hello# Title");
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Paragraph),
            "mid-paragraph `# Title` must not become a heading, got {:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn insert_text_single_line_list_item_inside_item_becomes_sibling() {
        for (source, paste, second) in [
            ("- hello", "- world", "- world"),
            ("* hello", "* world", "* world"),
            ("1. hello", "2. world", "2. world"),
            ("- [ ] hello", "- [ ] world", "- [ ] world"),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText(paste.into()),
            );
            assert!(
                !after.contains("\\-") && !after.contains("\\*") && !after.contains("\\."),
                "list marker must not be escaped, got {after:?}"
            );
            assert!(
                !after.contains(&format!("{source}{paste}"))
                    && !after.contains(&format!("{source}{second}")),
                "must not concatenate onto the item, got {after:?}"
            );
            assert!(
                after.contains(second) && !after.contains(&format!("  {second}")),
                "pasted marker line must be a sibling item, not continuation, got {after:?}"
            );
            engine.sync(&doc);
            assert_eq!(
                count_list_items(&engine.tree().blocks),
                2,
                "expected two list items after pasting {paste:?} into {source:?}, got {after:?} {:?}",
                engine.tree().blocks[0].kind
            );
        }
    }

    #[test]
    fn insert_text_single_line_list_item_in_quoted_list_stays_quoted_sibling() {
        let source = "> - hello";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("- world".into()),
        );
        assert!(
            after.contains("> - hello") && after.contains("> - world"),
            "quoted sibling list paste without leading newline, got {after:?}"
        );
        assert!(
            !after.contains(">   - world") && !after.contains(">>") && !after.contains("- hello-"),
            "must not continuation-indent, double quote, or concatenate, got {after:?}"
        );
        engine.sync(&doc);
        assert_eq!(count_list_items(&engine.tree().blocks), 2, "{after:?}");
    }

    #[test]
    fn insert_text_single_line_hash_in_fence_stays_literal() {
        let source = "```\ncode\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code") + "code".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("# Title".into()),
        );
        assert!(
            still_one_fence(&after) && after.contains("# Title") && !after.contains("\\#"),
            "fence paste must stay literal `#`, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::CodeBlock { .. }),
            "must remain a code block, not a heading, got {:?}",
            engine.tree().blocks[0].kind
        );
        assert!(
            !engine
                .tree()
                .blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Heading { .. })),
            "single-line `#` inside a fence must not become a heading, got {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn insert_text_single_line_hash_in_table_stays_cell_literal() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('a').expect("header a") + 1);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("# Title".into()),
        );
        assert!(
            after.contains("# Title") && !after.contains("\\#"),
            "table paste of `# Title` must stay cell text, got {after:?}"
        );
        assert!(
            !after.contains("\n# Title"),
            "must not split the GFM row, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Table { .. }),
            "table must not become a heading, got {:?}",
            engine.tree().blocks[0].kind
        );
        assert!(
            !engine
                .tree()
                .blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Heading { .. })),
            "cell `#` must not parse as a heading, got {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn insert_text_single_line_hash_in_html_stays_literal() {
        let source = "<div>\nx\n</div>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('x').expect("html body") + 1);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("# Title".into()),
        );
        assert!(
            after.contains("<div>") && after.contains("x# Title") && after.contains("</div>"),
            "HTML paste must stay inside the block, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Opaque { .. }),
            "must remain an HTML block, got {:?}",
            engine.tree().blocks[0].kind
        );
        assert!(
            !engine
                .tree()
                .blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Heading { .. })),
            "HTML-body `#` must not become a heading, got {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn insert_text_paste_list_line_in_paragraph_becomes_list() {
        let (mut doc, mut engine, mut caret) = setup("hello");
        caret.collapse_to(5);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n- world".into()),
        );
        assert_eq!(after, "hello\n- world");
        engine.sync(&doc);
        assert!(
            matches!(
                engine.tree().blocks.get(1).map(|b| &b.kind),
                Some(BlockKind::BulletList { .. })
            ),
            "later `- world` must be a list, got {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn insert_text_multiline_in_heading_keeps_heading_on_first_line() {
        let source = "# Title";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("a\nb".into()),
        );
        assert!(!after.contains("&#10;"), "{after:?}");
        assert!(
            after.starts_with("# Titlea"),
            "first pasted line must stay in the heading, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(
                engine.tree().blocks[0].kind,
                BlockKind::Heading { level: 1, .. }
            ),
            "must not smash the heading into a paragraph, got {:?}",
            engine.tree().blocks[0].kind
        );
        assert!(
            after.contains('\n') && after.contains('b'),
            "further lines become a following block, got {after:?}"
        );
    }

    #[test]
    fn insert_text_multiline_in_fence_stays_in_the_fence() {
        let source = "```\ncode\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code") + "code".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("a\nb".into()),
        );
        assert!(!after.contains("&#10;"), "{after:?}");
        assert!(
            still_one_fence(&after) && after.contains("codea") && after.contains('\n'),
            "paste must stay inside the fence, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::CodeBlock { .. }),
            "must remain a code block, got {:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn insert_text_paste_hash_in_fence_stays_literal() {
        let source = "```\ncode\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code") + "code".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n# Title".into()),
        );
        assert!(
            still_one_fence(&after) && after.contains("# Title") && !after.contains("\\#"),
            "fence paste must stay literal `#`, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::CodeBlock { .. }),
            "must remain a code block, not a heading, got {:?}",
            engine.tree().blocks[0].kind
        );
        assert!(
            !engine
                .tree()
                .blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Heading { .. })),
            "pasted `#` inside a fence must not become a heading, got {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn insert_text_multiline_in_quoted_fence_keeps_quote_prefixes() {
        let source = "> ```\n> code\n> ```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code") + "code".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("a\nb".into()),
        );
        assert!(
            still_one_fence(&after) && every_line_quoted(&after) && !after.contains("&#10;"),
            "quoted fence paste must keep `>` on every line, got {after:?}"
        );
    }

    #[test]
    fn insert_text_multiline_in_html_block_stays_inside() {
        let source = "<div>\nx\n</div>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('x').expect("html body") + 1);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("a\nb".into()),
        );
        assert!(
            after.contains("<div>") && after.contains("</div>") && after.contains("xa"),
            "paste must stay inside the HTML block, got {after:?}"
        );
        assert!(!after.contains("&#10;"), "{after:?}");
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Opaque { .. }),
            "must remain an HTML block, got {:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn insert_text_multiline_in_table_uses_br_not_row_break() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('a').expect("header a") + 1);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x\ny".into()),
        );
        assert!(
            after.contains("<br>") && !after.contains("&#10;"),
            "table paste must use <br>, got {after:?}"
        );
        assert!(
            !after.contains("a\n") && !after.contains("ax\n"),
            "paste must not splice a newline into the GFM row: {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Table { .. }),
            "table must survive multiline paste, got {:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn insert_text_paste_hash_in_table_stays_cell_literal() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('a').expect("header a") + 1);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n# Title".into()),
        );
        assert!(
            after.contains("<br>") && after.contains("# Title") && !after.contains("\n#"),
            "table paste of `#` must stay in the cell via <br>, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Table { .. }),
            "table must not become a heading, got {:?}",
            engine.tree().blocks[0].kind
        );
        assert!(
            !engine
                .tree()
                .blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Heading { .. })),
            "cell `#` must not parse as a heading, got {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );
    }

    fn paste_into(source: &str, at: usize, text: &str) -> (String, RichEngine) {
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(at.min(source.len()));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText(text.into()),
        );
        engine.sync(&doc);
        (after, engine)
    }

    fn tree_has_link(blocks: &[Block], url: &str) -> bool {
        blocks.iter().any(|b| {
            b.inlines.iter().any(|inline| match inline {
                Inline::Run {
                    link: Some(link), ..
                } if link.url.contains(url) => true,
                Inline::Image { url: u, .. } if u.contains(url) => true,
                _ => false,
            }) || tree_has_link(&b.children, url)
        })
    }

    fn tree_has_mark(blocks: &[Block], mark: MarkSet, needle: &str) -> bool {
        blocks.iter().any(|b| {
            b.inlines.iter().any(|inline| {
                matches!(
                    inline,
                    Inline::Run { text, marks, .. }
                        if marks.contains(mark) && text.contains(needle)
                )
            }) || tree_has_mark(&b.children, mark, needle)
        })
    }

    fn tree_has_image(blocks: &[Block], url: &str) -> bool {
        blocks.iter().any(|b| {
            b.inlines
                .iter()
                .any(|inline| matches!(inline, Inline::Image { url: u, .. } if u.contains(url)))
                || tree_has_image(&b.children, url)
        })
    }

    /// Copy writes source markdown; paste must not `escape_text` that clipboard
    /// (`\[hello](url)`, `\*\*bold\*\*`, `\![alt](url)`). Typed `*` still escapes.
    #[test]
    fn insert_text_paste_copied_gfm_inlines_stay_markdown() {
        for (text, check) in [
            ("[hello](https://e.com)", "link"),
            ("**bold**", "bold"),
            ("*italic*", "italic"),
            ("![alt](a.png)", "image"),
            ("<https://e.com>", "autolink"),
            ("~~strike~~", "strike"),
            ("`code`", "code"),
            ("A&amp;B", "entity"),
        ] {
            let (after, engine) = paste_into("", 0, text);
            assert!(
                !after.contains('\\'),
                "clipboard {check} must not be escaped, got {after:?}"
            );
            assert!(
                after.contains(text) || after.contains(text.trim_end()),
                "paste must keep source markdown {text:?}, got {after:?}"
            );
            let blocks = &engine.tree().blocks;
            match check {
                "link" | "autolink" => assert!(
                    tree_has_link(blocks, "e.com"),
                    "{check} paste must parse as a link, {after:?} {:?}",
                    blocks.iter().map(|b| &b.kind).collect::<Vec<_>>()
                ),
                "bold" => assert!(
                    tree_has_mark(blocks, MarkSet::BOLD, "bold"),
                    "bold paste must keep emphasis, {after:?}"
                ),
                "italic" => assert!(
                    tree_has_mark(blocks, MarkSet::ITALIC, "italic"),
                    "italic paste must keep emphasis, {after:?}"
                ),
                "image" => assert!(
                    tree_has_image(blocks, "a.png"),
                    "image paste must stay an image, {after:?}"
                ),
                "strike" => assert!(
                    tree_has_mark(blocks, MarkSet::STRIKE, "strike"),
                    "strike paste must keep GFM strikethrough, {after:?}"
                ),
                "code" => assert!(
                    tree_has_mark(blocks, MarkSet::CODE, "code"),
                    "code-span paste must stay a code span, {after:?}"
                ),
                "entity" => assert!(
                    !after.contains("\\&") && after.contains("&amp;"),
                    "entity paste must keep `&amp;`, got {after:?}"
                ),
                _ => {}
            }
        }

        let (after, engine) = paste_into("see ", 4, "[hello](https://e.com)");
        assert!(
            after.starts_with("see [hello](https://e.com)") && !after.contains('\\'),
            "mid-paragraph link paste must stay a link, got {after:?}"
        );
        assert!(
            tree_has_link(&engine.tree().blocks, "e.com"),
            "mid-paragraph paste must parse as a link, {after:?}"
        );

        for (source, at_needle) in [
            ("# Title", "Title"),
            ("> hello", "hello"),
            ("- hello", "hello"),
        ] {
            let at = source.find(at_needle).expect(at_needle) + at_needle.len();
            let (after, engine) = paste_into(source, at, "[lab](https://e.com)");
            assert!(
                after.contains("[lab](https://e.com)") && !after.contains("\\["),
                "paste into {source:?} must keep the link, got {after:?}"
            );
            assert!(
                tree_has_link(&engine.tree().blocks, "e.com"),
                "paste into {source:?} must parse as a link, {after:?}"
            );
        }
    }

    /// Copy of a visible link/image/bold is source markdown; paste into an
    /// empty doc must round-trip the construct (not escaped delimiters).
    #[test]
    fn insert_text_paste_roundtrips_copied_link_image_bold() {
        for source in ["[hello](https://e.com)\n", "**bold**\n", "![cat](a.png)\n"] {
            let (_doc, engine, _) = setup(source);
            let inner = if source.contains("hello") {
                let s = source.find("hello").expect("hello");
                s..s + 5
            } else if source.contains("bold") {
                let s = source.find("bold").expect("bold");
                s..s + 4
            } else {
                let s = source.find('!').expect("image");
                s..s
            };
            let copied = engine.markdown_for_selection(source, inner);
            assert!(
                copied.contains('[') || copied.contains('*') || copied.contains('!'),
                "copy of {source:?} must be markdown, got {copied:?}"
            );
            let (after, pasted) = paste_into("", 0, &copied);
            assert!(
                !after.contains('\\'),
                "paste of copied {source:?} must not escape, copy={copied:?} got {after:?}"
            );
            if source.contains("hello") {
                assert!(
                    tree_has_link(&pasted.tree().blocks, "e.com"),
                    "copied link paste must stay a link, {copied:?} -> {after:?}"
                );
            } else if source.contains("bold") {
                assert!(
                    tree_has_mark(&pasted.tree().blocks, MarkSet::BOLD, "bold"),
                    "copied bold paste must stay bold, {copied:?} -> {after:?}"
                );
            } else {
                assert!(
                    tree_has_image(&pasted.tree().blocks, "a.png"),
                    "copied image paste must stay an image, {copied:?} -> {after:?}"
                );
            }
        }
    }

    /// Compact tables, setext, thematic breaks, HTML, and `[ref]:` are listed
    /// GFM. Their first bytes (`-`, `=`, `<`, `[`) were escaped at line start.
    #[test]
    fn insert_text_paste_gfm_blocks_that_are_not_atx_list_quote_fence() {
        let (after, engine) = paste_into("", 0, "foo|bar\n---|---\nbaz|bim");
        assert!(
            !after.contains('\\'),
            "compact table alignment must not escape, got {after:?}"
        );
        assert!(
            engine
                .tree()
                .blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Table { .. })),
            "compact table paste must stay a table, got {after:?} {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );

        let (after, engine) = paste_into("", 0, "Title\n===");
        assert!(
            !after.contains("\\="),
            "setext underline must not escape, got {after:?}"
        );
        assert!(
            matches!(
                engine.tree().blocks.first().map(|b| &b.kind),
                Some(BlockKind::Heading { .. })
            ),
            "setext paste must be a heading, got {after:?} {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );

        for rule in ["---", "***", "___"] {
            let (after, engine) = paste_into("", 0, rule);
            assert!(
                !after.contains('\\'),
                "thematic {rule} paste must not escape, got {after:?}"
            );
            assert!(
                engine
                    .tree()
                    .blocks
                    .iter()
                    .any(|b| matches!(b.kind, BlockKind::ThematicBreak)),
                "thematic {rule} paste must be a rule, got {after:?} {:?}",
                engine
                    .tree()
                    .blocks
                    .iter()
                    .map(|b| &b.kind)
                    .collect::<Vec<_>>()
            );
        }

        let (after, engine) = paste_into("", 0, "<div>hello</div>");
        assert!(
            !after.contains("\\<"),
            "HTML paste must not escape tags, got {after:?}"
        );
        assert!(
            engine
                .tree()
                .blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Opaque { .. })),
            "HTML paste must stay an HTML block, got {after:?} {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );

        let (after, engine) = paste_into("", 0, "[ref]: https://e.com");
        assert!(
            !after.contains("\\["),
            "[ref]: paste must not escape, got {after:?}"
        );
        assert!(
            engine
                .tree()
                .blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::LinkReferenceDefinition { .. })),
            "[ref]: paste must stay a definition, got {after:?} {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn insert_in_second_paragraph_leaves_first_bytes_untouched() {
        let source = "alpha\n\nbeta\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let first_end = engine.tree().blocks[0].source_range.end;
        let prefix = source[..first_end].to_string();
        caret.collapse_to(engine.tree().blocks[1].source_range.start);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        let new_src = doc.buffer.content();
        assert_eq!(&new_src[..first_end], prefix);
    }

    #[test]
    fn typing_coalesces_and_undo_restores_string_and_caret() {
        let (mut doc, mut engine, mut caret) = setup("ab");
        caret.collapse_to(2);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("c".into()),
        );
        let after_c = caret.clone();
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("d".into()),
        );
        assert_eq!(doc.buffer.content(), "abcd");
        assert_eq!(doc.undo_stack().undo_depth(), 1);
        let tx = doc.undo_tx().unwrap();
        engine.sync(&doc);
        caret.restore(tx.selection_after);
        assert_eq!(doc.buffer.content(), "ab");
        assert_eq!(caret.cursor(), after_c.cursor() - 1);
    }

    #[test]
    fn replacing_a_backward_grapheme_selection_is_one_undo_step() {
        let source = "Native 👩🏽‍💻\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.range = "Native".len()..source.len() - 1;
        caret.reversed = true;
        let before = caret.snapshot();
        let replaced = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("Café 👩🏽‍💻".into()),
        );
        let after = caret.snapshot();
        assert_eq!(replaced, "NativeCafé 👩🏽‍💻\n");
        assert_eq!(doc.undo_stack().undo_depth(), 1);
        let undo = doc.undo_tx().unwrap();
        assert_eq!(doc.buffer.content(), source);
        assert_eq!(undo.selection_after, before);
        let redo = doc.redo_tx().unwrap();
        assert_eq!(doc.buffer.content(), replaced);
        assert_eq!(redo.selection_after, after);
    }

    #[test]
    fn replacing_selection_with_multiline_table_text_is_one_undo_step() {
        let source = "| Name | Value |\n| --- | --- |\n| key | old |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let start = source.find("old").unwrap();
        caret.range = start..start + "old".len();
        let before = caret.snapshot();
        let replaced = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("first\nsecond".into()),
        );
        let after = caret.snapshot();
        assert!(replaced.contains("first<br>second"), "{replaced:?}");
        assert_eq!(doc.undo_stack().undo_depth(), 1);
        let undo = doc.undo_tx().unwrap();
        assert_eq!(doc.buffer.content(), source);
        assert_eq!(undo.selection_after, before);
        let redo = doc.redo_tx().unwrap();
        assert_eq!(doc.buffer.content(), replaced);
        assert_eq!(redo.selection_after, after);
    }

    #[test]
    fn pasted_input_rule_trigger_does_not_absorb_preceding_typing() {
        let (mut doc, mut engine, mut caret) = setup("");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("#".into()),
        );
        let before_paste = doc.buffer.content();
        let before_caret = caret.snapshot();
        let paste = doc.begin_undo_group(before_caret);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText(" ".into()),
        );
        doc.finish_undo_group(paste, caret.snapshot());
        assert_eq!(doc.undo_stack().undo_depth(), 2);
        let undo = doc.undo_tx().unwrap();
        assert_eq!(doc.buffer.content(), before_paste);
        assert_eq!(undo.selection_after, before_caret);
        assert!(doc.undo());
        assert_eq!(doc.buffer.content(), "");
    }

    #[test]
    fn split_paragraph_inserts_blank_line() {
        let (mut doc, mut engine, mut caret) = setup("hello world\n");
        caret.collapse_to("hello".len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert_eq!(after, "hello\n\n world\n");
        engine.sync(&doc);
        assert_eq!(engine.tree().blocks.len(), 2);
    }

    #[test]
    fn toggle_bold_wraps_selection() {
        let (mut doc, mut engine, mut caret) = setup("hello\n");
        caret.range = 0..5;
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        assert!(
            after.contains("**hello**") || after.contains("__hello__"),
            "{after:?}"
        );
    }

    #[test]
    fn toggle_bold_on_empty_caret_inserts_pair_and_types_inside() {
        // Typora / source wrap: Cmd-B with no selection inserts `****` and
        // leaves the caret between the marks so the next insert is `**x**`,
        // not a wrap of the whole run (`**hello**`).
        let (mut doc, mut engine, mut caret) = setup("hello\n");
        caret.collapse_to("he".len());
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        let wrapped = doc.buffer.content();
        assert_eq!(
            wrapped, "he****llo\n",
            "empty Cmd-B must insert a pair, not wrap the run, got {wrapped:?}"
        );
        assert_eq!(
            caret.cursor(),
            "he**".len(),
            "caret must sit inside the empty pair, got {}",
            caret.cursor()
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        assert_eq!(
            typed, "he**x**llo\n",
            "typing after empty Cmd-B must go inside the marks, got {typed:?}"
        );
    }

    #[test]
    fn toggle_mark_empty_caret_second_press_unwraps() {
        let (mut doc, mut engine, mut caret) = setup("ab");
        caret.collapse_to(1);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        assert_eq!(doc.buffer.content(), "a****b");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        assert_eq!(
            doc.buffer.content(),
            "ab",
            "second empty Cmd-B must unwrap the pair"
        );
        assert_eq!(caret.cursor(), 1);
    }

    #[test]
    fn toggle_italic_and_code_on_empty_caret_insert_pairs() {
        let (mut doc, mut engine, mut caret) = setup("ab");
        caret.collapse_to(1);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::ITALIC),
        );
        assert_eq!(doc.buffer.content(), "a**b");
        assert_eq!(caret.cursor(), 2);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert_eq!(doc.buffer.content(), "a*x*b");

        let (mut doc, mut engine, mut caret) = setup("ab");
        caret.collapse_to(1);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::CODE),
        );
        assert_eq!(doc.buffer.content(), "a``b");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert_eq!(doc.buffer.content(), "a`x`b");
    }

    #[test]
    fn toggle_link_on_empty_caret_inserts_brackets() {
        // Mid-word Cmd-K wraps the word; a caret on whitespace is a true empty.
        let (mut doc, mut engine, mut caret) = setup("hello ");
        caret.collapse_to("hello ".len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        assert_eq!(after, "hello []()");
        assert_eq!(
            caret.cursor(),
            "hello [".len(),
            "empty Cmd-K must put the caret in the label, got {}",
            caret.cursor()
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert_eq!(doc.buffer.content(), "hello [x]()");
    }

    #[test]
    fn toggle_link_on_word_wraps_the_word() {
        let (mut doc, mut engine, mut caret) = setup("hello");
        caret.collapse_to(2);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        assert_eq!(
            after, "[hello]()",
            "Cmd-K on a word must use empty (), got {after:?}"
        );
        assert!(
            !after.contains("<>"),
            "empty dest must not serialize as <>, got {after:?}"
        );
        let url_at = after
            .find("[hello](")
            .map(|i| i + "[hello](".len())
            .expect("url slot");
        assert_eq!(
            caret.cursor(),
            url_at,
            "Cmd-K on a word must leave the caret in the URL, got {} in {after:?}",
            caret.cursor()
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("https://e.com".into()),
        );
        assert_eq!(
            doc.buffer.content(),
            "[hello](https://e.com)",
            "typing a URL must fill (), not leave <>"
        );
    }

    #[test]
    fn toggle_bold_in_fenced_code_is_noop() {
        let source = "```\ncode\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("code").expect("code body"));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        assert_eq!(
            doc.buffer.content(),
            source,
            "Cmd-B inside a fence must not insert wrap marks"
        );
    }

    /// Cmd-B/I/E/K inside `$math$`, inline code, `[[wiki]]`, or `:emoji:`
    /// must not splice markdown wrappers into the span (`$****x^2$`).
    #[test]
    fn toggle_wrap_inside_math_code_wiki_emoji_is_noop() {
        let cases = [
            ("see $x^2$ here\n", "x", "math"),
            ("see `code` here\n", "c", "inline code"),
            ("see [[page]] here\n", "p", "wikilink"),
            ("see :smile: here\n", "smile", "emoji"),
        ];
        let cmds = [
            RichCommand::ToggleMark(MarkSet::BOLD),
            RichCommand::ToggleMark(MarkSet::ITALIC),
            RichCommand::ToggleMark(MarkSet::CODE),
            RichCommand::ToggleLink,
        ];
        for (source, needle, label) in cases {
            let at = source.find(needle).expect(label);
            for cmd in &cmds {
                let (mut doc, mut engine, mut caret) = setup(source);
                caret.collapse_to(at);
                apply(&mut doc, &mut engine, &mut caret, cmd.clone());
                assert_eq!(
                    doc.buffer.content(),
                    source,
                    "wrap {cmd:?} inside {label} must no-op, got {:?}",
                    doc.buffer.content()
                );
            }
        }
    }

    #[test]
    fn toggle_bold_on_text_next_to_math_still_wraps() {
        let source = "see $x^2$ here\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("here").expect("here"));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("$x^2$") && after.contains("**"),
            "Cmd-B on `here` must still wrap, got {after:?}"
        );
        assert!(
            !after.contains("$**") && !after.contains("**$") && !after.contains("$****"),
            "math span must stay unmarked, got {after:?}"
        );
    }

    #[test]
    fn backspace_deletes_visible_grapheme_not_delimiters() {
        let source = "**ab**\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        // Caret at end of "ab" (source byte 4).
        caret.collapse_to(4);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert_eq!(after, "**a**\n", "{after:?}");
    }

    #[test]
    fn backspace_deletes_an_entire_extended_grapheme_cluster() {
        for (cluster, label) in [
            ("e\u{301}", "combining accent"),
            ("👩\u{200d}💻", "ZWJ emoji"),
        ] {
            let source = format!("{cluster}x\n");
            let (mut doc, mut engine, mut caret) = setup(&source);
            caret.collapse_to(cluster.len());

            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);

            assert_eq!(
                doc.buffer.content(),
                "x\n",
                "Backspace must remove one visible {label}, not leave a partial cluster"
            );
            assert_eq!(caret.cursor(), 0, "caret follows the removed {label}");
        }
    }

    #[test]
    fn delete_removes_an_entire_extended_grapheme_cluster() {
        for (cluster, label) in [
            ("e\u{301}", "combining accent"),
            ("👩\u{200d}💻", "ZWJ emoji"),
        ] {
            let source = format!("{cluster}x\n");
            let (mut doc, mut engine, mut caret) = setup(&source);

            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);

            assert_eq!(
                doc.buffer.content(),
                "x\n",
                "Delete must remove one visible {label}, not leave a partial cluster"
            );
            assert_eq!(
                caret.cursor(),
                0,
                "caret remains before the removed {label}"
            );
        }
    }

    #[test]
    fn backspace_after_image_deletes_the_whole_image() {
        let source = "hello ![cat](a.png) world\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        engine.sync(&doc);
        let img = engine
            .tree()
            .blocks
            .iter()
            .find_map(|b| {
                b.inlines.iter().find_map(|inline| match inline {
                    Inline::Image { source_range, .. } => Some(source_range.clone()),
                    _ => None,
                })
            })
            .expect("image");
        caret.collapse_to(img.end);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            !after.contains("![cat]"),
            "Backspace after an image must delete `![…](url)`, got {after:?}"
        );
        assert!(after.contains("hello"), "{after:?}");
        assert!(after.contains("world"), "{after:?}");
    }

    #[test]
    fn delete_before_image_deletes_the_whole_image() {
        let source = "hello ![cat](a.png) world\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        engine.sync(&doc);
        let img = engine
            .tree()
            .blocks
            .iter()
            .find_map(|b| {
                b.inlines.iter().find_map(|inline| match inline {
                    Inline::Image { source_range, .. } => Some(source_range.clone()),
                    _ => None,
                })
            })
            .expect("image");
        caret.collapse_to(img.start);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            !after.contains("![cat]"),
            "Delete before an image must delete `![…](url)`, got {after:?}"
        );
        assert!(after.contains("hello"), "{after:?}");
        assert!(after.contains("world"), "{after:?}");
    }

    #[test]
    fn delete_word_left_removes_previous_word_and_skips_bold_marks() {
        let (mut doc, mut engine, mut caret) = setup("hello world");
        caret.collapse_to("hello world".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert_eq!(
            after, "hello ",
            "hello world| Option-Backspace must leave `hello |`, got {after:?}"
        );
        assert_eq!(caret.cursor(), "hello ".len());

        let source = "**hello** world";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert_eq!(
            after, "**hello** ",
            "word-delete must skip bold delimiters like word move, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup("hello world");
        caret.range = 0.."hello".len();
        caret.reversed = false;
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert_eq!(
            after, " world",
            "non-empty selection Option-Backspace deletes the selection, got {after:?}"
        );
    }

    #[test]
    fn delete_word_right_and_line_bounds() {
        let (mut doc, mut engine, mut caret) = setup("hello world");
        caret.collapse_to(0);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordRight,
        );
        assert_eq!(
            after, " world",
            "Option-Delete from the start must remove `hello`, got {after:?}"
        );

        let source = "hello\nworld extra";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteToLineStart,
        );
        assert_eq!(
            after, "hello\n",
            "Cmd-Backspace deletes to the current line start, not the document, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to("hello\n".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteToLineEnd,
        );
        assert_eq!(
            after, "hello\n",
            "Cmd-Delete deletes to the current line end, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup("# Title extra");
        caret.collapse_to("# Title extra".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteToLineStart,
        );
        assert!(
            after.starts_with("# "),
            "Cmd-Backspace on a heading must keep `# `, got {after:?}"
        );
        assert!(
            !after.contains("Title") && !after.contains("extra"),
            "Cmd-Backspace must delete the heading body, got {after:?}"
        );
    }

    #[test]
    fn delete_word_clamps_to_table_cell_and_fence_body() {
        let source = "| hello world | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let world = source.find("world").expect("world");
        caret.collapse_to(world + "world".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Table { .. }),
            "word-delete in a cell must keep a table, got {after:?}"
        );
        assert!(
            after.contains('|') && after.contains('b'),
            "word-delete must not eat `|`, got {after:?}"
        );
        assert!(
            after.contains("hello") && !after.contains("world"),
            "Option-Backspace in the cell must delete `world` only, got {after:?}"
        );

        let source = "```\nhello world\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let world = source.find("world").expect("world");
        caret.collapse_to(world + "world".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert!(
            after.starts_with("```") && after.contains("```\n"),
            "word-delete must stay in the fence body, got {after:?}"
        );
        assert!(
            after.contains("hello ") && !after.contains("world"),
            "Option-Backspace in a fence must delete `world`, got {after:?}"
        );

        let source = "```\nhello world\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("hello").expect("hello"));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert_eq!(
            after, source,
            "Option-Backspace at fence body start must not nibble ticks, got {after:?}"
        );

        let source = "---\ntitle: Hello\n---\n\nhello world";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("hello").expect("hello"));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert_eq!(
            after, source,
            "Option-Backspace at body start after YAML must not nibble the fence, got {after:?}"
        );
    }

    /// Keyboard End / Cmd-Delete-to-line-end in a GFM table stay in the
    /// current cell. Source-line End from `hello` used to jump to `world`.
    #[test]
    fn line_end_in_table_cell_does_not_cross_pipes() {
        for source in [
            "| hello | world |\n|---|---|\n| 1 | 2 |\n",
            "> | hello | world |\n> |---|---|\n> | 1 | 2 |\n",
            "hello|world\n---|---\n1|2\n",
            "| [label](https://e.com) | world |\n|---|---|\n| 1 | 2 |\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let from = source
                .find("hello")
                .or_else(|| source.find("label"))
                .expect("first cell");
            let end = engine.line_end_caret(source, from);
            caret.collapse_to(end);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            assert_eq!(
                typed.matches('|').count(),
                source.matches('|').count(),
                "typing at cell End must not split the row, {source:?} got {typed:?}"
            );
            assert!(
                typed.contains("world") && !typed.contains("worldx"),
                "typing at first-cell End must not extend the next cell, {source:?} got {typed:?}"
            );
            if source.contains("[label]") {
                assert!(
                    typed.contains("[labelx]") && typed.contains("https://e.com"),
                    "typing at End must extend the cell label, {source:?} got {typed:?}"
                );
            } else {
                assert!(
                    typed.contains("hellox"),
                    "typing at End must extend the first cell, {source:?} got {typed:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(from);
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::DeleteToLineEnd,
            );
            assert_eq!(
                after.matches('|').count(),
                source.matches('|').count(),
                "DeleteToLineEnd must not eat `|`, {source:?} got {after:?}"
            );
            assert!(
                after.contains("world"),
                "DeleteToLineEnd must not delete the next cell, {source:?} got {after:?}"
            );
            if source.contains("[label]") {
                assert!(
                    after.contains("](https://e.com)") && !after.contains("[label]"),
                    "DeleteToLineEnd must delete the label, keep dest, {source:?} got {after:?}"
                );
            } else {
                assert!(
                    !after.contains("hello"),
                    "DeleteToLineEnd must delete the first cell body, {source:?} got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            let world = source.find("world").expect("world");
            caret.collapse_to(world + "world".len());
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::DeleteToLineStart,
            );
            assert_eq!(
                after.matches('|').count(),
                source.matches('|').count(),
                "DeleteToLineStart must not eat `|`, {source:?} got {after:?}"
            );
            assert!(
                after.contains("hello") || after.contains("[label]"),
                "DeleteToLineStart must not delete the previous cell, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("world"),
                "DeleteToLineStart must delete the second cell body, {source:?} got {after:?}"
            );
        }
    }

    #[test]
    fn delete_word_left_at_heading_start_converts_to_paragraph() {
        let source = "# Title";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(atx_body_start(source, 0));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert_eq!(
            first_line(&after).trim_end(),
            "Title",
            "Option-Backspace at heading start must strip `#`, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            !caret_in_heading(&engine, caret.cursor()),
            "heading must become a paragraph, got {:?}",
            engine.tree().blocks[0].kind
        );

        let source = "hello\n\n# Title";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(atx_body_start(
            source,
            source.find("# Title").expect("heading"),
        ));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert!(
            after.contains("hello") && after.contains("Title") && !after.contains("# Title"),
            "Option-Backspace at `# Title` must convert, not splice `hello# Title`, got {after:?}"
        );
        assert!(
            !after.contains("helloTitle") && !after.contains("hello#"),
            "must not join the previous paragraph into heading chrome, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            !caret_in_heading(&engine, caret.cursor()),
            "converted heading must be a paragraph, got {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );

        let source = "# Title extra";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let mid = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert!(
            mid.contains("# Title") && !mid.contains("extra"),
            "mid-heading Option-Backspace still deletes a word, got {mid:?}"
        );
    }

    #[test]
    fn delete_word_left_at_list_start_strips_the_marker() {
        let (mut doc, mut engine, mut caret) = setup("- hello");
        caret.collapse_to(list_body_start("- hello", 0));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert_eq!(
            first_line(&after).trim_end(),
            "hello",
            "Option-Backspace at list start must strip the marker, got {after:?}"
        );
        assert!(
            list_marker_prefix(first_line(&after)).is_none(),
            "list marker must be gone, got {after:?}"
        );

        let source = "hello\n\n- world";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(list_body_start(
            source,
            source.find("- world").expect("item"),
        ));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert!(
            after.contains("hello") && after.contains("world") && !after.contains("- world"),
            "Option-Backspace at list start must not join the previous paragraph, got {after:?}"
        );
        assert!(
            list_marker_prefix(
                after
                    .lines()
                    .find(|line| line.contains("world"))
                    .expect("world line")
            )
            .is_none(),
            "list marker must be gone, got {after:?}"
        );
    }

    #[test]
    fn delete_word_left_at_quote_start_outdents() {
        let source = "> hello";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(quote_body_start(source, 0));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert_eq!(
            first_line(&after).trim_end(),
            "hello",
            "Option-Backspace at quote start must strip `>`, got {after:?}"
        );

        let source = "> > hello";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(quote_body_start(source, 0));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        let line = first_line(&after);
        assert!(
            line.starts_with('>') && !line.starts_with("> >") && line.contains("hello"),
            "nested quote Option-Backspace outdents one `>`, got {after:?}"
        );

        let source = "hello\n\n> world";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(quote_body_start(
            source,
            source.find("> world").expect("quote"),
        ));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert!(
            after.contains("hello") && after.contains("world") && !after.contains("> world"),
            "Option-Backspace at quote start must not eat the previous block, got {after:?}"
        );
    }

    #[test]
    fn delete_word_left_at_paragraph_start_does_not_eat_heading_chrome() {
        let source = "# Hello\n\nnext";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("next").expect("next"));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert!(
            after.contains("# Hello") && after.contains("next") && !after.contains("Hellonext"),
            "Option-Backspace after a heading must not eat `# `, got {after:?}"
        );

        let source = "- Hello\n\nnext";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("next").expect("next"));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert!(
            after.contains("- Hello") && after.contains("next") && !after.contains("Hellonext"),
            "Option-Backspace after a list must not eat `- `, got {after:?}"
        );

        let source = "hello\n\nworld";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("world").expect("world"));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert!(
            after.contains("world") && !after.contains("hello\n\nworld"),
            "paragraph-to-paragraph Option-Backspace may join, got {after:?}"
        );
    }

    #[test]
    fn delete_word_right_at_paragraph_end_does_not_eat_heading_or_list_chrome() {
        let source = "hello\n\n# Title";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to("hello".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordRight,
        );
        assert!(
            after.contains("# Title") && after.contains("hello"),
            "Option-Delete at `hello|` must not eat `# Title`, got {after:?}"
        );
        assert!(
            !after.contains("helloTitle") && !after.contains("hello#"),
            "must not join the paragraph onto heading chrome, got {after:?}"
        );

        let source = "hello\n\n- world";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to("hello".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordRight,
        );
        assert!(
            after.contains("- world") && after.contains("hello"),
            "Option-Delete at `hello|` must not eat `- world`, got {after:?}"
        );
        assert!(
            !after.contains("helloworld") && !after.contains("hello-"),
            "must not join the paragraph onto list chrome, got {after:?}"
        );

        let source = "hello\n\n> world";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to("hello".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordRight,
        );
        assert!(
            after.contains("> world") && after.contains("hello"),
            "Option-Delete at `hello|` must not eat `> world`, got {after:?}"
        );

        let source = "hello\n\nworld";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to("hello".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordRight,
        );
        assert!(
            after.contains("hello") && !after.contains("hello\n\nworld"),
            "paragraph-to-paragraph Option-Delete may join, got {after:?}"
        );

        let source = "hello world extra";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to("hello ".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordRight,
        );
        assert_eq!(
            after, "hello  extra",
            "mid-paragraph Option-Delete still deletes the next word, got {after:?}"
        );
    }

    #[test]
    fn delete_word_left_at_alert_start_does_not_splice_previous_paragraph() {
        let source = "hello\n\n> [!NOTE]\n> body\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("[!NOTE]").expect("alert tag"));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert!(
            after.contains("hello") && after.contains("[!NOTE]"),
            "Option-Backspace at `> [!NOTE]` must not splice `hello` into the alert, got {after:?}"
        );
        assert!(
            !after.contains("hello[!NOTE]")
                && !after.contains("hello [!NOTE]")
                && !after.contains("hello> [!NOTE]"),
            "previous paragraph must stay a separate block, got {after:?}"
        );

        let source = "hello\n\n> [!NOTE]\n> body\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to("hello".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordRight,
        );
        assert!(
            after.contains("hello") && after.contains("[!NOTE]") && after.contains("> "),
            "Option-Delete at `hello|` must not eat alert chrome, got {after:?}"
        );
    }

    #[test]
    fn cut_of_visible_bold_removes_markdown_marks() {
        let source = "**hello** world\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let inner = source.find("hello").expect("hello");
        caret.range = inner..inner + "hello".len();
        caret.reversed = false;
        let expanded = engine.expand_markdown_selection(&doc.buffer.content(), caret.range.clone());
        assert_eq!(&source[expanded.clone()], "**hello**");
        caret.range = expanded;
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            !after.contains("**") && after.contains("world"),
            "cut of a fully selected bold word must remove the marks, got {after:?}"
        );
    }

    #[test]
    fn empty_caret_cut_removes_the_current_block() {
        let source = "# Title\n\npara\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let t = source.find('T').expect("T");
        caret.collapse_to(t);
        let expanded =
            engine.expand_markdown_cut_selection(&doc.buffer.content(), caret.range.clone());
        assert!(
            source[expanded.clone()].contains("# Title"),
            "cut range must be the heading, got {:?}",
            &source[expanded.clone()]
        );
        caret.range = expanded;
        caret.reversed = false;
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            !after.contains("# Title") && after.contains("para"),
            "empty-caret heading cut must remove the heading, got {after:?}"
        );

        let source = "- hello\n- world\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let h = source.find('h').expect("h");
        caret.collapse_to(h);
        let expanded =
            engine.expand_markdown_cut_selection(&doc.buffer.content(), caret.range.clone());
        caret.range = expanded;
        caret.reversed = false;
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            !after.contains("hello") && after.contains("- world"),
            "empty-caret list cut must remove that item, got {after:?}"
        );

        let source = "```\ncode\n```\n\npara\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let c = source.find("code").expect("code");
        caret.collapse_to(c);
        let expanded =
            engine.expand_markdown_cut_selection(&doc.buffer.content(), caret.range.clone());
        caret.range = expanded;
        caret.reversed = false;
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            !after.contains("```") && after.contains("para"),
            "empty-caret fence cut must remove the fence, got {after:?}"
        );

        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (_doc, engine, mut caret) = setup(source);
        let a = source.find('a').expect("a");
        caret.collapse_to(a);
        let expanded = engine.expand_markdown_cut_selection(source, caret.range.clone());
        assert_eq!(
            expanded.start, expanded.end,
            "empty-caret table Cut must stay a no-op"
        );
    }

    #[test]
    fn set_heading_changes_block_type() {
        let (mut doc, mut engine, mut caret) = setup("hello\n");
        caret.collapse_to(0);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetBlockType(BlockType::Heading(1)),
        );
        assert!(after.starts_with("# hello"), "{after:?}");
    }

    #[test]
    fn split_list_item_continues_the_list() {
        let (mut doc, mut engine, mut caret) = setup("- hello\n");
        caret.collapse_to("- hello".len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.starts_with("- hello\n- "),
            "expected continued list, got {after:?}"
        );
    }

    #[test]
    fn split_task_item_continues_as_a_task() {
        for source in [
            "- [ ] hello",
            "* [ ] hello",
            "1. [ ] hello",
            "> - [ ] hello",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert!(
                after.contains("[ ] hello"),
                "original task must stay, {source:?} -> {after:?}"
            );
            let new_item = after.lines().nth(1).unwrap_or("");
            assert!(
                new_item.contains("[ ]"),
                "Enter on a GFM task must continue as a task, {source:?} -> {after:?}"
            );
            assert!(
                !new_item.contains("[x]") && !new_item.contains("[X]"),
                "new task must be unchecked, {source:?} -> {after:?}"
            );
        }
    }

    #[test]
    fn split_checked_task_continues_unchecked() {
        for source in [
            "- [x] done",
            "* [X] done",
            "+ [x] done",
            "1. [x] done",
            "> - [x] done",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert!(
                after.contains("[x] done") || after.contains("[X] done"),
                "checked item must stay, {source:?} -> {after:?}"
            );
            let new_item = after.lines().nth(1).unwrap_or("");
            assert!(
                new_item.contains("[ ]"),
                "Typora: Enter on a checked task opens an unchecked item, {source:?} -> {after:?}"
            );
            assert!(
                !new_item.contains("[x]") && !new_item.contains("[X]"),
                "must not copy the checked mark onto the new item, {source:?} -> {after:?}"
            );
        }

        let source = "- [x] hello world";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("world").expect("world"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("- [x] hello") && after.contains("- [ ] world"),
            "Enter mid checked task keeps the check on the first item, {after:?}"
        );

        let source = "- [x] outer\n  - [x] inner";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("- [x] outer") && after.contains("  - [x] inner"),
            "nested checked item must stay, {after:?}"
        );
        assert!(
            after.contains("  - [ ] "),
            "Enter on a nested checked task must continue nested unchecked, {after:?}"
        );
    }

    #[test]
    fn empty_checked_task_enter_exits_the_list() {
        let (mut doc, mut engine, mut caret) = setup("- [x] done\n- [x] ");
        caret.collapse_to(doc.buffer.len_bytes());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("- [x] done"),
            "checked item must stay, got {after:?}"
        );
        assert!(
            !after.contains("- [x] \n") && !after.trim_end().ends_with("[x]"),
            "empty checked task Enter must exit, not copy `[x]`, got {after:?}"
        );
    }

    #[test]
    fn empty_list_item_enter_exits_the_list() {
        let (mut doc, mut engine, mut caret) = setup("- hello\n- ");
        caret.collapse_to(doc.buffer.len_bytes());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.starts_with("- hello\n"),
            "expected list exit, got {after:?}"
        );
        assert!(
            !after.trim_end().ends_with('-'),
            "empty marker should be gone: {after:?}"
        );
    }

    #[test]
    fn nested_empty_item_enter_outdents() {
        let (mut doc, mut engine, mut caret) = setup("- hello\n  - ");
        caret.collapse_to(doc.buffer.len_bytes());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("\n- ") && !after.contains("  - "),
            "expected outdent to top-level item, got {after:?}"
        );
    }

    #[test]
    fn blockquote_enter_continues_the_quote() {
        let (mut doc, mut engine, mut caret) = setup("> hello");
        caret.collapse_to(doc.buffer.len_bytes());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.starts_with("> hello\n>"),
            "expected a new quoted line, got {after:?}"
        );
        engine.sync(&doc);
        let leaf = engine.block_at(caret.cursor()).expect("caret in a block");
        assert!(
            ancestor_is_quote(&engine, leaf),
            "caret must stay in the quote after Enter on a non-empty line"
        );
    }

    #[test]
    fn empty_blockquote_enter_exits_the_quote() {
        let (mut doc, mut engine, mut caret) = setup("> hello\n> ");
        caret.collapse_to(doc.buffer.len_bytes());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.starts_with("> hello"),
            "expected quote content kept, got {after:?}"
        );
        assert!(
            !after.trim_end().ends_with('>'),
            "empty quote marker should be gone: {after:?}"
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        let x_line = typed
            .lines()
            .find(|line| line.contains('x'))
            .expect("typed x");
        assert!(
            !x_line.trim_start().starts_with('>'),
            "text after exiting the quote must not be quoted, got {typed:?}"
        );
    }

    #[test]
    fn empty_blockquote_enter_after_continue_exits() {
        let (mut doc, mut engine, mut caret) = setup("> hello");
        caret.collapse_to(doc.buffer.len_bytes());
        apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.starts_with("> hello"),
            "quote body must remain, got {after:?}"
        );
        assert!(
            !after.trim_end().ends_with('>'),
            "second Enter on the empty quoted line must leave the quote: {after:?}"
        );
    }

    #[test]
    fn nested_empty_quote_enter_outdents() {
        let (mut doc, mut engine, mut caret) = setup("> > nested\n> > ");
        caret.collapse_to(doc.buffer.len_bytes());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("> > nested") && after.contains("\n> ") && !after.contains("\n> > "),
            "expected one quote level outdented, got {after:?}"
        );
        engine.sync(&doc);
        let leaf = engine.block_at(caret.cursor()).expect("caret in a block");
        assert!(
            ancestor_is_quote(&engine, leaf),
            "nested empty quote Enter outdents, it does not jump out of the outer quote"
        );
    }

    fn atx_body_start(source: &str, at: usize) -> usize {
        let start = line_start(source, at);
        let line = current_line(source, start);
        let after = after_quote(line);
        let marker_len = list_marker_prefix(after).map(|p| p.len()).unwrap_or(0);
        let rest = after.get(marker_len..).unwrap_or("");
        start
            + quote_prefix(line).len()
            + marker_len
            + atx_marker_prefix(rest).expect("atx marker").len()
    }

    fn line_is_empty_atx_heading(line: &str) -> bool {
        let after = after_quote(line);
        let Some(prefix) = atx_marker_prefix(after) else {
            return false;
        };
        strip_closing_atx(after.get(prefix.len()..).unwrap_or(""))
            .trim()
            .is_empty()
    }

    fn caret_in_heading(engine: &RichEngine, offset: usize) -> bool {
        heading_at(engine, offset).is_some()
    }

    #[test]
    fn empty_atx_heading_enter_converts_to_paragraph_levels_1_to_6() {
        for level in 1u8..=6 {
            let hashes = "#".repeat(level as usize);
            let source = format!("{hashes} ");
            let (mut doc, mut engine, mut caret) = setup(&source);
            caret.collapse_to(doc.buffer.len_bytes());
            assert!(
                empty_heading_at(&engine, caret.cursor()),
                "level {level} `{source:?}` should be an empty heading"
            );
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert!(
                !after.lines().any(line_is_empty_atx_heading),
                "empty h{level} Enter must drop heading chrome, got {after:?}"
            );
            engine.sync(&doc);
            assert!(
                !caret_in_heading(&engine, caret.cursor()),
                "caret must not stay on an empty heading after Enter, got {:?}",
                doc.buffer.content()
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            let x_line = typed
                .lines()
                .find(|line| line.contains('x'))
                .expect("typed x");
            assert!(
                !x_line.trim_start().starts_with('#'),
                "typing after empty h{level} Enter must be a paragraph, got {typed:?}"
            );
        }
    }

    #[test]
    fn empty_atx_heading_enter_between_blocks_does_not_leave_hashes() {
        let source = "hello\n\n# \n\nworld";
        let (mut doc, mut engine, mut caret) = setup(source);
        let empty_at = source.find("# ").expect("empty heading");
        caret.collapse_to(empty_at + 2);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("hello") && after.contains("world"),
            "surrounding paragraphs must remain, got {after:?}"
        );
        assert!(
            !after.lines().any(line_is_empty_atx_heading),
            "empty heading must not remain as `# `, got {after:?}"
        );
    }

    #[test]
    fn empty_quoted_atx_heading_enter_becomes_quoted_paragraph() {
        let source = "> # ";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(doc.buffer.len_bytes());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            line_is_empty_quoted_paragraph(last_line(&after)),
            "empty quoted heading Enter must become `> `, got {after:?}"
        );
        assert!(
            !last_line(&after).contains('#'),
            "heading hashes must be gone: {after:?}"
        );
        engine.sync(&doc);
        let leaf = engine.block_at(caret.cursor()).expect("caret in a block");
        assert!(
            ancestor_is_quote(&engine, leaf),
            "caret must stay in the quote after dropping heading chrome"
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        let x_line = typed
            .lines()
            .find(|line| line.contains('x'))
            .expect("typed x");
        assert!(
            x_line.trim_start().starts_with('>') && !x_line.contains('#'),
            "text after empty quoted heading Enter must stay quoted, got {typed:?}"
        );
    }

    #[test]
    fn nonempty_heading_enter_splits_like_a_paragraph() {
        let (mut doc, mut engine, mut caret) = setup("# Title");
        caret.collapse_to("# Title".len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.starts_with("# Title"),
            "original heading must be kept, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(
                engine.tree().blocks[0].kind,
                BlockKind::Heading { level: 1, .. }
            ),
            "first block must stay a heading, got {:?}",
            engine.tree().blocks[0].kind
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        let x_line = typed
            .lines()
            .find(|line| line.contains('x'))
            .expect("typed x");
        assert!(
            !x_line.trim_start().starts_with('#'),
            "text after splitting a heading must be a new paragraph, got {typed:?}"
        );

        let (mut doc, mut engine, mut caret) = setup("# Hello");
        caret.collapse_to(atx_body_start("# Hello", 0) + "Hel".len());
        let mid = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            mid.contains("# Hel") && mid.contains("lo"),
            "mid-heading Enter splits like a paragraph, got {mid:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(
                engine.tree().blocks[0].kind,
                BlockKind::Heading { level: 1, .. }
            ),
            "original heading kept after mid split, got {:?}",
            engine.tree().blocks[0].kind
        );
        assert!(
            engine.tree().blocks.len() >= 2,
            "new paragraph after the heading, got {} blocks",
            engine.tree().blocks.len()
        );
    }

    fn line_is_atx_heading_title(line: &str, level: u8, title: &str) -> bool {
        let hashes = "#".repeat(level as usize);
        after_quote(line).trim_end() == format!("{hashes} {title}")
    }

    fn typed_line_is_paragraph(line: &str) -> bool {
        let after = after_quote(line);
        atx_marker_prefix(after).is_none() && !is_setext_underline(after) && after.contains('x')
    }

    #[test]
    fn nonempty_heading_enter_at_start_inserts_paragraph_above_levels_1_to_6() {
        for level in 1u8..=6 {
            let hashes = "#".repeat(level as usize);
            let source = format!("{hashes} Title");
            let (mut doc, mut engine, mut caret) = setup(&source);
            caret.collapse_to(atx_body_start(&source, 0));
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert!(
                !after.lines().any(line_is_empty_atx_heading),
                "h{level} Enter at start must not leave an empty heading, got {after:?}"
            );
            assert!(
                after
                    .lines()
                    .any(|line| line_is_atx_heading_title(line, level, "Title")),
                "h{level} original heading must stay intact, got {after:?}"
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            let x_line = typed
                .lines()
                .find(|line| line.contains('x'))
                .expect("typed x");
            assert!(
                typed_line_is_paragraph(x_line),
                "h{level} typing after Enter at start must be a paragraph, got {typed:?}"
            );
            assert!(
                typed
                    .lines()
                    .any(|line| line_is_atx_heading_title(line, level, "Title")),
                "h{level} heading must survive typing into the new paragraph, got {typed:?}"
            );
            engine.sync(&doc);
            assert!(
                engine
                    .tree()
                    .blocks
                    .iter()
                    .any(|b| matches!(b.kind, BlockKind::Heading { level: l, .. } if l == level)),
                "h{level} must remain a heading after Enter at start, got {:?}",
                engine
                    .tree()
                    .blocks
                    .iter()
                    .map(|b| &b.kind)
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn nonempty_heading_enter_at_start_snap_stays_on_blank() {
        let source = "# Title";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(atx_body_start(source, 0));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        let after = doc.buffer.content();
        let at = caret.cursor();
        let snapped_left = engine.snap_caret(at, Bias::Left);
        let snapped_right = engine.snap_caret(at, Bias::Right);
        assert_eq!(
            snapped_left, at,
            "snap must not move the caret off the blank after Enter-at-start, caret={at} snapped={snapped_left} in {after:?}"
        );
        assert_eq!(snapped_right, at);
        let title = after.find("Title").expect("Title");
        assert!(
            snapped_left < title,
            "snap must not land on `# Title`, caret={at} title={title} in {after:?}"
        );
        assert!(
            !after[snapped_left..].starts_with("# Title")
                && !after[snapped_left..].starts_with("Title"),
            "caret after Enter-at-start must remain on the inserted blank, offset {snapped_left} in {after:?}"
        );
        let heading = engine
            .tree()
            .blocks
            .iter()
            .find(|b| matches!(b.kind, BlockKind::Heading { .. }))
            .expect("heading");
        let body = engine.snap_caret(heading.source_range.start, Bias::Right);
        let up = engine.prev_caret(&after, body);
        assert_eq!(engine.snap_caret(up, Bias::Left), up);
        assert!(
            up < heading.source_range.start,
            "arrow-up from the heading must sit on the blank, up={up} heading={:?} in {after:?}",
            heading.source_range
        );
    }

    #[test]
    fn nonempty_heading_enter_at_start_between_blocks_keeps_heading() {
        let source = "hello\n\n# Title\n\nworld";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(atx_body_start(
            source,
            source.find("# Title").expect("heading"),
        ));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("hello") && after.contains("world"),
            "surrounding paragraphs must remain, got {after:?}"
        );
        assert!(
            !after.lines().any(line_is_empty_atx_heading),
            "must not leave `# `, got {after:?}"
        );
        assert!(
            after
                .lines()
                .any(|line| line_is_atx_heading_title(line, 1, "Title")),
            "heading must stay a heading, got {after:?}"
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains("hello") && typed.contains("world"),
            "surrounding paragraphs must remain after typing, got {typed:?}"
        );
        let x_line = typed
            .lines()
            .find(|line| line.contains('x'))
            .expect("typed x");
        assert!(
            typed_line_is_paragraph(x_line) && !x_line.contains("Title"),
            "typed text belongs in the new paragraph above, got {typed:?}"
        );
        assert!(
            typed
                .lines()
                .any(|line| line_is_atx_heading_title(line, 1, "Title")),
            "Title must stay a heading, got {typed:?}"
        );
    }

    #[test]
    fn insert_in_standard_separator_creates_paragraph_not_heading_chrome() {
        let source = "hello\n\n# Title";
        let (mut doc, mut engine, mut caret) = setup(source);
        let gap = blank_caret_gap_before(engine.tree(), 1).expect("separator gap");
        caret.collapse_to(gap.start);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains("hello") && typed.contains("# Title"),
            "surrounding blocks must remain, got {typed:?}"
        );
        let x_line = typed
            .lines()
            .find(|line| line.contains('x'))
            .expect("typed x");
        assert!(
            typed_line_is_paragraph(x_line) && !x_line.contains('#'),
            "typing in the gap must be a new paragraph, not heading chrome, got {typed:?}"
        );
        assert!(
            typed
                .lines()
                .any(|line| line_is_atx_heading_title(line, 1, "Title")),
            "heading must stay a heading, got {typed:?}"
        );
    }

    #[test]
    fn insert_in_trailing_blank_appends_paragraph_not_into_last_block() {
        let source = "hello\n\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let gap = blank_caret_gap_after_last(engine.tree()).expect("trailing gap");
        caret.collapse_to(gap.start);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains("hello"),
            "last paragraph must remain, got {typed:?}"
        );
        let hello_line = typed
            .lines()
            .find(|line| line.contains("hello"))
            .expect("hello line");
        assert!(
            !hello_line.contains('x'),
            "typing must not prepend into the last paragraph, got {typed:?}"
        );
        let x_line = typed
            .lines()
            .find(|line| line.contains('x'))
            .expect("typed x");
        assert!(
            typed_line_is_paragraph(x_line) && x_line.trim() == "x",
            "typing below the last block must append a new paragraph, got {typed:?}"
        );
    }

    #[test]
    fn toggle_mark_empty_caret_in_trailing_blank_inserts_pair() {
        let source = "hello\n\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let gap = blank_caret_gap_after_last(engine.tree()).expect("trailing gap");
        caret.collapse_to(gap.start);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains("**x**") && typed.contains("hello"),
            "empty Cmd-B in the trailing blank must wrap the new paragraph, got {typed:?}"
        );
        let hello_line = typed
            .lines()
            .find(|line| line.contains("hello"))
            .expect("hello line");
        assert!(
            !hello_line.contains('*'),
            "wrap must not attach to the last paragraph, got {typed:?}"
        );
    }

    #[test]
    fn insert_in_newlines_only_document_types_a_paragraph() {
        for source in ["", "\n", "\n\n"] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let gap = blank_caret_gap_after_last(engine.tree()).expect("caret home");
            caret.collapse_to(gap.start);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            assert!(
                typed.contains('x'),
                "typing in a newlines-only document must insert, {source:?} got {typed:?}"
            );
            let x_line = typed
                .lines()
                .find(|line| line.contains('x'))
                .expect("typed x");
            assert!(
                typed_line_is_paragraph(x_line) && x_line.trim() == "x",
                "must be a paragraph, {source:?} got {typed:?}"
            );
        }
    }

    #[test]
    fn toggle_mark_empty_caret_in_newlines_only_inserts_pair() {
        let (mut doc, mut engine, mut caret) = setup("\n\n");
        let gap = blank_caret_gap_after_last(engine.tree()).expect("caret home");
        caret.collapse_to(gap.start);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains("**x**"),
            "empty Cmd-B on a newlines-only document must wrap, got {typed:?}"
        );
    }

    fn leftover_click_then_type(source: &str) -> String {
        let (mut doc, mut engine, mut caret) = setup(source);
        place_caret_for_click_below(&mut doc, &mut engine, &mut caret);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        )
    }

    #[test]
    fn insert_at_click_below_content_appends_paragraph() {
        for source in [
            "hello\n\n",
            "hello",
            "hello\n",
            "# Title",
            "![cat](pic.png)",
            "---",
        ] {
            let typed = leftover_click_then_type(source);
            assert!(
                typed.lines().any(|line| line.trim() == "x"),
                "leftover click + type must be a new paragraph, {source:?} got {typed:?}"
            );
            assert!(
                !typed.contains("hellox")
                    && !typed.contains("Titlex")
                    && !typed.contains("png)x")
                    && !typed.contains("---x"),
                "must not continue the last block, {source:?} got {typed:?}"
            );
        }
    }

    #[test]
    fn click_below_without_trailing_blank_opens_blank_not_eof() {
        for (source, opened) in [("hello", "hello\n\n"), ("hello\n", "hello\n\n")] {
            let (mut doc, mut engine, mut caret) = setup(source);
            assert_eq!(
                place_caret_for_click_below(&mut doc, &mut engine, &mut caret),
                RichOutcome::Changed
            );
            assert_eq!(doc.buffer.content(), opened);
            let gap = blank_caret_gap_after_last(engine.tree()).expect("opened trailing blank");
            assert_eq!(caret.cursor(), gap.start);
            assert_eq!(
                place_caret_for_click_below(&mut doc, &mut engine, &mut caret),
                RichOutcome::Noop,
                "second leftover click must reuse the trailing blank, {source:?}"
            );
            assert_eq!(doc.buffer.content(), opened);
        }
    }

    #[test]
    fn toggle_mark_after_click_below_without_blank_wraps_new_paragraph() {
        let (mut doc, mut engine, mut caret) = setup("hello");
        place_caret_for_click_below(&mut doc, &mut engine, &mut caret);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains("**x**") && typed.contains("hello"),
            "empty Cmd-B after leftover click must wrap the new paragraph, got {typed:?}"
        );
        let hello_line = typed
            .lines()
            .find(|line| line.contains("hello"))
            .expect("hello line");
        assert!(
            !hello_line.contains('*'),
            "wrap must not attach to the last paragraph, got {typed:?}"
        );
    }

    #[test]
    fn insert_at_eof_on_last_line_still_continues_paragraph() {
        let source = "hello";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert_eq!(
            doc.buffer.content(),
            "hellox",
            "click on the last line (EOF in the paragraph) must still continue it"
        );
    }

    #[test]
    fn toggle_mark_empty_caret_in_separator_inserts_pair() {
        let source = "hello\n\n# Title";
        let (mut doc, mut engine, mut caret) = setup(source);
        let gap = blank_caret_gap_before(engine.tree(), 1).expect("separator gap");
        caret.collapse_to(gap.start);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains("**x**") && typed.contains("# Title") && typed.contains("hello"),
            "empty Cmd-B in the gap must wrap the new paragraph, got {typed:?}"
        );
        assert!(
            !typed.contains("**#") && !typed.contains("# **"),
            "wrap must not attach to heading chrome, got {typed:?}"
        );
    }

    #[test]
    fn nonempty_heading_in_list_enter_at_start_inserts_list_item() {
        let source = "- # Title";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(atx_body_start(source, 0));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            !after.starts_with('\n'),
            "must not insert a document-level blank above the list, got {after:?}"
        );
        assert!(
            !after.lines().any(line_is_empty_atx_heading),
            "must not leave an empty heading, got {after:?}"
        );
        let lines: Vec<_> = after.lines().collect();
        let blank_item = lines.first().copied().unwrap_or("");
        let blank_body = list_marker_prefix(after_quote(blank_item))
            .map(|m| after_quote(blank_item).get(m.len()..).unwrap_or("").trim())
            .unwrap_or("not-a-list");
        assert!(
            lines.len() >= 2
                && blank_body.is_empty()
                && lines.iter().any(|line| {
                    let after = after_quote(line);
                    list_marker_prefix(after)
                        .is_some_and(|m| after.get(m.len()..).unwrap_or("").trim_end() == "# Title")
                }),
            "Enter at start of `- # Title` must insert a blank list item above, got {after:?}"
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        let x_line = typed
            .lines()
            .find(|line| line.contains('x'))
            .expect("typed x");
        assert!(
            list_marker_prefix(after_quote(x_line)).is_some() && typed_line_is_paragraph(x_line),
            "typing must stay a list item, got {typed:?}"
        );
        assert!(
            typed.lines().any(
                |line| list_marker_prefix(after_quote(line)).is_some_and(|m| {
                    after_quote(line).get(m.len()..).unwrap_or("").trim_end() == "# Title"
                })
            ),
            "original heading-in-list must remain, got {typed:?}"
        );

        let quoted = "> - # Title";
        let (mut doc, mut engine, mut caret) = setup(quoted);
        caret.collapse_to(atx_body_start(quoted, 0));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.lines().next().is_some_and(|line| {
                line.starts_with('>')
                    && list_marker_prefix(after_quote(line)).is_some_and(|m| {
                        after_quote(line)
                            .get(m.len()..)
                            .unwrap_or("")
                            .trim()
                            .is_empty()
                    })
            }),
            "quoted heading-in-list Enter must insert a quoted blank item, got {after:?}"
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        let x_line = typed
            .lines()
            .find(|line| line.contains('x'))
            .expect("typed x");
        assert!(
            x_line.trim_start().starts_with('>')
                && list_marker_prefix(after_quote(x_line)).is_some()
                && typed_line_is_paragraph(x_line),
            "quoted heading-in-list Enter must insert a quoted list item, got {typed:?}"
        );
        assert!(
            typed.lines().any(|line| line.starts_with('>')
                && list_marker_prefix(after_quote(line)).is_some_and(|m| {
                    after_quote(line).get(m.len()..).unwrap_or("").trim_end() == "# Title"
                })),
            "quoted `- # Title` must remain, got {typed:?}"
        );
    }

    #[test]
    fn nonempty_quoted_heading_enter_at_start_keeps_quote_and_heading() {
        let source = "> # Title";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(atx_body_start(source, 0));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            !after.lines().any(line_is_empty_atx_heading),
            "must not leave `> # `, got {after:?}"
        );
        assert!(
            after
                .lines()
                .any(|line| line.starts_with('>') && line_is_atx_heading_title(line, 1, "Title")),
            "quoted heading must stay `> # Title`, got {after:?}"
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        let x_line = typed
            .lines()
            .find(|line| line.contains('x'))
            .expect("typed x");
        assert!(
            x_line.trim_start().starts_with('>') && typed_line_is_paragraph(x_line),
            "text after quoted heading Enter at start must stay quoted, got {typed:?}"
        );
        assert!(
            typed
                .lines()
                .any(|line| line.starts_with('>') && line_is_atx_heading_title(line, 1, "Title")),
            "quoted heading must survive, got {typed:?}"
        );

        let nested = "> > # Title";
        let (mut doc, mut engine, mut caret) = setup(nested);
        caret.collapse_to(atx_body_start(nested, 0));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after
                .lines()
                .any(|line| after_quote(line).trim_end() == "# Title"
                    && quote_marker_prefix(line).is_some_and(|p| p.matches('>').count() >= 2)),
            "nested quoted heading must keep `>`, got {after:?}"
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        let x_line = typed
            .lines()
            .find(|line| line.contains('x'))
            .expect("typed x");
        assert!(
            quote_marker_prefix(x_line).is_some_and(|p| p.matches('>').count() >= 2)
                && typed_line_is_paragraph(x_line),
            "nested quote depth must stay on the new paragraph, got {typed:?}"
        );
    }

    #[test]
    fn nonempty_setext_heading_enter_at_start_inserts_paragraph_above() {
        for (source, underline) in [("Title\n=====\n", "====="), ("Title\n-----\n", "-----")] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(0);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert!(
                after.contains("Title") && after.contains(underline),
                "setext heading must stay intact, got {after:?}"
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            let x_line = typed
                .lines()
                .find(|line| line.contains('x'))
                .expect("typed x");
            assert!(
                typed_line_is_paragraph(x_line) && !x_line.contains("Title"),
                "typed text must be a paragraph above the setext heading, got {typed:?}"
            );
            assert!(
                typed.contains("Title") && typed.contains(underline),
                "setext underline must remain, got {typed:?}"
            );
            engine.sync(&doc);
            assert!(
                engine.tree().blocks.iter().any(|b| matches!(
                    b.kind,
                    BlockKind::Heading {
                        style: HeadingStyle::Setext,
                        ..
                    }
                )),
                "must remain a setext heading, got {:?}",
                engine
                    .tree()
                    .blocks
                    .iter()
                    .map(|b| &b.kind)
                    .collect::<Vec<_>>()
            );
        }

        let quoted = "> Title\n> =====\n";
        let (mut doc, mut engine, mut caret) = setup(quoted);
        caret.collapse_to(quote_prefix(quoted.lines().next().unwrap_or(quoted)).len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after
                .lines()
                .any(|line| after_quote(line).trim_end() == "Title")
                && after
                    .lines()
                    .any(|line| is_setext_underline(after_quote(line))),
            "quoted setext must stay a heading, got {after:?}"
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        let x_line = typed
            .lines()
            .find(|line| line.contains('x'))
            .expect("typed x");
        assert!(
            x_line.trim_start().starts_with('>') && typed_line_is_paragraph(x_line),
            "quoted setext Enter at start must keep `>` on the new paragraph, got {typed:?}"
        );
        assert!(
            typed
                .lines()
                .any(|line| line.starts_with('>') && after_quote(line).trim_end() == "Title"),
            "quoted setext title must stay quoted, got {typed:?}"
        );
    }

    #[test]
    fn empty_setext_heading_enter_converts_if_caret_can_sit_on_it() {
        let source = "a\n=====\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(1);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        engine.sync(&doc);
        let offset = caret.cursor();
        if empty_heading_at(&engine, offset) {
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            engine.sync(&doc);
            assert!(
                !caret_in_heading(&engine, caret.cursor()),
                "empty setext Enter must drop heading chrome, got {after:?}"
            );
            assert!(
                !after.contains("=====") && !after.contains("-----"),
                "setext underline must be gone, got {after:?}"
            );
        }
    }

    #[test]
    fn empty_list_line_sees_marker_after_quote() {
        assert!(empty_list_line("> - ", "> - ".len()), "quoted bullet");
        assert!(empty_list_line("> 1. ", "> 1. ".len()), "quoted ordered");
        assert!(empty_list_line("> - [ ] ", "> - [ ] ".len()), "quoted task");
        assert!(
            empty_list_line("> > - ", "> > - ".len()),
            "nested quoted bullet"
        );
        assert!(
            empty_list_line("- ", 2),
            "unquoted empty list still matches"
        );
        assert!(
            !empty_list_line("> ", 2),
            "empty quote without a list marker is not an empty list"
        );
        assert!(
            !empty_list_line("> hello", 7),
            "quoted paragraph is not an empty list"
        );
    }

    fn last_line(source: &str) -> &str {
        source.lines().last().unwrap_or(source)
    }

    fn line_is_empty_quoted_paragraph(line: &str) -> bool {
        let Some(quote) = quote_marker_prefix(line) else {
            return false;
        };
        let after = &line[quote.len()..];
        list_marker_prefix(after).is_none() && after.trim().is_empty()
    }

    #[test]
    fn empty_quoted_list_item_enter_becomes_quoted_paragraph() {
        let (mut doc, mut engine, mut caret) = setup("> - hello\n> - ");
        caret.collapse_to(doc.buffer.len_bytes());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.starts_with("> - hello"),
            "quoted list body must remain, got {after:?}"
        );
        assert!(
            line_is_empty_quoted_paragraph(last_line(&after)),
            "empty quoted list Enter must become `> `, got {after:?}"
        );
        assert!(
            !last_line(&after).contains('-'),
            "list marker must be gone: {after:?}"
        );
        engine.sync(&doc);
        let leaf = engine.block_at(caret.cursor()).expect("caret in a block");
        assert!(
            ancestor_is_quote(&engine, leaf),
            "caret must stay in the quote after exiting the list"
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        let x_line = typed
            .lines()
            .find(|line| line.contains('x'))
            .expect("typed x");
        assert!(
            x_line.trim_start().starts_with('>') && !x_line.contains('-'),
            "text after exiting the quoted list must stay quoted, got {typed:?}"
        );
    }

    #[test]
    fn typing_on_empty_quote_inserts_in_the_body() {
        let (mut doc, mut engine, mut caret) = setup("> ");
        caret.collapse_to(engine.snap_caret(0, Bias::Right));
        assert_eq!(caret.cursor(), 2, "caret must sit after `> `");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.starts_with("> x"),
            "typing on empty quote must yield `> x`, got {typed:?}"
        );
    }

    #[test]
    fn typing_on_empty_quote_without_marker_space_inserts_body() {
        let (mut doc, mut engine, mut caret) = setup(">");
        caret.collapse_to(engine.snap_caret(0, Bias::Right));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.starts_with("> x"),
            "typing on empty `>` must yield `> x`, got {typed:?}"
        );
        assert!(
            !typed.starts_with(">x"),
            "must insert the marker space, got {typed:?}"
        );
        assert_eq!(
            caret.cursor(),
            "> x".len(),
            "caret must sit after the typed body"
        );

        let (mut doc, mut engine, mut caret) = setup(">");
        caret.collapse_to(engine.snap_caret(0, Bias::Right));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText(" ".into()),
        );
        assert_eq!(
            doc.buffer.content(),
            "> ",
            "typing a space on `>` must not double it, got {:?}",
            doc.buffer.content()
        );
    }

    #[test]
    fn typing_on_empty_list_markers_without_marker_space_inserts_body() {
        // Unquoted `-` / `*` / `1.` without a space are paragraphs (input
        // rules / `---` / italic). Quoted empty lists are real list nodes.
        for (source, want_prefix) in [(">-", ">- x"), ("> -", "> - x"), (">*", ">* x")] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(engine.snap_caret(0, Bias::Right));
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            assert!(
                typed.starts_with(want_prefix),
                "typing on empty `{source}` must yield `{want_prefix}`, got {typed:?}"
            );
        }
        let (mut doc, mut engine, mut caret) = setup(">1.");
        caret.collapse_to(engine.snap_caret(0, Bias::Right));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains('x') && !typed.contains("1.x") && !typed.contains(">x"),
            "quoted ordered marker without a space must not glue `x`, got {typed:?}"
        );
    }

    #[test]
    fn typing_on_empty_list_item_inserts_in_the_body() {
        let (mut doc, mut engine, mut caret) = setup("- ");
        caret.collapse_to(engine.snap_caret(0, Bias::Right));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.starts_with("- x"),
            "typing on empty list must yield `- x`, got {typed:?}"
        );
        assert!(!typed.starts_with("x-"), "must not eat the list marker");
    }

    #[test]
    fn typing_on_empty_ordered_item_inserts_in_the_body() {
        let (mut doc, mut engine, mut caret) = setup("1. ");
        caret.collapse_to(engine.snap_caret(0, Bias::Right));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.starts_with("1. x"),
            "typing on empty ordered item must yield `1. x`, got {typed:?}"
        );
    }

    #[test]
    fn empty_quoted_ordered_list_item_enter_becomes_quoted_paragraph() {
        let (mut doc, mut engine, mut caret) = setup("> 1. hello\n> 1. ");
        caret.collapse_to(doc.buffer.len_bytes());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.starts_with("> 1. hello"),
            "quoted ordered body must remain, got {after:?}"
        );
        assert!(
            line_is_empty_quoted_paragraph(last_line(&after)),
            "empty quoted ordered Enter must become `> `, got {after:?}"
        );
    }

    #[test]
    fn empty_quoted_task_item_enter_becomes_quoted_paragraph() {
        let (mut doc, mut engine, mut caret) = setup("> - [ ] hello\n> - [ ] ");
        caret.collapse_to(doc.buffer.len_bytes());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("> - [ ] hello"),
            "quoted task body must remain, got {after:?}"
        );
        assert!(
            line_is_empty_quoted_paragraph(last_line(&after)),
            "empty quoted task Enter must become `> `, got {after:?}"
        );
        assert!(
            !last_line(&after).contains('['),
            "task marker must be gone: {after:?}"
        );
    }

    #[test]
    fn nested_quote_empty_list_item_enter_keeps_quote_depth() {
        let (mut doc, mut engine, mut caret) = setup("> > - nested\n> > - ");
        caret.collapse_to(doc.buffer.len_bytes());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("> > - nested"),
            "nested quoted list body must remain, got {after:?}"
        );
        let last = last_line(&after);
        assert!(
            last.starts_with("> >") && list_marker_prefix(after_quote(last)).is_none(),
            "must exit the list and keep both quote levels, got {after:?}"
        );
        assert!(
            line_is_empty_quoted_paragraph(last),
            "nested empty quoted list must become `> > `, got {after:?}"
        );
    }

    #[test]
    fn quoted_nested_empty_list_item_enter_outdents_inside_quote() {
        let (mut doc, mut engine, mut caret) = setup("> - hello\n>   - ");
        caret.collapse_to(doc.buffer.len_bytes());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("> - hello") && after.contains("\n> - ") && !after.contains("\n>   - "),
            "quoted nested empty item must outdent one list level, got {after:?}"
        );
        assert!(
            !line_is_empty_quoted_paragraph(last_line(&after)),
            "still a quoted list item after one outdent, got {after:?}"
        );
    }

    #[test]
    fn quoted_list_enter_continues_the_quoted_list() {
        let (mut doc, mut engine, mut caret) = setup("> - hello");
        caret.collapse_to(doc.buffer.len_bytes());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.starts_with("> - hello\n> -") || after.starts_with("> - hello\n> *"),
            "Enter on a quoted list item must continue the quoted list, got {after:?}"
        );
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            line_is_empty_quoted_paragraph(last_line(&after)),
            "second Enter on the empty quoted list item must become `> `, got {after:?}"
        );
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            !after.trim_end().ends_with('>'),
            "third Enter on the empty quoted paragraph must leave the quote, got {after:?}"
        );
    }

    #[test]
    fn insert_line_break_in_paragraph_is_backslash_newline() {
        let (mut doc, mut engine, mut caret) = setup("hello world\n");
        caret.collapse_to("hello".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertLineBreak,
        );
        assert!(
            after.contains("hello\\\n world") || after.contains("hello\\\nworld"),
            "Shift-Enter outside a table must insert a markdown hard break, got {after:?}"
        );
        assert!(
            !after.contains("<br>"),
            "paragraph hard break is not HTML <br>: {after:?}"
        );
    }

    /// Shift-Enter in a quote / list / task / alert must keep container
    /// prefixes on the continuation line (Typora; same chrome Enter/paste
    /// already copy). A bare `\\\n` is a lazy continuation and paints the
    /// next line like body prose.
    #[test]
    fn insert_line_break_in_quote_and_list_keeps_container_prefix() {
        for (source, after_break, typed) in [
            ("> hello", "> hello\\\n> ", "> hello\\\n> x"),
            ("> > nested", "> > nested\\\n> > ", "> > nested\\\n> > x"),
            ("- hello", "- hello\\\n  ", "- hello\\\n  x"),
            ("* hello", "* hello\\\n  ", "* hello\\\n  x"),
            ("+ hello", "+ hello\\\n  ", "+ hello\\\n  x"),
            ("1. hello", "1. hello\\\n   ", "1. hello\\\n   x"),
            ("1) hello", "1) hello\\\n   ", "1) hello\\\n   x"),
            ("> - hello", "> - hello\\\n>   ", "> - hello\\\n>   x"),
            (
                "- [x] done",
                "- [x] done\\\n      ",
                "- [x] done\\\n      x",
            ),
            (
                "+ [x] done",
                "+ [x] done\\\n      ",
                "+ [x] done\\\n      x",
            ),
            (
                "1. [x] done",
                "1. [x] done\\\n       ",
                "1. [x] done\\\n       x",
            ),
            (
                "> [!NOTE]\n> hello",
                "> [!NOTE]\n> hello\\\n> ",
                "> [!NOTE]\n> hello\\\n> x",
            ),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertLineBreak,
            );
            assert_eq!(
                after, after_break,
                "Shift-Enter must keep container chrome, {source:?} got {after:?}"
            );
            assert!(
                after.contains("\\\n"),
                "must stay a markdown hard break, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("<br>"),
                "quote/list hard break is not HTML <br>, {source:?} got {after:?}"
            );
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            assert_eq!(
                after, typed,
                "typing after Shift-Enter must sit after the prefix, {source:?} got {after:?}"
            );
            engine.sync(&doc);
            if count_list_items(&engine.tree().blocks) > 0 {
                assert_eq!(
                    count_list_items(&engine.tree().blocks),
                    1,
                    "Shift-Enter must not open a sibling item, {source:?} got {after:?}"
                );
            }
        }

        let (mut doc, mut engine, mut caret) = setup("> hello");
        caret.collapse_to("> he".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertLineBreak,
        );
        assert_eq!(
            after, "> he\\\n> llo",
            "mid-quote Shift-Enter must prefix the rest, got {after:?}"
        );
    }

    /// Shift-Enter in definition details must keep `: ` (quoted keep `>`)
    /// like Enter/`details_split_text`. A bare `\\\n` is a lazy
    /// continuation that can break the list. Terms must not gain `: `.
    #[test]
    fn insert_line_break_in_definition_details_keeps_colon_prefix() {
        for (source, after_break, typed) in [
            (
                "Term\n: hello",
                "Term\n: hello\\\n: ",
                "Term\n: hello\\\n: x",
            ),
            (
                "Term\n\n: hello",
                "Term\n\n: hello\\\n: ",
                "Term\n\n: hello\\\n: x",
            ),
            (
                "> Term\n> : hello",
                "> Term\n> : hello\\\n> : ",
                "> Term\n> : hello\\\n> : x",
            ),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            assert!(
                has_definition_list(&engine.tree().blocks),
                "fixture must parse as a definition list: {source:?}"
            );
            let end = source.find("hello").expect("hello") + "hello".len();
            caret.collapse_to(end);
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertLineBreak,
            );
            assert_eq!(
                after, after_break,
                "Shift-Enter in details must keep `: `, {source:?} got {after:?}"
            );
            assert!(
                after.contains("\\\n"),
                "must stay a markdown hard break, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("<br>"),
                "details hard break is not HTML <br>, {source:?} got {after:?}"
            );
            assert!(
                has_definition_list(&engine.tree().blocks),
                "must remain a definition list after Shift-Enter, {source:?} -> {after:?}"
            );
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            assert_eq!(
                after, typed,
                "typing after details Shift-Enter must sit after `: `, {source:?} got {after:?}"
            );
            engine.sync(&doc);
            assert!(
                has_definition_list(&engine.tree().blocks),
                "must remain a definition list after typing, {source:?} -> {after:?}"
            );
        }

        let (mut doc, mut engine, mut caret) = setup("Term\n: hello");
        caret.collapse_to("Term\n: he".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertLineBreak,
        );
        assert_eq!(
            after, "Term\n: he\\\n: llo",
            "mid-details Shift-Enter must prefix the rest with `: `, got {after:?}"
        );
        assert!(
            has_definition_list(&engine.tree().blocks),
            "mid-details Shift-Enter must keep a definition list, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup("Term\n: hello");
        caret.collapse_to("Term".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertLineBreak,
        );
        assert!(
            after.contains("Term\\") && after.contains(": hello"),
            "term Shift-Enter must keep the details opener, got {after:?}"
        );
        assert!(
            !after.contains("Term\\\n: \n") && !after.contains("Term\\\n: : hello"),
            "term Shift-Enter must not insert `: ` (that would turn the term into details), got {after:?}"
        );
        assert!(
            has_definition_list(&engine.tree().blocks),
            "term Shift-Enter must keep a definition list, got {after:?}"
        );
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert!(
            after.contains("Term\\\nx") && after.contains(": hello") && !after.contains(": x"),
            "typing after term Shift-Enter must not land on a new `: ` details, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup("> Term\n> : hello");
        caret.collapse_to("> Term".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertLineBreak,
        );
        assert!(
            after.contains("> Term\\") && after.contains("> : hello"),
            "quoted term Shift-Enter must keep quote + details, got {after:?}"
        );
        assert!(
            !after.contains("> Term\\\n> : \n") && !after.contains("> Term\\\n> : : hello"),
            "quoted term Shift-Enter must not insert `: `, got {after:?}"
        );
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert!(
            after.contains("> Term\\\n> x")
                && after.contains("> : hello")
                && !after.contains(": x"),
            "typing after quoted term Shift-Enter must sit after `>`, not `: `, got {after:?}"
        );
    }

    fn list_body_start(source: &str, at: usize) -> usize {
        let start = line_start(source, at);
        let line = current_line(source, start);
        start
            + quote_prefix(line).len()
            + list_marker_prefix(after_quote(line))
                .expect("list marker")
                .len()
    }

    fn quote_body_start(source: &str, at: usize) -> usize {
        let start = line_start(source, at);
        let line = current_line(source, start);
        start + quote_prefix(line).len()
    }

    fn first_line(source: &str) -> &str {
        source.lines().next().unwrap_or(source)
    }

    #[test]
    fn indent_outdent_list_item() {
        let (mut doc, mut engine, mut caret) = setup("- hello\n");
        caret.collapse_to(4);
        let indented = apply(&mut doc, &mut engine, &mut caret, RichCommand::IndentList);
        assert!(
            indented.starts_with("  - hello"),
            "expected indent, got {indented:?}"
        );
        let out = apply(&mut doc, &mut engine, &mut caret, RichCommand::OutdentList);
        assert!(out.starts_with("- hello"), "expected outdent, got {out:?}");
    }

    #[test]
    fn indent_quoted_list_indents_inside_the_quote() {
        let (mut doc, mut engine, mut caret) = setup("> - hello");
        caret.collapse_to(list_body_start("> - hello", 0));
        let indented = apply(&mut doc, &mut engine, &mut caret, RichCommand::IndentList);
        let line = first_line(&indented);
        assert!(
            !line.starts_with(' '),
            "Tab must not put a space before `>`, got {indented:?}"
        );
        assert!(
            line.starts_with('>'),
            "quote marker must stay at column 0, got {indented:?}"
        );
        assert!(
            after_quote(line).starts_with("  - hello")
                || after_quote(line).starts_with("  * hello"),
            "indent belongs after `>`, got {indented:?}"
        );

        let out = apply(&mut doc, &mut engine, &mut caret, RichCommand::OutdentList);
        let restored = first_line(&out);
        assert!(
            after_quote(restored).starts_with("- hello")
                || after_quote(restored).starts_with("* hello"),
            "Shift-Tab must outdent inside the quote, got {out:?}"
        );
        assert!(
            restored.starts_with('>'),
            "outdent must not smash `>`, got {out:?}"
        );

        let para = apply(&mut doc, &mut engine, &mut caret, RichCommand::OutdentList);
        let last = first_line(&para);
        assert!(
            last.starts_with('>') && list_marker_prefix(after_quote(last)).is_none(),
            "outermost quoted outdent becomes a quoted paragraph, got {para:?}"
        );
        assert!(
            last.contains("hello"),
            "item body must remain after stripping the marker, got {para:?}"
        );
    }

    #[test]
    fn indent_nested_quoted_list_and_task_stay_quoted() {
        let (mut doc, mut engine, mut caret) = setup("> - a\n>   - b");
        let nested_at = doc.buffer.content().find("- b").expect("nested");
        caret.collapse_to(nested_at);
        let indented = apply(&mut doc, &mut engine, &mut caret, RichCommand::IndentList);
        assert!(
            indented.contains(">     - b") || indented.contains(">     * b"),
            "nested quoted Tab adds indent after `>`, got {indented:?}"
        );
        assert!(
            !indented.contains(" >"),
            "must not prefix a space before `>`, got {indented:?}"
        );

        let (mut doc, mut engine, mut caret) = setup("> - [ ] task");
        caret.collapse_to(list_body_start("> - [ ] task", 0));
        let indented = apply(&mut doc, &mut engine, &mut caret, RichCommand::IndentList);
        let line = first_line(&indented);
        assert!(
            line.starts_with('>') && after_quote(line).contains("[ ] task"),
            "quoted task Tab stays quoted, got {indented:?}"
        );
        assert!(!line.starts_with(' '), "no leading space, got {indented:?}");
    }

    #[test]
    fn indent_quoted_ordered_list_stays_inside_quote() {
        let (mut doc, mut engine, mut caret) = setup("> 1. hello");
        caret.collapse_to(list_body_start("> 1. hello", 0));
        let indented = apply(&mut doc, &mut engine, &mut caret, RichCommand::IndentList);
        let line = first_line(&indented);
        assert!(
            line.starts_with('>') && after_quote(line).contains("1. hello"),
            "quoted ordered Tab stays quoted, got {indented:?}"
        );
        assert!(!line.starts_with(' '), "{indented:?}");
        apply(&mut doc, &mut engine, &mut caret, RichCommand::OutdentList);
        let para = apply(&mut doc, &mut engine, &mut caret, RichCommand::OutdentList);
        let last = first_line(&para);
        assert!(
            last.starts_with('>') && list_marker_prefix(after_quote(last)).is_none(),
            "outermost quoted ordered outdent is a quoted paragraph, got {para:?}"
        );
    }

    #[test]
    fn backspace_at_start_of_list_item_strips_the_marker() {
        let (mut doc, mut engine, mut caret) = setup("- hello");
        caret.collapse_to(list_body_start("- hello", 0));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert_eq!(
            first_line(&after).trim_end(),
            "hello",
            "unquoted list Backspace at body start becomes a paragraph, got {after:?}"
        );
        assert!(
            list_marker_prefix(first_line(&after)).is_none(),
            "list marker must be gone, got {after:?}"
        );
    }

    /// Tab / extra spaces after `-` / `1.` are marker padding. Backspace at
    /// the body strips the whole opener (`-\t` / `-   `); typing at Home
    /// inserts in the body. Five spaces keep indented-code indent.
    #[test]
    fn list_marker_padding_backspace_and_insert_stay_in_body() {
        for source in [
            "-\titem\n",
            "*\titem\n",
            "1.\titem\n",
            "1)\titem\n",
            "-   item\n",
            "1.   item\n",
            "> -\titem\n",
            "-   [x] done\n",
            "-\t[ ] hello\n",
        ] {
            let needle = if source.contains("done") {
                "done"
            } else if source.contains("hello") {
                "hello"
            } else {
                "item"
            };
            let (mut doc, mut engine, mut caret) = setup(source);
            let home =
                engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
            assert_eq!(
                home,
                source.find(needle).expect(needle),
                "Home must sit on {needle:?}, {source:?} got {home}"
            );
            caret.collapse_to(home);
            let typed = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("z".into()),
            );
            assert!(
                typed.contains(&format!("z{needle}")),
                "typing at Home must extend the body, {source:?} got {typed:?}"
            );
            assert!(
                !typed.starts_with('z'),
                "must not type before the list marker, {source:?} got {typed:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(home);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let first = first_line(&after);
            let after_q = after_quote(first);
            assert!(
                list_marker_prefix(after_q).is_none(),
                "Backspace at body start must strip the list marker, {source:?} got {after:?}"
            );
            if source.starts_with('>') {
                assert!(
                    first.trim_start().starts_with('>'),
                    "quoted list must keep `>`, {source:?} got {after:?}"
                );
            }
            assert!(
                after.contains(needle),
                "body text must survive, {source:?} got {after:?}"
            );
        }

        let five = "-     item\n";
        let (mut doc, mut engine, mut caret) = setup(five);
        let home = engine.clamp_raw_prefix(five, engine.snap_caret(0, Bias::Right), Bias::Right);
        caret.collapse_to(home);
        let typed = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        assert!(
            typed.contains("zitem"),
            "five-space padding must still type in the body, got {typed:?}"
        );
    }

    /// CommonMark code-span padding spaces (` foo `) are dest chrome. Typing
    /// at Home inserts in the painted content; Backspace/Delete do not nibble
    /// the stripped spaces into `` `foo` `` / `` ` foo` ``.
    #[test]
    fn code_span_padding_insert_and_delete_stay_in_body() {
        for source in ["` foo `\n", "> ` foo `\n", "- ` foo `\n"] {
            let needle = "foo";
            let (mut doc, mut engine, mut caret) = setup(source);
            let home =
                engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
            assert_eq!(
                home,
                source.find(needle).expect(needle),
                "Home must sit on {needle:?}, {source:?} got {home}"
            );
            caret.collapse_to(home);
            let typed = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("z".into()),
            );
            assert!(
                typed.contains("zfoo"),
                "typing at Home must extend the code body, {source:?} got {typed:?}"
            );
            assert!(
                typed.contains('`'),
                "ticks must survive insert, {source:?} got {typed:?}"
            );
            assert!(
                !typed.contains("`z foo") && !typed.contains("``z foo"),
                "must not insert before the stripped padding space, {source:?} got {typed:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(home);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            assert!(
                after.contains('`') && after.contains(needle),
                "Backspace at body start must not nibble ticks or unwrap, {source:?} got {after:?}"
            );
            assert!(
                after.contains("` foo") || after.contains("`foo`bar"),
                "Backspace must not nibble the leading padding space, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(home + needle.len());
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            assert!(
                after.contains('`') && after.contains(needle),
                "Delete at body end must not nibble ticks, {source:?} got {after:?}"
            );
            assert!(
                after.contains("foo `") || after.contains("` foo `"),
                "Delete must not nibble the trailing padding space, {source:?} got {after:?}"
            );
        }

        let source = "see ` foo ` now\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let home = source.find("foo").expect("foo");
        caret.collapse_to(home);
        let typed = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        assert!(
            typed.contains("` zfoo `"),
            "typing at foo must keep padding spaces, got {typed:?}"
        );
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(home);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert!(
            after.contains("` foo `") && after.contains("see` foo `"),
            "Backspace must skip padding/ticks and delete the previous visible space, got {after:?}"
        );
    }

    #[test]
    fn list_item_link_with_x_label_does_not_eat_dest_as_task() {
        let source = "- [x](https://e.com)\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let x = source.find("[x]").expect("label") + 1;
        let home = engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
        assert_eq!(home, x, "Home must sit on the link label, got {home}");
        caret.collapse_to(home);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains("[zx](https://e.com)"),
            "typing at Home must extend the label, got {typed:?}"
        );
        assert!(
            !typed.contains("[x](zhttps") && !typed.contains("[x](https://e.comz"),
            "must not type into dest chrome, got {typed:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(x);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("[x](https://e.com)"),
            "Backspace at the label must keep the link, got {after:?}"
        );
        assert!(
            !after.contains("- [x]"),
            "Backspace at the label must strip `- `, got {after:?}"
        );
        assert!(
            !after.trim_start().starts_with("https://") && !after.trim_start().starts_with("x]("),
            "must not eat `[x](` as a task checkbox or nibble `[`, got {after:?}"
        );
        assert!(
            list_marker_prefix(first_line(&after)).is_none(),
            "list marker must be gone, got {after:?}"
        );
    }

    #[test]
    fn nested_list_reference_link_does_not_eat_dest() {
        let source = "- [hello][ref]\n  [ref]: https://e.com\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let hello = source.find("hello").expect("hello");
        let home = engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
        assert_eq!(home, hello, "Home must sit on `h`, got {home}");
        caret.collapse_to(home);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains("[zhello][ref]"),
            "typing at Home must extend the reference label, got {typed:?}"
        );
        assert!(
            !typed.contains("[hello][zref]") && !typed.contains("[hello]z[ref]"),
            "must not type into `[ref]` dest chrome, got {typed:?}"
        );
        assert!(
            typed.contains("[ref]: https://e.com"),
            "nested definition must stay, got {typed:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(hello);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("[hello][ref]") && after.contains("[ref]: https://e.com"),
            "Backspace at the label must keep the reference link, got {after:?}"
        );
        assert!(
            list_marker_prefix(first_line(&after)).is_none(),
            "Backspace at the label must strip `- `, got {after:?}"
        );
        assert!(
            !after.trim_start().starts_with("hello][ref]"),
            "must not nibble `[` off the reference link, got {after:?}"
        );
    }

    #[test]
    fn nested_marked_reference_link_does_not_eat_dest() {
        let source = "- [**hello**][ref]\n  [ref]: https://e.com\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let hello = source.find("hello").expect("hello");
        let home = engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
        assert_eq!(
            home, hello,
            "Home must sit on `h`, not dest `[ref]`, got {home}"
        );
        caret.collapse_to(home);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains("[**zhello**][ref]"),
            "typing at Home must extend the bold label, got {typed:?}"
        );
        assert!(
            !typed.contains("[**hello**][zref]") && !typed.contains("[**hello**]z[ref]"),
            "must not type into dest `[ref]`, got {typed:?}"
        );

        let nested = "- outer\n  - [hello][ref]\n    [ref]: https://e.com\n";
        let (mut doc, mut engine, mut caret) = setup(nested);
        let hello = nested.find("hello").expect("hello");
        let line = nested[..hello].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let home =
            engine.clamp_raw_prefix(nested, engine.snap_caret(line, Bias::Right), Bias::Right);
        assert_eq!(home, hello, "nested-list Home must sit on `h`, got {home}");
        caret.collapse_to(home);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains("[zhello][ref]"),
            "nested-list typing must extend the label, got {typed:?}"
        );
        assert!(
            typed.contains("[ref]: https://e.com"),
            "nested-list definition must stay, got {typed:?}"
        );

        let wrapped = "- [![cat](a.png)][ref]\n  [ref]: https://e.com\n";
        let (mut doc, mut engine, mut caret) = setup(wrapped);
        let img = first_image_range(&engine);
        let home = engine.clamp_raw_prefix(wrapped, engine.snap_caret(0, Bias::Right), Bias::Right);
        assert_eq!(
            home, img.start,
            "Home on wrapping `[![cat]…][ref]` must sit on the image, got {home}"
        );
        caret.collapse_to(home);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains("[z![cat](a.png)][ref]"),
            "typing at Home must insert before the image, not dest, got {typed:?}"
        );
        assert!(
            !typed.contains("[![cat](a.png)][zref]") && !typed.contains("[![cat](a.png)]z[ref]"),
            "must not type into wrapping dest `[ref]`, got {typed:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(wrapped);
        let img = first_image_range(&engine);
        caret.collapse_to(img.start);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("[![cat](a.png)][ref]") && after.contains("[ref]: https://e.com"),
            "Backspace at the image must keep the wrapping reference, got {after:?}"
        );
        assert!(
            list_marker_prefix(first_line(&after)).is_none(),
            "Backspace at the image must strip `- `, got {after:?}"
        );
        assert!(
            !after.trim_start().starts_with("![cat](a.png)][ref]"),
            "must not nibble wrapping `[`, got {after:?}"
        );
    }

    #[test]
    fn list_item_x_at_eol_without_space_does_not_eat_as_task() {
        let shortcut = "- [x]\n\n[x]: https://e.com\n";
        let (mut doc, mut engine, mut caret) = setup(shortcut);
        let x = shortcut.find("[x]").expect("label") + 1;
        let home =
            engine.clamp_raw_prefix(shortcut, engine.snap_caret(0, Bias::Right), Bias::Right);
        assert_eq!(
            home, x,
            "Home must sit on the shortcut-ref label, got {home}"
        );
        caret.collapse_to(home);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains("[zx]"),
            "typing at Home must extend the shortcut-ref label, got {typed:?}"
        );
        assert!(
            !typed.contains("[x]z") && !typed.contains("[x]z\n"),
            "must not type after `[x]` as if it were a task checkbox, got {typed:?}"
        );
        assert!(
            typed.contains("[x]: https://e.com"),
            "reference definition must stay, got {typed:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(shortcut);
        caret.collapse_to(x);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("[x]") && after.contains("[x]: https://e.com"),
            "Backspace at the label must keep the shortcut-ref, got {after:?}"
        );
        assert!(
            list_marker_prefix(first_line(&after)).is_none(),
            "Backspace at the label must strip `- `, got {after:?}"
        );

        let incomplete = "- [x]\n";
        let (mut doc, mut engine, mut caret) = setup(incomplete);
        let home =
            engine.clamp_raw_prefix(incomplete, engine.snap_caret(0, Bias::Right), Bias::Right);
        let close = incomplete.find(']').expect("]");
        assert!(
            home <= close,
            "Home on incomplete `- [x]` must not skip the slot, got {home}"
        );
        caret.collapse_to(home);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            !typed.contains("[x]z"),
            "typing at Home must not append after `[x]` as a checkbox, got {typed:?}"
        );
        assert!(
            typed.contains("z[x]") || typed.contains("[zx]"),
            "typing must land in the `[x]` slot, got {typed:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(incomplete);
        caret.collapse_to(home);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("[x]"),
            "Backspace must not eat `[x]` as a task checkbox, got {after:?}"
        );
    }

    #[test]
    fn backspace_at_start_of_quoted_list_item_keeps_the_quote() {
        let source = "> - hello";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(list_body_start(source, 0));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let line = first_line(&after);
        assert!(
            line.starts_with('>') && list_marker_prefix(after_quote(line)).is_none(),
            "quoted list Backspace at body start becomes a quoted paragraph, got {after:?}"
        );
        assert!(
            line.contains("hello"),
            "body grapheme must not be deleted, got {after:?}"
        );
        assert!(
            !line.contains('-'),
            "list marker must be gone, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup("> - hello");
        caret.collapse_to("> - h".len());
        let mid = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert!(
            mid.contains("> -") && !mid.contains("hello"),
            "Backspace mid-item still deletes a grapheme, got {mid:?}"
        );
    }

    #[test]
    fn backspace_at_start_of_nested_quoted_list_outdents_inside_quote() {
        let source = "> - a\n>   - hello";
        let (mut doc, mut engine, mut caret) = setup(source);
        let nested = source.find("- hello").expect("nested");
        caret.collapse_to(list_body_start(source, nested));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert!(
            after.contains("> - a")
                && after.contains("> - hello")
                && !after.contains(">   - hello"),
            "nested quoted Backspace at start outdents inside the quote, got {after:?}"
        );
        assert!(
            !after.contains(" >"),
            "must not smash `>` with a leading space, got {after:?}"
        );
    }

    #[test]
    fn backspace_at_start_of_quoted_ordered_and_task_strips_marker() {
        let (mut doc, mut engine, mut caret) = setup("> 1. hello");
        caret.collapse_to(list_body_start("> 1. hello", 0));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let line = first_line(&after);
        assert!(
            line.starts_with('>') && list_marker_prefix(after_quote(line)).is_none(),
            "quoted ordered Backspace at start, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup("> - [ ] hello");
        caret.collapse_to(list_body_start("> - [ ] hello", 0));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let line = first_line(&after);
        assert!(
            line.starts_with('>')
                && list_marker_prefix(after_quote(line)).is_none()
                && line.contains("hello"),
            "quoted task Backspace at start, got {after:?}"
        );
    }

    #[test]
    fn backspace_at_start_of_heading_converts_to_paragraph() {
        let source = "# Title";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(atx_body_start(source, 0));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert_eq!(
            first_line(&after).trim_end(),
            "Title",
            "Backspace at heading start must strip `#`, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            !caret_in_heading(&engine, caret.cursor()),
            "heading must become a paragraph, got {:?}",
            engine.tree().blocks[0].kind
        );

        let (mut doc, mut engine, mut caret) = setup("# Title");
        caret.collapse_to(atx_body_start("# Title", 0) + "T".len());
        let mid = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert!(
            mid.contains("#") && mid.contains("itle") && !mid.contains("Title"),
            "mid-heading Backspace still deletes a grapheme, got {mid:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(
                engine.tree().blocks[0].kind,
                BlockKind::Heading { level: 1, .. }
            ),
            "mid-heading Backspace must not drop heading chrome, got {:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn backspace_at_start_of_heading_in_list_strips_heading_first() {
        let source = "- # Title";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(atx_body_start(source, 0));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let line = first_line(&after);
        assert!(
            list_marker_prefix(after_quote(line)).is_some()
                && !line.contains('#')
                && line.contains("Title"),
            "heading-in-list Backspace at Title must strip `#` and keep the list, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            !caret_in_heading(&engine, caret.cursor()),
            "must no longer be a heading, got {:?}",
            engine
                .tree()
                .blocks
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>()
        );
        caret.collapse_to(list_body_start(&after, 0));
        let para = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let para_line = first_line(&para);
        assert!(
            list_marker_prefix(after_quote(para_line)).is_none() && para_line.contains("Title"),
            "second Backspace strips the list marker, got {para:?}"
        );
    }

    #[test]
    fn backspace_at_start_of_atx_levels_1_to_6_converts_to_paragraph() {
        for level in 1u8..=6 {
            let hashes = "#".repeat(level as usize);
            let source = format!("{hashes} Title");
            let (mut doc, mut engine, mut caret) = setup(&source);
            caret.collapse_to(atx_body_start(&source, 0));
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            assert_eq!(
                first_line(&after).trim_end(),
                "Title",
                "h{level} Backspace at start must become a paragraph, got {after:?}"
            );
        }
    }

    #[test]
    fn backspace_at_start_of_quoted_heading_keeps_the_quote() {
        let source = "> # Title";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(atx_body_start(source, 0));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let line = first_line(&after);
        assert!(
            line.starts_with('>') && !line.contains('#') && line.contains("Title"),
            "quoted heading Backspace at start becomes a quoted paragraph, got {after:?}"
        );
    }

    #[test]
    fn backspace_at_start_of_setext_heading_strips_the_underline() {
        let source = "Title\n=====\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(0);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert!(
            after.contains("Title") && !after.contains("====="),
            "setext Backspace at start must strip the underline, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            !caret_in_heading(&engine, caret.cursor()),
            "setext heading must become a paragraph, got {:?}",
            engine.tree().blocks.first().map(|b| &b.kind)
        );

        let (mut doc, mut engine, mut caret) = setup("Title\n-----\n");
        caret.collapse_to(0);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert!(
            after.contains("Title") && !after.contains("-----"),
            "setext h2 Backspace at start must strip the underline, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup("Title\n=====\n");
        caret.collapse_to("T".len());
        let mid = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert!(
            mid.contains("itle") && mid.contains("=====") && !mid.contains("Title"),
            "mid-setext Backspace still deletes a grapheme, got {mid:?}"
        );
    }

    #[test]
    fn indent_list_in_table_navigates_cells_instead_of_inserting_spaces() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let cell_a = engine.tree().blocks[0].children[0].children[0]
            .source_range
            .start;
        caret.collapse_to(cell_a);
        let before = doc.buffer.content();
        apply(&mut doc, &mut engine, &mut caret, RichCommand::IndentList);
        assert_eq!(
            doc.buffer.content(),
            before,
            "Tab in a table must not insert indent spaces"
        );
        let pos = engine.table_pos(caret.cursor()).expect("still in table");
        assert_eq!(
            pos.col, 1,
            "IndentList in a table must TableTab to the next cell"
        );
        apply(&mut doc, &mut engine, &mut caret, RichCommand::OutdentList);
        let back = engine.table_pos(caret.cursor()).expect("still in table");
        assert_eq!(back.col, 0, "OutdentList in a table must Shift-Tab");
        assert_eq!(doc.buffer.content(), before);

        let last_cell = engine.tree().blocks[0]
            .children
            .last()
            .and_then(|row| row.children.last())
            .expect("last cell")
            .source_range
            .start;
        caret.collapse_to(last_cell);
        let rows_before = engine.tree().blocks[0].children.len();
        apply(&mut doc, &mut engine, &mut caret, RichCommand::IndentList);
        engine.sync(&doc);
        assert!(
            engine.tree().blocks[0].children.len() > rows_before,
            "Tab on the last cell must insert a row, got {}",
            doc.buffer.content()
        );
    }

    #[test]
    fn split_block_in_table_inserts_br_instead_of_breaking_the_row() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let after_a = source.find('a').expect("header a") + 1;
        caret.collapse_to(after_a);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("<br>"),
            "Enter in a table cell must insert <br>, got {after:?}"
        );
        assert!(
            !after.contains("a\n") && !after.contains("a\r"),
            "Enter must not splice a newline into the GFM table row: {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Table { .. }),
            "table must survive Enter, got {:?}",
            engine.tree().blocks[0].kind
        );
        assert_eq!(
            engine.tree().blocks[0].children[0].children.len(),
            2,
            "header must keep two cells, got {}",
            after
        );
        assert!(
            engine.table_pos(caret.cursor()).is_some(),
            "caret must stay in the table after Enter"
        );

        // IME / InsertText("\\n") shares SplitBlock.
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('1').expect("body 1") + 1);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n".into()),
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Table { .. }),
            "InsertText newline must not break the table, got {}",
            doc.buffer.content()
        );
        assert!(
            doc.buffer.content().contains("<br>"),
            "{}",
            doc.buffer.content()
        );
    }

    #[test]
    fn insert_line_break_in_table_is_br_not_backslash_newline() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('a').expect("header a") + 1);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertLineBreak,
        );
        assert!(
            after.contains("<br>"),
            "Shift-Enter in a table cell must insert <br>, got {after:?}"
        );
        assert!(
            !after.contains("\\\n"),
            "backslash-newline would split the GFM row: {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Table { .. }),
            "{:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn indent_list_in_table_code_span_still_tabs() {
        let source = "| `x` | y |\n| --- | --- |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let code_off = source.find('x').expect("code span");
        caret.collapse_to(code_off);
        assert!(engine.in_raw_context(code_off), "caret in inline code");
        assert!(engine.in_table(code_off));
        let before = doc.buffer.content();
        apply(&mut doc, &mut engine, &mut caret, RichCommand::IndentList);
        assert_eq!(
            doc.buffer.content(),
            before,
            "Tab inside table inline-code must not insert spaces"
        );
        let pos = engine.table_pos(caret.cursor()).expect("still in table");
        assert_eq!(pos.col, 1);
    }

    #[test]
    fn toggle_link_wraps_selection() {
        let (mut doc, mut engine, mut caret) = setup("hello\n");
        caret.range = 0..5;
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        assert_eq!(
            after, "[hello]()\n",
            "Cmd-K on a selection must use empty (), got {after:?}"
        );
        assert!(
            !after.contains("<>"),
            "empty dest must not serialize as <>, got {after:?}"
        );
        let url_at = after
            .find("[hello](")
            .map(|i| i + "[hello](".len())
            .expect("url slot");
        assert_eq!(
            caret.cursor(),
            url_at,
            "Cmd-K on a selection must leave the caret in the URL, got {} in {after:?}",
            caret.cursor()
        );
        assert!(
            caret.range.is_empty(),
            "URL caret must be collapsed, got {:?}",
            caret.range
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("https://e.com".into()),
        );
        assert_eq!(
            doc.buffer.content(),
            "[hello](https://e.com)\n",
            "typing a URL must fill (), not prepend inside <>"
        );
    }

    #[test]
    fn insert_text_replaces_empty_angle_destination() {
        let source = "[hello](<>)\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let at = source.find("<>").expect("empty dest");
        caret.collapse_to(at);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("https://e.com".into()),
        );
        assert_eq!(
            doc.buffer.content(),
            "[hello](https://e.com)\n",
            "InsertText at <> must replace the brackets, not type inside"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(at + 1);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("https://e.com".into()),
        );
        assert_eq!(
            doc.buffer.content(),
            "[hello](https://e.com)\n",
            "InsertText inside <> must replace the brackets"
        );
    }

    #[test]
    fn backspace_at_start_of_link_label_does_not_eat_bracket() {
        let source = "see [label](https://e.com) now\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('l').expect("label"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("[label](https://e.com)"),
            "Backspace at the start of a link label must not nibble `[`, got {after:?}"
        );
        assert!(
            after.contains("see[label]") || after.contains("see [label]"),
            "expected the previous visible character to be deleted, got {after:?}"
        );
        assert!(
            !after.contains("see label]("),
            "broken dest leftover `label](` means `[` was eaten, got {after:?}"
        );
    }

    #[test]
    fn delete_at_end_of_link_label_does_not_eat_dest() {
        let source = "see [label](https://e.com) now\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let end_label = source.find("label").unwrap() + "label".len();
        caret.collapse_to(end_label);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            after.contains("[label](https://e.com)"),
            "Delete at the end of a link label must not swallow `](url)`, got {after:?}"
        );
    }

    #[test]
    fn backspace_at_start_of_bold_does_not_eat_delimiter() {
        let source = "hello **bold**\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('b').expect("bold"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("**bold**"),
            "Backspace at the start of bold must not nibble `*`, got {after:?}"
        );
        assert!(
            !after.contains("hello *bold**") && !after.contains("hello **bold*"),
            "unbalanced emphasis after Backspace, got {after:?}"
        );
    }

    #[test]
    fn backspace_at_emphasis_wrapping_a_link_does_not_eat_marks() {
        for (source, keep, glued) in [
            (
                "see **[hello](https://e.com)** now\n",
                "**[hello](https://e.com)**",
                "see**[",
            ),
            (
                "see *[hello](https://e.com)* now\n",
                "*[hello](https://e.com)*",
                "see*[",
            ),
            (
                "see ~~[hello](https://e.com)~~ now\n",
                "~~[hello](https://e.com)~~",
                "see~~[",
            ),
            (
                "see **[hello][ref]** now\n\n[ref]: https://e.com\n",
                "**[hello][ref]**",
                "see**[",
            ),
            (
                "> see **[hello](https://e.com)** now\n",
                "**[hello](https://e.com)**",
                "see**[",
            ),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.find("hello").expect("hello"));
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                after.contains(keep),
                "Backspace at a wrapped-link label must keep {keep:?}, {source:?} got {after:?}"
            );
            assert!(
                after.contains(glued),
                "must delete the previous visible space, not wrapping marks, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("see [hello]") && !after.contains("see *[hello]"),
                "eaten wrapping mark leftover, {source:?} got {after:?}"
            );
        }

        let source = "see **[hello](https://e.com)** now\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let end = source.find("hello").unwrap() + "hello".len();
        caret.collapse_to(end);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            after.contains("**[hello](https://e.com)**"),
            "Delete at the end of a wrapped-link label must not swallow `](url)**`, got {after:?}"
        );
    }

    #[test]
    fn backspace_at_emphasis_wrapping_link_siblings_does_not_eat_marks() {
        for (source, keep, glued) in [
            ("see **![alt](a.png)** now\n", "**![alt](a.png)**", "see**!"),
            (
                "see *[![cat](a.png)](https://e.com)* now\n",
                "*[![cat](a.png)](https://e.com)*",
                "see*[!",
            ),
            (
                "see ~~[hello][ref]~~ now\n\n[ref]: https://e.com\n",
                "~~[hello][ref]~~",
                "see~~[",
            ),
            ("see **<b>hello</b>** now\n", "**<b>hello</b>**", "see**<"),
            (
                "see *<a href=\"https://e.com\">hello</a>* now\n",
                "*<a href=\"https://e.com\">hello</a>*",
                "see*<",
            ),
            (
                "see ***[hello](https://e.com)*** now\n",
                "***[hello](https://e.com)***",
                "see***[",
            ),
            (
                "see [<b>hello</b>](https://e.com) now\n",
                "[<b>hello</b>](https://e.com)",
                "see[<",
            ),
            (
                "see **<img src=\"a.png\">** now\n",
                "**<img src=\"a.png\">**",
                "see**<",
            ),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let at = if source.contains("![") || source.contains("<img") {
                first_image_range(&engine).start
            } else {
                source.find("hello").expect("hello")
            };
            caret.collapse_to(at);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                after.contains(keep),
                "Backspace at wrapped sibling must keep {keep:?}, {source:?} got {after:?}"
            );
            assert!(
                after.contains(glued),
                "must delete the previous visible space, not wrapping marks, {source:?} got {after:?}"
            );
        }

        let html = "see **<b>hello</b>** now\n";
        let (mut doc, mut engine, mut caret) = setup(html);
        caret.collapse_to(html.find("hello").unwrap() + "hello".len());
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            after.contains("**<b>hello</b>**"),
            "Delete at HTML inner end must not swallow `</b>**`, got {after:?}"
        );

        let nested = "see ***[hello](https://e.com)*** now\n";
        let (mut doc, mut engine, mut caret) = setup(nested);
        caret.collapse_to(nested.find("hello").unwrap() + "hello".len());
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            after.contains("***[hello](https://e.com)***"),
            "Delete at nested `***[hello]` must not swallow dest+marks, got {after:?}"
        );
    }

    /// Emptying the last inner character of `==highlight==` / `~~strike~~` /
    /// `**bold**` must unwrap the marks, not leave `====` / `~~~~` / `****`
    /// painted as chrome. Empty Cmd-B inserting `****` is a different path.
    #[test]
    fn backspace_emptying_highlight_or_strike_unwraps_marks() {
        for (source, inner, leftover) in [
            ("==m==", "m", "===="),
            ("~~x~~", "x", "~~~~"),
            ("**b**", "b", "****"),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.find(inner).expect(inner) + inner.len());
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains(leftover),
                "Backspace emptying {source:?} must not leave {leftover:?}, got {after:?}"
            );
            assert!(
                after.trim().is_empty(),
                "Backspace emptying {source:?} must unwrap to an empty paragraph, got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.find(inner).expect(inner));
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains(leftover),
                "Delete emptying {source:?} must not leave {leftover:?}, got {after:?}"
            );
            assert!(
                after.trim().is_empty(),
                "Delete emptying {source:?} must unwrap to an empty paragraph, got {after:?}"
            );
        }

        let source = "==hello==";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('o').expect("o") + 1);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("==hell=="),
            "partial highlight delete must keep the marks, got {after:?}"
        );
    }

    #[test]
    fn backspace_at_start_of_email_autolink_does_not_eat_bracket() {
        let source = "see <user@example.com> now\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("user").expect("user"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("<user@example.com>"),
            "Backspace at the start of an email autolink must not nibble `<`, got {after:?}"
        );
        assert!(
            !after.contains("see user@example.com>"),
            "broken leftover `user@example.com>` means `<` was eaten, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("user@example.com").unwrap() + "user@example.com".len());
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            after.contains("<user@example.com>"),
            "Delete at the end of an email autolink must not swallow `>`, got {after:?}"
        );
    }

    #[test]
    fn backspace_at_start_of_url_autolink_does_not_eat_bracket() {
        let source = "see <https://example.com> now\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("https").expect("https"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("<https://example.com>"),
            "Backspace at the start of a URL autolink must not nibble `<`, got {after:?}"
        );
        assert!(
            !after.contains("see https://example.com>"),
            "broken leftover `https://example.com>` means `<` was eaten, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret
            .collapse_to(source.find("https://example.com").unwrap() + "https://example.com".len());
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            after.contains("<https://example.com>"),
            "Delete at the end of a URL autolink must not swallow `>`, got {after:?}"
        );
    }

    #[test]
    fn backspace_at_linked_image_does_not_eat_wrapping_dest() {
        let source = "see [![cat](a.png)](https://e.com) now\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let img = engine.tree().blocks[0]
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::Image { source_range, .. } => Some(source_range.clone()),
                _ => None,
            })
            .expect("linked image");
        caret.collapse_to(img.start);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("[![cat](a.png)](https://e.com)"),
            "Backspace at a linked inline image must not nibble wrapping `[`, got {after:?}"
        );
        assert!(
            !after.contains("see ![cat](a.png)]("),
            "broken dest leftover `![…](url)](…)` means wrapping `[` was eaten, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        let img = engine.tree().blocks[0]
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::Image { source_range, .. } => Some(source_range.clone()),
                _ => None,
            })
            .expect("linked image");
        caret.collapse_to(img.end);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            after.contains("[![cat](a.png)](https://e.com)"),
            "Delete after a linked inline image must not swallow wrapping `](url)`, got {after:?}"
        );
    }

    #[test]
    fn table_pipe_dest_chrome_is_not_nibbled() {
        for source in [
            "| a | b |\n|---|---|\n| 1 | 2 |\n",
            "| a |\n|---|\n| 1 |\n",
            "> | a | b |\n> |---|---|\n> | 1 | 2 |\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let home =
                engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
            assert_eq!(
                source.as_bytes().get(home).copied(),
                Some(b'a'),
                "Home must sit on `a`, not `|`, {source:?} home={home}"
            );
            caret.collapse_to(home);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                after.contains('|') && after.contains("a") && after.contains("---"),
                "Backspace at table Home must not nibble `|`, {source:?} got {after:?}"
            );
            assert!(
                after.contains("| a |") || after.contains("> | a |"),
                "leading `|` must stay, {source:?} got {after:?}"
            );
            assert!(
                !after.starts_with('a') && !after.contains("\na |"),
                "must not strip the opening pipe, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.find('a').expect("a") + 1);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                after.matches('|').count() == source.matches('|').count(),
                "Delete after `a` must not nibble `|`, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(home);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            assert!(
                typed.contains("| xa |")
                    || typed.contains("| xa | b |")
                    || typed.contains("> | xa |"),
                "typing at table Home must insert in the cell, {source:?} got {typed:?}"
            );
            assert!(
                !typed.contains("x|") && !typed.starts_with('x'),
                "must not type into dest `|`, {source:?} got {typed:?}"
            );
        }
    }

    /// Compact GFM `foo|bar` (no wrapping pipes): `|` is dest chrome.
    /// Backspace/Delete/InsertText must not nibble it or split the row.
    #[test]
    fn compact_gfm_table_pipe_dest_chrome_is_not_nibbled() {
        fn header_columns(engine: &RichEngine) -> usize {
            fn walk(blocks: &[Block]) -> Option<usize> {
                for b in blocks {
                    if matches!(b.kind, BlockKind::Table { .. }) {
                        return Some(b.children.first()?.children.len());
                    }
                    if let Some(found) = walk(&b.children) {
                        return Some(found);
                    }
                }
                None
            }
            walk(&engine.tree().blocks).expect("table columns")
        }
        for source in [
            "foo|bar\n---|---\nbaz|bim\n",
            "foo | bar\n--- | ---\nbaz | bim\n",
            "> foo|bar\n> ---|---\n> baz|bim\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let home =
                engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
            let first = source.find("foo").expect("foo");
            assert_eq!(
                home, first,
                "Home must sit on `f`, not `|`, {source:?} home={home}"
            );
            caret.collapse_to(home);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                after.contains('|') && after.contains("foo") && after.contains("---"),
                "Backspace at compact-table Home must not nibble `|`, {source:?} got {after:?}"
            );
            engine.sync(&doc);
            assert_eq!(
                header_columns(&engine),
                2,
                "Backspace must not split/merge compact columns, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            let o = source.find("foo").expect("foo") + 2;
            caret.collapse_to(o + 1);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert_eq!(
                after.matches('|').count(),
                source.matches('|').count(),
                "Delete after the first cell must not nibble `|`, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(home);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            assert!(
                typed.contains("xfoo") || typed.contains("> xfoo"),
                "typing at compact-table Home must insert in the cell, {source:?} got {typed:?}"
            );
            assert!(
                !typed.contains("x|") && !typed.contains("|xfoo"),
                "must not type into dest `|`, {source:?} got {typed:?}"
            );
            engine.sync(&doc);
            assert_eq!(
                header_columns(&engine),
                2,
                "insert must keep two columns, got {typed:?}"
            );

            let pipe = source.find('|').expect("pipe");
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(pipe);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            engine.sync(&doc);
            assert_eq!(
                header_columns(&engine),
                2,
                "InsertText at compact `|` must stay in a cell, {source:?} got {typed:?}"
            );
            assert!(
                typed.contains("foox|")
                    || typed.contains("foo x|")
                    || typed.contains("foo|x")
                    || typed.contains("foo |x")
                    || typed.contains("> foox|")
                    || typed.contains("> foo|x"),
                "insert at the compact pipe must stay in a cell, {source:?} got {typed:?}"
            );
        }
    }

    /// GFM `\|` is a literal pipe in the cell, not a column boundary.
    /// Typing / wrap / Backspace at or next to it must not split the row.
    #[test]
    fn typing_at_escaped_table_pipe_does_not_split_the_row() {
        fn escaped_pipe_at(source: &str) -> usize {
            source
                .find("\\|")
                .map(|i| i + 1)
                .expect("escaped table pipe")
        }
        fn table_block(engine: &RichEngine) -> &Block {
            fn walk(blocks: &[Block]) -> Option<&Block> {
                for b in blocks {
                    if matches!(b.kind, BlockKind::Table { .. }) {
                        return Some(b);
                    }
                    if let Some(found) = walk(&b.children) {
                        return Some(found);
                    }
                }
                None
            }
            walk(&engine.tree().blocks).expect("table")
        }
        fn header_columns(engine: &RichEngine) -> usize {
            table_block(engine).children[0].children.len()
        }
        fn still_two_col_table(engine: &RichEngine, after: &str, label: &str) {
            assert!(
                matches!(table_block(engine).kind, BlockKind::Table { .. }),
                "{label}: table must survive, got {after:?}"
            );
            assert_eq!(
                header_columns(engine),
                2,
                "{label}: must keep two columns (no split on \\|), got {after:?}"
            );
            assert!(
                after.contains('|'),
                "{label}: GFM pipes must remain, got {after:?}"
            );
        }
        fn cell_keeps_literal_pipe(after: &str, label: &str) {
            assert!(
                after.contains("\\|"),
                "{label}: cell must keep escaped \\| as a literal pipe, got {after:?}"
            );
            // A bare `|` after a lost backslash (`a\x|b`, `a\**|`, `a\\||b`)
            // is an extra column. `\|\|` (two literal pipes) is fine.
            assert!(
                !after.contains("\\x|")
                    && !after.contains("\\**|")
                    && !after.contains("\\*|")
                    && !after.contains("\\||"),
                "{label}: must not un-escape \\| by inserting between \\\\ and |, got {after:?}"
            );
        }

        let sources = [
            "| a\\|b | c |\n|---|---|\n| 1 | 2 |\n",
            "| a | b\\| |\n|---|---|\n| 1 | 2 |\n",
            "| a | b\\|\n|---|---|\n| 1 | 2 |\n",
            "> | a\\|b | c |\n> |---|---|\n> | 1 | 2 |\n",
        ];
        for source in sources {
            let pipe = escaped_pipe_at(source);
            let slash = pipe - 1;
            let after_pair = pipe + 1;
            let before = slash.saturating_sub(1);
            let positions = [before, slash, pipe, after_pair];

            for at in positions {
                if at > source.len() {
                    continue;
                }
                let (mut doc, mut engine, mut caret) = setup(source);
                engine.sync(&doc);
                if !engine.in_table(at) && at != source.len() {
                    continue;
                }
                caret.collapse_to(at);
                apply(
                    &mut doc,
                    &mut engine,
                    &mut caret,
                    RichCommand::InsertText("x".into()),
                );
                let after = doc.buffer.content();
                engine.sync(&doc);
                still_two_col_table(
                    &engine,
                    &after,
                    &format!("InsertText x @{at} in {source:?}"),
                );
                cell_keeps_literal_pipe(&after, &format!("InsertText x @{at} in {source:?}"));
                if source.contains('>') {
                    assert!(
                        after
                            .lines()
                            .any(|line| line.trim_start().starts_with('>') && line.contains('|')),
                        "quoted table must keep `>`, {source:?} got {after:?}"
                    );
                }

                let (mut doc, mut engine, mut caret) = setup(source);
                caret.collapse_to(at);
                apply(
                    &mut doc,
                    &mut engine,
                    &mut caret,
                    RichCommand::ToggleMark(MarkSet::BOLD),
                );
                let after = doc.buffer.content();
                engine.sync(&doc);
                still_two_col_table(&engine, &after, &format!("wrap @{at} in {source:?}"));
                cell_keeps_literal_pipe(&after, &format!("wrap @{at} in {source:?}"));
                if source.contains('>') {
                    assert!(
                        after
                            .lines()
                            .any(|line| line.trim_start().starts_with('>') && line.contains('|')),
                        "quoted wrap must keep `>`, {source:?} got {after:?}"
                    );
                }

                let (mut doc, mut engine, mut caret) = setup(source);
                caret.collapse_to(at);
                apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
                let after = doc.buffer.content();
                engine.sync(&doc);
                still_two_col_table(&engine, &after, &format!("Backspace @{at} in {source:?}"));
                assert!(
                    !after.contains("\\x|") && header_columns(&engine) == 2,
                    "Backspace must not leave a bare | column split, {source:?} got {after:?}"
                );
                if source.contains('>') {
                    assert!(
                        after.contains('>'),
                        "quoted Backspace must keep `>`, {source:?} got {after:?}"
                    );
                }

                let (mut doc, mut engine, mut caret) = setup(source);
                caret.collapse_to(at);
                apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
                let after = doc.buffer.content();
                engine.sync(&doc);
                still_two_col_table(&engine, &after, &format!("Delete @{at} in {source:?}"));
                if source.contains('>') {
                    assert!(
                        after.contains('>'),
                        "quoted Delete must keep `>`, {source:?} got {after:?}"
                    );
                }
            }

            // Paste / type `|` in the cell and next to `\|` must stay escaped.
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(slash);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("|".into()),
            );
            let after = doc.buffer.content();
            engine.sync(&doc);
            still_two_col_table(&engine, &after, &format!("paste | at \\\\ in {source:?}"));
            cell_keeps_literal_pipe(&after, &format!("paste | at \\\\ in {source:?}"));

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(pipe);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("|".into()),
            );
            let after = doc.buffer.content();
            engine.sync(&doc);
            still_two_col_table(
                &engine,
                &after,
                &format!("paste | on | of \\| in {source:?}"),
            );
            cell_keeps_literal_pipe(&after, &format!("paste | on | of \\| in {source:?}"));
        }

        // Alignment row `|---|` stays dest chrome (not a third column) after
        // editing a header cell that already has `\|`.
        let aligned = "| a\\|b | c |\n|:--|--:|\n| 1 | 2 |\n";
        let pipe = escaped_pipe_at(aligned);
        let (mut doc, mut engine, mut caret) = setup(aligned);
        caret.collapse_to(pipe);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let after = doc.buffer.content();
        engine.sync(&doc);
        still_two_col_table(&engine, &after, "alignment-row sibling");
        cell_keeps_literal_pipe(&after, "alignment-row sibling");
        assert!(
            after.contains(":--") && after.contains("--:"),
            "alignment row must stay, got {after:?}"
        );

        // Unescaped `|` are still cell boundaries: Home typing stays in-cell.
        let plain = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(plain);
        let home = engine.clamp_raw_prefix(plain, engine.snap_caret(0, Bias::Right), Bias::Right);
        caret.collapse_to(home);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains("| xa |"),
            "unescaped | must stay a cell boundary, got {typed:?}"
        );

        // Last unescaped `|` at EOF still opens a paragraph.
        let eof = "| a | b |\n|---|---|\n| 1 | 2 |";
        let (mut doc, mut engine, mut caret) = setup(eof);
        caret.collapse_to(eof.len());
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("| 1 | 2 |\nx") || after.ends_with("|\nx"),
            "last | at EOF must start a new paragraph, got {after:?}"
        );
        assert!(
            !after.contains("|x") && !after.contains("| 2 |x"),
            "must not glue onto the closing pipe, got {after:?}"
        );
    }

    #[test]
    fn reference_definition_dest_chrome_is_not_nibbled() {
        for source in [
            "[hello][ref]\n\n[ref]: https://e.com\n",
            "[ref]: https://e.com\n",
            "> [hello][ref]\n>\n> [ref]: https://e.com\n",
            "- [hello][ref]\n  [ref]: https://e.com\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let dest = source.find("https://e.com").expect("dest");
            caret.collapse_to(dest);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            assert!(
                after.contains("https://e.com"),
                "dest must survive, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("[ref]https")
                    && !after.contains("[ref] https://e.com")
                    && !after.contains("]:https"),
                "Backspace at dest start must not nibble `: ` leaving `[ref]url`, {source:?} got {after:?}"
            );
            if source.contains("[hello][ref]") {
                assert!(
                    after.contains("[hello][ref]") || after.contains("[hello]["),
                    "must not drop `[hello][ref]` to fix dest chrome, {source:?} got {after:?}"
                );
            }
            if source.contains('>') {
                assert!(
                    after.contains('>'),
                    "quoted definition must keep `>`, {source:?} got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(dest);
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::DeleteWordLeft,
            );
            assert!(
                !after.contains("[ref]https") && !after.contains("[ref] https://e.com"),
                "Option-Backspace at dest start must not nibble `: `, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(dest);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            assert!(
                typed.contains("xhttps://e.com"),
                "typing at dest start must insert in dest, {source:?} got {typed:?}"
            );
            assert!(
                typed.contains("[ref]:") || typed.contains("[ref]: "),
                "opener `[ref]:` must stay, {source:?} got {typed:?}"
            );
            assert!(
                !typed.contains("x[ref]") && !typed.contains("[ref]x:"),
                "typing at dest must not splice into opener chrome, {source:?} got {typed:?}"
            );
        }
    }

    #[test]
    fn backspace_delete_do_not_nibble_math_wiki_emoji_dest() {
        let math = "see $x^2$ here\n";
        let (mut doc, mut engine, mut caret) = setup(math);
        caret.collapse_to(math.find('x').expect("x"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("$x^2$"),
            "Backspace at math must not nibble `$`, got {after:?}"
        );
        assert!(
            !after.contains("see x^2$"),
            "broken leftover `x^2$` means `$` was eaten, got {after:?}"
        );
        assert!(
            after.contains("see$x^2$") || after.contains("see $x^2$"),
            "expected the previous visible character to be deleted, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(math);
        caret.collapse_to(math.find('2').expect("2") + 1);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            after.contains("$x^2$"),
            "Delete at math end must not swallow `$`, got {after:?}"
        );

        let wiki = "see [[page]] here\n";
        let (mut doc, mut engine, mut caret) = setup(wiki);
        caret.collapse_to(wiki.find("page").expect("page"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("[[page]]"),
            "Backspace at wiki must not nibble `[`, got {after:?}"
        );
        assert!(
            !after.contains("see page]]"),
            "broken leftover `page]]` means `[[` was eaten, got {after:?}"
        );
        assert!(
            after.contains("see[[page]]") || after.contains("see [[page]]"),
            "expected the previous visible character to be deleted, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(wiki);
        caret.collapse_to(wiki.find("page").expect("page") + "page".len());
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            after.contains("[[page]]"),
            "Delete at wiki end must not swallow `]]`, got {after:?}"
        );

        let piped = "see [[page|Label]] here\n";
        let (mut doc, mut engine, mut caret) = setup(piped);
        caret.collapse_to(piped.find("Label").expect("Label"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("[[page|Label]]"),
            "Backspace at a piped wiki label must not nibble `[[page|`, got {after:?}"
        );

        let emoji = "see :smile: here\n";
        let (mut doc, mut engine, mut caret) = setup(emoji);
        caret.collapse_to(emoji.find("smile").expect("smile"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains(":smile:"),
            "Backspace at emoji must not nibble `:`, got {after:?}"
        );
        assert!(
            !after.contains("see smile:"),
            "broken leftover `smile:` means opening `:` was eaten, got {after:?}"
        );
        assert!(
            after.contains("see:smile:") || after.contains("see :smile:"),
            "expected the previous visible character to be deleted, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(emoji);
        caret.collapse_to(emoji.find("smile").expect("smile") + "smile".len());
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            after.contains(":smile:"),
            "Delete at emoji end must not swallow `:`, got {after:?}"
        );
    }

    fn first_image_range(engine: &RichEngine) -> std::ops::Range<usize> {
        fn walk(blocks: &[Block]) -> Option<std::ops::Range<usize>> {
            for b in blocks {
                if let Some(r) = crate::rich::engine::html_block_image_range(b) {
                    return Some(r);
                }
                for inline in &b.inlines {
                    match inline {
                        Inline::Image { source_range, .. } => return Some(source_range.clone()),
                        Inline::OpaqueInline {
                            raw, source_range, ..
                        } if crate::html_visual::html_inline_image(raw).is_some() => {
                            return Some(source_range.clone());
                        }
                        _ => {}
                    }
                }
                if let Some(r) = walk(&b.children) {
                    return Some(r);
                }
            }
            None
        }
        walk(&engine.tree().blocks).expect("image")
    }

    #[test]
    fn collapsed_and_shortcut_ref_edits_do_not_eat_dest() {
        for source in [
            "[foo][] bar\n\n[foo]: https://e.com\n",
            "[foo] bar\n\n[foo]: https://e.com\n",
            "[foo][ref] bar\n\n[ref]: https://e.com\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let f = source.find("foo").expect("foo");
            let home =
                engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
            assert_eq!(home, f, "Home on {source:?} must be the label");
            caret.collapse_to(home);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("z".into()),
            );
            let typed = doc.buffer.content();
            assert!(
                typed.contains("[zfoo]"),
                "typing at Home must extend the label, got {typed:?}"
            );
            assert!(
                !typed.contains("[foo]z")
                    && !typed.contains("[foo][]z")
                    && !typed.contains("[foo][zref]"),
                "must not type into collapsed/shortcut dest chrome, got {typed:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            let end = source.find("foo").unwrap() + 3;
            caret.collapse_to(end);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                after.contains("[foo]")
                    && (after.contains("[foo][]")
                        || after.contains("[foo][ref]")
                        || after.contains("[foo] bar")
                        || after.contains("[foo]bar")),
                "Delete at end of label must not swallow `[]` / `[ref]`, got {after:?}"
            );
            assert!(
                !after.contains("[foo] bar") || after.contains("bar"),
                "Delete must keep dest chrome, got {after:?}"
            );
            assert!(
                after.contains("][]")
                    || after.contains("][ref]")
                    || (source.contains("[foo] bar") && after.contains("[foo]")),
                "dest chrome must survive Delete, got {after:?}"
            );
        }
    }

    #[test]
    fn list_and_quote_image_edits_are_atomic() {
        for source in [
            "- ![alt](u.png)\n",
            "> ![alt](u.png)\n",
            "- hello ![alt](u.png)\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let img = first_image_range(&engine);
            caret.collapse_to(img.end);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("![alt]") && !after.contains("](u.png)"),
                "Backspace after a list/quote image must delete the whole `![…](url)`, got {after:?}"
            );
            assert!(
                !after.contains("alt](u") && !after.contains("![alt]"),
                "must not leave dest leftover, got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            let img = first_image_range(&engine);
            caret.collapse_to(img.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("![alt]") && !after.contains("](u.png)"),
                "Delete before a list/quote image must delete the whole image, got {after:?}"
            );
        }
    }

    #[test]
    fn html_img_edits_are_atomic() {
        let source = "hello <img src=\"a.png\" alt=\"x\"> world\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let img = first_image_range(&engine);
        caret.collapse_to(img.end);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            !after.contains("<img") && !after.contains("src="),
            "Backspace after HTML `<img>` must delete the whole tag, got {after:?}"
        );
        assert!(after.contains("hello"), "{after:?}");
        assert!(after.contains("world"), "{after:?}");

        let (mut doc, mut engine, mut caret) = setup(source);
        let img = first_image_range(&engine);
        caret.collapse_to(img.start);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            !after.contains("<img") && !after.contains("src="),
            "Delete before HTML `<img>` must delete the whole tag, got {after:?}"
        );
    }

    #[test]
    fn html_block_img_in_list_or_quote_edits_are_atomic() {
        for source in [
            "<img src=\"a.png\" alt=\"x\">\n",
            "- <img src=\"a.png\" alt=\"x\">\n",
            "> <img src=\"a.png\" alt=\"x\">\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let img = first_image_range(&engine);
            caret.collapse_to(img.end);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<img") && !after.contains("src="),
                "Backspace after HTML-block `<img>` must delete the whole tag, got {after:?}"
            );
            assert!(
                !after.contains("a.png"),
                "must not leave dest leftover, got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            let img = first_image_range(&engine);
            caret.collapse_to(img.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<img") && !after.contains("src="),
                "Delete before HTML-block `<img>` must delete the whole tag, got {after:?}"
            );
        }
    }

    #[test]
    fn html_block_svg_edits_are_atomic() {
        let inline = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\"><rect width=\"8\" height=\"8\" fill=\"#f00\"/></svg>\n";
        let block = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\">\n<rect width=\"8\" height=\"8\" fill=\"#f00\"/>\n</svg>\n";
        for source in [
            inline.to_string(),
            format!("- {inline}"),
            format!("> {inline}"),
            block.to_string(),
        ] {
            let (mut doc, mut engine, mut caret) = setup(&source);
            let img = first_image_range(&engine);
            caret.collapse_to(img.end);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<svg") && !after.contains("fill="),
                "Backspace after HTML-block `<svg>` must delete the whole tag, got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(&source);
            let img = first_image_range(&engine);
            caret.collapse_to(img.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<svg") && !after.contains("fill="),
                "Delete before HTML-block `<svg>` must delete the whole tag, got {after:?}"
            );
        }
    }

    fn first_thematic_break_range(engine: &RichEngine) -> std::ops::Range<usize> {
        fn walk(blocks: &[Block]) -> Option<std::ops::Range<usize>> {
            for b in blocks {
                if let Some(r) = thematic_break_range(b) {
                    return Some(r);
                }
                if let Some(r) = walk(&b.children) {
                    return Some(r);
                }
            }
            None
        }
        walk(&engine.tree().blocks).expect("thematic break")
    }

    fn first_html_break_range(engine: &RichEngine) -> std::ops::Range<usize> {
        fn walk(blocks: &[Block]) -> Option<std::ops::Range<usize>> {
            for b in blocks {
                if let Some(r) = html_block_break_range(b) {
                    return Some(r);
                }
                for inline in &b.inlines {
                    if let Inline::OpaqueInline {
                        raw, source_range, ..
                    } = inline
                    {
                        if crate::html_visual::html_inline_break(raw) {
                            return Some(source_range.clone());
                        }
                    }
                }
                if let Some(r) = walk(&b.children) {
                    return Some(r);
                }
            }
            None
        }
        walk(&engine.tree().blocks).expect("html break")
    }

    fn first_footnote_ref_range(engine: &RichEngine) -> std::ops::Range<usize> {
        fn walk(blocks: &[Block]) -> Option<std::ops::Range<usize>> {
            for b in blocks {
                for inline in &b.inlines {
                    if let Inline::OpaqueInline {
                        raw, source_range, ..
                    } = inline
                    {
                        if crate::html_visual::footnote_ref_label(raw).is_some() {
                            return Some(source_range.clone());
                        }
                    }
                }
                if let Some(r) = walk(&b.children) {
                    return Some(r);
                }
            }
            None
        }
        walk(&engine.tree().blocks).expect("footnote ref")
    }

    #[test]
    fn thematic_break_edits_are_atomic() {
        for source in [
            "hello\n\n---\n\nworld\n",
            "hello\n\n***\n\nworld\n",
            "hello\n\n___\n\nworld\n",
            "hello\n\n* * *\n\nworld\n",
            "hello\n\n- - -\n\nworld\n",
            "hello\n\n<hr>\n\nworld\n",
            "hello\n\n<hr/>\n\nworld\n",
            "- <hr>\n",
            "> <hr>\n",
            "> ---\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let rule = first_thematic_break_range(&engine);
            caret.collapse_to(rule.end);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("---")
                    && !after.contains("***")
                    && !after.contains("___")
                    && !after.contains("* *")
                    && !after.contains("- -")
                    && !after.contains("<hr")
                    && !after.contains("hr>"),
                "Backspace after a painted rule must delete the whole rule, got {after:?} from {source:?}"
            );
            if source.contains("hello") {
                assert!(
                    after.contains("hello") && after.contains("world"),
                    "surrounding paragraphs must survive, got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            let rule = first_thematic_break_range(&engine);
            caret.collapse_to(rule.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("---")
                    && !after.contains("***")
                    && !after.contains("___")
                    && !after.contains("* *")
                    && !after.contains("- -")
                    && !after.contains("<hr")
                    && !after.contains("hr>"),
                "Delete before a painted rule must delete the whole rule, got {after:?} from {source:?}"
            );
        }
    }

    #[test]
    fn html_block_br_edits_are_atomic() {
        for source in [
            "hello\n\n<br>\n\nworld\n",
            "<br>\n",
            "<br/>\n",
            "- <br>\n",
            "> <br>\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let br = first_html_break_range(&engine);
            caret.collapse_to(br.end);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<br") && !after.contains("br>") && !after.contains("<br/"),
                "Backspace after HTML-block `<br>` must delete the whole tag, got {after:?} from {source:?}"
            );
            if source.contains("hello") {
                assert!(
                    after.contains("hello") && after.contains("world"),
                    "surrounding paragraphs must survive, got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            let br = first_html_break_range(&engine);
            caret.collapse_to(br.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<br") && !after.contains("br>") && !after.contains("<br/"),
                "Delete before HTML-block `<br>` must delete the whole tag, got {after:?} from {source:?}"
            );
            assert!(
                !after.contains("<br") || !after.contains('<'),
                "must not nibble `<` leftover, got {after:?}"
            );
        }
    }

    #[test]
    fn inline_br_edits_are_atomic() {
        for source in ["a<br>b\n", "a<br/>b\n", "hello <br> world\n"] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let br = first_html_break_range(&engine);
            caret.collapse_to(br.end);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<br") && !after.contains("br>"),
                "Backspace after inline `<br>` must delete the whole tag, got {after:?} from {source:?}"
            );
            assert!(
                after.contains('a') || after.contains("hello"),
                "text before the break must survive, got {after:?}"
            );
            assert!(
                !after.contains("a<b") && !after.contains("a<brb"),
                "must not nibble `>` leftover, got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            let br = first_html_break_range(&engine);
            caret.collapse_to(br.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<br") && !after.contains("br>"),
                "Delete before inline `<br>` must delete the whole tag, got {after:?} from {source:?}"
            );
        }
    }

    #[test]
    fn footnote_ref_edits_are_atomic() {
        for source in [
            "Hello[^1] world\n\n[^1]: the note\n",
            "Hello[^1]\n\n[^1]: the note\n",
            "Hello[^note] world\n\n[^note]: the note\n",
            "> Hello[^1]\n\n[^1]: the note\n",
            "Hello[^1] world\n",
            "Hello[^note] world\n",
            "> Hello[^1]\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let r = first_footnote_ref_range(&engine);
            caret.collapse_to(r.end);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            let para = after.split("\n\n").next().unwrap_or(&after);
            assert!(
                !para.contains("[^"),
                "Backspace after a footnote ref must delete the whole `[^…]`, got {after:?} from {source:?}"
            );
            assert!(
                !para.contains("1]") && !para.contains("note]"),
                "must not leave `1]` dest leftover, got {after:?}"
            );
            assert!(
                after.contains("Hello") || after.contains("hello"),
                "surrounding text must survive, got {after:?}"
            );
            if source.contains("world") {
                assert!(after.contains("world"), "got {after:?}");
            }
            if source.contains("]: the note") {
                assert!(
                    after.contains("[^1]:") || after.contains("[^note]:"),
                    "footnote definition must survive, got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            let r = first_footnote_ref_range(&engine);
            caret.collapse_to(r.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            let para = after.split("\n\n").next().unwrap_or(&after);
            assert!(
                !para.contains("[^"),
                "Delete before a footnote ref must delete the whole `[^…]`, got {after:?} from {source:?}"
            );
            assert!(
                !para.contains("1]") && !para.contains("[^"),
                "must not nibble brackets leaving a partial ref, got {after:?}"
            );
        }
    }

    #[test]
    fn list_link_x_and_gfm_task_survive_br_footnote_widgets() {
        let link = "- [x](https://e.com)\n";
        let (mut doc, mut engine, mut caret) = setup(link);
        let x = link.find("[x]").unwrap() + 1;
        caret.collapse_to(x);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("[zx](https://e.com)"),
            "- [x](url) must stay a link, got {after:?}"
        );

        let task = "- [x] done\n";
        let (mut doc, mut engine, mut caret) = setup(task);
        let d = task.find('d').unwrap();
        caret.collapse_to(d);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("- [x] zdone") || after.contains("- [x]zdone"),
            "- [x] done must stay a task, got {after:?}"
        );
    }

    #[test]
    fn setext_typing_does_not_leak_into_underline() {
        let source = "Title\n=====\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let end = source.find("Title").unwrap() + 5;
        caret.collapse_to(end);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("Titlex") && after.contains("====="),
            "typing at end of setext title must extend the title, got {after:?}"
        );
        assert!(
            !after.contains("=x") && !after.contains("x="),
            "must not type into the setext underline, got {after:?}"
        );

        // Keyboard Down from the title, then type (the view snaps, then InsertText).
        let (mut doc, mut engine, mut caret) = setup(source);
        let t = source.find('T').expect("T");
        let down = engine.vertical_caret(source, t, 1);
        let snapped =
            engine.clamp_raw_prefix(source, engine.snap_caret(down, Bias::Left), Bias::Left);
        caret.collapse_to(snapped);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("====="),
            "Down+type must keep the setext underline, got {after:?}"
        );
        assert!(
            !after.contains("x====") && !after.contains("=x") && !after.contains("====x"),
            "Down+type must not leak into the setext underline, got {after:?}"
        );
    }

    #[test]
    fn nested_quoted_task_typing_lands_on_body() {
        let source = "> - [ ] outer\n>   - [ ] inner\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let i = source.find("inner").expect("inner");
        let home = engine.clamp_raw_prefix(source, engine.snap_caret(i, Bias::Left), Bias::Right);
        caret.collapse_to(home);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        let typed = doc.buffer.content();
        assert!(
            typed.contains("zinner") || typed.contains("[ ] zinner"),
            "typing on nested quoted task must extend the body, got {typed:?}"
        );
        assert!(
            !typed.contains("[z] inner") && !typed.contains("[ ]inner"),
            "must not type into the checkbox, got {typed:?}"
        );
    }

    #[test]
    fn word_delete_at_start_of_link_label_does_not_eat_bracket() {
        let source = "see [label](https://e.com) now\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("label").expect("label"));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("[label](https://e.com)"),
            "Option-Backspace at a link label must not nibble `[`, got {after:?}"
        );
        assert!(
            !after.starts_with("label]"),
            "broken dest leftover `label](` means `[` was eaten, got {after:?}"
        );
    }

    #[test]
    fn set_task_checked_toggles_marker() {
        let (mut doc, mut engine, mut caret) = setup("- [ ] todo\n");
        engine.sync(&doc);
        let id = engine.tree().blocks[0].children[0].id;
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetTaskChecked { id, checked: true },
        );
        assert!(
            doc.buffer.content().contains("- [x] todo"),
            "{}",
            doc.buffer.content()
        );
    }

    fn nested_task_ids(engine: &RichEngine) -> (NodeId, NodeId) {
        let outer = &engine.tree().blocks[0].children[0];
        let inner = outer
            .children
            .iter()
            .find(|c| matches!(c.kind, BlockKind::BulletList { .. }))
            .and_then(|list| list.children.first())
            .expect("nested task item");
        (outer.id, inner.id)
    }

    #[test]
    fn set_task_checked_toggles_nested_item_only() {
        let source = "- [ ] outer\n  - [ ] inner\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        engine.sync(&doc);
        let (_, inner_id) = nested_task_ids(&engine);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetTaskChecked {
                id: inner_id,
                checked: true,
            },
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("- [ ] outer") && after.contains("[x] inner"),
            "inner checkbox must toggle without checking outer, got {after:?}"
        );
        engine.sync(&doc);
        let (outer_id, _) = nested_task_ids(&engine);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetTaskChecked {
                id: outer_id,
                checked: true,
            },
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("[x] outer") && after.contains("[x] inner"),
            "outer checkbox must toggle independently, got {after:?}"
        );
    }

    #[test]
    fn insert_text_at_document_start_does_not_mutate_frontmatter() {
        for source in [
            "---\ntitle: Hello\n---\n\n# Body\n",
            "---\ntitle: Hello\n...\n\n# Body\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(0);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let after = doc.buffer.content();
            let info = crate::parse_frontmatter(&after).expect("frontmatter kept");
            assert_eq!(
                info.title.as_deref(),
                Some("Hello"),
                "YAML title must stay, {source:?} got {after:?}"
            );
            assert!(
                !after.starts_with("x---"),
                "typed text must not prefix the opening fence, {source:?} got {after:?}"
            );
            assert!(
                after[info.end_byte..].contains('x'),
                "typed text must land in the body, {source:?} got {after:?}"
            );
            engine.sync(&doc);
            let fm_end = super::frontmatter_body_start(engine.tree());
            caret.collapse_to(fm_end);
            let before = doc.buffer.content();
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            assert_eq!(
                doc.buffer.content(),
                before,
                "Backspace at the body start must not nibble YAML, {source:?}"
            );
        }
    }

    #[test]
    fn insert_text_at_eof_on_closing_fence_does_not_glue() {
        for source in [
            "---\ntitle: Hello\n---",
            "---\n---",
            "---\ntitle: Hello\n...",
            "---\n...",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_frontmatter_fence(&after, source);
            assert!(
                !after.contains("---x") && !after.contains("...x"),
                "InsertText at EOF must not glue onto the closing fence, {source:?} got {after:?}"
            );
            let info = crate::parse_frontmatter(&after).expect("frontmatter kept");
            assert!(
                after[info.end_byte..].contains('x'),
                "typed text must land in the body, {source:?} got {after:?}"
            );
            assert!(
                !after.starts_with("x---"),
                "must not prefix the opening fence, {source:?} got {after:?}"
            );
        }

        for source in [
            "---\ntitle: Hello\n---\n\n# Body\n",
            "---\ntitle: Hello\n...\n\n# Body\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(0);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let after = doc.buffer.content();
            assert!(
                !after.starts_with("x---") && after.contains('x'),
                "prefix clamp at document start still holds, {source:?} got {after:?}"
            );
        }
    }

    fn assert_not_glued_to_frontmatter_fence(after: &str, source: &str) {
        assert!(
            !after.contains("---x")
                && !after.contains("---*")
                && !after.contains("---`")
                && !after.contains("---[")
                && !after.contains("---#")
                && !after.contains("----")
                && !after.contains("--->")
                && !after.contains("...x")
                && !after.contains("...*")
                && !after.contains("...`")
                && !after.contains("...[")
                && !after.contains("...#")
                && !after.contains("...>")
                && !after.contains("...-"),
            "body wrap/type must not glue onto the closing fence, {source:?} got {after:?}"
        );
        let info = crate::parse_frontmatter(after).expect("frontmatter kept");
        assert!(
            !after.starts_with("x---"),
            "must not prefix the opening fence, {source:?} got {after:?}"
        );
        assert!(
            info.end_byte > 0 && info.end_byte <= after.len(),
            "frontmatter end must stay in range, {source:?} got {after:?}"
        );
    }

    fn type_keys(
        doc: &mut Document,
        engine: &mut RichEngine,
        caret: &mut CaretState,
        keys: &[&str],
    ) -> String {
        for key in keys {
            apply(doc, engine, caret, RichCommand::InsertText((*key).into()));
        }
        doc.buffer.content()
    }

    fn assert_body_after_fence_newline(after: &str, source: &str) {
        if source.ends_with('\n') || after.len() <= source.len() {
            return;
        }
        assert_eq!(
            &after[source.len()..source.len() + 1],
            "\n",
            "body splice at EOF must prepend a newline after atomic chrome, {source:?} got {after:?}"
        );
    }

    fn at_eof_on_closing_fence(
        source: &str,
        cmd: RichCommand,
        then_keys: &[&str],
        body_needle: &str,
    ) {
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        apply(&mut doc, &mut engine, &mut caret, cmd);
        let after = type_keys(&mut doc, &mut engine, &mut caret, then_keys);
        assert_not_glued_to_frontmatter_fence(&after, source);
        assert_body_after_fence_newline(&after, source);
        let info = crate::parse_frontmatter(&after).expect("frontmatter kept");
        assert!(
            after[info.end_byte..].contains(body_needle),
            "body must contain {body_needle:?} after the fence, {source:?} got {after:?}"
        );
    }

    /// Cmd-B/I/E/K at EOF on a closing `---` (no following newline) must open
    /// a body paragraph, like InsertText (`---x` is already covered).
    #[test]
    fn wrap_at_eof_on_closing_fence_does_not_glue() {
        let cases: [(RichCommand, &str); 4] = [
            (RichCommand::ToggleMark(MarkSet::BOLD), "**x**"),
            (RichCommand::ToggleMark(MarkSet::ITALIC), "*x*"),
            (RichCommand::ToggleMark(MarkSet::CODE), "`x`"),
            (RichCommand::ToggleLink, "[x]()"),
        ];
        for source in [
            "---\ntitle: Hello\n---",
            "---\n---",
            "---\ntitle: Hello\n...",
            "---\n...",
        ] {
            for (cmd, needle) in &cases {
                let (mut doc, mut engine, mut caret) = setup(source);
                caret.collapse_to(source.len());
                apply(&mut doc, &mut engine, &mut caret, cmd.clone());
                apply(
                    &mut doc,
                    &mut engine,
                    &mut caret,
                    RichCommand::InsertText("x".into()),
                );
                let after = doc.buffer.content();
                assert_not_glued_to_frontmatter_fence(&after, source);
                assert!(
                    after.contains(needle),
                    "empty wrap then type must be {needle} in the body, {source:?} cmd={cmd:?} got {after:?}"
                );
                let info = crate::parse_frontmatter(&after).expect("frontmatter kept");
                assert!(
                    after[info.end_byte..].contains('x'),
                    "wrapped text must land in the body, {source:?} got {after:?}"
                );
            }
        }
    }

    #[test]
    fn leftover_click_after_frontmatter_opens_body_paragraph() {
        for source in [
            "---\ntitle: Hello\n---",
            "---\n---",
            "---\ntitle: Hello\n---\n",
            "---\ntitle: Hello\n...",
            "---\n...",
            "---\ntitle: Hello\n...\n",
        ] {
            let typed = leftover_click_then_type(source);
            assert_not_glued_to_frontmatter_fence(&typed, source);
            assert!(
                typed.lines().any(|line| line.trim() == "x"),
                "leftover click + type must be a body paragraph, {source:?} got {typed:?}"
            );
            let info = crate::parse_frontmatter(&typed).expect("frontmatter kept");
            assert!(
                typed[info.end_byte..].contains('x') && !typed.contains("title: x"),
                "typed text must not land in YAML, {source:?} got {typed:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            place_caret_for_click_below(&mut doc, &mut engine, &mut caret);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let wrapped = doc.buffer.content();
            assert_not_glued_to_frontmatter_fence(&wrapped, source);
            assert!(
                wrapped.contains("**x**"),
                "leftover click + Cmd-B must wrap a body paragraph, {source:?} got {wrapped:?}"
            );
        }
    }

    #[test]
    fn mid_document_ellipsis_is_not_treated_as_frontmatter() {
        for source in ["# Hello\n...\nbody", "hello\n...\nworld"] {
            assert!(
                crate::parse_frontmatter(source).is_none(),
                "mid-document `...` must not be YAML, {source:?}"
            );
            let typed = leftover_click_then_type(source);
            assert!(
                crate::parse_frontmatter(&typed).is_none(),
                "leftover type must not invent frontmatter, {source:?} got {typed:?}"
            );
            assert!(
                typed.contains("..."),
                "body `...` must stay prose, {source:?} got {typed:?}"
            );
        }
    }

    /// SetHeading / paragraph / lists / quotes / paste / indent / input rules
    /// at EOF on a closing `---` / `...` must open a body line (same newline as
    /// InsertText / Cmd-B), not glue `---# Title` / `...x`.
    #[test]
    fn block_commands_at_eof_on_closing_fence_do_not_glue() {
        let sources = [
            "---\ntitle: Hello\n---",
            "---\n---",
            "---\ntitle: Hello\n...",
            "---\n...",
        ];
        for source in sources {
            at_eof_on_closing_fence(
                source,
                RichCommand::SetBlockType(BlockType::Heading(1)),
                &["Title"],
                "# Title",
            );
            at_eof_on_closing_fence(
                source,
                RichCommand::SetBlockType(BlockType::Heading(2)),
                &["Sub"],
                "## Sub",
            );
            at_eof_on_closing_fence(
                source,
                RichCommand::SetBlockType(BlockType::Paragraph),
                &["x"],
                "x",
            );
            at_eof_on_closing_fence(
                source,
                RichCommand::ToggleList { ordered: false },
                &["item"],
                "- item",
            );
            at_eof_on_closing_fence(
                source,
                RichCommand::ToggleList { ordered: true },
                &["item"],
                "1. item",
            );
            at_eof_on_closing_fence(source, RichCommand::ToggleBlockquote, &["q"], "> q");
        }
    }

    #[test]
    fn input_rules_and_paste_at_eof_on_closing_fence_do_not_glue() {
        let sources = [
            "---\ntitle: Hello\n---",
            "---\n---",
            "---\ntitle: Hello\n...",
            "---\n...",
        ];
        for source in sources {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(&mut doc, &mut engine, &mut caret, &["#", " ", "Title"]);
            assert_not_glued_to_frontmatter_fence(&after, source);
            assert_body_after_fence_newline(&after, source);
            let info = crate::parse_frontmatter(&after).expect("frontmatter kept");
            assert!(
                after[info.end_byte..].contains("# Title"),
                "typed `# ` must be an ATX heading in the body, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(&mut doc, &mut engine, &mut caret, &["-", " ", "item"]);
            assert_not_glued_to_frontmatter_fence(&after, source);
            assert_body_after_fence_newline(&after, source);
            let info = crate::parse_frontmatter(&after).expect("frontmatter kept");
            assert!(
                after[info.end_byte..].contains("- item"),
                "typed `- ` must be a list in the body, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(&mut doc, &mut engine, &mut caret, &[">", " ", "q"]);
            assert_not_glued_to_frontmatter_fence(&after, source);
            assert_body_after_fence_newline(&after, source);
            let info = crate::parse_frontmatter(&after).expect("frontmatter kept");
            assert!(
                after[info.end_byte..].contains("> q"),
                "typed `> ` must be a quote in the body, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(
                &mut doc,
                &mut engine,
                &mut caret,
                &["-", " ", "[", " ", "]", " ", "task"],
            );
            assert_not_glued_to_frontmatter_fence(&after, source);
            assert_body_after_fence_newline(&after, source);
            let info = crate::parse_frontmatter(&after).expect("frontmatter kept");
            assert!(
                after[info.end_byte..].contains("- [ ] task"),
                "typed task must land in the body, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            for _ in 0..3 {
                apply(
                    &mut doc,
                    &mut engine,
                    &mut caret,
                    RichCommand::InsertText("`".into()),
                );
            }
            let after = doc.buffer.content();
            assert_not_glued_to_frontmatter_fence(&after, source);
            assert_body_after_fence_newline(&after, source);
            let info = crate::parse_frontmatter(&after).expect("frontmatter kept");
            assert!(
                after[info.end_byte..].contains("```"),
                "fence input rule must open in the body, {source:?} got {after:?}"
            );

            for (paste, needle) in [
                ("# Title", "# Title"),
                ("- item", "- item"),
                ("- [ ] task", "- [ ] task"),
                ("> q", "> q"),
                ("| a | b |\n| --- | --- |\n| 1 | 2 |", "| a | b |"),
            ] {
                let (mut doc, mut engine, mut caret) = setup(source);
                caret.collapse_to(source.len());
                apply(
                    &mut doc,
                    &mut engine,
                    &mut caret,
                    RichCommand::InsertText(paste.into()),
                );
                let after = doc.buffer.content();
                assert_not_glued_to_frontmatter_fence(&after, source);
                assert_body_after_fence_newline(&after, source);
                let info = crate::parse_frontmatter(&after).expect("frontmatter kept");
                assert!(
                    after[info.end_byte..].contains(needle),
                    "paste {paste:?} must land in the body, {source:?} got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(&mut doc, &mut engine, &mut caret, RichCommand::IndentList);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_frontmatter_fence(&after, source);
            assert_not_glued_to_frontmatter_fence(&after, source);
            assert_body_after_fence_newline(&after, source);
            assert!(
                after.contains('x'),
                "indent at EOF must not glue, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let outcome =
                apply_rich_command(&mut doc, &mut engine, &mut caret, RichCommand::OutdentList)
                    .expect("outdent");
            assert_eq!(
                outcome,
                RichOutcome::Noop,
                "outdent on frontmatter-only must not rewrite YAML, {source:?} got {}",
                doc.buffer.content()
            );
            assert_eq!(doc.buffer.content(), source);

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertTableRow { after: true },
            );
            assert_eq!(
                doc.buffer.content(),
                source,
                "table-row insert without a table must no-op, {source:?}"
            );
        }
    }

    fn assert_not_glued_to_atomic_chrome(after: &str, source: &str) {
        assert_body_after_fence_newline(after, source);
        assert!(
            !after.contains("---x")
                && !after.contains("---#")
                && !after.contains("----")
                && !after.contains("<hr>x")
                && !after.contains("<hr>#")
                && !after.contains("```x"),
            "must not glue onto atomic chrome, {source:?} got {after:?}"
        );
    }

    /// Thematic `---`, HTML `<hr>`, and fence-close ticks at EOF share the
    /// same body-newline path as a closing frontmatter fence.
    #[test]
    fn commands_at_eof_on_atomic_chrome_do_not_glue() {
        for source in ["hello\n\n---", "<hr>", "```\ncode\n```"] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_atomic_chrome(&after, source);
            assert!(
                after.contains('x'),
                "InsertText must land after chrome, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::SetBlockType(BlockType::Heading(1)),
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("Title".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_atomic_chrome(&after, source);
            assert!(
                after.contains("# Title"),
                "SetHeading must open after chrome, {source:?} got {after:?}"
            );
            if source.contains("hello") {
                assert!(
                    after.contains("hello"),
                    "thematic break's previous paragraph must stay, got {after:?}"
                );
            }
            if source.starts_with("```") {
                assert!(
                    after.contains("```\ncode\n```"),
                    "fence body must stay, got {after:?}"
                );
            }
        }
    }

    fn has_setext_underline_line(source: &str) -> bool {
        source.lines().any(|line| {
            let t = after_quote(line).trim();
            t.len() >= 3 && (t.bytes().all(|b| b == b'=') || t.bytes().all(|b| b == b'-'))
        })
    }

    fn assert_not_glued_to_setext_underline(after: &str, source: &str) {
        assert_body_after_fence_newline(after, source);
        assert!(
            !after.contains("===x")
                && !after.contains("===#")
                && !after.contains("===*")
                && !after.contains("===`")
                && !after.contains("===[")
                && !after.contains("===>")
                && !after.contains("===-")
                && !after.contains("---x")
                && !after.contains("---#")
                && !after.contains("----")
                && !after.contains("---*")
                && !after.contains("---`")
                && !after.contains("---[")
                && !after.contains("--->")
                && !after.contains("[===]")
                && !after.contains("[---]"),
            "must not glue onto setext underline, {source:?} got {after:?}"
        );
        assert!(
            has_setext_underline_line(after),
            "setext underline must stay, {source:?} got {after:?}"
        );
        assert!(
            after.contains("Title") || after.contains("Sub"),
            "setext title must stay, {source:?} got {after:?}"
        );
    }

    fn at_eof_on_setext(source: &str, cmd: RichCommand, then_keys: &[&str], body_needle: &str) {
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        apply(&mut doc, &mut engine, &mut caret, cmd);
        let after = type_keys(&mut doc, &mut engine, &mut caret, then_keys);
        assert_not_glued_to_setext_underline(&after, source);
        assert!(
            after.contains(body_needle),
            "body must contain {body_needle:?} after the underline, {source:?} got {after:?}"
        );
    }

    /// InsertText / Cmd-B/I/E/K / SetHeading / lists / quotes / paste / `# `
    /// / `- ` / `> ` at EOF on a setext underline (no following newline)
    /// must open a body line (`===\nx`, not `===x` / `===# H`).
    #[test]
    fn wrap_at_eof_on_setext_underline_does_not_glue() {
        let cases: [(RichCommand, &str); 4] = [
            (RichCommand::ToggleMark(MarkSet::BOLD), "**x**"),
            (RichCommand::ToggleMark(MarkSet::ITALIC), "*x*"),
            (RichCommand::ToggleMark(MarkSet::CODE), "`x`"),
            (RichCommand::ToggleLink, "[x]()"),
        ];
        for source in ["Title\n===", "Title\n---", "Sub\n---", "> Title\n> ==="] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_setext_underline(&after, source);
            assert!(
                after.contains('x'),
                "InsertText must land after the underline, {source:?} got {after:?}"
            );

            for (cmd, needle) in &cases {
                let (mut doc, mut engine, mut caret) = setup(source);
                caret.collapse_to(source.len());
                apply(&mut doc, &mut engine, &mut caret, cmd.clone());
                apply(
                    &mut doc,
                    &mut engine,
                    &mut caret,
                    RichCommand::InsertText("x".into()),
                );
                let after = doc.buffer.content();
                assert_not_glued_to_setext_underline(&after, source);
                assert!(
                    after.contains(needle),
                    "empty wrap then type must be {needle} after the underline, {source:?} cmd={cmd:?} got {after:?}"
                );
            }
        }
    }

    #[test]
    fn block_commands_at_eof_on_setext_underline_do_not_glue() {
        let sources = ["Title\n===", "Title\n---", "Sub\n---", "> Title\n> ==="];
        for source in sources {
            at_eof_on_setext(
                source,
                RichCommand::SetBlockType(BlockType::Heading(1)),
                &["H"],
                "# H",
            );
            at_eof_on_setext(
                source,
                RichCommand::SetBlockType(BlockType::Heading(2)),
                &["Sub"],
                "## Sub",
            );
            at_eof_on_setext(
                source,
                RichCommand::SetBlockType(BlockType::Paragraph),
                &["x"],
                "x",
            );
            at_eof_on_setext(
                source,
                RichCommand::ToggleList { ordered: false },
                &["item"],
                "- item",
            );
            at_eof_on_setext(
                source,
                RichCommand::ToggleList { ordered: true },
                &["item"],
                "1. item",
            );
            at_eof_on_setext(source, RichCommand::ToggleBlockquote, &["q"], "> q");
        }
    }

    #[test]
    fn input_rules_and_paste_at_eof_on_setext_underline_do_not_glue() {
        let sources = ["Title\n===", "Title\n---", "> Title\n> ==="];
        for source in sources {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(&mut doc, &mut engine, &mut caret, &["#", " ", "H"]);
            assert_not_glued_to_setext_underline(&after, source);
            assert!(
                after.contains("# H"),
                "typed `# ` must be an ATX heading after the underline, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(&mut doc, &mut engine, &mut caret, &["-", " ", "item"]);
            assert_not_glued_to_setext_underline(&after, source);
            assert!(
                after.contains("- item"),
                "typed `- ` must be a list after the underline, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(&mut doc, &mut engine, &mut caret, &[">", " ", "q"]);
            assert_not_glued_to_setext_underline(&after, source);
            assert!(
                after.contains("> q"),
                "typed `> ` must be a quote after the underline, {source:?} got {after:?}"
            );

            for (paste, needle) in [("# Title", "# Title"), ("- item", "- item"), ("> q", "> q")] {
                let (mut doc, mut engine, mut caret) = setup(source);
                caret.collapse_to(source.len());
                apply(
                    &mut doc,
                    &mut engine,
                    &mut caret,
                    RichCommand::InsertText(paste.into()),
                );
                let after = doc.buffer.content();
                assert_not_glued_to_setext_underline(&after, source);
                assert!(
                    after.contains(needle),
                    "paste {paste:?} must land after the underline, {source:?} got {after:?}"
                );
            }
        }
    }

    fn assert_not_glued_to_html_close(after: &str, source: &str) {
        assert_body_after_fence_newline(after, source);
        assert!(
            !after.contains("</div>x")
                && !after.contains("</div>#")
                && !after.contains("</div>-")
                && !after.contains("</div>*")
                && !after.contains("</div>`")
                && !after.contains("</div>[")
                && !after.contains("</div>>")
                && !after.contains("</p>x")
                && !after.contains("</p>#")
                && !after.contains("</pre>x")
                && !after.contains("</pre>#")
                && !after.contains("</pre>-")
                && !after.contains("</pre>*"),
            "must not glue onto HTML close tag, {source:?} got {after:?}"
        );
        assert!(
            after.contains("</div>") || after.contains("</p>") || after.contains("</pre>"),
            "HTML close tag must stay, {source:?} got {after:?}"
        );
    }

    /// HTML-block `</div>` / `</p>` at EOF (no following newline) shares the
    /// same body-newline path as setext / fence-close.
    #[test]
    fn commands_at_eof_on_html_close_tag_do_not_glue() {
        for source in [
            "<div>\ninner\n</div>",
            "<p>\ninner\n</p>",
            "> <div>\n> inner\n> </div>",
            "<pre>**bold**</pre>",
            "<div>hello</div>",
            "<div></div>",
            "> <pre>**bold**</pre>",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_html_close(&after, source);
            assert!(
                after.contains('x') && after.lines().any(|line| line.trim() == "x"),
                "InsertText must land after the close tag, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::SetBlockType(BlockType::Heading(1)),
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("H".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_html_close(&after, source);
            assert!(
                after.contains("# H"),
                "SetHeading must open after the close tag, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleList { ordered: false },
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("item".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_html_close(&after, source);
            assert!(
                after.contains("- item"),
                "list must open after the close tag, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleBlockquote,
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("q".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_html_close(&after, source);
            assert!(
                after.contains("> q"),
                "quote must open after the close tag, {source:?} got {after:?}"
            );

            let cases: [(RichCommand, &str); 4] = [
                (RichCommand::ToggleMark(MarkSet::BOLD), "**x**"),
                (RichCommand::ToggleMark(MarkSet::ITALIC), "*x*"),
                (RichCommand::ToggleMark(MarkSet::CODE), "`x`"),
                (RichCommand::ToggleLink, "[x]()"),
            ];
            for (cmd, needle) in &cases {
                let (mut doc, mut engine, mut caret) = setup(source);
                caret.collapse_to(source.len());
                apply(&mut doc, &mut engine, &mut caret, cmd.clone());
                apply(
                    &mut doc,
                    &mut engine,
                    &mut caret,
                    RichCommand::InsertText("x".into()),
                );
                let after = doc.buffer.content();
                assert_not_glued_to_html_close(&after, source);
                assert!(
                    after.contains(needle),
                    "empty wrap then type must be {needle} after the close tag, {source:?} cmd={cmd:?} got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(&mut doc, &mut engine, &mut caret, &["#", " ", "H"]);
            assert_not_glued_to_html_close(&after, source);
            assert!(
                after.contains("# H"),
                "typed `# ` must be an ATX heading after the close tag, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(&mut doc, &mut engine, &mut caret, &["-", " ", "item"]);
            assert_not_glued_to_html_close(&after, source);
            assert!(
                after.contains("- item"),
                "typed `- ` must be a list after the close tag, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(&mut doc, &mut engine, &mut caret, &[">", " ", "q"]);
            assert_not_glued_to_html_close(&after, source);
            assert!(
                after.contains("> q"),
                "typed `> ` must be a quote after the close tag, {source:?} got {after:?}"
            );

            for (paste, needle) in [("# Title", "# Title"), ("- item", "- item"), ("> q", "> q")] {
                let (mut doc, mut engine, mut caret) = setup(source);
                caret.collapse_to(source.len());
                apply(
                    &mut doc,
                    &mut engine,
                    &mut caret,
                    RichCommand::InsertText(paste.into()),
                );
                let after = doc.buffer.content();
                assert_not_glued_to_html_close(&after, source);
                assert!(
                    after.contains(needle),
                    "paste {paste:?} must land after the close tag, {source:?} got {after:?}"
                );
            }
        }
    }

    fn assert_not_glued_to_closed_atx(after: &str, source: &str) {
        assert_body_after_fence_newline(after, source);
        assert!(
            !after.contains("#x")
                && !after.contains("#*")
                && !after.contains("#`")
                && !after.contains("#[")
                && !after.contains("#-")
                && !after.contains("#>")
                && after.contains("Title"),
            "must not glue onto closed ATX trailing hashes, {source:?} got {after:?}"
        );
        let orig_close = source.rsplit_once('\n').map(|(_, l)| l).unwrap_or(source);
        assert!(
            after.contains(orig_close)
                || after
                    .lines()
                    .any(|l| l.contains("Title") && l.contains('#')),
            "closed ATX heading must stay, {source:?} got {after:?}"
        );
    }

    /// Closed ATX trailing `#` at EOF shares the heading-chrome newline path
    /// with setext underlines (`# Title #x` / `# Title ## H` blocked).
    #[test]
    fn commands_at_eof_on_closed_atx_do_not_glue() {
        for source in ["# Title #", "## Title ##", "> # Title #"] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_closed_atx(&after, source);
            assert!(
                after.contains('x'),
                "InsertText must land after trailing hashes, {source:?} got {after:?}"
            );

            let cases: [(RichCommand, &str); 4] = [
                (RichCommand::ToggleMark(MarkSet::BOLD), "**x**"),
                (RichCommand::ToggleMark(MarkSet::ITALIC), "*x*"),
                (RichCommand::ToggleMark(MarkSet::CODE), "`x`"),
                (RichCommand::ToggleLink, "[x]()"),
            ];
            for (cmd, needle) in &cases {
                let (mut doc, mut engine, mut caret) = setup(source);
                caret.collapse_to(source.len());
                apply(&mut doc, &mut engine, &mut caret, cmd.clone());
                apply(
                    &mut doc,
                    &mut engine,
                    &mut caret,
                    RichCommand::InsertText("x".into()),
                );
                let after = doc.buffer.content();
                assert_not_glued_to_closed_atx(&after, source);
                assert!(
                    after.contains(needle),
                    "empty wrap then type must be {needle} after trailing hashes, {source:?} cmd={cmd:?} got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::SetBlockType(BlockType::Heading(1)),
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("H".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_closed_atx(&after, source);
            assert!(
                after.contains("# H"),
                "SetHeading must open after trailing hashes, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleList { ordered: false },
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("item".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_closed_atx(&after, source);
            assert!(
                after.contains("- item"),
                "list must open after trailing hashes, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleBlockquote,
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("q".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_closed_atx(&after, source);
            assert!(
                after.contains("> q"),
                "quote must open after trailing hashes, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(&mut doc, &mut engine, &mut caret, &["#", " ", "H"]);
            assert_not_glued_to_closed_atx(&after, source);
            assert!(
                after.contains("# H"),
                "typed `# ` must open after trailing hashes, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(&mut doc, &mut engine, &mut caret, &["-", " ", "item"]);
            assert_not_glued_to_closed_atx(&after, source);
            assert!(
                after.contains("- item"),
                "typed `- ` must open after trailing hashes, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(&mut doc, &mut engine, &mut caret, &[">", " ", "q"]);
            assert_not_glued_to_closed_atx(&after, source);
            assert!(
                after.contains("> q"),
                "typed `> ` must open after trailing hashes, {source:?} got {after:?}"
            );

            for (paste, needle) in [("# Next", "# Next"), ("- item", "- item"), ("> q", "> q")] {
                let (mut doc, mut engine, mut caret) = setup(source);
                caret.collapse_to(source.len());
                apply(
                    &mut doc,
                    &mut engine,
                    &mut caret,
                    RichCommand::InsertText(paste.into()),
                );
                let after = doc.buffer.content();
                assert_not_glued_to_closed_atx(&after, source);
                assert!(
                    after.contains(needle),
                    "paste {paste:?} must land after trailing hashes, {source:?} got {after:?}"
                );
            }
        }
    }

    #[test]
    fn insert_text_at_eof_on_open_atx_extends_the_title() {
        let source = "# Title";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let after = doc.buffer.content();
        assert_eq!(
            after, "# Titlex",
            "open ATX at EOF must extend the title, not open a paragraph, got {after:?}"
        );
    }

    fn eof_table_sources() -> &'static [&'static str] {
        &[
            "| a | b |\n|---|---|\n| 1 | 2 |",
            "| a | b |\n|---|---|",
            "| a |\n|---|\n| 1 |",
            "| a | b |\n|---|---|\n| 1 | 2 |  ",
            "> | a | b |\n> |---|---|\n> | 1 | 2 |",
            "---\ntitle: Hello\n---\n| a | b |\n|---|---|\n| 1 | 2 |",
        ]
    }

    fn assert_not_glued_to_table_pipe(after: &str, source: &str) {
        assert_body_after_fence_newline(after, source);
        assert!(
            !after.contains("|x")
                && !after.contains("|X")
                && !after.contains("|#")
                && !after.contains("|**")
                && !after.contains("|`")
                && !after.contains("|[")
                && !after.contains("2 |x")
                && !after.contains("| 2 |x")
                && !after.contains("|---|---|x")
                && !after.contains("| 1 |x")
                && !after.contains("x|")
                && !after.contains("|<br>")
                && !after.contains("| <br>"),
            "must not glue onto the table's last `|`, {source:?} got {after:?}"
        );
        assert!(
            after.contains('|'),
            "table pipes must stay, {source:?} got {after:?}"
        );
        if source.contains('>') {
            assert!(
                after
                    .lines()
                    .any(|line| line.trim_start().starts_with('>') && line.contains('|')),
                "quoted table must keep `>`, {source:?} got {after:?}"
            );
        }
        if source.starts_with("---") {
            let info = crate::parse_frontmatter(after).expect("frontmatter kept");
            assert_eq!(
                info.title.as_deref(),
                Some("Hello"),
                "YAML must not be nibbled, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("x---") && !after.contains("title: x"),
                "typed text must not land in YAML, {source:?} got {after:?}"
            );
        }
        let x_line = after.lines().find(|line| {
            let t = line.trim();
            t == "x"
                || t == "**x**"
                || t == "*x*"
                || t == "`x`"
                || t == "[x]()"
                || t == "# H"
                || t == "- item"
                || t == "> q"
                || t == "# Title"
        });
        if let Some(x_line) = x_line {
            assert!(
                !x_line.contains('|'),
                "new paragraph must not keep table pipes, {source:?} got {after:?}"
            );
        }
    }

    /// InsertText / Cmd-B/I/E/K / SetHeading / lists / quotes / paste / `# `
    /// / `- ` / `> ` at EOF on a table's last `|` (no following newline)
    /// must open a body line (`| 1 | 2 |\nx`, not `| 1 | 2 |x`).
    #[test]
    fn commands_at_eof_on_table_closing_pipe_do_not_glue() {
        let cases: [(RichCommand, &str); 4] = [
            (RichCommand::ToggleMark(MarkSet::BOLD), "**x**"),
            (RichCommand::ToggleMark(MarkSet::ITALIC), "*x*"),
            (RichCommand::ToggleMark(MarkSet::CODE), "`x`"),
            (RichCommand::ToggleLink, "[x]()"),
        ];
        for source in eof_table_sources() {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_table_pipe(&after, source);
            assert!(
                after.contains('x'),
                "InsertText must land after the last `|`, {source:?} got {after:?}"
            );
            engine.sync(&doc);
            assert!(
                engine
                    .tree()
                    .blocks
                    .iter()
                    .any(|b| matches!(b.kind, BlockKind::Table { .. }))
                    || engine.tree().blocks.iter().any(|b| {
                        b.children
                            .iter()
                            .any(|c| matches!(c.kind, BlockKind::Table { .. }))
                    }),
                "table must survive InsertText, {source:?} got {after:?}"
            );

            for (cmd, needle) in &cases {
                let (mut doc, mut engine, mut caret) = setup(source);
                caret.collapse_to(source.len());
                apply(&mut doc, &mut engine, &mut caret, cmd.clone());
                apply(
                    &mut doc,
                    &mut engine,
                    &mut caret,
                    RichCommand::InsertText("x".into()),
                );
                let after = doc.buffer.content();
                assert_not_glued_to_table_pipe(&after, source);
                assert!(
                    after.contains(needle),
                    "empty wrap then type must be {needle} after the last `|`, {source:?} cmd={cmd:?} got {after:?}"
                );
                assert!(
                    !after.contains("|**")
                        && !after.contains("**|")
                        && !after.contains("| **")
                        && !after.contains("** |")
                        && !after.contains("|*")
                        && !after.contains("*|"),
                    "wrap must not splice into a cell, {source:?} got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::SetBlockType(BlockType::Heading(1)),
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("H".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_table_pipe(&after, source);
            assert!(
                after.contains("# H"),
                "SetHeading must open after the last `|`, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleList { ordered: false },
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("item".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_table_pipe(&after, source);
            assert!(
                after.contains("- item"),
                "ToggleList must open after the last `|`, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleBlockquote,
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("q".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_table_pipe(&after, source);
            assert!(
                after.contains("> q"),
                "ToggleBlockquote must open after the last `|`, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert_not_glued_to_table_pipe(&after, source);
            assert!(
                after.ends_with('\n') && !after.contains("<br>"),
                "Enter at the last `|` must open a body line, not `<br>`, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertLineBreak,
            );
            assert_not_glued_to_table_pipe(&after, source);
            assert!(
                after.ends_with('\n') && !after.contains("<br>"),
                "Shift-Enter at the last `|` must open a body line, not `<br>`, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(&mut doc, &mut engine, &mut caret, &["#", " ", "Title"]);
            assert_not_glued_to_table_pipe(&after, source);
            assert!(
                after.contains("# Title"),
                "typed `# ` must be an ATX heading after the last `|`, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(&mut doc, &mut engine, &mut caret, &["-", " ", "item"]);
            assert_not_glued_to_table_pipe(&after, source);
            assert!(
                after.contains("- item"),
                "typed `- ` must be a list after the last `|`, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            let after = type_keys(&mut doc, &mut engine, &mut caret, &[">", " ", "q"]);
            assert_not_glued_to_table_pipe(&after, source);
            assert!(
                after.contains("> q"),
                "typed `> ` must be a quote after the last `|`, {source:?} got {after:?}"
            );

            for (paste, needle) in [("# Title", "# Title"), ("- item", "- item"), ("> q", "> q")] {
                let (mut doc, mut engine, mut caret) = setup(source);
                caret.collapse_to(source.len());
                apply(
                    &mut doc,
                    &mut engine,
                    &mut caret,
                    RichCommand::InsertText(paste.into()),
                );
                let after = doc.buffer.content();
                assert_not_glued_to_table_pipe(&after, source);
                assert!(
                    after.contains(needle),
                    "paste {paste:?} must land after the last `|`, {source:?} got {after:?}"
                );
            }
        }
    }

    #[test]
    fn table_cell_typing_and_last_cell_tab_still_work_at_eof() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |";
        let (mut doc, mut engine, mut caret) = setup(source);
        let two = source.rfind('2').expect("cell 2") + 1;
        caret.collapse_to(two);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("2x") && after.contains('|') && !after.contains("2 |\nx"),
            "cell-internal typing must stay in the cell, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Table { .. }),
            "table must survive cell typing, got {:?}",
            engine.tree().blocks[0].kind
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        let last_cell = engine.tree().blocks[0]
            .children
            .last()
            .and_then(|row| row.children.last())
            .expect("last cell")
            .source_range
            .start;
        caret.collapse_to(last_cell);
        let rows_before = engine.tree().blocks[0].children.len();
        apply(&mut doc, &mut engine, &mut caret, RichCommand::IndentList);
        engine.sync(&doc);
        assert!(
            engine.tree().blocks[0].children.len() > rows_before,
            "Tab on the last cell at EOF must insert a row, got {}",
            doc.buffer.content()
        );
        let after = doc.buffer.content();
        assert!(
            after.contains('|') && after.matches('|').count() > source.matches('|').count(),
            "inserted row must keep GFM pipes, got {after}"
        );
    }

    #[test]
    fn insert_text_at_eof_on_pipeless_table_and_inline_dest_still_extends() {
        fn type_eof(source: &str) -> String {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            )
        }

        // Pipeless last cell is body, not a closing pipe.
        let pipeless = type_eof("a | b\n---|---\n1 | 2");
        assert!(
            pipeless.contains("2x") && !pipeless.contains("2\nx"),
            "pipeless last cell must stay cell text, got {pipeless:?}"
        );

        // Inline dest close in a last-block paragraph continues that paragraph
        // (leftover-below click is the new-paragraph path, like `hello` → `hellox`).
        for source in [
            "![alt](https://e.com/i.png)",
            "[label](https://e.com)",
            "<https://example.com>",
            "[[page]]",
            "$x^2$",
        ] {
            let after = type_eof(source);
            assert!(
                after.starts_with(source)
                    && after.ends_with('x')
                    && !after.contains(&format!("{source}\nx")),
                "inline last-block EOF still extends the paragraph, {source:?} got {after:?}"
            );
        }
    }

    fn shift_tab_first_cell(
        source: &str,
        doc: &mut Document,
        engine: &mut RichEngine,
        caret: &mut CaretState,
    ) {
        engine.sync(doc);
        let cell = source.find('a').expect("first-cell a");
        caret.collapse_to(cell);
        assert!(
            engine.table_pos(caret.cursor()).is_some(),
            "precondition: caret in first cell"
        );
        apply(doc, engine, caret, RichCommand::TableTab { reverse: true });
    }

    fn table_gfm_intact(after: &str) {
        assert!(
            after.contains("| a | b |") && after.contains("| 1 | 2 |"),
            "table GFM must stay, got {after:?}"
        );
        let header = after
            .lines()
            .find(|line| line.contains("| a |"))
            .unwrap_or("");
        assert!(
            header.matches('|').count() >= 3,
            "header pipes must stay, got {after:?}"
        );
    }

    #[test]
    fn table_shift_tab_on_first_cell_exits_and_opens_a_blank() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        shift_tab_first_cell(source, &mut doc, &mut engine, &mut caret);
        let after = doc.buffer.content();
        table_gfm_intact(&after);
        engine.sync(&doc);
        assert!(
            engine.table_pos(caret.cursor()).is_none(),
            "Shift-Tab in the first cell must leave the table, got {}",
            caret.cursor()
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        table_gfm_intact(&typed);
        assert!(
            typed.contains('x') && !typed.contains("x|") && !typed.contains("| x |"),
            "typing after exit must be a paragraph above the table, got {typed:?}"
        );
        let info = crate::parse_frontmatter(&typed);
        assert!(
            info.is_none(),
            "opened blank must not become YAML, got {typed:?}"
        );
    }

    #[test]
    fn table_shift_tab_on_first_cell_exits_to_blank_before_table() {
        let source = "hello\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        shift_tab_first_cell(source, &mut doc, &mut engine, &mut caret);
        assert_eq!(
            doc.buffer.content(),
            source,
            "Shift-Tab must not rewrite a table that already has a blank above"
        );
        engine.sync(&doc);
        assert!(
            engine.table_pos(caret.cursor()).is_none(),
            "caret must leave the table onto the blank, got {}",
            caret.cursor()
        );
        let gap = blank_caret_gap_before(engine.tree(), 1).expect("separator gap");
        assert!(
            caret.cursor() >= gap.start && caret.cursor() <= gap.end,
            "caret must sit on the blank before the table, got {} gap {gap:?}",
            caret.cursor()
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        table_gfm_intact(&typed);
        let x_line = typed
            .lines()
            .find(|line| line.contains('x'))
            .expect("typed x");
        assert!(
            typed_line_is_paragraph(x_line) && !x_line.contains('|'),
            "typing on the exit blank must be a paragraph, got {typed:?}"
        );
        assert!(
            typed.contains("hello")
                && !typed
                    .lines()
                    .any(|l| l.contains("hello") && l.contains('x')),
            "must not splice into hello, got {typed:?}"
        );
    }

    #[test]
    fn table_shift_tab_on_first_cell_exits_to_previous_paragraph() {
        let source = "hello\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        shift_tab_first_cell(source, &mut doc, &mut engine, &mut caret);
        assert_eq!(
            doc.buffer.content(),
            source,
            "Shift-Tab must not rewrite the table when a previous paragraph exists"
        );
        engine.sync(&doc);
        assert!(
            engine.table_pos(caret.cursor()).is_none(),
            "caret must leave the table, got {}",
            caret.cursor()
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        table_gfm_intact(&typed);
        assert!(
            typed.contains("hellox") || typed.lines().any(|l| l.trim() == "hellox"),
            "typing must continue the previous paragraph, got {typed:?}"
        );
        assert!(
            !typed.contains("x|") && !typed.contains("| x |"),
            "must not type into the first cell, got {typed:?}"
        );
    }

    #[test]
    fn table_shift_tab_on_quoted_first_cell_opens_quoted_blank() {
        let source = "> | a | b |\n> |---|---|\n> | 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        shift_tab_first_cell(source, &mut doc, &mut engine, &mut caret);
        let after = doc.buffer.content();
        table_gfm_intact(&after);
        engine.sync(&doc);
        assert!(
            engine.table_pos(caret.cursor()).is_none(),
            "Shift-Tab must leave the quoted table, got {}",
            caret.cursor()
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        table_gfm_intact(&typed);
        assert!(
            typed.contains('x') && !typed.contains("x|") && !typed.contains("| x |"),
            "typing after quoted exit must not enter the cell, got {typed:?}"
        );
        assert!(
            typed.lines().any(|l| l.contains('x') && l.contains('>')),
            "opened line must stay quoted, got {typed:?}"
        );
    }

    #[test]
    fn table_shift_tab_on_quoted_table_after_paragraph_stays_in_quote() {
        let source = "> hello\n> | a | b |\n> |---|---|\n> | 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        shift_tab_first_cell(source, &mut doc, &mut engine, &mut caret);
        assert_eq!(
            doc.buffer.content(),
            source,
            "Shift-Tab must not rewrite a quoted table that has a previous line"
        );
        engine.sync(&doc);
        assert!(
            engine.table_pos(caret.cursor()).is_none(),
            "caret must leave the table, got {}",
            caret.cursor()
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        table_gfm_intact(&typed);
        assert!(
            typed.contains("hellox") || typed.contains("> hellox"),
            "typing must continue the quoted paragraph, got {typed:?}"
        );
        assert!(
            !typed.contains("x|") && !typed.contains("| x |"),
            "must not type into the first cell, got {typed:?}"
        );
    }

    #[test]
    fn table_shift_tab_on_first_cell_after_frontmatter_does_not_nibble_yaml() {
        let source = "---\ntitle: Hello\n---\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        shift_tab_first_cell(source, &mut doc, &mut engine, &mut caret);
        let after = doc.buffer.content();
        table_gfm_intact(&after);
        let info = crate::parse_frontmatter(&after).expect("frontmatter kept");
        assert_eq!(
            info.title.as_deref(),
            Some("Hello"),
            "YAML title must stay, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            engine.table_pos(caret.cursor()).is_none(),
            "caret must leave the table, got {}",
            caret.cursor()
        );
        let fm_end = super::frontmatter_body_start(engine.tree());
        assert!(
            caret.cursor() >= fm_end,
            "exit caret must stay in the body, got {} fm_end {fm_end}",
            caret.cursor()
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        let info = crate::parse_frontmatter(&typed).expect("frontmatter after type");
        assert_eq!(info.title.as_deref(), Some("Hello"));
        assert!(
            !typed.contains("x---") && typed[info.end_byte..].contains('x'),
            "typed text must land in the body, got {typed:?}"
        );
        table_gfm_intact(&typed);
        assert!(
            !typed.contains("x|") && !typed.contains("| x |"),
            "must not type into the first cell, got {typed:?}"
        );
    }

    #[test]
    fn table_outdent_on_first_cell_exits_like_shift_tab() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let cell = source.find('a').expect("a");
        caret.collapse_to(cell);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::OutdentList);
        engine.sync(&doc);
        assert!(
            engine.table_pos(caret.cursor()).is_none(),
            "OutdentList in the first cell must Shift-Tab out, got {}",
            caret.cursor()
        );
        table_gfm_intact(&doc.buffer.content());
    }

    #[test]
    fn delete_cross_cell_selection_does_not_remove_pipes() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let a = source.find('a').expect("header a");
        let after_b = source.find('b').expect("header b") + 1;
        caret.range = a..after_b;
        caret.reversed = false;
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Table { .. }),
            "cross-cell Backspace must keep a table, got {after:?}"
        );
        assert_eq!(
            engine.tree().blocks[0].children[0].children.len(),
            2,
            "deleting a selection across `|` must not merge header cells, got {after:?}"
        );
        assert!(
            after.contains('|'),
            "column pipes must survive, got {after:?}"
        );
        let header = after.lines().next().unwrap_or("");
        assert!(
            header.matches('|').count() >= 3,
            "header must keep GFM pipes, got {after:?}"
        );
        assert!(
            after.contains('b'),
            "clamp to the start cell must not delete the other cell, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.range = a..after_b;
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        engine.sync(&doc);
        assert_eq!(
            engine.tree().blocks[0].children[0].children.len(),
            2,
            "cross-cell Delete must not merge header cells, got {after:?}"
        );
    }

    fn table_source() -> &'static str {
        "| a | b |\n|---|---|\n| 1 | 2 |\n"
    }

    fn assert_gfm_table_survives(engine: &RichEngine, after: &str, label: &str) {
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Table { .. }),
            "{label}: table must survive, got {after:?}"
        );
        assert_eq!(
            engine.tree().blocks[0].children[0].children.len(),
            2,
            "{label}: header must keep two cells, got {after:?}"
        );
        let header = after.lines().next().unwrap_or("");
        assert!(
            header.matches('|').count() >= 3,
            "{label}: header must keep GFM pipes, got {after:?}"
        );
        assert!(
            after.contains('b'),
            "{label}: the other cell must remain, got {after:?}"
        );
        assert!(
            !after.contains("**|")
                && !after.contains("|**")
                && !after.contains("*|")
                && !after.contains("|*")
                && !after.contains("`|")
                && !after.contains("|`")
                && !after.contains("[|"),
            "{label}: wrap must not splice delimiters onto `|`, got {after:?}"
        );
    }

    #[test]
    fn toggle_mark_full_doc_selection_does_not_wrap_table_pipes() {
        let source = table_source();
        let a = source.find('a').expect("header a");
        for (cmd, label) in [
            (RichCommand::ToggleMark(MarkSet::BOLD), "bold"),
            (RichCommand::ToggleMark(MarkSet::ITALIC), "italic"),
            (RichCommand::ToggleMark(MarkSet::CODE), "code"),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(a);
            caret.range = 0..source.len();
            caret.reversed = false;
            apply(&mut doc, &mut engine, &mut caret, cmd.clone());
            let after = doc.buffer.content();
            engine.sync(&doc);
            assert_gfm_table_survives(&engine, &after, label);
        }
    }

    #[test]
    fn toggle_link_full_doc_selection_does_not_wrap_table_pipes() {
        let source = table_source();
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('a').expect("header a"));
        caret.range = 0..source.len();
        caret.reversed = false;
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let after = doc.buffer.content();
        engine.sync(&doc);
        assert_gfm_table_survives(&engine, &after, "link");
        assert!(
            after.contains("[a]") || after.contains("[ a ]") || after.contains("[ a]"),
            "Cmd-K must wrap the cell text, not the row, got {after:?}"
        );
    }

    #[test]
    fn toggle_bold_in_cell_still_wraps_that_cell() {
        let source = table_source();
        let (mut doc, mut engine, mut caret) = setup(source);
        let a = source.find('a').expect("header a");
        caret.range = a..a + 1;
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        let after = doc.buffer.content();
        engine.sync(&doc);
        assert_gfm_table_survives(&engine, &after, "in-cell bold");
        assert!(
            after.contains("**a**") || after.contains("__a__"),
            "in-cell Cmd-B must still wrap the cell text, got {after:?}"
        );
    }

    #[test]
    fn toggle_mark_empty_caret_in_table_cell_inserts_pair() {
        let source = table_source();
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('a').expect("header a"));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        let after = doc.buffer.content();
        engine.sync(&doc);
        assert_gfm_table_survives(&engine, &after, "empty Cmd-B in cell");
        assert!(
            after.contains("****") || after.contains("**a**"),
            "empty Cmd-B in a cell must insert a wrap pair, got {after:?}"
        );
    }

    #[test]
    fn toggle_bold_full_selection_from_paragraph_does_not_clamp_into_table() {
        let source = "hello\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.range = 0..source.len();
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        let after = doc.buffer.content();
        engine.sync(&doc);
        assert!(
            after.contains("**hello**") || after.contains("__hello__"),
            "Cmd-A from a paragraph must still wrap that paragraph, got {after:?}"
        );
        let table = engine
            .tree()
            .blocks
            .iter()
            .find(|b| matches!(b.kind, BlockKind::Table { .. }))
            .expect("table must survive");
        assert_eq!(
            table.children[0].children.len(),
            2,
            "wrapping the paragraph must not collapse the table, got {after:?}"
        );
    }

    #[test]
    fn table_select_all_selects_cell_then_document() {
        let source = table_source();
        let (_doc, engine, _) = setup(source);
        let a = source.find('a').expect("header a");
        let cell = engine.cell_edit_range(a, source).expect("cell a");
        let first = table_select_all_range(&engine, source, &(a..a), None).expect("first Cmd-A");
        assert_eq!(
            first, cell,
            "first SelectAll must be the cell, got {first:?}"
        );
        assert!(
            !source[first.clone()].contains('|'),
            "cell SelectAll must not include `|`, got {:?}",
            &source[first.clone()]
        );
        assert!(
            table_select_all_range(&engine, source, &cell, None).is_none(),
            "second SelectAll (already the cell) must fall through to the document"
        );
        assert!(
            table_select_all_range(&engine, source, &(0..source.len()), None).is_none(),
            "SelectAll must not shrink a whole-document selection back to a cell"
        );

        let mixed = "hello\n\n| a | b |\n|---|---|\n";
        let (_doc, engine, _) = setup(mixed);
        assert!(
            table_select_all_range(&engine, mixed, &(0..0), None).is_none(),
            "SelectAll in a paragraph must still take the document"
        );
    }

    fn first_empty_cell_body(engine: &RichEngine, source: &str) -> Range<usize> {
        fn walk(blocks: &[Block], engine: &RichEngine, source: &str) -> Option<Range<usize>> {
            for b in blocks {
                if matches!(b.kind, BlockKind::TableCell) {
                    for probe in b.source_range.start..=b.source_range.end.min(source.len()) {
                        if let Some(cell) = engine.cell_edit_range(probe, source) {
                            if cell.is_empty() {
                                return Some(cell);
                            }
                        }
                    }
                }
                if let Some(found) = walk(&b.children, engine, source) {
                    return Some(found);
                }
            }
            None
        }
        walk(&engine.tree().blocks, engine, source).expect("empty table cell")
    }

    #[test]
    fn table_select_all_empty_cell_first_stays_in_cell() {
        let source = "|| b |\n|---|---|\n| 1 | 2 |\n";
        let (_doc, engine, _) = setup(source);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Table { .. }),
            "fixture must parse as a table, got {:?}",
            engine.tree().blocks[0].kind
        );
        let cell = first_empty_cell_body(&engine, source);
        assert!(
            cell.is_empty(),
            "empty header cell body must be collapsed, got {cell:?}"
        );
        let caret = cell.clone();
        assert_eq!(
            caret, cell,
            "the bug: empty cell range equals the collapsed caret"
        );
        let first = table_select_all_range(&engine, source, &caret, None)
            .expect("first Cmd-A on an empty cell must still select the cell");
        assert_eq!(first, cell, "first SelectAll must be the empty cell body");
        assert!(
            table_select_all_range(&engine, source, &first, Some(&first)).is_none(),
            "second SelectAll (empty cell already latched) must fall through to the document"
        );
        assert!(
            table_select_all_range(&engine, source, &(0..source.len()), Some(&first)).is_none(),
            "SelectAll must not shrink a whole-document selection back to a cell"
        );
    }

    #[test]
    fn block_commands_in_table_do_not_rewrite_gfm_structure() {
        let source = table_source();
        let a = source.find('a').expect("header a");
        let cmds: [(RichCommand, &str); 5] = [
            (RichCommand::ToggleList { ordered: false }, "ToggleList"),
            (
                RichCommand::ToggleList { ordered: true },
                "ToggleList ordered",
            ),
            (RichCommand::ToggleBlockquote, "ToggleBlockquote"),
            (
                RichCommand::SetBlockType(BlockType::Heading(1)),
                "SetBlockType heading",
            ),
            (
                RichCommand::SetBlockType(BlockType::Paragraph),
                "SetBlockType paragraph",
            ),
        ];
        for (cmd, label) in cmds {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(a);
            let outcome =
                apply_rich_command(&mut doc, &mut engine, &mut caret, cmd.clone()).expect(label);
            assert_eq!(
                outcome,
                RichOutcome::Noop,
                "{label}: in-cell block command must no-op, got {}",
                doc.buffer.content()
            );
            let after = doc.buffer.content();
            engine.sync(&doc);
            assert_gfm_table_survives(&engine, &after, label);
            assert_eq!(after, source, "{label}: source bytes must stay the table");

            // Selection start in the table (cursor may sit past `|`).
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.range = a..source.len();
            caret.reversed = false;
            apply_rich_command(&mut doc, &mut engine, &mut caret, cmd.clone()).expect(label);
            let after = doc.buffer.content();
            engine.sync(&doc);
            assert_gfm_table_survives(&engine, &after, &format!("{label} selection start"));
        }
    }

    #[test]
    fn block_commands_on_quoted_table_do_not_eat_pipes() {
        let source = "> | a | b |\n> |---|---|\n> | 1 | 2 |\n";
        let a = source.find('a').expect("header a");
        for cmd in [
            RichCommand::ToggleList { ordered: false },
            RichCommand::ToggleBlockquote,
            RichCommand::SetBlockType(BlockType::Heading(1)),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(a);
            apply_rich_command(&mut doc, &mut engine, &mut caret, cmd).unwrap();
            let after = doc.buffer.content();
            engine.sync(&doc);
            assert!(
                after.contains('|'),
                "quoted table must keep GFM pipes, got {after:?}"
            );
            let table = engine
                .tree()
                .blocks
                .iter()
                .find(|b| matches!(b.kind, BlockKind::Table { .. }))
                .or_else(|| {
                    engine.tree().blocks.iter().find_map(|b| {
                        b.children
                            .iter()
                            .find(|c| matches!(c.kind, BlockKind::Table { .. }))
                    })
                })
                .expect("table must survive");
            assert_eq!(
                table.children[0].children.len(),
                2,
                "quoted table must keep two header cells, got {after:?}"
            );
        }
    }

    #[test]
    fn block_commands_on_paragraph_next_to_table_still_apply() {
        let source = "hello\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(0);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetBlockType(BlockType::Heading(1)),
        );
        let after = doc.buffer.content();
        engine.sync(&doc);
        assert!(
            after.starts_with("# hello"),
            "heading on the paragraph must still apply, got {after:?}"
        );
        let table = engine
            .tree()
            .blocks
            .iter()
            .find(|b| matches!(b.kind, BlockKind::Table { .. }))
            .expect("table must survive");
        assert_eq!(
            table.children[0].children.len(),
            2,
            "heading the paragraph must not collapse the table, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(0);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleList { ordered: false },
        );
        let after = doc.buffer.content();
        engine.sync(&doc);
        assert!(
            after.contains("- hello") || after.starts_with("- "),
            "ToggleList on the paragraph must still wrap it, got {after:?}"
        );
        let table = engine
            .tree()
            .blocks
            .iter()
            .find(|b| matches!(b.kind, BlockKind::Table { .. }))
            .expect("table must survive list wrap");
        assert_eq!(
            table.children[0].children.len(),
            2,
            "list wrap must not eat table pipes, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(0);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleBlockquote,
        );
        let after = doc.buffer.content();
        engine.sync(&doc);
        assert!(
            after.contains("> hello") || after.starts_with("> "),
            "ToggleBlockquote on the paragraph must still wrap it, got {after:?}"
        );
        let table = engine
            .tree()
            .blocks
            .iter()
            .find(|b| matches!(b.kind, BlockKind::Table { .. }))
            .expect("table must survive quote wrap");
        assert_eq!(
            table.children[0].children.len(),
            2,
            "quote wrap must not eat table pipes, got {after:?}"
        );
    }

    #[test]
    fn insert_text_inside_autolink_keeps_the_url() {
        let source = "<https://example.com>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let at = source.find("example").expect("host");
        caret.collapse_to(at);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("<https://") && after.contains("example.com>"),
            "typing inside an autolink must not drop the URL, got {after:?}"
        );
    }

    #[test]
    fn editing_reference_definition_dest_updates_resolution() {
        let source = "[hello][ref]\n\n[ref]: https://e.com\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let dest_end = source.find("https://e.com").expect("dest") + "https://e.com".len();
        caret.collapse_to(dest_end);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("/x".into()),
        );
        assert!(
            after.contains("[ref]: https://e.com/x"),
            "dest edit must stay in the definition, got {after:?}"
        );
        assert!(
            after.contains("[hello][ref]"),
            "must not drop the reference link, got {after:?}"
        );
        let url = engine.tree().blocks.iter().find_map(|b| {
            b.inlines.iter().find_map(|i| match i {
                Inline::Run {
                    text,
                    link: Some(link),
                    ..
                } if text == "hello" => Some(link.url.clone()),
                _ => None,
            })
        });
        assert_eq!(
            url.as_deref(),
            Some("https://e.com/x"),
            "resolved dest must follow the definition edit"
        );
        assert!(
            engine
                .tree()
                .blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::LinkReferenceDefinition { .. })),
            "definition must remain a WYSIWYG block after dest edit"
        );
    }

    #[test]
    fn input_rule_hash_space_becomes_heading_one_undo() {
        let (mut doc, mut engine, mut caret) = setup("hello");
        caret.collapse_to(0);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("#".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText(" ".into()),
        );
        let after = doc.buffer.content();
        assert!(
            after.starts_with("# hello")
                || after.starts_with("#  hello")
                || after.starts_with("#hello"),
            "{after:?}"
        );
        assert!(after.contains("hello"), "{after:?}");
        engine.sync(&doc);
        assert!(
            matches!(
                engine.tree().blocks[0].kind,
                BlockKind::Heading { level: 1, .. }
            ),
            "{:?}",
            engine.tree().blocks[0].kind
        );
        assert_eq!(
            doc.undo_stack().undo_depth(),
            1,
            "hash+space is one undo group"
        );
        doc.undo();
        assert_eq!(doc.buffer.content(), "hello");
    }

    #[test]
    fn input_rule_list_quote_ordered() {
        let (mut doc, mut engine, mut caret) = setup("");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("-".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText(" ".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("a".into()),
        );
        assert!(
            doc.buffer.content().starts_with("- a"),
            "{}",
            doc.buffer.content()
        );

        let (mut doc, mut engine, mut caret) = setup("");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText(">".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText(" ".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("q".into()),
        );
        assert!(
            doc.buffer.content().starts_with("> q"),
            "{}",
            doc.buffer.content()
        );

        let (mut doc, mut engine, mut caret) = setup("");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("1".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText(".".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText(" ".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert!(
            doc.buffer.content().starts_with("1. x"),
            "{}",
            doc.buffer.content()
        );
    }

    #[test]
    fn input_rule_fence_and_thematic_break() {
        let (mut doc, mut engine, mut caret) = setup("");
        for _ in 0..3 {
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("`".into()),
            );
        }
        let after = doc.buffer.content();
        assert!(after.starts_with("```"), "{after:?}");
        assert!(after.contains("```\n"), "{after:?}");

        let (mut doc, mut engine, mut caret) = setup("");
        for _ in 0..3 {
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("-".into()),
            );
        }
        let after = doc.buffer.content();
        assert!(after.starts_with("---"), "{after:?}");
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::ThematicBreak),
            "{:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn input_rule_auto_close_italic() {
        let (mut doc, mut engine, mut caret) = setup("");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("*".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("hi".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("*".into()),
        );
        let after = doc.buffer.content();
        assert!(after.contains("*hi*"), "{after:?}");
        engine.sync(&doc);
        let italic = engine.tree().blocks[0].inlines.iter().any(|i| match i {
            Inline::Run { marks, .. } => marks.contains(MarkSet::ITALIC),
            _ => false,
        });
        assert!(italic, "expected italic run in {after:?}");
    }

    #[test]
    fn input_rules_disabled_in_code_block() {
        let (mut doc, mut engine, mut caret) = setup("```\n# not heading\n```\n");
        engine.sync(&doc);
        let body = doc.buffer.content().find("# not").unwrap();
        caret.collapse_to(body);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("-".into()),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("# not") || after.contains("-# not") || after.contains("- not"),
            "{after:?}"
        );
        assert!(after.contains("```"), "fence kept: {after:?}");
    }

    #[test]
    fn set_code_info_rewrites_fence_language() {
        let (mut doc, mut engine, mut caret) = setup("```\nfn main() {}\n```\n");
        engine.sync(&doc);
        let id = engine.tree().blocks[0].id;
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetCodeInfo {
                id,
                info: "rust".into(),
            },
        );
        let after = doc.buffer.content();
        assert!(after.contains("```rust"), "{after:?}");
        assert!(after.contains("fn main()"), "{after:?}");
    }

    #[test]
    fn set_image_alt_rewrites_alt_text() {
        let (mut doc, mut engine, mut caret) = setup("![old](pic.png)\n");
        engine.sync(&doc);
        let range = match &engine.tree().blocks[0].inlines[0] {
            Inline::Image { source_range, .. } => source_range.clone(),
            other => panic!("{other:?}"),
        };
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetImageAlt {
                source_range: range,
                alt: "cat".into(),
            },
        );
        assert!(
            doc.buffer.content().contains("![cat](pic.png)"),
            "{}",
            doc.buffer.content()
        );
    }

    #[test]
    fn set_image_alt_keeps_enclosing_link() {
        let source = "[![old](pic.png)](https://e.com)\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        engine.sync(&doc);
        let range = engine.tree().blocks[0]
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::Image {
                    source_range,
                    link: Some(_),
                    ..
                } => Some(source_range.clone()),
                _ => None,
            })
            .expect("linked image");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetImageAlt {
                source_range: range,
                alt: "cat".into(),
            },
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("![cat](pic.png)"),
            "alt must change, got {after:?}"
        );
        assert!(
            after.contains("https://e.com") && after.contains("[![cat]"),
            "wrapping link must survive alt edit, got {after:?}"
        );
    }

    #[test]
    fn set_frontmatter_inserts_and_replaces() {
        let (mut doc, mut engine, mut caret) = setup("# Body\n");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetFrontmatter {
                raw: "title: Hello".into(),
            },
        );
        let after = doc.buffer.content();
        assert!(after.starts_with("---\n"), "{after:?}");
        assert!(after.contains("title: Hello"), "{after:?}");
        assert!(after.contains("# Body"), "{after:?}");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetFrontmatter {
                raw: "title: World".into(),
            },
        );
        let after = doc.buffer.content();
        assert!(after.contains("title: World"), "{after:?}");
        assert!(!after.contains("title: Hello"), "{after:?}");
    }

    #[test]
    fn table_tab_and_insert_row_col() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        engine.sync(&doc);
        let cell_a = engine.tree().blocks[0].children[0].children[0]
            .source_range
            .start;
        caret.collapse_to(cell_a);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::TableTab { reverse: false },
        );
        let pos = engine.table_pos(caret.cursor()).expect("still in table");
        assert_eq!(pos.col, 1);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertTableColumn { after: true },
        );
        engine.sync(&doc);
        let table = &engine.tree().blocks[0];
        assert_eq!(
            table.children[0].children.len(),
            3,
            "{}",
            doc.buffer.content()
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertTableRow { after: true },
        );
        engine.sync(&doc);
        assert_eq!(
            engine.tree().blocks[0].children.len(),
            3,
            "{}",
            doc.buffer.content()
        );
        let after_insert = caret.cursor();
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteTableRow,
        );
        engine.sync(&doc);
        assert_eq!(
            engine.tree().blocks[0].children.len(),
            2,
            "{}",
            doc.buffer.content()
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteTableColumn,
        );
        engine.sync(&doc);
        assert_eq!(
            engine.tree().blocks[0].children[0].children.len(),
            2,
            "{}",
            doc.buffer.content()
        );
        let _ = after_insert;
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteTableColumn,
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteTableColumn,
        );
        engine.sync(&doc);
        assert_eq!(
            engine.tree().blocks[0].children[0].children.len(),
            1,
            "last column is kept: {}",
            doc.buffer.content()
        );
    }

    #[test]
    fn delete_last_row_is_noop() {
        let source = "| a |\n|---|\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        engine.sync(&doc);
        caret.collapse_to(
            engine.tree().blocks[0].children[0].children[0]
                .source_range
                .start,
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteTableRow,
        );
        engine.sync(&doc);
        assert_eq!(engine.tree().blocks[0].children.len(), 1);
        assert!(matches!(
            engine.tree().blocks[0].children[0].kind,
            BlockKind::TableRow { header: true }
        ));
    }

    #[test]
    fn insert_row_keeps_first_row_as_header() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        engine.sync(&doc);
        caret.collapse_to(
            engine.tree().blocks[0].children[0].children[0]
                .source_range
                .start,
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertTableRow { after: false },
        );
        engine.sync(&doc);
        let rows = &engine.tree().blocks[0].children;
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0].kind, BlockKind::TableRow { header: true }));
        assert!(matches!(
            rows[1].kind,
            BlockKind::TableRow { header: false }
        ));
        assert!(engine.table_pos(caret.cursor()).is_some());
    }

    #[test]
    fn underscore_italic_via_input_rule() {
        let (mut doc, mut engine, mut caret) = setup("");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("_".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("hi".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("_".into()),
        );
        let after = doc.buffer.content();
        assert!(after.contains("_hi_"), "{after:?}");
        engine.sync(&doc);
        let italic = engine.tree().blocks[0].inlines.iter().any(|i| match i {
            Inline::Run { marks, .. } => marks.contains(MarkSet::ITALIC),
            _ => false,
        });
        assert!(italic, "expected italic run in {after:?}");
    }

    #[test]
    fn heading_inside_list_item_and_one_undo() {
        let (mut doc, mut engine, mut caret) = setup("");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("-".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText(" ".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("#".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText(" ".into()),
        );
        let after = doc.buffer.content();
        assert!(after.contains("- #"), "{after:?}");
        let tx = doc.undo_tx().unwrap();
        engine.sync(&doc);
        caret.restore(tx.selection_after);
        let undone = doc.buffer.content();
        assert!(
            undone.contains("- ") && !undone.contains("- # "),
            "heading conversion is one undo group: {undone:?}"
        );
    }

    #[test]
    fn heading_undo_peels_prefix_out_of_longer_typing() {
        let (mut doc, mut engine, mut caret) = setup("");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("#".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        caret.collapse_to(1);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText(" ".into()),
        );
        let after = doc.buffer.content();
        assert!(after.starts_with("# z"), "{after:?}");
        let tx = doc.undo_tx().unwrap();
        engine.sync(&doc);
        caret.restore(tx.selection_after);
        assert_eq!(doc.buffer.content(), "z");
    }

    #[test]
    fn set_frontmatter_field_upserts_title() {
        let (mut doc, mut engine, mut caret) = setup("# Body\n");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetFrontmatterField {
                key: "title".into(),
                value: "Hello".into(),
            },
        );
        let after = doc.buffer.content();
        assert!(after.contains("title: \"Hello\""), "{after:?}");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetFrontmatterField {
                key: "tags".into(),
                value: "[a, b]".into(),
            },
        );
        let after = doc.buffer.content();
        assert!(after.contains("tags: [\"a\", \"b\"]"), "{after:?}");
        assert!(after.contains("title: \"Hello\""), "{after:?}");
    }

    #[test]
    fn set_frontmatter_field_description_and_yaml_body() {
        let (mut doc, mut engine, mut caret) = setup("# Body\n");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetFrontmatterField {
                key: "description".into(),
                value: "A note".into(),
            },
        );
        let after = doc.buffer.content();
        assert!(after.contains("description: \"A note\""), "{after:?}");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetFrontmatter {
                raw: "title: T\ndescription: A note\nauthor: me".into(),
            },
        );
        let after = doc.buffer.content();
        assert!(after.contains("author: me"), "{after:?}");
        assert!(after.contains("title: T"), "{after:?}");
        assert!(after.contains("# Body"), "{after:?}");
    }

    #[test]
    fn invalid_frontmatter_commands_leave_the_document_unchanged() {
        let (mut doc, mut engine, mut caret) = setup("---\ntitle: Old\n---\n\n# Body\n");
        let before = doc.buffer.content();
        let error = apply_rich_command(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetFrontmatter {
                raw: "title: [unterminated".into(),
            },
        )
        .unwrap_err();
        assert_eq!(
            error,
            RichError::InvalidFrontmatter(crate::FrontmatterError::InvalidYaml)
        );
        assert_eq!(doc.buffer.content(), before);

        let (mut invalid_doc, mut invalid_engine, mut invalid_caret) =
            setup("---\ntitle: [unterminated\n---\n\n# Body\n");
        let invalid_before = invalid_doc.buffer.content();
        let error = apply_rich_command(
            &mut invalid_doc,
            &mut invalid_engine,
            &mut invalid_caret,
            RichCommand::SetFrontmatterField {
                key: "title".into(),
                value: "Safe".into(),
            },
        )
        .unwrap_err();
        assert_eq!(
            error,
            RichError::InvalidFrontmatter(crate::FrontmatterError::InvalidYaml)
        );
        assert_eq!(invalid_doc.buffer.content(), invalid_before);
    }

    #[test]
    fn frontmatter_field_command_quotes_pasted_yaml_syntax() {
        let (mut doc, mut engine, mut caret) = setup("# Body\n");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::SetFrontmatterField {
                key: "description".into(),
                value: "A: note\nowner: Mallory".into(),
            },
        );
        let after = doc.buffer.content();
        assert!(
            after.contains(r#"description: "A: note\nowner: Mallory""#),
            "{after:?}"
        );
        assert!(!after.contains("\nowner: Mallory\n"), "{after:?}");
        let info = crate::parse_frontmatter(&after).unwrap();
        crate::validate_frontmatter_yaml(&info.yaml_body).unwrap();
    }

    #[test]
    fn typing_brackets_builds_a_task_list_and_a_link() {
        let (mut doc, mut engine, mut caret) = setup("");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("-".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText(" ".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("[".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText(" ".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("]".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText(" ".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("todo".into()),
        );
        let after = doc.buffer.content();
        assert!(
            after.starts_with("- [ ] todo"),
            "task list must not escape brackets: {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(
                engine.tree().blocks[0].children[0].kind,
                BlockKind::ListItem { task: Some(false) }
            ),
            "{:?}",
            engine.tree().blocks[0].children[0].kind
        );

        let (mut doc, mut engine, mut caret) = setup("");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("[".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("hi".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("]".into()),
        );
        let after = doc.buffer.content();
        assert_eq!(after, "[hi]");
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("<".into()),
        );
        assert!(
            doc.buffer.content().ends_with("<"),
            "{}",
            doc.buffer.content()
        );
        assert!(
            !doc.buffer.content().contains("\\["),
            "{}",
            doc.buffer.content()
        );
    }

    #[test]
    fn table_cell_dashes_do_not_become_a_thematic_break() {
        let source = "|  | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        engine.sync(&doc);
        let cell_start = engine.tree().blocks[0].children[0].children[0]
            .source_range
            .start;
        caret.collapse_to(cell_start);
        for _ in 0..3 {
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("-".into()),
            );
        }
        let after = doc.buffer.content();
        assert!(
            !after.contains("---\n\n"),
            "thematic break must not split a table: {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Table { .. }),
            "{:?}",
            engine.tree().blocks[0].kind
        );
    }

    fn type_chars(doc: &mut Document, engine: &mut RichEngine, caret: &mut CaretState, text: &str) {
        for ch in text.chars() {
            apply(doc, engine, caret, RichCommand::InsertText(ch.to_string()));
        }
    }

    fn cell_caret_at(engine: &RichEngine, source: &str, needle: char) -> usize {
        let line = source.lines().next().unwrap_or(source);
        let off = line
            .find(needle)
            .unwrap_or_else(|| source.find(needle).expect("cell needle"));
        engine
            .cell_edit_range(off, source)
            .map(|r| r.start)
            .unwrap_or(off)
    }

    fn assert_two_col_table_keeps_literal(source: &str, caret_at: usize, typed: &str, label: &str) {
        let (mut doc, mut engine, mut caret) = setup(source);
        engine.sync(&doc);
        assert!(
            engine.in_table(caret_at),
            "{label}: caret must start in the table"
        );
        caret.collapse_to(caret_at);
        type_chars(&mut doc, &mut engine, &mut caret, typed);
        let after = doc.buffer.content();
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Table { .. }),
            "{label}: table must survive typing {typed:?}, got {after:?}"
        );
        assert_eq!(
            engine.tree().blocks[0].children[0].children.len(),
            2,
            "{label}: must keep two columns after {typed:?}, got {after:?}"
        );
        let header = after.lines().next().unwrap_or("");
        assert!(
            header.contains('|'),
            "{label}: GFM pipes must remain after {typed:?}, got {after:?}"
        );
        if source.lines().next().is_some_and(|l| l.starts_with('|')) {
            assert!(
                header.matches('|').count() >= 3,
                "{label}: piped header must keep pipes after {typed:?}, got {after:?}"
            );
        }
        let visible = header.replace('\\', "");
        assert!(
            visible.contains(typed) || header.contains(typed),
            "{label}: cell must contain {typed:?}, got {after:?}"
        );
        assert!(
            !after.contains("\n\n```") && !after.contains("---\n\n"),
            "{label}: must not insert a fence or thematic break, got {after:?}"
        );
    }

    #[test]
    fn table_cell_block_input_rules_stay_literal() {
        let piped = table_source();
        let empty_first = "|| b |\n|---|---|\n| 1 | 2 |\n";
        let pipeless = "a | b\n---|---\n1 | 2\n";
        let prefixes = ["# ", "- ", "> ", "* ", "1. ", "```"];

        for typed in prefixes {
            let (doc, engine, _) = setup(piped);
            let a = cell_caret_at(&engine, piped, 'a');
            drop(doc);
            assert_two_col_table_keeps_literal(piped, a, typed, &format!("piped a {typed:?}"));

            let b = piped.find('b').expect("header b");
            assert_two_col_table_keeps_literal(piped, b, typed, &format!("piped b {typed:?}"));

            let (_doc, engine, _) = setup(empty_first);
            let empty = first_empty_cell_body(&engine, empty_first).start;
            assert_two_col_table_keeps_literal(
                empty_first,
                empty,
                typed,
                &format!("empty first {typed:?}"),
            );

            let (_doc, engine, _) = setup(pipeless);
            let pa = cell_caret_at(&engine, pipeless, 'a');
            assert_two_col_table_keeps_literal(
                pipeless,
                pa,
                typed,
                &format!("pipeless a {typed:?}"),
            );
        }
    }

    #[test]
    fn table_cell_italic_auto_close_still_works() {
        let source = "|| b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let empty = first_empty_cell_body(&engine, source).start;
        caret.collapse_to(empty);
        type_chars(&mut doc, &mut engine, &mut caret, "*hi*");
        let after = doc.buffer.content();
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Table { .. }),
            "italic in a cell must not smash the table, got {after:?}"
        );
        assert_eq!(engine.tree().blocks[0].children[0].children.len(), 2);
        assert!(
            after.contains("*hi*") || after.contains("_hi_"),
            "cell italic auto-close must still wrap, got {after:?}"
        );
    }

    fn fence_body_offset(source: &str, needle: &str) -> usize {
        source.find(needle).expect(needle)
    }

    fn count_list_items(blocks: &[Block]) -> usize {
        blocks
            .iter()
            .map(|b| {
                usize::from(matches!(b.kind, BlockKind::ListItem { .. }))
                    + count_list_items(&b.children)
            })
            .sum()
    }

    fn still_one_fence(source: &str) -> bool {
        let ticks = source.matches("```").count();
        ticks == 2
            && source.contains("```")
            && !source.contains("`\n``")
            && !source.contains("``\n`")
    }

    fn first_code(blocks: &[Block]) -> Option<&Block> {
        for b in blocks {
            if matches!(b.kind, BlockKind::CodeBlock { .. }) {
                return Some(b);
            }
            if let Some(found) = first_code(&b.children) {
                return Some(found);
            }
        }
        None
    }

    fn first_opaque(blocks: &[Block]) -> Option<&Block> {
        for b in blocks {
            if matches!(b.kind, BlockKind::Opaque { .. }) {
                return Some(b);
            }
            if let Some(found) = first_opaque(&b.children) {
                return Some(found);
            }
        }
        None
    }

    fn painted_code_len(block: &Block) -> usize {
        match &block.kind {
            BlockKind::CodeBlock { literal, .. } => {
                literal.strip_suffix('\n').unwrap_or(literal).len()
            }
            _ => 0,
        }
    }

    #[test]
    fn tab_in_fenced_code_inserts_indent_not_list_indent() {
        let source = "```\ncode\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::IndentList);
        assert!(
            after.contains("```\n  code\n```") || after.contains("```\n\tcode\n```"),
            "Tab in a fence must indent the body, got {after:?}"
        );
        assert!(
            still_one_fence(&after),
            "Tab must not break fence chrome, got {after:?}"
        );
        assert!(
            !after.contains("  ```") && !after.contains("- code"),
            "Tab must not IndentList the document or the fence line, got {after:?}"
        );
    }

    #[test]
    fn tab_in_list_nested_fence_indents_code_not_the_list() {
        let source = "- item\n  ```\n  code\n  ```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::IndentList);
        assert!(
            after.contains("```"),
            "nested fence must survive Tab, got {after:?}"
        );
        assert!(
            after.starts_with("- item"),
            "Tab in nested fence must not re-indent the list item, got {after:?}"
        );
        assert!(
            after.contains("  code") || after.contains("\tcode") || after.contains("    code"),
            "Tab must indent the fenced body, got {after:?}"
        );
    }

    #[test]
    fn enter_in_fenced_code_stays_inside_the_fence() {
        let source = "```\ncode\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code") + "code".len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            still_one_fence(&after),
            "Enter must not split fence chrome, got {after:?}"
        );
        assert!(
            after.contains("```\ncode\n\n```") || after.contains("```\ncode\n \n```"),
            "Enter must add a line inside the fence, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::CodeBlock { .. }),
            "must remain a single code block, got {:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn enter_on_fence_chrome_does_not_split_ticks() {
        let source = "```\ncode\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(1);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            still_one_fence(&after),
            "Enter on opening ticks must not split ```, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::CodeBlock { .. }),
            "must remain a code block, got {:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn enter_on_list_looking_line_inside_fence_stays_in_fence() {
        let source = "```\n- \n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "- ") + 2);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            still_one_fence(&after),
            "Enter on `- ` inside a fence must not outdent as a list, got {after:?}"
        );
        assert!(
            after.contains("- "),
            "the code line `- ` must remain, got {after:?}"
        );
        assert!(
            after.contains("```\n- \n\n```") || after.contains("- \n\n```"),
            "Enter must insert a newline inside the fence, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::CodeBlock { .. }),
            "must remain a code block, got {:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn insert_line_break_in_fenced_code_is_a_newline() {
        let source = "```\ncode\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code") + "code".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertLineBreak,
        );
        assert!(
            still_one_fence(&after) && !after.contains('\\'),
            "Shift-Enter in a fence must be a raw newline, got {after:?}"
        );
        assert!(
            after.contains("```\ncode\n\n```"),
            "Shift-Enter must add a line inside the fence, got {after:?}"
        );
    }

    #[test]
    fn backspace_at_fenced_body_start_does_not_eat_the_fence() {
        let source = "hello\n\n```\ncode\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert_eq!(
            after, source,
            "Backspace at fence body start must be a no-op, got {after:?}"
        );
    }

    #[test]
    fn backspace_last_char_in_fence_does_not_delete_the_fence() {
        let source = "```\nx\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "x") + 1);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert!(
            still_one_fence(&after),
            "deleting the last body char must not swallow ```, got {after:?}"
        );
        assert!(
            !after.contains('x'),
            "the body character must be deleted, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::CodeBlock { .. }),
            "empty body must still be a fence, got {:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn shift_tab_in_fenced_code_does_not_strip_list_looking_lines() {
        let source = "```\n- foo\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "- foo") + 2);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::OutdentList);
        assert_eq!(
            after, source,
            "Shift-Tab in a fence must not treat a code line as a list item, got {after:?}"
        );
    }

    #[test]
    fn shift_tab_in_fenced_code_unindents_leading_spaces() {
        let source = "```\n  x\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "x"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::OutdentList);
        assert!(
            after.contains("```\nx\n```"),
            "Shift-Tab must strip leading indent inside the fence, got {after:?}"
        );
        assert!(
            still_one_fence(&after),
            "Shift-Tab must not break fence chrome, got {after:?}"
        );
    }

    fn every_line_quoted(source: &str) -> bool {
        source
            .lines()
            .all(|line| line.is_empty() || line.starts_with('>'))
    }

    fn still_list_nested_fence(source: &str) -> bool {
        still_one_fence(source)
            && source.starts_with("- ")
            && source.contains("\n  ```")
            && !source.contains("\n```")
    }

    #[test]
    fn shift_tab_in_list_nested_fence_keeps_required_indent() {
        let source = "- item\n  ```\n  code\n  ```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::OutdentList);
        assert_eq!(
            after, source,
            "Shift-Tab must not strip the list indent that keeps the fence in `- `, got {after:?}"
        );
        assert!(
            still_list_nested_fence(&after),
            "nested fence chrome must stay indented, got {after:?}"
        );
    }

    #[test]
    fn shift_tab_in_list_nested_fence_unindents_body_only() {
        let source = "- item\n  ```\n    code\n  ```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::OutdentList);
        assert!(
            still_list_nested_fence(&after),
            "Shift-Tab must keep the list-nested fence, got {after:?}"
        );
        assert!(
            after.contains("\n  code\n"),
            "Shift-Tab must strip only body indent after the list prefix, got {after:?}"
        );
        assert!(
            !after.contains("\ncode\n"),
            "must not pop the body out of the list item, got {after:?}"
        );
    }

    #[test]
    fn tab_in_quoted_fence_indents_after_the_quote() {
        let source = "> ```\n> code\n> ```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::IndentList);
        assert!(
            still_one_fence(&after) && every_line_quoted(&after),
            "Tab must keep a quoted fence, got {after:?}"
        );
        assert!(
            after.contains(">   code") || after.contains(">\tcode") || after.contains("> \tcode"),
            "Tab must indent after `>`, got {after:?}"
        );
        assert!(
            !after.contains(" >") && !after.starts_with(' '),
            "Tab must not put a space before `>`, got {after:?}"
        );
    }

    #[test]
    fn shift_tab_in_quoted_fence_unindents_after_the_quote() {
        let source = "> ```\n>   code\n> ```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::OutdentList);
        assert!(
            still_one_fence(&after) && every_line_quoted(&after),
            "Shift-Tab must keep a quoted fence, got {after:?}"
        );
        assert!(
            after.contains("> code"),
            "Shift-Tab must strip body indent after `>`, got {after:?}"
        );
    }

    #[test]
    fn shift_tab_in_quoted_fence_does_not_eat_quote() {
        let source = "> ```\n> code\n> ```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::OutdentList);
        assert_eq!(
            after, source,
            "Shift-Tab must not eat `>` when there is no body indent, got {after:?}"
        );
    }

    #[test]
    fn enter_in_quoted_fence_keeps_quote_prefixes() {
        let source = "> ```\n> code\n> ```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code") + "code".len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            still_one_fence(&after) && every_line_quoted(&after),
            "Enter must keep `>` on every fence line, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::BlockQuote),
            "must remain a quoted fence, got {:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn enter_on_quoted_fence_chrome_keeps_quote_prefixes() {
        let source = "> ```\n> code\n> ```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('`').expect("ticks"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            still_one_fence(&after) && every_line_quoted(&after),
            "Enter on quoted ticks must not unquote the fence, got {after:?}"
        );
    }

    #[test]
    fn insert_line_break_in_quoted_fence_keeps_quote_prefixes() {
        let source = "> ```\n> code\n> ```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code") + "code".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertLineBreak,
        );
        assert!(
            still_one_fence(&after) && every_line_quoted(&after) && !after.contains('\\'),
            "Shift-Enter must keep quoted fence lines, got {after:?}"
        );
    }

    #[test]
    fn backspace_at_quoted_fence_body_start_does_not_eat_quote() {
        let source = "> ```\n> code\n> ```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert_eq!(
            after, source,
            "Backspace at quoted body start must not eat `>` or the fence, got {after:?}"
        );
    }

    #[test]
    fn backspace_last_char_in_quoted_fence_keeps_quote_and_fence() {
        let source = "> ```\n> x\n> ```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "x") + 1);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert!(
            still_one_fence(&after) && every_line_quoted(&after),
            "deleting the last quoted body char must keep `>` and ```, got {after:?}"
        );
        assert!(
            !after.contains('x'),
            "body char must be deleted, got {after:?}"
        );
    }

    #[test]
    fn backspace_on_later_quoted_fence_line_joins_without_eating_quote() {
        let source = "> ```\n> a\n> b\n> ```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "b"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert!(
            still_one_fence(&after) && every_line_quoted(&after),
            "join must keep quoted fence chrome, got {after:?}"
        );
        assert!(
            after.contains("> ab") || after.contains("> a b"),
            "Backspace at the start of the next code line must join, got {after:?}"
        );
    }

    #[test]
    fn tab_and_shift_tab_on_quoted_list_nested_fence() {
        let source = "> - item\n>   ```\n>   code\n>   ```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code"));
        let indented = apply(&mut doc, &mut engine, &mut caret, RichCommand::IndentList);
        assert!(
            still_one_fence(&indented) && every_line_quoted(&indented),
            "Tab must keep a quoted list-nested fence, got {indented:?}"
        );
        assert!(
            indented.contains("> - item"),
            "Tab must not indent before `- `, got {indented:?}"
        );
        let out = apply(&mut doc, &mut engine, &mut caret, RichCommand::OutdentList);
        assert!(
            still_one_fence(&out) && every_line_quoted(&out),
            "Shift-Tab must keep the quoted list-nested fence, got {out:?}"
        );
        assert!(
            out.contains(">   ```"),
            "Shift-Tab must not strip the list indent inside the quote, got {out:?}"
        );
    }

    #[test]
    fn enter_in_quoted_html_block_keeps_quote_prefixes() {
        let source = "> <div>\n> x\n> </div>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('x').expect("x"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            every_line_quoted(&after),
            "Enter in quoted HTML must keep `>` on every line, got {after:?}"
        );
        assert!(
            after.contains("<div>") && after.contains("</div>"),
            "HTML chrome must survive, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::BlockQuote),
            "must remain quoted, got {:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn backspace_at_html_block_start_does_not_eat_previous_paragraph() {
        let source = "hello\n\n<div>\nx\n</div>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("<div>").expect("div"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert!(
            after.starts_with("hello"),
            "Backspace at HTML start must not eat the previous paragraph, got {after:?}"
        );
        assert!(
            after.contains("<div>"),
            "HTML chrome must survive, got {after:?}"
        );
    }

    #[test]
    fn enter_in_html_block_stays_inside() {
        let source = "<div>\nx\n</div>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('x').expect("x") + 1);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("<div>") && after.contains("</div>"),
            "Enter must stay inside the HTML block, got {after:?}"
        );
        assert!(
            after.contains("x\n"),
            "Enter must insert a newline in the HTML body, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Opaque { .. }),
            "must remain an HTML block, got {:?}",
            engine.tree().blocks[0].kind
        );
    }

    #[test]
    fn quoted_fence_visible_map_skips_quote_prefix() {
        let source = "> ```\n> code\n> ```\n";
        let (_doc, engine, _) = setup(source);
        let block = first_code(&engine.tree().blocks).expect("quoted fence");
        let map = super::code_body_source_map(source, block, painted_code_len(block));
        let c = source.find("code").expect("code");
        let gt = source.find('>').expect(">");
        assert_eq!(
            map[0], c,
            "first painted body byte must be `c`, map={map:?}"
        );
        assert_ne!(
            map[0], gt,
            "click on painted `code` must not land on `>`, map={map:?}"
        );
        assert_eq!(&source[map[0]..map[0] + 1], "c");
        assert_eq!(map.len(), "code".len() + 1);
    }

    #[test]
    fn unquoted_fence_visible_map_is_one_to_one() {
        let source = "```\ncode\n```\n";
        let (_doc, engine, _) = setup(source);
        let block = first_code(&engine.tree().blocks).expect("fence");
        let map = super::code_body_source_map(source, block, painted_code_len(block));
        let c = source.find("code").expect("code");
        assert_eq!(map[0], c);
        assert_eq!(map[1], c + 1);
        assert_eq!(map[4], c + 4);
    }

    #[test]
    fn list_nested_fence_visible_map_skips_indent() {
        let source = "- item\n  ```\n  code\n  ```\n";
        let (_doc, engine, _) = setup(source);
        let block = first_code(&engine.tree().blocks).expect("nested fence");
        let map = super::code_body_source_map(source, block, painted_code_len(block));
        let c = source.find("code").expect("code");
        let indent = source.find("  code").expect("indented code line");
        assert_eq!(map[0], c, "first painted byte must be `c`, map={map:?}");
        assert_ne!(
            map[0], indent,
            "click on painted `code` must not land on list indent, map={map:?}"
        );
    }

    #[test]
    fn quoted_list_nested_fence_visible_map_skips_quote_and_indent() {
        let source = "> - item\n>   ```\n>   code\n>   ```\n";
        let (_doc, engine, _) = setup(source);
        let block = first_code(&engine.tree().blocks).expect("quoted nested fence");
        let map = super::code_body_source_map(source, block, painted_code_len(block));
        let c = source.find("code").expect("code");
        let gt = source.rfind(">   code").expect("quoted code line");
        assert_eq!(map[0], c);
        assert_ne!(map[0], gt, "must skip `>` on the body line, map={map:?}");
    }

    #[test]
    fn quoted_multiline_fence_visible_map_skips_prefix_on_each_line() {
        let source = "> ```\n> ab\n> cd\n> ```\n";
        let (_doc, engine, _) = setup(source);
        let block = first_code(&engine.tree().blocks).expect("quoted fence");
        let map = super::code_body_source_map(source, block, painted_code_len(block));
        let a = source.find("ab").expect("ab");
        let c = source.find("cd").expect("cd");
        assert_eq!(map[0], a);
        assert_eq!(&source[map[0]..map[0] + 2], "ab");
        let nl = map
            .iter()
            .position(|&off| source.as_bytes().get(off) == Some(&b'\n'));
        assert!(
            nl.is_some(),
            "newline must stay in the painted map, map={map:?}"
        );
        assert_eq!(map[3], c, "second line `c` after skipped `>`, map={map:?}");
        assert_ne!(map[3], source.find('>').expect(">"));
    }

    fn painted_html_len(block: &Block) -> usize {
        match &block.kind {
            BlockKind::Opaque { raw } => raw.len(),
            _ => 0,
        }
    }

    #[test]
    fn quoted_html_visible_map_skips_quote_prefix() {
        let source = "> <div>\n> x\n> </div>\n";
        let (_doc, engine, _) = setup(source);
        let block = first_opaque(&engine.tree().blocks).expect("quoted html");
        let map = super::code_body_source_map(source, block, painted_html_len(block));
        let x = source.find('x').expect("x");
        let gt = source.find('>').expect(">");
        assert!(
            map.contains(&x),
            "map must include the `x` byte, map={map:?}"
        );
        assert_ne!(
            map[0], gt,
            "first painted HTML body byte must not be `>`, map={map:?}"
        );
        assert_eq!(&source[x..x + 1], "x");
    }

    #[test]
    fn unquoted_html_visible_map_is_one_to_one_on_literal() {
        let source = "<div>\nx\n</div>\n";
        let (_doc, engine, _) = setup(source);
        let block = first_opaque(&engine.tree().blocks).expect("html");
        let raw = match &block.kind {
            BlockKind::Opaque { raw } => raw.as_str(),
            _ => unreachable!(),
        };
        let map = super::code_body_source_map(source, block, raw.len());
        assert_eq!(map[0], block.source_range.start);
        assert_eq!(map[1], block.source_range.start + 1);
        let x = source.find('x').expect("x");
        assert!(map.contains(&x), "map={map:?}");
    }

    #[test]
    fn indented_code_visible_map_skips_indent() {
        let source = "    code\n";
        let (_doc, engine, _) = setup(source);
        let block = first_code(&engine.tree().blocks).expect("indented code");
        let map = super::code_body_source_map(source, block, painted_code_len(block));
        let c = source.find("code").expect("code");
        let indent = source.find("    code").expect("indent");
        assert_eq!(map[0], c, "first painted byte must be `c`, map={map:?}");
        assert_ne!(map[0], indent);
    }

    #[test]
    fn insert_text_at_indented_code_indent_goes_into_the_body() {
        let source = "    indented\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(0);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert!(
            after.contains("    xindented") || after.contains("\txindented"),
            "typing at indent must insert in the body, got {after:?}"
        );
        assert!(
            !after.starts_with('x'),
            "must not glue onto the indent, got {after:?}"
        );
    }

    #[test]
    fn backspace_at_indented_code_body_start_does_not_nibble_indent() {
        let source = "    indented\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("indented").expect("body"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert_eq!(
            after, source,
            "Backspace at indented-code body start must not nibble indent, got {after:?}"
        );
    }

    /// 0–3 spaces before ATX / fence are dest chrome. Typing at Home goes
    /// into the title/body. Backspace at the first body byte strips
    /// indent+`#` (Typora) or does not nibble fence indent. Quoted keep `>`.
    #[test]
    fn cm_opening_indent_insert_and_backspace_stay_in_body() {
        for source in [" # Title\n", "  # Title\n", "   # Title\n"] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(0);
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("z".into()),
            );
            assert!(
                after.contains("# zTitle") || after.contains("#zTitle"),
                "typing at opening indent must insert in the title, {source:?} got {after:?}"
            );
            assert!(
                !after.starts_with('z'),
                "must not glue onto the indent, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(atx_body_start(source, 0));
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            assert_eq!(
                first_line(&after).trim_end(),
                "Title",
                "Backspace at title must strip indent+`#`, {source:?} got {after:?}"
            );
            assert!(
                !after.contains('#'),
                "must not nibble indent into a heading, {source:?} got {after:?}"
            );
        }

        let quoted = ">  # Title\n";
        let (mut doc, mut engine, mut caret) = setup(quoted);
        let home = engine.clamp_raw_prefix(quoted, engine.snap_caret(0, Bias::Right), Bias::Right);
        caret.collapse_to(home);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        assert!(
            after.contains("# zTitle") || after.contains("#zTitle"),
            "quoted typing at Home must insert in the title, got {after:?}"
        );
        assert!(
            after.starts_with('>'),
            "quoted ATX must keep `>`, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(quoted);
        caret.collapse_to(atx_body_start(quoted, 0));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let line = first_line(&after);
        assert!(
            line.starts_with('>') && !line.contains('#') && line.contains("Title"),
            "quoted Backspace at title keeps `>` and strips indent+`#`, got {after:?}"
        );

        let setext = " Title\n===\n";
        let (mut doc, mut engine, mut caret) = setup(setext);
        caret.collapse_to(0);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        assert!(
            after.contains("zTitle"),
            "typing at setext indent must insert in the title, got {after:?}"
        );
        assert!(
            !after.starts_with('z'),
            "must not glue onto setext indent, got {after:?}"
        );
        assert!(
            after.contains("==="),
            "must keep the setext underline, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(setext);
        caret.collapse_to(setext.find("Title").expect("Title"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert_eq!(
            first_line(&after).trim_end(),
            "Title",
            "Backspace at setext title must strip indent+underline, got {after:?}"
        );
        assert!(
            !after.contains('='),
            "setext underline must be gone, got {after:?}"
        );

        let fence = "  ```\n  foo\n  ```\n";
        let (mut doc, mut engine, mut caret) = setup(fence);
        caret.collapse_to(0);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        assert!(
            after.contains("zfoo") || after.contains("z foo"),
            "typing at fence indent must insert in the body, got {after:?}"
        );
        assert!(
            !after.starts_with('z'),
            "must not glue onto fence indent, got {after:?}"
        );
        assert!(
            still_one_fence(&after),
            "must keep the fence, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(fence);
        caret.collapse_to(fence.find("foo").expect("foo"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert_eq!(
            after, fence,
            "Backspace at fence body start must not nibble indent, got {after:?}"
        );
    }

    #[test]
    fn indented_fence_visible_map_skips_fence_offset() {
        let source = "  ```\n  foo\n  ```\n";
        let (_doc, engine, _) = setup(source);
        let block = first_code(&engine.tree().blocks).expect("indented fence");
        let map = super::code_body_source_map(source, block, painted_code_len(block));
        let f = source.find("foo").expect("foo");
        let indent = source.find("  foo").expect("content indent");
        assert_eq!(map[0], f, "first painted byte must be `f`, map={map:?}");
        assert_ne!(
            map[0], indent,
            "click on painted `foo` must not land on fence_offset spaces, map={map:?}"
        );

        let extra = "  ```\n    foo\n  ```\n";
        let (_doc, engine, _) = setup(extra);
        let block = first_code(&engine.tree().blocks).expect("extra indent");
        let map = super::code_body_source_map(extra, block, painted_code_len(block));
        let line = extra.find("    foo").expect("line");
        let f = extra.find("foo").expect("foo");
        assert_eq!(
            map[0],
            line + 2,
            "fence_offset 2 of 4 content spaces; first painted is remaining indent, map={map:?}"
        );
        assert!(map.contains(&f), "map must include `f`, map={map:?}");
        assert_ne!(map[0], f, "must not strip content indent twice");
    }

    #[test]
    fn delete_at_end_of_fence_does_not_nibble_closing_ticks() {
        let source = "```\ncode\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code") + "code".len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        assert_eq!(
            after, source,
            "Delete at end of fence body must not nibble closing ticks, got {after:?}"
        );
    }

    #[test]
    fn delete_in_middle_of_fence_deletes_a_grapheme() {
        let source = "```\ncode\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code") + 1);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        assert!(
            still_one_fence(&after),
            "mid-body Delete must keep the fence, got {after:?}"
        );
        assert!(
            after.contains("```\ncde\n```") || after.contains("cde"),
            "Delete on `o` must remove that grapheme, got {after:?}"
        );
        assert!(!after.contains("code"), "got {after:?}");
    }

    #[test]
    fn delete_last_char_in_fence_does_not_nibble_ticks() {
        let source = "```\ncode\n```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code") + "code".len() - 1);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        assert!(
            still_one_fence(&after),
            "Delete on the last body grapheme must keep ```, got {after:?}"
        );
        assert!(
            after.contains("```\ncod\n```"),
            "Delete must remove the last body grapheme, got {after:?}"
        );
    }

    #[test]
    fn delete_at_end_of_quoted_fence_does_not_nibble_quote_or_ticks() {
        let source = "> ```\n> code\n> ```\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(fence_body_offset(source, "code") + "code".len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        assert_eq!(
            after, source,
            "Delete at end of quoted fence must not nibble `>` or ticks, got {after:?}"
        );
    }

    #[test]
    fn delete_at_end_of_html_block_does_not_nibble_closing() {
        let source = "hello\n\n<div>\nx\n</div>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let html_end = first_opaque(&engine.tree().blocks)
            .expect("html")
            .source_range
            .end;
        caret.collapse_to(html_end);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        assert_eq!(
            after, source,
            "Delete at end of HTML body must not nibble tags or the next bytes, got {after:?}"
        );
    }

    #[test]
    fn delete_in_middle_of_html_deletes_a_grapheme() {
        let source = "<div>\nx\n</div>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('x').expect("x"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        assert!(
            after.contains("<div>") && after.contains("</div>"),
            "HTML chrome must survive, got {after:?}"
        );
        assert!(
            !after.contains('x'),
            "Delete must remove `x`, got {after:?}"
        );
    }

    #[test]
    fn delete_at_end_of_quoted_html_does_not_eat_quote() {
        let source = "> <div>\n> x\n> </div>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let html_end = first_opaque(&engine.tree().blocks)
            .expect("html")
            .source_range
            .end;
        caret.collapse_to(html_end);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        assert_eq!(
            after, source,
            "Delete at end of quoted HTML must not nibble `>` or tags, got {after:?}"
        );
    }

    fn has_definition_list(blocks: &[Block]) -> bool {
        blocks.iter().any(|b| {
            matches!(b.kind, BlockKind::DefinitionList) || has_definition_list(&b.children)
        })
    }

    fn extra_blank_before_details(source: &str) -> bool {
        source.contains("\n\n\n")
    }

    #[test]
    fn enter_at_end_of_definition_term_places_details_opener() {
        for source in [
            "Term\n\n: details\n",
            "Term\n: details\n",
            "> Term\n> : details\n",
            "> Term\n>\n> : details\n",
            "Term\n: \n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            assert!(
                has_definition_list(&engine.tree().blocks),
                "fixture must parse as a definition list: {source:?}"
            );
            let term_end = source.find("Term").expect("Term") + "Term".len();
            caret.collapse_to(term_end);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert!(
                !extra_blank_before_details(&after),
                "Enter at end of term must not insert extra blanks, {source:?} -> {after:?}"
            );
            assert!(
                has_definition_list(&engine.tree().blocks),
                "must remain a definition list after Enter, {source:?} -> {after:?}"
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            assert!(
                typed.contains(": x") || typed.contains(":x"),
                "typing after term-end Enter must land in details, {source:?} -> {typed:?}"
            );
            assert!(
                !typed.contains("Termx") && !typed.contains("Term\n\n\nx"),
                "must not type into the term or a new blank paragraph, {source:?} -> {typed:?}"
            );
        }
    }

    #[test]
    fn enter_at_end_of_definition_term_via_insert_newline_places_details() {
        let source = "Term\n\n: details\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to("Term".len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n".into()),
        );
        assert_eq!(
            after, source,
            "IME Enter at end of term must place the details opener, not splice blanks"
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert!(
            doc.buffer.content().contains(": xdetails"),
            "got {:?}",
            doc.buffer.content()
        );
    }

    #[test]
    fn backspace_at_start_of_definition_details_strips_marker() {
        for source in [
            "Term\n\n: details\n",
            "Term\n: details\n",
            "> Term\n> : details\n",
            "> Term\n>\n> : details\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let d = source.find("details").expect("details");
            caret.collapse_to(d);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            assert!(
                !after.contains("Termdetails") && !after.contains("> Termdetails"),
                "Backspace at details start must not concatenate term+details, {source:?} -> {after:?}"
            );
            assert!(
                after.contains("Term") && after.contains("details"),
                "term and details text must survive, {source:?} -> {after:?}"
            );
            let details_line = after
                .lines()
                .find(|l| l.contains("details"))
                .expect("details line");
            assert!(
                definition_details_marker_prefix(after_quote(details_line)).is_none(),
                "`: ` marker must be stripped, {source:?} -> {after:?}"
            );
        }
    }

    #[test]
    fn backspace_mid_definition_details_still_deletes_a_grapheme() {
        let source = "Term\n: details\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("tails").expect("mid"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert!(
            after.contains(": ") && after.contains("Term") && !after.contains("details"),
            "mid-details Backspace still deletes a grapheme, got {after:?}"
        );
        assert!(
            has_definition_list(&engine.tree().blocks),
            "must remain a definition list, got {after:?}"
        );
    }

    #[test]
    fn delete_word_left_at_definition_details_start_strips_marker() {
        let source = "Term\n: details\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("details").expect("details"));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert!(
            !after.contains("Termdetails"),
            "Option-Backspace at details start must not join into Termdetails, got {after:?}"
        );
        assert!(
            after.contains("Term") && after.contains("details"),
            "got {after:?}"
        );
        let details_line = after
            .lines()
            .find(|l| l.contains("details"))
            .expect("details line");
        assert!(
            definition_details_marker_prefix(after_quote(details_line)).is_none(),
            "marker must be gone, got {after:?}"
        );
    }

    fn count_kind(blocks: &[Block], pred: fn(&BlockKind) -> bool) -> usize {
        blocks.iter().fold(0, |n, b| {
            n + usize::from(pred(&b.kind)) + count_kind(&b.children, pred)
        })
    }

    #[test]
    fn enter_in_middle_of_definition_details_keeps_the_list() {
        for source in [
            "Term\n: details\n",
            "Term\n\n: details\n",
            "> Term\n> : details\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            assert!(
                has_definition_list(&engine.tree().blocks),
                "fixture must parse as a definition list: {source:?}"
            );
            let mid = source.find("tails").expect("mid details");
            caret.collapse_to(mid);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert!(
                has_definition_list(&engine.tree().blocks),
                "Enter mid-details must keep a definition list, {source:?} -> {after:?}"
            );
            assert_eq!(
                engine.tree().blocks.len(),
                1,
                "must not split the rest of details into a top-level paragraph, {source:?} -> {after:?}"
            );
            assert!(
                after.contains("de") && after.contains("tails") && after.contains("Term"),
                "both details halves and the term must survive, {source:?} -> {after:?}"
            );
            assert!(
                after.contains(": de") && (after.contains(": tails") || after.contains(":tails")),
                "must continue as more `: ` details, {source:?} -> {after:?}"
            );
            assert!(
                !after.contains("de\n\ntails"),
                "must not use a paragraph split, {source:?} -> {after:?}"
            );
            assert!(
                count_kind(&engine.tree().blocks, |k| matches!(
                    k,
                    BlockKind::DefinitionDetails
                )) >= 1,
                "details must survive as details, {source:?} -> {after:?}"
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            assert!(
                !typed.contains("dextails") && !typed.contains("dex\ntails"),
                "typing after mid-details Enter must land in the continuation, {source:?} -> {typed:?}"
            );
            assert!(
                has_definition_list(&engine.tree().blocks),
                "must remain a definition list after typing, {source:?} -> {typed:?}"
            );
            assert!(
                typed.contains("xtails") || typed.contains("x tails"),
                "typed text must prefix the second half, {source:?} -> {typed:?}"
            );
        }
    }

    #[test]
    fn enter_in_middle_of_definition_details_via_insert_newline_keeps_the_list() {
        let source = "Term\n: details\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("tails").expect("mid"));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n".into()),
        );
        assert!(
            has_definition_list(&engine.tree().blocks),
            "IME Enter mid-details must keep a definition list, got {after:?}"
        );
        assert_eq!(
            engine.tree().blocks.len(),
            1,
            "must not orphan the rest as prose, got {after:?}"
        );
        assert!(
            !after.contains("de\n\ntails"),
            "must not use a paragraph split, got {after:?}"
        );
    }

    #[test]
    fn enter_in_middle_of_definition_term_keeps_the_list() {
        for source in [
            "Term\n: details\n",
            "Term\n\n: details\n",
            "> Term\n> : details\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let mid = source.find("Term").expect("Term") + 2;
            caret.collapse_to(mid);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert!(
                has_definition_list(&engine.tree().blocks),
                "Enter mid-term must keep a definition list, {source:?} -> {after:?}"
            );
            assert_eq!(
                engine.tree().blocks.len(),
                1,
                "must not split `Te` into a top-level paragraph, {source:?} -> {after:?}"
            );
            assert!(
                after.contains("Te") && after.contains("rm") && after.contains("details"),
                "both halves and details must survive, {source:?} -> {after:?}"
            );
            assert!(
                !after.contains("Te\n\nrm"),
                "must not use a paragraph split, {source:?} -> {after:?}"
            );
        }
    }

    #[test]
    fn enter_at_start_of_definition_term_keeps_the_list() {
        for source in [
            "Term\n: details\n",
            "Term\n\n: details\n",
            "> Term\n> : details\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let start = source.find("Term").expect("Term");
            caret.collapse_to(start);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert!(
                has_definition_list(&engine.tree().blocks),
                "Enter at term start must keep a definition list, {source:?} -> {after:?}"
            );
            assert!(
                after.contains("Term") && after.contains("details"),
                "term and details must survive, {source:?} -> {after:?}"
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            assert!(
                !typed.contains("Termx") && !typed.contains("> Termx"),
                "typing after start-Enter must not glue onto the term, {source:?} -> {typed:?}"
            );
            assert!(
                has_definition_list(&engine.tree().blocks),
                "must remain a definition list after typing, {source:?} -> {typed:?}"
            );
        }
    }

    #[test]
    fn enter_at_end_of_definition_details_opens_a_new_term() {
        for source in [
            "Term\n: details\n",
            "Term\n\n: details\n",
            "> Term\n> : details\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let end = source.find("details").expect("details") + "details".len();
            caret.collapse_to(end);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("Next".into()),
            );
            let after_term = doc.buffer.content();
            assert!(
                after_term.contains("Next"),
                "typing after details-end Enter must start a new term, {source:?} -> {after_term:?}"
            );
            assert!(
                !after_term.contains("detailsNext") && !after_term.contains("details\nNext\n:"),
                "Next must not continue the previous details, {source:?} -> {after_term:?}"
            );
            apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("more".into()),
            );
            let after = doc.buffer.content();
            assert!(
                has_definition_list(&engine.tree().blocks),
                "must remain a definition list, {source:?} -> {after:?}"
            );
            assert!(
                count_kind(&engine.tree().blocks, |k| matches!(
                    k,
                    BlockKind::DefinitionItem { .. }
                )) >= 2,
                "Enter at end of details then a new term must add a second item, {source:?} -> {after:?}"
            );
            assert!(
                after.contains("more") && after.contains("Next") && after.contains("details"),
                "both items must survive, {source:?} -> {after:?}"
            );
        }
    }

    #[test]
    fn enter_at_end_of_definition_details_via_insert_newline_opens_term() {
        let source = "Term\n: details\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("details").expect("details") + "details".len());
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("Next".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n".into()),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("more".into()),
        );
        let after = doc.buffer.content();
        assert!(
            count_kind(&engine.tree().blocks, |k| matches!(
                k,
                BlockKind::DefinitionItem { .. }
            )) >= 2,
            "IME Enter at end of details must open a new term, got {after:?}"
        );
        assert!(after.contains(": more") || after.contains(":more"));
    }

    #[test]
    fn typing_on_empty_definition_details_stays_in_the_body() {
        let source = "Term\n: ";
        let (mut doc, mut engine, mut caret) = setup(source);
        let home = engine
            .tree()
            .empty_prefix_homes
            .iter()
            .find(|h| {
                source.get(h.line.clone()).is_some_and(|l| {
                    definition_details_marker_on_line(after_quote(l)).starts_with(':')
                })
            })
            .map(|h| h.home)
            .expect("empty details home");
        caret.collapse_to(home);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert!(
            after.contains(": x") || after.contains(":x"),
            "typing on empty `: ` must fill the details body, got {after:?}"
        );
        assert!(
            !after.contains("Termx"),
            "must not type into the term, got {after:?}"
        );
    }

    fn has_footnote_def(blocks: &[Block]) -> bool {
        blocks.iter().any(|b| {
            matches!(b.kind, BlockKind::FootnoteDefinition { .. }) || has_footnote_def(&b.children)
        })
    }

    #[test]
    fn backspace_at_start_of_footnote_def_strips_marker() {
        for source in [
            "Hello[^1]\n\n[^1]: the note\n",
            "Hello[^note]\n\n[^note]: the note\n",
            "> Hello[^1]\n\n> [^1]: the note\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            assert!(
                has_footnote_def(&engine.tree().blocks),
                "fixture must parse as a footnote def: {source:?}"
            );
            caret.collapse_to(source.find("the").expect("the"));
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            assert!(
                !after.contains("[^1]:") && !after.contains("[^note]:"),
                "Backspace at footnote def body start must strip `[^…]: `, {source:?} -> {after:?}"
            );
            assert!(
                after.contains("the note"),
                "body text must survive, {source:?} -> {after:?}"
            );
            assert!(
                after.contains("[^1]") || after.contains("[^note]"),
                "the footnote ref must survive, {source:?} -> {after:?}"
            );
            if source.contains("> Hello") {
                assert!(
                    after.contains("> the note") || after.contains(">the note"),
                    "quoted def must keep `>`, {source:?} -> {after:?}"
                );
            }
        }
    }

    #[test]
    fn backspace_mid_footnote_def_still_deletes_a_grapheme() {
        let source = "Hello[^1]\n\n[^1]: the note\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("note").expect("mid"));
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert!(
            after.contains("[^1]:") && after.contains("the") && !after.contains("the note"),
            "mid-def Backspace still deletes a grapheme, got {after:?}"
        );
        assert!(
            has_footnote_def(&engine.tree().blocks),
            "must remain a footnote def, got {after:?}"
        );
    }

    #[test]
    fn delete_word_left_at_footnote_def_start_strips_marker() {
        let source = "Hello[^1]\n\n[^1]: the note\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("the").expect("the"));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        assert!(
            !after.contains("[^1]:"),
            "Option-Backspace at footnote def start must strip `[^1]: `, got {after:?}"
        );
        assert!(
            after.contains("the note") && after.contains("[^1]"),
            "ref and body must survive, got {after:?}"
        );
    }

    #[test]
    fn backspace_on_empty_footnote_def_strips_marker() {
        let source = "Hello[^1]\n\n[^1]: ";
        let (mut doc, mut engine, mut caret) = setup(source);
        let home = engine
            .tree()
            .empty_prefix_homes
            .iter()
            .find(|h| source.get(h.line.clone()).is_some_and(|l| l.contains("]:")))
            .map(|h| h.home)
            .expect("empty footnote def home");
        caret.collapse_to(home);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert!(
            !after.contains("[^1]:"),
            "Backspace on empty `[^1]: ` must strip the marker, got {after:?}"
        );
        assert!(
            after.contains("Hello[^1]"),
            "the footnote ref must survive, got {after:?}"
        );
    }

    #[test]
    fn typing_on_empty_footnote_def_stays_in_the_body() {
        let source = "Hello[^1]\n\n[^1]: ";
        let (mut doc, mut engine, mut caret) = setup(source);
        let home = engine
            .tree()
            .empty_prefix_homes
            .iter()
            .find(|h| source.get(h.line.clone()).is_some_and(|l| l.contains("]:")))
            .map(|h| h.home)
            .expect("empty footnote def home");
        caret.collapse_to(home);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert!(
            after.contains("[^1]: x") || after.contains("[^1]:x"),
            "typing on empty `[^1]: ` must fill the def body, got {after:?}"
        );
        assert!(
            !after.contains("Hello[^1]x") && !after.contains("Hellox"),
            "must not type into the previous paragraph, got {after:?}"
        );
    }

    /// Leftover viewport below a last-block table/list/quote/fence/frontmatter
    /// must open a new paragraph, not continue the last cell/item/body.
    #[test]
    fn leftover_click_after_gfm_constructs_appends_paragraph() {
        for source in [
            "| a | b |\n|---|---|\n| 1 | 2 |",
            "| a | b |\n|---|---|\n| 1 | 2 |\n",
            "| a | b |\n|---|---|",
            "| a |\n|---|\n| 1 |",
            "| a | b |\n|---|---|\n| 1 | 2 |  ",
            "> | a | b |\n> |---|---|\n> | 1 | 2 |",
            "- hello",
            "- hello\n",
            "> hello",
            "> hello\n",
            "```\ncode\n```",
            "```\ncode\n```\n",
            "- [x] done",
            "- [x] done\n",
            "- outer\n  - inner",
            "> - item",
            "Title\n===",
            "# Title #",
            "<div>hello</div>",
            "<pre>**bold**</pre>",
            "<div></div>",
            "[TOC]",
            "[[toc]]",
            "Term\n: details",
            "Hello[^1]\n\n[^1]: note",
            "[label][ref]\n\n[ref]: https://e.com",
            "<style>body { color: red }</style>",
            "<textarea>hello</textarea>",
            "<script>alert(1)</script>",
            "<iframe src=\"https://e.com\"></iframe>",
            "<iframe src=\"https://e.com\"><p>nested</p></iframe>",
            "<title>Doc title</title>",
            "<xmp>raw <b>html</b></xmp>",
            "<noembed>fallback</noembed>",
            "<noframes>fallback</noframes>",
            "<plaintext>raw text",
            "<details><summary>Title</summary>body</details>",
            "<details>\n<summary>Title</summary>\nhidden\n</details>",
            "- <details><summary>Title</summary>body</details>",
            "> <details><summary>Title</summary>body</details>",
            "<video src=\"x.mp4\"></video>",
            "<video>\nhello\n</video>",
            "<audio src=\"x.mp3\"></audio>",
            "<dialog>hello</dialog>",
            "<form action=\"/x\">ok</form>",
            "<object data=\"x\"></object>",
            "<math>x^2</math>",
            "<canvas>fallback</canvas>",
            "- <video src=\"x.mp4\"></video>",
            "> <dialog>hello</dialog>",
            "<button>click</button>",
            "<button>\nclick\n</button>",
            "hello <button>click</button>",
            "- <button>click</button>",
            "> <button>click</button>",
            "<select><option>a</option></select>",
            "> <select><option>a</option></select>",
            "<input type=\"text\">",
            "- <input type=\"text\">",
            "<label>Name</label>",
            "<option>a</option>",
            "<noscript>fallback</noscript>",
            "<template><p>slot</p></template>",
            "- <noscript>fallback</noscript>",
            "> <template><p>slot</p></template>",
            "<fieldset><legend>Title</legend>body</fieldset>",
            "<fieldset>\n<legend>Title</legend>\nhidden\n</fieldset>",
            "- <fieldset><legend>Title</legend>body</fieldset>",
            "> <fieldset><legend>Title</legend>body</fieldset>",
            "<legend>Title</legend>",
            "<output>42</output>",
            "hello <output>42</output>",
            "- <output>42</output>",
            "> <output>42</output>",
            "<progress value=\"70\" max=\"100\">70%</progress>",
            "> <progress value=\"70\">70%</progress>",
            "<meter value=\"0.6\">60%</meter>",
            "- <meter value=\"0.6\">60%</meter>",
        ] {
            let typed = leftover_click_then_type(source);
            assert!(
                typed.lines().any(|line| line.trim() == "x"),
                "leftover click + type must be a new paragraph, {source:?} got {typed:?}"
            );
            let x_line = typed
                .lines()
                .find(|line| line.trim() == "x")
                .expect("typed x line");
            assert!(
                !x_line.starts_with('>')
                    && !x_line.starts_with('-')
                    && !x_line.starts_with('|')
                    && !x_line.starts_with('`')
                    && !x_line.starts_with(':')
                    && !x_line.starts_with('#'),
                "new paragraph must not keep last-block chrome, {source:?} got {typed:?}"
            );
            assert!(
                !typed.contains("| 2x")
                    && !typed.contains("|2x")
                    && !typed.contains("hellox")
                    && !typed.contains("codex")
                    && !typed.contains("Testx")
                    && !typed.contains("title: x")
                    && !typed.contains("title: Testx")
                    && !typed.contains("donex")
                    && !typed.contains("innerx")
                    && !typed.contains("itemx")
                    && !typed.contains("Titlex")
                    && !typed.contains("===x")
                    && !typed.contains("detailsx")
                    && !typed.contains("notex")
                    && !typed.contains("e.comx")
                    && !typed.contains("Doc titlex")
                    && !typed.contains("</div>x")
                    && !typed.contains("</pre>x")
                    && !typed.contains("</style>x")
                    && !typed.contains("</textarea>x")
                    && !typed.contains("</script>x")
                    && !typed.contains("</iframe>x")
                    && !typed.contains("</title>x")
                    && !typed.contains("</xmp>x")
                    && !typed.contains("</noembed>x")
                    && !typed.contains("</noframes>x")
                    && !typed.contains("</plaintext>x")
                    && !typed.contains("</details>x")
                    && !typed.contains("</video>x")
                    && !typed.contains("</audio>x")
                    && !typed.contains("</dialog>x")
                    && !typed.contains("</form>x")
                    && !typed.contains("</object>x")
                    && !typed.contains("</math>x")
                    && !typed.contains("</canvas>x")
                    && !typed.contains("</button>x")
                    && !typed.contains("</select>x")
                    && !typed.contains("</label>x")
                    && !typed.contains("</option>x")
                    && !typed.contains("<input type=\"text\">x")
                    && !typed.contains("clickx")
                    && !typed.contains("Namex")
                    && !typed.contains("</noscript>x")
                    && !typed.contains("</template>x")
                    && !typed.contains("</fieldset>x")
                    && !typed.contains("</legend>x")
                    && !typed.contains("</output>x")
                    && !typed.contains("</progress>x")
                    && !typed.contains("</meter>x")
                    && !typed.contains("42x")
                    && !typed.contains("70%x")
                    && !typed.contains("60%x")
                    && !typed.contains("slotx")
                    && !typed.contains("nestedx")
                    && !typed.contains("fallbackx")
                    && !typed.contains("raw textx")
                    && !typed.contains("bodyx")
                    && !typed.contains("hiddenx")
                    && !typed.contains("[TOC]x")
                    && !typed.contains("[[toc]]x")
                    && !typed.contains("[^1]: x")
                    && !typed.contains("[ref]: x")
                    && !typed.contains("|x")
                    && !typed.contains("x|")
                    && !typed.contains("|---|---|x")
                    && !typed.contains("| 1 |x"),
                "must not continue the last GFM construct, {source:?} got {typed:?}"
            );
            if source.contains('|') {
                assert!(
                    typed.contains('|'),
                    "table pipes must survive leftover click, {source:?} got {typed:?}"
                );
            }
        }
    }

    /// Nested / less-covered GFM leftovers: list-nested table/fence, tilde
    /// fence, indented code, alerts, TOC, HTML comment/PI, setext h2, lazy
    /// quote, ordered list, nested quote. Leftover click must open a paragraph.
    #[test]
    fn leftover_click_after_nested_gfm_appends_paragraph() {
        let cases = [
            "~~~\ncode\n~~~",
            "~~~\ncode\n~~~\n",
            "```rust\nfn x() {}\n```",
            "```\n```",
            "    indented",
            "    indented\n",
            "<!-- secret -->",
            "<!-- secret -->\n",
            "<?php echo 1; ?>",
            "<?php if ($a > $b) echo 1; ?>",
            "<![CDATA[a > b]]>",
            "> <?php if ($a > $b) echo 1; ?>",
            "- <![CDATA[a > b]]>",
            "<pre>**bold**</pre>",
            "> [!NOTE]\n> body",
            "> [!NOTE]\n> body\n",
            "[TOC]",
            "[[toc]]",
            "1. hello",
            "1) hello",
            "> > nested",
            "Title\n---",
            "Title\n---\n",
            "> hello\nworld",
            "- item\n  ```\n  code\n  ```",
            "- item\n\n  | a | b |\n  |---|---|\n  | 1 | 2 |",
            "- [x] done\n  ```\n  code\n  ```",
            "> ```\n> code\n> ```",
            "> # Title",
            "- # Title",
            "<svg></svg>",
            "```rust",
            "<style>body { color: red }</style>",
            "<textarea>hello</textarea>",
            "<script>alert(1)</script>",
            "<iframe src=\"https://e.com\"></iframe>",
            "<title>Doc title</title>",
            "<xmp>raw <b>html</b></xmp>",
            "<noembed>fallback</noembed>",
            "<noframes>fallback</noframes>",
            "- <iframe src=\"https://e.com\"></iframe>",
            "> <title>Doc title</title>",
            "<plaintext>raw text",
            "<details><summary>Title</summary>body</details>",
            "<details>\n<summary>Title</summary>\nhidden\n</details>",
            "- <details><summary>Title</summary>body</details>",
            "> <details><summary>Title</summary>body</details>",
            "<video src=\"x.mp4\"></video>",
            "<video>\nhello\n</video>",
            "<dialog>hello</dialog>",
            "<form action=\"/x\">ok</form>",
            "<object data=\"x\"></object>",
            "<math>x^2</math>",
            "<canvas>fallback</canvas>",
            "- <video src=\"x.mp4\"></video>",
            "> <dialog>hello</dialog>",
            "<button>click</button>",
            "<button>\nclick\n</button>",
            "- <button>click</button>",
            "> <button>click</button>",
            "<select><option>a</option></select>",
            "> <select><option>a</option></select>",
            "<input type=\"text\">",
            "- <input type=\"text\">",
            "<label>Name</label>",
            "<option>a</option>",
            "<noscript>fallback</noscript>",
            "<template><p>slot</p></template>",
            "- <noscript>fallback</noscript>",
            "> <template><p>slot</p></template>",
            "<fieldset><legend>Title</legend>body</fieldset>",
            "- <fieldset><legend>Title</legend>body</fieldset>",
            "> <fieldset><legend>Title</legend>body</fieldset>",
            "<legend>Title</legend>",
            "<output>42</output>",
            "- <output>42</output>",
            "> <output>42</output>",
            "<progress value=\"70\">70%</progress>",
            "<meter value=\"0.6\">60%</meter>",
        ];
        let mut failures = Vec::new();
        for source in cases {
            let typed = leftover_click_then_type(source);
            let ok_para = typed.lines().any(|line| line.trim() == "x");
            let glued = typed.contains("codex")
                || typed.contains("indentedx")
                || typed.contains("secretx")
                || typed.contains("-->x")
                || typed.contains("?>x")
                || typed.contains("]]>x")
                || typed.contains("</pre>x")
                || typed.contains("bodyx")
                || typed.contains("[TOC]x")
                || typed.contains("[[toc]]x")
                || typed.contains("hellox")
                || typed.contains("nestedx")
                || typed.contains("Titlex")
                || typed.contains("---x")
                || typed.contains("worldx")
                || typed.contains("| 2x")
                || typed.contains("|2x")
                || typed.contains("itemx")
                || typed.contains("donex")
                || typed.contains("</svg>x")
                || typed.contains("</style>x")
                || typed.contains("</textarea>x")
                || typed.contains("</script>x")
                || typed.contains("</iframe>x")
                || typed.contains("</title>x")
                || typed.contains("</xmp>x")
                || typed.contains("</noembed>x")
                || typed.contains("</noframes>x")
                || typed.contains("</plaintext>x")
                || typed.contains("</details>x")
                || typed.contains("</video>x")
                || typed.contains("</audio>x")
                || typed.contains("</dialog>x")
                || typed.contains("</form>x")
                || typed.contains("</object>x")
                || typed.contains("</math>x")
                || typed.contains("</canvas>x")
                || typed.contains("</button>x")
                || typed.contains("</select>x")
                || typed.contains("</label>x")
                || typed.contains("</option>x")
                || typed.contains("<input type=\"text\">x")
                || typed.contains("clickx")
                || typed.contains("Namex")
                || typed.contains("</noscript>x")
                || typed.contains("</template>x")
                || typed.contains("</fieldset>x")
                || typed.contains("</legend>x")
                || typed.contains("</output>x")
                || typed.contains("</progress>x")
                || typed.contains("</meter>x")
                || typed.contains("42x")
                || typed.contains("70%x")
                || typed.contains("60%x")
                || typed.contains("slotx")
                || typed.contains("nestedx")
                || typed.contains("fallbackx")
                || typed.contains("raw textx")
                || typed.contains("Doc titlex")
                || typed.contains("bodyx")
                || typed.contains("hiddenx")
                || typed.contains("```rustx")
                || typed.contains("rustx");
            if !ok_para || glued {
                failures.push(format!("{source:?} -> {typed:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "leftover click must open a new paragraph:\n{}",
            failures.join("\n")
        );
    }

    fn assert_not_glued_to_toc_marker(after: &str, source: &str) {
        assert_body_after_fence_newline(after, source);
        assert!(
            !after.contains("[TOC]x")
                && !after.contains("[TOC]#")
                && !after.contains("[TOC]-")
                && !after.contains("[TOC]*")
                && !after.contains("[TOC]`")
                && !after.contains("[[toc]]x")
                && !after.contains("[[toc]]#")
                && !after.contains("[[toc]]-"),
            "must not glue onto the TOC marker, {source:?} got {after:?}"
        );
        assert!(
            after.contains("[TOC]") || after.contains("[[toc]]"),
            "TOC marker must stay, {source:?} got {after:?}"
        );
    }

    /// `[TOC]` / `[[toc]]` at EOF (no following newline) shares the
    /// body-newline path so typing does not become `[TOC]x`.
    #[test]
    fn commands_at_eof_on_toc_marker_do_not_glue() {
        for source in ["[TOC]", "[[toc]]", "> [TOC]"] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_toc_marker(&after, source);
            assert!(
                after.lines().any(|line| line.trim() == "x"),
                "InsertText must land after the marker, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::SetBlockType(BlockType::Heading(1)),
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("H".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_toc_marker(&after, source);
            assert!(
                after.contains("# H"),
                "SetHeading must open after the marker, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_toc_marker(&after, source);
            assert!(
                after.contains("**x**"),
                "empty wrap must open after the marker, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("# Title".into()),
            );
            let after = doc.buffer.content();
            assert_not_glued_to_toc_marker(&after, source);
            assert!(
                after.contains("# Title"),
                "paste must land after the marker, {source:?} got {after:?}"
            );
        }
    }

    fn type_at_eof(source: &str) -> String {
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        )
    }

    fn eof_glued_without_newline(source: &str, after: &str) -> bool {
        after.len() > source.len()
            && after.starts_with(source)
            && after.as_bytes().get(source.len()) == Some(&b'x')
    }

    /// Last-block HTML widgets / comments / PI / void tags and an opening
    /// fence info-string at EOF must share the body-newline path (siblings of
    /// `</pre>` / `[TOC]` / fence-close ticks). Dest typing on `[ref]:` /
    /// footnote body / alert body / open ATX / markdown `![alt](url)` still
    /// extends in place. Leftover-below click (not EOF-on-`)`) is the new
    /// paragraph after a last-block markdown image.
    #[test]
    fn commands_at_eof_on_html_widget_and_fence_opener_do_not_glue() {
        let must_break = [
            "<!-- secret -->",
            "<!--\nsecret\n-->",
            "> <!-- secret -->",
            "- <!-- secret -->",
            "<?php echo 1; ?>",
            "<?php if ($a > $b) echo 1; ?>",
            "<![CDATA[a > b]]>",
            "> <?php if ($a > $b) echo 1; ?>",
            "- <![CDATA[a > b]]>",
            "<img src=\"a.png\">",
            "<img src=\"a.png\"/>",
            "<svg></svg>",
            "- <svg></svg>",
            "> <svg></svg>",
            "<hr/>",
            "<br>",
            "<br/>",
            "> <img src=\"a.png\">",
            "- <br>",
            "```rust",
            "```",
            "> ```rust",
            "> ```\n> code\n> ```",
            "<style>body { color: red }</style>",
            "<style>body > p { color: red }</style>",
            "<textarea>hello</textarea>",
            "<script>alert(1)</script>",
            "> <style>body { color: red }</style>",
            "- <script>alert(1)</script>",
            "<iframe src=\"https://e.com\"></iframe>",
            "<iframe src=\"https://e.com\"><p>nested</p></iframe>",
            "<title>Doc title</title>",
            "<xmp>raw <b>html</b></xmp>",
            "<noembed>fallback</noembed>",
            "<noframes>fallback</noframes>",
            "<plaintext>raw text",
            "- <iframe src=\"https://e.com\"></iframe>",
            "> <title>Doc title</title>",
            "<details><summary>Title</summary>body</details>",
            "<details>\n<summary>Title</summary>\nhidden\n</details>",
            "- <details><summary>Title</summary>body</details>",
            "> <details><summary>Title</summary>body</details>",
            "<video src=\"x.mp4\"></video>",
            "<video>\nhello\n</video>",
            "<audio src=\"x.mp3\"></audio>",
            "<dialog>hello</dialog>",
            "<form action=\"/x\">ok</form>",
            "<object data=\"x\"></object>",
            "<math>x^2</math>",
            "<canvas>fallback</canvas>",
            "- <video src=\"x.mp4\"></video>",
            "> <dialog>hello</dialog>",
            "<button>click</button>",
            "<button>\nclick\n</button>",
            "- <button>click</button>",
            "> <button>click</button>",
            "<select><option>a</option></select>",
            "> <select><option>a</option></select>",
            "<input type=\"text\">",
            "- <input type=\"text\">",
            "<label>Name</label>",
            "<option>a</option>",
            "<noscript>fallback</noscript>",
            "<template><p>slot</p></template>",
            "- <noscript>fallback</noscript>",
            "> <template><p>slot</p></template>",
            "<fieldset><legend>Title</legend>body</fieldset>",
            "<fieldset>\n<legend>Title</legend>\nhidden\n</fieldset>",
            "- <fieldset><legend>Title</legend>body</fieldset>",
            "> <fieldset><legend>Title</legend>body</fieldset>",
            "<legend>Title</legend>",
            "<output>42</output>",
            "- <output>42</output>",
            "> <output>42</output>",
            "<progress value=\"70\" max=\"100\">70%</progress>",
            "> <progress value=\"70\">70%</progress>",
            "<meter value=\"0.6\">60%</meter>",
            "- <meter value=\"0.6\">60%</meter>",
        ];
        let mut failures = Vec::new();
        for source in must_break {
            let after = type_at_eof(source);
            if eof_glued_without_newline(source, &after)
                || !after.contains('x')
                || !after.lines().any(|line| line.trim() == "x")
            {
                failures.push(format!("InsertText {source:?} -> {after:?}"));
            }
        }
        for source in ["<svg></svg>", "```rust", "> ```\n> code\n> ```"] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let after = doc.buffer.content();
            if after.contains("</svg>**")
                || after.contains("```rust*")
                || after.contains("```*")
                || !after.contains("**x**")
            {
                failures.push(format!("wrap {source:?} -> {after:?}"));
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::SetBlockType(BlockType::Heading(1)),
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("H".into()),
            );
            let after = doc.buffer.content();
            if after.contains("</svg>#") || after.contains("```rust#") || !after.contains("# H") {
                failures.push(format!("SetHeading {source:?} -> {after:?}"));
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.len());
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("# Title".into()),
            );
            let after = doc.buffer.content();
            if after.contains("</svg>#") || after.contains("```rust#") || !after.contains("# Title")
            {
                failures.push(format!("paste {source:?} -> {after:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "InsertText / wrap / SetHeading / paste at EOF must open a body line after last-block chrome:\n{}",
            failures.join("\n")
        );

        // Dest / title / paragraph body at EOF still extends (not leftover).
        for (source, glued) in [
            ("# Title", "Titlex"),
            ("hello", "hellox"),
            ("hello <svg></svg>", "</svg>x"),
            ("hello <button>click</button>", "</button>x"),
            ("hello <output>42</output>", "</output>x"),
            ("[ref]: https://e.com", "https://e.comx"),
            ("[^1]: the note", "the notex"),
            ("> [!NOTE]\n> body", "bodyx"),
            ("![alt](https://e.com/i.png)", ".png)x"),
        ] {
            let after = type_at_eof(source);
            assert!(
                after.contains(glued) && !after.contains(&format!("{source}\nx")),
                "in-place dest/body typing must stay, {source:?} got {after:?}"
            );
        }
    }

    #[test]
    fn html_anchor_dest_chrome_is_not_nibbled() {
        let source = "see <a href=\"https://e.com\">label</a> now\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("label").expect("label"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("<a href=\"https://e.com\">label</a>"),
            "Backspace at the start of an HTML link label must not nibble the tag, got {after:?}"
        );
        assert!(
            after.contains("see<a") || after.contains("see <a"),
            "expected the previous visible character to be deleted, got {after:?}"
        );
        assert!(
            !after.contains("see a href") && !after.contains("href=\"https://e.com\"label"),
            "broken leftover means `<` or `>` was eaten, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("label").unwrap() + "label".len());
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            after.contains("<a href=\"https://e.com\">label</a>"),
            "Delete at the end of an HTML link label must not swallow `</a>`, got {after:?}"
        );
    }

    #[test]
    fn html_phrasing_tag_dest_chrome_is_not_nibbled() {
        let source = "hello <b>bold</b>!\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("bold").expect("bold"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("<b>bold</b>"),
            "Backspace at the start of HTML bold must not nibble `<b>`, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("bold").unwrap() + "bold".len());
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            after.contains("<b>bold</b>"),
            "Delete at the end of HTML bold must not swallow `</b>`, got {after:?}"
        );
        assert!(
            after.contains('!'),
            "Delete must not jump dest chrome and eat the following `!`, got {after:?}"
        );

        for wrapped in ["> hello <b>bold</b>!\n", "- hello <b>bold</b>!\n"] {
            let (mut doc, mut engine, mut caret) = setup(wrapped);
            caret.collapse_to(wrapped.find("bold").expect("bold"));
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                after.contains("<b>bold</b>"),
                "quoted/list HTML dest chrome must stay, {wrapped:?} got {after:?}"
            );
        }

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("bold").expect("bold"));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteWordLeft,
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("<b>bold</b>"),
            "Option-Backspace at HTML bold must not nibble `<b>`, got {after:?}"
        );
    }

    #[test]
    fn html_comment_dest_chrome_is_not_nibbled() {
        let source = "hello <!-- x -->world\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("world").expect("world"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("<!-- x -->"),
            "Backspace after an HTML comment must not nibble `>`, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("hello").unwrap() + "hello".len());
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            after.contains("<!-- x -->"),
            "Delete before an HTML comment must not nibble `<`, got {after:?}"
        );
    }

    #[test]
    fn link_with_title_dest_chrome_is_not_nibbled() {
        let source = "see [label](https://e.com \"title\") now\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("label").expect("label"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("[label](https://e.com \"title\")"),
            "Backspace at a titled link must not nibble `[`, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("label").unwrap() + "label".len());
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            after.contains("[label](https://e.com \"title\")"),
            "Delete at the end of a titled link label must not swallow dest+title, got {after:?}"
        );
    }

    #[test]
    fn titled_link_dest_quote_is_not_nibbled() {
        for source in [
            "see [label](https://e.com \"title\") now\n",
            "see [label](https://e.com 'title') now\n",
            "see [label](https://e.com (title)) now\n",
            "> [label](https://e.com \"title\")\n",
            "- [label](https://e.com \"title\")\n",
            "see ![alt](a.png \"title\") now\n",
            "> ![alt](a.png \"title\")\n",
            "- ![alt](a.png \"title\")\n",
        ] {
            let title = source.find("title").expect("title");
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(title);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                after.contains("\"title\"")
                    || after.contains("'title'")
                    || after.contains("(title)"),
                "Backspace at titled dest must not nibble wrapping quotes, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(title + "title".len());
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                after.contains("\"title\"")
                    || after.contains("'title'")
                    || after.contains("(title)"),
                "Delete at titled dest must not nibble wrapping quotes, {source:?} got {after:?}"
            );

            let url = source
                .find("https://e.com")
                .or_else(|| source.find("a.png"))
                .expect("url");
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(url);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("label]https://")
                    && !after.contains("alt]a.png")
                    && !after.contains("label]<https://"),
                "Backspace at dest URL must not nibble `(` wrapping, {source:?} got {after:?}"
            );
            if !source.contains("![") {
                assert!(
                    after.contains("](") || after.contains("](<"),
                    "link dest `(` wrapping must stay, {source:?} got {after:?}"
                );
            }
        }
    }

    #[test]
    fn nested_list_enter_continues_at_the_same_indent() {
        let source = "- parent\n  - child";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("- parent") && after.contains("  - child"),
            "parent and child must stay, got {after:?}"
        );
        assert!(
            after.lines().any(|line| line == "  - " || line == "  -"),
            "Enter on a nested item must continue nested, got {after:?}"
        );
    }

    #[test]
    fn quoted_paragraph_enter_at_start_inserts_blank_quoted_line() {
        let source = "> hello";
        let (mut doc, mut engine, mut caret) = setup(source);
        let h = source.find('h').expect("h");
        caret.collapse_to(h);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        let lines: Vec<_> = after.lines().collect();
        assert!(
            lines.iter().any(|l| l.trim() == ">" || *l == "> "),
            "Enter at start of a quoted paragraph must insert a blank quoted line, got {after:?}"
        );
        assert!(
            after.contains("> hello") || after.contains(">hello"),
            "original quoted text must stay, got {after:?}"
        );
    }

    #[test]
    fn html_comment_block_is_one_caret_delete_step() {
        for source in [
            "hello\n\n<!-- secret -->\n\nworld\n",
            "<!-- secret -->\n",
            "- <!-- secret -->\n",
            "> <!-- secret -->\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let comment = first_html_comment_range(&engine, source);
            caret.collapse_to(comment.end.min(source.len()));
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<!--") && !after.contains("secret"),
                "Backspace after an HTML-block comment must delete the whole comment, {source:?} got {after:?}"
            );
            if source.contains("hello") {
                assert!(
                    after.contains("hello") && after.contains("world"),
                    "surrounding paragraphs must survive, got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            let comment = first_html_comment_range(&engine, source);
            caret.collapse_to(comment.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<!--") && !after.contains("secret"),
                "Delete before an HTML-block comment must delete the whole comment, {source:?} got {after:?}"
            );
        }
    }

    #[test]
    fn html_script_block_is_one_caret_delete_step() {
        for source in [
            "hello\n\n<script>alert(1)</script>\n\nworld\n",
            "<script>alert(1)</script>\n",
            "- <script>alert(1)</script>\n",
            "> <script>alert(1)</script>\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.end.min(source.len()));
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<script") && !after.contains("alert"),
                "Backspace after a script block must delete the whole widget, {source:?} got {after:?}"
            );
            if source.contains("hello") {
                assert!(
                    after.contains("hello") && after.contains("world"),
                    "surrounding paragraphs must survive, got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<script") && !after.contains("alert"),
                "Delete before a script block must delete the whole widget, {source:?} got {after:?}"
            );
        }
    }

    #[test]
    fn html_tagfilter_block_is_one_caret_delete_step() {
        for source in [
            "hello\n\n<iframe src=\"https://e.com\"></iframe>\n\nworld\n",
            "<iframe src=\"https://e.com\"><p>nested</p></iframe>\n",
            "- <iframe src=\"https://e.com\"></iframe>\n",
            "> <iframe src=\"https://e.com\"></iframe>\n",
            "hello\n\n<title>Doc title</title>\n\nworld\n",
            "<title>Doc title</title>\n",
            "- <title>Doc title</title>\n",
            "> <xmp>raw <b>html</b></xmp>\n",
            "<xmp>raw <b>html</b></xmp>\n",
            "<noembed>fallback</noembed>\n",
            "<noframes>fallback</noframes>\n",
            "<plaintext>raw text\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.end.min(source.len()));
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<iframe")
                    && !after.contains("<title")
                    && !after.contains("<xmp")
                    && !after.contains("<noembed")
                    && !after.contains("<noframes")
                    && !after.contains("<plaintext")
                    && !after.contains("nested")
                    && !after.contains("Doc title")
                    && !after.contains("fallback")
                    && !after.contains("raw text"),
                "Backspace after a tagfilter block must delete the whole widget, {source:?} got {after:?}"
            );
            if source.contains("hello") {
                assert!(
                    after.contains("hello") && after.contains("world"),
                    "surrounding paragraphs must survive, got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<iframe")
                    && !after.contains("<title")
                    && !after.contains("<xmp")
                    && !after.contains("<noembed")
                    && !after.contains("<noframes")
                    && !after.contains("<plaintext")
                    && !after.contains("nested")
                    && !after.contains("Doc title")
                    && !after.contains("fallback")
                    && !after.contains("raw text"),
                "Delete before a tagfilter block must delete the whole widget, {source:?} got {after:?}"
            );
        }
    }

    #[test]
    fn html_details_block_is_one_caret_delete_step() {
        for source in [
            "hello\n\n<details><summary>Title</summary>body</details>\n\nworld\n",
            "<details><summary>Title</summary>body</details>\n",
            "<details>\n<summary>Title</summary>\nhidden\n</details>\n",
            "- <details><summary>Title</summary>body</details>\n",
            "> <details><summary>Title</summary>body</details>\n",
            "> <details>\n> <summary>Title</summary>\n> hidden\n> </details>\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.end.min(source.len()));
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<details")
                    && !after.contains("<summary")
                    && !after.contains("Title")
                    && !after.contains("hidden"),
                "Backspace after a details block must delete the whole widget, {source:?} got {after:?}"
            );
            if source.contains("hello") {
                assert!(
                    after.contains("hello") && after.contains("world"),
                    "surrounding paragraphs must survive, got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<details")
                    && !after.contains("<summary")
                    && !after.contains("Title")
                    && !after.contains("hidden"),
                "Delete before a details block must delete the whole widget, {source:?} got {after:?}"
            );
        }
    }

    #[test]
    fn html_dangerous_block_is_one_caret_delete_step() {
        for source in [
            "hello\n\n<video src=\"x.mp4\"></video>\n\nworld\n",
            "<video src=\"x.mp4\"></video>\n",
            "<video>\nhello\n</video>\n",
            "- <video src=\"x.mp4\"></video>\n",
            "> <dialog>hello</dialog>\n",
            "<dialog>hello</dialog>\n",
            "<form action=\"/x\">ok</form>\n",
            "<object data=\"x\"></object>\n",
            "<math>x^2</math>\n",
            "<canvas>fallback</canvas>\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.end.min(source.len()));
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<video")
                    && !after.contains("<dialog")
                    && !after.contains("<form")
                    && !after.contains("<object")
                    && !after.contains("<math")
                    && !after.contains("<canvas")
                    && !after.contains("x.mp4")
                    && !after.contains("fallback")
                    && !after.contains("x^2"),
                "Backspace after dangerous HTML must delete the whole widget, {source:?} got {after:?}"
            );
            if source.contains("world") {
                assert!(
                    after.contains("hello") && after.contains("world"),
                    "surrounding paragraphs must survive, got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<video")
                    && !after.contains("<dialog")
                    && !after.contains("<form")
                    && !after.contains("<object")
                    && !after.contains("<math")
                    && !after.contains("<canvas")
                    && !after.contains("x.mp4")
                    && !after.contains("fallback")
                    && !after.contains("x^2"),
                "Delete before dangerous HTML must delete the whole widget, {source:?} got {after:?}"
            );
        }
    }

    #[test]
    fn html_form_control_block_is_one_caret_delete_step() {
        for source in [
            "hello\n\n<button>click</button>\n\nworld\n",
            "<button>click</button>\n",
            "<button>\nclick\n</button>\n",
            "- <button>click</button>\n",
            "> <button>click</button>\n",
            "hello <button>click</button>\n",
            "<select><option>a</option></select>\n",
            "> <select><option>a</option></select>\n",
            "<input type=\"text\">\n",
            "- <input type=\"text\">\n",
            "hello <input type=\"text\"> world\n",
            "<label>Name</label>\n",
            "<option>a</option>\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.end.min(source.len()));
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<button")
                    && !after.contains("<select")
                    && !after.contains("<input")
                    && !after.contains("<label")
                    && !after.contains("<option")
                    && !after.contains("click")
                    && !after.contains("Name"),
                "Backspace after a form control must delete the whole widget, {source:?} got {after:?}"
            );
            if source.contains("hello") {
                assert!(
                    after.contains("hello"),
                    "surrounding text must survive, got {after:?}"
                );
            }
            if source.contains("world") {
                assert!(
                    after.contains("world"),
                    "following text must survive, got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<button")
                    && !after.contains("<select")
                    && !after.contains("<input")
                    && !after.contains("<label")
                    && !after.contains("<option")
                    && !after.contains("click")
                    && !after.contains("Name"),
                "Delete before a form control must delete the whole widget, {source:?} got {after:?}"
            );
        }
    }

    #[test]
    fn html_noscript_and_template_are_one_caret_delete_step() {
        for source in [
            "hello\n\n<noscript>fallback</noscript>\n\nworld\n",
            "<noscript>fallback</noscript>\n",
            "- <noscript>fallback</noscript>\n",
            "> <noscript>fallback</noscript>\n",
            "<template><p>slot</p></template>\n",
            "- <template><p>slot</p></template>\n",
            "> <template><p>slot</p></template>\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.end.min(source.len()));
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<noscript")
                    && !after.contains("<template")
                    && !after.contains("fallback")
                    && !after.contains("slot"),
                "Backspace after noscript/template must delete the whole widget, {source:?} got {after:?}"
            );
            if source.contains("world") {
                assert!(
                    after.contains("hello") && after.contains("world"),
                    "surrounding paragraphs must survive, got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<noscript")
                    && !after.contains("<template")
                    && !after.contains("fallback")
                    && !after.contains("slot"),
                "Delete before noscript/template must delete the whole widget, {source:?} got {after:?}"
            );
        }
    }

    #[test]
    fn html_fieldset_legend_are_one_caret_delete_step() {
        for source in [
            "hello\n\n<fieldset><legend>Title</legend>body</fieldset>\n\nworld\n",
            "<fieldset><legend>Title</legend>body</fieldset>\n",
            "<fieldset>\n<legend>Title</legend>\nhidden\n</fieldset>\n",
            "- <fieldset><legend>Title</legend>body</fieldset>\n",
            "> <fieldset><legend>Title</legend>body</fieldset>\n",
            "> <fieldset>\n> <legend>Title</legend>\n> hidden\n> </fieldset>\n",
            "<legend>Title</legend>\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.end.min(source.len()));
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<fieldset")
                    && !after.contains("<legend")
                    && !after.contains("Title")
                    && !after.contains("hidden"),
                "Backspace after fieldset/legend must delete the whole widget, {source:?} got {after:?}"
            );
            if source.contains("hello") {
                assert!(
                    after.contains("hello") && after.contains("world"),
                    "surrounding paragraphs must survive, got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<fieldset")
                    && !after.contains("<legend")
                    && !after.contains("Title")
                    && !after.contains("hidden"),
                "Delete before fieldset/legend must delete the whole widget, {source:?} got {after:?}"
            );
        }
    }

    #[test]
    fn html_output_progress_meter_are_one_caret_delete_step() {
        for source in [
            "hello\n\n<output>42</output>\n\nworld\n",
            "<output>42</output>\n",
            "<output>\n42\n</output>\n",
            "- <output>42</output>\n",
            "> <output>42</output>\n",
            "hello <output>42</output>\n",
            "<progress value=\"70\" max=\"100\">70%</progress>\n",
            "> <progress value=\"70\">70%</progress>\n",
            "- <progress value=\"70\">70%</progress>\n",
            "<meter value=\"0.6\">60%</meter>\n",
            "hello <meter value=\"0.6\">60%</meter> world\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.end.min(source.len()));
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<output")
                    && !after.contains("<progress")
                    && !after.contains("<meter")
                    && !after.contains("42")
                    && !after.contains("70%")
                    && !after.contains("60%"),
                "Backspace after output/progress/meter must delete the whole widget, {source:?} got {after:?}"
            );
            if source.contains("hello") {
                assert!(
                    after.contains("hello"),
                    "surrounding text must survive, got {after:?}"
                );
            }
            if source.contains("world") {
                assert!(
                    after.contains("world"),
                    "following text must survive, got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_script_range(&engine, source);
            caret.collapse_to(chrome.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("<output")
                    && !after.contains("<progress")
                    && !after.contains("<meter")
                    && !after.contains("42")
                    && !after.contains("70%")
                    && !after.contains("60%"),
                "Delete before output/progress/meter must delete the whole widget, {source:?} got {after:?}"
            );
        }
    }

    #[test]
    fn html_style_and_textarea_tag_dest_chrome_is_not_nibbled() {
        for source in [
            "<style>body { color: red }</style>\n",
            "<style>body > p { color: red }</style>\n",
            "<textarea>hello</textarea>\n",
            "> <style>body { color: red }</style>\n",
            "- <textarea>hello</textarea>\n",
        ] {
            let word = if source.contains("textarea") {
                "hello"
            } else {
                "red"
            };
            let inner = source.find(word).expect("inner");
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(inner);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                after.contains("<style>") || after.contains("<textarea>"),
                "Backspace at Type-1 inner source must not nibble the open tag, {source:?} got {after:?}"
            );
            assert!(
                after.contains("</style>") || after.contains("</textarea>"),
                "close tag must survive Backspace, {source:?} got {after:?}"
            );

            let close = if source.contains("</style>") {
                "</style>"
            } else {
                "</textarea>"
            };
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(inner + word.len());
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                after.contains(close),
                "Delete at inner end must not swallow {close}, {source:?} got {after:?}"
            );
        }
    }

    fn first_html_script_range(engine: &RichEngine, source: &str) -> std::ops::Range<usize> {
        fn walk(blocks: &[Block]) -> Option<std::ops::Range<usize>> {
            for b in blocks {
                if let Some(r) = crate::rich::engine::html_block_script_range(b) {
                    return Some(r);
                }
                if let Some(r) = crate::rich::engine::tagfilter_inline_widget_ranges(b)
                    .into_iter()
                    .next()
                {
                    return Some(r);
                }
                if let Some(found) = walk(&b.children) {
                    return Some(found);
                }
            }
            None
        }
        walk(&engine.tree().blocks).unwrap_or_else(|| {
            let start = source.find("<script").expect("script");
            let end = source[start..]
                .find("</script>")
                .map(|rel| start + rel + "</script>".len())
                .unwrap_or(source.len());
            start..end
        })
    }

    #[test]
    fn html_pi_and_cdata_block_is_one_caret_delete_step() {
        for source in [
            "hello\n\n<?php if ($a > $b) echo 1; ?>\n\nworld\n",
            "<?php if ($a > $b) echo 1; ?>\n",
            "- <?php if ($a > $b) echo 1; ?>\n",
            "> <?php if ($a > $b) echo 1; ?>\n",
            "hello\n\n<![CDATA[a > b]]>\n\nworld\n",
            "<![CDATA[a > b]]>\n",
            "- <![CDATA[a > b]]>\n",
            "> <![CDATA[a > b]]>\n",
        ] {
            let needle = if source.contains("CDATA") {
                "<![CDATA["
            } else {
                "<?"
            };
            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_comment_range(&engine, source);
            assert!(
                source[chrome.clone()].contains(needle),
                "atomic range, {source:?} got {:?}",
                &source[chrome.clone()]
            );
            caret.collapse_to(chrome.end.min(source.len()));
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains(needle) && !after.contains("?>") && !after.contains("]]>"),
                "Backspace after PI/CDATA must delete the whole block, {source:?} got {after:?}"
            );
            if source.contains("hello") {
                assert!(
                    after.contains("hello") && after.contains("world"),
                    "surrounding paragraphs must survive, got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            let chrome = first_html_comment_range(&engine, source);
            caret.collapse_to(chrome.start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains(needle) && !after.contains("?>") && !after.contains("]]>"),
                "Delete before PI/CDATA must delete the whole block, {source:?} got {after:?}"
            );
        }

        let inline = "hello <?php echo 1 > 0; ?> world\n";
        let (mut doc, mut engine, mut caret) = setup(inline);
        caret.collapse_to(inline.find("world").expect("world"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("<?php echo 1 > 0; ?>"),
            "Backspace after an inline PI must not nibble `>`, got {after:?}"
        );
    }

    fn first_html_comment_range(engine: &RichEngine, source: &str) -> std::ops::Range<usize> {
        fn walk(blocks: &[Block]) -> Option<std::ops::Range<usize>> {
            for b in blocks {
                if let Some(r) = crate::rich::engine::html_block_comment_range(b) {
                    return Some(r);
                }
                if let Some(found) = walk(&b.children) {
                    return Some(found);
                }
            }
            None
        }
        walk(&engine.tree().blocks).unwrap_or_else(|| {
            let start = source.find("<!--").expect("comment");
            start..start + "<!-- secret -->".len()
        })
    }

    #[test]
    fn hard_break_edits_are_atomic() {
        for source in [
            "a  \nb\n",
            "a\\\nb\n",
            "> a  \n> b\n",
            "- a  \nb\n",
            "- a  \n  b\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let b = source.find('b').expect("b");
            caret.collapse_to(b);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("  \n") && !after.contains("\\\n"),
                "Backspace at the line after a hard break must remove the whole marker, {source:?} got {after:?}"
            );
            assert!(
                after.contains('a') && after.contains('b'),
                "surrounding text must survive, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("a  b")
                    && !after.contains("a \\b")
                    && !after.contains("a>")
                    && !after.contains("a-"),
                "must not nibble leftover chrome, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.find('a').expect("a") + 1);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("  \n") && !after.contains("\\\n"),
                "Delete after `a` must remove the whole hard break, {source:?} got {after:?}"
            );
            assert!(
                after.contains('a') && after.contains('b'),
                "surrounding text must survive Delete, {source:?} got {after:?}"
            );
        }
    }

    #[test]
    fn character_reference_edits_are_atomic() {
        let cases = [
            "A&amp;B\n",
            "A&lt;B\n",
            "A&gt;B\n",
            "A&quot;B\n",
            "A&#39;B\n",
            "A&#123;B\n",
            "A&#x7B;B\n",
            "> A&amp;B\n",
            "- A&amp;B\n",
            "[A&amp;B](https://e.com)\n",
            "| A&amp;B | x |\n| --- | --- |\n",
        ];
        for source in cases {
            let entity = source
                .find("&amp;")
                .or_else(|| source.find("&lt;"))
                .or_else(|| source.find("&gt;"))
                .or_else(|| source.find("&quot;"))
                .or_else(|| source.find("&#39;"))
                .or_else(|| source.find("&#123;"))
                .or_else(|| source.find("&#x7B;"))
                .expect("entity");
            let literal_end = entity + source[entity..].find(';').expect("entity semicolon") + 1;
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(literal_end);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains("&amp;")
                    && !after.contains("&lt;")
                    && !after.contains("&gt;")
                    && !after.contains("&quot;")
                    && !after.contains("&#39;")
                    && !after.contains("&#123;")
                    && !after.contains("&#x7B;"),
                "Backspace after an entity must remove the whole entity, {source:?} got {after:?}"
            );
            assert!(
                after.contains('A') && after.contains('B'),
                "surrounding text must survive Backspace, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("Aamp;B")
                    && !after.contains("Alt;B")
                    && !after.contains("Agt;B")
                    && !after.contains("Aquot;B"),
                "must not nibble entity dest chrome, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(entity);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains("&amp;")
                    && !after.contains("&lt;")
                    && !after.contains("&gt;")
                    && !after.contains("&quot;")
                    && !after.contains("&#39;")
                    && !after.contains("&#123;")
                    && !after.contains("&#x7B;"),
                "Delete on a painted entity must remove the whole entity, {source:?} got {after:?}"
            );
            assert!(
                after.contains('A') && after.contains('B'),
                "surrounding text must survive Delete, {source:?} got {after:?}"
            );
        }

        let code = "`A&amp;B`\n";
        let (mut doc, mut engine, mut caret) = setup(code);
        let b = code.find('B').expect("B");
        caret.collapse_to(b + 1);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("&amp;"),
            "Backspace in a code span must not swallow `&amp;`, got {after:?}"
        );
        assert!(
            after.contains("`A&amp;`") && !after.contains("`A&amp;B`"),
            "code Backspace removes the last inner character, got {after:?}"
        );
    }

    #[test]
    fn backslash_escape_is_one_delete_step() {
        for source in [
            "A\\*B\n",
            "> A\\*B\n",
            "- A\\*B\n",
            "[A\\*B](https://e.com)\n",
            "| A\\*B | x |\n| --- | --- |\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let b = source.find('B').expect("B");
            caret.collapse_to(b);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                !after.contains('\\') && !after.contains('*'),
                "Backspace on an escaped glyph must remove `\\*` as one step, {source:?} got {after:?}"
            );
            assert!(
                after.contains('A') && after.contains('B'),
                "surrounding text must survive, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            let slash = source.find('\\').expect("slash");
            caret.collapse_to(slash);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                !after.contains('\\') && !after.contains('*'),
                "Delete before an escaped glyph must remove `\\*` as one step, {source:?} got {after:?}"
            );
        }

        let source = "A\\*B\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let slash = source.find('\\').expect("slash");
        caret.collapse_to(slash);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let after = doc.buffer.content();
        assert_eq!(
            after, "Ax\\*B\n",
            "InsertText at the escape home must not un-escape, got {after:?}"
        );

        let code = "`A\\*B`\n";
        let (mut doc, mut engine, mut caret) = setup(code);
        let b = code.find('B').expect("B");
        caret.collapse_to(b + 1);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("\\*"),
            "Backspace in a code span must not swallow `\\*`, got {after:?}"
        );
    }

    /// Display `$$\nE=mc^2\n$$` wrapping newlines are dest chrome like `$` /
    /// `$$`. Backspace at the formula must not nibble `$` or the blank; Delete
    /// at the end must not swallow `\n$$`. Quoted / list forms match. Single-line
    /// `$$E=mc^2$$` is unchanged (already skipped).
    #[test]
    fn backspace_delete_do_not_nibble_multiline_display_math() {
        for source in [
            "see\n$$\nE=mc^2\n$$\nhere\n",
            "> $$\n> E=mc^2\n> $$\n",
            "- $$\n  E=mc^2\n  $$\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let e = source.find('E').expect("E");
            caret.collapse_to(e);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
            let after = doc.buffer.content();
            assert!(
                after.contains("$$") && after.contains("E=mc^2"),
                "Backspace at multiline display math must not nibble `$` / wrapping newline, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("$E=mc^2") && !after.contains("$$E=mc^2"),
                "broken leftover means `$` or `\\n` was eaten, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.find("mc^2").expect("formula") + "mc^2".len());
            apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
            let after = doc.buffer.content();
            assert!(
                after.contains("$$") && after.contains("E=mc^2"),
                "Delete at multiline display math end must not swallow `\\n$$`, {source:?} got {after:?}"
            );
        }

        let single = "see $$E=mc^2$$ here\n";
        let (mut doc, mut engine, mut caret) = setup(single);
        caret.collapse_to(single.find('E').expect("E"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("$$E=mc^2$$"),
            "single-line display math must still skip `$$`, got {after:?}"
        );
    }

    /// Cmd-K on a markdown image wraps the widget as `[![alt](url)]()`,
    /// like wrapping a word. Empty `[]()` in front (`[]()![alt](url)`) is
    /// wrong: `!` is dest chrome, not a word byte.
    #[test]
    fn cmd_k_on_image_wraps_the_widget_as_a_link() {
        for source in [
            "![alt](pic.png)",
            "![alt](pic.png)\n",
            "# ![alt](pic.png)",
            "- ![alt](pic.png)",
            "> ![alt](pic.png)",
            "see ![alt](pic.png) now",
            "| ![alt](pic.png) | b |\n|---|---|",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let bang = source.find('!').expect("image");
            caret.collapse_to(bang);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
            let after = doc.buffer.content();
            assert!(
                after.contains("[![alt](pic.png)]()"),
                "Cmd-K on an image must wrap it as a linked image, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("[]()!["),
                "must not insert an empty link in front of the image, {source:?} got {after:?}"
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("https://e.com".into()),
            );
            let filled = doc.buffer.content();
            assert!(
                filled.contains("[![alt](pic.png)](https://e.com)"),
                "typing after Cmd-K must fill the dest, {source:?} got {filled:?}"
            );
        }
    }

    #[test]
    fn cmd_k_on_selected_image_wraps_the_widget() {
        let source = "![alt](pic.png)\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let bang = source.find('!').expect("image");
        let end = source.find(')').expect("close") + 1;
        caret.range = bang..end;
        caret.reversed = false;
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let after = doc.buffer.content();
        assert!(
            after.contains("[![alt](pic.png)]()"),
            "Cmd-K on a selected image must wrap it, got {after:?}"
        );
        assert!(!after.contains("[]()!["));
    }

    #[test]
    fn cmd_k_on_linked_image_unwraps() {
        let source = "[![alt](pic.png)](https://e.com)\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let bang = source.find('!').expect("image");
        caret.collapse_to(bang);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let after = doc.buffer.content();
        assert!(
            after.contains("![alt](pic.png)") && !after.contains("[![alt](pic.png)]("),
            "second Cmd-K must unwrap the linked image, got {after:?}"
        );
        assert!(
            !after.contains("[]()!["),
            "unwrap must not splice an empty link, got {after:?}"
        );
    }

    /// Cmd-B/I/E on a markdown image wrap the widget (`**![alt](url)**`),
    /// not splice `****` in front of dest-chrome `!`.
    #[test]
    fn cmd_b_i_e_on_image_wrap_the_widget() {
        for source in [
            "![alt](pic.png)",
            "![alt](pic.png)\n",
            "# ![alt](pic.png)",
            "- ![alt](pic.png)",
            "> ![alt](pic.png)",
            "see ![alt](pic.png) now",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let bang = source.find('!').expect("image");
            caret.collapse_to(bang);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            let after = doc.buffer.content();
            assert!(
                after.contains("**![alt](pic.png)**"),
                "Cmd-B on an image must wrap it, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("****!"),
                "must not splice empty bold in front of the image, {source:?} got {after:?}"
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            let unwrapped = doc.buffer.content();
            assert!(
                unwrapped.contains("![alt](pic.png)") && !unwrapped.contains("**![alt](pic.png)**"),
                "second Cmd-B must unwrap, {source:?} got {unwrapped:?}"
            );
        }

        let source = "![alt](pic.png)\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('!').expect("image"));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::ITALIC),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("*![alt](pic.png)*") && !after.contains("**!"),
            "Cmd-I on an image must wrap it, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('!').expect("image"));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::CODE),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("`![alt](pic.png)`") && !after.contains("``!"),
            "Cmd-E on an image must wrap it in ticks, got {after:?}"
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::CODE),
        );
        let unwrapped = doc.buffer.content();
        assert!(
            unwrapped.contains("![alt](pic.png)") && !unwrapped.contains("`![alt](pic.png)`"),
            "second Cmd-E must unwrap ticks, got {unwrapped:?}"
        );
    }

    #[test]
    fn cmd_k_on_html_img_and_svg_wraps_the_widget() {
        for source in [
            "<img src=\"a.png\">",
            "<img src=\"a.png\">\n",
            "see <img src=\"a.png\"> now",
            "- <img src=\"a.png\">",
            "> <img src=\"a.png\">",
            "<svg></svg>",
            "see <svg></svg> now",
            "- <svg></svg>",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let tag = source.find('<').expect("tag");
            caret.collapse_to(tag);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
            let after = doc.buffer.content();
            let wrapped_img = after.contains("[<img src=\"a.png\">]()");
            let wrapped_svg = after.contains("[<svg></svg>]()");
            assert!(
                wrapped_img || wrapped_svg,
                "Cmd-K on HTML img/svg must wrap the widget, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("[]()<img") && !after.contains("[]()<svg"),
                "must not splice an empty link in front, {source:?} got {after:?}"
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("https://e.com".into()),
            );
            let filled = doc.buffer.content();
            assert!(
                filled.contains("](https://e.com)"),
                "typing after Cmd-K must fill dest, {source:?} got {filled:?}"
            );
        }

        let source = "<img src=\"a.png\">\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('<').expect("tag"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let wrapped = doc.buffer.content();
        caret.collapse_to(wrapped.find('<').expect("wrapped tag"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let after = doc.buffer.content();
        assert!(
            after.contains("<img src=\"a.png\">") && !after.contains("[<img"),
            "second Cmd-K must unwrap HTML img, got {after:?}"
        );
    }

    #[test]
    fn cmd_k_on_thematic_break_wraps_the_rule() {
        for source in [
            "---",
            "---\n",
            "***",
            "* * *",
            "___",
            "<hr>",
            "<hr/>",
            "> ---",
            "- <hr>",
            "hello\n\n---\n\nworld\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let at = source
                .find("<hr")
                .or_else(|| source.find("---"))
                .or_else(|| source.find("***"))
                .or_else(|| source.find("* * *"))
                .or_else(|| source.find("___"))
                .expect("rule");
            caret.collapse_to(at);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
            let after = doc.buffer.content();
            let wrapped = after.contains("[---]()")
                || after.contains("[***]()")
                || after.contains("[* * *]()")
                || after.contains("[___]()")
                || after.contains("[<hr>]()")
                || after.contains("[<hr/>]()");
            assert!(
                wrapped,
                "Cmd-K on a thematic rule must wrap the widget, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("[]()---")
                    && !after.contains("[]()***")
                    && !after.contains("[]()<hr"),
                "must not splice an empty link in front of the rule, {source:?} got {after:?}"
            );
        }

        let source = "***\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('*').expect("star"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let wrapped = doc.buffer.content();
        caret.collapse_to(wrapped.find('*').expect("wrapped star"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let after = doc.buffer.content();
        assert!(
            after.contains("***") && !after.contains("[***]("),
            "second Cmd-K must unwrap a wrapped thematic rule, got {after:?}"
        );
    }

    #[test]
    fn cmd_b_on_html_img_wraps_the_widget() {
        let source = "see <img src=\"a.png\"> now\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('<').expect("tag"));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("**<img src=\"a.png\">**"),
            "Cmd-B on HTML img must wrap the widget, got {after:?}"
        );
        assert!(
            !after.contains("****<"),
            "must not splice empty bold in front, got {after:?}"
        );
    }

    /// CommonMark `&amp;` is dest chrome (`&` is not a word byte), same splice
    /// as `!` on an image. Empty-caret wrap must wrap the entity.
    #[test]
    fn cmd_b_and_k_on_character_reference_wrap_the_entity() {
        let source = "A&amp;B\n";
        let entity = source.find("&amp;").expect("entity");
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(entity);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("**&amp;**") || after.contains("**&**"),
            "Cmd-B on an entity must wrap it, got {after:?}"
        );
        assert!(
            !after.contains("****&"),
            "must not splice empty bold in front of the entity, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(entity);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let after = doc.buffer.content();
        assert!(
            after.contains("[&amp;]()") || after.contains("[&]()"),
            "Cmd-K on an entity must wrap it, got {after:?}"
        );
        assert!(
            !after.contains("[]()&"),
            "must not splice an empty link in front of the entity, got {after:?}"
        );
    }

    #[test]
    fn leftover_cmd_k_after_last_block_image_opens_empty_link() {
        for source in [
            "![alt](pic.png)",
            "![alt](pic.png)\n",
            "<img src=\"a.png\">",
            "<br>",
            "<br>\n",
            "Hello[^1]",
            "a  \nb",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            place_caret_for_click_below(&mut doc, &mut engine, &mut caret);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
            let after = doc.buffer.content();
            assert!(
                after.contains("[]()"),
                "leftover Cmd-K must open an empty link, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("[![alt](pic.png)]()")
                    && !after.contains("[<img")
                    && !after.contains("[<br")
                    && !after.contains("[[^1]]()")
                    && !after.contains("[  \n]()"),
                "leftover Cmd-K must not wrap the last-block widget, {source:?} got {after:?}"
            );
        }
    }

    fn first_hard_break_range(engine: &RichEngine) -> std::ops::Range<usize> {
        fn walk(blocks: &[Block]) -> Option<std::ops::Range<usize>> {
            for b in blocks {
                for inline in &b.inlines {
                    if let Inline::HardBreak { source_range, .. } = inline {
                        return Some(source_range.clone());
                    }
                }
                if let Some(r) = walk(&b.children) {
                    return Some(r);
                }
            }
            None
        }
        walk(&engine.tree().blocks).expect("hard break")
    }

    /// Empty-caret wrap on `<br>` / `<br/>` must wrap the widget, not splice
    /// `****` / `[]()` in front of dest-chrome `<`.
    #[test]
    fn cmd_b_i_e_k_on_html_break_wrap_the_widget() {
        for source in [
            "<br>",
            "<br>\n",
            "<br/>",
            "<br />\n",
            "a<br>b",
            "see <br> now",
            "- <br>",
            "> <br>",
            "- a<br>b",
            "> a<br>b",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let br = first_html_break_range(&engine);
            caret.collapse_to(br.start);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            let after = doc.buffer.content();
            let wrapped = after.contains("**<br>**")
                || after.contains("**<br/>**")
                || after.contains("**<br />**");
            assert!(
                wrapped,
                "Cmd-B on `<br>` must wrap the widget, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("****<"),
                "must not splice empty bold in front of `<br>`, {source:?} got {after:?}"
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            let unwrapped = doc.buffer.content();
            assert!(
                (unwrapped.contains("<br>")
                    || unwrapped.contains("<br/>")
                    || unwrapped.contains("<br />"))
                    && !unwrapped.contains("**<br"),
                "second Cmd-B must unwrap `<br>`, {source:?} got {unwrapped:?}"
            );
        }

        let source = "a<br>b\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(first_html_break_range(&engine).start);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::ITALIC),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("*<br>*") && !after.contains("**<br"),
            "Cmd-I on `<br>` must wrap the widget, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(first_html_break_range(&engine).start);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::CODE),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("`<br>`") && !after.contains("``<"),
            "Cmd-E on `<br>` must wrap in ticks, got {after:?}"
        );
        caret.collapse_to(after.find("<br>").expect("wrapped br"));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::CODE),
        );
        let unwrapped = doc.buffer.content();
        assert!(
            unwrapped.contains("<br>") && !unwrapped.contains("`<br>`"),
            "second Cmd-E must unwrap ticks around `<br>`, got {unwrapped:?}"
        );

        for source in ["<br>\n", "a<br>b\n", "- <br>\n", "> <br>\n"] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(first_html_break_range(&engine).start);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
            let after = doc.buffer.content();
            assert!(
                after.contains("[<br>]()")
                    || after.contains("[<br/>]()")
                    || after.contains("[<br />]()"),
                "Cmd-K on `<br>` must wrap the widget, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("[]()<br") && !after.contains("[]()<"),
                "must not splice an empty link in front of `<br>`, {source:?} got {after:?}"
            );
        }

        let source = "<br>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(first_html_break_range(&engine).start);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let wrapped = doc.buffer.content();
        caret.collapse_to(wrapped.find("<br>").expect("wrapped br"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let after = doc.buffer.content();
        assert!(
            after.contains("<br>") && !after.contains("[<br>"),
            "second Cmd-K must unwrap `<br>`, got {after:?}"
        );
    }

    /// Two-space / backslash hard-break markers are dest chrome like `<br>`.
    /// Empty-caret wrap must wrap the break, not splice `****` / `[]()` in front.
    #[test]
    fn cmd_b_and_k_on_hard_break_wrap_the_break() {
        for source in ["a  \nb\n", "a\\\nb\n", "> a  \n> b\n", "- a  \n  b\n"] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let hard = first_hard_break_range(&engine);
            caret.collapse_to(hard.start);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            let after = doc.buffer.content();
            let wrapped = after.contains("**  \n**") || after.contains("**\\\n**");
            assert!(
                wrapped,
                "Cmd-B on a hard break must wrap the marker, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("****  ") && !after.contains("****\\"),
                "must not splice empty bold in front of the hard break, {source:?} got {after:?}"
            );
            caret.collapse_to(first_hard_break_range(&engine).start);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            let unwrapped = doc.buffer.content();
            assert!(
                !unwrapped.contains("**  \n**") && !unwrapped.contains("**\\\n**"),
                "second Cmd-B must unwrap the hard break, {source:?} got {unwrapped:?}"
            );
        }

        let source = "a  \nb\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(first_hard_break_range(&engine).start);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let after = doc.buffer.content();
        assert!(
            after.contains("[  \n]()"),
            "Cmd-K on a two-space hard break must wrap the marker, got {after:?}"
        );
        assert!(
            !after.contains("[]()  "),
            "must not splice an empty link in front of the hard break, got {after:?}"
        );
        caret.collapse_to(first_hard_break_range(&engine).start);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let unwrapped = doc.buffer.content();
        assert!(
            unwrapped.contains("a  \nb") && !unwrapped.contains("[  \n]("),
            "second Cmd-K must unwrap the hard break, got {unwrapped:?}"
        );

        let source = "a\\\nb\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(first_hard_break_range(&engine).start);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let after = doc.buffer.content();
        assert!(
            after.contains("[\\\n]()"),
            "Cmd-K on a backslash hard break must wrap the marker, got {after:?}"
        );
        assert!(
            !after.contains("[]()\\"),
            "must not splice an empty link in front of `\\`, got {after:?}"
        );
    }

    /// Footnote refs skip `[` / `^` / `]` as dest chrome. Empty-caret wrap
    /// must wrap `[^1]`, not splice `****` / `[]()` before the opener.
    #[test]
    fn cmd_b_i_e_k_on_footnote_ref_wrap_the_widget() {
        for source in [
            "Hello[^1] world\n",
            "Hello[^note]\n",
            "- Hello[^1]\n",
            "> Hello[^1]\n",
            "see [^1] now\n",
            "Hello[^1]\n\n[^1]: the note\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let r = first_footnote_ref_range(&engine);
            caret.collapse_to(r.start);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            let after = doc.buffer.content();
            let wrapped = after.contains("**[^1]**") || after.contains("**[^note]**");
            assert!(
                wrapped,
                "Cmd-B on a footnote ref must wrap the widget, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("****[") && !after.contains("****[^"),
                "must not splice empty bold in front of `[^`, {source:?} got {after:?}"
            );
            caret.collapse_to(first_footnote_ref_range(&engine).start);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            let unwrapped = doc.buffer.content();
            assert!(
                (unwrapped.contains("[^1]") || unwrapped.contains("[^note]"))
                    && !unwrapped.contains("**[^"),
                "second Cmd-B must unwrap the footnote ref, {source:?} got {unwrapped:?}"
            );
        }

        let source = "Hello[^1] world\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(first_footnote_ref_range(&engine).start);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::ITALIC),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("*[^1]*") && !after.contains("**[^"),
            "Cmd-I on a footnote ref must wrap the widget, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(first_footnote_ref_range(&engine).start);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::CODE),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("`[^1]`") && !after.contains("``["),
            "Cmd-E on a footnote ref must wrap in ticks, got {after:?}"
        );
        caret.collapse_to(after.find("[^1]").expect("wrapped ref"));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::CODE),
        );
        let unwrapped = doc.buffer.content();
        assert!(
            unwrapped.contains("[^1]") && !unwrapped.contains("`[^1]`"),
            "second Cmd-E must unwrap ticks around the footnote ref, got {unwrapped:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(first_footnote_ref_range(&engine).start);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let after = doc.buffer.content();
        assert!(
            after.contains("[[^1]]()"),
            "Cmd-K on a footnote ref must wrap the widget, got {after:?}"
        );
        assert!(
            !after.contains("[]()[^"),
            "must not splice an empty link in front of the footnote ref, got {after:?}"
        );
        caret.collapse_to(after.find("[^1]").expect("wrapped ref"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let unwrapped = doc.buffer.content();
        assert!(
            unwrapped.contains("[^1]") && !unwrapped.contains("[[^1]]("),
            "second Cmd-K must unwrap the footnote ref, got {unwrapped:?}"
        );
    }

    /// Wrap at a footnote-def opener must not splice `****` / `[]()` in front
    /// of `[^1]:` (prefix chrome). The pair lands in the body.
    #[test]
    fn cmd_b_and_k_on_footnote_def_opener_do_not_splice_in_front() {
        for source in [
            "Hello[^1]\n\n[^1]: the note\n",
            "> Hello[^1]\n\n> [^1]: the note\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let at = source.rfind("[^1]:").expect("def");
            caret.collapse_to(at);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            let after = doc.buffer.content();
            assert!(
                after.contains("[^1]:") && !after.contains("****[^1]:") && !after.contains("****>"),
                "Cmd-B on a footnote-def opener must not splice in front, {source:?} got {after:?}"
            );
            assert!(
                after.contains("****") || after.contains("**the note**") || after.contains("**"),
                "Cmd-B on a footnote-def opener still wraps in the body, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(at);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
            let after = doc.buffer.content();
            assert!(
                after.contains("[^1]:") && !after.contains("[]()[^1]:"),
                "Cmd-K on a footnote-def opener must not splice in front, {source:?} got {after:?}"
            );
        }
    }

    /// Cmd-K from inside empty dest `()` of `[widget]()` is the dest home
    /// (typing fills the URL). Do not wrap the inner widget from dest.
    #[test]
    fn cmd_k_in_empty_dest_of_wrapped_widget_stays_in_dest() {
        let source = "[<br>]()\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let dest = source.find("]()").expect("dest") + 2;
        caret.collapse_to(dest);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("https://e.com".into()),
        );
        let filled = doc.buffer.content();
        assert!(
            filled.contains("[<br>](https://e.com)"),
            "typing into empty dest must fill the URL, got {filled:?}"
        );
    }

    /// GFM `<https://…>` / `<user@host>` skip `<>` as dest chrome. Empty wrap
    /// on those brackets must wrap the autolink, not splice `****` / `[]()`.
    #[test]
    fn cmd_b_and_k_on_angle_autolink_wrap_the_widget() {
        for source in [
            "<https://e.com>\n",
            "see <https://e.com> now\n",
            "- <https://e.com>\n",
            "> <https://e.com>\n",
            "<user@example.com>\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let open = source.find('<').expect("autolink");
            caret.collapse_to(open);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            let after = doc.buffer.content();
            let wrapped =
                after.contains("**<https://e.com>**") || after.contains("**<user@example.com>**");
            assert!(
                wrapped,
                "Cmd-B on autolink `<>` must wrap the widget, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("****<"),
                "must not splice empty bold in front of autolink `<>`, {source:?} got {after:?}"
            );
            caret.collapse_to(after.find('<').expect("wrapped"));
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            let unwrapped = doc.buffer.content();
            assert!(
                (unwrapped.contains("<https://e.com>") || unwrapped.contains("<user@example.com>"))
                    && !unwrapped.contains("**<"),
                "second Cmd-B must unwrap the autolink, {source:?} got {unwrapped:?}"
            );
        }

        let source = "<https://e.com>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('<').expect("autolink"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let after = doc.buffer.content();
        assert!(
            !after.contains("[]()<"),
            "Cmd-K must not splice an empty link in front of the autolink, got {after:?}"
        );
        assert!(
            after.contains("[<https://e.com>]()")
                || after.contains("https://e.com") && !after.contains("<https://e.com>"),
            "Cmd-K on an autolink wraps it as a link or toggles dest chrome, got {after:?}"
        );
    }

    #[test]
    fn empty_cmd_b_inside_autolink_url_still_inserts_pair() {
        let source = "<https://e.com>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("e.com").expect("url"));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("****"),
            "empty Cmd-B inside the URL must insert a pair, got {after:?}"
        );
        assert!(
            !after.contains("**<https://e.com>**"),
            "must not wrap the whole autolink from an inner caret, got {after:?}"
        );
    }

    /// GFM task `[x]` / `[ ]` is prefix chrome. Empty wrap must sit in the
    /// body (`- [ ] **x**`), not splice `****[x]` / `[]()[x]` or wrap the
    /// checkbox as a mark.
    #[test]
    fn empty_wrap_on_task_checkbox_sits_in_the_body() {
        for source in [
            "- [x] done\n",
            "- [ ] done\n",
            "- [X] done\n",
            "* [x] done\n",
            "+ [ ] done\n",
            "1. [x] done\n",
            "> - [ ] done\n",
            "- [ ] \n",
        ] {
            let box_at = source.find('[').expect("checkbox");
            for at in [box_at, box_at + 1, box_at + 2] {
                let (mut doc, mut engine, mut caret) = setup(source);
                caret.collapse_to(at);
                apply(
                    &mut doc,
                    &mut engine,
                    &mut caret,
                    RichCommand::ToggleMark(MarkSet::BOLD),
                );
                apply(
                    &mut doc,
                    &mut engine,
                    &mut caret,
                    RichCommand::InsertText("x".into()),
                );
                let after = doc.buffer.content();
                assert!(
                    !after.contains("****[")
                        && !after.contains("**x**[")
                        && !after.contains("**[x]**")
                        && !after.contains("**[ ]**")
                        && !after.contains("**[X]**"),
                    "Cmd-B must not wrap or splice onto the checkbox, {source:?} at {at} got {after:?}"
                );
                assert!(
                    after.contains("[x]") || after.contains("[ ]") || after.contains("[X]"),
                    "checkbox must stay a task marker, {source:?} got {after:?}"
                );
                assert!(
                    after.contains("**x**"),
                    "empty wrap then type must sit in the body, {source:?} got {after:?}"
                );
            }

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(box_at);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
            let after = doc.buffer.content();
            assert!(
                !after.contains("[]()[") && !after.contains("[]() ["),
                "Cmd-K must not splice an empty link in front of the checkbox, {source:?} got {after:?}"
            );
            assert!(
                after.contains("[x]") || after.contains("[ ]") || after.contains("[X]"),
                "checkbox must stay, {source:?} got {after:?}"
            );
        }

        let source = "- [x](https://e.com)\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('[').expect("link"));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("z".into()),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("[x](https://e.com)") || after.contains("[**z**x](https://e.com)"),
            "list-item link `[x](url)` is not a task checkbox, got {after:?}"
        );
        assert!(
            !after.contains("- [x] **z**"),
            "must not skip `[x](url)` as a task checkbox, got {after:?}"
        );

        let source = "- [x] done\n";
        let typed = leftover_click_then_type(source);
        assert!(
            typed.contains("- [x] done") && typed.lines().any(|line| line.trim() == "x"),
            "leftover click below a task must open a paragraph, got {typed:?}"
        );
        let (mut doc, mut engine, mut caret) = setup(source);
        place_caret_for_click_below(&mut doc, &mut engine, &mut caret);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let after = doc.buffer.content();
        assert!(
            after.contains("[]()") && after.contains("- [x] done"),
            "leftover Cmd-K below a task must open [](), got {after:?}"
        );
        assert!(
            !after.contains("[]()[x]"),
            "leftover Cmd-K must not wrap the checkbox, got {after:?}"
        );
    }

    /// Empty wrap on `~~strike~~` opener skips onto inner text like `**bold**`.
    #[test]
    fn empty_wrap_on_strikethrough_opener_skips_onto_inner() {
        for source in [
            "~~strike~~\n",
            "see ~~strike~~ now\n",
            "- ~~strike~~\n",
            "> ~~strike~~\n",
        ] {
            let at = source.find("~~").expect("strike");
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(at);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let after = doc.buffer.content();
            assert!(
                !after.contains("****~~") && !after.contains("**x**~~"),
                "Cmd-B on `~~` must not splice in front, {source:?} got {after:?}"
            );
            assert!(
                after.contains("~~**x**strike~~") || after.contains("**~~strike~~**"),
                "empty wrap on strike opener must wrap inner or the span, {source:?} got {after:?}"
            );

            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(at);
            apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
            let after = doc.buffer.content();
            assert!(
                !after.contains("[]()~~"),
                "Cmd-K on `~~` must not splice in front, {source:?} got {after:?}"
            );
            assert!(
                after.contains("~~[strike]()~~") || after.contains("[~~strike~~]()"),
                "Cmd-K on strike opener must wrap inner or the span, {source:?} got {after:?}"
            );
        }
    }

    /// Keyboard End / Cmd-Delete-to-line-end on a last-in-line markdown
    /// link stay on the label. Hidden dest is not revealed or eaten.
    #[test]
    fn line_end_from_link_label_does_not_reveal_or_eat_dest() {
        for (source, label) in [
            ("[label](https://e.com)\n", "label"),
            ("[label](https://e.com \"title\")\n", "label"),
            ("> [label](https://e.com)\n", "label"),
            ("- [label](https://e.com)\n", "label"),
            ("# [label](https://e.com)\n", "label"),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let at = source.find(label).expect(label);
            let end = engine.line_end_caret(source, at);
            assert_eq!(
                source.as_bytes().get(end).copied().unwrap_or(0) as char,
                ']',
                "End from {label:?} sat in dest, {source:?} at {end}"
            );
            caret.collapse_to(end);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            assert!(
                typed.contains(&format!("[{label}x]")) && typed.contains("https://e.com"),
                "typing at End must extend the label, {source:?} got {typed:?}"
            );
            assert!(
                !typed.contains("https://e.comx") && !typed.contains("titlex"),
                "typing at End must not extend dest, {source:?} got {typed:?}"
            );
        }

        let source = "[label](https://e.com)\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("label").expect("label"));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteToLineEnd,
        );
        assert!(
            after.contains("](https://e.com)"),
            "DeleteToLineEnd from the label must keep dest wrapping, got {after:?}"
        );
        assert!(
            !after.contains("[label]"),
            "DeleteToLineEnd from the label must delete the label text, got {after:?}"
        );
    }

    /// Keyboard End / Cmd-Delete-to-line-end on a last-in-line HTML
    /// `<a href>label</a>` stay on the label (before `</a>`). Hidden href
    /// dest is not revealed or eaten.
    #[test]
    fn line_end_from_html_anchor_label_does_not_reveal_or_eat_href() {
        for (source, label) in [
            ("<a href=\"https://e.com\">label</a>\n", "label"),
            ("<a href=\"https://e.com\" title=\"t\">label</a>\n", "label"),
            ("> <a href=\"https://e.com\">label</a>\n", "label"),
            ("- <a href=\"https://e.com\">label</a>\n", "label"),
            ("# <a href=\"https://e.com\">label</a>\n", "label"),
            ("<b>bold</b>\n", "bold"),
            ("hello<!-- x -->\n", "hello"),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let at = source.find(label).expect(label);
            let end = engine.line_end_caret(source, at);
            assert!(
                source[end..].starts_with('<'),
                "End from {label:?} sat in dest/href, {source:?} at {end}"
            );
            caret.collapse_to(end);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            assert!(
                typed.contains(&format!("{label}x<")),
                "typing at End must extend the inner text before HTML dest, {source:?} got {typed:?}"
            );
            assert!(
                !typed.contains("https://e.comx")
                    && !typed.contains("hrefx")
                    && !typed.contains("titlex"),
                "typing at End must not extend dest/href, {source:?} got {typed:?}"
            );
        }

        let source = "<a href=\"https://e.com\">label</a>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("label").expect("label"));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteToLineEnd,
        );
        assert!(
            after.contains("<a href=\"https://e.com\">") && after.contains("</a>"),
            "DeleteToLineEnd from the label must keep HTML dest wrapping, got {after:?}"
        );
        assert!(
            !after.contains(">label<"),
            "DeleteToLineEnd from the label must delete the label text, got {after:?}"
        );
    }

    /// Keyboard End / Cmd-Delete-to-line-end on last-in-line `&amp;` / `\*`
    /// stay after the painted glyph. Hidden `amp;` is not revealed or eaten.
    #[test]
    fn line_end_from_entity_does_not_sit_in_dest_chrome() {
        for (source, from, literal) in [
            ("A&amp;\n", "A", "&amp;"),
            ("A&lt;\n", "A", "&lt;"),
            ("A&#123;\n", "A", "&#123;"),
            ("A&#x7B;\n", "A", "&#x7B;"),
            ("> A&amp;\n", "A", "&amp;"),
            ("- A&amp;\n", "A", "&amp;"),
            ("# A&amp;\n", "A", "&amp;"),
            ("A\\*\n", "A", "\\*"),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let at = source.find(from).expect(from);
            let end = engine.line_end_caret(source, at);
            let home = source.find(literal).expect(literal) + literal.len();
            assert_eq!(
                end, home,
                "End from {from:?} sat in dest chrome, {source:?} at {end}"
            );
            caret.collapse_to(end);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            assert!(
                typed.contains(&format!("{literal}x")) || typed.contains("\\*x"),
                "typing at End must insert after the glyph, {source:?} got {typed:?}"
            );
            assert!(
                !typed.contains("&xamp;")
                    && !typed.contains("&xlt;")
                    && !typed.contains("&#x123;")
                    && !typed.contains("\\x*"),
                "typing at End must not splice dest chrome, {source:?} got {typed:?}"
            );
        }

        let source = "A&amp;\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('A').expect("A"));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteToLineEnd,
        );
        assert!(
            !after.contains("&amp;") && !after.contains("amp;"),
            "DeleteToLineEnd from A must remove the entity, got {after:?}"
        );
        assert!(
            !after.contains("Aamp;") && !after.contains("A&"),
            "DeleteToLineEnd must not nibble dest chrome, got {after:?}"
        );
    }

    /// Keyboard End / Cmd-Delete-to-line-end on last-in-line `[^1]` stay
    /// after the widget. Typing extends after `]`, not `Hello[^x1]`.
    #[test]
    fn line_end_from_footnote_does_not_sit_on_label_start() {
        for (source, from, raw) in [
            ("Hello[^1]\n\n[^1]: note\n", "Hello", "[^1]"),
            ("Hello[^1]\n", "Hello", "[^1]"),
            ("Hello[^note]\n", "Hello", "[^note]"),
            ("> Hello[^1]\n\n[^1]: note\n", "Hello", "[^1]"),
            ("- Hello[^1]\n\n[^1]: note\n", "Hello", "[^1]"),
            ("# Hello[^1]\n\n[^1]: note\n", "Hello", "[^1]"),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let at = source.find(from).expect(from);
            let end = engine.line_end_caret(source, at);
            let home = source.find(raw).expect(raw) + raw.len();
            assert_eq!(
                end, home,
                "End from {from:?} sat on the footnote label, {source:?} at {end}"
            );
            caret.collapse_to(end);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            assert!(
                typed.contains(&format!("{raw}x")),
                "typing at End must insert after the widget, {source:?} got {typed:?}"
            );
            assert!(
                !typed.contains("[^x") && !typed.contains(&format!("[{}x", &raw[1..raw.len() - 1])),
                "typing at End must not splice the label, {source:?} got {typed:?}"
            );
        }

        let source = "Hello[^1]\n\n[^1]: note\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("Hello").expect("Hello"));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteToLineEnd,
        );
        assert!(
            !after.contains("[^1]") || after.matches("[^1]").count() == 1,
            "DeleteToLineEnd from Hello must remove the line's footnote ref, got {after:?}"
        );
        assert!(
            !after.contains("Hello[^") && !after.contains("[^x") && !after.contains("Hello1]"),
            "DeleteToLineEnd must not nibble footnote dest chrome, got {after:?}"
        );
    }

    /// Keyboard End / Cmd-Delete-to-line-end on last-in-line
    /// `[![alt](img)](url)` stay on wrapping `]`. Hidden dest is not
    /// revealed or eaten.
    #[test]
    fn line_end_from_linked_image_does_not_reveal_or_eat_dest() {
        for source in [
            "[![alt](a.png)](https://e.com)\n",
            "[![alt](a.png)](https://e.com \"title\")\n",
            "> [![alt](a.png)](https://e.com)\n",
            "- [![alt](a.png)](https://e.com)\n",
            "# [![alt](a.png)](https://e.com)\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let at = source.find("alt").expect("alt");
            let end = engine.line_end_caret(source, at);
            assert_eq!(
                source.as_bytes().get(end).copied().unwrap_or(0) as char,
                ']',
                "End from alt sat in dest, {source:?} at {end}"
            );
            caret.collapse_to(end);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            assert!(
                typed.contains("[![alt](a.png)x]") && typed.contains("https://e.com"),
                "typing at End must not extend dest, {source:?} got {typed:?}"
            );
            assert!(
                !typed.contains("https://e.comx") && !typed.contains("titlex"),
                "typing at End must not extend wrapping dest, {source:?} got {typed:?}"
            );
        }

        let source = "[![alt](a.png)](https://e.com)\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("alt").expect("alt"));
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::DeleteToLineEnd,
        );
        assert!(
            after.contains("](https://e.com)"),
            "DeleteToLineEnd from alt must keep wrapping dest, got {after:?}"
        );
        assert!(
            !after.contains("https://e.comx") && after.contains("https://e.com"),
            "DeleteToLineEnd must not eat wrapping dest, got {after:?}"
        );
    }

    /// GFM `__strong__` opener is the same wrap-mark skip as `**` / `~~`.
    #[test]
    fn empty_wrap_on_underscore_strong_opener_skips_onto_inner() {
        let source = "__strong__\n";
        let at = source.find("__").expect("strong");
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(at);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::ITALIC),
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let after = doc.buffer.content();
        assert!(
            !after.contains("*__") && !after.contains("**x**__"),
            "Cmd-I on `__` must not splice in front, got {after:?}"
        );
        assert!(
            after.contains("__*x*strong__") || after.contains("*__strong__*"),
            "empty wrap on underscore strong opener must wrap inner or the span, got {after:?}"
        );
    }

    /// Enter in the middle of a footnote definition must keep the rest of
    /// the note inside `[^1]:` (lazy continuation). A paragraph `\n\n`
    /// split orphans the tail as prose.
    #[test]
    fn enter_in_footnote_def_keeps_the_rest_in_the_definition() {
        for source in [
            "Hello[^1]\n\n[^1]: the note\n",
            "Hello[^note]\n\n[^note]: the note\n",
            "> Hello[^1]\n>\n> [^1]: the note\n",
            "Hello[^1]\n\n[^1]: **the** note\n",
        ] {
            let at = source.rfind("note").expect("note") + 2;
            let (mut doc, mut engine, mut caret) = setup(source);
            assert!(
                has_footnote_def(&engine.tree().blocks),
                "fixture must parse as a footnote def: {source:?}"
            );
            caret.collapse_to(at);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert!(
                has_footnote_def(&engine.tree().blocks),
                "mid-def Enter must keep a footnote definition, {source:?} -> {after:?}"
            );
            assert!(
                after.contains("[^1]:") || after.contains("[^note]:"),
                "footnote opener must survive, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("no\n\nte") && !after.contains("no\n\n> te"),
                "mid-def Enter must not paragraph-split the footnote, {source:?} got {after:?}"
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            engine.sync(&doc);
            assert!(
                has_footnote_def(&engine.tree().blocks),
                "must remain a footnote def after typing, {source:?} -> {typed:?}"
            );
            let def = footnote_def_slice(&engine.tree().blocks, &typed);
            assert!(
                def.contains("no") && def.contains("te") && def.contains('x'),
                "both halves and the typed char must stay in the def, {source:?} def={def:?} typed={typed:?}"
            );
            if source.contains('>') {
                assert!(
                    typed.contains("> te") || typed.contains("> xte") || typed.contains(">xte"),
                    "quoted mid-def Enter must keep `>` on the continuation, {source:?} -> {typed:?}"
                );
            }
        }
    }

    /// IME Enter (`InsertText("\n")`) shares SplitBlock: mid-def must not
    /// orphan the rest as prose.
    #[test]
    fn enter_in_footnote_def_via_insert_newline_keeps_the_definition() {
        let source = "Hello[^1]\n\n[^1]: the note\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("note").expect("note") + 2);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n".into()),
        );
        assert!(
            has_footnote_def(&engine.tree().blocks),
            "IME Enter mid-def must keep a footnote definition, got {after:?}"
        );
        assert!(
            !after.contains("no\n\nte"),
            "must not paragraph-split the footnote, got {after:?}"
        );
        let def = footnote_def_slice(&engine.tree().blocks, &after);
        assert!(
            def.contains("no") && def.contains("te"),
            "both halves must stay in the def, def={def:?} after={after:?}"
        );
    }

    /// Enter at the end of a footnote definition opens a body paragraph
    /// (`[^1]: note` then `x`, not lazy `notex` / `note\nx` inside the def).
    #[test]
    fn enter_at_end_of_footnote_def_exits_to_a_paragraph() {
        let source = "Hello[^1]\n\n[^1]: the note";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        engine.sync(&doc);
        assert!(
            has_footnote_def(&engine.tree().blocks),
            "the definition must survive, got {typed:?}"
        );
        let def = footnote_def_slice(&engine.tree().blocks, &typed);
        assert!(
            def.contains("the note") && !def.contains('x'),
            "typed `x` must not stay in the footnote, def={def:?} typed={typed:?}"
        );
        assert!(
            typed.contains("the note") && typed.lines().any(|line| line.trim() == "x"),
            "Enter at end must open a paragraph, got {typed:?}"
        );
    }

    /// Enter on empty `[^1]: ` drops the opener (quoted keep `>`).
    #[test]
    fn empty_footnote_def_enter_strips_the_marker() {
        for source in ["Hello[^1]\n\n[^1]: ", "Hello[^1]\n\n> [^1]: "] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let home = engine
                .tree()
                .empty_prefix_homes
                .iter()
                .find(|h| source.get(h.line.clone()).is_some_and(|l| l.contains("]:")))
                .map(|h| h.home)
                .expect("empty footnote def home");
            caret.collapse_to(home);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert!(
                !after.contains("[^1]:"),
                "empty footnote Enter must strip `[^1]: `, {source:?} got {after:?}"
            );
            assert!(
                after.contains("Hello[^1]"),
                "the footnote ref must survive, {source:?} got {after:?}"
            );
            if source.contains('>') {
                assert!(
                    after.contains('>'),
                    "quoted empty def Enter must keep `>`, {source:?} got {after:?}"
                );
            }
        }
    }

    /// Enter inside a list nested in a footnote still continues the list.
    #[test]
    fn enter_in_list_inside_footnote_def_continues_the_list() {
        let source = "Hello[^1]\n\n[^1]:\n    - hello\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("hello").expect("hello") + "hello".len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            has_footnote_def(&engine.tree().blocks),
            "nested list Enter must keep the footnote, got {after:?}"
        );
        assert!(
            after.contains("- hello") && after.contains("- "),
            "must continue as a list, got {after:?}"
        );
        assert!(
            after.matches("- ").count() >= 2,
            "must add a sibling item, got {after:?}"
        );
    }

    /// Shift-Enter in a footnote definition must keep the rest inside
    /// `[^1]:` (lazy body; quoted keep `>`). Comrak lifts defs out of the
    /// quote tree, so a bare `\\\n` dropped `>` the same way deflists used
    /// to lose `: `. Do not copy `[^1]: ` (that would open a second def).
    #[test]
    fn insert_line_break_in_footnote_def_keeps_lazy_continuation() {
        for (source, after_break, typed) in [
            (
                "Hello[^1]\n\n[^1]: the note",
                "Hello[^1]\n\n[^1]: the note\\\n",
                "Hello[^1]\n\n[^1]: the note\\\nx",
            ),
            (
                "> Hello[^1]\n>\n> [^1]: the note",
                "> Hello[^1]\n>\n> [^1]: the note\\\n> ",
                "> Hello[^1]\n>\n> [^1]: the note\\\n> x",
            ),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            assert!(
                has_footnote_def(&engine.tree().blocks),
                "fixture must parse as a footnote def: {source:?}"
            );
            caret.collapse_to(source.len());
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertLineBreak,
            );
            assert_eq!(
                after, after_break,
                "Shift-Enter in a footnote def must keep lazy/quoted continuation, {source:?} got {after:?}"
            );
            assert!(
                after.contains("\\\n"),
                "must stay a markdown hard break, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("<br>"),
                "footnote hard break is not HTML <br>, {source:?} got {after:?}"
            );
            assert!(
                has_footnote_def(&engine.tree().blocks),
                "must remain a footnote def after Shift-Enter, {source:?} -> {after:?}"
            );
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            assert_eq!(
                after, typed,
                "typing after footnote Shift-Enter must stay in the def, {source:?} got {after:?}"
            );
            engine.sync(&doc);
            assert!(
                has_footnote_def(&engine.tree().blocks),
                "must remain a footnote def after typing, {source:?} -> {after:?}"
            );
            let def = footnote_def_slice(&engine.tree().blocks, &after);
            assert!(
                def.contains("the note") && def.contains('x'),
                "typed x must stay in the def, {source:?} def={def:?} typed={after:?}"
            );
        }

        let source = "> Hello[^1]\n>\n> [^1]: the note";
        let (mut doc, mut engine, mut caret) = setup(source);
        let at = source.rfind("note").expect("note") + 2;
        caret.collapse_to(at);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertLineBreak,
        );
        assert_eq!(
            after, "> Hello[^1]\n>\n> [^1]: the no\\\n> te",
            "mid-def Shift-Enter must prefix the rest with `>`, got {after:?}"
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        engine.sync(&doc);
        let def = footnote_def_slice(&engine.tree().blocks, &typed);
        assert!(
            def.contains("no") && def.contains("te") && def.contains('x'),
            "both halves and typed x must stay in the def, def={def:?} typed={typed:?}"
        );
        assert!(
            typed.contains("> te") || typed.contains("> xte"),
            "quoted mid-def Shift-Enter must keep `>` on the continuation, got {typed:?}"
        );

        let source = "Hello[^1]\n\n[^1]: the note";
        let (mut doc, mut engine, mut caret) = setup(source);
        let at = source.rfind("note").expect("note") + 2;
        caret.collapse_to(at);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertLineBreak,
        );
        assert_eq!(
            after, "Hello[^1]\n\n[^1]: the no\\\nte",
            "unquoted mid-def Shift-Enter is a lazy hard break, got {after:?}"
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        engine.sync(&doc);
        let def = footnote_def_slice(&engine.tree().blocks, &typed);
        assert!(
            def.contains("no") && def.contains("te") && def.contains('x'),
            "unquoted lazy body must stay in the def, def={def:?} typed={typed:?}"
        );

        for source in ["Hello[^1]\n\n[^1]: ", "Hello[^1]\n\n> [^1]: "] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let home = engine
                .tree()
                .empty_prefix_homes
                .iter()
                .find(|h| source.get(h.line.clone()).is_some_and(|l| l.contains("]:")))
                .map(|h| h.home)
                .expect("empty footnote def home");
            caret.collapse_to(home);
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertLineBreak,
            );
            assert!(
                after.contains("\\\n"),
                "empty-def Shift-Enter must stay a hard break, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("[^1]: \\\n[^1]:") && !after.contains("[^1]:\\\\\n"),
                "empty-def Shift-Enter must not duplicate the opener, {source:?} got {after:?}"
            );
            assert!(
                !after.contains(": :") && !after.contains("\\\n: "),
                "empty-def Shift-Enter must not insert definition-list `: `, {source:?} got {after:?}"
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            engine.sync(&doc);
            let def = footnote_def_slice(&engine.tree().blocks, &typed);
            assert!(
                def.contains('x'),
                "empty-def Shift-Enter typing must stay in the def, {source:?} def={def:?} typed={typed:?}"
            );
            if source.contains('>') {
                assert!(
                    typed.contains("> x") || typed.contains(">x"),
                    "quoted empty-def Shift-Enter must keep `>`, {source:?} got {typed:?}"
                );
            }
        }

        let source = "Hello[^1]\n\n[^1]:\n    - hello";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertLineBreak,
        );
        assert!(
            after.contains("- hello\\\n"),
            "nested list Shift-Enter inside a footnote must stay a hard break, got {after:?}"
        );
        assert!(
            !after.contains("- hello\\\n- ") && !after.contains("- hello\\\n    - "),
            "Shift-Enter must not open a sibling item, got {after:?}"
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        engine.sync(&doc);
        assert!(
            has_footnote_def(&engine.tree().blocks),
            "nested list Shift-Enter must keep the footnote, got {typed:?}"
        );
        assert_eq!(
            count_list_items(&engine.tree().blocks),
            1,
            "Shift-Enter must not open a sibling item, got {typed:?}"
        );
    }

    fn has_link_ref_def(blocks: &[Block]) -> bool {
        blocks.iter().any(|b| {
            matches!(b.kind, BlockKind::LinkReferenceDefinition { .. })
                || has_link_ref_def(&b.children)
        })
    }

    fn link_ref_def_url(blocks: &[Block]) -> Option<String> {
        for b in blocks {
            if let BlockKind::LinkReferenceDefinition { url, .. } = &b.kind {
                if !url.is_empty() {
                    return Some(url.clone());
                }
            }
            if let Some(url) = link_ref_def_url(&b.children) {
                return Some(url);
            }
        }
        None
    }

    fn resolved_hello_url(blocks: &[Block]) -> Option<String> {
        for b in blocks {
            for inline in &b.inlines {
                if let Inline::Run {
                    text,
                    link: Some(link),
                    ..
                } = inline
                {
                    if text == "hello" {
                        return Some(link.url.clone());
                    }
                }
            }
            if let Some(url) = resolved_hello_url(&b.children) {
                return Some(url);
            }
        }
        None
    }

    fn empty_ref_def_home(engine: &RichEngine, source: &str) -> usize {
        engine
            .tree()
            .empty_prefix_homes
            .iter()
            .find(|h| {
                source
                    .get(h.line.clone())
                    .is_some_and(|l| l.contains("[ref]:") && !l.contains("[^"))
            })
            .map(|h| h.home)
            .unwrap_or_else(|| {
                let line_at = source.rfind("[ref]:").expect("[ref]:");
                let after = line_at + "[ref]:".len();
                if source.as_bytes().get(after) == Some(&b' ') {
                    after + 1
                } else {
                    after
                }
            })
    }

    /// Enter in a `[ref]:` dest must not `\n\n`-split dest into prose.
    /// Bare `\n` also breaks CommonMark dest, so Enter is `\\\n` (quoted
    /// keep `>`). `[hello][ref]` must keep resolving.
    #[test]
    fn enter_in_link_ref_def_keeps_the_dest_one_definition() {
        for source in [
            "[hello][ref]\n\n[ref]: https://ex.com/path\n",
            "> [hello][ref]\n>\n> [ref]: https://ex.com/path\n",
            "- [hello][ref]\n  [ref]: https://ex.com/path\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            assert!(
                has_link_ref_def(&engine.tree().blocks),
                "fixture must parse as a `[ref]:` def: {source:?}"
            );
            let at = source.find("https://ex.com/pa").expect("dest") + "https://ex.com/pa".len();
            caret.collapse_to(at);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert!(
                !after.contains("pa\n\nth") && !after.contains("pa\n\n> th"),
                "mid-dest Enter must not paragraph-split dest, {source:?} got {after:?}"
            );
            assert!(
                after.contains("\\\n"),
                "mid-dest Enter must wrap dest with `\\\\n`, {source:?} got {after:?}"
            );
            assert!(
                has_link_ref_def(&engine.tree().blocks),
                "must remain a `[ref]:` def after Enter, {source:?} -> {after:?}"
            );
            if source.contains('>') {
                assert!(
                    after.contains("> th") || after.contains(">th"),
                    "quoted mid-dest Enter must keep `>`, {source:?} got {after:?}"
                );
            }
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            engine.sync(&doc);
            assert!(
                has_link_ref_def(&engine.tree().blocks),
                "must remain a `[ref]:` def after typing, {source:?} -> {typed:?}"
            );
            let url = link_ref_def_url(&engine.tree().blocks);
            assert_eq!(
                url.as_deref(),
                Some("https://ex.com/paxth"),
                "both dest halves and typed x must stay one dest, {source:?} got {url:?} typed={typed:?}"
            );
            assert_eq!(
                resolved_hello_url(&engine.tree().blocks).as_deref(),
                Some("https://ex.com/paxth"),
                "must not drop `[hello][ref]` resolution, {source:?} typed={typed:?}"
            );
            if source.starts_with("- ") {
                assert_eq!(
                    count_list_items(&engine.tree().blocks),
                    1,
                    "Enter in list-nested `[ref]:` must not open a sibling item, {source:?} -> {typed:?}"
                );
            }
        }

        let source = "[hello][ref]\n\n[ref]: https://ex.com/pa";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        let typed = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("th".into()),
        );
        engine.sync(&doc);
        assert!(
            has_link_ref_def(&engine.tree().blocks),
            "`[ref]: https://ex.com/pa` + Enter + `th` must stay one definition, got {typed:?}"
        );
        assert_eq!(
            link_ref_def_url(&engine.tree().blocks).as_deref(),
            Some("https://ex.com/path"),
            "dest must concatenate to path, got {typed:?}"
        );
        assert_eq!(
            resolved_hello_url(&engine.tree().blocks).as_deref(),
            Some("https://ex.com/path"),
            "must not drop `[hello][ref]` resolution, got {typed:?}"
        );
        assert!(
            !typed.contains("pa\n\nth"),
            "must not paragraph-split dest, got {typed:?}"
        );
    }

    /// IME Enter (`InsertText("\n")`) shares SplitBlock.
    #[test]
    fn enter_in_link_ref_def_via_insert_newline_keeps_the_definition() {
        let source = "[hello][ref]\n\n[ref]: https://ex.com/path\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let at = source.find("https://ex.com/pa").expect("dest") + "https://ex.com/pa".len();
        caret.collapse_to(at);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n".into()),
        );
        assert!(
            has_link_ref_def(&engine.tree().blocks),
            "IME Enter mid-dest must keep a `[ref]:` definition, got {after:?}"
        );
        assert!(
            !after.contains("pa\n\nth"),
            "must not paragraph-split dest, got {after:?}"
        );
        assert_eq!(
            link_ref_def_url(&engine.tree().blocks).as_deref(),
            Some("https://ex.com/path"),
            "dest must stay concatenated, got {after:?}"
        );
    }

    /// Enter on empty `[ref]: ` drops the opener (quoted keep `>`).
    #[test]
    fn empty_link_ref_def_enter_strips_the_marker() {
        for source in ["[hello][ref]\n\n[ref]: ", "> [hello][ref]\n>\n> [ref]: "] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(empty_ref_def_home(&engine, source));
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert!(
                !after.contains("[ref]:"),
                "empty `[ref]:` Enter must strip the opener, {source:?} got {after:?}"
            );
            assert!(
                after.contains("[hello][ref]"),
                "the reference link must survive, {source:?} got {after:?}"
            );
            if source.contains('>') {
                assert!(
                    after.contains('>'),
                    "quoted empty `[ref]:` Enter must keep `>`, {source:?} got {after:?}"
                );
            }
        }
    }

    /// Shift-Enter in a `[ref]:` dest must keep the dest one definition
    /// (`\\\n`; quoted keep `>`). Empty Shift-Enter stays a hard break.
    #[test]
    fn insert_line_break_in_link_ref_def_keeps_dest_continuation() {
        for source in [
            "[hello][ref]\n\n[ref]: https://ex.com/path",
            "> [hello][ref]\n>\n> [ref]: https://ex.com/path",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let at = source.find("https://ex.com/pa").expect("dest") + "https://ex.com/pa".len();
            caret.collapse_to(at);
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertLineBreak,
            );
            assert!(
                after.contains("\\\n"),
                "Shift-Enter in dest must be a hard break, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("<br>"),
                "[ref]: hard break is not HTML <br>, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("[ref]: https://ex.com/pa\\\n[ref]:"),
                "must not duplicate the opener, {source:?} got {after:?}"
            );
            if source.contains('>') {
                assert!(
                    after.contains("> th") || after.contains(">th"),
                    "quoted dest Shift-Enter must keep `>`, {source:?} got {after:?}"
                );
            }
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            engine.sync(&doc);
            assert!(
                has_link_ref_def(&engine.tree().blocks),
                "must remain a `[ref]:` def after Shift-Enter, {source:?} -> {typed:?}"
            );
            assert_eq!(
                link_ref_def_url(&engine.tree().blocks).as_deref(),
                Some("https://ex.com/paxth"),
                "typed x must stay in dest, {source:?} typed={typed:?}"
            );
            assert_eq!(
                resolved_hello_url(&engine.tree().blocks).as_deref(),
                Some("https://ex.com/paxth"),
                "must not drop `[hello][ref]` resolution, {source:?} typed={typed:?}"
            );
        }

        for source in ["[hello][ref]\n\n[ref]: ", "> [hello][ref]\n>\n> [ref]: "] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(empty_ref_def_home(&engine, source));
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertLineBreak,
            );
            assert!(
                after.contains("\\\n"),
                "empty-def Shift-Enter must stay a hard break, {source:?} got {after:?}"
            );
            assert!(
                after.contains("[ref]:"),
                "empty-def Shift-Enter must not strip the opener, {source:?} got {after:?}"
            );
            assert!(
                !after.contains(": :") && !after.contains("\\\n: "),
                "empty-def Shift-Enter must not insert definition-list `: `, {source:?} got {after:?}"
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            engine.sync(&doc);
            assert!(
                has_link_ref_def(&engine.tree().blocks),
                "empty-def Shift-Enter typing must stay a definition, {source:?} got {typed:?}"
            );
            assert_eq!(
                link_ref_def_url(&engine.tree().blocks).as_deref(),
                Some("x"),
                "typed x must stay in dest, {source:?} typed={typed:?}"
            );
            if source.contains('>') {
                assert!(
                    typed.contains("> x") || typed.contains(">x"),
                    "quoted empty-def Shift-Enter must keep `>`, {source:?} got {typed:?}"
                );
            }
        }
    }

    fn footnote_def_slice(blocks: &[Block], source: &str) -> String {
        fn walk(blocks: &[Block], source: &str, out: &mut String) {
            for b in blocks {
                if matches!(b.kind, BlockKind::FootnoteDefinition { .. }) {
                    if let Some(slice) = source.get(b.source_range.clone()) {
                        out.push_str(slice);
                    }
                }
                walk(&b.children, source, out);
            }
        }
        let mut out = String::new();
        walk(blocks, source, &mut out);
        out
    }

    fn has_math_inline(blocks: &[Block], display: Option<bool>) -> bool {
        fn walk(blocks: &[Block], display: Option<bool>) -> bool {
            for b in blocks {
                for inline in &b.inlines {
                    if let Inline::Math {
                        display: is_display,
                        ..
                    } = inline
                    {
                        if display.is_none_or(|want| want == *is_display) {
                            return true;
                        }
                    }
                }
                if walk(&b.children, display) {
                    return true;
                }
            }
            false
        }
        walk(blocks, display)
    }

    /// Enter inside `$x^2$` / `$$\nE=mc^2\n$$` stays in the TeX. A paragraph
    /// `\n\n` split (or a list sibling) used to break the dollars.
    #[test]
    fn enter_inside_dollar_math_stays_in_the_formula() {
        for (source, at_needle, after_enter, typed) in [
            (
                "$$\nE=mc^2\n$$",
                "E",
                "$$\nE\n=mc^2\n$$",
                "$$\nE\nz=mc^2\n$$",
            ),
            (
                "see $x^2$ here",
                "x",
                "see $x\n^2$ here",
                "see $x\nz^2$ here",
            ),
            (
                "> $$\n> E=mc^2\n> $$",
                "E",
                "> $$\n> E\n> =mc^2\n> $$",
                "> $$\n> E\n> z=mc^2\n> $$",
            ),
            (
                "> see $x^2$ here",
                "x",
                "> see $x\n> ^2$ here",
                "> see $x\n> z^2$ here",
            ),
            (
                "- $$\n  E=mc^2\n  $$",
                "E",
                "- $$\n  E\n  =mc^2\n  $$",
                "- $$\n  E\n  z=mc^2\n  $$",
            ),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let at = source.find(at_needle).expect(at_needle) + at_needle.len();
            caret.collapse_to(at);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert_eq!(
                after, after_enter,
                "Enter inside math must stay in the TeX, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("\n\n"),
                "must not paragraph-split the dollars, {source:?} got {after:?}"
            );
            engine.sync(&doc);
            let display = source.contains("$$");
            assert!(
                has_math_inline(&engine.tree().blocks, Some(display)),
                "must remain math after Enter, {source:?} -> {after:?}"
            );
            if source.starts_with("- ") {
                assert_eq!(
                    count_list_items(&engine.tree().blocks),
                    1,
                    "Enter inside list math must not open a sibling item, {source:?} -> {after:?}"
                );
            }
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("z".into()),
            );
            assert_eq!(
                after, typed,
                "typing after math Enter must stay in the TeX, {source:?} got {after:?}"
            );
            engine.sync(&doc);
            assert!(
                has_math_inline(&engine.tree().blocks, Some(display)),
                "must remain math after typing, {source:?} -> {after:?}"
            );
        }

        let source = "$$\nE=mc^2\n$$";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('E').expect("E") + 1);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n".into()),
        );
        assert_eq!(
            after, "$$\nE\n=mc^2\n$$",
            "IME Enter must share SplitBlock, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            has_math_inline(&engine.tree().blocks, Some(true)),
            "IME Enter must keep display math, got {after:?}"
        );

        let source = "$x^2$";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("$x^2$\n\n") || after == "$x^2$\n\n",
            "Enter after the closer must still split the paragraph, got {after:?}"
        );
        engine.sync(&doc);
        assert!(
            has_math_inline(&engine.tree().blocks, Some(false)),
            "the original inline math must survive, got {after:?}"
        );
    }

    /// Shift-Enter inside dollar math is a raw newline in the TeX, not `\\\n`.
    #[test]
    fn insert_line_break_inside_dollar_math_is_a_newline() {
        for (source, at_needle, after_break, typed) in [
            (
                "$$\nE=mc^2\n$$",
                "E",
                "$$\nE\n=mc^2\n$$",
                "$$\nE\nz=mc^2\n$$",
            ),
            (
                "see $x^2$ here",
                "x",
                "see $x\n^2$ here",
                "see $x\nz^2$ here",
            ),
            (
                "> $$\n> E=mc^2\n> $$",
                "E",
                "> $$\n> E\n> =mc^2\n> $$",
                "> $$\n> E\n> z=mc^2\n> $$",
            ),
            (
                "- $$\n  E=mc^2\n  $$",
                "E",
                "- $$\n  E\n  =mc^2\n  $$",
                "- $$\n  E\n  z=mc^2\n  $$",
            ),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let at = source.find(at_needle).expect(at_needle) + at_needle.len();
            caret.collapse_to(at);
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertLineBreak,
            );
            assert_eq!(
                after, after_break,
                "Shift-Enter inside math must be a raw newline, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("\\\n"),
                "math hard break must not inject TeX `\\\\`, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("<br>"),
                "math hard break is not HTML <br>, {source:?} got {after:?}"
            );
            engine.sync(&doc);
            let display = source.contains("$$");
            assert!(
                has_math_inline(&engine.tree().blocks, Some(display)),
                "must remain math after Shift-Enter, {source:?} -> {after:?}"
            );
            if source.starts_with("- ") {
                assert_eq!(
                    count_list_items(&engine.tree().blocks),
                    1,
                    "Shift-Enter inside list math must not open a sibling item, {source:?} -> {after:?}"
                );
            }
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("z".into()),
            );
            assert_eq!(
                after, typed,
                "typing after math Shift-Enter must stay in the TeX, {source:?} got {after:?}"
            );
        }

        let source = "hello";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertLineBreak,
        );
        assert_eq!(
            after, "hello\\\n",
            "paragraph Shift-Enter must stay a markdown hard break, got {after:?}"
        );
    }

    /// Enter in a GFM `[hello](url)` / `![alt](url)` / autolink must not
    /// `\n\n`-split dest or label into prose. Label/title wrap with `\n`;
    /// dest/autolink split after the node. List-nested does not open a
    /// sibling item.
    #[test]
    fn enter_inside_markdown_link_keeps_the_link() {
        for source in [
            "[hello](https://e.com)\n",
            "> [hello](https://e.com)\n",
            "- [hello](https://e.com)\n",
            "# [hello](https://e.com)\n",
            "[hello][ref]\n\n[ref]: https://e.com\n",
            "[foo][]\n\n[foo]: https://e.com\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let label = if source.contains("[hello]") {
                "hello"
            } else {
                "foo"
            };
            caret.collapse_to(source.find(label).unwrap() + 3);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert!(
                tree_has_link(&engine.tree().blocks, "e.com"),
                "mid-label Enter must keep the link, {source:?} got {after:?}"
            );
            assert!(
                !after.contains("hel\n\nlo") && !after.contains("foo\n\n"),
                "mid-label Enter must not paragraph-split the label, {source:?} got {after:?}"
            );
            if source.contains('>') {
                assert!(
                    after.contains("> lo") || after.contains(">lo"),
                    "quoted mid-label Enter must keep `>`, {source:?} got {after:?}"
                );
            }
            if source.starts_with("- ") {
                assert_eq!(
                    count_list_items(&engine.tree().blocks),
                    1,
                    "Enter in list-nested link must not open a sibling item, {source:?} -> {after:?}"
                );
            }
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            engine.sync(&doc);
            assert!(
                tree_has_link(&engine.tree().blocks, "e.com"),
                "must remain a link after typing, {source:?} -> {typed:?}"
            );
        }

        let source = "[hello](https://e.com)\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find(']').unwrap());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            tree_has_link(&engine.tree().blocks, "e.com"),
            "Enter on label `]` must keep the link, got {after:?}"
        );
        assert!(
            !after.contains("hello\n\n](") && !after.contains("hello]\n\n("),
            "Enter on `]` must not splice a paragraph before dest, got {after:?}"
        );
    }

    /// Dest URL cannot contain a line ending: Enter splits after the node.
    #[test]
    fn enter_in_markdown_link_dest_splits_after_the_link() {
        for source in [
            "[hello](https://e.com/path)\n",
            "> [hello](https://e.com/path)\n",
            "![alt](https://e.com/i.png)\n",
            "<https://e.com/path>\n",
            "www.example.com/path\n",
            "see user@example.com now\n",
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            let at = source.find("/pa").or_else(|| source.find("e.com")).unwrap() + 3;
            caret.collapse_to(at);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            assert!(
                !after.contains("pa\n\nth")
                    && !after.contains("e.c\n\nom")
                    && !after.contains("pa\nth"),
                "dest Enter must not wrap the URL, {source:?} got {after:?}"
            );
            if source.contains("[hello]")
                || source.contains("<https")
                || source.contains("www.")
                || source.contains('@')
            {
                assert!(
                    tree_has_link(&engine.tree().blocks, "e.com")
                        || tree_has_link(&engine.tree().blocks, "example.com"),
                    "dest Enter must keep the link, {source:?} got {after:?}"
                );
            }
            if source.contains("![alt]") {
                assert!(
                    tree_has_image(&engine.tree().blocks, "e.com"),
                    "image dest Enter must keep the image, {source:?} got {after:?}"
                );
            }
            if source.contains('>') {
                assert!(
                    after.contains('>'),
                    "quoted dest Enter must keep `>`, {source:?} got {after:?}"
                );
            }
        }
    }

    /// IME Enter (`InsertText("\n")`) shares SplitBlock.
    #[test]
    fn enter_inside_markdown_link_via_insert_newline_keeps_the_link() {
        let source = "[hello](https://e.com)\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("hello").unwrap() + 3);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n".into()),
        );
        assert!(
            tree_has_link(&engine.tree().blocks, "e.com"),
            "IME Enter mid-label must keep the link, got {after:?}"
        );
        assert!(
            !after.contains("hel\n\nlo"),
            "must not paragraph-split the label, got {after:?}"
        );
    }

    /// Shift-Enter in a link label is a hard break that stays inside.
    #[test]
    fn insert_line_break_inside_markdown_link_keeps_the_link() {
        for source in ["[hello](https://e.com)", "> [hello](https://e.com)"] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.find("hello").unwrap() + 3);
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertLineBreak,
            );
            assert!(
                after.contains("\\\n"),
                "Shift-Enter in label must be a hard break, {source:?} got {after:?}"
            );
            assert!(
                tree_has_link(&engine.tree().blocks, "e.com"),
                "must remain a link after Shift-Enter, {source:?} -> {after:?}"
            );
            if source.contains('>') {
                assert!(
                    after.contains("> lo") || after.contains(">lo"),
                    "quoted label Shift-Enter must keep `>`, {source:?} got {after:?}"
                );
            }
        }

        let source = "[hello](https://e.com/path)";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("/pa").unwrap() + 3);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertLineBreak,
        );
        assert!(
            tree_has_link(&engine.tree().blocks, "e.com"),
            "Shift-Enter in dest must keep the link, got {after:?}"
        );
        assert!(
            !after.contains("pa\\\nth") && !after.contains("pa\nth"),
            "Shift-Enter must not wrap dest URL, got {after:?}"
        );
    }

    /// Link titles may wrap; dest URL may not.
    #[test]
    fn enter_in_markdown_link_title_stays_in_the_title() {
        let source = "[hello](https://e.com \"title\")\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("title").unwrap() + 2);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            tree_has_link(&engine.tree().blocks, "e.com"),
            "title Enter must keep the link, got {after:?}"
        );
        assert!(
            after.contains("ti\ntle") && !after.contains("ti\n\ntle"),
            "title Enter must soft-wrap the title, got {after:?}"
        );
    }

    /// Enter inside wrap marks / code spans / HTML phrasing must not `\n\n`
    /// split the closer into prose. Mid-span is a soft wrap (quoted keep
    /// `>`; list-nested does not open a sibling item). ATX headings split
    /// after the heading line so `# **hello**` is not broken.
    #[test]
    fn enter_inside_wrap_marks_keeps_the_span() {
        for (source, needle, mark) in [
            ("**bold**\n", "bold", Some(MarkSet::BOLD)),
            ("> **bold**\n", "bold", Some(MarkSet::BOLD)),
            ("- **bold**\n", "bold", Some(MarkSet::BOLD)),
            ("*italic*\n", "italic", Some(MarkSet::ITALIC)),
            ("~~strike~~\n", "strike", Some(MarkSet::STRIKE)),
            ("`code`\n", "code", Some(MarkSet::CODE)),
            ("> `code`\n", "code", Some(MarkSet::CODE)),
            ("- `code`\n", "code", Some(MarkSet::CODE)),
            ("__bold__\n", "bold", Some(MarkSet::BOLD)),
            ("<b>hello</b>\n", "hello", None),
            ("> <b>hello</b>\n", "hello", None),
            ("- <b>hello</b>\n", "hello", None),
            ("<a href=\"https://e.com\">label</a>\n", "label", None),
            ("<em>hello</em>\n", "hello", None),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.find(needle).unwrap() + 2);
            let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
            let left = &needle[..2];
            let right = &needle[2..];
            assert!(
                !after.contains(&format!("{left}\n\n{right}")),
                "mid-span Enter must not paragraph-split, {source:?} got {after:?}"
            );
            if let Some(mark) = mark {
                assert!(
                    tree_has_mark(&engine.tree().blocks, mark, left)
                        || tree_has_mark(&engine.tree().blocks, mark, right)
                        || tree_has_mark(&engine.tree().blocks, mark, needle),
                    "must remain a wrap mark after Enter, {source:?} -> {after:?}"
                );
            } else {
                assert!(
                    after.contains('<') && after.contains("</"),
                    "must remain HTML phrasing after Enter, {source:?} -> {after:?}"
                );
            }
            if source.starts_with("> ") {
                assert!(
                    after.contains(&format!("> {right}")) || after.contains(&format!(">{right}")),
                    "quoted mid-span Enter must keep `>`, {source:?} got {after:?}"
                );
            }
            if source.starts_with("- ") {
                assert_eq!(
                    count_list_items(&engine.tree().blocks),
                    1,
                    "Enter in list-nested wrap must not open a sibling item, {source:?} -> {after:?}"
                );
            }
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            let typed = doc.buffer.content();
            engine.sync(&doc);
            if let Some(mark) = mark {
                assert!(
                    tree_has_mark(&engine.tree().blocks, mark, "x")
                        || tree_has_mark(&engine.tree().blocks, mark, left)
                        || tree_has_mark(&engine.tree().blocks, mark, right),
                    "must remain a wrap mark after typing, {source:?} -> {typed:?}"
                );
            } else {
                assert!(
                    typed.contains('<') && typed.contains("</"),
                    "must remain HTML phrasing after typing, {source:?} -> {typed:?}"
                );
            }
        }
    }

    /// ATX cannot contain a wrap newline; Enter splits after the heading.
    #[test]
    fn enter_inside_atx_heading_wrap_splits_after_the_heading() {
        let source = "# **hello**\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("hello").unwrap() + 2);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        engine.sync(&doc);
        assert!(
            matches!(
                engine.tree().blocks[0].kind,
                BlockKind::Heading {
                    level: 1,
                    style: HeadingStyle::Atx,
                    ..
                }
            ),
            "first block must stay an ATX heading, got {:?}",
            engine.tree().blocks[0].kind
        );
        assert!(
            tree_has_mark(&engine.tree().blocks, MarkSet::BOLD, "hello"),
            "heading wrap must stay one node, got {after:?}"
        );
        assert!(
            !after.contains("hel\nlo") && !after.contains("hel\n\nlo"),
            "must not wrap inside an ATX heading, got {after:?}"
        );

        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("hello").unwrap() + "hello".len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        engine.sync(&doc);
        assert!(
            matches!(
                engine.tree().blocks[0].kind,
                BlockKind::Heading {
                    level: 1,
                    style: HeadingStyle::Atx,
                    ..
                }
            ),
            "heading-end Enter must keep the ATX heading, got {:?}",
            engine.tree().blocks[0].kind
        );
        assert!(
            tree_has_mark(&engine.tree().blocks, MarkSet::BOLD, "hello"),
            "heading-end Enter must not break `# **hello**`, got {after:?}"
        );
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        let typed = doc.buffer.content();
        let x_line = typed
            .lines()
            .find(|line| line.contains('x'))
            .expect("typed x");
        assert!(
            !x_line.trim_start().starts_with('#'),
            "text after heading-end Enter must be a paragraph, got {typed:?}"
        );
    }

    /// IME Enter (`InsertText("\n")`) shares SplitBlock.
    #[test]
    fn enter_inside_wrap_marks_via_insert_newline_keeps_the_span() {
        let source = "**bold**\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("bold").unwrap() + 2);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("\n".into()),
        );
        assert!(
            tree_has_mark(&engine.tree().blocks, MarkSet::BOLD, "bo")
                || tree_has_mark(&engine.tree().blocks, MarkSet::BOLD, "ld")
                || tree_has_mark(&engine.tree().blocks, MarkSet::BOLD, "bold"),
            "IME Enter mid-span must keep emphasis, got {after:?}"
        );
        assert!(
            !after.contains("bo\n\nld"),
            "must not paragraph-split the wrap, got {after:?}"
        );
    }

    /// Shift-Enter stays inside the span (hard break in emphasis; raw
    /// newline in code / HTML so `\` does not paint).
    #[test]
    fn insert_line_break_inside_wrap_marks_keeps_the_span() {
        for source in ["**bold**", "> **bold**"] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.find("bold").unwrap() + 2);
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertLineBreak,
            );
            assert!(
                after.contains("\\\n"),
                "Shift-Enter in emphasis must be a hard break, {source:?} got {after:?}"
            );
            assert!(
                tree_has_mark(&engine.tree().blocks, MarkSet::BOLD, "bo")
                    || tree_has_mark(&engine.tree().blocks, MarkSet::BOLD, "ld")
                    || tree_has_mark(&engine.tree().blocks, MarkSet::BOLD, "bold"),
                "must remain emphasis after Shift-Enter, {source:?} -> {after:?}"
            );
            if source.contains('>') {
                assert!(
                    after.contains("> ld") || after.contains(">ld"),
                    "quoted emphasis Shift-Enter must keep `>`, {source:?} got {after:?}"
                );
            }
        }

        let source = "`code`";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("code").unwrap() + 2);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertLineBreak,
        );
        assert!(
            !after.contains("\\"),
            "Shift-Enter in a code span must be a raw newline, got {after:?}"
        );
        assert!(
            tree_has_mark(&engine.tree().blocks, MarkSet::CODE, "co")
                || tree_has_mark(&engine.tree().blocks, MarkSet::CODE, "de")
                || tree_has_mark(&engine.tree().blocks, MarkSet::CODE, "code"),
            "must remain a code span after Shift-Enter, got {after:?}"
        );

        let source = "<b>hello</b>";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("hello").unwrap() + 2);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertLineBreak,
        );
        assert!(
            !after.contains('\\'),
            "Shift-Enter in HTML phrasing must be a raw newline, got {after:?}"
        );
        assert!(
            after.contains("<b>") && after.contains("</b>"),
            "must remain HTML phrasing after Shift-Enter, got {after:?}"
        );
    }

    /// After a wrap-span soft wrap, Home/End follow the source `\n` line
    /// (painted wrap of `**bo\nld**`), not the whole paragraph.
    #[test]
    fn home_end_after_wrap_span_enter_follow_the_source_line() {
        let source = "**bold**\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("bold").unwrap() + 2);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        engine.sync(&doc);
        let at = caret.cursor();
        let home = engine.line_start_caret(&after, at);
        let end = engine.line_end_caret(&after, at);
        let ld = after.find("ld").expect("ld");
        assert_eq!(
            home, ld,
            "Home on the wrapped line must sit on `ld`, not the opener, got {home} {after:?}"
        );
        assert_eq!(
            end,
            ld + 2,
            "End on the wrapped line must sit after `ld` (before closer), got {end} {after:?}"
        );
        assert!(
            home > after.find("bo").expect("bo"),
            "Home must not jump to the first source line, got {home} {after:?}"
        );
    }

    /// Remaining listed-GFM Enter probes after wrap-mark / HTML phrasing.
    #[test]
    fn enter_inside_nested_and_html_code_phrasing_keeps_the_span() {
        let source = "***hello***\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("hello").unwrap() + 2);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            !after.contains("he\n\nllo"),
            "nested emphasis must not paragraph-split, got {after:?}"
        );
        assert!(
            tree_has_mark(&engine.tree().blocks, MarkSet::BOLD, "he")
                || tree_has_mark(&engine.tree().blocks, MarkSet::BOLD, "hello")
                || tree_has_mark(&engine.tree().blocks, MarkSet::ITALIC, "he")
                || tree_has_mark(&engine.tree().blocks, MarkSet::ITALIC, "hello"),
            "must remain nested emphasis, got {after:?}"
        );

        let source = "<code>hello</code>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("hello").unwrap() + 2);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("<code>") && after.contains("</code>") && !after.contains("he\n\nllo"),
            "HTML <code> phrasing must stay one node, got {after:?}"
        );

        let source = "**hello**\n=========\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("hello").unwrap() + 2);
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        engine.sync(&doc);
        assert!(
            !after.contains("he\n\nllo"),
            "setext wrap Enter must not paragraph-split, got {after:?}"
        );
        assert!(
            matches!(
                engine.tree().blocks[0].kind,
                BlockKind::Heading {
                    style: HeadingStyle::Setext,
                    ..
                }
            ),
            "setext heading must stay a heading, got {:?} {after:?}",
            engine.tree().blocks[0].kind
        );

        let source = "A&amp;B\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('&').unwrap());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::SplitBlock);
        assert!(
            after.contains("&amp;"),
            "Enter at an entity home must not split the literal, got {after:?}"
        );
    }

    /// Empty Cmd-B/I/E/K on ATX `#`, setext underlines, table `|`, and 0–3
    /// space heading indent must sit in the body (same skip as Home/click).
    /// A click on revealed `#` / `|` used to splice `**x**# Title` / `**x**|`.
    #[test]
    fn empty_wrap_on_heading_and_table_prefix_chrome_skips_onto_body() {
        for (source, at, must_not, must) in [
            ("# Title\n", 0usize, "**x**#", "# **x**Title"),
            ("## Hello\n", 0, "**x**#", "## **x**Hello"),
            ("> # Title\n", 0, "**x**#", "> # **x**Title"),
            ("- # Title\n", 0, "**x**#", "- # **x**Title"),
            (" # Title\n", 0, "**x**#", " # **x**Title"),
            (
                "# Title #\n",
                "# Title #\n".find(" #").unwrap() + 1,
                "**x**#",
                "# Title**x**",
            ),
            (
                "Title\n===\n",
                "Title\n===\n".find('=').unwrap(),
                "**x**=",
                "**x**",
            ),
            ("| a | b |\n|---|---|\n", 0, "**x**|", "**x**"),
            (
                "foo|bar\n---|---\n",
                "foo|bar\n---|---\n".find('|').unwrap(),
                "**x**|",
                "**x**",
            ),
        ] {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(at);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            let after = apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            );
            assert!(
                !after.contains(must_not),
                "empty wrap on block chrome must not splice onto it, {source:?} at {at} got {after:?}"
            );
            assert!(
                after.contains(must),
                "empty wrap must sit in the body, {source:?} expected {must:?} got {after:?}"
            );
            if source.starts_with('|') {
                assert!(
                    after.starts_with('|'),
                    "empty wrap on a piped table must stay a table, got {after:?}"
                );
            }
        }

        let source = "# Title\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(0);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let after = doc.buffer.content();
        assert!(
            !after.contains("[]()#"),
            "Cmd-K on ATX `#` must not splice in front of hashes, got {after:?}"
        );
        assert!(
            after.contains("# [Title]()") || after.contains("# []()"),
            "Cmd-K on ATX `#` must sit in the title, got {after:?}"
        );

        let source = "| a | b |\n|---|---|\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(0);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        let after = doc.buffer.content();
        assert!(
            !after.contains("[]()|") && !after.contains("[]() |"),
            "Cmd-K on table `|` must not splice in front of the pipe, got {after:?}"
        );
        assert!(
            after.contains("| [a]() |")
                || after.contains("| [a]()")
                || after.contains("|[a]()")
                || after.contains("|[]()"),
            "Cmd-K on table `|` must sit in the cell, got {after:?}"
        );

        let source = "# Title\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find('T').unwrap());
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert!(
            after.contains("# **x**Title") || after.contains("# **x**"),
            "empty wrap already on the title still inserts in the body, got {after:?}"
        );
        assert!(!after.contains("**x**#"));

        let typed = leftover_click_then_type("# Title");
        assert!(
            typed.lines().any(|line| line.trim() == "x"),
            "leftover below a heading still opens a paragraph, got {typed:?}"
        );
        assert!(!typed.contains("Titlex") && !typed.contains("**x**#"));

        // Trailing paragraph space is an empty-caret home, not dest chrome.
        let source = "hello ";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        assert_eq!(
            after, "hello []()",
            "empty Cmd-K after a word must not wrap the word, got {after:?}"
        );
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert!(
            after.contains("hello **x**") && !after.contains("**x**hello"),
            "empty Cmd-B after a word must insert there, got {after:?}"
        );
    }

    /// Empty wrap / InsertText on markdown-link `[`, HTML phrasing tags, and
    /// GFM alignment dashes skip onto inner text (same as Home/click). A
    /// click on revealed `[` / `<b>` used to splice `**x**[` / `x<b>`;
    /// typing on `|---|` used to break the table.
    #[test]
    fn empty_wrap_and_insert_on_link_html_and_alignment_dest_chrome_skips_onto_inner() {
        fn wrap_then_type(source: &str, at: usize) -> String {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(at);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            );
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            )
        }

        for (source, at, must_not, must) in [
            (
                "[hello](https://e.com)\n",
                0usize,
                "**x**[",
                "[**x**hello](https://e.com)",
            ),
            (
                "> [hello](https://e.com)\n",
                "> [hello](https://e.com)\n".find('[').unwrap(),
                "**x**[",
                "[**x**hello](https://e.com)",
            ),
            (
                "- [hello](https://e.com)\n",
                "- [hello](https://e.com)\n".find('[').unwrap(),
                "**x**[",
                "[**x**hello](https://e.com)",
            ),
            (
                "[hello][ref]\n\n[ref]: https://e.com\n",
                0,
                "**x**[",
                "[**x**hello][ref]",
            ),
            ("<b>hello</b>\n", 0, "**x**<b>", "<b>**x**hello</b>"),
            (
                "> <b>hello</b>\n",
                "> <b>hello</b>\n".find('<').unwrap(),
                "**x**<b>",
                "<b>**x**hello</b>",
            ),
            (
                "<a href=\"https://e.com\">hello</a>\n",
                0,
                "**x**<a",
                "<a href=\"https://e.com\">**x**hello</a>",
            ),
        ] {
            let after = wrap_then_type(source, at);
            assert!(
                !after.contains(must_not),
                "empty wrap on inline dest chrome must not splice onto it, {source:?} at {at} got {after:?}"
            );
            assert!(
                after.contains(must),
                "empty wrap must sit in the inner text, {source:?} expected {must:?} got {after:?}"
            );
        }

        let source = "[hello](https://e.com)\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(0);
        let typed = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert_eq!(
            typed, "[xhello](https://e.com)\n",
            "InsertText on link `[` must extend the label, got {typed:?}"
        );

        let source = "<b>hello</b>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(0);
        let typed = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert_eq!(
            typed, "<b>xhello</b>\n",
            "InsertText on `<b>` must extend the inner text, got {typed:?}"
        );

        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let at = source.find("---").unwrap();
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(at);
        let typed = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        engine.sync(&doc);
        assert!(
            typed.contains("|---|") && !typed.contains("x---") && !typed.contains("-x-"),
            "InsertText on alignment dashes must not nibble the separator, got {typed:?}"
        );
        assert!(
            typed.contains("x")
                && (typed.contains("| xa |")
                    || typed.contains("|xa |")
                    || typed.contains("| x1 |")
                    || typed.contains("|x1 |")
                    || typed.contains("| a |") && typed.contains("x")),
            "InsertText on alignment dashes must land in a cell, got {typed:?}"
        );
        assert!(
            engine
                .tree()
                .blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Table { .. })),
            "alignment InsertText must keep a table, got {typed:?}"
        );

        // Trailing paragraph space is still an empty-caret home.
        let source = "hello ";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        assert_eq!(
            after, "hello []()",
            "empty Cmd-K after a word must not wrap the word, got {after:?}"
        );
    }

    /// InsertText on revealed list/quote/task/`[ref]:` prefixes uses the
    /// same Home/click skip empty wrap already uses. Typing on `-` / `>` /
    /// `[x]` / `[ref]:` `[` used to splice `x- hello` / `x> hello` /
    /// `x[x]` / `x[ref]:` and break the construct.
    #[test]
    fn insert_text_on_list_quote_task_and_ref_def_prefix_skips_onto_body() {
        fn type_at(source: &str, at: usize) -> String {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(at);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            )
        }

        for (source, want, what) in [
            ("> hello\n", "> xhello\n", "quote `>`"),
            ("- hello\n", "- xhello\n", "list `-`"),
            ("* hello\n", "* xhello\n", "list `*`"),
            ("1. hello\n", "1. xhello\n", "ordered `1.`"),
            ("1) hello\n", "1) xhello\n", "ordered `1)`"),
        ] {
            let typed = type_at(source, 0);
            assert_eq!(
                typed, want,
                "InsertText on {what} must extend the body, got {typed:?}"
            );
        }

        let source = "> - hello\n";
        let at = source.find('-').unwrap();
        let typed = type_at(source, at);
        assert!(
            typed.contains("> - xhello") && !typed.contains("x- hello") && !typed.contains("> x-"),
            "InsertText on a quoted list marker must extend the item, got {typed:?}"
        );

        let source = "- [x] done\n";
        let at = source.find('[').unwrap();
        let typed = type_at(source, at);
        assert!(
            typed.contains("- [x] xdone") && !typed.contains("x[x]"),
            "InsertText on a task checkbox must sit in the body, got {typed:?}"
        );

        let source = "1. [x] done\n";
        let at = source.find('[').unwrap();
        let typed = type_at(source, at);
        assert!(
            typed.contains("1. [x] xdone") && !typed.contains("x[x]"),
            "InsertText on an ordered task checkbox must sit in the body, got {typed:?}"
        );

        let source = "> - [ ] hello\n";
        let at = source.find('[').unwrap();
        let typed = type_at(source, at);
        assert!(
            typed.contains("[ ] xhello") && !typed.contains("x[ ]"),
            "InsertText on a quoted task checkbox must sit in the body, got {typed:?}"
        );

        let source = "[ref]: https://e.com\n";
        let typed = type_at(source, 0);
        assert!(
            typed.contains("[xref]:") && !typed.contains("x[ref]:"),
            "InsertText on `[ref]:` `[` must extend the label, got {typed:?}"
        );

        let source = "> [ref]: https://e.com\n";
        let at = source.find('[').unwrap();
        let typed = type_at(source, at);
        assert!(
            typed.contains("[xref]:") && !typed.contains("x[ref]:"),
            "InsertText on quoted `[ref]:` `[` must extend the label, got {typed:?}"
        );

        // Colon after the label is dest chrome: type into dest, not `x:`.
        let source = "[ref]: https://e.com\n";
        let at = source.find(':').unwrap();
        let typed = type_at(source, at);
        assert!(
            typed.contains("[ref]:")
                && typed.contains("xhttps://e.com")
                && !typed.contains("x[ref]"),
            "InsertText on `[ref]:` colon must sit in dest, got {typed:?}"
        );

        // Cell-end `|` stays the End insert home (do not skip onto `world`).
        let source = "| hello | world |\n|---|---|\n";
        let after_hello = source.find("hello").unwrap() + "hello".len();
        let typed = type_at(source, after_hello);
        assert!(
            typed.contains("| hellox") && !typed.contains("xworld") && !typed.contains("hello x |"),
            "InsertText at cell end must stay in that cell, got {typed:?}"
        );

        // Trailing paragraph space is still an empty-caret home.
        let source = "hello ";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.len());
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        assert_eq!(
            after, "hello []()",
            "empty Cmd-K after a word must not wrap the word, got {after:?}"
        );
    }

    /// HTML-block `<div>` / `</div>` is dest chrome: InsertText on the open
    /// tag skips onto inner markdown (`<div>xhello</div>`), not glue `x<div>`.
    /// Typing on the close tag opens a body paragraph after the block
    /// (leftover-below / `</div>` EOF newline). Tags are not nibbled.
    /// Quoted keep `>`.
    #[test]
    fn insert_text_on_html_block_wrapper_tags_skips_onto_inner_or_after() {
        fn type_at(source: &str, at: usize) -> String {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(at);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            )
        }

        for source in [
            "<div>hello</div>\n",
            "<div>hello</div>",
            "<p>hello</p>\n",
            "<div>\ninner\n</div>\n",
        ] {
            let typed = type_at(source, 0);
            assert!(
                !typed.contains("x<div>") && !typed.contains("x<p>"),
                "InsertText on the open tag must not glue onto it, {source:?} got {typed:?}"
            );
            assert!(
                typed.contains('x')
                    && (typed.contains("xhello")
                        || typed.contains("xinner")
                        || typed.contains("x\ninner")),
                "InsertText on `<div>` / `<p>` must sit in the inner, {source:?} got {typed:?}"
            );
            assert!(
                typed.contains("<div>") || typed.contains("<p>"),
                "open tag must stay, {source:?} got {typed:?}"
            );
        }

        let source = "> <div>hello</div>\n";
        let at = source.find('<').expect("open");
        let typed = type_at(source, at);
        assert!(
            typed.contains("> <div>xhello</div>")
                && !typed.contains("x<div>")
                && !typed.contains("x>"),
            "quoted InsertText on `<div>` must keep `>` and sit inner, got {typed:?}"
        );

        let source = "- <div>hello</div>\n";
        let at = source.find('<').expect("open");
        let typed = type_at(source, at);
        assert!(
            typed.contains("- <div>xhello</div>") && !typed.contains("x<div>"),
            "list-nested InsertText on `<div>` must sit inner, got {typed:?}"
        );

        let source = "<div><span>hello</span></div>\n";
        let at = source.find("<span>").expect("span");
        let typed = type_at(source, at);
        assert!(
            typed.contains("<span>xhello</span>")
                && !typed.contains("x<span>")
                && typed.contains("<div>"),
            "InsertText on an inner `<span>` must sit in that tag's inner, got {typed:?}"
        );

        for source in [
            "<div>hello</div>\n",
            "<div>hello</div>",
            "<div>\ninner\n</div>",
            "> <div>hello</div>\n",
            "<div></div>\n",
        ] {
            let at = source.find("</div>").expect("close");
            let typed = type_at(source, at);
            assert!(
                !typed.contains("</div>x") && typed.contains("</div>"),
                "InsertText on `</div>` must not glue onto the close tag, {source:?} got {typed:?}"
            );
            assert!(
                typed.lines().any(|line| line.trim() == "x"),
                "InsertText on `</div>` must open a body paragraph after, {source:?} got {typed:?}"
            );
            let x_line = typed
                .lines()
                .find(|line| line.trim() == "x")
                .expect("typed x");
            assert!(
                !x_line.starts_with('>'),
                "body paragraph after `</div>` must not keep quote chrome, {source:?} got {typed:?}"
            );
        }

        let empty = "<div></div>\n";
        let typed = type_at(empty, 0);
        assert!(
            typed.contains("<div>x</div>") && !typed.contains("x<div>"),
            "InsertText on empty `<div></div>` open tag must sit between tags, got {typed:?}"
        );

        let leftover = leftover_click_then_type("<div>hello</div>");
        assert!(
            leftover.lines().any(|line| line.trim() == "x") && !leftover.contains("</div>x"),
            "leftover below a `<div>` still opens a paragraph, got {leftover:?}"
        );

        let source = "<div>hello</div>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("hello").expect("inner"));
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        let after = doc.buffer.content();
        assert!(
            after.contains("<div>") && after.contains("</div>"),
            "Backspace at inner start must not nibble wrapper tags, got {after:?}"
        );
    }

    /// Inline `<!--` dest chrome: empty wrap wraps the widget (`**<!-- x -->**`),
    /// not splice `**x**<!--`. InsertText on opener bytes after the End home
    /// (`<`) skips onto comment inner. Typing at End still extends the
    /// previous text (`hellox<!--`).
    #[test]
    fn empty_wrap_and_insert_on_inline_comment_dest_chrome() {
        fn wrap_at(source: &str, at: usize) -> String {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(at);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::ToggleMark(MarkSet::BOLD),
            )
        }
        fn type_at(source: &str, at: usize) -> String {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(at);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            )
        }

        for (source, at) in [
            (
                "hello <!-- x -->world\n",
                "hello <!-- x -->world\n".find("<!--").unwrap(),
            ),
            (
                "> hello <!-- x -->\n",
                "> hello <!-- x -->\n".find("<!--").unwrap(),
            ),
            (
                "- hello <!-- x -->\n",
                "- hello <!-- x -->\n".find("<!--").unwrap(),
            ),
        ] {
            let after = wrap_at(source, at);
            assert!(
                !after.contains("**x**<!--") && !after.contains("****<!--"),
                "empty wrap on `<!--` must not splice in front, {source:?} got {after:?}"
            );
            assert!(
                after.contains("**<!-- x -->**") || after.contains("**<!-- x -->"),
                "empty wrap must wrap the comment widget, {source:?} got {after:?}"
            );

            let typed = type_at(source, at + 1);
            assert!(
                !typed.contains("x<!--") && typed.contains("<!--"),
                "InsertText inside `<!--` must not glue onto the opener, {source:?} got {typed:?}"
            );
            assert!(
                typed.contains("<!--x") || typed.contains("<!-- x"),
                "InsertText inside `<!--` must sit in the comment inner, {source:?} got {typed:?}"
            );
        }

        let source = "hello <!-- x -->world\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("<!--").expect("comment"));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        let wrapped = doc.buffer.content();
        caret.collapse_to(wrapped.find("<!--").expect("wrapped comment"));
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        let unwrapped = doc.buffer.content();
        assert!(
            unwrapped.contains("<!-- x -->") && !unwrapped.contains("**<!--"),
            "second Cmd-B must unwrap the comment widget, got {unwrapped:?}"
        );

        let source = "hello <?php echo 1; ?>world\n";
        let at = source.find("<?").expect("pi");
        let after = wrap_at(source, at);
        assert!(
            !after.contains("**x**<?") && after.contains("**<?php echo 1; ?>**"),
            "empty wrap on inline PI must wrap the widget, got {after:?}"
        );
    }

    /// InsertText on dest-chrome openers Home/click already skip: GFM
    /// `<https://…>` / `$math$` / `[[wiki]]` / dest `(` / `"title"`. Glue
    /// `x<https://` / `x$x^2$` / `x[[page]]` / `[hello]x(url)` used to break
    /// the construct. Empty wrap still wraps autolink / image widgets.
    /// Wrap-mark `*` / `[**|**hello]` stay put. Standalone `![alt]` at `!`
    /// stays insert-before.
    #[test]
    fn insert_text_on_autolink_math_wiki_dest_chrome_skips_onto_inner() {
        fn type_at(source: &str, at: usize) -> String {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(at);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            )
        }

        for (source, at, must_not, must) in [
            (
                "<https://e.com>\n",
                0usize,
                "x<https://",
                "<xhttps://e.com>",
            ),
            (
                "> <https://e.com>\n",
                "> <https://e.com>\n".find('<').unwrap(),
                "x<https://",
                "> <xhttps://e.com>",
            ),
            (
                "- <https://e.com>\n",
                "- <https://e.com>\n".find('<').unwrap(),
                "x<https://",
                "- <xhttps://e.com>",
            ),
            ("$x^2$\n", 0, "x$x^2$", "$xx^2$"),
            (
                "> $x^2$\n",
                "> $x^2$\n".find('$').unwrap(),
                "x$x^2$",
                "> $xx^2$",
            ),
            (
                "- $x^2$\n",
                "- $x^2$\n".find('$').unwrap(),
                "x$x^2$",
                "- $xx^2$",
            ),
            ("[[page]]\n", 0, "x[[page]]", "[[xpage]]"),
            (
                "- [[page]]\n",
                "- [[page]]\n".find('[').unwrap(),
                "x[[page]]",
                "- [[xpage]]",
            ),
            ("[[page|Label]]\n", 0, "x[[page|Label]]", "[[page|xLabel]]"),
            (":smile:\n", 0, "x:smile:", ":xsmile:"),
        ] {
            let typed = type_at(source, at);
            assert!(
                !typed.contains(must_not),
                "InsertText on dest chrome must not glue onto it, {source:?} at {at} got {typed:?}"
            );
            assert!(
                typed.contains(must),
                "InsertText on dest chrome must sit in the inner, {source:?} expected {must:?} got {typed:?}"
            );
        }

        let source = "$$\nE=mc^2\n$$\n";
        let typed = type_at(source, 0);
        assert!(
            !typed.contains("x$$") && typed.contains("$$") && typed.contains("xE=mc^2"),
            "InsertText on display `$$` must sit in the TeX, got {typed:?}"
        );

        let source = "| <https://e.com> |\n|---|\n";
        let at = source.find('<').unwrap();
        let typed = type_at(source, at);
        assert!(
            typed.contains("| <xhttps://e.com> |") && !typed.contains("x<https://"),
            "InsertText on a table autolink `<` must sit in the URL, got {typed:?}"
        );

        let source = "| $x$ |\n|---|\n";
        let at = source.find('$').unwrap();
        let typed = type_at(source, at);
        assert!(
            typed.contains("| $xx$ |") && !typed.contains("x$x$"),
            "InsertText on a table `$` must sit in the TeX, got {typed:?}"
        );

        let source = "| [[p]] |\n|---|\n";
        let at = source.find('[').unwrap();
        let typed = type_at(source, at);
        assert!(
            typed.contains("| [[xp]] |") && !typed.contains("x[[p]]"),
            "InsertText on a table wiki `[` must sit in the target, got {typed:?}"
        );

        let source = "[hello](https://e.com)\n";
        let at = source.find('(').unwrap();
        let typed = type_at(source, at);
        assert!(
            typed.contains("[hello](xhttps://e.com)") && !typed.contains("]x("),
            "InsertText on dest `(` must sit in the URL, got {typed:?}"
        );

        let source = "[hi](url \"t\")\n";
        let at = source.find('"').unwrap();
        let typed = type_at(source, at);
        assert!(
            typed.contains("[hi](url \"xt\")") && !typed.contains("x\"t\""),
            "InsertText on dest title `\"` must sit in the title, got {typed:?}"
        );

        let source = "![alt](url)\n";
        let at = source.find('(').unwrap();
        let typed = type_at(source, at);
        assert!(
            typed.contains("![alt](xurl)") && !typed.contains("]x("),
            "InsertText on image dest `(` must sit in the URL, got {typed:?}"
        );

        let source = "[ref]: /url \"title\"\n";
        let at = source.find('"').unwrap();
        let typed = type_at(source, at);
        assert!(
            typed.contains("[ref]: /url \"xtitle\"") && !typed.contains("x\"title\""),
            "InsertText on `[ref]:` title `\"` must sit in the title, got {typed:?}"
        );

        let source = "[![alt](img)](url)\n";
        let typed = type_at(source, 0);
        assert!(
            !typed.contains("x[![")
                && (typed.contains("[x![alt](img)](url)") || typed.contains("[![xalt](img)](url)")),
            "InsertText on wrapping `[![` must sit in the label, got {typed:?}"
        );

        let source = "![alt](url)\n";
        let typed = type_at(source, 0);
        assert_eq!(
            typed, "x![alt](url)\n",
            "InsertText at a standalone image `!` must insert before the widget, got {typed:?}"
        );

        let source = "<https://e.com>\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(0);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("**<https://e.com>**") && !after.contains("<**"),
            "empty wrap on an autolink must still wrap the widget, got {after:?}"
        );

        let source = "[hello](https://e.com)\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(0);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert!(
            after.contains("[**x**hello](https://e.com)") && !after.contains("[****xhello]"),
            "empty wrap then type in a link label must stay inside the pair, got {after:?}"
        );

        let source = "**hello**\n";
        let typed = type_at(source, 0);
        assert_eq!(
            typed, "x**hello**\n",
            "InsertText on wrap-mark `*` must stay put, got {typed:?}"
        );

        let source = "www.example.com\n";
        let typed = type_at(source, 0);
        assert_eq!(
            typed, "xwww.example.com\n",
            "InsertText at a bare GFM autolink start must extend the URL, got {typed:?}"
        );
    }

    /// InsertText on setext underline / closed ATX trailing hashes / leading
    /// table `|` uses the same Home/click skip. Click on revealed `===` /
    /// trailing ` #` / first `|` used to glue `Title\n===x` / `# Title #x` /
    /// `x| a |`. Document EOF on underline / trailing hashes still opens a
    /// body line. Open ATX `# Titlex` and cell-end `hellox|` stay.
    #[test]
    fn insert_text_on_setext_closed_atx_and_leading_table_pipe_skips_onto_title_or_cell() {
        fn type_at(source: &str, at: usize) -> String {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(at);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            )
        }

        for source in [
            "Title\n===\n",
            "Title\n---\n",
            "> Title\n> ===\n",
            "Title\n===\n\nNext\n",
            "Hello\n\nTitle\n===\n\nNext\n",
        ] {
            let at = source.find('=').or_else(|| source.find("---")).unwrap();
            let typed = type_at(source, at);
            assert!(
                typed.contains("Titlex") && has_setext_underline_line(&typed),
                "InsertText on setext underline must extend the title, {source:?} got {typed:?}"
            );
            assert!(
                !typed.contains("===x")
                    && !typed.contains("=x")
                    && !typed.contains("x=")
                    && !typed.contains("---x")
                    && !typed.contains("-x-"),
                "must not glue onto the underline, {source:?} got {typed:?}"
            );
            if source.contains("Next") {
                assert!(
                    typed.contains("Next") && !typed.contains("xNext"),
                    "must not type into the following paragraph, {source:?} got {typed:?}"
                );
            }
        }

        let source = "Title\n===";
        let typed = type_at(source, source.len());
        assert!(
            typed.contains("===\nx") && !typed.contains("===x") && typed.contains("Title\n"),
            "InsertText at document EOF on a setext underline must still open a body line, got {typed:?}"
        );

        for source in ["# Title #\n", "## Title ##\n", "> # Title #\n"] {
            let at = source.rfind('#').unwrap();
            let typed = type_at(source, at);
            assert!(
                typed.contains("Titlex")
                    && typed.contains('#')
                    && !typed.contains("#x")
                    && !typed.contains("x#")
                    && !typed.contains("# Title #x")
                    && !typed.contains("# Titlex#"),
                "InsertText on closed ATX trailing hashes must sit at the title end, {source:?} got {typed:?}"
            );
        }

        let source = "# Title #\n";
        let at = source.find("Title").unwrap() + "Title".len();
        let typed = type_at(source, at);
        assert!(
            typed.contains("# Titlex #") || typed.contains("# Titlex#"),
            "InsertText at closed-ATX title end is the End home, got {typed:?}"
        );

        let source = "# Title\n";
        let typed = type_at(source, source.find("Title").unwrap() + "Title".len());
        assert_eq!(
            typed, "# Titlex\n",
            "open ATX title-end typing `# Titlex` must stay, got {typed:?}"
        );

        let source = "# Title\n";
        let typed = type_at(source, 0);
        assert!(
            typed.contains("# xTitle") || typed.contains("#xTitle"),
            "InsertText on opening ATX `#` must sit in the title, got {typed:?}"
        );
        assert!(!typed.contains("x# Title"));

        for source in [
            "| a | b |\n|---|---|\n| 1 | 2 |\n",
            "> | a | b |\n> |---|---|\n> | 1 | 2 |\n",
            "| a | b |\n|---|---|\n",
        ] {
            let at = source.find('|').unwrap();
            let typed = type_at(source, at);
            assert!(
                typed.contains("| xa |") || typed.contains("|xa |") || typed.contains("> | xa |"),
                "InsertText on the leading table `|` must sit in the first cell, {source:?} got {typed:?}"
            );
            assert!(
                !typed.contains("x|") && !typed.starts_with('x'),
                "must not glue onto the leading pipe, {source:?} got {typed:?}"
            );
        }

        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let at = source.find("| 1 |").unwrap();
        let typed = type_at(source, at);
        assert!(
            typed.contains("| x1 |") || typed.contains("|x1 |"),
            "InsertText on a data-row leading `|` must sit in that cell, got {typed:?}"
        );
        assert!(!typed.contains("x| 1"));

        let source = "| hello | world |\n|---|---|\n| 1 | 2 |\n";
        let at = source.find("hello").unwrap() + "hello".len() + 1;
        assert_eq!(
            source.as_bytes().get(at).copied(),
            Some(b'|'),
            "precondition: caret on first-cell End `|`"
        );
        let typed = type_at(source, at);
        assert!(
            typed.contains("hellox") || typed.contains("hello x|") || typed.contains("hellox|"),
            "InsertText on cell-end `|` must stay the End home, got {typed:?}"
        );
        assert!(
            typed.contains("world") && !typed.contains("xworld") && !typed.contains("worldx"),
            "must not skip onto the next cell, got {typed:?}"
        );
        assert_eq!(
            typed.matches('|').count(),
            source.matches('|').count(),
            "must not split the row, got {typed:?}"
        );

        let source = "foo|bar\n---|---\nbaz|bim\n";
        let at = source.find('|').unwrap();
        let typed = type_at(source, at);
        assert!(
            (typed.contains("foox|bar") || typed.contains("foo x|bar") || typed.contains("foox|"))
                && !typed.contains("xbar")
                && !typed.contains("xfoo"),
            "compact-table `|` stays the left-cell End home, got {typed:?}"
        );
        assert!(
            typed.contains('|') && typed.contains("---"),
            "must keep a compact table, got {typed:?}"
        );
    }

    /// InsertText on GitHub `[!NOTE]` / TIP / … uses the same Home/click skip
    /// onto the custom title or body. Typing on the revealed tag used to
    /// splice `x[!NOTE]`. Empty wrap splice onto the tag is unchanged.
    #[test]
    fn insert_text_on_github_alert_tag_skips_onto_title_or_body() {
        fn type_at(source: &str, at: usize) -> String {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(at);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            )
        }

        for (source, inner) in [
            ("> [!NOTE]\n> body\n", "xbody"),
            ("> [!TIP]\n> hi\n", "xhi"),
            ("> [!WARNING]\n> careful\n", "xcareful"),
            ("> [!NOTE] Pay attention\n> body\n", "xPay"),
        ] {
            let at = source.find("[!").unwrap();
            let typed = type_at(source, at);
            assert!(
                !typed.contains("x[!NOTE]")
                    && !typed.contains("x[!TIP]")
                    && !typed.contains("x[!WARNING]")
                    && typed.contains("[!"),
                "InsertText on `[!NOTE]` must not splice onto the tag, {source:?} got {typed:?}"
            );
            assert!(
                typed.contains(inner),
                "InsertText on `[!NOTE]` must sit in the title or body, {source:?} expected {inner:?} got {typed:?}"
            );
        }

        let source = "> [!NOTE]\n> body\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.collapse_to(source.find("[!").unwrap());
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::ToggleMark(MarkSet::BOLD),
        );
        let after = doc.buffer.content();
        assert!(
            after.contains("**") && !after.contains("**> [!NOTE]"),
            "empty wrap on `[!NOTE]` must not wrap the whole alert, got {after:?}"
        );
        assert!(
            after.contains("**[!NOTE]")
                || after.contains("**x**[!NOTE]")
                || after.contains("****[!NOTE]")
                || after.contains("[!**"),
            "empty wrap splice onto `[!NOTE]` is unchanged, got {after:?}"
        );
    }

    /// InsertText on a thematic `---` / `***` / `___` / `<hr>` widget opens a
    /// new paragraph above (type-at-start) or after (close edge / leftover-
    /// below). Typing must not rewrite the rule into `x---` / a setext
    /// heading. Quoted keep `>`. Atomic Left/Right/Delete stay one step.
    #[test]
    fn insert_text_on_thematic_break_opens_paragraph_above_or_after() {
        fn type_at(source: &str, at: usize) -> String {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(at);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            )
        }
        fn still_thematic(source: &str) -> bool {
            let doc = Document::new(source);
            let mut engine = RichEngine::new();
            engine.sync(&doc);
            fn walk(blocks: &[Block]) -> bool {
                blocks
                    .iter()
                    .any(|b| thematic_break_range(b).is_some() || walk(&b.children))
            }
            walk(&engine.tree().blocks)
        }

        for source in [
            "---\n", "---", "***\n", "___\n", "* * *\n", "- - -\n", "<hr>\n", "<hr/>\n", " ---\n",
        ] {
            let at = source
                .find("---")
                .or_else(|| source.find("***"))
                .or_else(|| source.find("___"))
                .or_else(|| source.find("* * *"))
                .or_else(|| source.find("- - -"))
                .or_else(|| source.find("<hr"))
                .unwrap();
            let typed = type_at(source, at);
            assert!(
                still_thematic(&typed),
                "InsertText on a thematic widget must keep a rule, {source:?} got {typed:?}"
            );
            assert!(
                !typed.starts_with("x---")
                    && !typed.starts_with("x***")
                    && !typed.starts_with("x___")
                    && !typed.starts_with("x* * *")
                    && !typed.starts_with("x- - -")
                    && !typed.starts_with("x<hr")
                    && !typed.contains("x---")
                    && !typed.contains("x***")
                    && !typed.contains("x___")
                    && !typed.contains("x<hr"),
                "must not rewrite the rule into `x---`, {source:?} got {typed:?}"
            );
            assert!(
                typed.lines().any(|line| line.trim() == "x"),
                "InsertText on the widget must open a paragraph, {source:?} got {typed:?}"
            );
            let x_line = typed
                .lines()
                .position(|line| line.trim() == "x")
                .expect("x line");
            let rule_line = typed
                .lines()
                .position(|line| {
                    let t = line.trim();
                    t == "---"
                        || t == "***"
                        || t == "___"
                        || t == "* * *"
                        || t == "- - -"
                        || t.starts_with("<hr")
                })
                .expect("rule line");
            assert!(
                x_line < rule_line,
                "type-at-start must open a paragraph above the rule, {source:?} got {typed:?}"
            );
        }

        let source = "hello\n\n---\n\nworld\n";
        let at = source.find("---").unwrap();
        let typed = type_at(source, at);
        assert!(
            still_thematic(&typed)
                && typed.contains("hello")
                && typed.contains("world")
                && !typed.contains("hellox")
                && !typed.contains("xworld")
                && !typed.contains("x---")
                && typed.lines().any(|line| line.trim() == "x"),
            "mid-document type-at-start must insert above the rule, got {typed:?}"
        );
        assert!(
            !has_setext_underline_line(&typed) || typed.contains("hello\n\n"),
            "must not turn the previous paragraph into a setext heading, got {typed:?}"
        );

        let source = "> ---\n";
        let at = source.find("---").unwrap();
        let typed = type_at(source, at);
        assert!(
            still_thematic(&typed) && typed.contains("> ---") && !typed.contains("x---"),
            "quoted InsertText must keep `>` and the rule, got {typed:?}"
        );
        assert!(
            typed
                .lines()
                .any(|line| line.trim() == "x" || line.trim() == "> x"),
            "quoted type-at-start must open a quoted paragraph above, got {typed:?}"
        );
        let x_line = typed.lines().find(|line| line.contains('x')).expect("x");
        assert!(
            x_line.trim_start().starts_with('>'),
            "quoted paragraph above must keep `>`, got {typed:?}"
        );

        let source = "- <hr>\n";
        let at = source.find('<').unwrap();
        let typed = type_at(source, at);
        assert!(
            still_thematic(&typed) && typed.contains("<hr>") && !typed.contains("x<hr"),
            "list-nested `<hr>` must stay a widget, got {typed:?}"
        );
        assert!(
            typed.contains("- x") || typed.lines().any(|line| line.trim() == "x"),
            "list-nested type-at-start must open an item/paragraph above, got {typed:?}"
        );

        for source in ["---", "<hr>", "***"] {
            let leftover = leftover_click_then_type(source);
            assert!(
                leftover.lines().any(|line| line.trim() == "x")
                    && still_thematic(&leftover)
                    && !leftover.contains("---x")
                    && !leftover.contains("<hr>x")
                    && !leftover.contains("***x"),
                "leftover below a last-block rule still opens after, {source:?} got {leftover:?}"
            );
            let x_line = leftover
                .lines()
                .position(|line| line.trim() == "x")
                .expect("x");
            let rule_line = leftover
                .lines()
                .position(|line| {
                    let t = line.trim();
                    t == "---" || t == "***" || t.starts_with("<hr")
                })
                .expect("rule");
            assert!(
                x_line > rule_line,
                "leftover-below must open after the rule, {source:?} got {leftover:?}"
            );
        }

        let source = "hello\n\n---\n\nworld\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let rule = first_thematic_break_range(&engine);
        caret.collapse_to(rule.end);
        let typed = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("x".into()),
        );
        assert!(
            still_thematic(&typed)
                && typed.contains("hello")
                && typed.contains("world")
                && !typed.contains("x---")
                && !typed.contains("---x")
                && typed.lines().any(|line| line.trim() == "x"),
            "InsertText at the close edge must open after, not nibble the rule, got {typed:?}"
        );

        let source = "hello\n\n---\n\nworld\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let rule = first_thematic_break_range(&engine);
        caret.collapse_to(rule.start);
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Delete);
        let after = doc.buffer.content();
        assert!(
            !after.contains("---") && after.contains("hello") && after.contains("world"),
            "Delete on the widget must still remove the whole rule, got {after:?}"
        );
    }

    /// InsertText on two-space / backslash hard-break chrome must not sit in
    /// the marker. Interior click/IME lands on the next line's first visible
    /// char; the break start stays after the previous word.
    #[test]
    fn insert_text_on_hard_break_skips_onto_visible_text() {
        fn type_at(source: &str, at: usize) -> String {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(at);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            )
        }

        for source in [
            "a  \nb\n",
            "a\\\nb\n",
            "> a  \n> b\n",
            "- a  \nb\n",
            "- a  \n  b\n",
        ] {
            let (doc, mut engine, _) = setup(source);
            engine.sync(&doc);
            let hard = first_hard_break_range(&engine);
            assert!(
                hard.end > hard.start + 1,
                "precondition: interior chrome, {source:?} {hard:?}"
            );
            let interior = hard.start + 1;
            let typed = type_at(source, interior);
            assert!(
                typed.contains("  \n") || typed.contains("\\\n"),
                "InsertText must not nibble the hard-break marker, {source:?} got {typed:?}"
            );
            assert!(
                typed.contains("xb") || typed.contains("x b") || typed.contains("> xb"),
                "interior InsertText must land on the next line's first visible char, {source:?} got {typed:?}"
            );
            assert!(
                !typed.contains("a x")
                    && !typed.contains("ax  ")
                    && !typed.contains("a  x")
                    && !typed.contains("a x\n")
                    && !typed.contains("a\\x")
                    && !typed.contains("ax\\"),
                "must not type inside the two spaces / `\\`, {source:?} got {typed:?}"
            );

            let after_a = source.find('a').unwrap() + 1;
            let typed = type_at(source, after_a);
            assert!(
                typed.contains("ax  \n")
                    || typed.contains("ax\\\n")
                    || typed.contains("ax  \n>")
                    || typed.contains("> ax  \n"),
                "InsertText at the break start must extend the previous word, {source:?} got {typed:?}"
            );
        }
    }

    /// HTML-block `<br>` is a type-7 block: it continues until a blank line.
    /// Leftover / EOF InsertText with a single `\n` swallows the next
    /// paragraph into the tag (`<br>\nx` is one HTML block).
    #[test]
    fn leftover_after_html_block_br_keeps_a_paragraph() {
        fn still_html_br(source: &str) -> bool {
            let doc = Document::new(source);
            let mut engine = RichEngine::new();
            engine.sync(&doc);
            fn walk(blocks: &[Block]) -> bool {
                blocks
                    .iter()
                    .any(|b| html_block_break_range(b).is_some() || walk(&b.children))
            }
            walk(&engine.tree().blocks)
        }
        for source in ["<br>", "<br/>", "<br>\n"] {
            let leftover = leftover_click_then_type(source);
            assert!(
                leftover.lines().any(|line| line.trim() == "x") && still_html_br(&leftover),
                "leftover below HTML-block `<br>` must keep a break widget plus a paragraph, {source:?} got {leftover:?}"
            );
            let eof = type_at_eof(source.trim_end());
            assert!(
                eof.lines().any(|line| line.trim() == "x") && still_html_br(&eof),
                "EOF InsertText after HTML-block `<br>` must keep a break widget plus a paragraph, {source:?} got {eof:?}"
            );
        }
    }

    /// InsertText on `&amp;` / `\*` dest suffixes and inline `<br>` interiors
    /// must not glue into hidden chrome (`A&xamp;B`, `A\x*B`, `a<xbr>b`).
    /// The widget start still extends the previous word.
    #[test]
    fn insert_text_on_entity_escape_and_br_dest_chrome_skips_onto_visible() {
        fn type_at(source: &str, at: usize) -> String {
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(at);
            apply(
                &mut doc,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("x".into()),
            )
        }

        for (source, literal, after) in [
            ("A&amp;B\n", "&amp;", "A&amp;xB"),
            ("A&lt;B\n", "&lt;", "A&lt;xB"),
            ("A&#38;B\n", "&#38;", "A&#38;xB"),
            ("A&#x7B;B\n", "&#x7B;", "A&#x7B;xB"),
            ("> A&amp;B\n", "&amp;", "> A&amp;xB"),
            ("- A&amp;B\n", "&amp;", "- A&amp;xB"),
            (
                "[A&amp;B](https://e.com)\n",
                "&amp;",
                "[A&amp;xB](https://e.com)",
            ),
            ("| A&amp;B | z |\n| --- | --- |\n", "&amp;", "| A&amp;xB |"),
        ] {
            let entity = source.find(literal).expect(literal);
            for at in entity + 1..entity + literal.len() {
                let typed = type_at(source, at);
                assert!(
                    typed.contains(after) && !typed.contains("&x") && !typed.contains("A&x"),
                    "InsertText on {literal} dest chrome must skip after the glyph, {source:?} at {at} got {typed:?}"
                );
                assert!(
                    typed.contains(literal),
                    "must not nibble the entity, {source:?} at {at} got {typed:?}"
                );
            }
            let typed = type_at(source, entity);
            assert!(
                typed.contains("Ax&") || typed.contains("A&amp;x") || typed.contains("> Ax&") || typed.contains("- Ax&") || typed.contains("[Ax&") || typed.contains("| Ax&"),
                "InsertText at the `&` home must still extend the previous word, {source:?} got {typed:?}"
            );
        }

        for source in [
            "A\\*B\n",
            "> A\\*B\n",
            "- A\\*B\n",
            "[A\\*B](https://e.com)\n",
        ] {
            let star = source.find('*').expect("star");
            let typed = type_at(source, star);
            assert!(
                typed.contains("\\*x") && !typed.contains("\\x*"),
                "InsertText on escaped `*` must not un-escape, {source:?} got {typed:?}"
            );
            let slash = source.find('\\').expect("slash");
            let typed = type_at(source, slash);
            assert!(
                typed.contains("Ax\\*") || typed.contains("> Ax\\*") || typed.contains("- Ax\\*") || typed.contains("[Ax\\*"),
                "InsertText at the `\\` home must still extend the previous word, {source:?} got {typed:?}"
            );
        }

        let doubled = "A\\\\B\n";
        let second = doubled.find('\\').unwrap() + 1;
        let typed = type_at(doubled, second);
        assert_eq!(
            typed, "A\\\\xB\n",
            "InsertText on escaped `\\\\` dest chrome must skip after the glyph, got {typed:?}"
        );

        for source in ["a<br>b\n", "a<br/>b\n", "> a<br>b\n", "- a<br>b\n"] {
            let br = source.find('<').expect("br");
            for at in br + 1..br + source[br..].find('>').expect(">") + 1 {
                let typed = type_at(source, at);
                assert!(
                    typed.contains("br>x") || typed.contains("br/>x") || typed.contains("br> xb"),
                    "InsertText on inline `<br>` interior must skip after the widget, {source:?} at {at} got {typed:?}"
                );
                assert!(
                    !typed.contains("<xbr") && !typed.contains("<bx") && typed.contains("<br"),
                    "must not nibble the `<br>` tag, {source:?} at {at} got {typed:?}"
                );
            }
            let typed = type_at(source, br);
            assert!(
                typed.contains("ax<br") || typed.contains("ax<br/") || typed.contains("> ax<br") || typed.contains("- ax<br"),
                "InsertText at `<br>` start must still extend the previous word, {source:?} got {typed:?}"
            );
        }

        let code = "`A&amp;B`\n";
        let amp = code.find("amp").expect("amp");
        let typed = type_at(code, amp);
        assert!(
            typed.contains("`A&xamp;B`") || typed.contains("xamp"),
            "InsertText in a code span must stay literal, got {typed:?}"
        );
    }
}
