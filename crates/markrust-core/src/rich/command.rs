// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rich editing commands. Each command compiles to a byte splice on the
//! source buffer (the single source of truth) and is one undo transaction.

use std::ops::Range;

use crate::document::Document;
use crate::undo::{SelectionSnapshot, TransactionKind};

use super::engine::{
    blank_caret_gap_after_last, blank_caret_gap_at, caret_for_click_below_content,
    expand_mark_delimiters, frontmatter_body_start, raw_body_range, raw_container_prefix,
    step_right_in_slice, Bias, RichEngine, TablePos,
};

pub use super::engine::code_body_source_map;
use super::escape::{escape_text, EscapeContext};
use super::input_rules::{input_rule_breaks_table, match_input_rule_with, InputRule};
use super::serialize::serialize_block;
use super::tree::{
    Block, BlockKind, ColumnAlign, Frontmatter, HeadingStyle, Inline, LinkAttrs, MarkSet, NodeId,
    RichTree,
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
        clamp_caret_out_of_frontmatter(engine, caret);
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
    if blank_caret_gap_after_last(engine.tree()).is_some() {
        caret.collapse_to(caret_for_click_below_content(engine.tree()));
        return RichOutcome::Noop;
    }
    let source = doc.buffer.content();
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
    let offset = caret.cursor();
    let source = doc.buffer.content();
    let raw = engine.in_raw_context(offset);
    let in_table = engine.in_table(offset);
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
    let mut inserted = if raw {
        text.to_string()
    } else {
        let ctx = EscapeContext {
            in_table: engine.in_table(offset),
            at_line_start: offset == 0 || source.as_bytes().get(offset - 1) == Some(&b'\n'),
        };
        escape_text(text, ctx)
    };
    // Typora empty quotes/lists are `> ` / `- `, not `>`. InsertText at
    // that home fills in the missing marker space so typing is `> x`.
    if empty_prefix_home_needs_marker_space(engine.tree(), &source, offset, &inserted) {
        inserted.insert(0, ' ');
    }
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
/// paragraph soft-wrap vs `\n\n` paragraph; later lines that look like GFM
/// (ATX / list / quote / fence) stay markdown instead of escaped text; list
/// marker lines become sibling items; quote/list prefixes kept for wraps;
/// raw blocks stay inside. Single-line complete GFM pastes (`# Title` with
/// no `\n`) are rewritten onto this path by `try_insert_gfm_block_paste`.
fn insert_multiline_text(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    text: &str,
) -> Result<RichOutcome, RichError> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let offset = caret.cursor();
    let source = doc.buffer.content();
    if engine.in_table(offset) {
        let inserted = format_table_paste(&text, &source, offset);
        return commit_inserted_text(doc, engine, caret, offset, inserted, false);
    }
    if in_raw_block(engine, offset) {
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
    let first_at_start = offset == 0 || source.as_bytes().get(offset - 1) == Some(&b'\n');
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
        inserted.push_str(&escape_text(
            line,
            EscapeContext {
                in_table: false,
                at_line_start: if i == 0 {
                    first_at_start
                } else {
                    wrap_prefix.is_empty()
                },
            },
        ));
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
        let at_line_start = offset == 0 || source.as_bytes().get(offset - 1) == Some(&b'\n');
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
    let at_line_start = offset == 0 || source.as_bytes().get(offset - 1) == Some(&b'\n');
    if !at_line_start {
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
            let kind = if is_coalescable_insert(&text) {
                TransactionKind::Typing
            } else {
                TransactionKind::Command
            };
            let before = caret.snapshot();
            let after = CaretState::collapsed(offset + text.len());
            doc.replace_range_tx(offset, offset, &text, kind, before, after.snapshot());
            *caret = after;
            engine.sync(doc);
            Ok(RichOutcome::Changed)
        }
        InputRule::Replace {
            range,
            insert,
            caret: new_caret,
        } => {
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
        quote_marker_prefix(line).is_some() && deepest_is_quote_or_list(&tree.blocks, blank.home)
    })
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
fn clamp_caret_out_of_frontmatter(engine: &RichEngine, caret: &mut CaretState) {
    let end = frontmatter_body_start(engine.tree());
    if end == 0 || caret.range.start >= end {
        return;
    }
    if caret.range.end <= end {
        caret.collapse_to(end);
        return;
    }
    caret.range.start = end;
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
    let mut range = trim_inline_chrome(engine, &source, del_start..del_end);
    if range.start >= range.end {
        return Ok(RichOutcome::Noop);
    }
    extend_empty_mark_wrappers(&source, engine, &mut range);
    caret.range = range.clone();
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
    let mut range = trim_inline_chrome(engine, &source, from..to);
    if range.start >= range.end {
        return Ok(RichOutcome::Noop);
    }
    extend_empty_mark_wrappers(&source, engine, &mut range);
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
        DeleteBound::LineStart => {
            let start = line_start(&source, cursor);
            engine.clamp_raw_prefix(&source, engine.snap_caret(start, Bias::Right), Bias::Right)
        }
        DeleteBound::LineEnd => {
            let end = line_end_exclusive(&source, cursor);
            engine.clamp_raw_prefix(&source, engine.snap_caret(end, Bias::Left), Bias::Left)
        }
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
    let range = trim_inline_chrome(engine, &source, start..end);
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

/// Drop `[` / `](url)` / `**` / ticks from a delete range so Backspace at the
/// start of a link label (or Delete at the end) does not nibble dest chrome.
fn trim_inline_chrome(engine: &RichEngine, source: &str, mut range: Range<usize>) -> Range<usize> {
    while range.start < range.end && engine.byte_is_inline_chrome(source, range.start) {
        range.start += 1;
    }
    while range.end > range.start && engine.byte_is_inline_chrome(source, range.end - 1) {
        range.end -= 1;
    }
    range
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
    let start = caret.range.start;
    let end = caret.range.end;
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
        if source.as_bytes().get(byte) != Some(&b'|') {
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
    if !engine.in_table(caret.range.start) && !engine.in_table(caret.cursor()) {
        return true;
    }
    let Some(cell) = cell_edit_range_near(engine, source, caret.range.start)
        .or_else(|| cell_edit_range_near(engine, source, caret.cursor()))
    else {
        return false;
    };
    if caret.range.is_empty() {
        caret.collapse_to(caret.cursor().clamp(cell.start, cell.end));
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
    // `Term\n\n: details` into `Term\n\n\n\n: details`.
    if at_definition_term_end(engine, offset) {
        return split_at_definition_term_end(doc, engine, caret);
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
        _ => {
            if let Some(item) = ancestor_list_item(engine, leaf_id) {
                list_split_text(&source, engine, item, offset)
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

fn list_split_text(source: &str, _engine: &RichEngine, item: &Block, offset: usize) -> String {
    let line = current_line(source, offset);
    let quote = quote_prefix(line);
    let marker = list_marker_prefix(after_quote(line)).or_else(|| {
        let slice = source.get(item.source_range.clone()).unwrap_or_default();
        let first = slice.split('\n').next().unwrap_or(slice);
        list_marker_prefix(after_quote(first))
    });
    let marker = marker.unwrap_or_else(|| "- ".to_string());
    format!("\n{quote}{marker}")
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

/// Leading indent plus `:` and an optional following space/tab (the PHP-Extra
/// / Typora details marker, after any quote prefix).
fn definition_details_marker_prefix(line: &str) -> Option<String> {
    let indent_len = line
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    let rest = &line[indent_len..];
    if !rest.starts_with(':') {
        return None;
    }
    let mut take = indent_len + 1;
    if rest.as_bytes().get(1) == Some(&b' ') || rest.as_bytes().get(1) == Some(&b'\t') {
        take += 1;
    }
    Some(line[..take.min(line.len())].to_string())
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
    let end = last_visible_body_end(term).unwrap_or(offset);
    let line_at = if end == 0 { 0 } else { end - 1 };
    let line = current_line(&source, line_at);
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
        let mut out = head.to_string();
        if trailing_nl {
            out.push('\n');
        }
        return out;
    }
    slice.to_string()
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
    let indent_len = line
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    let rest = &line[indent_len..];
    let marker_len = if rest.starts_with(['-', '*', '+']) && rest.as_bytes().get(1) == Some(&b' ') {
        2
    } else if rest == "-" || rest == "*" || rest == "+" {
        rest.len()
    } else if let Some(end) = rest.find(['.', ')']) {
        if !rest[..end].is_empty() && rest[..end].bytes().all(|b| b.is_ascii_digit()) {
            end + 1 + usize::from(rest.as_bytes().get(end + 1) == Some(&b' '))
        } else {
            0
        }
    } else {
        0
    };
    if marker_len == 0 {
        return None;
    }
    let mut take = indent_len + marker_len;
    let after = line.get(take..).unwrap_or("");
    if after.starts_with("[ ]") || after.starts_with("[x]") || after.starts_with("[X]") {
        take = (take + 4).min(line.len());
    }
    Some(line[..take.min(line.len())].to_string())
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
    if engine.in_table(offset) {
        return table_cell_break(doc, engine, caret);
    }
    if in_raw_block(engine, offset) {
        let source = doc.buffer.content();
        return insert_raw_newline(doc, engine, caret, &source, offset);
    }
    splice(doc, caret, offset, offset, "\\\n", TransactionKind::Command);
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
    // Typora: Cmd-B/I/E inside `$math$` / `` `code` `` / `[[wiki]]` / `:emoji:`
    // must not splice `**` into the span (fenced bodies already no-op).
    if wrap_is_immune(engine, caret) {
        return Ok(RichOutcome::Noop);
    }
    if caret.range.is_empty() {
        // Typora / source wrap: empty Cmd-B/I/E inserts `****` / `**` / `` ` ` ``
        // with the caret inside so the next insert is wrapped. Do not toggle
        // the whole run the caret sits in (`**hello**` for a mid-word caret).
        return toggle_mark_collapsed(doc, engine, caret, mark);
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
        return Ok(RichOutcome::Noop);
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
    let pair = format!("{open}{close}");
    splice(doc, caret, offset, offset, &pair, TransactionKind::Command);
    caret.collapse_to(offset + open.len());
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
        BlockKind::CodeBlock { .. } | BlockKind::Opaque { .. } => Some(block.source_range.clone()),
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
    if selection_in_table(engine, caret) {
        // GFM cells are not headings; rewriting the table eats `|`.
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
    if wrap_is_immune(engine, caret) {
        return Ok(RichOutcome::Noop);
    }
    if caret.range.is_empty() {
        let word = word_range(&source, caret.cursor());
        if !word.is_empty() {
            caret.range = word;
            caret.reversed = false;
            // Word bounds cannot include `|`, but clamp if the caret sat on a
            // pipe and snapped into a cell.
            if !clamp_wrap_to_table_cell(engine, caret, &source) {
                return Ok(RichOutcome::Noop);
            }
        }
        if caret.range.is_empty() {
            splice(
                doc,
                caret,
                caret.cursor(),
                caret.cursor(),
                "[]()",
                TransactionKind::Command,
            );
            // Caret inside the brackets.
            caret.collapse_to(caret.cursor() - 3);
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
        } => ranges_overlap(source_range, &sel),
        _ => false,
    });
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
            group: 1,
        })
    } else {
        None
    };
    for inline in inlines.iter_mut() {
        if let Inline::Run {
            source_range,
            link: slot,
            raw,
            ..
        } = inline
        {
            if ranges_overlap(source_range, sel) {
                *slot = link.clone();
                *raw = None;
            }
        }
    }
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
fn clamp_to_raw_edit(engine: &RichEngine, source: &str, offset: usize) -> usize {
    let Some(block) = raw_block_at(engine, offset) else {
        return offset;
    };
    let body = raw_body_range(block, source);
    let at = offset.clamp(body.start, body.end);
    let prefix = raw_container_prefix(source, block);
    let content = line_start(source, at) + prefix.len();
    if at < content {
        content.clamp(body.start, body.end)
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
    let body = raw_body_range(block, source);
    let prefix = raw_container_prefix(source, block);
    let first_line_start = line_start(source, body.start);
    let first_content = (first_line_start + prefix.len()).clamp(body.start, body.end);
    if to <= first_content {
        return Ok(Some(RichOutcome::Noop));
    }
    let line_s = line_start(source, to);
    let content = line_s + prefix.len();
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
    let wrapped = wrap_frontmatter(raw);
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

fn wrap_frontmatter(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with("---") {
        let mut s = trimmed.to_string();
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s
    } else {
        format!("---\n{trimmed}\n---\n")
    }
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
    let updated = crate::upsert_yaml_key(&raw, key, value);
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
            return Ok(RichOutcome::Noop);
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
    use crate::rich::engine::{Bias, RichEngine};
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
        let source = "---\ntitle: Hello\n---\n\n# Body\n";
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
            "YAML title must stay, got {after:?}"
        );
        assert!(
            !after.starts_with("x---"),
            "typed text must not prefix the opening fence, got {after:?}"
        );
        assert!(
            after[info.end_byte..].contains('x'),
            "typed text must land in the body, got {after:?}"
        );
        engine.sync(&doc);
        let fm_end = super::frontmatter_body_start(engine.tree());
        caret.collapse_to(fm_end);
        let before = doc.buffer.content();
        apply(&mut doc, &mut engine, &mut caret, RichCommand::Backspace);
        assert_eq!(
            doc.buffer.content(),
            before,
            "Backspace at the body start must not nibble YAML"
        );
    }

    #[test]
    fn table_shift_tab_on_first_cell_keeps_the_table() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(source);
        let cell_a = engine.tree().blocks[0].children[0].children[0]
            .source_range
            .start;
        caret.collapse_to(cell_a);
        let before = doc.buffer.content();
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::TableTab { reverse: true },
        );
        assert_eq!(
            doc.buffer.content(),
            before,
            "Shift-Tab in the first cell must not rewrite the table"
        );
        assert!(
            engine.table_pos(caret.cursor()).is_some(),
            "caret must stay in the table (or a defined exit), got {}",
            caret.cursor()
        );
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
        assert!(after.contains("title: Hello"), "{after:?}");
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
        assert!(after.contains("tags: [a, b]"), "{after:?}");
        assert!(after.contains("title: Hello"), "{after:?}");
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
        assert!(after.contains("description: A note"), "{after:?}");
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
}
