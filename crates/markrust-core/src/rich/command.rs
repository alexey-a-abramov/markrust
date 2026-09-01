// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rich editing commands. Each command compiles to a byte splice on the
//! source buffer (the single source of truth) and is one undo transaction.

use std::ops::Range;

use crate::document::Document;
use crate::undo::{SelectionSnapshot, TransactionKind};

pub use super::engine::code_body_source_map;
use super::engine::{
    blank_caret_gap_after_last, caret_for_click_below_content, expand_mark_delimiters,
    frontmatter_body_start, RichEngine, TablePos,
};
use super::escape::{escape_text, EscapeContext};
use super::input_rules::{input_rule_breaks_table, match_input_rule_with, InputRule};
use super::serialize::serialize_block;
use super::tree::{
    Block, BlockKind, ColumnAlign, Frontmatter, HeadingStyle, Inline, LinkAttrs, MarkSet, NodeId,
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
    if text == "\n" {
        return split_block(doc, engine, caret);
    }
    if text.is_empty() {
        return Ok(RichOutcome::Noop);
    }
    if !caret.range.is_empty() {
        delete_range(doc, engine, caret, TransactionKind::Command)?;
        engine.sync(doc);
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
    let inserted = if raw {
        text.to_string()
    } else {
        let ctx = EscapeContext {
            in_table: engine.in_table(offset),
            at_line_start: offset == 0 || source.as_bytes().get(offset - 1) == Some(&b'\n'),
        };
        escape_text(text, ctx)
    };
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
        return Ok(RichOutcome::Noop);
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

/// Word/line delete boundary kinds for [`delete_to_bound`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteBound {
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
}

/// Option/Ctrl word-delete and Cmd line-delete. A non-empty selection is
/// removed like Backspace; otherwise the caret deletes the visible region up
/// to the previous/next word start/end or the current source line bounds.
fn delete_to_bound(
    doc: &mut Document,
    engine: &mut RichEngine,
    caret: &mut CaretState,
    bound: DeleteBound,
) -> Result<RichOutcome, RichError> {
    if !caret.range.is_empty() {
        return delete_range(doc, engine, caret, TransactionKind::Command);
    }
    let source = doc.buffer.content();
    let cursor = caret.cursor();
    let target = match bound {
        DeleteBound::WordLeft => engine.prev_word_caret(&source, cursor),
        DeleteBound::WordRight => engine.next_word_caret(&source, cursor),
        DeleteBound::LineStart => line_start(&source, cursor),
        DeleteBound::LineEnd => line_end_exclusive(&source, cursor),
    };
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
    if empty_list_line(&source, offset) {
        return outdent_current_list_line(doc, engine, caret);
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
        BlockKind::CodeBlock { .. } | BlockKind::Opaque { .. } => "\n".to_string(),
        BlockKind::ListItem { .. } => list_split_text(&source, engine, leaf, offset),
        _ => {
            if let Some(item) = ancestor_list_item(engine, leaf_id) {
                list_split_text(&source, engine, item, offset)
            } else if ancestor_is_quote(engine, leaf_id) {
                "\n>\n> ".to_string()
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

fn list_split_text(source: &str, _engine: &RichEngine, item: &Block, _offset: usize) -> String {
    let slice = source.get(item.source_range.clone()).unwrap_or_default();
    let first_line = slice.split('\n').next().unwrap_or(slice);
    let prefix = list_marker_prefix(first_line).unwrap_or_else(|| "- ".to_string());
    format!("\n{prefix}")
}

fn empty_list_line(source: &str, offset: usize) -> bool {
    let line = current_line(source, offset);
    match list_marker_prefix(line) {
        Some(prefix) => line[prefix.len()..].trim().is_empty(),
        None => false,
    }
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
    splice(doc, caret, offset, offset, "\\\n", TransactionKind::Command);
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
        // Typora / source wrap: empty Cmd-B/I/E inserts `****` / `**` / `` ` ` `
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
    // Typora: Cmd-K inside `$math$` / `` `code` `` / `[[wiki]]` / `:emoji:`
    // must not wrap the word (or the atom) in link brackets.
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
    // Source wrap leaves the caret in the URL `()` after wrapping a selection
    // (or a word), so Cmd-K can type the destination right away.
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
    if engine.in_raw_context(caret.cursor()) {
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
    outdent_current_list_line(doc, engine, caret)
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
    if list_marker_prefix(line).is_none() {
        let Some(id) = engine.block_at(offset) else {
            return Ok(RichOutcome::Noop);
        };
        let Some(item) = ancestor_list_item(engine, id).cloned() else {
            return Ok(RichOutcome::Noop);
        };
        let range = item_visual_range(&source, &item);
        let slice = source.get(range.clone()).unwrap_or("");
        let first = slice.split('\n').next().unwrap_or(slice);
        let indent = first
            .bytes()
            .take_while(|b| *b == b' ' || *b == b'\t')
            .count();
        if indent >= 2 {
            return rewrite_range(doc, engine, caret, range, &unprefix_item_lines(slice, 2));
        }
        return Ok(RichOutcome::Noop);
    }
    let indent = line
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    if indent >= 2 {
        let new_line = unprefix_item_lines(line, 2);
        return rewrite_range(doc, engine, caret, start..start + line.len(), &new_line);
    }
    // Exit the list: drop this empty (or top-level) item line.
    let prefix = list_marker_prefix(line).unwrap_or_default();
    let rest = line.get(prefix.len()..).unwrap_or("");
    if rest.trim().is_empty() {
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
    let new_line = rest.to_string();
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
    let trailing_nl = slice.ends_with('\n');
    let body = slice.strip_suffix('\n').unwrap_or(slice);
    let mut out = String::new();
    for (i, line) in body.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !line.is_empty() {
            out.push_str(&pad);
        }
        out.push_str(line);
    }
    if trailing_nl {
        out.push('\n');
    }
    out
}

fn unprefix_item_lines(slice: &str, n: usize) -> String {
    let trailing_nl = slice.ends_with('\n');
    let body = slice.strip_suffix('\n').unwrap_or(slice);
    let mut out = String::new();
    for (i, line) in body.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if let Some(rest) = line.strip_prefix('\t') {
            out.push_str(rest);
            continue;
        }
        let mut take = 0usize;
        for (idx, b) in line.bytes().enumerate() {
            if b == b' ' && idx < n {
                take += 1;
            } else {
                break;
            }
        }
        out.push_str(&line[take..]);
    }
    if trailing_nl {
        out.push('\n');
    }
    out
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
    use crate::rich::engine::RichEngine;
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

    fn typed_line_is_paragraph(line: &str) -> bool {
        line.contains('x')
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
    fn toggle_link_wraps_selection() {
        let (mut doc, mut engine, mut caret) = setup("hello\n");
        caret.range = 0..5;
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        assert!(
            after.contains("[hello]("),
            "expected markdown link, got {after:?}"
        );
    }

    #[test]
    fn toggle_link_on_selection_wraps_with_empty_url_caret() {
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
    fn toggle_link_on_word_wraps_with_empty_url_caret() {
        // Cmd-K on a caret inside a word wraps the whole word, not just the
        // typed character, and leaves the caret in the empty URL `()`.
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
    fn empty_destination_becomes_empty_parens() {
        // A just-wrapped link with no url serializes its destination as bare
        // `()` (Typora), never `<>`.
        let (mut doc, mut engine, mut caret) = setup("hello");
        caret.range = 0..5;
        let after = apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        assert_eq!(
            after, "[hello]()",
            "empty dest must serialize bare, got {after:?}"
        );
        assert!(
            !after.contains("<>"),
            "empty dest must not be <>, got {after:?}"
        );
    }

    #[test]
    fn empty_destination_with_url_fills_it() {
        // Typing a URL into an existing empty `()` fills the destination.
        let source = "[hello]()";
        let (mut doc, mut engine, mut caret) = setup(source);
        let at = source.find("](").expect("dest") + 2; // after `](`
        caret.collapse_to(at);
        apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("https://e.com".into()),
        );
        assert_eq!(
            doc.buffer.content(),
            "[hello](https://e.com)",
            "typing a URL must fill the empty (), got {:?}",
            doc.buffer.content()
        );
    }

    #[test]
    fn empty_angle_destination_becomes_empty_parens() {
        // A leftover `<>` empty destination collapses back to `()` when the
        // link is unwrapped and rewrapped (the empty URL no longer needs <>).
        let source = "[hello](<>)";
        let (mut doc, mut engine, mut caret) = setup(source);
        caret.range = 0..5;
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        assert_eq!(doc.buffer.content(), "hello", "unwrap must drop the link");
        apply(&mut doc, &mut engine, &mut caret, RichCommand::ToggleLink);
        assert_eq!(
            doc.buffer.content(),
            "[hello]()",
            "empty <> dest must become empty parens, got {:?}",
            doc.buffer.content()
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
    fn typing_in_autolink_does_not_double_wrap() {
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
            "typing inside an autolink must keep the url and not double-wrap, got {after:?}"
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

        let source = "hello\nworld extra";
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
    fn block_commands_no_op_in_table() {
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
    fn wrap_commands_clamp_to_table_cell() {
        let source = table_source();
        for (cmd, label) in [
            (RichCommand::ToggleMark(MarkSet::BOLD), "bold"),
            (RichCommand::ToggleMark(MarkSet::ITALIC), "italic"),
            (RichCommand::ToggleMark(MarkSet::CODE), "code"),
        ] {
            // A document-wide selection that starts in the table stays in the cell.
            let (mut doc, mut engine, mut caret) = setup(source);
            caret.collapse_to(source.find('a').expect("header a"));
            caret.range = 0..source.len();
            caret.reversed = false;
            apply(&mut doc, &mut engine, &mut caret, cmd.clone());
            let after = doc.buffer.content();
            engine.sync(&doc);
            assert_gfm_table_survives(&engine, &after, label);
        }

        // Cmd-K on a document-wide selection starting in a table stays in the cell.
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

        // In-cell selection still wraps that cell.
        let a = source.find('a').expect("header a");
        let (mut doc, mut engine, mut caret) = setup(source);
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

        // Selecting the whole doc from a paragraph still wraps that paragraph.
        let mixed = "hello\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (mut doc, mut engine, mut caret) = setup(mixed);
        caret.range = 0..mixed.len();
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
    fn select_all_in_empty_table_cell_selects_that_cell() {
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

        // Empty cell: collapsed body range equals the caret, so the prior-cell
        // latch decides first vs second Cmd-A.
        let empty = "|| b |\n|---|---|\n| 1 | 2 |\n";
        let (_doc, engine, _) = setup(empty);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Table { .. }),
            "fixture must parse as a table, got {:?}",
            engine.tree().blocks[0].kind
        );
        let cell = first_empty_cell_body(&engine, empty);
        assert!(
            cell.is_empty(),
            "empty header cell body must be collapsed, got {cell:?}"
        );
        let first = table_select_all_range(&engine, empty, &cell, None)
            .expect("first Cmd-A on an empty cell must still select the cell");
        assert_eq!(first, cell, "first SelectAll must be the empty cell body");
        assert!(
            table_select_all_range(&engine, empty, &first, Some(&first)).is_none(),
            "second SelectAll (empty cell already latched) must fall through to the document"
        );
        assert!(
            table_select_all_range(&engine, empty, &(0..empty.len()), Some(&first)).is_none(),
            "SelectAll must not shrink a whole-document selection back to a cell"
        );
    }

    #[test]
    fn drag_select_across_pipe_clamps_to_cell() {
        let source = table_source();
        let a = source.find('a').expect("header a");
        let after_b = source.find('b').expect("header b") + 1;
        let (mut doc, mut engine, mut caret) = setup(source);
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
}
