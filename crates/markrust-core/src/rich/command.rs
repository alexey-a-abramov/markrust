// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rich editing commands. Each command compiles to a byte splice on the
//! source buffer (the single source of truth) and is one undo transaction.

use std::ops::Range;

use crate::document::Document;
use crate::undo::{SelectionSnapshot, TransactionKind};

use super::engine::RichEngine;
use super::escape::{escape_text, EscapeContext};
use super::serialize::serialize_block;
use super::tree::{Block, BlockKind, HeadingStyle, Inline, LinkAttrs, MarkSet, NodeId};

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
    SplitBlock,
    InsertLineBreak,
    ToggleMark(MarkSet),
    ToggleLink,
    SetBlockType(BlockType),
    ToggleBlockquote,
    ToggleList { ordered: bool },
    SetTaskChecked { id: NodeId, checked: bool },
    IndentList,
    OutdentList,
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
    }
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
    let before = caret.snapshot();
    let offset = caret.cursor();
    let source = doc.buffer.content();
    let raw = engine.in_raw_context(offset);
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
    let after = CaretState::collapsed(offset + inserted.len());
    doc.replace_range_tx(offset, offset, &inserted, kind, before, after.snapshot());
    *caret = after;
    engine.sync(doc);
    Ok(RichOutcome::Changed)
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
    let mut range = del_start..del_end;
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
    let mut range = from..to;
    extend_empty_mark_wrappers(&source, engine, &mut range);
    caret.range = range;
    caret.reversed = false;
    delete_range(doc, engine, caret, TransactionKind::Command)
}

/// If a deletion empties a marked run, swallow the surrounding delimiters too.
fn extend_empty_mark_wrappers(source: &str, engine: &RichEngine, range: &mut Range<usize>) {
    let Some(id) = engine.block_at(range.start) else {
        return;
    };
    let Some(block) = engine.block(id) else {
        return;
    };
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
        // Expand to include ASCII delimiter runs immediately outside the text.
        let mut start = source_range.start;
        while start > block.source_range.start {
            let b = source.as_bytes()[start - 1];
            if matches!(b, b'*' | b'_' | b'`' | b'~') {
                start -= 1;
            } else {
                break;
            }
        }
        let mut end = source_range.end;
        while end < block.source_range.end.min(source.len()) {
            let b = source.as_bytes()[end];
            if matches!(b, b'*' | b'_' | b'`' | b'~') {
                end += 1;
            } else {
                break;
            }
        }
        range.start = range.start.min(start);
        range.end = range.end.max(end);
    }
}

fn delete_range(
    doc: &mut Document,
    _engine: &mut RichEngine,
    caret: &mut CaretState,
    kind: TransactionKind,
) -> Result<RichOutcome, RichError> {
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
            let q = in_quote || matches!(b.kind, BlockKind::BlockQuote);
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
    let sel = if caret.range.is_empty() {
        // Toggle the run containing the caret.
        let Some(id) = engine.block_at(caret.cursor()) else {
            return Ok(RichOutcome::Noop);
        };
        let Some(block) = engine.block(id) else {
            return Ok(RichOutcome::Noop);
        };
        let Some(run) = block.inlines.iter().find_map(|i| match i {
            Inline::Run { source_range, .. }
                if source_range.start <= caret.cursor() && caret.cursor() <= source_range.end =>
            {
                Some(source_range.clone())
            }
            _ => None,
        }) else {
            return Ok(RichOutcome::Noop);
        };
        run
    } else {
        caret.range.clone()
    };
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
    let Some(top) = engine.top_level_at(caret.cursor()).cloned() else {
        return Ok(RichOutcome::Noop);
    };
    if top.is_container() && !matches!(top.kind, BlockKind::BlockQuote) {
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
    let Some(top) = engine.top_level_at(caret.cursor()).cloned() else {
        return Ok(RichOutcome::Noop);
    };
    if matches!(top.kind, BlockKind::BlockQuote) {
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
    if caret.range.is_empty() {
        let source = doc.buffer.content();
        let word = word_range(&source, caret.cursor());
        if word.is_empty() {
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
        caret.range = word;
        caret.reversed = false;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rich::engine::RichEngine;
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
        caret.collapse_to(second.start);
        let after = apply(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("*".into()),
        );
        assert!(after.starts_with("hello\n\n"), "prefix kept: {after:?}");
        assert!(
            after[second.start..].contains("\\*") || after.contains("\\*world"),
            "star escaped: {after:?}"
        );
        assert!(
            after.ends_with("world\n") || after.contains("world"),
            "{after:?}"
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
}
