// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The engine keeps a [`RichTree`] in sync with a [`Document`] and answers
//! the view layer's position questions: which block a byte lives in, where a
//! caret may legally sit in WYSIWYG mode, and how the tree maps to source
//! lines. This is the contract the GPUI view consumes.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::document::Document;

use super::entities::{
    backslash_escape_closed_suffix, character_reference_closed_suffix,
    character_reference_visible_range, is_decoded_backslash_escape, is_decoded_character_reference,
};
use super::import::import_markdown;
use super::tree::{
    alert_title_range, code_span_visible_range, emoji_visible_range, expand_link_and_html_chrome,
    expand_marks_and_link_chrome, link_reference_def_chrome, markdown_link_chrome,
    markdown_link_dests, math_visible_range, quoted_title_inner, toc_visible_range,
    wiki_visible_range, Block, BlockKind, IdGen, Inline, MarkSet, NodeId, PrefixBlank, RichTree,
};

/// Caret snapping direction when a byte falls on delimiter bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bias {
    Left,
    Right,
}

/// One top-level block's span, for side-by-side scroll/caret sync.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockSpan {
    pub id: NodeId,
    pub source_range: Range<usize>,
    /// 0-based source line range (start inclusive, end exclusive).
    pub source_lines: Range<usize>,
}

/// Report of which top-level blocks changed in the last [`RichEngine::sync`],
/// as a splice: `range` old block indices were replaced by `new_count` blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSplice {
    pub range: Range<usize>,
    pub new_count: usize,
}

/// Location of a table cell containing a source byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TablePos {
    pub table_id: NodeId,
    pub row: usize,
    pub col: usize,
    pub n_rows: usize,
    pub n_cols: usize,
}

#[derive(Debug, Default)]
pub struct RichEngine {
    tree: RichTree,
    /// Last synced source, so caret snap can parse dest `(url "title")`
    /// wrapping without a `Document` argument.
    source: String,
    ids: IdGen,
    synced_revision: Option<u64>,
    last_splice: Option<BlockSplice>,
}

impl RichEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reparse if the document changed since the last sync. Top-level blocks
    /// whose source slice is unchanged keep their [`NodeId`]s; the changed
    /// middle window is reported via [`RichEngine::last_splice`].
    pub fn sync(&mut self, doc: &Document) -> &RichTree {
        let revision = doc.revision();
        if self.synced_revision == Some(revision) {
            return &self.tree;
        }
        let source = doc.buffer.content();
        let mut new_tree = import_markdown(&source, &mut self.ids);
        self.last_splice = Some(reconcile_ids(&self.tree, &mut new_tree, &source));
        self.tree = new_tree;
        self.source = source;
        self.synced_revision = Some(revision);
        &self.tree
    }

    /// Force the next [`RichEngine::sync`] to reparse.
    pub fn invalidate(&mut self) {
        self.synced_revision = None;
    }

    pub fn tree(&self) -> &RichTree {
        &self.tree
    }

    /// The splice produced by the most recent re-sync (None when the last
    /// sync was a no-op). Drives the view's virtualized-list invalidation.
    pub fn last_splice(&self) -> Option<&BlockSplice> {
        self.last_splice.as_ref()
    }

    pub fn block(&self, id: NodeId) -> Option<&Block> {
        fn find(blocks: &[Block], id: NodeId) -> Option<&Block> {
            for b in blocks {
                if b.id == id {
                    return Some(b);
                }
                if let Some(found) = find(&b.children, id) {
                    return Some(found);
                }
            }
            None
        }
        find(&self.tree.blocks, id)
    }

    /// Top-level block containing `byte`.
    pub fn top_level_at(&self, byte: usize) -> Option<&Block> {
        if blank_caret_gap_at(&self.tree, byte).is_some() {
            return None;
        }
        let mut best = None;
        for b in &self.tree.blocks {
            if b.source_range.start <= byte && byte <= b.source_range.end {
                best = Some(b);
            } else if b.source_range.start > byte {
                break;
            }
        }
        best.or_else(|| {
            self.tree
                .blocks
                .iter()
                .rev()
                .find(|b| b.source_range.start <= byte)
                .or_else(|| self.tree.blocks.first())
        })
    }

    /// True when `byte` sits in a code block, opaque block, or inline code run.
    pub fn in_raw_context(&self, byte: usize) -> bool {
        let Some(id) = self.block_at(byte) else {
            return false;
        };
        let Some(block) = self.block(id) else {
            return false;
        };
        match &block.kind {
            BlockKind::CodeBlock { .. }
            | BlockKind::Opaque { .. }
            | BlockKind::Alert { .. }
            | BlockKind::LinkReferenceDefinition { .. } => true,
            _ => block.inlines.iter().any(|inline| match inline {
                Inline::Run {
                    source_range,
                    marks,
                    ..
                } => {
                    source_range.start <= byte
                        && byte <= source_range.end
                        && marks.contains(MarkSet::CODE)
                }
                Inline::OpaqueInline { source_range, .. } => {
                    source_range.start <= byte && byte <= source_range.end
                }
                Inline::Math { source_range, .. }
                | Inline::WikiLink { source_range, .. }
                | Inline::Emoji { source_range, .. } => {
                    source_range.start <= byte && byte <= source_range.end
                }
                _ => false,
            }),
        }
    }

    /// Cell containing `byte`, if any.
    pub fn table_pos(&self, byte: usize) -> Option<TablePos> {
        fn walk(blocks: &[Block], byte: usize) -> Option<TablePos> {
            for b in blocks {
                if matches!(b.kind, BlockKind::Table { .. })
                    && b.source_range.start <= byte
                    && byte <= b.source_range.end
                {
                    let n_rows = b.children.len();
                    let n_cols = b.children.first().map(|r| r.children.len()).unwrap_or(0);
                    for (ri, row) in b.children.iter().enumerate() {
                        for (ci, cell) in row.children.iter().enumerate() {
                            if cell.source_range.start <= byte && byte <= cell.source_range.end {
                                return Some(TablePos {
                                    table_id: b.id,
                                    row: ri,
                                    col: ci,
                                    n_rows,
                                    n_cols,
                                });
                            }
                        }
                    }
                    if n_rows > 0 && n_cols > 0 {
                        return Some(TablePos {
                            table_id: b.id,
                            row: n_rows.saturating_sub(1),
                            col: n_cols.saturating_sub(1),
                            n_rows,
                            n_cols,
                        });
                    }
                }
                if let Some(found) = walk(&b.children, byte) {
                    return Some(found);
                }
            }
            None
        }
        walk(&self.tree.blocks, byte)
    }

    /// Editable source range of the table cell containing `byte`.
    ///
    /// GFM `|` separators are not part of the range. A byte that sits only on
    /// a pipe (not in any cell) returns `None` — unlike [`Self::table_pos`],
    /// there is no last-cell fallback.
    pub fn cell_edit_range(&self, byte: usize, source: &str) -> Option<Range<usize>> {
        table_cell_copy_range(&self.tree.blocks, byte, source)
    }

    /// Source caret for the start of cell `(row, col)` in `table_id`.
    pub fn cell_caret(&self, table_id: NodeId, row: usize, col: usize) -> Option<usize> {
        let table = self.block(table_id)?;
        let cell = table.children.get(row)?.children.get(col)?;
        Some(
            inline_ranges(cell, &self.source)
                .first()
                .map(|r| r.start)
                .unwrap_or(cell.source_range.start),
        )
    }

    /// True when `byte` is inside a table cell.
    pub fn in_table(&self, byte: usize) -> bool {
        fn walk(blocks: &[Block], byte: usize) -> bool {
            for b in blocks {
                if b.source_range.start <= byte && byte <= b.source_range.end {
                    if matches!(b.kind, BlockKind::TableCell | BlockKind::Table { .. }) {
                        return true;
                    }
                    if walk(&b.children, byte) {
                        return true;
                    }
                }
            }
            false
        }
        walk(&self.tree.blocks, byte)
    }

    /// Deepest leaf block containing `byte`.
    ///
    /// Comrak has no empty-paragraph node, so leading newlines, blank lines
    /// between top-level blocks (a standard `\n\n` separator, plus extras),
    /// and a trailing blank after the last block are not inside any range.
    /// Those offsets stay unmapped (Typora: caret on the blank) instead of
    /// snapping to a neighbor. Other positions after the last block still
    /// fall back to nearest.
    pub fn block_at(&self, byte: usize) -> Option<NodeId> {
        if blank_caret_gap_at(&self.tree, byte).is_some() {
            return None;
        }
        fn descend(blocks: &[Block], byte: usize) -> Option<NodeId> {
            let mut best: Option<&Block> = None;
            for b in blocks {
                if b.source_range.start <= byte && byte <= b.source_range.end {
                    best = Some(b);
                }
            }
            let b = best?;
            descend(&b.children, byte).or(Some(b.id))
        }
        if let Some(id) = descend(&self.tree.blocks, byte) {
            return Some(id);
        }
        // Nearest block: last one starting before `byte`, else first.
        let mut prev = None;
        for b in &self.tree.blocks {
            if b.source_range.start <= byte {
                prev = Some(b.id);
            }
        }
        prev.or_else(|| self.tree.blocks.first().map(|b| b.id))
    }

    /// Valid WYSIWYG caret positions in a leaf block are the bytes covered by
    /// its inline runs (delimiters are not editable positions). Snap `byte`
    /// to the nearest valid position in the given direction. Offsets on a
    /// Comrak-less blank (leading newlines, a block separator, or a trailing
    /// blank after the last block) stay on that painted line (EOF on a
    /// trailing blank maps to the gap start, not into the last paragraph).
    /// Empty `> ` / `- ` / `: ` lines sit after the prefix (not on chrome).
    pub fn snap_caret(&self, byte: usize, bias: Bias) -> usize {
        let fm_end = frontmatter_body_start(&self.tree);
        if fm_end > 0 && byte < fm_end {
            return self.snap_caret(fm_end, bias);
        }
        if let Some(gap) = blank_caret_gap_at(&self.tree, byte) {
            // One painted line: extra trailing `\n`s share the gap start
            // (EOF must not snap into the last paragraph).
            if gap.end == self.tree.source_len {
                return gap.start;
            }
            return byte;
        }
        if let Some(home) = self.prefix_blank_home_at(byte) {
            return home;
        }
        let Some(id) = self.block_at(byte) else {
            return byte;
        };
        let Some(block) = self.block(id) else {
            return byte;
        };
        let ranges = inline_ranges(block, &self.source);
        if ranges.is_empty() {
            return block.source_range.start;
        }
        if let Some(home) = footnote_snap_home(block, byte, bias) {
            return home;
        }
        if let Some(home) = markdown_link_dest_snap(&self.source, block, byte) {
            return home;
        }
        // `[ref]: url` — `]: ` between label and dest is dest chrome. Click
        // uses Bias::Left, which would otherwise sit on `]` / `:`.
        if matches!(block.kind, BlockKind::LinkReferenceDefinition { .. }) {
            if let Some(home) = link_ref_def_gap_snap(&ranges, byte) {
                return home;
            }
            if let Some(home) = link_ref_def_title_snap(&self.source, block, byte) {
                return home;
            }
        }
        for r in &ranges {
            if r.start <= byte && byte <= r.end {
                if range_is_atomic_widget(block, r) && byte > r.start && byte < r.end {
                    return match bias {
                        Bias::Left => r.start,
                        Bias::Right => r.end,
                    };
                }
                // Compact GFM `foo|bar`: the pipe *is* the left cell's exclusive
                // end. It is dest chrome, not a Right-bias caret home (Home/click
                // skip onto the next cell). Left-bias keeps the insert home.
                if is_unescaped_pipe(&self.source, byte) && self.in_table(byte) {
                    match bias {
                        Bias::Right => continue,
                        Bias::Left if byte == r.end => return byte,
                        Bias::Left => continue,
                    }
                }
                return byte;
            }
        }
        match bias {
            Bias::Left => ranges
                .iter()
                .rev()
                .find(|r| r.end <= byte)
                .map(|r| r.end)
                .unwrap_or(ranges[0].start),
            Bias::Right => ranges
                .iter()
                .find(|r| r.start >= byte)
                .map(|r| r.start)
                .unwrap_or_else(|| {
                    let end = ranges.last().unwrap().end;
                    if is_unescaped_pipe(&self.source, end) && self.in_table(end) {
                        (end + 1).min(self.source.len())
                    } else {
                        end
                    }
                }),
        }
    }

    /// One visible-grapheme step left from `byte`, skipping delimiter gaps
    /// between runs. A blank gap between blocks is one stop (extra unused
    /// newlines are not extra steps).
    /// Quote markers, list markers, HTML tags, and GFM table `|` are not
    /// caret stops (same prefixes click/IME skip). Empty `> ` / `- `
    /// lines are one stop.
    pub fn prev_caret(&self, source: &str, byte: usize) -> usize {
        let byte = self.snap_caret(byte, Bias::Left);
        if let Some(gap) = blank_caret_gap_at(&self.tree, byte) {
            return self.snap_caret(gap.start.saturating_sub(1), Bias::Left);
        }
        if let Some(blank) = self.prefix_blank_at(byte) {
            if byte == blank.home {
                return self.snap_caret(blank.line.start.saturating_sub(1), Bias::Left);
            }
        }
        let Some(block) = self.block_at(byte).and_then(|id| self.block(id)) else {
            return byte.saturating_sub(1);
        };
        let ranges = inline_ranges(block, source);
        let Some(idx) = caret_range_index(&ranges, source, block, byte) else {
            return byte;
        };
        let stepped = if byte > ranges[idx].start {
            step_left_caret(source, block, &ranges[idx], byte)
        } else if idx > 0 {
            let prev = &ranges[idx - 1];
            step_left_caret(source, block, prev, prev.end.max(byte))
        } else if let Some(home) = self.prev_prefix_blank_home(byte, block.source_range.start) {
            home
        } else if let Some(gap) = blank_caret_gap_ending_at(&self.tree, block.source_range.start) {
            gap.start
        } else {
            match self.block_before(block.id) {
                Some(pb) => {
                    if let Some(home) = self.prev_prefix_blank_home(byte, pb.source_range.end) {
                        home
                    } else {
                        let pranges = inline_ranges(pb, source);
                        pranges.last().map(|r| r.end).unwrap_or(pb.source_range.end)
                    }
                }
                None => byte,
            }
        };
        let clamped = self.clamp_raw_prefix(source, stepped, Bias::Left);
        if clamped == byte {
            if let Some(raw) = raw_leaf_at(self, byte) {
                let first =
                    self.clamp_raw_prefix(source, raw_body_range(raw, source).start, Bias::Right);
                if byte <= first {
                    if let Some(gap) = blank_caret_gap_ending_at(&self.tree, raw.source_range.start)
                    {
                        return gap.start;
                    }
                    return match self.block_before(raw.id) {
                        Some(pb) => {
                            let pranges = inline_ranges(pb, source);
                            pranges.last().map(|r| r.end).unwrap_or(pb.source_range.end)
                        }
                        None => byte,
                    };
                }
            }
        }
        self.skip_table_pipe_stop(source, clamped, Bias::Left)
    }

    /// One visible-grapheme step right from `byte` (mirror of `prev_caret`).
    pub fn next_caret(&self, source: &str, byte: usize) -> usize {
        let byte = self.snap_caret(byte, Bias::Right);
        if let Some(gap) = blank_caret_gap_at(&self.tree, byte) {
            return self.snap_caret(gap.end.min(source.len()), Bias::Right);
        }
        if let Some(blank) = self.prefix_blank_at(byte) {
            if byte == blank.home {
                let after = blank.line.end;
                let next = if source.as_bytes().get(after) == Some(&b'\n') {
                    after + 1
                } else {
                    after
                };
                return self.snap_caret(next.min(source.len()), Bias::Right);
            }
        }
        let Some(block) = self.block_at(byte).and_then(|id| self.block(id)) else {
            return (byte + 1).min(source.len());
        };
        let ranges = inline_ranges(block, source);
        let Some(idx) = caret_range_index(&ranges, source, block, byte) else {
            return byte;
        };
        let stepped = if byte < ranges[idx].end {
            step_right_caret(source, block, byte, &ranges[idx])
        } else if idx + 1 < ranges.len() {
            let next = &ranges[idx + 1];
            if range_is_atomic_widget(block, next) && byte <= next.start {
                next.end
            } else if next.start > byte {
                // Gap is quote/list chrome (`\n>` / `- `), not a caret stop.
                // Continuation indent (`  world`) is also skipped so Right
                // from `hello` lands on `w`.
                if let Some(home) = self.next_prefix_blank_home(byte, next.start) {
                    home
                } else {
                    self.clamp_raw_prefix(source, next.start, Bias::Right)
                }
            } else {
                step_right_caret(source, block, byte.max(next.start), next)
            }
        } else if let Some(home) = self.next_prefix_blank_home(
            byte,
            self.block_after(block.id)
                .map(|nb| nb.source_range.start)
                .unwrap_or(source.len()),
        ) {
            home
        } else {
            match self.block_after(block.id) {
                Some(nb) => {
                    if let Some(gap) = blank_caret_gap_ending_at(&self.tree, nb.source_range.start)
                    {
                        gap.start
                    } else {
                        let nranges = inline_ranges(nb, source);
                        nranges
                            .first()
                            .map(|r| r.start)
                            .unwrap_or(nb.source_range.start)
                    }
                }
                None => blank_caret_gap_after_last(&self.tree)
                    .map(|gap| gap.start)
                    .unwrap_or(byte),
            }
        };
        self.skip_table_pipe_stop(
            source,
            self.clamp_raw_prefix(source, stepped, Bias::Right),
            Bias::Right,
        )
    }

    /// Compact GFM `foo|bar`: the pipe is the left cell's exclusive end, so
    /// snap Left keeps it as an insert home. Left/Right must not *stop* there.
    fn skip_table_pipe_stop(&self, source: &str, byte: usize, bias: Bias) -> usize {
        let mut at = byte.min(source.len());
        for _ in 0..8 {
            if !self.in_table(at) || !is_unescaped_pipe(source, at) {
                return at;
            }
            let next = match bias {
                Bias::Right => {
                    let fwd = (at + 1).min(source.len());
                    self.clamp_raw_prefix(source, self.snap_caret(fwd, Bias::Right), Bias::Right)
                }
                Bias::Left => {
                    let back = step_left_in_slice(source, 0, at);
                    self.clamp_raw_prefix(source, back, Bias::Left)
                }
            };
            if next == at {
                return at;
            }
            at = next;
        }
        at
    }

    /// Option-Right: end of the current (or next) visible word / punct run.
    /// Hidden delimiter bytes (`**`, `*`, ticks) are not word stops.
    pub fn next_word_caret(&self, source: &str, byte: usize) -> usize {
        let mut pos = self.clamp_raw_prefix(
            source,
            self.snap_caret(byte.min(source.len()), Bias::Right),
            Bias::Right,
        );
        pos = self.skip_hidden_marks(source, pos, true);
        if pos >= source.len() {
            return pos;
        }
        let mut prev_kind = word_kind_at(source, pos);
        let guard = source.len().saturating_add(4);
        for _ in 0..guard {
            let next = self.step_visible_caret(source, pos, true);
            if next == pos {
                return pos;
            }
            let next_kind = word_kind_at(source, next);
            if prev_kind != next_kind && prev_kind != WordCharKind::Whitespace {
                return next;
            }
            pos = next;
            prev_kind = next_kind;
        }
        pos
    }

    /// Option-Left: start of the current (or previous) visible word / punct run.
    pub fn prev_word_caret(&self, source: &str, byte: usize) -> usize {
        let mut pos = self.clamp_raw_prefix(
            source,
            self.snap_caret(byte.min(source.len()), Bias::Left),
            Bias::Left,
        );
        pos = self.skip_hidden_marks(source, pos, false);
        if pos == 0 {
            return 0;
        }
        pos = self.step_visible_caret(source, pos, false);
        let guard = source.len().saturating_add(4);
        for _ in 0..guard {
            let prev = self.step_visible_caret(source, pos, false);
            if prev == pos {
                return pos;
            }
            let right_kind = word_kind_at(source, pos);
            let left_kind = word_kind_at(source, prev);
            if left_kind != right_kind && right_kind != WordCharKind::Whitespace {
                return pos;
            }
            pos = prev;
        }
        pos
    }

    /// One grapheme step that skips exclusive-end delimiter bytes (hidden
    /// `**` / `*` / ticks). Real punctuation (`hello, world`) is kept: those
    /// move by one grapheme. Mark wrappers jump more than one byte.
    fn step_visible_caret(&self, source: &str, pos: usize, forward: bool) -> usize {
        let mut at = pos;
        for _ in 0..8 {
            let next = if forward {
                self.next_caret(source, at)
            } else {
                self.prev_caret(source, at)
            };
            if next == at {
                return at;
            }
            if self.is_hidden_mark_caret(source, next) {
                at = next;
                continue;
            }
            return next;
        }
        at
    }

    fn is_hidden_mark_caret(&self, source: &str, at: usize) -> bool {
        let Some(c) = source.get(at..).and_then(|s| s.chars().next()) else {
            return false;
        };
        if !matches!(c, '*' | '_' | '`' | '~' | '[' | ']' | '(' | ')' | '!') {
            return false;
        }
        let skipped = self.next_caret(source, at);
        skipped > at + c.len_utf8()
    }

    fn skip_hidden_marks(&self, source: &str, mut pos: usize, forward: bool) -> usize {
        for _ in 0..8 {
            if !self.is_hidden_mark_caret(source, pos) {
                return pos;
            }
            let next = if forward {
                self.next_caret(source, pos)
            } else {
                self.prev_caret(source, pos)
            };
            if next == pos {
                return pos;
            }
            pos = next;
        }
        pos
    }

    fn prefix_blank_at(&self, byte: usize) -> Option<&PrefixBlank> {
        self.tree
            .empty_prefix_homes
            .iter()
            .find(|blank| blank.contains(byte))
    }

    fn prefix_blank_home_at(&self, byte: usize) -> Option<usize> {
        self.prefix_blank_at(byte).map(|blank| blank.home)
    }

    fn next_prefix_blank_home(&self, byte: usize, until: usize) -> Option<usize> {
        self.tree
            .empty_prefix_homes
            .iter()
            .filter(|blank| blank.home > byte && blank.home <= until)
            .map(|blank| blank.home)
            .min()
    }

    fn prev_prefix_blank_home(&self, byte: usize, after: usize) -> Option<usize> {
        self.tree
            .empty_prefix_homes
            .iter()
            .filter(|blank| blank.home < byte && blank.home >= after)
            .map(|blank| blank.home)
            .max()
    }

    /// If `byte` sits on a quote/list prefix (fence, HTML, quoted paragraph,
    /// or list item), move onto the editable body of that line (`Right`) or
    /// the previous line's terminator (`Left`). Unquoted 1:1 bodies are
    /// unchanged. HTML-block tag bytes (`<div>`, `</div>`) are skipped the
    /// same way.
    pub fn clamp_raw_prefix(&self, source: &str, byte: usize, bias: Bias) -> usize {
        let mut at = byte.min(source.len());
        // Bounded: dest chrome after `> ` / `- ` used to bounce prefix ↔ `$`
        // / `[[` forever (`> $$\n> E=mc^2`).
        for _ in 0..16 {
            let next = self.clamp_raw_prefix_once(source, at, bias);
            if next == at {
                return at;
            }
            at = next;
        }
        at
    }

    fn clamp_raw_prefix_once(&self, source: &str, byte: usize, bias: Bias) -> usize {
        let Some(id) = self.block_at(byte) else {
            return byte;
        };
        let Some(block) = self.block(id) else {
            return byte;
        };
        let body = raw_body_range(block, source);
        let mut at = byte.clamp(body.start, body.end);
        // Painted rules (`---` / `* * *` / `<hr>`) are not list items: `* `
        // / `- ` on that line is dest chrome, not a marker to skip onto.
        if html_block_atomic_range(block).is_none()
            && !matches!(block.kind, BlockKind::ThematicBreak)
        {
            let prefix = raw_container_prefix(source, block);
            let fence_offset = match &block.kind {
                BlockKind::CodeBlock { fence: Some(f), .. } => f.fence_offset,
                _ => 0,
            };
            if !prefix.is_empty() || fence_offset > 0 {
                let ls = source_line_start(source, at);
                let le = source_line_end_exclusive(source, at);
                let line = &source[ls..le];
                let skip = skip_line_prefix_and_fence(line, &prefix, fence_offset);
                if skip > 0 {
                    let content = (ls + skip).min(le).clamp(body.start, body.end);
                    if at < content {
                        at = match bias {
                            Bias::Right => content,
                            Bias::Left => {
                                if ls > body.start {
                                    ls.saturating_sub(1).clamp(body.start, body.end)
                                } else {
                                    content
                                }
                            }
                        };
                    }
                }
            }
        }
        if matches!(block.kind, BlockKind::Opaque { .. })
            && html_block_atomic_range(block).is_none()
        {
            at = clamp_html_tag(source, at, body, bias);
        }
        if let BlockKind::Alert {
            tag_range,
            chrome_range,
            ..
        } = &block.kind
        {
            if at >= tag_range.start && at < tag_range.end {
                let dest = alert_title_range(tag_range, chrome_range)
                    .map(|r| r.start)
                    .or_else(|| {
                        inline_ranges(block, source)
                            .into_iter()
                            .find(|r| r.start >= tag_range.end)
                            .map(|r| r.start)
                    })
                    .unwrap_or(tag_range.end);
                if at < dest {
                    at = dest;
                }
            }
        }
        let skipped = self.skip_inline_delimiter_chrome(source, at, bias);
        if skipped != at {
            return skipped;
        }
        // GFM `|` / alignment `|---|` are dest chrome: Home/click skip onto
        // the painted cell the same way `[` / `>` skip onto body text.
        // Escaped `\|` is cell text (a literal pipe), not dest chrome — but
        // the caret must not sit *between* `\` and `|`.
        if self.in_table(at) {
            if let Some(pair) = escaped_pipe_pair_at(source, at) {
                if at > pair.start && at < pair.end {
                    let snapped = match bias {
                        Bias::Right => pair.end.min(source.len()),
                        Bias::Left => pair.start,
                    };
                    if snapped != at {
                        return snapped;
                    }
                }
            }
            let on_pipe = is_unescaped_pipe(source, at);
            let outside_cell = self.cell_edit_range(at, source).is_none();
            if on_pipe || outside_cell {
                let snapped = self.snap_caret(at, bias);
                if snapped != at {
                    return snapped;
                }
            }
        }
        at
    }

    /// Typora Home: first visible caret home on the source line. In a GFM
    /// table that is the current cell, not the first cell of the row.
    pub fn line_start_caret(&self, source: &str, cursor: usize) -> usize {
        let line_start = source_line_start(source, cursor);
        let start = self
            .cell_line_bound(source, cursor)
            .map(|cell| cell.start.max(line_start).min(cell.end))
            .unwrap_or(line_start);
        self.clamp_raw_prefix(source, self.snap_caret(start, Bias::Right), Bias::Right)
    }

    /// Typora End: last visible caret home on the source line. Hidden
    /// `[label](url)` dest is not a line-end stop; dest inner is when the
    /// caret is already in dest. HTML `</a>` / comments are the inner insert
    /// home, not dest `href`. Last-in-line `&amp;` / `\*` land after the
    /// painted glyph, not inside `amp;` / on `*`. Last-in-line `[^1]` lands
    /// after the widget, not on the label start. Linked `[![alt](img)](url)`
    /// stays on wrapping `]`, not dest. In a GFM table that is the current
    /// cell.
    pub fn line_end_caret(&self, source: &str, cursor: usize) -> usize {
        let line_end = source_line_end_exclusive(source, cursor);
        let end = self
            .cell_line_bound(source, cursor)
            .map(|cell| cell.end.min(line_end).max(cell.start))
            .unwrap_or(line_end);
        self.visible_end_caret(source, cursor, end)
    }

    /// Cell for Home/End. A caret on the exclusive-end `|` (compact-table
    /// insert home) still belongs to the left cell.
    fn cell_line_bound(&self, source: &str, cursor: usize) -> Option<Range<usize>> {
        self.cell_edit_range(cursor, source).or_else(|| {
            cursor
                .checked_sub(1)
                .and_then(|prev| self.cell_edit_range(prev, source))
        })
    }

    /// Document End: last visible caret home in the file (same dest skip as
    /// line End).
    pub fn document_end_caret(&self, source: &str, cursor: usize) -> usize {
        self.visible_end_caret(source, cursor, source.len())
    }

    fn visible_end_caret(&self, source: &str, cursor: usize, bound: usize) -> usize {
        let mut bound = bound.min(source.len());
        if bound > 0 && source.as_bytes()[bound - 1] == b'\r' {
            bound -= 1;
        }
        let at = self.clamp_raw_prefix(source, self.snap_caret(bound, Bias::Left), Bias::Left);
        self.skip_unrevealed_link_dest(source, cursor, at)
    }

    /// End/DocumentEnd must not jump into hidden dest `(url)` / `[ref]` /
    /// HTML `<a href>`. A caret already in dest stays on dest inner (URL or
    /// title). Images are atomic: End after the widget is left alone. Empty
    /// `()` dest still counts as dest wrapping. HTML `</a>` / `</b>` /
    /// comments are the inner insert home (like `]`), not prefix chrome to
    /// retreat through. Last-in-line `&amp;` / `\*` restore the insert home
    /// after the painted glyph, not `amp;` dest chrome. Last-in-line `[^1]`
    /// restores after `]`. Wrapping dest of `[![alt](img)](url)` is a link
    /// dest, not the image's own dest.
    fn skip_unrevealed_link_dest(&self, source: &str, cursor: usize, at: usize) -> usize {
        fn dest_belongs_to_image(source: &str, block: &Block, dest_start: usize) -> bool {
            block.inlines.iter().any(|inline| {
                let Inline::Image { source_range, .. } = inline else {
                    return false;
                };
                let slice = source.get(source_range.clone()).unwrap_or("");
                if slice.starts_with("![") {
                    // Full `![alt](url)` / `![alt][ref]` sourcepos: the image
                    // dest is inside the span. Wrapping `[![…](img)](url)`
                    // dest starts after the image (`end+2`) and must not be
                    // treated as atomic image dest.
                    dest_start >= source_range.start && dest_start < source_range.end
                } else {
                    // Inner `alt` sourcepos: image dest is `](url)` immediately
                    // after. Wrapping dest is further (`](img)](url)`).
                    dest_start >= source_range.end
                        && dest_start <= source_range.end.saturating_add(2)
                }
            })
        }

        fn dest_end_home(
            dest_start: usize,
            dest_end: usize,
            inner_end: usize,
            cursor: usize,
            at: usize,
        ) -> Option<usize> {
            if at < dest_start || at >= dest_end {
                return None;
            }
            if cursor >= dest_start && cursor < dest_end {
                return Some(inner_end);
            }
            Some(dest_start.saturating_sub(1))
        }

        fn walk(blocks: &[Block], source: &str, cursor: usize, at: usize) -> Option<usize> {
            for block in blocks {
                if matches!(block.kind, BlockKind::LinkReferenceDefinition { .. }) {
                    continue;
                }
                for dest in markdown_link_dests(source, block) {
                    if dest_belongs_to_image(source, block, dest.outer.start) {
                        continue;
                    }
                    let inner_end = dest.title.as_ref().map(|t| t.end).unwrap_or(dest.url.end);
                    if let Some(home) =
                        dest_end_home(dest.outer.start, dest.outer.end, inner_end, cursor, at)
                    {
                        return Some(home);
                    }
                }
                let lo = block.source_range.start;
                let hi = block.source_range.end.min(source.len());
                for inline in &block.inlines {
                    let (source_range, link) = match inline {
                        Inline::Run {
                            source_range,
                            link: Some(link),
                            ..
                        }
                        | Inline::Emoji {
                            source_range,
                            link: Some(link),
                            ..
                        } if !link.autolink => (source_range.clone(), Some(link)),
                        _ => continue,
                    };
                    let outer = expand_link_and_html_chrome(source, source_range, link, lo, hi);
                    let Some(chrome) = markdown_link_chrome(source, outer) else {
                        continue;
                    };
                    if chrome.dest.start >= chrome.dest.end
                        || dest_belongs_to_image(source, block, chrome.dest.start)
                    {
                        continue;
                    }
                    if markdown_link_dests(source, block)
                        .iter()
                        .any(|d| d.outer == chrome.dest)
                    {
                        continue;
                    }
                    let inner_end = chrome.dest.end.saturating_sub(1).max(chrome.dest.start);
                    if let Some(home) =
                        dest_end_home(chrome.dest.start, chrome.dest.end, inner_end, cursor, at)
                    {
                        return Some(home);
                    }
                }
                if let Some(found) = walk(&block.children, source, cursor, at) {
                    return Some(found);
                }
            }
            None
        }
        let home = walk(&self.tree.blocks, source, cursor, at).unwrap_or(at);
        let home = self.clamp_raw_prefix(source, home, Bias::Left);
        let home = html_phrasing_suffix_end_home(&self.tree.blocks, home).unwrap_or(home);
        let home = entity_escape_end_home(&self.tree.blocks, home).unwrap_or(home);
        footnote_ref_end_home(&self.tree.blocks, home).unwrap_or(home)
    }

    /// Move `delta` visual lines (`+` down, `-` up). A painted blank gap
    /// between blocks is one line even when the source has extra `\n`s.
    pub fn vertical_caret(&self, source: &str, cursor: usize, delta: i32) -> usize {
        if delta == 0 {
            return cursor;
        }
        let down = delta > 0;
        let mut pos = cursor.min(source.len());
        let mut left = delta.unsigned_abs() as usize;
        let guard = source.len().saturating_add(4);
        for _ in 0..guard {
            if left == 0 {
                break;
            }
            if let Some(gap) = blank_caret_gap_at(&self.tree, pos) {
                let next = if down {
                    self.snap_caret(gap.end.min(source.len()), Bias::Right)
                } else {
                    self.snap_caret(gap.start.saturating_sub(1), Bias::Left)
                };
                if next == pos {
                    break;
                }
                pos = next;
                left -= 1;
                continue;
            }
            let next = adjacent_source_line_offset(source, pos, down);
            if next == pos {
                break;
            }
            if let Some(gap) = blank_caret_gap_at(&self.tree, next) {
                pos = gap.start;
                left -= 1;
                continue;
            }
            let snapped = self.snap_caret(next, Bias::Left);
            // Fence ticks (and similar chrome) are not painted lines. Snap
            // would land back on the body; step to the next/prev caret home
            // instead (trailing blank, previous block).
            let moved = if snapped == pos {
                if down {
                    self.next_caret(source, pos)
                } else {
                    self.prev_caret(source, pos)
                }
            } else {
                snapped
            };
            if moved == pos {
                break;
            }
            // Quoted/nested fences, HTML, quotes, and lists: skip `>` / `- `
            // so Up/Down land on the painted body (same prefixes click maps past).
            let next_pos = self.clamp_raw_prefix(source, moved, Bias::Right);
            if next_pos == pos {
                break;
            }
            pos = next_pos;
            left -= 1;
        }
        pos
    }

    /// Leaf block immediately before `id` in document order.
    fn block_before(&self, id: NodeId) -> Option<&Block> {
        let mut prev: Option<&Block> = None;
        let mut found: Option<&Block> = None;
        fn walk<'t>(
            blocks: &'t [Block],
            id: NodeId,
            prev: &mut Option<&'t Block>,
            found: &mut Option<&'t Block>,
        ) {
            for b in blocks {
                if found.is_some() {
                    return;
                }
                if b.id == id {
                    *found = Some(b);
                    return;
                }
                if b.children.is_empty() {
                    *prev = Some(b);
                }
                walk(&b.children, id, prev, found);
            }
        }
        walk(&self.tree.blocks, id, &mut prev, &mut found);
        found.and(prev)
    }

    /// Leaf block immediately after `id` in document order.
    fn block_after(&self, id: NodeId) -> Option<&Block> {
        let mut take_next = false;
        fn walk<'t>(blocks: &'t [Block], id: NodeId, take_next: &mut bool) -> Option<&'t Block> {
            for b in blocks {
                if *take_next && b.children.is_empty() {
                    return Some(b);
                }
                if b.id == id {
                    *take_next = true;
                } else if let Some(found) = walk(&b.children, id, take_next) {
                    return Some(found);
                }
            }
            None
        }
        walk(&self.tree.blocks, id, &mut take_next)
    }

    /// Block ↔ source-line map over top-level blocks.
    pub fn line_map(&self, source: &str) -> Vec<BlockSpan> {
        let mut line_starts = vec![0usize];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        let line_of = |byte: usize| match line_starts.binary_search(&byte) {
            Ok(l) => l,
            Err(l) => l.saturating_sub(1),
        };
        self.tree
            .blocks
            .iter()
            .map(|b| BlockSpan {
                id: b.id,
                source_range: b.source_range.clone(),
                source_lines: line_of(b.source_range.start)
                    ..line_of(b.source_range.end.saturating_sub(1)) + 1,
            })
            .collect()
    }

    /// Headings as (source offset, level, text) — replaces the span-based
    /// outline for WYSIWYG mode.
    pub fn outline(&self) -> Vec<(usize, u8, String)> {
        self.tree.outline()
    }

    /// Expand a WYSIWYG visual selection so Copy emits source markdown
    /// (Typora), not only the painted inner text. A collapsed caret expands
    /// to the current block (`# ` / `- ` / `>` / fence ticks / paragraph /
    /// `![…](url)`). Table `|` is never included (collapsed caret copies the
    /// cell text). Fenced/HTML bodies keep their ticks/tags.
    pub fn expand_markdown_selection(&self, source: &str, range: Range<usize>) -> Range<usize> {
        expand_markdown_selection(&self.tree, source, range, false)
    }

    /// Like [`Self::expand_markdown_selection`], but a collapsed caret in a
    /// table stays empty (Cut must not delete the cell/row) and a collapsed
    /// block cut includes one trailing `\n` so the line is removed.
    pub fn expand_markdown_cut_selection(&self, source: &str, range: Range<usize>) -> Range<usize> {
        expand_markdown_selection(&self.tree, source, range, true)
    }

    /// Source markdown for a WYSIWYG selection (after
    /// [`Self::expand_markdown_selection`]). A collapsed caret yields the
    /// current block; empty when there is no block (blank gap).
    pub fn markdown_for_selection(&self, source: &str, range: Range<usize>) -> String {
        let range = self.expand_markdown_selection(source, range);
        source.get(range).unwrap_or("").to_string()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WordCharKind {
    Word,
    Punct,
    Whitespace,
}

fn word_char_kind(c: char) -> WordCharKind {
    if c.is_whitespace() {
        WordCharKind::Whitespace
    } else if c.is_alphanumeric() || c == '_' {
        WordCharKind::Word
    } else {
        WordCharKind::Punct
    }
}

fn word_kind_at(source: &str, at: usize) -> WordCharKind {
    source
        .get(at..)
        .and_then(|s| s.chars().next())
        .map(word_char_kind)
        .unwrap_or(WordCharKind::Whitespace)
}

/// Byte where body editing may start. YAML is the frontmatter panel, not a
/// body block. Comrak's FrontMatter sourcepos is often empty (`0..0`); fall
/// back to the captured `raw` length when the fences sit at document start.
pub(crate) fn frontmatter_body_start(tree: &RichTree) -> usize {
    let Some(fm) = &tree.frontmatter else {
        return 0;
    };
    let from_range = fm.source_range.end;
    let from_raw = fm.source_range.start.saturating_add(fm.raw.len());
    from_range.max(from_raw).min(tree.source_len)
}

/// Comrak has no empty-paragraph node. Leading newlines before the first
/// top-level block, a standard `\n\n` separator between blocks, extra blanks
/// beyond that, a trailing blank after the last block, and a document that
/// is only newlines (no blocks), are still valid Typora caret homes (click
/// the gap, Enter at start of a heading, leftover click below the last
/// painted line). One source separator is one painted line. A lone
/// terminator `\n` after the last block is not an empty paragraph.
pub fn blank_caret_gaps(tree: &RichTree) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let fm_end = frontmatter_body_start(tree);
    for (i, block) in tree.blocks.iter().enumerate() {
        let lo = if i == 0 {
            fm_end
        } else {
            tree.blocks[i - 1].source_range.end
        };
        let hi = block.source_range.start;
        if let Some(gap) = blank_between(lo, hi, i == 0 && fm_end == 0) {
            out.push(gap);
        }
    }
    if let Some(gap) = blank_caret_gap_after_last(tree) {
        out.push(gap);
    }
    out
}

/// Empty-paragraph slot painted/clicked immediately before top-level `index`.
pub fn blank_caret_gap_before(tree: &RichTree, index: usize) -> Option<Range<usize>> {
    let hi = tree.blocks.get(index)?.source_range.start;
    blank_caret_gaps(tree).into_iter().find(|gap| gap.end == hi)
}

/// Empty-paragraph slot after the last top-level block when the file ends
/// with a blank line (`hello\n\n`). A single trailing `\n` is the last
/// block's terminator, not a painted line. Detected from source bytes so
/// lists/fences that include their terminator still get a gap.
///
/// A document with no blocks (empty, or only newlines) still hosts a caret
/// so the user can type. Frontmatter-only files sit after the YAML.
pub fn blank_caret_gap_after_last(tree: &RichTree) -> Option<Range<usize>> {
    if tree.blocks.is_empty() {
        let start = frontmatter_body_start(tree).min(tree.source_len);
        if let Some(gap) = &tree.trailing_blank {
            if gap.start >= start {
                return Some(gap.clone());
            }
        }
        return Some(start..tree.source_len);
    }
    tree.trailing_blank.clone()
}

/// Caret home for leftover viewport below the last painted block (and any
/// trailing blank-gap leaf). Trailing blank if one exists, else document
/// end — never a hit-test onto the last paragraph (that would insert
/// mid-line).
///
/// A leftover **mouse click** (not this query) must call
/// `place_caret_for_click_below` first so a document with no trailing blank
/// (`hello`) opens an empty paragraph. This query is the home after that,
/// and the IME/drag mapping (which must not mutate).
pub fn caret_for_click_below_content(tree: &RichTree) -> usize {
    blank_caret_gap_after_last(tree)
        .map(|gap| gap.start)
        .unwrap_or(tree.source_len)
}

/// Byte lives on a Comrak-less blank (leading, between top-level blocks, or
/// a trailing blank after the last block — including EOF on that line).
pub fn blank_caret_gap_at(tree: &RichTree, byte: usize) -> Option<Range<usize>> {
    blank_caret_gaps(tree).into_iter().find(|gap| {
        (byte >= gap.start && byte < gap.end)
            || (gap.end == tree.source_len && byte == tree.source_len)
    })
}

fn blank_caret_gap_ending_at(tree: &RichTree, end: usize) -> Option<Range<usize>> {
    blank_caret_gaps(tree)
        .into_iter()
        .find(|gap| gap.end == end)
}

/// Bytes between `lo` (previous block end or frontmatter) and `hi` (next
/// block start or source end) that paint as one empty line.
fn blank_between(lo: usize, hi: usize, leading: bool) -> Option<Range<usize>> {
    if hi <= lo {
        return None;
    }
    // Non-leading: skip the previous block's inclusive end (its line
    // terminator). The empty line starts at the next byte.
    // A lone `\n` (ATX interrupting a paragraph, or a block terminator
    // at EOF) is not a blank line.
    let start = if leading { lo } else { (lo + 1).min(hi) };
    if start >= hi {
        return None;
    }
    Some(start..hi)
}

fn raw_leaf_at(engine: &RichEngine, offset: usize) -> Option<&Block> {
    let id = engine.block_at(offset)?;
    let block = engine.block(id)?;
    matches!(
        block.kind,
        BlockKind::CodeBlock { .. } | BlockKind::Opaque { .. }
    )
    .then_some(block)
}

/// True when byte `i` is a GFM table delimiter `|`, not a cell-literal `\|`.
pub(crate) fn is_unescaped_pipe(source: &str, i: usize) -> bool {
    source.as_bytes().get(i) == Some(&b'|') && !odd_backslash_escape(source.as_bytes(), i)
}

/// `\|` pair containing `i`, when `i` sits on the backslash or the escaped `|`.
pub(crate) fn escaped_pipe_pair_at(source: &str, i: usize) -> Option<Range<usize>> {
    let bytes = source.as_bytes();
    if bytes.get(i) == Some(&b'|') && odd_backslash_escape(bytes, i) {
        return Some(i - 1..i + 1);
    }
    if bytes.get(i) == Some(&b'\\')
        && bytes.get(i + 1) == Some(&b'|')
        && odd_backslash_escape(bytes, i + 1)
    {
        return Some(i..i + 2);
    }
    None
}

/// Insert/wrap must not land on the `|` of `\|` (that un-escapes the pipe and
/// splits the GFM row). Sit on the backslash, i.e. before the painted pipe.
pub(crate) fn insert_offset_escaping_table_pipe(
    engine: &RichEngine,
    source: &str,
    offset: usize,
) -> usize {
    let in_table = engine.in_table(offset) || (offset > 0 && engine.in_table(offset - 1));
    if !in_table {
        return offset;
    }
    match escaped_pipe_pair_at(source, offset) {
        Some(pair) if offset > pair.start && offset < pair.end => pair.start,
        _ => offset,
    }
}

/// If `range` covers only one half of a cell `\|`, grow onto the whole pair
/// so Backspace/Delete cannot leave a bare `|` column delimiter.
pub(crate) fn expand_range_over_escaped_table_pipe(
    engine: &RichEngine,
    source: &str,
    range: Range<usize>,
) -> Range<usize> {
    if range.start >= range.end {
        return range;
    }
    let probe_end = range.end.saturating_sub(1).max(range.start);
    if !(engine.in_table(range.start)
        || engine.in_table(probe_end)
        || (range.start > 0 && engine.in_table(range.start - 1)))
    {
        return range;
    }
    let mut start = range.start;
    let mut end = range.end;
    let lo = range.start.saturating_sub(1);
    let hi = range.end.saturating_add(1).min(source.len());
    for i in lo..hi {
        let Some(pair) = escaped_pipe_pair_at(source, i) else {
            continue;
        };
        if range.start < pair.end && range.end > pair.start {
            start = start.min(pair.start);
            end = end.max(pair.end);
        }
    }
    start..end
}

fn odd_backslash_escape(bytes: &[u8], i: usize) -> bool {
    let mut n = 0usize;
    let mut j = i;
    while j > 0 && bytes[j - 1] == b'\\' {
        n += 1;
        j -= 1;
    }
    n % 2 == 1
}

/// Drop unescaped `|` separators that comrak sometimes includes at a cell's
/// edges. An escaped `\|` is cell text (a literal pipe) and must stay.
fn trim_cell_pipes(source: &str, range: Range<usize>) -> Range<usize> {
    let mut start = range.start.min(source.len());
    let mut end = range.end.min(source.len());
    while start < end && is_unescaped_pipe(source, start) {
        start += 1;
    }
    while end > start && is_unescaped_pipe(source, end - 1) {
        end -= 1;
    }
    start..end
}

/// Editable inner range: fence body between ticks, indented-code body after
/// the opening indent, Type-1 `<style>` / `<textarea>` inner source after
/// the open tag, else the whole raw block.
pub(crate) fn raw_body_range(block: &Block, source: &str) -> Range<usize> {
    match &block.kind {
        BlockKind::CodeBlock { .. } => block.code_body_range(source),
        BlockKind::Opaque { raw } => {
            if let Some(inner) =
                crate::html_visual::html_type1_inner_range(raw, block.source_range.start)
            {
                let end = inner.end.min(block.source_range.end).min(source.len());
                let start = inner.start.min(end);
                return start..end;
            }
            block.source_range.clone()
        }
        _ => block.source_range.clone(),
    }
}

/// Quote markers, list marker (`- ` / `1. ` / task), footnote-def opener
/// (`[^1]: `), and a definition-details `: ` on that kind of block. Click /
/// IME / arrows skip this prefix; Tab/Enter keep it on fences. Unquoted
/// paragraphs have an empty prefix (1:1).
pub(crate) fn raw_container_prefix(source: &str, block: &Block) -> String {
    let start = block.source_range.start.min(source.len());
    let line_start = source_line_start(source, start);
    let line_end = source_line_end_exclusive(source, start);
    let line = &source[line_start..line_end];
    let quote = quote_marker_on_line(line);
    let after = &line[quote.len()..];
    let marker = list_marker_on_line(after);
    let rest = &after[marker.len()..];
    let fn_mark = footnote_def_marker_on_line(rest);
    let after_fn = &rest[fn_mark.len()..];
    let details = if matches!(block.kind, BlockKind::DefinitionDetails) {
        definition_details_marker_on_line(after_fn)
    } else {
        ""
    };
    let after_details = &after_fn[details.len()..];
    let indent = after_details
        .bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .count();
    format!(
        "{quote}{marker}{fn_mark}{details}{}",
        &after_details[..indent]
    )
}

/// Quote then list/task marker, footnote-def opener, and definition-details
/// `: ` on the source line containing `at`.
///
/// Ranges are empty (`start == end`) when that part is absent. Used by
/// WYSIWYG intersect-reveal to paint `>` / `- ` / `1. ` / `[ ] ` / `[^1]: `
/// / `: ` the same way source-mode masking reveals those delimiters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinePrefixParts {
    pub quote: Range<usize>,
    pub list: Range<usize>,
    pub footnote: Range<usize>,
    pub details: Range<usize>,
}

impl LinePrefixParts {
    pub fn is_empty(&self) -> bool {
        self.quote.start == self.quote.end
            && self.list.start == self.list.end
            && self.footnote.start == self.footnote.end
            && self.details.start == self.details.end
    }
}

pub fn line_prefix_parts(source: &str, at: usize) -> LinePrefixParts {
    let at = at.min(source.len());
    let line_start = source_line_start(source, at);
    let line_end = source_line_end_exclusive(source, at);
    let line = &source[line_start..line_end];
    let quote = quote_marker_on_line(line);
    let list = list_marker_on_line(&line[quote.len()..]);
    let after_list = quote.len() + list.len();
    let fn_mark = footnote_def_marker_on_line(&line[after_list..]);
    let after_fn = after_list + fn_mark.len();
    let details = definition_details_marker_on_line(&line[after_fn..]);
    let quote_end = line_start + quote.len();
    let list_end = quote_end + list.len();
    let fn_end = list_end + fn_mark.len();
    LinePrefixParts {
        quote: line_start..quote_end,
        list: quote_end..list_end,
        footnote: list_end..fn_end,
        details: fn_end..fn_end + details.len(),
    }
}

/// Leading indent plus `>` markers (optional space after each), or empty.
fn quote_marker_on_line(line: &str) -> &str {
    let indent_len = line
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    let bytes = line.as_bytes();
    if bytes.get(indent_len) != Some(&b'>') {
        return "";
    }
    let mut i = indent_len;
    while bytes.get(i) == Some(&b'>') {
        i += 1;
        if bytes.get(i) == Some(&b' ') || bytes.get(i) == Some(&b'\t') {
            i += 1;
        }
    }
    &line[..i]
}

/// Leading indent plus `- ` / `* ` / `+ ` / `1. ` / task checkbox, or empty.
/// Same width `command.rs` uses when stripping a list marker.
///
/// CommonMark padding after the marker is one tab or 1–4 spaces (five or
/// more spaces is only one space of padding; the rest is indented-code
/// indent). Ordered markers are 1–9 digits and need that same whitespace
/// (`1.item` is a paragraph).
///
/// A GFM task checkbox is `[ ]` / `[x]` / `[X]` followed by a space or tab.
/// `[x](url)` is a link whose label happens to be `x`. `[x]` at end of line
/// is a shortcut reference (or incomplete `[x]`) — not a task. Swallowing
/// `[x]` with no following space would skip Home/click past the painted
/// label.
pub fn list_marker_on_line(line: &str) -> &str {
    let indent_len = line
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    let rest = &line[indent_len..];
    let marker_len = {
        let bullet = bullet_list_marker_len(rest);
        if bullet > 0 {
            bullet
        } else {
            ordered_list_marker_len(rest)
        }
    };
    if marker_len == 0 {
        return "";
    }
    let mut take = indent_len + marker_len;
    take += gfm_task_checkbox_len(line.get(take..).unwrap_or(""));
    &line[..take.min(line.len())]
}

/// One tab, or 1–4 spaces. Five or more spaces: only the first is padding.
fn list_marker_padding_len(after_marker: &str) -> usize {
    let bytes = after_marker.as_bytes();
    match bytes.first() {
        Some(b'\t') => 1,
        Some(b' ') => {
            let n = bytes.iter().take_while(|&&b| b == b' ').count();
            if n <= 4 {
                n
            } else {
                1
            }
        }
        _ => 0,
    }
}

fn bullet_list_marker_len(rest: &str) -> usize {
    let Some(&first) = rest.as_bytes().first() else {
        return 0;
    };
    if !matches!(first, b'-' | b'*' | b'+') {
        return 0;
    }
    let after = &rest[1..];
    if after.is_empty() {
        return 1;
    }
    let pad = list_marker_padding_len(after);
    if pad == 0 {
        0
    } else {
        1 + pad
    }
}

fn ordered_list_marker_len(rest: &str) -> usize {
    let Some(end) = rest.find(['.', ')']) else {
        return 0;
    };
    if !(1..=9).contains(&end) || !rest[..end].bytes().all(|b| b.is_ascii_digit()) {
        return 0;
    }
    let after = &rest[end + 1..];
    if after.is_empty() {
        return end + 1;
    }
    let pad = list_marker_padding_len(after);
    if pad == 0 {
        0
    } else {
        end + 1 + pad
    }
}

/// Bytes of a GFM task checkbox after a list marker, including the trailing
/// space/tab. Zero when the slot is a link (`[x](url)`) or `[x]` at EOL
/// (shortcut-ref / incomplete — GFM requires a space after `]`).
fn gfm_task_checkbox_len(after_marker: &str) -> usize {
    if !(after_marker.starts_with("[ ]")
        || after_marker.starts_with("[x]")
        || after_marker.starts_with("[X]"))
    {
        return 0;
    }
    match after_marker.as_bytes().get(3) {
        Some(b' ' | b'\t') => 4,
        _ => 0,
    }
}

/// Quote markers plus list marker (`- ` / `1. ` / task) plus a footnote
/// definition opener (`[^1]: `) plus a definition-details `: `, without extra
/// body indent. Empty `> ` / `- ` / `[^1]: ` / `: ` lines use this as the
/// caret skip width. Import uses this as the floor when recovering 0–3
/// spaces before ATX / fence / thematic so quoted `>  # Title` keeps `>`.
/// Empty `[ref]: ` lines skip `[label]: ` the same way as `[^1]: `.
pub(crate) fn quote_list_prefix_on_line(line: &str) -> &str {
    let quote = quote_marker_on_line(line);
    let after_quote = &line[quote.len()..];
    let marker = list_marker_on_line(after_quote);
    let fn_mark = footnote_def_marker_on_line(&after_quote[marker.len()..]);
    let after_fn = &after_quote[marker.len() + fn_mark.len()..];
    let link_ref = if fn_mark.is_empty() {
        link_ref_def_marker_on_line(after_fn)
    } else {
        ""
    };
    let details = definition_details_marker_on_line(&after_fn[link_ref.len()..]);
    &line[..quote.len() + marker.len() + fn_mark.len() + link_ref.len() + details.len()]
}

/// Leading indent plus `:` and an optional following space/tab (PHP-Extra /
/// Typora details opener). Empty when the line does not start with `:`.
pub(crate) fn definition_details_marker_on_line(line: &str) -> &str {
    let indent_len = line
        .bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .count();
    let rest = &line[indent_len..];
    if !rest.starts_with(':') {
        return "";
    }
    let mut take = indent_len + 1;
    if matches!(line.as_bytes().get(take), Some(b' ' | b'\t')) {
        take += 1;
    }
    &line[..take.min(line.len())]
}

/// Leading indent plus `[^label]:` and an optional following space/tab.
/// Empty on a footnote *ref* (`Hello[^1]`) or a shortcut label (`[x]: url`).
pub(crate) fn footnote_def_marker_on_line(line: &str) -> &str {
    let indent_len = line
        .bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .count();
    let rest = &line[indent_len..];
    let Some(after) = rest.strip_prefix("[^") else {
        return "";
    };
    let Some(close) = after.find("]:") else {
        return "";
    };
    let label = &after[..close];
    if label.is_empty() || label.contains(['[', ']', '\n']) {
        return "";
    }
    let mut take = indent_len + 2 + label.len() + 2; // `[^` + label + `]:`
    if matches!(line.as_bytes().get(take), Some(b' ' | b'\t')) {
        take += 1;
    }
    &line[..take.min(line.len())]
}

/// Leading indent plus `[label]:` and an optional following space/tab.
/// Empty on a footnote def (`[^1]:`), a shortcut/collapsed ref (`[foo]` /
/// `[foo][]`), or a paragraph that is not a definition line.
pub(crate) fn link_ref_def_marker_on_line(line: &str) -> &str {
    let indent_len = line
        .bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .count();
    let rest = &line[indent_len..];
    if rest.starts_with("[^") {
        return "";
    }
    let Some(after) = rest.strip_prefix('[') else {
        return "";
    };
    let Some(close) = after.find("]:") else {
        return "";
    };
    let label = &after[..close];
    if label.is_empty() || label.contains(['[', ']', '\n']) {
        return "";
    }
    let mut take = indent_len + 1 + label.len() + 2; // `[` + label + `]:`
    if matches!(line.as_bytes().get(take), Some(b' ' | b'\t')) {
        take += 1;
    }
    &line[..take.min(line.len())]
}

/// Bytes to skip at the start of `line` so click/arrows land on painted
/// body. Opening-line prefix (`- `, `> `) on matching lines, GFM
/// continuation indent (`  world` after `- hello`), plus CommonMark
/// fence_offset spaces on content lines (indented fence contents).
/// `fence_offset` is extra spaces after the quote/list prefix;
/// already-consumed prefix indent is not stripped twice.
pub(crate) fn skip_line_prefix_and_fence(line: &str, prefix: &str, fence_offset: usize) -> usize {
    let mut skip = if prefix.is_empty() {
        0
    } else if line.starts_with(prefix) {
        prefix.len()
    } else {
        let quote = quote_marker_on_line(line);
        let rest = &line[quote.len()..];
        let budget = prefix.len().saturating_sub(quote.len());
        let indent = rest
            .bytes()
            .take_while(|&b| b == b' ' || b == b'\t')
            .count()
            .min(budget);
        quote.len() + indent
    };
    if fence_offset > 0 {
        let already = prefix.bytes().rev().take_while(|&b| b == b' ').count();
        let extra = fence_offset.saturating_sub(already);
        if extra > 0 {
            let rest = line.get(skip..).unwrap_or("");
            skip += rest.bytes().take_while(|&b| b == b' ').take(extra).count();
        }
    }
    skip
}

/// 0–3 spaces after quote/list prefix at `start` (CommonMark opening
/// indent before ATX `#`, a fence, or a thematic break).
fn skip_cm_opening_spaces(source: &str, start: usize, end: usize) -> usize {
    let start = start.min(end).min(source.len());
    let end = end.min(source.len()).max(start);
    let bytes = source.as_bytes();
    let mut i = start;
    let mut n = 0;
    while i < end && bytes[i] == b' ' && n < 3 {
        i += 1;
        n += 1;
    }
    i
}

/// Empty `> ` / `- ` / `1. ` / task / `[^1]: ` / `: ` lines that are not fenced/HTML body.
pub(crate) fn collect_empty_prefix_homes(source: &str, tree: &RichTree) -> Vec<PrefixBlank> {
    let mut out = Vec::new();
    let mut ls = 0usize;
    while ls <= source.len() {
        let le = source[ls..]
            .find('\n')
            .map(|i| ls + i)
            .unwrap_or(source.len());
        let line = &source[ls..le];
        let prefix = quote_list_prefix_on_line(line);
        if !prefix.is_empty() && line[prefix.len()..].trim().is_empty() {
            let home = (ls + prefix.len()).min(le).min(source.len());
            if prefix_blank_is_quote_or_list(tree, home)
                && details_prefix_home_is_in_definition_list(tree, line, home)
            {
                out.push(PrefixBlank { line: ls..le, home });
            }
        }
        if le == source.len() {
            break;
        }
        ls = le + 1;
        if ls > source.len() {
            break;
        }
    }
    out
}

/// A line that is only `: ` (after quote/list/footnote chrome) is a caret home
/// inside a definition list, not a paragraph that happens to start with `:`.
fn details_prefix_home_is_in_definition_list(tree: &RichTree, line: &str, home: usize) -> bool {
    let quote = quote_marker_on_line(line);
    let after_quote = &line[quote.len()..];
    let marker = list_marker_on_line(after_quote);
    let fn_mark = footnote_def_marker_on_line(&after_quote[marker.len()..]);
    let after_fn = &after_quote[marker.len() + fn_mark.len()..];
    if definition_details_marker_on_line(after_fn).is_empty() {
        return true;
    }
    deepest_block(&tree.blocks, home).is_some_and(|block| {
        matches!(
            block.kind,
            BlockKind::DefinitionDetails
                | BlockKind::DefinitionItem { .. }
                | BlockKind::DefinitionList
                | BlockKind::DefinitionTerm
        )
    })
}

fn prefix_blank_is_quote_or_list(tree: &RichTree, byte: usize) -> bool {
    let Some(block) = deepest_block(&tree.blocks, byte) else {
        return false;
    };
    !matches!(
        block.kind,
        BlockKind::CodeBlock { .. }
            | BlockKind::Opaque { .. }
            | BlockKind::Table { .. }
            | BlockKind::TableRow { .. }
            | BlockKind::TableCell
            | BlockKind::Heading { .. }
            | BlockKind::ThematicBreak
    )
}

fn deepest_block(blocks: &[Block], byte: usize) -> Option<&Block> {
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
    best
}

/// If `offset` sits inside an HTML tag (`<div>`, `</div>`, `<!-- -->`), skip
/// it like quote prefix. Leaves `a < b` text alone (not a tag).
pub(crate) fn html_tag_bounds(
    source: &str,
    offset: usize,
    body: Range<usize>,
) -> Option<Range<usize>> {
    let offset = offset.clamp(body.start, body.end);
    if body.start >= body.end || offset >= body.end {
        return None;
    }
    let slice = &source[body.start..body.end];
    let rel = offset - body.start;
    let before = &slice[..=rel.min(slice.len().saturating_sub(1))];
    let open_rel = before.rfind('<')?;
    let after_open = &slice[open_rel..];
    let tag_rest = after_open.get(1..)?;
    let first = tag_rest.chars().next()?;
    if first != '/' && first != '!' && !first.is_ascii_alphabetic() {
        return None;
    }
    let close = after_open.find('>')?;
    let start = body.start + open_rel;
    let end = (body.start + open_rel + close + 1).min(body.end);
    (offset >= start && offset < end).then_some(start..end)
}

fn clamp_html_tag(source: &str, offset: usize, body: Range<usize>, bias: Bias) -> usize {
    let Some(tag) = html_tag_bounds(source, offset, body.clone()) else {
        return offset;
    };
    match bias {
        Bias::Right => tag.end.clamp(body.start, body.end),
        Bias::Left => {
            if tag.start > body.start {
                tag.start.saturating_sub(1).clamp(body.start, body.end)
            } else {
                tag.end.clamp(body.start, body.end)
            }
        }
    }
}

fn source_line_start(source: &str, offset: usize) -> usize {
    let offset = offset.min(source.len());
    source[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

fn source_line_end_exclusive(source: &str, offset: usize) -> usize {
    let offset = offset.min(source.len());
    source[offset..]
        .find('\n')
        .map(|i| offset + i)
        .unwrap_or(source.len())
}

/// Source offsets for each UTF-8 index in a painted fence/HTML/indented-code
/// body (`painted_len + 1` slots). Quote/list prefixes are skipped so a click
/// on painted content does not land on `>` or list indent.
pub fn code_body_source_map(source: &str, block: &Block, painted_len: usize) -> Vec<usize> {
    let body = raw_body_range(block, source);
    let fence_offset = match &block.kind {
        BlockKind::CodeBlock { fence: Some(f), .. } => f.fence_offset,
        _ => 0,
    };
    map_visible_skipping_prefix(
        source,
        body,
        &raw_container_prefix(source, block),
        painted_len,
        fence_offset,
    )
}

/// Map painted HTML-block **literal** bytes (tags included) onto source,
/// skipping quote/list prefixes. Unlike [`code_body_source_map`], Type-1
/// `<style>` / `<textarea>` keep their tags so intersect-reveal click maps
/// onto `<style>`, not inner CSS.
pub fn html_block_literal_source_map(
    source: &str,
    block: &Block,
    painted_len: usize,
) -> Vec<usize> {
    map_visible_skipping_prefix(
        source,
        block.source_range.clone(),
        &raw_container_prefix(source, block),
        painted_len,
        0,
    )
}

/// Map painted body bytes onto `source[body]`, skipping `prefix` at the start
/// of each line (newlines stay in the painted stream).
fn map_visible_skipping_prefix(
    source: &str,
    body: Range<usize>,
    prefix: &str,
    painted_len: usize,
    fence_offset: usize,
) -> Vec<usize> {
    let start = body.start.min(source.len());
    let end = body.end.min(source.len()).max(start);
    if painted_len == 0 || start == end {
        return vec![start, start];
    }
    let slice = &source[start..end];
    let mut content = Vec::new();
    let mut i = 0usize;
    while i < slice.len() {
        let rest = &slice[i..];
        let nl = rest.find('\n').unwrap_or(rest.len());
        let line = &rest[..nl];
        let skip = skip_line_prefix_and_fence(line, prefix, fence_offset).min(nl);
        for j in skip..nl {
            content.push(start + i + j);
        }
        if nl < rest.len() {
            content.push(start + i + nl);
            i += nl + 1;
        } else {
            break;
        }
    }
    let mut source_at = Vec::with_capacity(painted_len + 1);
    for k in 0..painted_len {
        source_at.push(
            content
                .get(k)
                .copied()
                .or_else(|| content.last().copied())
                .unwrap_or(start),
        );
    }
    let last = content
        .get(painted_len)
        .copied()
        .or_else(|| {
            content.last().map(|p| {
                let next = p.saturating_add(1);
                if next <= end {
                    next
                } else {
                    *p
                }
            })
        })
        .unwrap_or(start);
    source_at.push(last);
    source_at
}

/// Byte ranges of caret-valid inline content within a leaf block. Quote and
/// list containers with no inlines of their own use their descendants so
/// `>` / `- ` are not caret homes (Home / snap land on painted body).
fn inline_ranges(block: &Block, source: &str) -> Vec<Range<usize>> {
    match &block.kind {
        BlockKind::CodeBlock { .. } => {
            if let Some(Inline::Run { source_range, .. }) = block.inlines.first() {
                vec![source_range.clone()]
            } else {
                vec![block.source_range.clone()]
            }
        }
        BlockKind::ThematicBreak => {
            let r = &block.source_range;
            let start = skip_cm_opening_spaces(source, r.start, r.end);
            // One caret range (indent skipped); not `start..end` collected as usizes.
            #[allow(clippy::single_range_in_vec_init)]
            {
                vec![start..r.end]
            }
        }
        BlockKind::Opaque { .. } => vec![block.source_range.clone()],
        BlockKind::Alert {
            tag_range,
            chrome_range,
            ..
        } => {
            // `[!NOTE]` is dest chrome (like quote `>`). A custom title after
            // the tag is a caret home; otherwise snap onto the body.
            let mut out = Vec::new();
            if let Some(title) = alert_title_range(tag_range, chrome_range) {
                out.push(title);
            }
            for child in &block.children {
                out.extend(inline_ranges(child, source));
            }
            if out.is_empty() {
                out.push(tag_range.end..tag_range.end);
            }
            out
        }
        BlockKind::Toc { wiki } => {
            let w = if *wiki { 2 } else { 1 };
            let start = block.source_range.start.saturating_add(w);
            let end = block.source_range.end.saturating_sub(w);
            if start < end {
                // One inner caret range; not `start..end` collected as usizes.
                #[allow(clippy::single_range_in_vec_init)]
                {
                    vec![start..end]
                }
            } else {
                leaf_inline_ranges(block, source)
            }
        }
        BlockKind::Table { .. } | BlockKind::TableRow { .. } => {
            // Pipes and the alignment row are dest chrome, not caret homes.
            // Descend into cells so Home/snap land on painted body (`a`), not `|`.
            let mut out = Vec::new();
            for child in &block.children {
                out.extend(inline_ranges(child, source));
            }
            if out.is_empty() {
                out.push(block.source_range.clone());
            }
            out
        }
        _ if block.inlines.is_empty() && !block.children.is_empty() => {
            let mut out = Vec::new();
            for child in &block.children {
                out.extend(inline_ranges(child, source));
            }
            if out.is_empty() {
                out.push(block.source_range.clone());
            }
            out
        }
        _ => leaf_inline_ranges(block, source),
    }
}

/// Markdown `![alt](url)` and safe HTML `<img>` are one caret/selection step
/// (pixels in WYSIWYG, not `!` / `[` / `)`).
pub(crate) fn atomic_image_range(inline: &Inline) -> Option<Range<usize>> {
    match inline {
        Inline::Image { source_range, .. } => Some(source_range.clone()),
        Inline::OpaqueInline {
            raw, source_range, ..
        } if crate::html_visual::html_inline_image(raw).is_some() => Some(source_range.clone()),
        _ => None,
    }
}

pub(crate) fn atomic_html_break_range(inline: &Inline) -> Option<Range<usize>> {
    match inline {
        Inline::OpaqueInline {
            raw, source_range, ..
        } if crate::html_visual::html_inline_break(raw) => Some(source_range.clone()),
        _ => None,
    }
}

pub(crate) fn atomic_hard_break_range(inline: &Inline) -> Option<Range<usize>> {
    match inline {
        Inline::HardBreak { source_range, .. } => Some(source_range.clone()),
        _ => None,
    }
}

pub(crate) fn footnote_ref_ranges(inline: &Inline) -> Option<(Range<usize>, Range<usize>)> {
    match inline {
        Inline::OpaqueInline {
            raw, source_range, ..
        } => {
            let inner = crate::html_visual::footnote_ref_inner_range(raw, source_range.clone())?;
            Some((inner, source_range.clone()))
        }
        _ => None,
    }
}

pub(crate) fn atomic_character_reference_range(inline: &Inline) -> Option<Range<usize>> {
    match inline {
        Inline::Run {
            text,
            raw,
            source_range,
            marks,
            ..
        } if !marks.contains(MarkSet::CODE) => {
            let slice = raw.as_deref()?;
            (is_decoded_character_reference(slice, text)
                || is_decoded_backslash_escape(slice, text))
            .then(|| source_range.clone())
        }
        _ => None,
    }
}

/// Caret range that is one Left/Right step (footnote inner label, decoded
/// `&amp;` / `\*` pair, else the whole widget).
fn atomic_caret_range(inline: &Inline) -> Option<Range<usize>> {
    atomic_image_range(inline)
        .or_else(|| atomic_html_break_range(inline))
        .or_else(|| atomic_hard_break_range(inline))
        .or_else(|| atomic_character_reference_range(inline))
        .or_else(|| footnote_ref_ranges(inline).map(|(inner, _)| inner))
}

/// Source span Backspace/Delete must remove as one unit (footnote outer
/// `[^label]`, character-reference `&amp;`, else the caret widget).
pub(crate) fn atomic_delete_range(inline: &Inline) -> Option<Range<usize>> {
    atomic_image_range(inline)
        .or_else(|| atomic_html_break_range(inline))
        .or_else(|| atomic_hard_break_range(inline))
        .or_else(|| atomic_character_reference_range(inline))
        .or_else(|| footnote_ref_ranges(inline).map(|(_, outer)| outer))
}

/// HTML-block `<img>` / safe `<svg>` (a line or block that paints as one
/// image, including in a list/quote) is the same atomic unit as an inline
/// image. Comrak imports those as [`BlockKind::Opaque`], not [`Inline::Image`].
pub(crate) fn html_block_image_range(block: &Block) -> Option<Range<usize>> {
    match &block.kind {
        BlockKind::Opaque { raw } => {
            let trimmed = raw.trim();
            if crate::html_visual::html_inline_image(trimmed).is_some() {
                return Some(block.source_range.clone());
            }
            if matches!(
                crate::html_visual::project_html_block(trimmed),
                crate::html_visual::HtmlBlockVisual::Image { .. }
            ) {
                return Some(block.source_range.clone());
            }
            None
        }
        _ => None,
    }
}

/// HTML-block `<br>` / `<wbr>` (a line that is only the tag, including in a
/// list/quote) is one caret/Delete step, not tag bytes.
pub(crate) fn html_block_break_range(block: &Block) -> Option<Range<usize>> {
    match &block.kind {
        BlockKind::Opaque { raw } if crate::html_visual::html_inline_break(raw.trim()) => {
            Some(block.source_range.clone())
        }
        _ => None,
    }
}

/// Markdown `---` / `***` / `___` and HTML-block `<hr>` paint as a rule
/// (one caret/Delete step, not dash or tag bytes).
pub(crate) fn thematic_break_range(block: &Block) -> Option<Range<usize>> {
    match &block.kind {
        BlockKind::ThematicBreak => Some(block.source_range.clone()),
        BlockKind::Opaque { raw }
            if matches!(
                crate::html_visual::project_html_block(raw.trim()),
                crate::html_visual::HtmlBlockVisual::ThematicBreak
            ) =>
        {
            Some(block.source_range.clone())
        }
        _ => None,
    }
}

/// HTML-block `<!-- … -->` / `<?…?>` / `<![CDATA[…]]>` (standalone, list, or
/// quote) is one caret/Delete step. Source paints when the caret intersects.
pub(crate) fn html_block_comment_range(block: &Block) -> Option<Range<usize>> {
    match &block.kind {
        BlockKind::Opaque { raw } if crate::html_visual::html_block_is_markup_chrome(raw) => {
            Some(block.source_range.clone())
        }
        _ => None,
    }
}

/// Type-1 `<script>`, GFM tagfilter widgets (`<iframe>` / `<title>` /
/// `<xmp>` / …), Type-6 `<details>` / `<dialog>` / `<form>` / `<option>` /
/// `<fieldset>` / `<legend>`, and Type-7 `<video>` / `<audio>` / `<canvas>` /
/// `<math>` / `<object>` / `<button>` / `<select>` / `<input>` / `<label>` /
/// `<output>` / `<progress>` / `<meter>` are one caret/Delete step (do not
/// walk inner HTML / raw text / form controls).
pub(crate) fn html_block_script_range(block: &Block) -> Option<Range<usize>> {
    match &block.kind {
        BlockKind::Opaque { raw } if crate::html_visual::html_block_is_tagfilter_widget(raw) => {
            Some(block.source_range.clone())
        }
        _ => None,
    }
}

/// Opaque HTML that paints as a unit (`<img>`, `<svg>`, `<hr>`, `<br>`,
/// comments, Type-1 `<script>`, tagfilter widgets, Type-6 `<details>` /
/// `<dialog>` / `<form>` / `<option>` / `<fieldset>` / `<legend>`, Type-7
/// `<video>` / `<object>` / `<button>` / `<select>` / `<output>`), not a
/// Flow body.
pub(crate) fn html_block_atomic_range(block: &Block) -> Option<Range<usize>> {
    html_block_image_range(block)
        .or_else(|| html_block_break_range(block))
        .or_else(|| html_block_comment_range(block))
        .or_else(|| html_block_script_range(block))
        .or_else(|| match &block.kind {
            BlockKind::Opaque { .. } => thematic_break_range(block),
            _ => None,
        })
}

/// Paragraph/list/quote inlines that form one dest-chrome HTML widget
/// (`<xmp>raw</xmp>` / `<video>…</video>` / `<button>click</button>` are
/// often Type-7, so same-line content is not an HTML block).
pub fn tagfilter_widget_ranges_in(inlines: &[Inline]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < inlines.len() {
        let Inline::OpaqueInline {
            raw, source_range, ..
        } = &inlines[i]
        else {
            i += 1;
            continue;
        };
        if crate::html_visual::html_inline_tagfilter_open(raw).is_none()
            && crate::html_visual::html_block_is_tagfilter_widget(raw)
        {
            out.push(source_range.clone());
            i += 1;
            continue;
        }
        let Some(name) = crate::html_visual::html_inline_tagfilter_open(raw) else {
            i += 1;
            continue;
        };
        let start = source_range.start;
        let mut end = source_range.end;
        let mut j = i + 1;
        if name == "plaintext" {
            while j < inlines.len() {
                let r = inline_source_span(&inlines[j]);
                end = end.max(r.end);
                j += 1;
            }
            out.push(start..end);
            break;
        }
        let mut depth = 1i32;
        while j < inlines.len() && depth > 0 {
            match &inlines[j] {
                Inline::OpaqueInline {
                    raw,
                    source_range: r,
                    ..
                } => {
                    end = end.max(r.end);
                    if crate::html_visual::html_inline_tagfilter_open(raw).as_deref()
                        == Some(name.as_str())
                    {
                        depth += 1;
                    } else if crate::html_visual::html_inline_tagfilter_close(raw, &name) {
                        depth -= 1;
                    }
                }
                other => {
                    let r = inline_source_span(other);
                    end = end.max(r.end);
                }
            }
            j += 1;
        }
        out.push(start..end);
        i = j;
    }
    out
}

pub(crate) fn tagfilter_inline_widget_ranges(block: &Block) -> Vec<Range<usize>> {
    tagfilter_widget_ranges_in(&block.inlines)
}

fn tagfilter_widget_touching(blocks: &[Block], byte: usize) -> Option<Range<usize>> {
    fn walk(blocks: &[Block], byte: usize) -> Option<Range<usize>> {
        for b in blocks {
            if let Some(w) = html_block_script_range(b) {
                if w.start <= byte && byte <= w.end {
                    return Some(w);
                }
            }
            for w in tagfilter_inline_widget_ranges(b) {
                if w.start <= byte && byte <= w.end {
                    return Some(w);
                }
            }
            if let Some(found) = walk(&b.children, byte) {
                return Some(found);
            }
        }
        None
    }
    walk(blocks, byte)
}

fn inline_source_span(inline: &Inline) -> Range<usize> {
    inline.source_range()
}

fn range_is_atomic_widget(block: &Block, range: &Range<usize>) -> bool {
    fn walk(block: &Block, range: &Range<usize>) -> bool {
        if html_block_atomic_range(block).as_ref() == Some(range)
            || tagfilter_inline_widget_ranges(block)
                .iter()
                .any(|w| w == range)
        {
            return true;
        }
        if matches!(block.kind, BlockKind::ThematicBreak) {
            let r = &block.source_range;
            if range.end == r.end
                && range.start >= r.start
                && range.start.saturating_sub(r.start) <= 3
            {
                return true;
            }
        } else if thematic_break_range(block).as_ref() == Some(range) {
            return true;
        }
        if block
            .inlines
            .iter()
            .any(|inline| atomic_caret_range(inline).as_ref() == Some(range))
        {
            return true;
        }
        block.children.iter().any(|child| walk(child, range))
    }
    walk(block, range)
}

fn ranges_overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && a.end > b.start
}

fn footnote_snap_home(block: &Block, byte: usize, bias: Bias) -> Option<usize> {
    fn walk(block: &Block, byte: usize) -> Option<(Range<usize>, Range<usize>)> {
        for inline in &block.inlines {
            if let Some((inner, outer)) = footnote_ref_ranges(inline) {
                if byte >= outer.start && byte < outer.end {
                    return Some((inner, outer));
                }
            }
        }
        for child in &block.children {
            if let Some(found) = walk(child, byte) {
                return Some(found);
            }
        }
        None
    }
    let (inner, outer) = walk(block, byte)?;
    if byte >= outer.start && byte < inner.start {
        return Some(inner.start);
    }
    if byte >= inner.end && byte < outer.end {
        return Some(match bias {
            Bias::Right => outer.end,
            Bias::Left => inner.start,
        });
    }
    None
}

fn step_left_caret(source: &str, block: &Block, range: &Range<usize>, byte: usize) -> usize {
    if range_is_atomic_widget(block, range) && byte > range.start {
        return range.start;
    }
    step_left_in_slice(source, range.start, byte)
}

fn step_right_caret(source: &str, block: &Block, byte: usize, range: &Range<usize>) -> usize {
    if range_is_atomic_widget(block, range) && byte < range.end {
        return range.end;
    }
    step_right_in_slice(source, byte, range.end)
}

fn expand_markdown_selection(
    tree: &RichTree,
    source: &str,
    range: Range<usize>,
    for_cut: bool,
) -> Range<usize> {
    let len = source.len();
    let mut start = range.start.min(len);
    let mut end = range.end.min(len);
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }
    if start == end {
        if for_cut && tree_in_table(&tree.blocks, start) {
            return start..end;
        }
        let mut expanded = expand_collapsed_caret_to_block(tree, source, start);
        if for_cut && expanded.start < expanded.end {
            let end = expanded.end;
            if end < source.len()
                && source.is_char_boundary(end)
                && source.as_bytes().get(end) == Some(&b'\n')
            {
                expanded.end = end + 1;
            }
        }
        return expanded;
    }
    if !source.is_char_boundary(start) || !source.is_char_boundary(end) {
        return start..end;
    }
    expand_inlines_for_copy(&tree.blocks, source, start..end, &mut start, &mut end);
    expand_block_chrome_for_copy(&tree.blocks, source, start..end, &mut start, &mut end);
    clamp_copy_range(tree, source, start, end, range)
}

fn expand_collapsed_caret_to_block(tree: &RichTree, source: &str, byte: usize) -> Range<usize> {
    let len = source.len();
    let byte = byte.min(len);
    if tree_in_table(&tree.blocks, byte) {
        return table_cell_copy_range(&tree.blocks, byte, source).unwrap_or(byte..byte);
    }
    if blank_caret_gap_at(tree, byte).is_some() {
        return byte..byte;
    }
    let Some(block) = innermost_block_at(tree, byte) else {
        return byte..byte;
    };
    if matches!(
        block.kind,
        BlockKind::Table { .. } | BlockKind::TableRow { .. } | BlockKind::TableCell
    ) {
        return table_cell_copy_range(&tree.blocks, byte, source).unwrap_or(byte..byte);
    }
    let selected = block.source_range.clone();
    if selected.start == selected.end {
        return clamp_copy_range(tree, source, selected.start, selected.end, selected);
    }
    if !source.is_char_boundary(selected.start) || !source.is_char_boundary(selected.end) {
        return byte..byte;
    }
    let mut start = selected.start.min(len);
    let mut end = selected.end.min(len);
    expand_inlines_for_copy(&tree.blocks, source, selected.clone(), &mut start, &mut end);
    expand_block_chrome_for_copy(&tree.blocks, source, selected.clone(), &mut start, &mut end);
    clamp_copy_range(tree, source, start, end, selected)
}

fn clamp_copy_range(
    tree: &RichTree,
    source: &str,
    mut start: usize,
    mut end: usize,
    fallback: Range<usize>,
) -> Range<usize> {
    let len = source.len();
    let fm = frontmatter_body_start(tree);
    start = start.max(fm).min(len);
    end = end.max(start).min(len);
    if !source.is_char_boundary(start) || !source.is_char_boundary(end) {
        let lo = fallback.start.min(len);
        let hi = fallback.end.min(len).max(lo);
        return lo..hi;
    }
    start..end
}

fn innermost_block_at(tree: &RichTree, byte: usize) -> Option<&Block> {
    fn descend(blocks: &[Block], byte: usize) -> Option<&Block> {
        let mut best = None;
        for b in blocks {
            if b.source_range.start <= byte && byte <= b.source_range.end {
                best = Some(b);
            }
        }
        let b = best?;
        descend(&b.children, byte).or(Some(b))
    }
    if let Some(block) = descend(&tree.blocks, byte) {
        return Some(block);
    }
    let mut prev = None;
    for b in &tree.blocks {
        if b.source_range.start <= byte {
            prev = Some(b);
        }
    }
    prev.or_else(|| tree.blocks.first())
}

fn tree_in_table(blocks: &[Block], byte: usize) -> bool {
    for b in blocks {
        if b.source_range.start <= byte && byte <= b.source_range.end {
            if matches!(b.kind, BlockKind::TableCell | BlockKind::Table { .. }) {
                return true;
            }
            if tree_in_table(&b.children, byte) {
                return true;
            }
        }
    }
    false
}

fn table_cell_copy_range(blocks: &[Block], byte: usize, source: &str) -> Option<Range<usize>> {
    fn walk(blocks: &[Block], byte: usize) -> Option<&Block> {
        for b in blocks {
            if matches!(b.kind, BlockKind::TableCell)
                && b.source_range.start <= byte
                && byte <= b.source_range.end
            {
                return Some(b);
            }
            if let Some(found) = walk(&b.children, byte) {
                return Some(found);
            }
        }
        None
    }
    let cell = walk(blocks, byte)?;
    Some(trim_cell_pipes(source, cell.source_range.clone()))
}

fn expand_inlines_for_copy(
    blocks: &[Block],
    source: &str,
    selected: Range<usize>,
    start: &mut usize,
    end: &mut usize,
) {
    for block in blocks {
        for inline in &block.inlines {
            match inline {
                Inline::Image { link, .. } => {
                    if let Some(img) = atomic_image_range(inline) {
                        if selected.start < img.end && selected.end > img.start {
                            let wrapped = expand_marks_and_link_chrome(
                                source,
                                img,
                                link.as_ref(),
                                block.source_range.start,
                                block.source_range.end.min(source.len()),
                            );
                            *start = (*start).min(wrapped.start);
                            *end = (*end).max(wrapped.end);
                        }
                    }
                }
                Inline::OpaqueInline { .. } => {
                    if let Some(img) = atomic_image_range(inline) {
                        if selected.start < img.end && selected.end > img.start {
                            let wrapped = expand_marks_and_link_chrome(
                                source,
                                img,
                                None,
                                block.source_range.start,
                                block.source_range.end.min(source.len()),
                            );
                            *start = (*start).min(wrapped.start);
                            *end = (*end).max(wrapped.end);
                        }
                    }
                    if let Some(br) = atomic_html_break_range(inline) {
                        if selected.start < br.end && selected.end > br.start {
                            *start = (*start).min(br.start);
                            *end = (*end).max(br.end);
                        }
                    }
                    if let Some((inner, outer)) = footnote_ref_ranges(inline) {
                        if selected.start <= inner.start && selected.end >= inner.end {
                            *start = (*start).min(outer.start);
                            *end = (*end).max(outer.end);
                        }
                    }
                }
                Inline::WikiLink {
                    raw, source_range, ..
                } => {
                    let vis = wiki_visible_range(raw, source_range.clone());
                    if selected.start <= vis.start && selected.end >= vis.end {
                        *start = (*start).min(source_range.start);
                        *end = (*end).max(source_range.end);
                    }
                }
                Inline::Math {
                    display,
                    source_range,
                    ..
                } => {
                    let vis = math_visible_range(source, *display, source_range.clone());
                    if selected.start <= vis.start && selected.end >= vis.end {
                        *start = (*start).min(source_range.start);
                        *end = (*end).max(source_range.end);
                    }
                }
                Inline::HardBreak { source_range, .. } => {
                    if selected.start < source_range.end && selected.end > source_range.start {
                        *start = (*start).min(source_range.start);
                        *end = (*end).max(source_range.end);
                    }
                }
                Inline::Emoji { source_range, .. } => {
                    if selected.start <= source_range.start && selected.end >= source_range.end {
                        *start = (*start).min(source_range.start);
                        *end = (*end).max(source_range.end);
                    }
                }
                Inline::Run {
                    text,
                    raw,
                    source_range,
                    marks,
                    link,
                    ..
                } => {
                    if !marks.contains(MarkSet::CODE) {
                        if let Some(slice) = raw.as_deref() {
                            if (is_decoded_character_reference(slice, text)
                                || is_decoded_backslash_escape(slice, text))
                                && selected.start < source_range.end
                                && selected.end > source_range.start
                            {
                                *start = (*start).min(source_range.start);
                                *end = (*end).max(source_range.end);
                            }
                        }
                    }
                    if selected.start <= source_range.start && selected.end >= source_range.end {
                        let wrapped = expand_marks_and_link_chrome(
                            source,
                            source_range.clone(),
                            link.as_ref(),
                            block.source_range.start,
                            block.source_range.end.min(source.len()),
                        );
                        *start = (*start).min(wrapped.start);
                        *end = (*end).max(wrapped.end);
                    }
                }
                Inline::SoftBreak { .. } => {}
            }
        }
        expand_inlines_for_copy(&block.children, source, selected.clone(), start, end);
    }
}

fn expand_block_chrome_for_copy(
    blocks: &[Block],
    source: &str,
    selected: Range<usize>,
    start: &mut usize,
    _end: &mut usize,
) {
    for block in blocks {
        if matches!(
            block.kind,
            BlockKind::CodeBlock { .. }
                | BlockKind::Opaque { .. }
                | BlockKind::Table { .. }
                | BlockKind::TableRow { .. }
                | BlockKind::TableCell
        ) {
            expand_block_chrome_for_copy(&block.children, source, selected.clone(), start, _end);
            continue;
        }
        let ranges = inline_ranges(block, source);
        if !ranges.is_empty() {
            let body_start = ranges
                .iter()
                .map(|r| r.start)
                .min()
                .unwrap_or(selected.start);
            let body_end = ranges.iter().map(|r| r.end).max().unwrap_or(selected.end);
            if selected.start <= body_start && selected.end >= body_end {
                *start = (*start).min(block.source_range.start);
            }
        }
        expand_block_chrome_for_copy(&block.children, source, selected.clone(), start, _end);
    }
}

/// ASCII mark delimiters immediately outside `inner` (`*`, `_`, ticks, `~`, `=`, `^`).
pub(crate) fn expand_mark_delimiters(
    source: &str,
    block: &Block,
    inner: &Range<usize>,
) -> Range<usize> {
    let lo = block.source_range.start;
    let hi = block.source_range.end.min(source.len());
    let mut start = inner.start;
    let mut end = inner.end;
    while start > lo {
        let b = source.as_bytes()[start - 1];
        if matches!(b, b'*' | b'_' | b'`' | b'~' | b'=' | b'^') {
            start -= 1;
        } else {
            break;
        }
    }
    while end < hi {
        let b = source.as_bytes()[end];
        if matches!(b, b'*' | b'_' | b'`' | b'~' | b'=' | b'^') {
            end += 1;
        } else {
            break;
        }
    }
    start..end
}

fn inline_inner_outer(
    source: &str,
    block: &Block,
    inline: &Inline,
) -> Option<(Range<usize>, Range<usize>)> {
    if let Some(chrome) = link_reference_def_chrome(source, block) {
        if let Inline::Run { source_range, .. } = inline {
            if *source_range == chrome.label {
                return Some((chrome.label, chrome.open.start..chrome.close.end));
            }
            if chrome.dest.start <= source_range.start
                && source_range.end <= chrome.dest.end
                && source_range.end > source_range.start
            {
                if let Inline::Run {
                    link: Some(link), ..
                } = inline
                {
                    let mut outer = chrome.colon.start..chrome.dest.end;
                    if let Some(angle) = link.angle_span(source_range) {
                        outer.end = outer.end.max(angle.end);
                        outer.start = outer.start.min(angle.start).min(chrome.colon.start);
                    }
                    return Some((source_range.clone(), outer));
                }
            }
            if let Some(inner) = quoted_title_inner(source, source_range.clone()) {
                return Some((inner, source_range.clone()));
            }
        }
    }
    match inline {
        Inline::Run {
            text,
            raw,
            source_range,
            marks,
            link,
            ..
        } => {
            let inner = if marks.contains(MarkSet::CODE) {
                code_span_visible_range(source, source_range.clone(), text)
            } else if source
                .get(source_range.clone())
                .or(raw.as_deref())
                .is_some_and(|slice| {
                    is_decoded_character_reference(slice, text)
                        || is_decoded_backslash_escape(slice, text)
                })
            {
                character_reference_visible_range(source_range.clone())
            } else {
                source_range.clone()
            };
            let outer = expand_marks_and_link_chrome(
                source,
                source_range.clone(),
                link.as_ref(),
                block.source_range.start,
                block.source_range.end.min(source.len()),
            );
            Some((inner, outer))
        }
        Inline::Image {
            source_range,
            marks: _,
            link,
            ..
        } => {
            let inner = source_range.clone();
            let outer = expand_marks_and_link_chrome(
                source,
                inner.clone(),
                link.as_ref(),
                block.source_range.start,
                block.source_range.end.min(source.len()),
            );
            Some((inner, outer))
        }
        Inline::OpaqueInline {
            raw, source_range, ..
        } => {
            if let Some(inner) =
                crate::html_visual::footnote_ref_inner_range(raw, source_range.clone())
            {
                return Some((inner, source_range.clone()));
            }
            if crate::html_visual::opaque_inline_is_caret_chrome(raw) {
                // Phrasing tags (`<b>`, `<a href>`, comments, …) are dest
                // chrome with no inner caret home — same skip as `[` / `](url)`.
                // Inner is empty at the tag end so Left/Backspace land on the
                // previous visible byte instead of nibbling `>`.
                let end = source_range.end;
                return Some((end..end, source_range.clone()));
            }
            if crate::html_visual::html_inline_image(raw).is_some() {
                let inner = source_range.clone();
                let outer = expand_marks_and_link_chrome(
                    source,
                    inner.clone(),
                    None,
                    block.source_range.start,
                    block.source_range.end.min(source.len()),
                );
                return Some((inner, outer));
            }
            None
        }
        Inline::Math {
            display,
            source_range,
            marks,
            ..
        } => {
            let inner = math_visible_range(source, *display, source_range.clone());
            let mut outer = source_range.clone();
            if !marks.is_empty() {
                outer = expand_mark_delimiters(source, block, &outer);
            }
            Some((inner, outer))
        }
        Inline::WikiLink {
            raw,
            source_range,
            marks,
            ..
        } => {
            let inner = wiki_visible_range(raw, source_range.clone());
            let mut outer = source_range.clone();
            if !marks.is_empty() {
                outer = expand_mark_delimiters(source, block, &outer);
            }
            Some((inner, outer))
        }
        Inline::Emoji {
            raw,
            source_range,
            marks: _,
            link,
            ..
        } => {
            let inner = emoji_visible_range(raw, source_range.clone());
            let outer = expand_marks_and_link_chrome(
                source,
                source_range.clone(),
                link.as_ref(),
                block.source_range.start,
                block.source_range.end.min(source.len()),
            );
            Some((inner, outer))
        }
        _ => None,
    }
}

/// Keyboard End home for HTML phrasing suffix chrome (`</a>` / `</b>` /
/// comments / PI / CDATA).
///
/// Those tags are dest chrome with an empty inner at the tag *end*, so
/// `clamp_raw_prefix` Left from the insert home (the byte after the inner
/// text) retreats onto the last inner letter. Markdown `[label](url)` keeps
/// `]` because that byte *is* the label inner.end. Restore that same home.
fn html_phrasing_suffix_end_home(blocks: &[Block], at: usize) -> Option<usize> {
    for block in blocks {
        for inline in &block.inlines {
            let Inline::OpaqueInline {
                raw, source_range, ..
            } = inline
            else {
                continue;
            };
            if !matches!(
                crate::html_visual::html_reveal_kind(raw),
                crate::html_visual::HtmlRevealKind::Close(_)
                    | crate::html_visual::HtmlRevealKind::Solo
            ) {
                continue;
            }
            if source_range.start >= source_range.end {
                continue;
            }
            if at >= source_range.start && at < source_range.end {
                return Some(source_range.start);
            }
            if at < source_range.start && at + 1 == source_range.start {
                return Some(source_range.start);
            }
        }
        if let Some(home) = html_phrasing_suffix_end_home(&block.children, at) {
            return Some(home);
        }
    }
    None
}

/// Keyboard End home after a last-in-line character reference / backslash
/// escape. `amp;` / the escaped `*` are dest chrome; clamp Left from the
/// line end (or from `]` after dest skip) can sit on those bytes or retreat
/// onto the glyph. Restore the insert home after the painted glyph.
fn entity_escape_end_home(blocks: &[Block], at: usize) -> Option<usize> {
    for block in blocks {
        for inline in &block.inlines {
            let Some(range) = atomic_character_reference_range(inline) else {
                continue;
            };
            if at < range.start || at > range.end {
                continue;
            }
            if at == range.end {
                return None;
            }
            return Some(range.end);
        }
        if let Some(home) = entity_escape_end_home(&block.children, at) {
            return Some(home);
        }
    }
    None
}

/// Keyboard End home after a last-in-line footnote ref. `[^1]` is a closed
/// suffix, so clamp Left from the line end retreats onto the label start.
/// Restore after `]` (same place Right from the painted mark lands).
fn footnote_ref_end_home(blocks: &[Block], at: usize) -> Option<usize> {
    for block in blocks {
        for inline in &block.inlines {
            let Some((_, outer)) = footnote_ref_ranges(inline) else {
                continue;
            };
            if at < outer.start || at > outer.end {
                continue;
            }
            if at == outer.end {
                return None;
            }
            return Some(outer.end);
        }
        if let Some(home) = footnote_ref_end_home(&block.children, at) {
            return Some(home);
        }
    }
    None
}

/// Left of dest chrome (`$`, `[[`, wrapping `$$\n`). When the opener sits
/// immediately after a quote/list prefix (`> $$`, `- $$`), do not land on
/// `>` / `- ` — `clamp_raw_prefix` would bounce back onto the opener.
fn left_of_dest_outer(source: &str, inner_start: usize, outer_start: usize) -> usize {
    if outer_start == 0 {
        return inner_start;
    }
    let ls = source_line_start(source, outer_start);
    let le = source_line_end_exclusive(source, outer_start);
    let line = &source[ls..le];
    let prefix_len = quote_list_prefix_on_line(line).len();
    let after = ls + prefix_len;
    let indent = source
        .get(after..outer_start)
        .map(|s| s.bytes().take_while(|&b| b == b' ' || b == b'\t').count())
        .unwrap_or(0);
    let dest = if after + indent == outer_start {
        ls
    } else {
        outer_start
    };
    if dest == 0 {
        inner_start
    } else {
        dest - 1
    }
}

fn walk_inline_inner_outer(
    blocks: &[Block],
    source: &str,
    f: &mut impl FnMut(Range<usize>, Range<usize>),
) {
    for block in blocks {
        if matches!(block.kind, BlockKind::Toc { .. }) {
            let inner = toc_visible_range(source, block.source_range.clone());
            if inner.start > block.source_range.start || inner.end < block.source_range.end {
                f(inner, block.source_range.clone());
            }
        }
        for inline in &block.inlines {
            if let Some((inner, outer)) = inline_inner_outer(source, block, inline) {
                f(inner, outer);
            }
        }
        for dest in markdown_link_dests(source, block) {
            f(dest.url.clone(), dest.outer.clone());
            if let Some(title) = dest.title.clone() {
                let open = title.start.saturating_sub(1);
                f(title, open..dest.outer.end);
            }
        }
        walk_inline_inner_outer(&block.children, source, f);
    }
}

/// `]: ` between a GFM `[label]: dest` label run and dest run is dest chrome.
/// Snap onto the dest (the following painted inner), not `:` / `]`.
fn link_ref_def_gap_snap(ranges: &[Range<usize>], byte: usize) -> Option<usize> {
    for pair in ranges.windows(2) {
        if byte > pair[0].end && byte < pair[1].start {
            return Some(pair[1].start);
        }
    }
    None
}

fn markdown_link_dest_snap(source: &str, block: &Block, byte: usize) -> Option<usize> {
    for dest in markdown_link_dests(source, block) {
        if let Some(home) = dest.snap(byte) {
            return Some(home);
        }
    }
    for child in &block.children {
        if let Some(home) = markdown_link_dest_snap(source, child, byte) {
            return Some(home);
        }
    }
    None
}

fn link_ref_def_title_snap(source: &str, block: &Block, byte: usize) -> Option<usize> {
    for inline in &block.inlines {
        let Inline::Run { source_range, .. } = inline else {
            continue;
        };
        let Some(inner) = quoted_title_inner(source, source_range.clone()) else {
            continue;
        };
        if byte < source_range.start || byte >= source_range.end {
            continue;
        }
        if byte >= inner.start && byte <= inner.end {
            return Some(byte);
        }
        return Some(if byte < inner.start {
            inner.start
        } else {
            inner.end
        });
    }
    None
}

fn dest_inner_containing(source: &str, block: &Block, byte: usize) -> Option<Range<usize>> {
    markdown_link_dests(source, block)
        .into_iter()
        .flat_map(|d| d.inners())
        .find(|inner| inner.start <= byte && byte <= inner.end)
}

fn caret_range_index(
    ranges: &[Range<usize>],
    source: &str,
    block: &Block,
    byte: usize,
) -> Option<usize> {
    if let Some(inner) = dest_inner_containing(source, block, byte) {
        if let Some(i) = ranges
            .iter()
            .position(|r| r.start == inner.start && r.end == inner.end)
        {
            return Some(i);
        }
    }
    ranges.iter().position(|r| r.start <= byte && byte <= r.end)
}

fn covering_dest_inner(
    dest_inners: &[Range<usize>],
    inner: &Range<usize>,
    outer: &Range<usize>,
    byte: usize,
) -> bool {
    dest_inners.iter().any(|dest| {
        dest.start <= byte
            && byte <= dest.end
            && (dest.start != inner.start || dest.end != inner.end)
            && outer.start <= dest.start
            && outer.end >= dest.end
    })
}

fn dest_inners_in_tree(blocks: &[Block], source: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    fn walk(blocks: &[Block], source: &str, out: &mut Vec<Range<usize>>) {
        for block in blocks {
            for dest in markdown_link_dests(source, block) {
                out.extend(dest.inners());
            }
            walk(&block.children, source, out);
        }
    }
    walk(blocks, source, &mut out);
    out
}

impl RichEngine {
    /// Insert home when the caret sits on markdown-link `[`, HTML phrasing
    /// open tags (`<b>`, `<a href>`), or interior bytes of inline comment /
    /// PI / CDATA openers (`<!--`). The End home at the comment `<` still
    /// extends the previous text (`hellox<!--`). Empty wrap wraps the
    /// comment as a widget (`**<!-- x -->**`), not splice `**x**<!--`.
    /// Close tags (`</a>`) are the End insert home. `[TOC]` / `[[wiki]]` /
    /// `[!NOTE]` are not this path.
    pub(crate) fn link_or_html_phrasing_prefix_home(
        &self,
        source: &str,
        offset: usize,
    ) -> Option<usize> {
        fn is_link_or_html_open_prefix(inline: &Inline) -> bool {
            match inline {
                Inline::Run {
                    link: Some(link), ..
                } if !link.autolink => true,
                Inline::OpaqueInline { raw, .. } => {
                    matches!(
                        crate::html_visual::html_reveal_kind(raw),
                        crate::html_visual::HtmlRevealKind::Open(_)
                    )
                }
                _ => false,
            }
        }
        fn walk(blocks: &[Block], source: &str, offset: usize) -> Option<usize> {
            for block in blocks {
                if matches!(block.kind, BlockKind::Toc { .. }) {
                    if let Some(found) = walk(&block.children, source, offset) {
                        return Some(found);
                    }
                    continue;
                }
                for inline in &block.inlines {
                    if let Inline::OpaqueInline {
                        raw, source_range, ..
                    } = inline
                    {
                        if let Some(inner) = crate::html_visual::html_solo_markup_inner_range(
                            raw,
                            source_range.clone(),
                        ) {
                            if offset > source_range.start && offset < inner.start {
                                return Some(inner.start.min(source.len()));
                            }
                            if offset > inner.end && offset < source_range.end {
                                return Some(inner.end.min(source.len()));
                            }
                        }
                    }
                    let Some((inner, outer)) = inline_inner_outer(source, block, inline) else {
                        continue;
                    };
                    if offset < outer.start || offset >= inner.start {
                        continue;
                    }
                    // Only the opener byte (`[` / `<`), not a following insert
                    // home that still sits before inner (`[**|**hello](url)`).
                    if !matches!(source.as_bytes().get(offset), Some(b'[' | b'<')) {
                        continue;
                    }
                    if !is_link_or_html_open_prefix(inline) {
                        continue;
                    }
                    if inner.start != offset {
                        return Some(inner.start.min(source.len()));
                    }
                }
                if let Some(found) = walk(&block.children, source, offset) {
                    return Some(found);
                }
            }
            None
        }
        walk(&self.tree.blocks, source, offset)
    }

    /// InsertText home when the caret sits on dest-chrome openers Home/click
    /// already skip: GFM `<https://…>` / `$math$` / `$$…$$` / `[[wiki]]` /
    /// `:emoji:` / wrapping `[![img]](url)` `[`, markdown dest `(` /
    /// `"title"`, and `[ref]: dest "title"` quote wrapping.
    ///
    /// InsertText-only. Empty wrap still wraps autolink / image widgets
    /// (`**<https://…>**`, `**![alt](url)**`). Wrap-mark `*` / `~~` stay put
    /// so `[**|**hello](url)` types inside the pair. Standalone `![alt](url)`
    /// at `!` stays insert-before. `[TOC]` / `[!NOTE]` are not this path.
    pub(crate) fn inline_dest_chrome_insert_home(
        &self,
        source: &str,
        offset: usize,
    ) -> Option<usize> {
        fn opener_prefix_of(inline: &Inline, source: &str, offset: usize) -> bool {
            let b = source.as_bytes().get(offset).copied();
            match inline {
                Inline::Run {
                    link: Some(link), ..
                } if link.autolink => matches!(b, Some(b'<')),
                Inline::Math { .. } => matches!(b, Some(b'$')),
                Inline::WikiLink { .. } => matches!(b, Some(b'[')),
                Inline::Emoji { .. } => matches!(b, Some(b':')),
                Inline::Image { link, .. } if link.is_some() => matches!(b, Some(b'[')),
                _ => false,
            }
        }
        fn dest_wrap_home(source: &str, block: &Block, offset: usize) -> Option<usize> {
            for dest in markdown_link_dests(source, block) {
                if offset >= dest.outer.start && offset < dest.url.start {
                    return (dest.url.start != offset).then_some(dest.url.start.min(source.len()));
                }
                if let Some(title) = &dest.title {
                    if offset >= dest.url.end && offset < title.start {
                        return (title.start != offset).then_some(title.start.min(source.len()));
                    }
                }
            }
            if matches!(block.kind, BlockKind::LinkReferenceDefinition { .. }) {
                for inline in &block.inlines {
                    let Inline::Run { source_range, .. } = inline else {
                        continue;
                    };
                    let Some(inner) = quoted_title_inner(source, source_range.clone()) else {
                        continue;
                    };
                    if offset >= source_range.start && offset < inner.start {
                        return Some(inner.start.min(source.len()));
                    }
                }
            }
            None
        }
        fn walk(blocks: &[Block], source: &str, offset: usize) -> Option<usize> {
            for block in blocks {
                if matches!(block.kind, BlockKind::Toc { .. }) {
                    if let Some(found) = walk(&block.children, source, offset) {
                        return Some(found);
                    }
                    continue;
                }
                if let Some(home) = dest_wrap_home(source, block, offset) {
                    return Some(home);
                }
                for inline in &block.inlines {
                    let Some((inner, outer)) = inline_inner_outer(source, block, inline) else {
                        continue;
                    };
                    if offset < outer.start || offset >= inner.start {
                        continue;
                    }
                    if !opener_prefix_of(inline, source, offset) {
                        continue;
                    }
                    if inner.start != offset {
                        return Some(inner.start.min(source.len()));
                    }
                }
                if let Some(found) = walk(&block.children, source, offset) {
                    return Some(found);
                }
            }
            None
        }
        walk(&self.tree.blocks, source, offset.min(source.len()))
    }

    /// `[` / `](url)` / `**` / ticks / autolink `<>` / `$math$` / `[[wiki]]`
    /// / `:emoji:` / HTML phrasing tags / character-reference `amp;` /
    /// backslash-escape `*` adjacent to a visible run, including wrapping dest
    /// around a linked image.
    /// `inner.end` (caret after the last visible letter) is not skipped so
    /// typing still extends the label / marked text / TeX / shortcode, except
    /// closed suffixes (`[^1]`, `&amp;`, `\*`).
    fn skip_inline_delimiter_chrome(&self, source: &str, byte: usize, bias: Bias) -> usize {
        let mut at = byte.min(source.len());
        if let Some(w) = tagfilter_widget_touching(&self.tree.blocks, at) {
            return match bias {
                Bias::Right if at >= w.start && at < w.end => w.end,
                Bias::Left if at > w.start && at <= w.end => w.start,
                _ => at,
            };
        }
        for _ in 0..16 {
            let next = self.inline_chrome_step(source, at, bias);
            if next == at {
                return at;
            }
            at = next.min(source.len());
        }
        at
    }

    fn inline_chrome_step(&self, source: &str, byte: usize, bias: Bias) -> usize {
        let dest_inners = dest_inners_in_tree(&self.tree.blocks, source);
        let mut found: Option<(Range<usize>, Range<usize>, bool)> = None;
        walk_inline_inner_outer(&self.tree.blocks, source, &mut |inner, outer| {
            if inner.start == outer.start && inner.end == outer.end {
                return;
            }
            if covering_dest_inner(&dest_inners, &inner, &outer, byte) {
                return;
            }
            let closed_suffix = source
                .get(outer.clone())
                .is_some_and(|s| crate::html_visual::footnote_ref_label(s).is_some())
                || character_reference_closed_suffix(source, &inner, &outer)
                || backslash_escape_closed_suffix(source, &inner, &outer);
            let in_prefix = byte >= outer.start && byte < inner.start;
            let in_suffix = if closed_suffix {
                byte >= inner.end && byte < outer.end
            } else {
                byte > inner.end && byte < outer.end
            };
            if in_prefix || in_suffix {
                found = Some((inner, outer, closed_suffix));
            }
        });
        let Some((inner, outer, closed_suffix)) = found else {
            return byte;
        };
        match bias {
            Bias::Right => {
                if byte < inner.start {
                    inner.start
                } else {
                    outer.end.min(source.len())
                }
            }
            Bias::Left => {
                if byte < inner.start {
                    left_of_dest_outer(source, inner.start, outer.start)
                } else if closed_suffix {
                    inner.start
                } else {
                    inner.end
                }
            }
        }
    }

    /// True when `byte` is markdown or HTML chrome around a visible run
    /// (`[`, `](url)`, `**`, ticks, autolink `<>`, `$math$`, `[[wiki]]`,
    /// `:emoji:`, `<b>` / `<a href>` / comments). Includes the byte at
    /// `inner.end` (`]`) so Backspace/Delete do not nibble dest / closers.
    /// Footnote `[^label]` suffix `]` is included the same way.
    pub(crate) fn byte_is_inline_chrome(&self, source: &str, byte: usize) -> bool {
        let dest_inners = dest_inners_in_tree(&self.tree.blocks, source);
        if dest_inners
            .iter()
            .any(|inner| inner.start <= byte && byte < inner.end)
        {
            return false;
        }
        let mut hit = false;
        walk_inline_inner_outer(&self.tree.blocks, source, &mut |inner, outer| {
            if inner.start == outer.start && inner.end == outer.end {
                return;
            }
            if covering_dest_inner(&dest_inners, &inner, &outer, byte) {
                return;
            }
            if (byte >= outer.start && byte < inner.start)
                || (byte >= inner.end && byte < outer.end)
            {
                hit = true;
            }
        });
        hit
    }

    /// True when `range` sits wholly inside markdown link/image dest
    /// `(url "title")`, so Backspace/Delete there edits dest instead of
    /// swallowing an atomic image widget.
    pub(crate) fn range_is_inside_link_dest(&self, source: &str, range: Range<usize>) -> bool {
        if range.start >= range.end {
            return false;
        }
        fn walk(blocks: &[Block], source: &str, range: &Range<usize>) -> bool {
            for block in blocks {
                if markdown_link_dests(source, block)
                    .iter()
                    .any(|dest| dest.outer.start <= range.start && range.end <= dest.outer.end)
                {
                    return true;
                }
                if walk(&block.children, source, range) {
                    return true;
                }
            }
            false
        }
        walk(&self.tree.blocks, source, &range)
    }

    /// True when `range` is exactly one atomic dest-chrome widget (HTML
    /// tagfilter / `<video>` paragraph blobs, `<br>`, `<img>`, rules, …).
    /// Backspace that already selected the whole widget must not trim it as
    /// phrasing chrome (that would Noop a Type-7 `<video></video>` delete).
    pub(crate) fn is_exact_atomic_widget(&self, range: &Range<usize>) -> bool {
        self.tree
            .blocks
            .iter()
            .any(|block| range_is_atomic_widget(block, range))
    }

    /// Grow `range` to cover any overlapping atomic widget (image, HTML
    /// `<br>` / `<img>` / `<hr>`, thematic break, footnote `[^label]`).
    pub(crate) fn expand_atomic_widget_range(&self, range: Range<usize>) -> Range<usize> {
        let mut start = range.start;
        let mut end = range.end;
        fn consider(
            widget: Range<usize>,
            range: &Range<usize>,
            start: &mut usize,
            end: &mut usize,
        ) {
            if ranges_overlap(range, &widget) {
                *start = (*start).min(widget.start);
                *end = (*end).max(widget.end);
            }
        }
        fn walk(blocks: &[Block], range: &Range<usize>, start: &mut usize, end: &mut usize) {
            for block in blocks {
                if let Some(w) = html_block_atomic_range(block) {
                    consider(w, range, start, end);
                }
                if let Some(w) = thematic_break_range(block) {
                    consider(w, range, start, end);
                }
                for w in tagfilter_inline_widget_ranges(block) {
                    consider(w, range, start, end);
                }
                for inline in &block.inlines {
                    if let Some(w) = atomic_delete_range(inline) {
                        consider(w, range, start, end);
                    }
                }
                walk(&block.children, range, start, end);
            }
        }
        walk(&self.tree.blocks, &range, &mut start, &mut end);
        start..end
    }
}

fn leaf_inline_ranges(block: &Block, source: &str) -> Vec<Range<usize>> {
    let widgets = tagfilter_inline_widget_ranges(block);
    let mut out = Vec::new();
    let mut emitted_widget = vec![false; widgets.len()];
    for inline in &block.inlines {
        let span = inline.source_range();
        if let Some(i) = widgets
            .iter()
            .position(|w| span.start >= w.start && span.end <= w.end && span.start < w.end)
        {
            if !emitted_widget[i] {
                out.push(widgets[i].clone());
                emitted_widget[i] = true;
            }
            continue;
        }
        match inline {
            Inline::Run {
                text,
                source_range,
                marks,
                ..
            } if marks.contains(MarkSet::CODE) => {
                out.push(code_span_visible_range(source, source_range.clone(), text));
            }
            Inline::Run { source_range, .. }
            | Inline::Image { source_range, .. }
            | Inline::Math { source_range, .. }
            | Inline::WikiLink { source_range, .. }
            | Inline::Emoji { source_range, .. } => {
                out.push(source_range.clone());
            }
            Inline::OpaqueInline {
                source_range, raw, ..
            } => {
                if let Some(inner) =
                    crate::html_visual::footnote_ref_inner_range(raw, source_range.clone())
                {
                    out.push(inner);
                } else if !crate::html_visual::opaque_inline_is_caret_chrome(raw) {
                    out.push(source_range.clone());
                }
            }
            Inline::SoftBreak { .. } => {
                // Not extra caret homes. WYSIWYG maps the painted space
                // onto `source_range.start`, which sits at the previous run's end.
            }
            Inline::HardBreak { source_range, .. } => {
                // Two-space / backslash marker is dest chrome like `<br>`.
                out.push(source_range.clone());
            }
        }
    }
    for dest in markdown_link_dests(source, block) {
        for inner in dest.inners() {
            if inner.start < inner.end
                && !out
                    .iter()
                    .any(|r| r.start == inner.start && r.end == inner.end)
            {
                out.push(inner);
            }
        }
    }
    out.sort_by_key(|r| (r.start, r.end));
    if out.is_empty() {
        out.push(block.source_range.clone());
    }
    out
}

/// Preserve NodeIds of top-level blocks whose source slice is unchanged
/// (common prefix/suffix match, like a diff), and report the changed window.
fn reconcile_ids(old: &RichTree, new: &mut RichTree, new_source: &str) -> BlockSplice {
    // Old ranges index the old source, so blocks are matched by the content
    // hash captured at import time, not by ranges.
    let prefix = old
        .blocks
        .iter()
        .zip(new.blocks.iter())
        .take_while(|(o, n)| o.content_hash == hash_slice(new_source, &n.source_range))
        .count();
    let remaining_old = old.blocks.len() - prefix;
    let remaining_new = new.blocks.len() - prefix;
    let suffix = old.blocks[prefix..]
        .iter()
        .rev()
        .zip(new.blocks[prefix..].iter().rev())
        .take(remaining_old.min(remaining_new))
        .take_while(|(o, n)| o.content_hash == hash_slice(new_source, &n.source_range))
        .count();

    for i in 0..prefix {
        new.blocks[i].id = old.blocks[i].id;
    }
    for k in 0..suffix {
        let oi = old.blocks.len() - 1 - k;
        let ni = new.blocks.len() - 1 - k;
        new.blocks[ni].id = old.blocks[oi].id;
    }
    BlockSplice {
        range: prefix..old.blocks.len() - suffix,
        new_count: new.blocks.len() - suffix - prefix,
    }
}

fn hash_slice(source: &str, range: &Range<usize>) -> u64 {
    hash_str(source.get(range.clone()).unwrap_or(""))
}

pub(crate) fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// One source-line step, preserving column when the target line is long enough.
fn adjacent_source_line_offset(source: &str, cursor: usize, down: bool) -> usize {
    let cursor = cursor.min(source.len());
    let line_start = source[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col = cursor - line_start;
    let mut starts = vec![0usize];
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    let line = match starts.binary_search(&line_start) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };
    let target_line = if down {
        (line + 1).min(starts.len().saturating_sub(1))
    } else {
        line.saturating_sub(1)
    };
    if target_line == line {
        return cursor;
    }
    let start = starts[target_line];
    let end = starts
        .get(target_line + 1)
        .copied()
        .unwrap_or(source.len())
        .saturating_sub(1)
        .max(start);
    (start + col).min(end)
}

/// Step one extended grapheme cluster left within `[start, byte)`.
///
/// A visible character is not always a single Unicode scalar: `e` plus its
/// combining accent and emoji joined with ZWJ must be one caret/delete step.
fn step_left_in_slice(source: &str, start: usize, byte: usize) -> usize {
    let start = start.min(source.len());
    let byte = byte.min(source.len());
    if byte <= start {
        return start;
    }
    let slice = &source[start..byte];
    let Some((last, _)) = slice.grapheme_indices(true).next_back() else {
        return start;
    };
    start + last
}

/// Step one extended grapheme cluster right within `(byte, end]`.
pub(crate) fn step_right_in_slice(source: &str, byte: usize, end: usize) -> usize {
    let byte = byte.min(source.len());
    let end = end.min(source.len());
    if byte >= end {
        return end;
    }
    let slice = &source[byte..end];
    let Some(first) = slice.graphemes(true).next() else {
        return end;
    };
    (byte + first.len()).min(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;
    use std::ops::Range;

    fn engine_for(source: &str) -> (Document, RichEngine) {
        let doc = Document::new(source);
        let mut engine = RichEngine::new();
        engine.sync(&doc);
        (doc, engine)
    }

    #[test]
    fn arrows_step_over_extended_grapheme_clusters() {
        // Combining marks and ZWJ emoji are each one visible character. Their
        // source byte spans are deliberately different to make a scalar-based
        // step fail both directions.
        let combining = "e\u{301}";
        let zwj_emoji = "👩\u{200d}💻";
        let source = format!("{combining}{zwj_emoji}x\n");
        let (_doc, engine) = engine_for(&source);
        let combining_end = combining.len();
        let emoji_end = combining_end + zwj_emoji.len();

        assert_eq!(engine.next_caret(&source, 0), combining_end);
        assert_eq!(engine.next_caret(&source, combining_end), emoji_end);
        assert_eq!(engine.prev_caret(&source, emoji_end), combining_end);
        assert_eq!(engine.prev_caret(&source, combining_end), 0);
    }

    #[test]
    fn sync_is_noop_when_revision_unchanged() {
        let (doc, mut engine) = engine_for("# A\n\npara\n");
        let ids: Vec<_> = engine.tree().blocks.iter().map(|b| b.id).collect();
        engine.sync(&doc);
        let ids_after: Vec<_> = engine.tree().blocks.iter().map(|b| b.id).collect();
        assert_eq!(ids, ids_after);
    }

    #[test]
    fn cell_edit_range_excludes_pipe_separators() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (_doc, engine) = engine_for(source);
        let a = source.find('a').expect("a");
        let pipe = a + source[a..].find('|').expect("pipe after a");
        let range = engine.cell_edit_range(a, source).expect("cell a");
        assert!(
            range.contains(&a),
            "cell range must cover the header text, got {range:?}"
        );
        assert!(
            !source[range.clone()].contains('|'),
            "editable cell range must not include `|`, got {:?}",
            &source[range.clone()]
        );
        if let Some(on_pipe) = engine.cell_edit_range(pipe, source) {
            assert!(
                !source[on_pipe.clone()].contains('|'),
                "a pipe byte must not yield an editable range that includes `|`, got {:?}",
                &source[on_pipe]
            );
        }
    }

    #[test]
    fn cell_edit_range_keeps_escaped_table_pipe() {
        for source in [
            "| a\\|b | c |\n|---|---|\n| 1 | 2 |\n",
            "| a | b\\| |\n|---|---|\n| 1 | 2 |\n",
            "> | a\\|b | c |\n> |---|---|\n> | 1 | 2 |\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let slash = source.find("\\|").expect("\\|");
            let pipe = slash + 1;
            let range = engine
                .cell_edit_range(slash, source)
                .or_else(|| engine.cell_edit_range(pipe, source))
                .expect("escaped pipe is inside a cell");
            assert!(
                source[range.clone()].contains("\\|"),
                "cell range must include escaped \\|, {source:?} got {:?}",
                &source[range.clone()]
            );
            fn table_cols(blocks: &[Block]) -> Option<usize> {
                for b in blocks {
                    if matches!(b.kind, BlockKind::Table { .. }) {
                        return Some(b.children[0].children.len());
                    }
                    if let Some(n) = table_cols(&b.children) {
                        return Some(n);
                    }
                }
                None
            }
            assert_eq!(table_cols(&engine.tree().blocks), Some(2), "{source:?}");
        }
    }

    #[test]
    fn node_ids_stable_outside_edited_window() {
        let (mut doc, mut engine) = engine_for("# A\n\nfirst\n\nsecond\n\nthird\n");
        let before: Vec<_> = engine.tree().blocks.iter().map(|b| b.id).collect();
        assert_eq!(before.len(), 4);
        // Edit the "second" paragraph only.
        let source = doc.buffer.content();
        let pos = source.find("second").unwrap();
        doc.replace_range(pos, pos + 6, "SECOND!");
        engine.sync(&doc);
        let after: Vec<_> = engine.tree().blocks.iter().map(|b| b.id).collect();
        assert_eq!(after.len(), 4);
        assert_eq!(before[0], after[0], "heading id stable");
        assert_eq!(before[1], after[1], "first para id stable");
        assert_ne!(before[2], after[2], "edited para gets fresh id");
        assert_eq!(before[3], after[3], "third para id stable");
        let splice = engine.last_splice().unwrap();
        assert_eq!(splice.range, 2..3);
        assert_eq!(splice.new_count, 1);
    }

    #[test]
    fn snap_caret_skips_delimiter_bytes() {
        let source = "**bold** tail\n";
        let (_doc, engine) = engine_for(source);
        // Byte 1 is inside the opening "**".
        let snapped = engine.snap_caret(1, Bias::Right);
        assert_eq!(snapped, 2, "snaps to the start of the bold text");
        // Inside the closing "**" snapping left lands at end of "bold".
        let close = source.find("**ated").unwrap_or(6);
        let snapped_left = engine.snap_caret(close + 1, Bias::Left);
        assert_eq!(snapped_left, 6);
    }

    #[test]
    fn snap_caret_skips_inline_code_backticks() {
        let source = "`code` tail\n";
        let (_doc, engine) = engine_for(source);
        let c = source.find('c').expect("c");
        let tick = source.find('`').expect("tick");
        assert_eq!(
            engine.snap_caret(tick, Bias::Right),
            c,
            "snap on the opening backtick must land on `c`, got {}",
            ch_at(source, engine.snap_caret(tick, Bias::Right))
        );
        let close = source.rfind('`').expect("close tick");
        assert_eq!(
            engine.snap_caret(close, Bias::Left),
            c + "code".len(),
            "snap on the closing backtick must land at the end of `code`"
        );
        assert_ne!(
            ch_at(source, engine.snap_caret(tick, Bias::Right)),
            '`',
            "backticks are not caret homes"
        );
    }

    /// CommonMark strips one leading and trailing space from a code span
    /// when both ends are a space. Those spaces are dest chrome like ticks.
    #[test]
    fn snap_caret_skips_code_span_stripped_padding_spaces() {
        for (source, needle) in [
            ("` foo `\n", "foo"),
            ("`` foo`bar ``\n", "foo"),
            ("> ` foo `\n", "foo"),
            ("- ` foo `\n", "foo"),
        ] {
            let (_doc, engine) = engine_for(source);
            let body = source.find(needle).expect(needle);
            let home = click_home(&engine, source, 0);
            assert_eq!(
                home,
                body,
                "Home must skip ticks and stripped spaces onto {needle:?}, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
            assert_ne!(
                ch_at(source, home),
                ' ',
                "Home must not sit on stripped padding, {source:?}"
            );
            assert_ne!(
                ch_at(source, home),
                '`',
                "Home must not sit on a tick, {source:?}"
            );
            let click = click_at(&engine, source, body);
            assert_eq!(
                click,
                body,
                "click on painted body must stay on {needle:?}, {source:?} got {} {:?}",
                click,
                ch_at(source, click)
            );
            let prev = engine.prev_caret(source, body);
            assert_ne!(
                ch_at(source, prev),
                ' ',
                "Left at body start must skip padding space, {source:?}"
            );
            assert_ne!(
                ch_at(source, prev),
                '`',
                "Left at body start must skip ticks, {source:?}"
            );
            let inner_end = body + needle.len();
            let next = engine.next_caret(source, inner_end);
            assert_ne!(
                ch_at(source, next),
                '`',
                "Right from the insert home after {needle:?} must skip ticks, {source:?} got {} {:?}",
                next,
                ch_at(source, next)
            );
        }

        let keep = "` foo`\n";
        let (_doc, engine) = engine_for(keep);
        let space = keep.find(' ').expect("content space");
        let home = click_home(&engine, keep, 0);
        assert_eq!(
            home, space,
            "a single leading space is content, not dest chrome"
        );

        let doubled = "``  foo`bar  ``\n";
        let (_doc, engine) = engine_for(doubled);
        let kept = doubled.find(" foo`bar ").expect("kept inner spaces");
        let home = click_home(&engine, doubled, 0);
        assert_eq!(
            home,
            kept,
            "one space remains as content after stripping one from each end, got {} {:?}",
            home,
            ch_at(doubled, home)
        );

        let in_sentence = "see ` foo ` now\n";
        let (_doc, engine) = engine_for(in_sentence);
        let foo = in_sentence.find("foo").expect("foo");
        let tick = in_sentence.find('`').expect("tick");
        assert_eq!(
            click_home(&engine, in_sentence, tick),
            foo,
            "click/Home on ticks must skip padding onto `f`"
        );
        let prev = engine.prev_caret(in_sentence, foo);
        assert_ne!(ch_at(in_sentence, prev), '`');
        assert_eq!(
            ch_at(in_sentence, prev),
            ' ',
            "Left from foo must skip padding and ticks onto the space after see"
        );
    }

    #[test]
    fn next_caret_skips_markdown_link_dest() {
        let source = "see [label](https://e.com) now\n";
        let (_doc, engine) = engine_for(source);
        let end_label = source.find("label").unwrap() + "label".len();
        let now = source.find("now").expect("now");
        let next = engine.next_caret(source, end_label);
        assert_ne!(ch_at(source, next), ']');
        assert_ne!(ch_at(source, next), '(');
        assert!(
            next <= now,
            "Right at the end of a link label must skip `](url)`, got {} {:?}",
            next,
            ch_at(source, next)
        );
        let l = source.find('l').expect("l");
        let prev = engine.prev_caret(source, l);
        assert_ne!(ch_at(source, prev), '[', "Left at the label must skip `[`");
        assert_eq!(
            ch_at(source, prev),
            ' ',
            "Left from the label must land on the previous visible space, got {} {:?}",
            prev,
            ch_at(source, prev)
        );
    }

    /// Keyboard End on a last-in-line markdown link is the label insert home
    /// (`]`), not hidden dest `(url)` / `[ref]`. Dest inner is End only when
    /// the caret is already in dest. Trailing text after the dest is still
    /// the visual line end.
    #[test]
    fn line_end_on_last_in_line_link_stays_on_label_not_dest() {
        fn label_closer(source: &str, label: &str) -> usize {
            let start = source.find(label).unwrap_or_else(|| panic!("{label}"));
            start + label.len()
        }

        for (source, from) in [
            ("[label](https://e.com)\n", "label"),
            ("[label](https://e.com \"title\")\n", "label"),
            ("[label](https://e.com)\r\n", "label"),
            ("# [label](https://e.com)\n", "label"),
            ("> [label](https://e.com)\n", "label"),
            ("- [label](https://e.com)\n", "label"),
            ("[label][ref]\n\n[ref]: https://e.com\n", "label"),
            ("[foo][]\n\n[foo]: https://e.com\n", "foo"),
            ("[label]()\n", "label"),
        ] {
            let (_doc, engine) = engine_for(source);
            let cursor = source.find(from).unwrap_or_else(|| panic!("{from}"));
            let landed = engine.line_end_caret(source, cursor);
            assert_eq!(
                landed,
                label_closer(source, from),
                "End from {from:?} must sit on `]`, {source:?} got {landed} {:?}",
                ch_at(source, landed)
            );
            assert_eq!(
                ch_at(source, landed),
                ']',
                "End from {from:?} sat on dest, {source:?} at {landed}"
            );
        }

        let source = "[label](https://e.com \"title\")\n";
        let (_doc, engine) = engine_for(source);
        let url = source.find("https").expect("url");
        let landed = engine.line_end_caret(source, url);
        assert_eq!(
            ch_at(source, landed),
            '"',
            "End from dest must sit on dest inner end, got {landed} {:?}",
            ch_at(source, landed)
        );

        let source = "[label](https://e.com) now\n";
        let (_doc, engine) = engine_for(source);
        let cursor = source.find("label").expect("label");
        let landed = engine.line_end_caret(source, cursor);
        assert_eq!(
            ch_at(source, landed),
            '\n',
            "End with trailing text must sit after now, got {landed} {:?}",
            ch_at(source, landed)
        );

        let source = "hello\n\n[label](https://e.com)\n";
        let (_doc, engine) = engine_for(source);
        let cursor = source.find("hello").expect("hello");
        let landed = engine.document_end_caret(source, cursor);
        assert_eq!(
            ch_at(source, landed),
            ']',
            "DocumentEnd onto a last-block link must sit on `]`, got {landed} {:?}",
            ch_at(source, landed)
        );

        for (source, from) in [
            ("**bold**\n", "bold"),
            ("`code`\n", "code"),
            ("~~strike~~\n", "strike"),
            ("<https://e.com>\n", "https"),
        ] {
            let (_doc, engine) = engine_for(source);
            let cursor = source.find(from).unwrap_or_else(|| panic!("{from}"));
            let landed = engine.line_end_caret(source, cursor);
            let ch = ch_at(source, landed);
            assert_ne!(
                ch, '(',
                "wrap/autolink End must not jump to a dest paren, {source:?}"
            );
            assert!(
                matches!(ch, '*' | '`' | '~' | '>'),
                "wrap/autolink End must sit on inner.end, {source:?} got {landed} {ch:?}"
            );
        }
    }

    /// Keyboard End on a last-in-line HTML `<a href>label</a>` is the label
    /// insert home (before `</a>`), not hidden dest `href` and not the last
    /// inner letter. Same contract as markdown `[label](url)` staying on `]`.
    #[test]
    fn line_end_on_last_in_line_html_anchor_stays_on_label_not_href() {
        fn label_end(source: &str, label: &str) -> usize {
            let start = source.find(label).unwrap_or_else(|| panic!("{label}"));
            start + label.len()
        }

        for (source, from, closer) in [
            ("<a href=\"https://e.com\">label</a>\n", "label", "</a>"),
            (
                "<a href=\"https://e.com\" title=\"t\">label</a>\n",
                "label",
                "</a>",
            ),
            ("<a href=https://e.com>label</a>\n", "label", "</a>"),
            ("<a href=\"https://e.com\">label</a>\r\n", "label", "</a>"),
            ("# <a href=\"https://e.com\">label</a>\n", "label", "</a>"),
            ("> <a href=\"https://e.com\">label</a>\n", "label", "</a>"),
            ("- <a href=\"https://e.com\">label</a>\n", "label", "</a>"),
            ("*<a href=\"https://e.com\">label</a>*\n", "label", "</a>"),
            ("<b>bold</b>\n", "bold", "</b>"),
            ("> <em>hello</em>\n", "hello", "</em>"),
            ("hello<!-- x -->\n", "hello", "<!--"),
            ("hello<!-- x -->\r\n", "hello", "<!--"),
            ("> hello<!-- x -->\n", "hello", "<!--"),
            ("hello<?php echo 1; ?>\n", "hello", "<?"),
            ("hello<![CDATA[a]]>\n", "hello", "<!["),
        ] {
            let (_doc, engine) = engine_for(source);
            let cursor = source.find(from).unwrap_or_else(|| panic!("{from}"));
            let landed = engine.line_end_caret(source, cursor);
            let home = label_end(source, from);
            assert_eq!(
                landed,
                home,
                "End from {from:?} must sit before {closer:?}, {source:?} got {landed} {:?}",
                ch_at(source, landed)
            );
            assert!(
                source[landed..].starts_with(closer),
                "End must be the {closer:?} insert home, {source:?} at {landed} {:?}",
                ch_at(source, landed)
            );
            assert_ne!(
                ch_at(source, landed),
                'h',
                "End from {from:?} must not jump into href, {source:?} at {landed}"
            );
        }

        let source = "see <a href=\"https://e.com\">label</a> now\n";
        let (_doc, engine) = engine_for(source);
        let cursor = source.find("label").expect("label");
        let landed = engine.line_end_caret(source, cursor);
        assert_eq!(
            ch_at(source, landed),
            '\n',
            "End with trailing text must sit after now, got {landed} {:?}",
            ch_at(source, landed)
        );

        let source = "hello\n\n<a href=\"https://e.com\">label</a>\n";
        let (_doc, engine) = engine_for(source);
        let cursor = source.find("hello").expect("hello");
        let landed = engine.document_end_caret(source, cursor);
        assert!(
            source[landed..].starts_with("</a>"),
            "DocumentEnd onto a last-block HTML anchor must sit before `</a>`, got {landed} {:?}",
            ch_at(source, landed)
        );
    }

    /// Keyboard End on last-in-line `&amp;` / `\*` is the insert home after
    /// the painted glyph, not hidden `amp;` / `*` dest chrome. Code stays
    /// literal.
    #[test]
    fn line_end_on_last_in_line_entity_stays_after_glyph_not_dest() {
        fn after_literal(source: &str, literal: &str) -> usize {
            let start = source.find(literal).unwrap_or_else(|| panic!("{literal}"));
            start + literal.len()
        }

        for (source, from, literal) in [
            ("A&amp;\n", "A", "&amp;"),
            ("A&amp;", "A", "&amp;"),
            ("A&amp;\r\n", "A", "&amp;"),
            ("hello &amp;\n", "hello", "&amp;"),
            ("A&lt;\n", "A", "&lt;"),
            ("A&#123;\n", "A", "&#123;"),
            ("A&#x7B;\n", "A", "&#x7B;"),
            ("A&#38;\n", "A", "&#38;"),
            ("A&#x26;\n", "A", "&#x26;"),
            ("> A&amp;\n", "A", "&amp;"),
            ("- A&amp;\n", "A", "&amp;"),
            ("# A&amp;\n", "A", "&amp;"),
            ("A\\\\\n", "A", "\\\\"),
            ("A\\*\n", "A", "\\*"),
        ] {
            let (_doc, engine) = engine_for(source);
            let cursor = source.find(from).unwrap_or_else(|| panic!("{from}"));
            let home = after_literal(source, literal);
            let landed = engine.line_end_caret(source, cursor);
            assert_eq!(
                landed,
                home,
                "End from {from:?} must sit after {literal}, {source:?} got {landed} {:?}",
                ch_at(source, landed)
            );
            if let Some(rest) = source.get(home.saturating_sub(literal.len())..home) {
                let dest = rest.trim_start_matches(['&', '\\']);
                if let Some(b) = dest.bytes().next() {
                    assert_ne!(
                        landed,
                        home - dest.len(),
                        "End must not sit on dest-chrome {b:?} of {literal}, {source:?}"
                    );
                }
            }
            for (i, b) in literal.bytes().enumerate().skip(1) {
                assert_ne!(
                    landed,
                    source.find(literal).unwrap() + i,
                    "End must not sit on hidden {b:?} in {literal}, {source:?}"
                );
            }
        }

        let source = "A&amp; now\n";
        let (_doc, engine) = engine_for(source);
        let cursor = source.find('A').expect("A");
        let landed = engine.line_end_caret(source, cursor);
        assert_eq!(
            ch_at(source, landed),
            '\n',
            "End with trailing text must sit after now, got {landed} {:?}",
            ch_at(source, landed)
        );

        let source = "hello\n\nA&amp;\n";
        let (_doc, engine) = engine_for(source);
        let cursor = source.find("hello").expect("hello");
        let landed = engine.document_end_caret(source, cursor);
        assert_eq!(
            landed,
            after_literal(source, "&amp;"),
            "DocumentEnd onto a last-block entity must sit after the glyph, got {landed} {:?}",
            ch_at(source, landed)
        );

        let source = "[A&amp;](https://e.com)\n";
        let (_doc, engine) = engine_for(source);
        let cursor = source.find('A').expect("A");
        let landed = engine.line_end_caret(source, cursor);
        assert_eq!(
            ch_at(source, landed),
            ']',
            "End on a last-in-line entity label must sit on `]`, got {landed} {:?}",
            ch_at(source, landed)
        );

        let source = "| A&amp; | x |\n| --- | --- |\n";
        let (_doc, engine) = engine_for(source);
        let cursor = source.find('A').expect("A");
        let cell = engine.cell_edit_range(cursor, source).expect("cell");
        let landed = engine.line_end_caret(source, cursor);
        assert!(
            landed >= cell.start && landed <= cell.end,
            "End in a last-in-cell entity must stay in that cell, got {landed} {:?}",
            ch_at(source, landed)
        );
        assert_eq!(
            landed,
            after_literal(source, "&amp;"),
            "End in a last-in-cell entity must sit after the glyph, got {landed} {:?}",
            ch_at(source, landed)
        );

        let code = "`A&amp;`\n";
        let (_doc, engine) = engine_for(code);
        let cursor = code.find('A').expect("A");
        let landed = engine.line_end_caret(code, cursor);
        assert_eq!(
            ch_at(code, landed),
            '`',
            "code-span End must sit on the closing tick, got {landed} {:?}",
            ch_at(code, landed)
        );
        let amp = code.find("&amp;").expect("literal");
        let next = engine.next_caret(code, amp);
        assert_eq!(
            next,
            amp + 1,
            "code spans must walk `&amp;` as literals, got {} {:?}",
            next,
            ch_at(code, next)
        );
    }

    /// Keyboard End on last-in-line `[^1]` is after the widget, not the
    /// label start. Closed-suffix clamp used to sit on `1` / `m` so typing
    /// was `Hello[^x1]`.
    #[test]
    fn line_end_on_last_in_line_footnote_stays_after_widget() {
        fn after_ref(source: &str, raw: &str) -> usize {
            let start = source.find(raw).unwrap_or_else(|| panic!("{raw}"));
            start + raw.len()
        }

        for (source, from, raw) in [
            ("Hello[^1]\n\n[^1]: note\n", "Hello", "[^1]"),
            ("Hello[^1]\n", "Hello", "[^1]"),
            ("Hello[^note]\n", "Hello", "[^note]"),
            ("Hello[^1]\r\n\n[^1]: note\n", "Hello", "[^1]"),
            ("# Hello[^1]\n\n[^1]: note\n", "Hello", "[^1]"),
            ("> Hello[^1]\n\n[^1]: note\n", "Hello", "[^1]"),
            ("- Hello[^1]\n\n[^1]: note\n", "Hello", "[^1]"),
        ] {
            let (_doc, engine) = engine_for(source);
            let cursor = source.find(from).unwrap_or_else(|| panic!("{from}"));
            let home = after_ref(source, raw);
            let landed = engine.line_end_caret(source, cursor);
            assert_eq!(
                landed,
                home,
                "End from {from:?} must sit after {raw}, {source:?} got {landed} {:?}",
                ch_at(source, landed)
            );
        }

        let source = "Hello[^1] now\n\n[^1]: note\n";
        let (_doc, engine) = engine_for(source);
        let cursor = source.find("Hello").expect("Hello");
        let landed = engine.line_end_caret(source, cursor);
        assert_eq!(
            ch_at(source, landed),
            '\n',
            "End with trailing text must sit after now, got {landed} {:?}",
            ch_at(source, landed)
        );

        let source = "hello\n\nSee[^1]\n";
        let (_doc, engine) = engine_for(source);
        let cursor = source.find("hello").expect("hello");
        let landed = engine.document_end_caret(source, cursor);
        assert_eq!(
            landed,
            after_ref(source, "[^1]"),
            "DocumentEnd onto a last-block footnote must sit after `]`, got {landed} {:?}",
            ch_at(source, landed)
        );

        let source = "| Hello[^1] | x |\n| --- | --- |\n\n[^1]: note\n";
        let (_doc, engine) = engine_for(source);
        let cursor = source.find("Hello").expect("Hello");
        let cell = engine.cell_edit_range(cursor, source).expect("cell");
        let landed = engine.line_end_caret(source, cursor);
        assert!(
            landed >= cell.start && landed <= cell.end,
            "End in a last-in-cell footnote must stay in that cell, got {landed} {:?}",
            ch_at(source, landed)
        );
        assert_eq!(
            landed,
            after_ref(source, "[^1]"),
            "End in a last-in-cell footnote must sit after `]`, got {landed} {:?}",
            ch_at(source, landed)
        );
    }

    /// Keyboard End on last-in-line `[![alt](img)](url)` stays on wrapping
    /// `]`, not hidden dest. Standalone `![alt](url)` stays atomic (End
    /// after the widget is left alone).
    #[test]
    fn line_end_on_last_in_line_linked_image_stays_on_wrapping_closer() {
        fn wrapping_closer(source: &str) -> usize {
            let img_dest = source.find("a.png)").expect("image dest") + "a.png)".len();
            assert_eq!(
                source.as_bytes().get(img_dest).copied(),
                Some(b']'),
                "wrapping closer must follow the image dest, {source:?}"
            );
            img_dest
        }

        for source in [
            "[![alt](a.png)](https://e.com)\n",
            "[![alt](a.png)](https://e.com \"title\")\n",
            "[![alt](a.png)](https://e.com)\r\n",
            "# [![alt](a.png)](https://e.com)\n",
            "> [![alt](a.png)](https://e.com)\n",
            "- [![alt](a.png)](https://e.com)\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let cursor = source.find("alt").expect("alt");
            let landed = engine.line_end_caret(source, cursor);
            assert_eq!(
                landed,
                wrapping_closer(source),
                "End from alt must sit on wrapping `]`, {source:?} got {landed} {:?}",
                ch_at(source, landed)
            );
            assert_eq!(
                ch_at(source, landed),
                ']',
                "End from alt sat in dest, {source:?} at {landed}"
            );
        }

        let source = "[![alt](a.png)](https://e.com) now\n";
        let (_doc, engine) = engine_for(source);
        let cursor = source.find("alt").expect("alt");
        let landed = engine.line_end_caret(source, cursor);
        assert_eq!(
            ch_at(source, landed),
            '\n',
            "End with trailing text must sit after now, got {landed} {:?}",
            ch_at(source, landed)
        );

        let source = "hello\n\n[![alt](a.png)](https://e.com)\n";
        let (_doc, engine) = engine_for(source);
        let cursor = source.find("hello").expect("hello");
        let landed = engine.document_end_caret(source, cursor);
        assert_eq!(
            ch_at(source, landed),
            ']',
            "DocumentEnd onto a last-block linked image must sit on wrapping `]`, got {landed} {:?}",
            ch_at(source, landed)
        );

        let source = "| [![alt](a.png)](https://e.com) | world |\n|---|---|\n| 1 | 2 |\n";
        let (_doc, engine) = engine_for(source);
        let cursor = source.find("alt").expect("alt");
        let cell = engine.cell_edit_range(cursor, source).expect("cell");
        let landed = engine.line_end_caret(source, cursor);
        assert!(
            landed >= cell.start && landed <= cell.end,
            "End in a last-in-cell linked image must stay in that cell, got {landed} {:?}",
            ch_at(source, landed)
        );
        assert_eq!(
            ch_at(source, landed),
            ']',
            "End in a last-in-cell linked image must sit on wrapping `]`, got {landed}"
        );

        let source = "![alt](a.png)\n";
        let (_doc, engine) = engine_for(source);
        let cursor = source.find("alt").expect("alt");
        let landed = engine.line_end_caret(source, cursor);
        assert_ne!(
            ch_at(source, landed),
            ']',
            "standalone image End must not pull back onto alt `]`, got {landed}"
        );
        assert!(
            landed >= source.find("a.png").expect("url"),
            "standalone image End stays after/in the atomic dest, got {landed} {:?}",
            ch_at(source, landed)
        );
    }

    /// Keyboard Home/End in a GFM table stay in the current cell (Typora).
    /// Source-line End from `hello` in `| hello | world |` used to jump to
    /// `world`; Home from `world` jumped to `hello`.
    #[test]
    fn line_home_end_in_table_stay_in_the_current_cell() {
        for source in [
            "| hello | world |\n|---|---|\n| 1 | 2 |\n",
            "> | hello | world |\n> |---|---|\n> | 1 | 2 |\n",
            "hello|world\n---|---\n1|2\n",
            "| [label](https://e.com) | world |\n|---|---|\n| 1 | 2 |\n",
            "| <a href=\"https://e.com\">label</a> | world |\n|---|---|\n| 1 | 2 |\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let hello = source
                .find("hello")
                .or_else(|| source.find("label"))
                .expect("first cell");
            let world = source.find("world").expect("world");
            let first = engine.cell_edit_range(hello, source).expect("cell 1");
            let second = engine.cell_edit_range(world, source).expect("cell 2");
            assert!(
                first.end <= second.start,
                "cells must be ordered, {source:?} {first:?} {second:?}"
            );

            let end = engine.line_end_caret(source, hello);
            assert!(
                end >= first.start && end <= first.end,
                "End from the first cell must stay in that cell, {source:?} got {end} {:?}",
                ch_at(source, end)
            );
            assert!(
                !second.contains(&end),
                "End must not jump to the next cell, {source:?} got {end} {:?}",
                ch_at(source, end)
            );
            if source.contains("[label]") {
                assert_eq!(
                    ch_at(source, end),
                    ']',
                    "End in a last-in-cell link must sit on `]`, {source:?} at {end}"
                );
            }
            if source.contains("<a href") {
                assert!(
                    source[end..].starts_with("</a>"),
                    "End in a last-in-cell HTML anchor must sit before `</a>`, {source:?} at {end}"
                );
            }

            let home = engine.line_start_caret(source, world);
            assert!(
                home >= second.start && home <= second.end,
                "Home from the second cell must stay in that cell, {source:?} got {home} {:?}",
                ch_at(source, home)
            );
            assert!(
                !first.contains(&home),
                "Home must not jump to the previous cell, {source:?} got {home} {:?}",
                ch_at(source, home)
            );
            assert_eq!(
                ch_at(source, home),
                'w',
                "Home in the second cell must sit on `w`, {source:?} got {home} {:?}",
                ch_at(source, home)
            );
        }

        let source = "| hello | world |\n|---|---|\n| 1 | 2 |\n";
        let (_doc, engine) = engine_for(source);
        let hello = source.find("hello").expect("hello");
        let landed = engine.document_end_caret(source, hello);
        assert!(
            landed >= source.find('2').expect("2"),
            "DocumentEnd from a table cell must leave the row, got {landed} {:?}",
            ch_at(source, landed)
        );
    }

    /// GFM `[label](url "title")` dest wrapping is chrome: click/Home on `(`
    /// / `"` / `'` / `(title)` land on the URL or title inner, not wrapping.
    #[test]
    fn click_home_on_titled_link_dest_lands_on_url_or_title_inner() {
        for source in [
            "see [label](https://e.com \"title\") now\n",
            "see [label](https://e.com 'title') now\n",
            "see [label](https://e.com (title)) now\n",
            "see [label](<https://e.com> \"title\") now\n",
            "> [label](https://e.com \"title\")\n",
            "- [label](https://e.com \"title\")\n",
            "see ![alt](a.png \"title\") now\n",
            "> ![alt](a.png \"title\")\n",
            "- ![alt](a.png \"title\")\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let url = source
                .find("https://e.com")
                .or_else(|| source.find("a.png"))
                .expect("url");
            let title = source.find("title").expect("title");
            let dest_open = source[..url].rfind('(').expect("dest (");
            assert_eq!(ch_at(source, dest_open), '(', "dest opener in {source:?}");
            for landed in [
                click_home(&engine, source, dest_open),
                click_at(&engine, source, dest_open),
            ] {
                assert_eq!(
                    landed,
                    url,
                    "click/Home on dest `(` must land on the URL, {source:?} got {} {:?}",
                    landed,
                    ch_at(source, landed)
                );
                assert_ne!(ch_at(source, landed), '(');
                assert_ne!(ch_at(source, landed), '"');
                assert_ne!(ch_at(source, landed), '\'');
            }
            let quote = source[..title]
                .rfind(['"', '\'', '('])
                .expect("title opener");
            let quote_ch = ch_at(source, quote);
            assert!(
                matches!(quote_ch, '"' | '\'' | '('),
                "title opener in {source:?} got {quote_ch:?}"
            );
            for landed in [
                click_home(&engine, source, quote),
                click_at(&engine, source, quote),
            ] {
                assert_eq!(
                    landed, title,
                    "click/Home on title {quote_ch:?} must land on title inner, {source:?} got {} {:?}",
                    landed,
                    ch_at(source, landed)
                );
                assert_ne!(ch_at(source, landed), '(');
                assert_ne!(ch_at(source, landed), '"');
                assert_ne!(ch_at(source, landed), '\'');
            }
            let closer = source[title + "title".len()..]
                .find(')')
                .map(|rel| title + "title".len() + rel)
                .expect("dest )");
            for landed in [
                click_home(&engine, source, closer),
                click_at(&engine, source, closer),
            ] {
                assert_ne!(
                    ch_at(source, landed),
                    ')',
                    "click/Home on dest `)` must not sit on wrapping, {source:?} got {} {:?}",
                    landed,
                    ch_at(source, landed)
                );
                assert!(
                    landed == url || landed == title || landed == title + "title".len(),
                    "click/Home on `)` must land on dest inner, {source:?} got {} {:?}",
                    landed,
                    ch_at(source, landed)
                );
            }
        }
    }

    /// Wrap marks around a whole link (`**[hello](url)**`) are dest chrome,
    /// same as `[**hello**](url)` marks inside the label.
    #[test]
    fn prev_caret_skips_emphasis_wrapping_a_link() {
        for source in [
            "see **[hello](https://e.com)** now\n",
            "see *[hello](https://e.com)* now\n",
            "see __[hello](https://e.com)__ now\n",
            "see _[hello](https://e.com)_ now\n",
            "see ~~[hello](https://e.com)~~ now\n",
            "see **[hello][ref]** now\n\n[ref]: https://e.com\n",
            "see **<https://e.com>** now\n",
            "> **[hello](https://e.com)**\n",
            "- **[hello](https://e.com)**\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let hello = source
                .find("hello")
                .or_else(|| source.find("https://e.com"))
                .expect("inner");
            let prev = engine.prev_caret(source, hello);
            assert_ne!(
                ch_at(source, prev),
                '*',
                "Left at a wrapped-link label must skip wrapping `*`, {source:?} got {} {:?}",
                prev,
                ch_at(source, prev)
            );
            assert_ne!(ch_at(source, prev), '_');
            assert_ne!(ch_at(source, prev), '~');
            assert_ne!(ch_at(source, prev), '[');
            assert_ne!(ch_at(source, prev), '<');
            if source.starts_with('>') {
                assert_ne!(ch_at(source, prev), '>');
            }
            if source.starts_with('-') {
                assert_ne!(ch_at(source, prev), '-');
            }
            let open = source.find("**[").or_else(|| {
                source
                    .find("*[")
                    .or_else(|| source.find("__[").or_else(|| source.find("_[")))
                    .or_else(|| source.find("~~["))
                    .or_else(|| source.find("**<"))
            });
            if let Some(open) = open {
                let home = click_home(&engine, source, open);
                assert_eq!(
                    home,
                    hello,
                    "click/Home on wrapping marks must skip onto the label, {source:?} got {} {:?}",
                    home,
                    ch_at(source, home)
                );
            }
            let inner_len = if source.contains("hello") {
                "hello".len()
            } else {
                "https://e.com".len()
            };
            if source.contains(" now") {
                let next = engine.next_caret(source, hello + inner_len);
                assert_ne!(
                    ch_at(source, next),
                    ']',
                    "Right at the label must skip dest, {source:?} got {} {:?}",
                    next,
                    ch_at(source, next)
                );
                assert_ne!(ch_at(source, next), ')');
                assert_ne!(
                    ch_at(source, next),
                    '*',
                    "Right must skip wrapping `*`, {source:?} got {} {:?}",
                    next,
                    ch_at(source, next)
                );
                assert_ne!(ch_at(source, next), '_');
                assert_ne!(ch_at(source, next), '~');
                assert_ne!(ch_at(source, next), '>');
            }
        }

        let inside = "[**hello**](https://e.com)\n";
        let (_doc, engine) = engine_for(inside);
        let h = inside.find("hello").expect("hello");
        let prev = engine.prev_caret(inside, h);
        assert_ne!(ch_at(inside, prev), '*');
        assert_ne!(ch_at(inside, prev), '[');
    }

    /// Siblings of wrap-around-link chrome: marks around an image, a linked
    /// image, a reference label, HTML phrasing, and nested `***[…](url)***`.
    #[test]
    fn prev_caret_skips_emphasis_wrapping_link_siblings() {
        for source in [
            "see **![alt](a.png)** now\n",
            "see *[![cat](a.png)](https://e.com)* now\n",
            "see ~~[hello][ref]~~ now\n\n[ref]: https://e.com\n",
            "see **<b>hello</b>** now\n",
            "see *<a href=\"https://e.com\">hello</a>* now\n",
            "see ***[hello](https://e.com)*** now\n",
            "see **<https://e.com>** now\n",
            "see [<b>hello</b>](https://e.com) now\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let inner = if source.contains("![") {
                first_image_range(&engine).start
            } else {
                source
                    .find("hello")
                    .or_else(|| source.find("https://e.com"))
                    .expect("inner")
            };
            let prev = engine.prev_caret(source, inner);
            assert_ne!(
                ch_at(source, prev),
                '*',
                "Left must skip wrapping `*`, {source:?} got {} {:?}",
                prev,
                ch_at(source, prev)
            );
            assert_ne!(ch_at(source, prev), '~');
            assert_ne!(ch_at(source, prev), '[');
            assert_ne!(ch_at(source, prev), '<');
            assert_ne!(ch_at(source, prev), '>');
            assert_ne!(ch_at(source, prev), '!');
            if source.contains(" now") {
                assert_eq!(
                    ch_at(source, prev),
                    ' ',
                    "Left must land on the previous visible space, {source:?} got {} {:?}",
                    prev,
                    ch_at(source, prev)
                );
            }
            let open = source.find("**![").or_else(|| {
                source.find("*[![").or_else(|| {
                    source.find("~~[").or_else(|| {
                        source.find("**<b>").or_else(|| {
                            source.find("*<a ").or_else(|| {
                                source.find("***[").or_else(|| {
                                    source
                                        .find("**<https://e.com>")
                                        .or_else(|| source.find("[<b>"))
                                })
                            })
                        })
                    })
                })
            });
            if let Some(open) = open {
                let home = click_home(&engine, source, open);
                assert_eq!(
                    home,
                    inner,
                    "click/Home on wrapping marks must skip onto the inner, {source:?} got {} {:?}",
                    home,
                    ch_at(source, home)
                );
            }
        }
    }

    #[test]
    fn prev_caret_skips_more_gfm_wrap_around_chrome() {
        for source in [
            "see **`code`** now\n",
            "see **[foo][]** now\n\n[foo]: https://e.com\n",
            "see **<img src=\"a.png\">** now\n",
            "see ~~![alt](a.png)~~ now\n",
            "see **[hello](https://e.com \"title\")** now\n",
            "see **<user@example.com>** now\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let inner = if source.contains("![") || source.contains("<img") {
                first_image_range(&engine).start
            } else {
                source
                    .find("code")
                    .or_else(|| source.find("foo"))
                    .or_else(|| source.find("hello"))
                    .or_else(|| source.find("user@example.com"))
                    .expect("inner")
            };
            let prev = engine.prev_caret(source, inner);
            assert_ne!(
                ch_at(source, prev),
                '*',
                "Left must skip wrapping `*`, {source:?} got {} {:?}",
                prev,
                ch_at(source, prev)
            );
            assert_ne!(ch_at(source, prev), '~');
            assert_ne!(ch_at(source, prev), '[');
            assert_ne!(ch_at(source, prev), '<');
            assert_ne!(ch_at(source, prev), '`');
            if source.contains(" now") {
                assert_eq!(
                    ch_at(source, prev),
                    ' ',
                    "Left must land on the previous visible space, {source:?} got {} {:?}",
                    prev,
                    ch_at(source, prev)
                );
            }
        }
    }

    #[test]
    fn prev_caret_skips_email_autolink_brackets() {
        let source = "see <user@example.com> now\n";
        let (_doc, engine) = engine_for(source);
        let u = source.find("user").expect("user");
        let prev = engine.prev_caret(source, u);
        assert_ne!(
            ch_at(source, prev),
            '<',
            "Left at an email autolink must skip `<`"
        );
        assert_eq!(
            ch_at(source, prev),
            ' ',
            "Left from the email must land on the previous visible space, got {} {:?}",
            prev,
            ch_at(source, prev)
        );
        let end = u + "user@example.com".len();
        let next = engine.next_caret(source, end);
        assert_ne!(ch_at(source, next), '>');
    }

    #[test]
    fn prev_caret_skips_url_autolink_brackets() {
        let source = "see <https://example.com> now\n";
        let (_doc, engine) = engine_for(source);
        let h = source.find("https").expect("https");
        let prev = engine.prev_caret(source, h);
        assert_ne!(
            ch_at(source, prev),
            '<',
            "Left at a URL autolink must skip `<`"
        );
        assert_eq!(
            ch_at(source, prev),
            ' ',
            "Left from the URL must land on the previous visible space, got {} {:?}",
            prev,
            ch_at(source, prev)
        );
        let end = h + "https://example.com".len();
        let next = engine.next_caret(source, end);
        assert_ne!(
            ch_at(source, next),
            '>',
            "Right at the end of a URL autolink must skip `>`"
        );
        let open = source.find('<').expect("<");
        let click =
            engine.clamp_raw_prefix(source, engine.snap_caret(open, Bias::Right), Bias::Right);
        assert_eq!(
            click,
            h,
            "click/Home on `<` must skip onto `h`, got {} {:?}",
            click,
            ch_at(source, click)
        );
    }

    fn click_home(engine: &RichEngine, source: &str, at: usize) -> usize {
        engine.clamp_raw_prefix(source, engine.snap_caret(at, Bias::Right), Bias::Right)
    }

    /// WYSIWYG mouse-down uses Bias::Left (`move_to`).
    fn click_at(engine: &RichEngine, source: &str, at: usize) -> usize {
        engine.clamp_raw_prefix(source, engine.snap_caret(at, Bias::Left), Bias::Left)
    }

    /// `$math$` / `[[wiki]]` / `:emoji:` skip dest chrome like autolink `<>`.
    #[test]
    fn prev_caret_skips_math_wiki_emoji_dest_chrome() {
        let math = "see $x^2$ here\n";
        let (_doc, engine) = engine_for(math);
        let x = math.find('x').expect("x");
        let prev = engine.prev_caret(math, x);
        assert_ne!(ch_at(math, prev), '$', "Left at math must skip `$`");
        assert_eq!(ch_at(math, prev), ' ', "Left from math lands on the space");
        let inner_end = math.find('2').expect("2") + 1;
        let next = engine.next_caret(math, inner_end);
        assert_ne!(ch_at(math, next), '$', "Right at math end must skip `$`");
        let open = math.find('$').expect("$");
        assert_eq!(
            click_home(&engine, math, open),
            x,
            "click/Home on `$` must skip onto `x`"
        );

        let display = "see $$x^2$$ here\n";
        let (_doc, engine) = engine_for(display);
        let dx = display.find('x').expect("x");
        assert_eq!(
            click_home(&engine, display, display.find("$$").expect("$$")),
            dx,
            "click/Home on `$$` must skip onto `x`"
        );
        let dprev = engine.prev_caret(display, dx);
        assert_ne!(ch_at(display, dprev), '$');

        let wiki = "see [[page]] here\n";
        let (_doc, engine) = engine_for(wiki);
        let p = wiki.find("page").expect("page");
        let prev = engine.prev_caret(wiki, p);
        assert_ne!(ch_at(wiki, prev), '[', "Left at wiki must skip `[`");
        assert_eq!(ch_at(wiki, prev), ' ');
        let label_end = p + "page".len();
        let next = engine.next_caret(wiki, label_end);
        assert_ne!(ch_at(wiki, next), ']', "Right at wiki end must skip `]`");
        assert_eq!(
            click_home(&engine, wiki, wiki.find("[[").expect("[[")),
            p,
            "click/Home on `[[` must skip onto `p`"
        );

        let piped = "see [[page|Label]] here\n";
        let (_doc, engine) = engine_for(piped);
        let l = piped.find("Label").expect("Label");
        assert_eq!(
            click_home(&engine, piped, piped.find("[[").expect("[[")),
            l,
            "piped wiki Home must skip `[[page|` onto `L`"
        );
        let prev = engine.prev_caret(piped, l);
        assert_ne!(ch_at(piped, prev), '[');
        assert_ne!(ch_at(piped, prev), '|');

        let emoji = "see :smile: here\n";
        let (_doc, engine) = engine_for(emoji);
        let s = emoji.find("smile").expect("smile");
        let prev = engine.prev_caret(emoji, s);
        assert_ne!(ch_at(emoji, prev), ':', "Left at emoji must skip `:`");
        assert_eq!(ch_at(emoji, prev), ' ');
        let name_end = s + "smile".len();
        let next = engine.next_caret(emoji, name_end);
        assert_ne!(ch_at(emoji, next), ':', "Right at emoji end must skip `:`");
        assert_eq!(
            click_home(&engine, emoji, emoji.find(":smile:").expect(":")),
            s,
            "click/Home on `:` must skip onto `s`"
        );

        for source in ["> $x^2$\n", "> [[page]]\n", "> :smile:\n"] {
            let (_doc, engine) = engine_for(source);
            let home = click_home(&engine, source, 0);
            let ch = ch_at(source, home);
            assert!(
                matches!(ch, 'x' | 'p' | 's'),
                "quoted Home must skip `>` and dest chrome onto the body, {source:?} got {home} {ch:?}"
            );
            assert_ne!(ch, '>');
            assert_ne!(ch, '$');
            assert_ne!(ch, '[');
            assert_ne!(ch, ':');
        }
    }

    /// Display `$$\nE=mc^2\n$$` wrapping newlines skip like `$` / `$$`.
    /// Click/Home on `$$` or the blank after the opener land on `E`; Left/Right
    /// do not sit on `\n` or `$`. Quoted / list forms match. Single-line
    /// `$$E=mc^2$$` is unchanged. Inline `$x^2$` is unchanged.
    #[test]
    fn multiline_display_math_skips_wrapping_newlines() {
        for source in [
            "$$\nE=mc^2\n$$\n",
            "see\n$$\nE=mc^2\n$$\nhere\n",
            "> $$\n> E=mc^2\n> $$\n",
            "- $$\n  E=mc^2\n  $$\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let e = source.find('E').expect("E");
            let open = source.find("$$").expect("$$");
            let home = click_home(&engine, source, open);
            assert_eq!(
                home,
                e,
                "click/Home on `$$` must skip wrapping newline onto `E`, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
            let nl = open + 2;
            if source.as_bytes().get(nl) == Some(&b'\n')
                || source.as_bytes().get(nl) == Some(&b'\r')
            {
                let on_nl = click_home(&engine, source, nl);
                assert_eq!(
                    on_nl,
                    e,
                    "click/Home on the wrapping newline must skip onto `E`, {source:?} got {} {:?}",
                    on_nl,
                    ch_at(source, on_nl)
                );
            }
            let prev = engine.prev_caret(source, e);
            assert_ne!(
                ch_at(source, prev),
                '$',
                "Left from `E` must skip `$$`, {source:?} got {prev}"
            );
            if source.as_bytes().get(open + 2) == Some(&b'\n') {
                assert_ne!(
                    prev,
                    open + 2,
                    "Left from `E` must skip the wrapping newline after `$$`, {source:?}"
                );
            }
            let after_formula = e + "E=mc^2".len();
            let next = engine.next_caret(source, after_formula);
            assert_ne!(
                ch_at(source, next),
                '$',
                "Right at formula end must skip wrapping newline and `$$`, {source:?} got {} {:?}",
                next,
                ch_at(source, next)
            );
        }

        let single = "see $$E=mc^2$$ here\n";
        let (_doc, engine) = engine_for(single);
        let e = single.find('E').expect("E");
        assert_eq!(
            click_home(&engine, single, single.find("$$").expect("$$")),
            e,
            "single-line display math must still skip `$$` onto `E`"
        );
        let inline = "see $x^2$ here\n";
        let (_doc, engine) = engine_for(inline);
        let x = inline.find('x').expect("x");
        assert_eq!(
            click_home(&engine, inline, inline.find('$').expect("$")),
            x,
            "inline math must still skip `$` onto `x`"
        );
    }

    #[test]
    fn prev_caret_skips_wrapping_dest_on_linked_image() {
        let source = "see [![cat](a.png)](https://e.com) now\n";
        let (_doc, engine) = engine_for(source);
        let img = first_image_range(&engine);
        let prev = engine.prev_caret(source, img.start);
        assert_ne!(
            ch_at(source, prev),
            '[',
            "Left at a linked image must skip wrapping `[`"
        );
        assert_eq!(
            ch_at(source, prev),
            ' ',
            "Left from a linked image must land on the previous visible space, got {} {:?}",
            prev,
            ch_at(source, prev)
        );
        let next = engine.next_caret(source, img.end);
        assert_ne!(ch_at(source, next), ']');
        assert_ne!(ch_at(source, next), '(');
    }

    #[test]
    fn word_caret_skips_bold_delimiters_and_punctuation() {
        let source = "**hello** world";
        let (_doc, engine) = engine_for(source);
        let h = source.find('h').expect("h");
        let w = source.find('w').expect("w");
        let end_hello = engine.next_word_caret(source, h);
        assert_eq!(
            end_hello,
            source.find(' ').expect("space"),
            "WordRight from `hello` lands on the visible space, not `*`"
        );
        assert_eq!(
            engine.next_word_caret(source, end_hello),
            w + "world".len(),
            "next WordRight is the end of `world`"
        );
        assert_eq!(
            engine.prev_word_caret(source, source.len()),
            w,
            "WordLeft from EOF is the start of `world`"
        );
        assert_eq!(
            engine.prev_word_caret(source, w),
            h,
            "WordLeft from `world` skips `**` onto `hello`"
        );

        let punct = "one, two";
        let (_doc, engine) = engine_for(punct);
        assert_eq!(engine.next_word_caret(punct, 0), 3, "end of `one`");
        assert_eq!(
            engine.next_word_caret(punct, 3),
            4,
            "comma is its own visible run"
        );
        assert_eq!(engine.next_word_caret(punct, 4), punct.len());
        assert_eq!(engine.prev_word_caret(punct, punct.len()), 5);
        assert_eq!(engine.prev_word_caret(punct, 5), 3);
        assert_eq!(engine.prev_word_caret(punct, 3), 0);
    }

    #[test]
    fn document_home_skips_frontmatter_yaml() {
        for source in [
            "---\ntitle: Hello\n---\n\n# Body\n",
            "---\ntitle: Hello\n...\n\n# Body\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let fm_end = frontmatter_body_start(engine.tree());
            let home =
                engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
            assert!(
                home >= fm_end,
                "Cmd-Up / document start must sit after YAML, {source:?} got {home} fm_end={fm_end}"
            );
            let body = source.find("Body").expect("Body");
            assert!(
                home <= body,
                "document start must not overshoot `# Body`, {source:?} got {home} body={body}"
            );
            let title = source.find("Hello").expect("title");
            assert!(
                home > title,
                "document start must not land in the YAML title, {source:?} home={home} title={title}"
            );
            let end = engine.clamp_raw_prefix(
                source,
                engine.snap_caret(source.len(), Bias::Left),
                Bias::Left,
            );
            assert!(
                end >= body,
                "document end must stay in the body, {source:?} got {end}"
            );
        }
    }

    #[test]
    fn snap_caret_skips_frontmatter() {
        for source in [
            "---\ntitle: Hello\n---\n\n# Body\n",
            "---\ntitle: Hello\n...\n\n# Body\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let fm_end = frontmatter_body_start(engine.tree());
            assert!(
                fm_end > 0,
                "YAML fences must occupy a positive prefix, {source:?} range={:?} raw_len={}",
                engine.tree().frontmatter.as_ref().map(|f| &f.source_range),
                engine
                    .tree()
                    .frontmatter
                    .as_ref()
                    .map(|f| f.raw.len())
                    .unwrap_or(0)
            );
            let snapped = engine.snap_caret(0, Bias::Right);
            assert!(
                snapped >= fm_end,
                "caret at 0 must sit after YAML, {source:?} got {snapped} fm_end={fm_end}"
            );
            let title = source.find("Hello").expect("title");
            assert!(
                snapped > title,
                "body caret must not land inside the YAML title, {source:?} snapped={snapped} title={title}"
            );
        }
    }

    #[test]
    fn snap_caret_skips_fenced_code_chrome() {
        let source = "```\ncode\n```\n";
        let (_doc, engine) = engine_for(source);
        let body = source.find("code").expect("code");
        assert_eq!(
            engine.snap_caret(0, Bias::Right),
            body,
            "opening ticks must snap into the body"
        );
        assert_eq!(
            engine.snap_caret(1, Bias::Left),
            body,
            "caret must not sit on fence ticks"
        );
        let close = source.rfind("```").expect("close");
        let body_end = body + "code".len();
        assert_eq!(
            engine.snap_caret(close, Bias::Left),
            body_end,
            "closing ticks must snap to the end of the body"
        );
        assert!(engine.in_raw_context(body));
        assert!(
            engine.in_raw_context(0),
            "fence bytes still belong to the code block"
        );
    }

    #[test]
    fn snap_caret_skips_inline_html_tags() {
        let source = "hello <b>bold</b>!\n";
        let (_doc, engine) = engine_for(source);
        let tag = source.find("<b>").unwrap();
        assert_eq!(engine.snap_caret(tag + 1, Bias::Right), tag + 3);
        let close = source.find("</b>").unwrap();
        assert_eq!(engine.snap_caret(close + 1, Bias::Left), close);
    }

    #[test]
    fn arrows_skip_html_phrasing_and_anchor_dest_chrome() {
        for source in [
            "hello <b>bold</b>!\n",
            "see <a href=\"https://e.com\">label</a> now\n",
            "a<!-- x -->b\n",
            "> hello <b>bold</b>\n",
            "- hello <b>bold</b>\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let inner = if source.contains("label") {
                source.find("label").expect("label")
            } else if source.contains("bold") {
                source.find("bold").expect("bold")
            } else {
                source.find('b').expect("b after comment")
            };
            let prev = engine.prev_caret(source, inner);
            assert_ne!(
                ch_at(source, prev),
                '>',
                "Left at HTML inner text must not sit on tag `>`, {source:?} got {} {:?}",
                prev,
                ch_at(source, prev)
            );
            assert_ne!(
                ch_at(source, prev),
                '<',
                "Left at HTML inner text must not sit on `<`, {source:?} got {} {:?}",
                prev,
                ch_at(source, prev)
            );
            let inner_end = if source.contains("label") {
                inner + "label".len()
            } else if source.contains("bold") {
                inner + "bold".len()
            } else {
                inner + 1
            };
            let next = engine.next_caret(source, inner_end);
            if inner_end < source.len() {
                assert_ne!(
                    ch_at(source, next),
                    '<',
                    "Right at the end of HTML inner text must skip the closer, {source:?} got {} {:?}",
                    next,
                    ch_at(source, next)
                );
            }
        }
    }

    #[test]
    fn snap_caret_skips_highlight_delimiters() {
        let source = "hello ==mark==!\n";
        let (_doc, engine) = engine_for(source);
        let open = source.find("==").unwrap();
        assert_eq!(engine.snap_caret(open + 1, Bias::Right), open + 2);
        let close = source.rfind("==").unwrap();
        assert_eq!(engine.snap_caret(close + 1, Bias::Left), close);
    }

    #[test]
    fn snap_caret_sits_on_math_dollars() {
        let source = "see $x$ tail\n";
        let (_doc, engine) = engine_for(source);
        let open = source.find('$').unwrap();
        assert_eq!(engine.snap_caret(open, Bias::Right), open);
        let inner = source.find('x').unwrap();
        assert_eq!(engine.snap_caret(inner, Bias::Right), inner);
        assert!(engine.in_raw_context(inner));
        assert!(!engine.in_raw_context(0));
    }

    #[test]
    fn snap_caret_sits_on_wikilink_brackets() {
        let source = "see [[page]] tail\n";
        let (_doc, engine) = engine_for(source);
        let open = source.find("[[").unwrap();
        assert_eq!(engine.snap_caret(open, Bias::Right), open);
        let inner = source.find("page").unwrap();
        assert_eq!(engine.snap_caret(inner, Bias::Right), inner);
        assert!(engine.in_raw_context(inner));
        assert!(!engine.in_raw_context(0));
    }

    #[test]
    fn snap_caret_skips_alert_tag_onto_title_or_body() {
        let source = "> [!NOTE]\n> body\n";
        let (_doc, engine) = engine_for(source);
        let tag = source.find("[!NOTE]").unwrap();
        let body = source.find("body").unwrap();
        assert_eq!(
            click_home(&engine, source, tag),
            body,
            "Home/click on `[!NOTE]` must skip onto the body"
        );
        assert_eq!(
            click_at(&engine, source, tag),
            body,
            "mouse-down on `[!NOTE]` must skip onto the body"
        );
        assert_ne!(ch_at(source, engine.snap_caret(tag, Bias::Right)), '[');
        assert!(engine.in_raw_context(tag));
        assert_eq!(engine.snap_caret(body, Bias::Right), body);
        assert!(!engine.in_raw_context(body));

        let titled = "> [!NOTE] Pay attention\n> body\n";
        let (_doc, engine) = engine_for(titled);
        let tag = titled.find("[!NOTE]").unwrap();
        let title = titled.find("Pay").unwrap();
        assert_eq!(
            click_home(&engine, titled, tag),
            title,
            "Home/click on `[!NOTE]` must skip onto the custom title"
        );
        assert_eq!(click_at(&engine, titled, tag), title);
        assert_eq!(ch_at(titled, click_home(&engine, titled, 0)), 'P');
    }

    #[test]
    fn snap_caret_stays_on_leading_blank_before_heading() {
        let source = "\n\n# Title";
        let (_doc, engine) = engine_for(source);
        let title = source.find("Title").expect("Title");
        assert_eq!(engine.snap_caret(0, Bias::Left), 0);
        assert_eq!(engine.snap_caret(0, Bias::Right), 0);
        assert_ne!(
            engine.snap_caret(0, Bias::Left),
            title,
            "leading blank must not snap onto `# Title`"
        );
        assert!(
            engine.block_at(0).is_none(),
            "Comrak has no empty paragraph"
        );
        let heading_start = engine.tree().blocks[0].source_range.start;
        let body = engine.snap_caret(heading_start, Bias::Right);
        let up = engine.prev_caret(source, body);
        assert!(
            up < heading_start,
            "arrow-up/left from the heading must sit on the blank, got {up} heading_start={heading_start} body={body}"
        );
        assert_eq!(engine.snap_caret(up, Bias::Left), up);
        assert!(
            !source[up..].starts_with("# Title") && !source[up..].starts_with("Title"),
            "blank caret must not land on the heading text, offset {up} in {source:?}"
        );
    }

    #[test]
    fn snap_caret_stays_on_extra_blank_between_paragraph_and_heading() {
        let source = "hello\n\n\n\n# Title";
        let (_doc, engine) = engine_for(source);
        let title = source.find("Title").expect("Title");
        let heading_start = engine.tree().blocks[1].source_range.start;
        let gap = blank_caret_gap_before(engine.tree(), 1).expect("extra blank before heading");
        assert!(
            gap.start < heading_start && gap.end == heading_start,
            "extra blank {gap:?} must abut the heading at {heading_start}"
        );
        let caret = gap.start;
        assert_eq!(engine.snap_caret(caret, Bias::Left), caret);
        assert_eq!(engine.snap_caret(caret, Bias::Right), caret);
        assert_ne!(engine.snap_caret(caret, Bias::Left), title);
        let body = engine.snap_caret(heading_start, Bias::Right);
        let up = engine.prev_caret(source, body);
        assert!(
            gap.start <= up && up < gap.end,
            "arrow-up from heading must sit in {gap:?}, got {up} body={body}"
        );
    }

    fn last_block_end_caret(engine: &RichEngine, source: &str, block_index: usize) -> usize {
        let end = engine.tree().blocks[block_index].source_range.end;
        let mut pos = engine.snap_caret(end.min(source.len()), Bias::Left);
        if blank_caret_gap_at(engine.tree(), pos).is_some() {
            pos = engine.snap_caret(end.saturating_sub(1).min(source.len()), Bias::Left);
        }
        pos
    }

    #[test]
    fn standard_block_separator_is_a_clickable_blank() {
        let source = "hello\n\n# Title";
        let (_doc, engine) = engine_for(source);
        let heading_start = engine.tree().blocks[1].source_range.start;
        let gaps = blank_caret_gaps(engine.tree());
        assert_eq!(
            gaps.len(),
            1,
            "one `\\n\\n` is one gap, not two, got {gaps:?}"
        );
        let gap = blank_caret_gap_before(engine.tree(), 1).expect("separator before heading");
        assert_eq!(gap.end, heading_start);
        assert!(
            gap.start < heading_start,
            "gap {gap:?} must sit before the heading at {heading_start}"
        );
        assert!(
            engine.block_at(gap.start).is_none(),
            "separator offset must not belong to a Comrak node"
        );
        assert_eq!(engine.snap_caret(gap.start, Bias::Left), gap.start);
        assert_eq!(engine.snap_caret(gap.start, Bias::Right), gap.start);
        let title = source.find("Title").expect("Title");
        assert_ne!(engine.snap_caret(gap.start, Bias::Left), title);
        assert!(
            !source[gap.start..].starts_with("# Title")
                && !source[gap.start..].starts_with("Title"),
            "separator caret must not land on heading chrome, offset {} in {source:?}",
            gap.start
        );

        let hello = last_block_end_caret(&engine, source, 0);
        let down1 = engine.vertical_caret(source, hello, 1);
        assert!(
            gap.start <= down1 && down1 < gap.end,
            "Down from the paragraph must land on the gap {gap:?}, got {down1} from {hello}"
        );
        let down2 = engine.vertical_caret(source, down1, 1);
        assert!(
            down2 >= heading_start,
            "second Down must leave the gap for the heading, got {down2} heading_start={heading_start}"
        );
        let down3 = engine.vertical_caret(source, down2, 1);
        assert_eq!(
            down3, down2,
            "no extra unused-newline step after the heading, got {down3} after {down2}"
        );

        let n1 = engine.next_caret(source, hello);
        assert!(
            gap.start <= n1 && n1 < gap.end,
            "Right from the paragraph must land on the gap, got {n1}"
        );
        let n2 = engine.next_caret(source, n1);
        assert!(
            n2 >= heading_start,
            "Right from the gap must enter the heading, got {n2}"
        );
        let up = engine.prev_caret(source, engine.snap_caret(heading_start, Bias::Right));
        assert!(
            gap.start <= up && up < gap.end,
            "Left from the heading must sit on the gap, got {up}"
        );
    }

    #[test]
    fn extra_newlines_collapse_to_one_blank_step() {
        let source = "hello\n\n\n\n# Title";
        let (_doc, engine) = engine_for(source);
        let heading_start = engine.tree().blocks[1].source_range.start;
        let gaps = blank_caret_gaps(engine.tree());
        assert_eq!(
            gaps.len(),
            1,
            "extra newlines stay one painted gap, got {gaps:?}"
        );
        let gap = blank_caret_gap_before(engine.tree(), 1).expect("extra blank");
        let hello = last_block_end_caret(&engine, source, 0);
        let down1 = engine.vertical_caret(source, hello, 1);
        assert!(
            gap.start <= down1 && down1 < gap.end,
            "Down from the paragraph lands on the gap, got {down1}"
        );
        let down2 = engine.vertical_caret(source, down1, 1);
        assert!(
            down2 >= heading_start,
            "extra unused newlines must not be extra Down steps, got {down2} heading_start={heading_start} gap={gap:?}"
        );
        let n1 = engine.next_caret(source, hello);
        let n2 = engine.next_caret(source, n1);
        assert!(
            n2 >= heading_start,
            "Right must skip unused newlines after one gap stop, got {n2} via {n1}"
        );
    }

    #[test]
    fn no_invented_blank_after_last_block() {
        for source in ["hello", "hello\n", "# Title"] {
            let (_doc, engine) = engine_for(source);
            assert!(
                blank_caret_gaps(engine.tree()).is_empty(),
                "no trailing gap without a blank line, {source:?} gaps={:?}",
                blank_caret_gaps(engine.tree())
            );
            assert!(
                blank_caret_gap_after_last(engine.tree()).is_none(),
                "single trailing `\\n` is the block terminator, {source:?}"
            );
        }
        let source = "hello\n\n# Title";
        let (_doc, engine) = engine_for(source);
        assert!(
            blank_caret_gap_before(engine.tree(), 0).is_none(),
            "first block has no leading gap"
        );
        assert!(blank_caret_gap_before(engine.tree(), 1).is_some());
        assert!(
            blank_caret_gap_after_last(engine.tree()).is_none(),
            "must not invent a trailing gap when the file does not end with a blank"
        );
        assert_eq!(
            blank_caret_gaps(engine.tree()).len(),
            1,
            "one `\\n\\n` between blocks is one gap, not a trailing extra"
        );
    }

    #[test]
    fn trailing_blank_after_last_block_is_a_clickable_blank() {
        for source in [
            "hello\n\n",
            "hello\n\n\n\n",
            "# Title\n\n",
            "- item\n\n",
            "```\ncode\n```\n\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let gap = blank_caret_gap_after_last(engine.tree())
                .unwrap_or_else(|| panic!("trailing blank must paint a gap, {source:?}"));
            assert_eq!(
                gap.end,
                source.len(),
                "trailing gap must run to EOF, {source:?} gap={gap:?}"
            );
            assert!(
                engine.block_at(gap.start).is_none(),
                "trailing offset must not belong to a Comrak node, {source:?}"
            );
            assert_eq!(engine.snap_caret(gap.start, Bias::Left), gap.start);
            assert_eq!(engine.snap_caret(gap.start, Bias::Right), gap.start);
            assert_eq!(
                engine.snap_caret(source.len(), Bias::Left),
                gap.start,
                "click/EOF on the trailing blank must not snap into the last block, {source:?}"
            );
            let last = last_block_end_caret(&engine, source, engine.tree().blocks.len() - 1);
            assert!(
                blank_caret_gap_at(engine.tree(), last).is_none()
                    && engine.block_at(last).is_some(),
                "end caret must sit in the last block, {source:?} last={last} gap={gap:?}"
            );
            let down1 = engine.vertical_caret(source, last, 1);
            assert!(
                gap.start <= down1 && down1 < gap.end,
                "Down from the last block must land on the trailing gap {gap:?}, got {down1} from {last} in {source:?}"
            );
            let down2 = engine.vertical_caret(source, down1, 1);
            assert_eq!(
                down2, down1,
                "extra unused trailing newlines must not be extra Down steps, {source:?} gap={gap:?}"
            );
            let n1 = engine.next_caret(source, last);
            assert!(
                gap.start <= n1 && n1 < gap.end,
                "Right from the last block must land on the trailing gap, got {n1} in {source:?}"
            );
            let up = engine.prev_caret(source, n1);
            assert_eq!(
                up, last,
                "Left from the trailing gap must return to the last block, got {up} last={last} in {source:?}"
            );
        }
    }

    #[test]
    fn trailing_blank_does_not_double_between_block_gap() {
        let source = "hello\n\n# Title\n\n";
        let (_doc, engine) = engine_for(source);
        let between = blank_caret_gap_before(engine.tree(), 1).expect("separator");
        let trailing = blank_caret_gap_after_last(engine.tree()).expect("trailing");
        assert!(
            between.end <= trailing.start,
            "between {between:?} and trailing {trailing:?} must not overlap"
        );
        assert_eq!(blank_caret_gaps(engine.tree()).len(), 2);
        let hello = last_block_end_caret(&engine, source, 0);
        let down1 = engine.vertical_caret(source, hello, 1);
        assert!(
            between.start <= down1 && down1 < between.end,
            "Down from hello is the between gap, got {down1}"
        );
        let heading_start = engine.tree().blocks[1].source_range.start;
        let down2 = engine.vertical_caret(source, down1, 1);
        assert!(
            down2 >= heading_start && down2 < trailing.start,
            "second Down is the heading, got {down2}"
        );
        let down3 = engine.vertical_caret(source, down2, 1);
        assert!(
            trailing.start <= down3 && down3 < trailing.end,
            "third Down is the trailing blank, got {down3}"
        );
    }

    #[test]
    fn newlines_only_document_hosts_a_caret() {
        for source in ["", "\n", "\n\n", "\n\n\n"] {
            let (_doc, engine) = engine_for(source);
            assert!(
                engine.tree().blocks.is_empty(),
                "newlines-only must have no Comrak blocks, {source:?}"
            );
            let gap = blank_caret_gap_after_last(engine.tree()).unwrap_or_else(|| {
                panic!("newlines-only document must host a caret gap, {source:?}")
            });
            assert_eq!(
                gap.start, 0,
                "caret home is the start, {source:?} gap={gap:?}"
            );
            assert_eq!(gap.end, source.len());
            assert_eq!(engine.snap_caret(0, Bias::Left), 0);
            assert_eq!(engine.snap_caret(source.len(), Bias::Left), 0);
            assert!(engine.block_at(0).is_none());
            assert_eq!(
                caret_for_click_below_content(engine.tree()),
                gap.start,
                "leftover click on an empty document sits on the blank, {source:?}"
            );
        }
    }

    #[test]
    fn click_below_content_uses_trailing_blank_else_eof() {
        let (_doc, engine) = engine_for("hello\n\n");
        let gap = blank_caret_gap_after_last(engine.tree()).expect("trailing blank");
        assert_eq!(caret_for_click_below_content(engine.tree()), gap.start);
        assert_ne!(
            caret_for_click_below_content(engine.tree()),
            engine.tree().blocks[0].source_range.end,
            "leftover click must not snap into the last paragraph"
        );

        for source in ["hello", "hello\n", "# Title"] {
            let (_doc, engine) = engine_for(source);
            assert!(blank_caret_gap_after_last(engine.tree()).is_none());
            assert_eq!(
                caret_for_click_below_content(engine.tree()),
                source.len(),
                "no trailing blank: query is document end (click opens a blank), {source:?}"
            );
        }
    }

    #[test]
    fn line_map_covers_blocks_in_order() {
        let source = "# A\n\npara one\nwrapped\n\n- item\n";
        let (_doc, engine) = engine_for(source);
        let map = engine.line_map(source);
        assert_eq!(map.len(), 3);
        assert_eq!(map[0].source_lines, 0..1);
        assert_eq!(map[1].source_lines, 2..4);
        assert_eq!(map[2].source_lines, 5..6);
    }

    #[test]
    fn outline_lists_headings_with_offsets() {
        let source = "# One\n\n## Two\n\n> ### Quoted\n";
        let (_doc, engine) = engine_for(source);
        let outline = engine.outline();
        assert_eq!(outline.len(), 3);
        assert_eq!(outline[0], (0, 1, "One".to_string()));
        assert_eq!(outline[1].1, 2);
        assert_eq!(outline[2].1, 3);
        assert_eq!(&outline[2].2, "Quoted");
    }

    fn first_raw(blocks: &[Block], pred: impl Fn(&BlockKind) -> bool + Copy) -> Option<&Block> {
        for b in blocks {
            if pred(&b.kind) {
                return Some(b);
            }
            if let Some(found) = first_raw(&b.children, pred) {
                return Some(found);
            }
        }
        None
    }

    fn ch_at(source: &str, off: usize) -> char {
        source
            .as_bytes()
            .get(off)
            .copied()
            .map(|b| b as char)
            .unwrap_or('∅')
    }

    #[test]
    fn quoted_paragraph_arrows_skip_quote_prefix() {
        let source = "> hello\n";
        let (_doc, engine) = engine_for(source);
        let h = source.find('h').expect("h");
        let gt = source.find('>').expect(">");
        let home = engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
        assert_eq!(
            home,
            h,
            "Home/click line-start must be `h`, got {}",
            ch_at(source, home)
        );
        assert_ne!(home, gt);
        assert_eq!(engine.snap_caret(0, Bias::Right), h);
        assert_eq!(engine.next_caret(source, h), h + 1);
        assert_ne!(engine.next_caret(source, h), gt);
        let prev = engine.prev_caret(source, h);
        assert_ne!(prev, gt, "Left at visible start must not sit on `>`");
        assert_ne!(ch_at(source, prev), '>');
    }

    #[test]
    fn quoted_wrapped_paragraph_right_skips_continuation_quote() {
        let source = "> hello\n> world\n";
        let (_doc, engine) = engine_for(source);
        let o = source.find("hello").unwrap() + 4;
        let w = source.find("world").expect("world");
        let gt = source.rfind("> world").expect("continuation");
        let after_o = engine.next_caret(source, o);
        let onto_w = if ch_at(source, after_o) == 'w' {
            after_o
        } else {
            engine.next_caret(source, after_o)
        };
        assert_eq!(
            onto_w,
            w,
            "Right at the wrap must skip `>` onto `w`, got {} {:?}",
            onto_w,
            ch_at(source, onto_w)
        );
        assert_ne!(onto_w, gt);
        assert_ne!(ch_at(source, onto_w), '>');
        let prev = engine.prev_caret(source, w);
        assert_ne!(
            ch_at(source, prev),
            '>',
            "Left from `w` must not sit on `>`"
        );
        let down = engine.vertical_caret(source, source.find('h').unwrap(), 1);
        assert_eq!(
            down,
            w,
            "Down from `h` must land on `w`, got {}",
            ch_at(source, down)
        );
    }

    #[test]
    fn list_item_arrows_skip_marker() {
        let source = "- hello\n";
        let (_doc, engine) = engine_for(source);
        let h = source.find('h').expect("h");
        let dash = source.find('-').expect("-");
        let home = engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
        assert_eq!(home, h, "Home must be `h`, got {}", ch_at(source, home));
        assert_ne!(home, dash);
        assert_eq!(engine.snap_caret(0, Bias::Right), h);
        assert_eq!(engine.next_caret(source, h), h + 1);
        let prev = engine.prev_caret(source, h);
        assert_ne!(
            ch_at(source, prev),
            '-',
            "Left at visible start must not sit on `-`"
        );
    }

    #[test]
    fn quoted_list_item_arrows_skip_quote_and_marker() {
        let source = "> - hello\n";
        let (_doc, engine) = engine_for(source);
        let h = source.find('h').expect("h");
        let home = engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
        assert_eq!(
            home,
            h,
            "Home must skip `> - ` onto `h`, got {}",
            ch_at(source, home)
        );
        assert_ne!(ch_at(source, home), '>');
        assert_ne!(ch_at(source, home), '-');
        assert_eq!(engine.snap_caret(0, Bias::Right), h);
        let prev = engine.prev_caret(source, h);
        assert_ne!(ch_at(source, prev), '>');
        assert_ne!(ch_at(source, prev), '-');
    }

    #[test]
    fn task_item_arrows_skip_checkbox_marker() {
        let source = "- [x] done\n";
        let (_doc, engine) = engine_for(source);
        let d = source.find('d').expect("d");
        let home = engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
        assert_eq!(
            home,
            d,
            "Home must skip `- [x] ` onto `d`, got {}",
            ch_at(source, home)
        );
        assert_eq!(engine.snap_caret(0, Bias::Right), d);
    }

    #[test]
    fn unquoted_paragraph_arrows_stay_one_to_one() {
        let source = "hello\n";
        let (_doc, engine) = engine_for(source);
        let h = source.find('h').expect("h");
        assert_eq!(engine.snap_caret(0, Bias::Right), h);
        assert_eq!(engine.next_caret(source, h), h + 1);
        assert_eq!(engine.next_caret(source, h + 1), h + 2);
        assert_eq!(engine.prev_caret(source, h + 2), h + 1);
        assert_eq!(
            engine.clamp_raw_prefix(source, h, Bias::Right),
            h,
            "unquoted body must stay 1:1"
        );
    }

    #[test]
    fn quoted_heading_home_skips_quote_and_hashes() {
        let source = "> # Title\n";
        let (_doc, engine) = engine_for(source);
        let t = source.find('T').expect("T");
        let home = engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
        assert_eq!(
            home,
            t,
            "quoted heading Home must be `T`, got {}",
            ch_at(source, home)
        );
        assert_eq!(engine.snap_caret(0, Bias::Right), t);
    }

    #[test]
    fn heading_home_still_skips_hashes() {
        let source = "# Title\n";
        let (_doc, engine) = engine_for(source);
        let t = source.find('T').expect("T");
        assert_eq!(engine.snap_caret(0, Bias::Right), t);
        assert_eq!(engine.next_caret(source, t), t + 1);
    }

    #[test]
    fn left_at_quote_body_start_leaves_the_block() {
        let source = "foo\n\n> hello\n";
        let (_doc, engine) = engine_for(source);
        let h = source.find('h').expect("h");
        let prev = engine.prev_caret(source, h);
        assert!(prev < h, "Left from `h` must leave the quote, got {prev}");
        assert_ne!(ch_at(source, prev), '>');
        assert_ne!(
            engine.snap_caret(prev, Bias::Left),
            h,
            "must not bounce back onto `hello`"
        );
    }

    #[test]
    fn left_at_list_body_start_leaves_the_item() {
        let source = "foo\n\n- hello\n";
        let (_doc, engine) = engine_for(source);
        let h = source.find("hello").expect("hello");
        let prev = engine.prev_caret(source, h);
        assert!(prev < h, "Left from `h` must leave the item, got {prev}");
        assert_ne!(ch_at(source, prev), '-');
    }

    #[test]
    fn quoted_fence_arrows_skip_quote_prefix() {
        let source = "> ```\n> ab\n> cd\n> ```\n";
        let (_doc, engine) = engine_for(source);
        let a = source.find("ab").expect("ab");
        let b = a + 1;
        let c = source.find("cd").expect("cd");
        let gt = source.find('>').expect(">");
        assert_eq!(engine.next_caret(source, a), b);
        assert_ne!(engine.next_caret(source, a), gt);
        assert_eq!(engine.prev_caret(source, b), a);
        assert_ne!(engine.prev_caret(source, a), gt);
        let after_b = engine.next_caret(source, b);
        let after_nl = engine.next_caret(source, after_b);
        assert_eq!(
            after_nl, c,
            "Right at end of `ab` must skip `>` onto `c`, got {after_nl}"
        );
        assert_ne!(&source[after_nl..after_nl + 1], ">");
        assert_eq!(engine.prev_caret(source, c), after_b);
        let down = engine.vertical_caret(source, a, 1);
        assert_eq!(
            down, c,
            "Down from `a` must land on `c`, not `>`, got {down}"
        );
        let up = engine.vertical_caret(source, c, -1);
        assert_eq!(up, a, "Up from `c` must land on `a`, got {up}");
    }

    #[test]
    fn unquoted_fence_arrows_stay_one_to_one() {
        let source = "```\ncode\n```\n";
        let (_doc, engine) = engine_for(source);
        let c = source.find("code").expect("code");
        assert_eq!(engine.next_caret(source, c), c + 1);
        assert_eq!(engine.next_caret(source, c + 1), c + 2);
        assert_eq!(engine.prev_caret(source, c + 2), c + 1);
        assert_eq!(engine.prev_caret(source, c + 1), c);
    }

    #[test]
    fn quoted_html_arrows_skip_quote_prefix() {
        let source = "> <div>\n> x\n> </div>\n";
        let (_doc, engine) = engine_for(source);
        let x = source.find('x').expect("x");
        let gt = source.find('>').expect(">");
        let block = first_raw(&engine.tree().blocks, |k| {
            matches!(k, BlockKind::Opaque { .. })
        })
        .expect("html");
        let start = engine.clamp_raw_prefix(source, block.source_range.start, Bias::Right);
        assert_ne!(start, gt);
        assert_ne!(&source[start..start + 1], ">");
        assert_ne!(
            &source[start..start + 1],
            "<",
            "HTML tag bytes must skip like quote prefix, start={start}"
        );
        let next = engine.next_caret(source, x);
        assert_ne!(next, gt);
        assert_ne!(
            &source[next.min(source.len().saturating_sub(1))..][..1],
            ">"
        );
        let prev = engine.prev_caret(source, x);
        assert_ne!(prev, gt, "Left from `x` must not land on `>`");
        assert!(
            source.as_bytes().get(prev) != Some(&b'>'),
            "Left from `x` landed on `>` at {prev}"
        );
    }

    #[test]
    fn quoted_list_nested_fence_arrows_skip_prefix() {
        let source = "> - item\n>   ```\n>   code\n>   ```\n";
        let (_doc, engine) = engine_for(source);
        let c = source.find("code").expect("code");
        let gt = source.rfind(">   code").expect("quoted body");
        assert_eq!(engine.next_caret(source, c), c + 1);
        assert_ne!(engine.prev_caret(source, c), gt);
        assert_ne!(
            engine.clamp_raw_prefix(source, gt, Bias::Right),
            gt,
            "caret on the body line must skip `>`"
        );
        assert_eq!(engine.clamp_raw_prefix(source, gt, Bias::Right), c);
    }

    #[test]
    fn empty_quoted_paragraph_caret_homes_after_prefix() {
        let source = "> ";
        let (_doc, engine) = engine_for(source);
        let home = engine.snap_caret(0, Bias::Right);
        assert_eq!(
            home,
            2,
            "Home/click on empty `> ` must sit after the prefix, got {}",
            ch_at(source, home)
        );
        assert_ne!(ch_at(source, home), '>');
        assert_eq!(engine.snap_caret(0, Bias::Left), 2);
        assert_eq!(engine.clamp_raw_prefix(source, 0, Bias::Right), 2);
        let prev = engine.prev_caret(source, home);
        assert_ne!(ch_at(source, prev), '>');
    }

    #[test]
    fn empty_list_item_caret_homes_after_marker() {
        let source = "- ";
        let (_doc, engine) = engine_for(source);
        let home = engine.snap_caret(0, Bias::Right);
        assert_eq!(
            home,
            2,
            "Home on empty `- ` must sit after the marker, got {}",
            ch_at(source, home)
        );
        assert_ne!(ch_at(source, home), '-');
        assert_eq!(engine.snap_caret(0, Bias::Left), 2);
    }

    #[test]
    fn empty_ordered_item_caret_homes_after_marker() {
        let source = "1. ";
        let (_doc, engine) = engine_for(source);
        let home = engine.snap_caret(0, Bias::Right);
        assert_eq!(
            home,
            3,
            "Home on empty `1. ` must sit after the marker, got {}",
            ch_at(source, home)
        );
        assert_ne!(ch_at(source, home), '1');
    }

    #[test]
    fn empty_task_item_caret_homes_after_checkbox() {
        let source = "- [ ] ";
        let (_doc, engine) = engine_for(source);
        let home = engine.snap_caret(0, Bias::Right);
        assert_eq!(
            home,
            source.len(),
            "Home on empty task must sit after `- [ ] `, got {}",
            ch_at(source, home)
        );
        assert_ne!(ch_at(source, home.min(source.len().saturating_sub(1))), '-');
        assert_ne!(ch_at(source, engine.snap_caret(0, Bias::Left)), '-');
    }

    #[test]
    fn empty_quoted_line_after_body_is_a_caret_home() {
        let source = "> hello\n> ";
        let (_doc, engine) = engine_for(source);
        let h = source.find('h').expect("h");
        let end = h + 5;
        let home = engine.next_caret(source, end);
        assert_eq!(
            home,
            source.len(),
            "Right from `hello` must sit on the empty quoted line, got {} {:?}",
            home,
            ch_at(source, home)
        );
        assert_ne!(ch_at(source, home.min(source.len().saturating_sub(1))), '>');
        assert_eq!(
            engine.snap_caret(source.find('>').unwrap() + 8, Bias::Right),
            home
        );
        let prev = engine.prev_caret(source, home);
        assert!(
            prev <= end,
            "Left from the empty quoted line must leave it, got {prev}"
        );
        assert_ne!(ch_at(source, prev), '>');
    }

    #[test]
    fn empty_quoted_line_between_paragraphs_is_a_caret_home() {
        let source = "> hello\n>\n> world\n";
        let (_doc, engine) = engine_for(source);
        let h = source.find("hello").unwrap() + 5;
        let w = source.find("world").expect("world");
        let mid = engine.next_caret(source, h);
        assert!(
            mid > h && mid < w,
            "Right from `hello` must sit on the empty `>` line before `world`, got {mid}"
        );
        assert_ne!(ch_at(source, mid), '>');
        assert_eq!(
            engine.snap_caret(source.find(">\n").expect("empty"), Bias::Right),
            mid
        );
        let onto_w = engine.next_caret(source, mid);
        assert_eq!(
            onto_w, w,
            "Right from the empty quote line must land on `w`"
        );
        let back = engine.prev_caret(source, w);
        assert_eq!(
            back,
            mid,
            "Left from `w` must sit on the empty quote line, got {}",
            ch_at(source, back)
        );
        let down = engine.vertical_caret(source, source.find('h').unwrap(), 1);
        assert_eq!(
            down,
            mid,
            "Down from `h` must land on the empty quoted line, got {}",
            ch_at(source, down)
        );
    }

    #[test]
    fn nested_quote_arrows_skip_inner_marker() {
        let source = "> > hello\n";
        let (_doc, engine) = engine_for(source);
        let h = source.find('h').expect("h");
        let home = engine.snap_caret(0, Bias::Right);
        assert_eq!(
            home,
            h,
            "nested quote Home must be `h`, got {}",
            ch_at(source, home)
        );
        assert_ne!(ch_at(source, home), '>');
        let prev = engine.prev_caret(source, h);
        assert_ne!(
            ch_at(source, prev),
            '>',
            "Left at start must not sit on `>`"
        );
        assert!(
            prev <= h,
            "Left at innermost body start must not walk onto chrome, got {prev}"
        );
    }

    #[test]
    fn nested_quote_after_paragraph_left_leaves_the_body() {
        let source = "foo\n\n> > hello\n";
        let (_doc, engine) = engine_for(source);
        let h = source.find('h').expect("h");
        let prev = engine.prev_caret(source, h);
        assert!(
            prev < h,
            "Left from `h` must leave the nested quote, got {prev}"
        );
        assert_ne!(ch_at(source, prev), '>');
        assert_ne!(engine.snap_caret(prev, Bias::Left), h);
    }

    #[test]
    fn ordered_list_arrows_skip_marker() {
        let source = "1. hello\n";
        let (_doc, engine) = engine_for(source);
        let h = source.find('h').expect("h");
        let home = engine.snap_caret(0, Bias::Right);
        assert_eq!(home, h, "Home must be `h`, got {}", ch_at(source, home));
        assert_ne!(ch_at(source, home), '1');
        let prev = engine.prev_caret(source, h);
        assert_ne!(ch_at(source, prev), '1');
        assert_ne!(ch_at(source, prev), '.');
    }

    #[test]
    fn unchecked_task_arrows_skip_checkbox_marker() {
        let source = "- [ ] hello\n";
        let (_doc, engine) = engine_for(source);
        let h = source.find('h').expect("h");
        let home = engine.snap_caret(0, Bias::Right);
        assert_eq!(
            home,
            h,
            "Home must skip `- [ ] ` onto `h`, got {}",
            ch_at(source, home)
        );
        assert_eq!(engine.snap_caret(0, Bias::Right), h);
        let prev = engine.prev_caret(source, h);
        assert_ne!(ch_at(source, prev), '-');
        assert_ne!(ch_at(source, prev), '[');
    }

    #[test]
    fn list_item_link_with_x_label_is_not_a_task_checkbox() {
        for source in [
            "- [x](https://e.com)\n",
            "* [x](https://e.com)\n",
            "1. [x](https://e.com)\n",
            "> - [x](https://e.com)\n",
            "- [X](https://e.com)\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let x = source
                .find("[x]")
                .or_else(|| source.find("[X]"))
                .expect("label")
                + 1;
            let dest = source.find("https").expect("dest");
            let home =
                engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
            assert_eq!(
                home,
                x,
                "Home on {source:?} must be the link label, got {} {:?}",
                home,
                ch_at(source, home)
            );
            assert_ne!(home, dest, "must not skip into dest chrome of {source:?}");
            assert_ne!(ch_at(source, home), '(');
            assert_ne!(ch_at(source, home), 'h');
            let click =
                engine.clamp_raw_prefix(source, engine.snap_caret(x, Bias::Left), Bias::Left);
            assert_eq!(
                click,
                x,
                "click on label of {source:?} must stay on `x`, got {} {:?}",
                click,
                ch_at(source, click)
            );
        }

        let task = "- [x] done\n";
        let (_doc, engine) = engine_for(task);
        let d = task.find('d').expect("d");
        let home = engine.clamp_raw_prefix(task, engine.snap_caret(0, Bias::Right), Bias::Right);
        assert_eq!(home, d, "real task Home must still skip `- [x] ` onto `d`");
    }

    #[test]
    fn list_item_x_at_eol_without_space_is_not_a_task_checkbox() {
        assert_eq!(
            list_marker_on_line("- [x]"),
            "- ",
            "`- [x]` at EOL is not a GFM task (no space after `]`)"
        );
        assert_eq!(list_marker_on_line("- [x] done"), "- [x] ");
        assert_eq!(list_marker_on_line("- [x](https://e.com)"), "- ");
        assert_eq!(list_marker_on_line("- [ ]"), "- ");
        assert_eq!(list_marker_on_line("- [ ] "), "- [ ] ");
        assert_eq!(list_marker_on_line("* [X]"), "* ");
        assert_eq!(list_marker_on_line("1. [x]"), "1. ");

        assert_eq!(list_marker_on_line("-\titem"), "-\t");
        assert_eq!(list_marker_on_line("*\titem"), "*\t");
        assert_eq!(list_marker_on_line("+\titem"), "+\t");
        assert_eq!(list_marker_on_line("1.\titem"), "1.\t");
        assert_eq!(list_marker_on_line("1)\titem"), "1)\t");
        assert_eq!(list_marker_on_line("-   item"), "-   ");
        assert_eq!(list_marker_on_line("-    item"), "-    ");
        assert_eq!(list_marker_on_line("-     item"), "- ");
        assert_eq!(list_marker_on_line("-\t[ ] hello"), "-\t[ ] ");
        assert_eq!(list_marker_on_line("-   [x] done"), "-   [x] ");
        assert_eq!(list_marker_on_line("1.   item"), "1.   ");
        assert_eq!(list_marker_on_line("-item"), "");
        assert_eq!(list_marker_on_line("1.item"), "");
        assert_eq!(list_marker_on_line("1234567890. item"), "");
        assert_eq!(list_marker_on_line("-"), "-");
        assert_eq!(list_marker_on_line("1."), "1.");

        let q = line_prefix_parts("> hello\n", 2);
        assert_eq!(&"> hello\n"[q.quote.clone()], "> ");
        assert!(
            q.list.start == q.list.end,
            "quoted prose has no list marker"
        );
        let item = line_prefix_parts("- [x] done\n", 8);
        assert_eq!(&"- [x] done\n"[item.list.clone()], "- [x] ");
        assert!(item.quote.start == item.quote.end);
        let both = line_prefix_parts("> 1. hi\n", 6);
        assert_eq!(&"> 1. hi\n"[both.quote.clone()], "> ");
        assert_eq!(&"> 1. hi\n"[both.list.clone()], "1. ");
        assert!(both.footnote.start == both.footnote.end);
        assert!(both.details.start == both.details.end);

        let details = line_prefix_parts("Term\n: details\n", 6);
        assert_eq!(&"Term\n: details\n"[details.details.clone()], ": ");
        assert!(details.footnote.start == details.footnote.end);
        let quoted_details = line_prefix_parts("> : details\n", 4);
        assert_eq!(&"> : details\n"[quoted_details.quote.clone()], "> ");
        assert_eq!(&"> : details\n"[quoted_details.details.clone()], ": ");
        let fn_def = line_prefix_parts("[^1]: the note\n", 6);
        assert_eq!(&"[^1]: the note\n"[fn_def.footnote.clone()], "[^1]: ");
        assert!(fn_def.details.start == fn_def.details.end);
        let quoted_fn = line_prefix_parts("> [^note]: x\n", 8);
        assert_eq!(&"> [^note]: x\n"[quoted_fn.quote.clone()], "> ");
        assert_eq!(&"> [^note]: x\n"[quoted_fn.footnote.clone()], "[^note]: ");
        let ref_line = line_prefix_parts("Hello[^1]\n", 0);
        assert!(ref_line.footnote.start == ref_line.footnote.end);

        let shortcut = "- [x]\n\n[x]: https://e.com\n";
        let (_doc, engine) = engine_for(shortcut);
        let x = shortcut.find("[x]").expect("label") + 1;
        let close = shortcut.find(']').expect("]");
        let home =
            engine.clamp_raw_prefix(shortcut, engine.snap_caret(0, Bias::Right), Bias::Right);
        assert_eq!(
            home,
            x,
            "Home on shortcut-ref `- [x]` must be the label, got {} {:?}",
            home,
            ch_at(shortcut, home)
        );
        assert!(
            home <= close,
            "must not skip `[x]` at EOL as a checkbox, home={home} {:?}",
            ch_at(shortcut, home)
        );
        assert_ne!(ch_at(shortcut, home), 'h');
        let click = engine.clamp_raw_prefix(shortcut, engine.snap_caret(x, Bias::Left), Bias::Left);
        assert_eq!(
            click,
            x,
            "click on shortcut-ref label must stay on `x`, got {} {:?}",
            click,
            ch_at(shortcut, click)
        );

        for source in ["- [x]\n", "* [x]\n", "1. [x]\n", "> - [x]\n", "- [ ]\n"] {
            let (_doc, engine) = engine_for(source);
            let close = source.find(']').expect("]");
            let home =
                engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
            assert!(
                home <= close,
                "Home on {source:?} must not skip `[x]`/`[ ]` at EOL as a task, got {} {:?}",
                home,
                ch_at(source, home)
            );
            assert_ne!(
                ch_at(source, home),
                '∅',
                "Home on {source:?} must sit on the `[x]` slot, not past it"
            );
        }
    }

    /// CommonMark list-marker padding is a tab or 1–4 spaces. Home/click skip
    /// that dest chrome onto the body (and GFM tasks after extra padding).
    /// Five spaces keep one-space padding so indented code still starts.
    /// `1.item` / ten-digit `1234567890. item` are paragraphs, not lists.
    #[test]
    fn list_marker_padding_tab_and_spaces_are_dest_chrome() {
        for (source, needle) in [
            ("-\titem\n", "item"),
            ("*\titem\n", "item"),
            ("+\titem\n", "item"),
            ("1.\titem\n", "item"),
            ("1)\titem\n", "item"),
            ("-   item\n", "item"),
            ("-    item\n", "item"),
            ("1.   item\n", "item"),
            ("1)   item\n", "item"),
            ("> -\titem\n", "item"),
            ("> -   item\n", "item"),
            ("- [ ] hello\n", "hello"),
            ("-\t[ ] hello\n", "hello"),
            ("-   [x] done\n", "done"),
            ("1.\t[ ] hello\n", "hello"),
            ("> -\t[ ] hello\n", "hello"),
        ] {
            let (_doc, engine) = engine_for(source);
            let body = source.find(needle).expect(needle);
            let home =
                engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
            assert_eq!(
                home,
                body,
                "Home must skip list padding onto {needle:?}, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
            assert_ne!(
                ch_at(source, home),
                '\t',
                "Home must not sit on marker tab, {source:?}"
            );
            assert_ne!(
                ch_at(source, home),
                ' ',
                "Home must not sit on marker padding, {source:?}"
            );
            let click =
                engine.clamp_raw_prefix(source, engine.snap_caret(body, Bias::Left), Bias::Left);
            assert_eq!(
                click,
                body,
                "click on body must stay on {needle:?}, {source:?} got {} {:?}",
                click,
                ch_at(source, click)
            );
            let prev = engine.prev_caret(source, body);
            assert_ne!(
                ch_at(source, prev),
                '\t',
                "Left at body start must not sit on tab padding, {source:?}"
            );
        }

        let five = "-     item\n";
        let (_doc, engine) = engine_for(five);
        let item = five.find("item").expect("item");
        let home = engine.clamp_raw_prefix(five, engine.snap_caret(0, Bias::Right), Bias::Right);
        assert_eq!(
            home,
            item,
            "five spaces after `-` keep one-space padding; Home still lands on `item`, got {} {:?}",
            home,
            ch_at(five, home)
        );

        for source in ["1.item\n", "1234567890. item\n", "-item\n"] {
            let (_doc, engine) = engine_for(source);
            assert!(
                matches!(engine.tree().blocks[0].kind, BlockKind::Paragraph),
                "{source:?} must import as a paragraph, got {:?}",
                engine.tree().blocks[0].kind
            );
            let home =
                engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
            assert_eq!(
                home,
                0,
                "paragraph {source:?} Home must not skip a fake list marker, got {} {:?}",
                home,
                ch_at(source, home)
            );
        }
    }

    #[test]
    fn wrapped_list_continuation_right_skips_indent() {
        let source = "- hello\n  world\n";
        let (_doc, engine) = engine_for(source);
        let end = source.find("hello").unwrap() + 5;
        let w = source.find("world").expect("world");
        let onto_w = engine.next_caret(source, end);
        assert_eq!(
            onto_w,
            w,
            "Right from end of `hello` must skip continuation spaces onto `w`, got {} {:?}",
            onto_w,
            ch_at(source, onto_w)
        );
        assert_ne!(ch_at(source, onto_w), ' ');
        let space = source.find("  world").expect("indent");
        assert_eq!(
            engine.snap_caret(space, Bias::Right),
            w,
            "snap on continuation indent must land on `w`"
        );
        assert_eq!(
            engine.clamp_raw_prefix(source, space, Bias::Right),
            w,
            "clamp must skip continuation indent"
        );
        let prev = engine.prev_caret(source, w);
        assert_ne!(
            ch_at(source, prev),
            ' ',
            "Left from `w` must not sit on indent"
        );
    }

    fn first_image_range(engine: &RichEngine) -> Range<usize> {
        fn walk(blocks: &[Block]) -> Option<Range<usize>> {
            for b in blocks {
                if let Some(r) = super::html_block_image_range(b) {
                    return Some(r);
                }
                for inline in &b.inlines {
                    if let Some(r) = super::atomic_image_range(inline) {
                        return Some(r);
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
    fn image_is_one_caret_step() {
        let source = "hello ![cat](a.png) world\n";
        let (_doc, engine) = engine_for(source);
        let img = first_image_range(&engine);
        let after_hello = source.find('!').expect("image");
        assert_eq!(img.start, after_hello);
        assert_eq!(
            engine.next_caret(source, img.start),
            img.end,
            "Right at the image must skip `![…](url)` in one step"
        );
        assert_eq!(
            engine.prev_caret(source, img.end),
            img.start,
            "Left after the image must skip `![…](url)` in one step"
        );
        let mid = img.start + 3;
        assert_eq!(
            engine.snap_caret(mid, Bias::Right),
            img.end,
            "caret inside image markdown must snap out"
        );
        assert_eq!(engine.snap_caret(mid, Bias::Left), img.start);
    }

    #[test]
    fn html_block_svg_is_one_caret_step() {
        let inline = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\"><rect width=\"8\" height=\"8\" fill=\"#f00\"/></svg>\n";
        let block = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\">\n<rect width=\"8\" height=\"8\" fill=\"#f00\"/>\n</svg>\n";
        for source in [
            inline.to_string(),
            format!("- {inline}"),
            format!("> {inline}"),
            block.to_string(),
        ] {
            let (_doc, engine) = engine_for(&source);
            let img = first_image_range(&engine);
            assert!(
                source[img.clone()].contains("<svg"),
                "HTML-block svg range, got {:?} in {source:?}",
                &source[img.clone()]
            );
            assert_eq!(
                engine.next_caret(&source, img.start),
                img.end,
                "Right at HTML-block `<svg>` must skip dest chrome, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(&source, img.end),
                img.start,
                "Left after HTML-block `<svg>` must skip dest chrome, {source:?}"
            );
            if let Some(fill) = source.find("fill=") {
                assert_ne!(
                    ch_at(&source, engine.snap_caret(fill, Bias::Right)),
                    'f',
                    "caret must not sit on SVG `fill=` dest chrome, {source:?}"
                );
            }
        }
    }

    #[test]
    fn html_block_img_is_one_caret_step() {
        for source in [
            "<img src=\"a.png\" alt=\"x\">\n",
            "- <img src=\"a.png\" alt=\"x\">\n",
            "> <img src=\"a.png\" alt=\"x\">\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let img = first_image_range(&engine);
            assert!(
                source[img.clone()].contains("<img"),
                "HTML-block img range, got {:?} in {source:?}",
                &source[img.clone()]
            );
            assert_eq!(
                engine.next_caret(source, img.start),
                img.end,
                "Right at HTML-block `<img>` must skip src dest, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, img.end),
                img.start,
                "Left after HTML-block `<img>` must skip src dest, {source:?}"
            );
            let src = source.find("src=").expect("src");
            assert_ne!(
                ch_at(source, engine.snap_caret(src, Bias::Right)),
                's',
                "caret must not sit on HTML-block img `src=` dest chrome, {source:?}"
            );
        }
    }

    #[test]
    fn standalone_image_arrows_skip_the_markdown() {
        let source = "![cat](a.png)\n";
        let (_doc, engine) = engine_for(source);
        let img = first_image_range(&engine);
        assert_eq!(engine.next_caret(source, img.start), img.end);
        assert_eq!(engine.prev_caret(source, img.end), img.start);
    }

    #[test]
    fn copy_of_visible_bold_is_markdown() {
        let source = "**hello** world\n";
        let (_doc, engine) = engine_for(source);
        let inner = source.find("hello").expect("hello")
            ..source.find("hello").expect("hello") + "hello".len();
        let copied = engine.markdown_for_selection(source, inner.clone());
        assert_eq!(
            copied, "**hello**",
            "Typora copies markdown, not painted inner text, got {copied:?}"
        );
        let empty_caret = engine.markdown_for_selection(source, 0..0);
        assert!(
            empty_caret.contains("**hello**") && empty_caret.contains("world"),
            "empty caret copies the current paragraph, got {empty_caret:?}"
        );
        let ell = inner.start + 1..inner.end - 1;
        assert_eq!(
            engine.markdown_for_selection(source, ell),
            "ell",
            "partial selection inside bold stays inner text"
        );
    }

    #[test]
    fn empty_caret_copy_is_current_block_markdown() {
        let heading = "# Title\n";
        let (_doc, engine) = engine_for(heading);
        let t = heading.find('T').expect("T");
        let copied = engine.markdown_for_selection(heading, t..t);
        assert!(
            copied.starts_with("# Title"),
            "empty-caret heading copy must include `# `, got {copied:?}"
        );
        assert!(!copied.contains('\n'), "copy omits the terminator newline");

        let list = "- hello\n";
        let (_doc, engine) = engine_for(list);
        let h = list.find('h').expect("h");
        let copied = engine.markdown_for_selection(list, h..h);
        assert!(
            copied.starts_with("- hello"),
            "empty-caret list copy must include the marker, got {copied:?}"
        );

        let quote = "> hello\n";
        let (_doc, engine) = engine_for(quote);
        let h = quote.find('h').expect("h");
        let copied = engine.markdown_for_selection(quote, h..h);
        assert!(
            copied.contains("> hello"),
            "empty-caret quote copy must include `>`, got {copied:?}"
        );

        let fence = "```rust\ncode\n```\n";
        let (_doc, engine) = engine_for(fence);
        let c = fence.find("code").expect("code");
        let copied = engine.markdown_for_selection(fence, c..c);
        assert!(
            copied.contains("```") && copied.contains("code"),
            "empty-caret fence copy must include ticks, got {copied:?}"
        );

        let para = "hello world\n";
        let (_doc, engine) = engine_for(para);
        let w = para.find('w').expect("w");
        assert_eq!(
            engine.markdown_for_selection(para, w..w).trim_end(),
            "hello world"
        );

        let img = "![cat](a.png)\n";
        let (_doc, engine) = engine_for(img);
        let bang = img.find('!').expect("image");
        assert_eq!(
            engine.markdown_for_selection(img, bang..bang),
            "![cat](a.png)"
        );

        let table = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (_doc, engine) = engine_for(table);
        let a = table.find('a').expect("a");
        let copied = engine.markdown_for_selection(table, a..a);
        assert!(
            copied.contains('a') && !copied.contains('|'),
            "empty-caret table copy is cell text, got {copied:?}"
        );
        let cut = engine.expand_markdown_cut_selection(table, a..a);
        assert_eq!(cut.start, cut.end, "empty-caret table Cut is a no-op");
    }

    #[test]
    fn copy_of_visible_heading_and_list_is_markdown() {
        let heading = "# Title\n";
        let (_doc, engine) = engine_for(heading);
        let title = heading.find("Title").expect("Title");
        let copied = engine.markdown_for_selection(heading, title..title + "Title".len());
        assert!(
            copied.starts_with("# Title"),
            "heading copy must include `# `, got {copied:?}"
        );

        let list = "- hello\n";
        let (_doc, engine) = engine_for(list);
        let h = list.find("hello").expect("hello");
        let copied = engine.markdown_for_selection(list, h..h + "hello".len());
        assert!(
            copied.starts_with("- hello"),
            "list copy must include the marker, got {copied:?}"
        );

        let quote = "> hello\n";
        let (_doc, engine) = engine_for(quote);
        let h = quote.find("hello").expect("hello");
        let copied = engine.markdown_for_selection(quote, h..h + "hello".len());
        assert!(
            copied.contains("> hello"),
            "quote copy must include `>`, got {copied:?}"
        );
    }

    #[test]
    fn copy_of_visible_link_is_markdown() {
        let source = "[hello](https://e.com)\n";
        let (_doc, engine) = engine_for(source);
        let hello = source.find("hello").expect("hello");
        let copied = engine.markdown_for_selection(source, hello..hello + "hello".len());
        assert_eq!(
            copied, "[hello](https://e.com)",
            "link copy must include dest, got {copied:?}"
        );

        let wrapped = "**[hello](https://e.com)**\n";
        let (_doc, engine) = engine_for(wrapped);
        let hello = wrapped.find("hello").expect("hello");
        let copied = engine.markdown_for_selection(wrapped, hello..hello + "hello".len());
        assert_eq!(
            copied, "**[hello](https://e.com)**",
            "copy of a fully selected bold-wrapped link must include wrapping `**` and dest, got {copied:?}"
        );
    }

    #[test]
    fn copy_of_visible_reference_link_is_markdown() {
        let source = "[hello][ref]\n\n[ref]: https://e.com\n";
        let (_doc, engine) = engine_for(source);
        let hello = source.find("hello").expect("hello");
        let copied = engine.markdown_for_selection(source, hello..hello + "hello".len());
        assert!(
            copied.contains("[hello][ref]"),
            "reference link copy must include `[ref]`, got {copied:?}"
        );
    }

    #[test]
    fn copy_of_reference_definition_is_markdown() {
        let source = "[hello][ref]\n\n[ref]: https://e.com\n";
        let (_doc, engine) = engine_for(source);
        let dest = source.find("https://e.com").expect("dest");
        let copied = engine.markdown_for_selection(source, dest..dest);
        assert!(
            copied.contains("[ref]: https://e.com"),
            "definition copy must keep `[ref]: url`, got {copied:?}"
        );
        let hello = source.find("hello").expect("hello");
        let copied_link = engine.markdown_for_selection(source, hello..hello + "hello".len());
        assert!(
            copied_link.contains("[hello][ref]"),
            "reference link copy must still include `[ref]`, got {copied_link:?}"
        );
    }

    #[test]
    fn reference_definition_caret_skips_brackets_onto_label_and_dest() {
        for source in [
            "[hello][ref]\n\n[ref]: https://e.com\n",
            "[ref]: https://e.com\n",
            "> [hello][ref]\n>\n> [ref]: https://e.com\n",
            "- [hello][ref]\n  [ref]: https://e.com\n",
            "![cat][ref]\n\n[ref]: a.png\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let def_open = source.rfind("[ref]").expect("def [ref]");
            let label = def_open + 1;
            assert_eq!(
                ch_at(source, label),
                'r',
                "def label starts at `r` in {source:?}"
            );
            let def_line = source[..def_open].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let home = click_home(&engine, source, def_line);
            let click_open = click_at(&engine, source, def_open);
            assert_eq!(
                ch_at(source, home),
                'r',
                "Home/click on a definition must skip `[` onto the label, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
            assert_eq!(ch_at(source, click_open), 'r');
            assert_ne!(ch_at(source, home), '[');
            assert_ne!(ch_at(source, home), ':');
            assert_ne!(ch_at(source, click_open), '[');
            assert_ne!(ch_at(source, click_open), ':');
            let colon = source.rfind("]:").expect("]:") + 1;
            assert_eq!(ch_at(source, colon), ':');
            let dest = source[colon + 1..]
                .find(|c: char| !c.is_whitespace())
                .map(|i| colon + 1 + i)
                .expect("dest");
            for landed in [
                click_home(&engine, source, colon),
                click_at(&engine, source, colon),
                engine.snap_caret(colon, Bias::Right),
                engine.snap_caret(colon, Bias::Left),
            ] {
                assert_ne!(
                    ch_at(source, landed),
                    ':',
                    "click/Home on `:` must skip onto dest, {source:?} got {} {:?}",
                    landed,
                    ch_at(source, landed)
                );
                assert_eq!(
                    landed,
                    dest,
                    "caret on `:` must skip onto dest, {source:?} got {} {:?}",
                    landed,
                    ch_at(source, landed)
                );
            }
            let after_label = label + 3;
            let next = engine.next_caret(source, after_label);
            assert_ne!(
                ch_at(source, next),
                ']',
                "Right at end of definition label must skip `]`, {source:?} got {} {:?}",
                next,
                ch_at(source, next)
            );
            assert_ne!(ch_at(source, next), ':');
            let prev = engine.prev_caret(source, dest);
            assert_ne!(
                ch_at(source, prev),
                ':',
                "Left at dest must skip `: `, {source:?} got {} {:?}",
                prev,
                ch_at(source, prev)
            );
            if source.starts_with("[hello][ref]") {
                let hello = source.find("[hello]").expect("[hello]");
                let link_home = click_home(&engine, source, hello);
                assert_eq!(
                    ch_at(source, link_home),
                    'h',
                    "must not regress `[hello][ref]` Home onto chrome, {source:?}"
                );
            }
        }
    }

    #[test]
    fn nested_list_reference_link_skips_open_bracket() {
        for source in [
            "- [hello][ref]\n  [ref]: https://e.com\n",
            "> - [hello][ref]\n>   [ref]: https://e.com\n",
            "- [x] [hello][ref]\n  [ref]: https://e.com\n",
            "- [x] done\n- [hello][ref]\n  [ref]: https://e.com\n",
            "> [hello][ref]\n> [ref]: https://e.com\n",
            "- [**hello**][ref]\n  [ref]: https://e.com\n",
            "- [x] [**hello**][ref]\n  [ref]: https://e.com\n",
            "- outer\n  - [hello][ref]\n    [ref]: https://e.com\n",
            "> - outer\n>   - [hello][ref]\n>     [ref]: https://e.com\n",
            "- [ ] [**hello**][ref]\n  [ref]: https://e.com\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let hello = source.find("hello").expect("hello");
            assert_eq!(ch_at(source, hello), 'h');
            let open = source[..hello].rfind('[').expect("[");
            let line = source[..open].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let home = click_home(&engine, source, line);
            let click = click_at(&engine, source, open);
            assert_eq!(
                ch_at(source, home),
                'h',
                "Home on nested `[hello][ref]` must skip `[`, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
            assert_eq!(
                ch_at(source, click),
                'h',
                "click on nested `[hello][ref]` must skip `[`, {source:?} got {} {:?}",
                click,
                ch_at(source, click)
            );
            assert_ne!(
                ch_at(source, home),
                'r',
                "must not treat dest `[ref]` as the link, {source:?}"
            );
            let copied = engine.markdown_for_selection(source, hello..hello + "hello".len());
            assert!(
                copied.contains("[hello][ref]") || copied.contains("[**hello**][ref]"),
                "copy of nested reference link must be markdown, {source:?} got {copied:?}"
            );
        }

        let task = "- [x] done\n  [x]: https://e.com\n";
        let (_doc, engine) = engine_for(task);
        let d = task.find('d').expect("d");
        let home = click_home(&engine, task, 0);
        assert_eq!(
            home,
            d,
            "task `[x]` with a nested `[x]:` def must still Home onto `d`, got {} {:?}",
            home,
            ch_at(task, home)
        );

        let inline_x = "- [x](https://e.com)\n";
        let (_doc, engine) = engine_for(inline_x);
        let x = inline_x.find("[x]").expect("x") + 1;
        assert_eq!(click_home(&engine, inline_x, 0), x);

        for source in [
            "- [![cat](a.png)][ref]\n  [ref]: https://e.com\n",
            "> - [![cat](a.png)][ref]\n>   [ref]: https://e.com\n",
            "- outer\n  - [![cat](a.png)][ref]\n    [ref]: https://e.com\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let img = first_image_range(&engine);
            let line = source[..img.start].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let home = click_home(&engine, source, line);
            let click = click_at(&engine, source, source[..img.start].rfind('[').expect("["));
            assert_eq!(
                home, img.start,
                "Home on wrapping `[![cat]…][ref]` must sit on the image, {source:?} got {home} {:?}",
                ch_at(source, home)
            );
            assert_eq!(
                click,
                img.start,
                "click on wrapping `[` must skip onto the image, {source:?} got {click} {:?}",
                ch_at(source, click)
            );
            assert_ne!(
                ch_at(source, home),
                'r',
                "must not treat dest `[ref]` as the wrapping link, {source:?}"
            );
            let copied = engine.markdown_for_selection(source, img.clone());
            assert!(
                copied.contains("[![cat]") && copied.contains("[ref]"),
                "copy of wrapping reference image must include dest, {source:?} got {copied:?}"
            );
            assert!(
                !copied.contains("(https://e.com)") || copied.contains("][ref]"),
                "copy must keep reference dest `[ref]`, {source:?} got {copied:?}"
            );
        }
    }

    #[test]
    fn github_alert_tag_is_dest_chrome() {
        for tag in ["NOTE", "TIP", "IMPORTANT", "WARNING", "CAUTION"] {
            let source = format!("> [!{tag}]\n> body\n");
            let (_doc, engine) = engine_for(&source);
            let open = source.find('[').expect("[");
            let body = source.find("body").expect("body");
            for landed in [
                click_home(&engine, &source, open),
                click_at(&engine, &source, open),
                click_home(&engine, &source, 0),
            ] {
                assert_eq!(
                    landed,
                    body,
                    "Home/click must skip `[!{tag}]` onto the body, got {} {:?}",
                    landed,
                    ch_at(&source, landed)
                );
                assert_ne!(ch_at(&source, landed), '[');
                assert_ne!(ch_at(&source, landed), '>');
            }
        }
        let titled = "> [!TIP] Watch this\n> body\n";
        let (_doc, engine) = engine_for(titled);
        let open = titled.find('[').expect("[");
        let title = titled.find("Watch").expect("title");
        assert_eq!(click_home(&engine, titled, open), title);
        assert_eq!(click_at(&engine, titled, open), title);
        assert_eq!(click_home(&engine, titled, 0), title);
    }

    #[test]
    fn toc_brackets_are_dest_chrome() {
        for source in [
            "[TOC]\n",
            "[toc]\n",
            "[[toc]]\n",
            "[[TOC]]\n",
            "# One\n\n[TOC]\n",
            "# One\n\n[[toc]]\n",
            "> [TOC]\n",
            "> [[toc]]\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let open = source.find('[').expect("[");
            let inner = source[open..]
                .find(|c: char| c.is_ascii_alphabetic())
                .map(|i| open + i)
                .expect("TOC name");
            for landed in [
                click_home(&engine, source, open),
                click_at(&engine, source, open),
                click_home(&engine, source, inner),
            ] {
                assert_eq!(
                    landed,
                    inner,
                    "Home/click must skip TOC brackets onto the name, {source:?} got {} {:?}",
                    landed,
                    ch_at(source, landed)
                );
                assert_ne!(ch_at(source, landed), '[');
                assert_ne!(ch_at(source, landed), ']');
            }
            if source.starts_with('>') {
                assert_ne!(ch_at(source, click_home(&engine, source, 0)), '>');
            }
        }
    }

    #[test]
    fn copy_of_visible_autolink_is_markdown() {
        let source = "see <https://example.com> now\n";
        let (_doc, engine) = engine_for(source);
        let url = source.find("https://example.com").expect("url");
        let copied = engine.markdown_for_selection(source, url..url + "https://example.com".len());
        assert_eq!(
            copied, "<https://example.com>",
            "fully selected autolink copy must include `<>`, got {copied:?}"
        );
        let email = "see <user@example.com> now\n";
        let (_doc, engine) = engine_for(email);
        let addr = email.find("user@example.com").expect("email");
        let copied = engine.markdown_for_selection(email, addr..addr + "user@example.com".len());
        assert_eq!(
            copied, "<user@example.com>",
            "fully selected email autolink copy must include `<>`, got {copied:?}"
        );
        let inner = url + 1..url + "https://example.com".len() - 1;
        assert_eq!(
            engine.markdown_for_selection(source, inner),
            "ttps://example.co",
            "partial selection inside an autolink stays inner text"
        );
    }

    #[test]
    fn copy_of_image_is_markdown() {
        let source = "hello ![cat](a.png) world\n";
        let (_doc, engine) = engine_for(source);
        let img = first_image_range(&engine);
        assert_eq!(
            engine.markdown_for_selection(source, img.clone()),
            "![cat](a.png)"
        );
        assert_eq!(
            engine.markdown_for_selection(source, img.start..img.end),
            "![cat](a.png)"
        );

        let linked = "see [![cat](a.png)](https://e.com) now\n";
        let (_doc, engine) = engine_for(linked);
        let img = first_image_range(&engine);
        let copied = engine.markdown_for_selection(linked, img.clone());
        assert_eq!(
            copied, "[![cat](a.png)](https://e.com)",
            "linked image copy must include wrapping dest, got {copied:?}"
        );
    }

    fn home(engine: &RichEngine, source: &str) -> usize {
        engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right)
    }

    /// Nested / combined GFM the previous passes may have missed.
    #[test]
    fn nested_combined_gfm_caret_does_not_eat_chrome() {
        // Nested task in a list: Home skips inner `- [ ] `, not the outer item.
        let nested = "- [ ] outer\n  - [ ] inner\n";
        let (_doc, engine) = engine_for(nested);
        let i = nested.find("inner").expect("inner");
        let inner_home =
            engine.clamp_raw_prefix(nested, engine.snap_caret(i, Bias::Left), Bias::Right);
        assert_eq!(
            inner_home,
            i,
            "Home/click on nested task must skip `- [ ] ` onto `i`, got {} {:?}",
            inner_home,
            ch_at(nested, inner_home)
        );
        assert_ne!(ch_at(nested, inner_home), '[');
        assert_ne!(ch_at(nested, inner_home), '-');

        // Quoted task: Home skips `> - [ ] `.
        let quoted = "> - [ ] task\n";
        let (_doc, engine) = engine_for(quoted);
        let t = quoted.find("task").expect("task");
        assert_eq!(home(&engine, quoted), t, "quoted task Home must be `t`");

        // Quoted nested task.
        let qn = "> - [ ] outer\n>   - [ ] inner\n";
        let (_doc, engine) = engine_for(qn);
        let qi = qn.find("inner").expect("inner");
        let qhome = engine.clamp_raw_prefix(qn, engine.snap_caret(qi, Bias::Left), Bias::Right);
        assert_eq!(
            qhome,
            qi,
            "quoted nested task must skip `>   - [ ] ` onto `i`, got {} {:?}",
            qhome,
            ch_at(qn, qhome)
        );

        // Image in a list / quote: one caret step, dest is not a home.
        for source in [
            "- ![alt](u.png)\n",
            "> ![alt](u.png)\n",
            "- hello ![alt](u.png)\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let img = first_image_range(&engine);
            assert!(
                source[img.clone()].starts_with("!["),
                "list/quote image range must include `![`, got {:?} in {source:?}",
                &source[img.clone()]
            );
            assert!(
                source[img.clone()].contains("]("),
                "list/quote image range must include dest, got {:?} in {source:?}",
                &source[img.clone()]
            );
            assert_eq!(
                engine.next_caret(source, img.start),
                img.end,
                "Right at list/quote image must skip dest in one step, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, img.end),
                img.start,
                "Left after list/quote image must skip dest in one step, {source:?}"
            );
            let dest = source.find("](").expect("dest");
            let snapped = engine.snap_caret(dest, Bias::Right);
            assert_ne!(
                ch_at(source, snapped),
                '(',
                "caret must not sit in list/quote image dest, {source:?}"
            );
            assert_ne!(ch_at(source, snapped), 'u');
        }

        // Collapsed / shortcut reference links: dest `[]` / `[ref]` / trailing
        // `]` is not a caret home. Trailing `bar` is the next visible run.
        for source in [
            "[foo][] bar\n\n[foo]: https://e.com\n",
            "[foo] bar\n\n[foo]: https://e.com\n",
            "[foo][ref] bar\n\n[ref]: https://e.com\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let f = source.find("foo").expect("foo");
            assert_eq!(
                home(&engine, source),
                f,
                "Home on {source:?} must be the label, got {} {:?}",
                home(&engine, source),
                ch_at(source, home(&engine, source))
            );
            let after_o = f + 3;
            let next = engine.next_caret(source, after_o);
            assert_ne!(
                ch_at(source, next),
                ']',
                "Right at end of shortcut/collapsed label must skip `]` / `[]`, {source:?}"
            );
            assert_ne!(ch_at(source, next), '[');
            let b = source.find("bar").expect("bar");
            assert!(
                next <= b,
                "Right must land on/before `bar`, got {} {:?}",
                next,
                ch_at(source, next)
            );
            let click =
                engine.clamp_raw_prefix(source, engine.snap_caret(f, Bias::Left), Bias::Left);
            assert_eq!(click, f, "click on {source:?} must stay on `f`");
        }

        // Right at the end of a last-in-paragraph collapsed ref must leave
        // dest chrome and reach the next paragraph.
        let lone = "[foo][]\n\nhello\n\n[foo]: https://e.com\n";
        let (_doc, engine) = engine_for(lone);
        let after_o = lone.find("foo").expect("foo") + 3;
        let next = engine.next_caret(lone, after_o);
        assert_ne!(
            ch_at(lone, next),
            ']',
            "Right at end of `[foo][]` must not stick on dest `]`, got {} {:?}",
            next,
            ch_at(lone, next)
        );
        let h = lone.find("hello").expect("hello");
        assert!(
            next <= h,
            "Right must reach the next paragraph, got {} {:?}",
            next,
            ch_at(lone, next)
        );

        // `*hi*` is italic, not a list. Home skips `*`, not a marker.
        let emph = "*hi*\n";
        let (_doc, engine) = engine_for(emph);
        assert!(
            matches!(engine.tree().blocks[0].kind, BlockKind::Paragraph),
            "`*hi*` must import as a paragraph, got {:?}",
            engine.tree().blocks[0].kind
        );
        let h = emph.find('h').expect("h");
        assert_eq!(home(&engine, emph), h, "`*hi*` Home must skip `*` onto `h`");
        assert_ne!(ch_at(emph, home(&engine, emph)), '*');
        assert_eq!(list_marker_on_line("*hi*"), "");

        // Setext underline is not a caret home.
        let setext = "Title\n=====\n";
        let (_doc, engine) = engine_for(setext);
        let e = setext.find("Title").unwrap() + 5;
        assert_ne!(
            ch_at(setext, engine.next_caret(setext, e - 1)),
            '=',
            "Right at end of setext title must not land on `=`"
        );
        let eq = setext.find('=').expect("=");
        for bias in [Bias::Left, Bias::Right] {
            let snapped = engine.snap_caret(eq, bias);
            assert_ne!(
                ch_at(setext, snapped),
                '=',
                "snap on setext underline must leave `=`, bias={bias:?} -> {} {:?}",
                snapped,
                ch_at(setext, snapped)
            );
        }
        let t = setext.find('T').expect("T");
        let down = engine.vertical_caret(setext, t, 1);
        assert_ne!(
            ch_at(setext, down),
            '=',
            "Down from setext title must not land on `=`, got {down} {:?}",
            ch_at(setext, down)
        );

        // Quoted setext underline is not a caret home.
        let qsetext = "> Title\n> =====\n";
        let (_doc, engine) = engine_for(qsetext);
        let eq = qsetext.find('=').expect("=");
        for bias in [Bias::Left, Bias::Right] {
            let snapped = engine.snap_caret(eq, bias);
            assert_ne!(
                ch_at(qsetext, snapped),
                '=',
                "quoted setext snap must leave `=`, bias={bias:?} -> {} {:?}",
                snapped,
                ch_at(qsetext, snapped)
            );
        }

        // Reference image dest (`[]` / `[ref]`) is part of the atomic image.
        for source in [
            "![foo][]\n\n[foo]: u.png\n",
            "![foo][ref]\n\n[ref]: u.png\n",
            "- ![foo][]\n\n[foo]: u.png\n",
            "- ![foo][ref]\n  [ref]: u.png\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let img = first_image_range(&engine);
            assert!(
                source[img.clone()].starts_with("!["),
                "ref image range must include `![`, got {:?} in {source:?}",
                &source[img.clone()]
            );
            assert_eq!(
                engine.next_caret(source, img.start),
                img.end,
                "Right at a reference image must skip dest in one step, {source:?}"
            );
            let dest = source.find(']').expect("]");
            let snapped = engine.snap_caret(dest, Bias::Right);
            assert_ne!(
                ch_at(source, snapped),
                ']',
                "caret must not sit on reference-image dest, {source:?}"
            );
        }

        // HTML-block `<img>` (list / quote / standalone): dest `src=` is not a home.
        for source in [
            "<img src=\"a.png\" alt=\"x\">\n",
            "- <img src=\"a.png\" alt=\"x\">\n",
            "> <img src=\"a.png\" alt=\"x\">\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let img = first_image_range(&engine);
            assert!(
                source[img.clone()].contains("<img"),
                "HTML-block img range, got {:?} in {source:?}",
                &source[img.clone()]
            );
            assert_eq!(
                engine.next_caret(source, img.start),
                img.end,
                "Right at HTML-block `<img>` must skip src dest in one step, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, img.end),
                img.start,
                "Left after HTML-block `<img>` must skip src dest in one step, {source:?}"
            );
            let src = source.find("src=").expect("src");
            let snapped = engine.snap_caret(src, Bias::Right);
            assert_ne!(
                ch_at(source, snapped),
                's',
                "caret must not sit on HTML-block img `src=` dest chrome, {source:?}"
            );
        }

        // Inline HTML `<img>` dest (src=) is not a caret walk.
        let html_img = "hello <img src=\"a.png\" alt=\"x\"> world\n";
        let (_doc, engine) = engine_for(html_img);
        let img = first_image_range(&engine);
        assert!(
            html_img[img.clone()].starts_with("<img"),
            "HTML img range, got {:?}",
            &html_img[img.clone()]
        );
        assert_eq!(
            engine.next_caret(html_img, img.start),
            img.end,
            "Right at HTML `<img>` must skip src dest in one step"
        );
        assert_eq!(engine.prev_caret(html_img, img.end), img.start);
        let src = html_img.find("src=").expect("src");
        let snapped = engine.snap_caret(src, Bias::Right);
        assert_ne!(
            ch_at(html_img, snapped),
            's',
            "caret must not sit on HTML img `src=` dest chrome"
        );
    }

    #[test]
    fn copy_of_table_cell_does_not_include_pipes() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (_doc, engine) = engine_for(source);
        let a = source.find('a').expect("a");
        let copied = engine.markdown_for_selection(source, a..a + 1);
        assert_eq!(copied, "a");
        assert!(
            !copied.contains('|'),
            "cell copy must not include `|`, got {copied:?}"
        );
    }

    fn first_thematic_break_range(engine: &RichEngine) -> Range<usize> {
        fn walk(blocks: &[Block]) -> Option<Range<usize>> {
            for b in blocks {
                if let Some(r) = super::thematic_break_range(b) {
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

    #[test]
    fn thematic_break_is_one_caret_step() {
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
            let (_doc, engine) = engine_for(source);
            let rule = first_thematic_break_range(&engine);
            let slice = &source[rule.clone()];
            assert!(
                slice.contains("---")
                    || slice.contains("***")
                    || slice.contains("___")
                    || slice.contains("* * *")
                    || slice.contains("- - -")
                    || slice.contains("<hr"),
                "thematic-break range, got {slice:?} in {source:?}"
            );
            assert_eq!(
                engine.next_caret(source, rule.start),
                rule.end,
                "Right at a painted rule must skip dashes/tag dest, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, rule.end),
                rule.start,
                "Left after a painted rule must skip dashes/tag dest, {source:?}"
            );
            if rule.end - rule.start > 1 {
                let mid = rule.start + (rule.end - rule.start) / 2;
                let snap_r = engine.snap_caret(mid, Bias::Right);
                let snap_l = engine.snap_caret(mid, Bias::Left);
                assert!(
                    snap_r == rule.start || snap_r == rule.end,
                    "caret must not sit on rule dest chrome, snapR={} {:?} in {source:?}",
                    snap_r,
                    ch_at(source, snap_r)
                );
                assert!(
                    snap_l == rule.start || snap_l == rule.end,
                    "caret must not sit on rule dest chrome, snapL={} {:?} in {source:?}",
                    snap_l,
                    ch_at(source, snap_l)
                );
            }
        }
    }

    fn first_html_break_range(engine: &RichEngine) -> Range<usize> {
        fn walk(blocks: &[Block]) -> Option<Range<usize>> {
            for b in blocks {
                if let Some(r) = super::html_block_break_range(b) {
                    return Some(r);
                }
                for inline in &b.inlines {
                    if let Some(r) = super::atomic_html_break_range(inline) {
                        return Some(r);
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

    fn first_footnote_ref_ranges(engine: &RichEngine) -> (Range<usize>, Range<usize>) {
        fn walk(blocks: &[Block]) -> Option<(Range<usize>, Range<usize>)> {
            for b in blocks {
                for inline in &b.inlines {
                    if let Some(r) = super::footnote_ref_ranges(inline) {
                        return Some(r);
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
    fn html_block_br_is_one_caret_step() {
        for source in ["<br>\n", "<br/>\n", "<br />\n", "- <br>\n", "> <br>\n"] {
            let (_doc, engine) = engine_for(source);
            let br = first_html_break_range(&engine);
            assert!(
                source[br.clone()].contains("<br"),
                "HTML-block br range, got {:?} in {source:?}",
                &source[br.clone()]
            );
            assert_eq!(
                engine.next_caret(source, br.start),
                br.end,
                "Right at HTML-block `<br>` must skip the tag, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, br.end),
                br.start,
                "Left after HTML-block `<br>` must skip the tag, {source:?}"
            );
            if let Some(r) = source.find('r') {
                if r >= br.start && r < br.end {
                    let snap = engine.snap_caret(r, Bias::Right);
                    assert_ne!(
                        ch_at(source, snap),
                        'r',
                        "caret must not sit on HTML-block br `r`, {source:?} snap={snap}"
                    );
                }
            }
        }
    }

    #[test]
    fn inline_br_is_one_caret_step() {
        for source in ["a<br>b\n", "a<br/>b\n", "a<br />b\n"] {
            let (_doc, engine) = engine_for(source);
            let br = first_html_break_range(&engine);
            assert!(
                source[br.clone()].contains("<br"),
                "inline br range, got {:?} in {source:?}",
                &source[br.clone()]
            );
            assert_eq!(
                engine.next_caret(source, br.start),
                br.end,
                "Right at inline `<br>` must skip the tag, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, br.end),
                br.start,
                "Left after inline `<br>` must skip the tag, {source:?}"
            );
            let gt = source.find('>').expect(">");
            if gt >= br.start && gt < br.end {
                let snap = engine.snap_caret(gt, Bias::Left);
                assert_ne!(
                    ch_at(source, snap),
                    '>',
                    "caret must not sit on inline br `>`, {source:?}"
                );
            }
        }
    }

    #[test]
    fn footnote_ref_skips_dest_chrome() {
        for source in [
            "Hello[^1] world\n\n[^1]: the note\n",
            "[^1] hello\n\n[^1]: the note\n",
            "Hello[^note] world\n\n[^note]: the note\n",
            "> Hello[^1]\n\n[^1]: the note\n",
            "Hello[^1] world\n",
            "[^1] hello\n",
            "- Hello[^1]\n",
            "> Hello[^1]\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let (inner, outer) = first_footnote_ref_ranges(&engine);
            assert_eq!(
                source[outer.clone()].chars().next(),
                Some('['),
                "outer must start at `[` in {source:?}"
            );
            assert!(
                source[inner.clone()]
                    .chars()
                    .all(|c| c != '[' && c != ']' && c != '^'),
                "inner must be the label, got {:?} in {source:?}",
                &source[inner.clone()]
            );
            let open = outer.start;
            let caret = source.find('^').expect("^");
            let close = outer.end.saturating_sub(1);
            for (byte, label) in [(open, '['), (caret, '^'), (close, ']')] {
                let snap = engine.snap_caret(byte, Bias::Right);
                assert_ne!(
                    ch_at(source, snap),
                    label,
                    "snap Right must not sit on {label:?} in {source:?}"
                );
                let home = engine.clamp_raw_prefix(source, snap, Bias::Right);
                assert_ne!(
                    ch_at(source, home),
                    label,
                    "Home/click must not sit on {label:?} in {source:?}"
                );
            }
            let mark = inner.start;
            assert_eq!(
                ch_at(source, mark),
                source[inner.clone()].chars().next().unwrap_or('∅'),
                "painted mark is the label start"
            );
            let after = engine.next_caret(source, mark);
            assert!(
                after >= outer.end,
                "Right on the painted mark must skip `]`, got {} {:?} in {source:?}",
                after,
                ch_at(source, after)
            );
            let prev = engine.prev_caret(source, outer.end);
            assert_eq!(
                prev,
                mark,
                "Left after the ref must land on the painted mark, got {} {:?} in {source:?}",
                prev,
                ch_at(source, prev)
            );
            assert_ne!(ch_at(source, prev), '[');
            assert_ne!(ch_at(source, prev), ']');
            assert_ne!(ch_at(source, prev), '^');
        }
    }

    #[test]
    fn footnote_def_marker_on_line_matches_opener_not_refs() {
        assert_eq!(
            super::footnote_def_marker_on_line("[^1]: the note"),
            "[^1]: "
        );
        assert_eq!(super::footnote_def_marker_on_line("[^1]:"), "[^1]:");
        assert_eq!(super::footnote_def_marker_on_line("[^note]: "), "[^note]: ");
        assert_eq!(super::footnote_def_marker_on_line("Hello[^1]"), "");
        assert_eq!(super::footnote_def_marker_on_line("[x]: https://e.com"), "");
        assert_eq!(super::footnote_def_marker_on_line("[^1]"), "");
    }

    #[test]
    fn footnote_def_skips_marker_chrome() {
        for source in [
            "Hello[^1]\n\n[^1]: the note\n",
            "Hello[^note]\n\n[^note]: the note\n",
            "> Hello[^1]\n\n> [^1]: the note\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let note = source.find("the").expect("the");
            let home =
                engine.clamp_raw_prefix(source, engine.snap_caret(note, Bias::Left), Bias::Right);
            assert_eq!(
                home,
                note,
                "Home/click must skip `[^…]: ` onto `t`, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
            let open = source.rfind("[^").expect("def marker");
            let snap_open = engine.snap_caret(open, Bias::Right);
            assert_ne!(
                ch_at(source, snap_open),
                '[',
                "snap on footnote def `[` must skip onto the body, {source:?}"
            );
            assert_ne!(ch_at(source, snap_open), '^');
            assert_ne!(ch_at(source, snap_open), ':');
            let prev = engine.prev_caret(source, note);
            assert_ne!(ch_at(source, prev), '[');
            assert_ne!(ch_at(source, prev), '^');
            assert_ne!(ch_at(source, prev), ':');
        }
    }

    #[test]
    fn empty_footnote_def_is_prefix_home() {
        for source in [
            "Hello[^1]\n\n[^1]: \n",
            "Hello[^1]\n\n[^1]:",
            "Hello[^note]\n\n[^note]: \n",
            "> Hello[^1]\n\n> [^1]: \n",
        ] {
            let (_doc, engine) = engine_for(source);
            let homes = &engine.tree().empty_prefix_homes;
            assert!(
                !homes.is_empty(),
                "empty `[^…]: ` must be an empty-prefix home, {source:?} homes={homes:?}"
            );
            let def_homes: Vec<_> = homes
                .iter()
                .filter(|h| {
                    source
                        .get(h.line.clone())
                        .is_some_and(|line| line.contains("]:"))
                })
                .collect();
            assert!(
                !def_homes.is_empty(),
                "home line must be the footnote def, {source:?} homes={homes:?}"
            );
            for h in def_homes {
                assert_ne!(
                    ch_at(source, h.home),
                    '[',
                    "empty def home must sit after `[^…]: `, {source:?} home={}",
                    h.home
                );
                let open = source.rfind("[^").expect("def");
                let snap = engine.snap_caret(open, Bias::Right);
                assert_eq!(
                    snap, h.home,
                    "snap on empty def `[` must land on the prefix home, {source:?}"
                );
            }
        }
    }

    #[test]
    fn definition_details_marker_on_line_matches_opener() {
        assert_eq!(super::definition_details_marker_on_line(": details"), ": ");
        assert_eq!(super::definition_details_marker_on_line(": "), ": ");
        assert_eq!(super::definition_details_marker_on_line(":"), ":");
        assert_eq!(
            super::definition_details_marker_on_line("  : details"),
            "  : "
        );
        assert_eq!(super::definition_details_marker_on_line("Term"), "");
        assert_eq!(super::definition_details_marker_on_line("[^1]: note"), "");
        assert_eq!(super::definition_details_marker_on_line("hello: world"), "");
    }

    #[test]
    fn definition_details_skips_marker_chrome() {
        for source in [
            "Term\n: details\n",
            "Term\n\n: details\n",
            "> Term\n> : details\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let d = source.find("details").expect("details");
            let home =
                engine.clamp_raw_prefix(source, engine.snap_caret(d, Bias::Left), Bias::Right);
            assert_eq!(
                home,
                d,
                "Home/click must skip `: ` onto `d`, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
            let colon = source.find(": details").expect(":");
            let snap = engine.snap_caret(colon, Bias::Right);
            assert_ne!(
                ch_at(source, snap),
                ':',
                "snap on details `:` must skip onto the body, {source:?}"
            );
            let prev = engine.prev_caret(source, d);
            assert_ne!(ch_at(source, prev), ':');
        }
    }

    #[test]
    fn empty_definition_details_is_prefix_home() {
        for source in ["Term\n: \n", "Term\n: ", "Term\n\n: \n", "> Term\n> : \n"] {
            let (_doc, engine) = engine_for(source);
            let homes = &engine.tree().empty_prefix_homes;
            assert!(
                !homes.is_empty(),
                "empty `: ` must be an empty-prefix home, {source:?} homes={homes:?}"
            );
            let detail_homes: Vec<_> = homes
                .iter()
                .filter(|h| {
                    source.get(h.line.clone()).is_some_and(|line| {
                        let after = line.trim_start_matches(['>', ' ', '\t']);
                        super::definition_details_marker_on_line(after).starts_with(':')
                    })
                })
                .collect();
            assert!(
                !detail_homes.is_empty(),
                "home line must be the details opener, {source:?} homes={homes:?}"
            );
            for h in detail_homes {
                assert_ne!(
                    ch_at(source, h.home),
                    ':',
                    "empty details home must sit after `: `, {source:?} home={}",
                    h.home
                );
                let colon = source.rfind(':').expect(":");
                let snap = engine.snap_caret(colon, Bias::Right);
                assert_eq!(
                    snap, h.home,
                    "snap on empty details `:` must land on the prefix home, {source:?}"
                );
            }
        }
    }

    /// GFM `|` / alignment `|---|` are dest chrome: Home/click skip onto the
    /// painted cell, Left/Right do not sit on a pipe.
    #[test]
    fn snap_caret_skips_table_pipe_chrome() {
        for source in [
            "| a | b |\n|---|---|\n| 1 | 2 |\n",
            "| a |\n|---|\n| 1 |\n",
            "> | a | b |\n> |---|---|\n> | 1 | 2 |\n",
            "| a | b |\n|:--|--:|\n| 1 | 2 |\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let a = source.find('a').expect("a");
            let home = click_home(&engine, source, 0);
            assert_eq!(
                home,
                a,
                "Home/click on a table must skip `|` onto `a`, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
            assert_ne!(ch_at(source, home), '|');
            assert_ne!(ch_at(source, home), '>');
            assert_ne!(ch_at(source, home), '-');

            for (i, _) in source.match_indices('|') {
                let landed = click_home(&engine, source, i);
                assert_ne!(
                    ch_at(source, landed),
                    '|',
                    "click/Home on `|`@{i} must skip dest chrome, {source:?} got {} {:?}",
                    landed,
                    ch_at(source, landed)
                );
                let snap_r = engine.snap_caret(i, Bias::Right);
                assert_ne!(
                    ch_at(source, snap_r),
                    '|',
                    "snap Right on `|`@{i} must not sit on the pipe, {source:?} got {} {:?}",
                    snap_r,
                    ch_at(source, snap_r)
                );
            }

            if let Some(align) = source.find("|---").or_else(|| source.find("|:--")) {
                for byte in align..align + 4 {
                    let ch = ch_at(source, byte);
                    if !matches!(ch, '|' | '-' | ':') {
                        continue;
                    }
                    let snap = engine.snap_caret(byte, Bias::Right);
                    assert_ne!(
                        ch_at(source, snap),
                        '|',
                        "alignment `{ch}` must not be a caret home, {source:?} got {} {:?}",
                        snap,
                        ch_at(source, snap)
                    );
                    assert_ne!(ch_at(source, snap), '-');
                }
            }

            let one = source.find('1').expect("1");
            let row_start = source[..one].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let row_home = click_home(&engine, source, row_start);
            assert_eq!(
                row_home,
                one,
                "Home on a body row must skip `|` onto `1`, {source:?} got {} {:?}",
                row_home,
                ch_at(source, row_home)
            );

            let prev = engine.prev_caret(source, a);
            assert_ne!(
                ch_at(source, prev),
                '|',
                "Left at `a` must not sit on `|`, {source:?} got {} {:?}",
                prev,
                ch_at(source, prev)
            );
            let next = engine.next_caret(source, a + 1);
            assert_ne!(
                ch_at(source, next),
                '|',
                "Right after `a` must not sit on `|`, {source:?} got {} {:?}",
                next,
                ch_at(source, next)
            );
        }
    }

    #[test]
    fn snap_caret_treats_escaped_table_pipe_as_cell_text() {
        let source = "| a\\|b | c |\n|---|---|\n| 1 | 2 |\n";
        let (_doc, engine) = engine_for(source);
        let slash = source.find("\\|").expect("\\|");
        let pipe = slash + 1;
        let cell = engine
            .cell_edit_range(slash, source)
            .expect("escaped pipe cell");
        let click = click_home(&engine, source, pipe);
        assert!(
            cell.start <= click && click <= cell.end,
            "click on escaped \\| must stay in that cell, got {click} {:?}",
            ch_at(source, click)
        );
        let sep = source[pipe + 1..]
            .find('|')
            .map(|i| pipe + 1 + i)
            .expect("sep");
        let landed = click_home(&engine, source, sep);
        assert_ne!(
            ch_at(source, landed),
            '|',
            "unescaped cell boundary must still skip, got {} {:?}",
            landed,
            ch_at(source, landed)
        );
        let align = source.find("|---").expect("align");
        let align_click = click_home(&engine, source, align);
        assert_ne!(ch_at(source, align_click), '|');
        assert_ne!(ch_at(source, align_click), '-');
    }

    #[test]
    fn html_block_comment_is_one_caret_step() {
        for source in [
            "<!-- secret -->\n",
            "- <!-- secret -->\n",
            "> <!-- secret -->\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let comment = first_html_comment_range(&engine);
            assert!(
                source[comment.clone()].contains("<!--"),
                "comment range, got {:?} in {source:?}",
                &source[comment.clone()]
            );
            assert_eq!(
                engine.next_caret(source, comment.start),
                comment.end,
                "Right at an HTML-block comment must skip the whole comment, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, comment.end),
                comment.start,
                "Left after an HTML-block comment must skip the whole comment, {source:?}"
            );
            if let Some(s) = source.find("secret") {
                let snap = engine.snap_caret(s, Bias::Right);
                assert!(
                    snap == comment.start || snap == comment.end,
                    "caret must not sit inside comment dest chrome, {source:?} snap={} {:?}",
                    snap,
                    ch_at(source, snap)
                );
            }
        }
    }

    #[test]
    fn html_block_comments_keep_distinct_source_ranges() {
        let source = "<!-- a -->\n\n<!-- a -->\n";
        let (_doc, engine) = engine_for(source);
        let mut ranges = Vec::new();
        fn walk(blocks: &[Block], out: &mut Vec<Range<usize>>) {
            for b in blocks {
                if let Some(r) = super::html_block_comment_range(b) {
                    out.push(r);
                }
                walk(&b.children, out);
            }
        }
        walk(&engine.tree().blocks, &mut ranges);
        assert_eq!(ranges.len(), 2, "two HTML-block comments, got {ranges:?}");
        assert_ne!(
            ranges[0], ranges[1],
            "duplicate comments must not share a recovered span"
        );
        assert_eq!(&source[ranges[0].clone()], "<!-- a -->");
        assert_eq!(&source[ranges[1].clone()], "<!-- a -->");
        assert_eq!(
            engine.next_caret(source, ranges[0].start),
            ranges[0].end,
            "first comment is one caret step"
        );
        assert_eq!(
            engine.next_caret(source, ranges[1].start),
            ranges[1].end,
            "second comment is one caret step"
        );
    }

    #[test]
    fn html_block_indent_does_not_steal_later_code_literal() {
        let source = "  <!-- foo -->\n\n    <!-- foo -->\n";
        let (_doc, engine) = engine_for(source);
        let comment = first_html_comment_range(&engine);
        assert_eq!(
            &source[comment.clone()],
            "  <!-- foo -->",
            "recovered HTML-block span must be the first line, not the indented code, got {:?}",
            &source[comment]
        );
        assert!(
            comment.end <= source.find("\n\n").expect("blank"),
            "must not overlap the indented code fence"
        );
    }

    fn first_html_comment_range(engine: &RichEngine) -> Range<usize> {
        fn walk(blocks: &[Block]) -> Option<Range<usize>> {
            for b in blocks {
                if let Some(r) = super::html_block_comment_range(b) {
                    return Some(r);
                }
                if let Some(found) = walk(&b.children) {
                    return Some(found);
                }
            }
            None
        }
        walk(&engine.tree().blocks).expect("html comment")
    }

    /// Two-space / backslash hard-break markers are dest chrome like `<br>`:
    /// Left/Right skip them; snap does not sit on a hidden space.
    #[test]
    fn hard_break_marker_is_dest_chrome() {
        for source in [
            "a  \nb\n",
            "a\\\nb\n",
            "> a  \n> b\n",
            "- a  \nb\n",
            "- a  \n  b\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let hard = first_hard_break_range(&engine);
            let marker = &source[hard.clone()];
            assert!(
                marker.contains('\n') && (marker.contains("  ") || marker.contains('\\')),
                "hard break must cover the marker, {source:?} got {marker:?}"
            );
            let a = source.find('a').expect("a");
            let b = source.find('b').expect("b");
            let after_a = a + 1;
            assert_eq!(
                engine.next_caret(source, after_a),
                b,
                "Right after `a` must skip hard-break chrome onto `b`, {source:?} got {} {:?}",
                engine.next_caret(source, after_a),
                ch_at(source, engine.next_caret(source, after_a))
            );
            assert_eq!(
                engine.prev_caret(source, b),
                after_a,
                "Left from `b` must land after `a`, {source:?} got {} {:?}",
                engine.prev_caret(source, b),
                ch_at(source, engine.prev_caret(source, b))
            );
            for bias in [Bias::Left, Bias::Right] {
                if hard.end > hard.start + 1 {
                    let mid = hard.start + 1;
                    let snapped =
                        engine.clamp_raw_prefix(source, engine.snap_caret(mid, bias), bias);
                    assert_ne!(
                        snapped, mid,
                        "snap must not stay on interior hard-break chrome, {source:?} bias={bias:?}"
                    );
                    assert_ne!(
                        ch_at(source, snapped),
                        '>',
                        "snap must skip quote prefix after a hard break, {source:?} bias={bias:?}"
                    );
                }
            }
        }
    }

    fn first_hard_break_range(engine: &RichEngine) -> Range<usize> {
        fn walk(blocks: &[Block]) -> Option<Range<usize>> {
            for b in blocks {
                for inline in &b.inlines {
                    if let Some(r) = super::atomic_hard_break_range(inline) {
                        return Some(r);
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

    /// GFM extended autolinks (`www.`, `https://`, bare email) skip onto the
    /// URL literal, not a bogus `0..1` sourcepos (paragraph start / `>` / `-`).
    #[test]
    fn gfm_extended_autolink_caret_walks_the_url_literal() {
        let cases = [
            ("see www.example.com now\n", "www.example.com"),
            ("see https://example.com now\n", "https://example.com"),
            ("see user@example.com now\n", "user@example.com"),
            ("> www.example.com\n", "www.example.com"),
            ("- www.example.com\n", "www.example.com"),
            ("www.example.com\n", "www.example.com"),
            ("see www.example.com. now\n", "www.example.com"),
            ("> see https://example.com now\n", "https://example.com"),
            ("- user@example.com\n", "user@example.com"),
        ];
        for (source, needle) in cases {
            let (_doc, engine) = engine_for(source);
            let url = source.find(needle).expect(needle);
            let after_url = url + needle.len();
            assert_eq!(&source[url..after_url], needle, "{source:?}");
            for bias in [Bias::Left, Bias::Right] {
                let snapped = engine.clamp_raw_prefix(source, engine.snap_caret(url, bias), bias);
                assert!(
                    snapped >= url && snapped <= after_url,
                    "snap must sit on the URL, {source:?} bias={bias:?} got {} {:?}",
                    snapped,
                    ch_at(source, snapped)
                );
            }
            let first =
                engine.clamp_raw_prefix(source, engine.snap_caret(url, Bias::Right), Bias::Right);
            assert_eq!(
                first,
                url,
                "Home/click on the autolink must be the first URL byte, {source:?} got {} {:?}",
                first,
                ch_at(source, first)
            );
            if source.starts_with("see ") {
                let next = engine.next_caret(source, 3);
                assert_eq!(
                    next,
                    url,
                    "Right from the space after `see` must land on the URL, {source:?} got {} {:?}",
                    next,
                    ch_at(source, next)
                );
            }
            let second = engine.next_caret(source, first);
            assert_eq!(
                second,
                url + 1,
                "Right on a GFM autolink must walk the URL, {source:?} got {} {:?}",
                second,
                ch_at(source, second)
            );
            assert_ne!(
                ch_at(source, second),
                '>',
                "must not sit on quote chrome, {source:?}"
            );
            let back = engine.prev_caret(source, second);
            assert_eq!(
                back, first,
                "Left must reverse Right on the URL, {source:?}"
            );
        }
    }

    #[test]
    fn gfm_extended_autolink_does_not_invent_markdown_dest() {
        let source = "see [www.example.com](https://e.com) now\n";
        let (_doc, engine) = engine_for(source);
        let inner = source.find("www.example.com").expect("label");
        let prev = engine.prev_caret(source, inner);
        assert_ne!(
            ch_at(source, prev),
            '[',
            "markdown `[www](url)` must still skip `[`"
        );
        let end = inner + "www.example.com".len();
        let next = engine.next_caret(source, end);
        assert_ne!(ch_at(source, next), ']');
        assert_ne!(ch_at(source, next), '(');
    }

    /// CommonMark character references skip `amp;` dest chrome: click/Home
    /// land on the painted glyph; Left/Right are one step; code stays literal.
    #[test]
    fn character_reference_is_dest_chrome() {
        let cases = [
            ("A&amp;B\n", "&amp;", 'A', 'B'),
            ("A&lt;B\n", "&lt;", 'A', 'B'),
            ("A&gt;B\n", "&gt;", 'A', 'B'),
            ("A&quot;B\n", "&quot;", 'A', 'B'),
            ("A&#39;B\n", "&#39;", 'A', 'B'),
            ("A&#123;B\n", "&#123;", 'A', 'B'),
            ("A&#x7B;B\n", "&#x7B;", 'A', 'B'),
            ("> A&amp;B\n", "&amp;", 'A', 'B'),
            ("- A&amp;B\n", "&amp;", 'A', 'B'),
            ("[A&amp;B](https://e.com)\n", "&amp;", 'A', 'B'),
            ("| A&amp;B | x |\n| --- | --- |\n", "&amp;", 'A', 'B'),
        ];
        for (source, literal, before, after) in cases {
            let (_doc, engine) = engine_for(source);
            let entity = source.find(literal).expect(literal);
            let amp = entity;
            let after_entity = entity + literal.len();
            let home =
                engine.clamp_raw_prefix(source, engine.snap_caret(amp, Bias::Right), Bias::Right);
            assert_eq!(
                home,
                amp,
                "click/Home on a painted entity must be `&` of {literal}, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
            assert_eq!(
                ch_at(source, home),
                '&',
                "painted-character home must be `&`, {source:?}"
            );
            let a = source.find(before).expect("before");
            let next = engine.next_caret(source, a);
            assert_eq!(
                next,
                amp,
                "Right from the previous character must land on the entity, {source:?} got {} {:?}",
                next,
                ch_at(source, next)
            );
            let skipped = engine.next_caret(source, amp);
            assert_eq!(
                skipped, after_entity,
                "Right on an entity must skip dest chrome onto the next character, {source:?} got {} {:?}",
                skipped,
                ch_at(source, skipped)
            );
            assert_eq!(
                ch_at(source, skipped),
                after,
                "after skipping {literal} must sit on {after}, {source:?}"
            );
            for (i, b) in literal.bytes().enumerate().skip(1) {
                let at = entity + i;
                let snapped =
                    engine.clamp_raw_prefix(source, engine.snap_caret(at, Bias::Left), Bias::Left);
                assert_ne!(
                    snapped, at,
                    "must not sit on hidden entity byte {b:?} in {literal}, {source:?}"
                );
                assert_ne!(
                    ch_at(source, snapped),
                    b as char,
                    "snap must not nibble {literal} dest chrome, {source:?}"
                );
            }
            let back = engine.prev_caret(source, after_entity);
            assert_eq!(
                back,
                amp,
                "Left from the next character must land on the entity, {source:?} got {} {:?}",
                back,
                ch_at(source, back)
            );
        }

        let code = "`A&amp;B`\n";
        let (_doc, engine) = engine_for(code);
        let amp = code.find("&amp;").expect("literal");
        let next = engine.next_caret(code, amp);
        assert_eq!(
            next,
            amp + 1,
            "code spans must walk `&amp;` as literals, got {} {:?}",
            next,
            ch_at(code, next)
        );
        assert_eq!(ch_at(code, next), 'a');
    }

    /// CommonMark backslash escapes skip like character references: `\*`
    /// paints `*`; `\` is the caret home; the escaped char is dest chrome.
    #[test]
    fn backslash_escape_is_dest_chrome() {
        let cases = [
            ("A\\*B\n", '\\', '*', 'A', 'B'),
            ("A\\_B\n", '\\', '_', 'A', 'B'),
            ("A\\[B\n", '\\', '[', 'A', 'B'),
            ("A\\\\B\n", '\\', '\\', 'A', 'B'),
            ("> A\\*B\n", '\\', '*', 'A', 'B'),
            ("- A\\*B\n", '\\', '*', 'A', 'B'),
            ("[A\\*B](https://e.com)\n", '\\', '*', 'A', 'B'),
            ("| A\\*B | x |\n| --- | --- |\n", '\\', '*', 'A', 'B'),
        ];
        for (source, slash_ch, escaped, before, after) in cases {
            let (_doc, engine) = engine_for(source);
            let slash = source.find('\\').expect("slash");
            let escaped_at = slash + 1;
            let after_escape = escaped_at + 1;
            let home =
                engine.clamp_raw_prefix(source, engine.snap_caret(slash, Bias::Right), Bias::Right);
            assert_eq!(
                home,
                slash,
                "click/Home on an escape must be `\\` of {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
            assert_eq!(ch_at(source, home), slash_ch);
            let a = source.find(before).expect("before");
            let next = engine.next_caret(source, a);
            assert_eq!(
                next,
                slash,
                "Right from the previous character must land on `\\`, {source:?} got {} {:?}",
                next,
                ch_at(source, next)
            );
            let skipped = engine.next_caret(source, slash);
            assert_eq!(
                skipped, after_escape,
                "Right on `\\` must skip the escaped glyph onto the next character, {source:?} got {} {:?}",
                skipped,
                ch_at(source, skipped)
            );
            assert_eq!(
                ch_at(source, skipped),
                after,
                "after skipping the escape must sit on {after}, {source:?}"
            );
            let on_escaped = engine.clamp_raw_prefix(
                source,
                engine.snap_caret(escaped_at, Bias::Left),
                Bias::Left,
            );
            assert_ne!(
                on_escaped, escaped_at,
                "must not sit on dest-chrome escaped {escaped:?}, {source:?}"
            );
            let back = engine.prev_caret(source, after_escape);
            assert_eq!(
                back,
                slash,
                "Left from the next character must land on `\\`, {source:?} got {} {:?}",
                back,
                ch_at(source, back)
            );
        }

        let code = "`A\\*B`\n";
        let (_doc, engine) = engine_for(code);
        let slash = code.find('\\').expect("slash");
        let next = engine.next_caret(code, slash);
        assert_eq!(
            next,
            slash + 1,
            "code spans must walk `\\*` as literals, got {} {:?}",
            next,
            ch_at(code, next)
        );
        assert_eq!(ch_at(code, next), '*');
    }

    /// CommonMark HTML-block PI / CDATA close at `?>` / `]]>`, not the first
    /// `>`. They skip as dest chrome like comments (quoted/list too).
    #[test]
    fn html_block_pi_and_cdata_with_inner_gt_are_one_caret_step() {
        for source in [
            "<?php if ($a > $b) echo 1; ?>\n",
            "- <?php if ($a > $b) echo 1; ?>\n",
            "> <?php if ($a > $b) echo 1; ?>\n",
            "<![CDATA[a > b]]>\n",
            "- <![CDATA[a > b]]>\n",
            "> <![CDATA[a > b]]>\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let chrome = first_html_comment_range(&engine);
            let slice = &source[chrome.clone()];
            assert!(
                slice.contains("?>") || slice.contains("]]>"),
                "atomic range must cover the closer, {source:?} got {slice:?}"
            );
            assert!(
                slice.contains('>'),
                "fixture must include an inner `>`, {source:?}"
            );
            assert_eq!(
                engine.next_caret(source, chrome.start),
                chrome.end,
                "Right at PI/CDATA must skip the whole block, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, chrome.end),
                chrome.start,
                "Left after PI/CDATA must skip the whole block, {source:?}"
            );
            if let Some(gt) = source.find(" > ") {
                let snap = engine.snap_caret(gt + 1, Bias::Right);
                assert!(
                    snap == chrome.start || snap == chrome.end,
                    "caret must not sit on inner `>` dest chrome, {source:?} snap={} {:?}",
                    snap,
                    ch_at(source, snap)
                );
            }
        }

        let inline = "hello <?php echo 1 > 0; ?> world\n";
        let (_doc, engine) = engine_for(inline);
        let open = inline.find("<?").expect("<?");
        let home = click_home(&engine, inline, open);
        assert_ne!(
            ch_at(inline, home),
            '<',
            "inline PI Home on `<` must skip dest chrome, got {} {:?}",
            home,
            ch_at(inline, home)
        );
        let prev = engine.prev_caret(inline, inline.find("world").expect("world"));
        assert_ne!(
            ch_at(inline, prev),
            '>',
            "Left at the next word must skip the PI closer, got {} {:?}",
            prev,
            ch_at(inline, prev)
        );
    }

    /// Type-1 `<style>` / `<textarea>` skip tags as dest chrome. Inner CSS /
    /// textarea text (including `>`) is a caret home, not a widget walk.
    #[test]
    fn html_block_style_and_textarea_skip_tags_not_inner() {
        for source in [
            "<style>body { color: red }</style>\n",
            "<style>body > p { color: red }</style>\n",
            "- <style>body { color: red }</style>\n",
            "> <style>body { color: red }</style>\n",
            "<textarea>hello</textarea>\n",
            "<textarea>a > b</textarea>\n",
            "- <textarea>hello</textarea>\n",
            "> <textarea>hello</textarea>\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let block = first_opaque_block(&engine);
            assert!(
                super::html_block_script_range(block).is_none()
                    && super::html_block_comment_range(block).is_none(),
                "style/textarea must not be atomic widgets, {source:?}"
            );
            let inner = ["body", "hello", "a > b"]
                .iter()
                .find_map(|needle| source.find(needle))
                .expect("inner");
            let home = click_home(&engine, source, block.source_range.start);
            assert_eq!(
                home, inner,
                "Home/click on `<style>` / `<textarea>` must skip onto inner source, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
            assert_ne!(ch_at(source, home), '<');
            assert_ne!(ch_at(source, home), '>');
            assert_eq!(
                engine.next_caret(source, block.source_range.start),
                inner,
                "Right at the open tag must skip onto inner source, {source:?}"
            );
            let prev = engine.prev_caret(source, inner);
            assert_ne!(
                ch_at(source, prev),
                '>',
                "Left from inner must not sit on tag `>`, {source:?} prev={prev}"
            );
            assert_ne!(
                ch_at(source, prev),
                '<',
                "Left from inner must not sit on `<`, {source:?} prev={prev}"
            );
            if let Some(gt) = source.find(" > ") {
                let snap = engine.snap_caret(gt + 1, Bias::Right);
                assert_eq!(
                    ch_at(source, snap),
                    '>',
                    "CSS/textarea `>` is inner source, not dest chrome, {source:?} snap={} {:?}",
                    snap,
                    ch_at(source, snap)
                );
            }
        }
    }

    /// Type-1 `<script>` is one caret step: do not walk inner JS.
    #[test]
    fn html_block_script_is_one_caret_step() {
        for source in [
            "<script>alert(1)</script>\n",
            "- <script>alert(1)</script>\n",
            "> <script>alert(1)</script>\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let chrome = first_html_script_range(&engine);
            assert!(
                source[chrome.clone()].contains("<script"),
                "script range, got {:?} in {source:?}",
                &source[chrome.clone()]
            );
            assert_eq!(
                engine.next_caret(source, chrome.start),
                chrome.end,
                "Right at a script block must skip the whole widget, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, chrome.end),
                chrome.start,
                "Left after a script block must skip the whole widget, {source:?}"
            );
            if let Some(js) = source.find("alert") {
                let snap = engine.snap_caret(js, Bias::Right);
                assert!(
                    snap == chrome.start || snap == chrome.end,
                    "caret must not sit inside script dest chrome, {source:?} snap={} {:?}",
                    snap,
                    ch_at(source, snap)
                );
            }
        }
    }

    /// GFM tagfilter Type-6 `<iframe>` / `<title>` / `<xmp>` (and similar
    /// `<noembed>` / `<noframes>` / `<plaintext>`) skip inner like `<script>`.
    #[test]
    fn html_block_tagfilter_is_one_caret_step() {
        for source in [
            "<iframe src=\"https://e.com\"></iframe>\n",
            "<iframe src=\"https://e.com\"><p>nested</p></iframe>\n",
            "- <iframe src=\"https://e.com\"></iframe>\n",
            "> <iframe src=\"https://e.com\"></iframe>\n",
            "<title>Doc title</title>\n",
            "- <title>Doc title</title>\n",
            "> <title>Doc title</title>\n",
            "<xmp>raw <b>html</b></xmp>\n",
            "- <xmp>raw</xmp>\n",
            "> <xmp>raw</xmp>\n",
            "<noembed>fallback</noembed>\n",
            "<noframes>fallback</noframes>\n",
            "<plaintext>raw text\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let chrome = first_html_script_range(&engine);
            let open = [
                "<iframe",
                "<title",
                "<xmp",
                "<noembed",
                "<noframes",
                "<plaintext",
            ]
            .iter()
            .copied()
            .find(|tag| {
                source
                    .to_ascii_lowercase()
                    .contains(&tag.to_ascii_lowercase())
            })
            .expect("open tag");
            assert!(
                source[chrome.clone()]
                    .to_ascii_lowercase()
                    .contains(&open.to_ascii_lowercase()),
                "tagfilter range, got {:?} in {source:?}",
                &source[chrome.clone()]
            );
            assert_eq!(
                engine.next_caret(source, chrome.start),
                chrome.end,
                "Right at a tagfilter block must skip the whole widget, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, chrome.end),
                chrome.start,
                "Left after a tagfilter block must skip the whole widget, {source:?}"
            );
            for needle in ["nested", "Doc title", "raw", "fallback", "e.com"] {
                if let Some(inner) = source.find(needle) {
                    if inner >= chrome.start && inner < chrome.end {
                        let snap = engine.snap_caret(inner, Bias::Right);
                        assert!(
                            snap == chrome.start || snap == chrome.end,
                            "caret must not sit inside tagfilter dest chrome, {source:?} snap={} {:?}",
                            snap,
                            ch_at(source, snap)
                        );
                    }
                }
            }
            let home = click_home(&engine, source, chrome.start);
            assert!(
                home == chrome.start || home == chrome.end,
                "click/Home must not land inside tagfilter inner, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
        }
    }

    /// Type-6 `<details>` is dest-chrome like tagfilter widgets: skip inner
    /// HTML (do not invent a disclosure UI). Inner GFM after a blank line is
    /// a following markdown block, not swallowed.
    #[test]
    fn html_block_details_is_one_caret_step() {
        for source in [
            "<details><summary>Title</summary>body</details>\n",
            "<details>\n<summary>Title</summary>\nhidden\n</details>\n",
            "- <details><summary>Title</summary>body</details>\n",
            "> <details><summary>Title</summary>body</details>\n",
            "> <details>\n> <summary>Title</summary>\n> hidden\n> </details>\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let chrome = first_html_script_range(&engine);
            assert!(
                source[chrome.clone()]
                    .to_ascii_lowercase()
                    .contains("<details"),
                "details range, got {:?} in {source:?}",
                &source[chrome.clone()]
            );
            assert_eq!(
                engine.next_caret(source, chrome.start),
                chrome.end,
                "Right at a details block must skip the whole widget, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, chrome.end),
                chrome.start,
                "Left after a details block must skip the whole widget, {source:?}"
            );
            for needle in ["Title", "body", "hidden", "summary"] {
                if let Some(inner) = source.find(needle) {
                    if inner >= chrome.start && inner < chrome.end {
                        let snap = engine.snap_caret(inner, Bias::Right);
                        assert!(
                            snap == chrome.start || snap == chrome.end,
                            "caret must not sit inside details inner HTML, {source:?} snap={} {:?}",
                            snap,
                            ch_at(source, snap)
                        );
                    }
                }
            }
            let home = click_home(&engine, source, chrome.start);
            assert!(
                home == chrome.start || home == chrome.end,
                "click/Home must not land inside details inner, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
        }

        let split = "<details>\n<summary>Title</summary>\n\n**bold**\n\n</details>\n";
        let (_doc, engine) = engine_for(split);
        let b = split.find("bold").expect("bold");
        let snap = engine.snap_caret(b, Bias::Right);
        assert_eq!(
            ch_at(split, snap),
            'b',
            "markdown after a blank in details must stay a caret home, got {} {:?}",
            snap,
            ch_at(split, snap)
        );
    }

    /// Dangerous HTML (`<video>` / `<dialog>` / `<form>` / `<object>` / …)
    /// is dest-chrome like `<details>`: skip inner, not a player or form UI.
    #[test]
    fn html_block_dangerous_html_is_one_caret_step() {
        for source in [
            "<video src=\"x.mp4\"></video>\n",
            "<video>\nhello\n</video>\n",
            "- <video src=\"x.mp4\"></video>\n",
            "> <video src=\"x.mp4\"></video>\n",
            "<audio src=\"x.mp3\"></audio>\n",
            "<dialog>hello</dialog>\n",
            "> <dialog>hello</dialog>\n",
            "<form action=\"/x\">ok</form>\n",
            "- <form action=\"/x\">ok</form>\n",
            "<object data=\"x\"></object>\n",
            "<math>x^2</math>\n",
            "<canvas>fallback</canvas>\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let chrome = first_html_script_range(&engine);
            let slice = source[chrome.clone()].to_ascii_lowercase();
            assert!(
                slice.contains("<video")
                    || slice.contains("<audio")
                    || slice.contains("<dialog")
                    || slice.contains("<form")
                    || slice.contains("<object")
                    || slice.contains("<math")
                    || slice.contains("<canvas"),
                "dangerous HTML range, got {:?} in {source:?}",
                &source[chrome.clone()]
            );
            assert_eq!(
                engine.next_caret(source, chrome.start),
                chrome.end,
                "Right at dangerous HTML must skip the whole widget, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, chrome.end),
                chrome.start,
                "Left after dangerous HTML must skip the whole widget, {source:?}"
            );
            for needle in ["hello", "ok", "fallback", "x.mp4", "x^2", "action"] {
                if let Some(inner) = source.find(needle) {
                    if inner >= chrome.start && inner < chrome.end {
                        let snap = engine.snap_caret(inner, Bias::Right);
                        assert!(
                            snap == chrome.start || snap == chrome.end,
                            "caret must not sit inside dangerous HTML inner, {source:?} snap={} {:?}",
                            snap,
                            ch_at(source, snap)
                        );
                    }
                }
            }
            let home = click_home(&engine, source, chrome.start);
            assert!(
                home == chrome.start || home == chrome.end,
                "click/Home must not land inside dangerous HTML inner, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
        }
    }

    /// Type-7 `<button>` / `<select>` / `<input>` / `<label>` and Type-6
    /// `<option>` skip as dest-chrome widgets: not a live form UI, caret
    /// does not walk inner (quoted/list/inline/paragraph blobs).
    #[test]
    fn html_block_form_controls_are_one_caret_step() {
        for source in [
            "<button>click</button>\n",
            "<button>\nclick\n</button>\n",
            "hello <button>click</button>\n",
            "- <button>click</button>\n",
            "> <button>click</button>\n",
            "<select><option>a</option></select>\n",
            "> <select><option>a</option></select>\n",
            "- <select><option>a</option></select>\n",
            "<input type=\"text\">\n",
            "hello <input type=\"text\"> world\n",
            "- <input type=\"text\">\n",
            "<label>Name</label>\n",
            "> <label>Name</label>\n",
            "<option>a</option>\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let chrome = first_html_script_range(&engine);
            let slice = source[chrome.clone()].to_ascii_lowercase();
            assert!(
                slice.contains("<button")
                    || slice.contains("<select")
                    || slice.contains("<input")
                    || slice.contains("<label")
                    || slice.contains("<option"),
                "form-control range, got {:?} in {source:?}",
                &source[chrome.clone()]
            );
            assert_eq!(
                engine.next_caret(source, chrome.start),
                chrome.end,
                "Right at a form control must skip the whole widget, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, chrome.end),
                chrome.start,
                "Left after a form control must skip the whole widget, {source:?}"
            );
            for needle in ["click", "Name", "a</option>", "type="] {
                if let Some(inner) = source.find(needle) {
                    if inner >= chrome.start && inner < chrome.end {
                        let snap = engine.snap_caret(inner, Bias::Right);
                        assert!(
                            snap == chrome.start || snap == chrome.end,
                            "caret must not sit inside form-control inner, {source:?} snap={} {:?}",
                            snap,
                            ch_at(source, snap)
                        );
                    }
                }
            }
            let home = click_home(&engine, source, chrome.start);
            assert!(
                home == chrome.start || home == chrome.end,
                "click/Home must not land inside form-control inner, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
        }

        let mixed = "hello <input type=\"text\"> world\n";
        let (_doc, engine) = engine_for(mixed);
        let world = mixed.find("world").expect("world");
        let snap = engine.snap_caret(world, Bias::Right);
        assert_eq!(
            ch_at(mixed, snap),
            'w',
            "void <input> must not swallow following text, got {} {:?}",
            snap,
            ch_at(mixed, snap)
        );
        let hello = mixed.find("hello").expect("hello");
        assert_eq!(
            ch_at(mixed, engine.snap_caret(hello, Bias::Right)),
            'h',
            "text before <input> stays a caret home"
        );
    }

    /// Type-7 `<noscript>` / `<template>` skip as dest-chrome widgets: not a
    /// nested document, caret does not walk inner (quoted/list too).
    #[test]
    fn html_block_noscript_and_template_are_one_caret_step() {
        for source in [
            "<noscript>fallback</noscript>\n",
            "<noscript>\nfallback\n</noscript>\n",
            "- <noscript>fallback</noscript>\n",
            "> <noscript>fallback</noscript>\n",
            "hello <noscript>fallback</noscript>\n",
            "<template><p>slot</p></template>\n",
            "<template>\n<p>slot</p>\n</template>\n",
            "- <template><p>slot</p></template>\n",
            "> <template><p>slot</p></template>\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let chrome = first_html_script_range(&engine);
            let slice = source[chrome.clone()].to_ascii_lowercase();
            assert!(
                slice.contains("<noscript") || slice.contains("<template"),
                "noscript/template range, got {:?} in {source:?}",
                &source[chrome.clone()]
            );
            assert_eq!(
                engine.next_caret(source, chrome.start),
                chrome.end,
                "Right at noscript/template must skip the whole widget, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, chrome.end),
                chrome.start,
                "Left after noscript/template must skip the whole widget, {source:?}"
            );
            for needle in ["fallback", "slot"] {
                if let Some(inner) = source.find(needle) {
                    if inner >= chrome.start && inner < chrome.end {
                        let snap = engine.snap_caret(inner, Bias::Right);
                        assert!(
                            snap == chrome.start || snap == chrome.end,
                            "caret must not sit inside noscript/template inner, {source:?} snap={} {:?}",
                            snap,
                            ch_at(source, snap)
                        );
                    }
                }
            }
            let home = click_home(&engine, source, chrome.start);
            assert!(
                home == chrome.start || home == chrome.end,
                "click/Home must not land inside noscript/template inner, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
        }
    }

    /// Type-6 `<fieldset>` / `<legend>` skip as dest-chrome widgets like
    /// `<details>`: not a live form UI, caret does not walk inner
    /// (quoted/list too). Inner GFM after a blank line stays a following
    /// markdown block.
    #[test]
    fn html_block_fieldset_legend_are_one_caret_step() {
        for source in [
            "<fieldset><legend>Title</legend>body</fieldset>\n",
            "<fieldset>\n<legend>Title</legend>\nhidden\n</fieldset>\n",
            "- <fieldset><legend>Title</legend>body</fieldset>\n",
            "> <fieldset><legend>Title</legend>body</fieldset>\n",
            "> <fieldset>\n> <legend>Title</legend>\n> hidden\n> </fieldset>\n",
            "<legend>Title</legend>\n",
            "hello <fieldset><legend>Title</legend>body</fieldset>\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let chrome = first_html_script_range(&engine);
            let slice = source[chrome.clone()].to_ascii_lowercase();
            assert!(
                slice.contains("<fieldset") || slice.contains("<legend"),
                "fieldset/legend range, got {:?} in {source:?}",
                &source[chrome.clone()]
            );
            assert_eq!(
                engine.next_caret(source, chrome.start),
                chrome.end,
                "Right at fieldset/legend must skip the whole widget, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, chrome.end),
                chrome.start,
                "Left after fieldset/legend must skip the whole widget, {source:?}"
            );
            for needle in ["Title", "body", "hidden"] {
                if let Some(inner) = source.find(needle) {
                    if inner >= chrome.start && inner < chrome.end {
                        let snap = engine.snap_caret(inner, Bias::Right);
                        assert!(
                            snap == chrome.start || snap == chrome.end,
                            "caret must not sit inside fieldset/legend inner, {source:?} snap={} {:?}",
                            snap,
                            ch_at(source, snap)
                        );
                    }
                }
            }
            let home = click_home(&engine, source, chrome.start);
            assert!(
                home == chrome.start || home == chrome.end,
                "click/Home must not land inside fieldset/legend inner, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
        }

        let split = "<fieldset>\n<legend>Title</legend>\n\n**bold**\n\n</fieldset>\n";
        let (_doc, engine) = engine_for(split);
        let b = split.find("bold").expect("bold");
        let snap = engine.snap_caret(b, Bias::Right);
        assert_eq!(
            ch_at(split, snap),
            'b',
            "markdown after a blank in fieldset must stay a caret home, got {} {:?}",
            snap,
            ch_at(split, snap)
        );
    }

    /// Type-7 `<output>` / `<progress>` / `<meter>` skip as dest-chrome
    /// widgets: not a live UI, caret does not walk inner
    /// (quoted/list/inline/paragraph Type-7 blobs).
    #[test]
    fn html_block_output_progress_meter_are_one_caret_step() {
        for source in [
            "<output>42</output>\n",
            "<output>\n42\n</output>\n",
            "hello <output>42</output>\n",
            "- <output>42</output>\n",
            "> <output>42</output>\n",
            "<progress value=\"70\" max=\"100\">70%</progress>\n",
            "> <progress value=\"70\">70%</progress>\n",
            "- <progress value=\"70\">70%</progress>\n",
            "<meter value=\"0.6\">60%</meter>\n",
            "> <meter value=\"0.6\">60%</meter>\n",
            "hello <meter value=\"0.6\">60%</meter>\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let chrome = first_html_script_range(&engine);
            let slice = source[chrome.clone()].to_ascii_lowercase();
            assert!(
                slice.contains("<output")
                    || slice.contains("<progress")
                    || slice.contains("<meter"),
                "output/progress/meter range, got {:?} in {source:?}",
                &source[chrome.clone()]
            );
            assert_eq!(
                engine.next_caret(source, chrome.start),
                chrome.end,
                "Right at output/progress/meter must skip the whole widget, {source:?}"
            );
            assert_eq!(
                engine.prev_caret(source, chrome.end),
                chrome.start,
                "Left after output/progress/meter must skip the whole widget, {source:?}"
            );
            for needle in ["42", "70%", "60%", "value="] {
                if let Some(inner) = source.find(needle) {
                    if inner >= chrome.start && inner < chrome.end {
                        let snap = engine.snap_caret(inner, Bias::Right);
                        assert!(
                            snap == chrome.start || snap == chrome.end,
                            "caret must not sit inside output/progress/meter inner, {source:?} snap={} {:?}",
                            snap,
                            ch_at(source, snap)
                        );
                    }
                }
            }
            let home = click_home(&engine, source, chrome.start);
            assert!(
                home == chrome.start || home == chrome.end,
                "click/Home must not land inside output/progress/meter inner, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
        }

        let mixed = "hello <output>42</output> world\n";
        let (_doc, engine) = engine_for(mixed);
        let world = mixed.find("world").expect("world");
        let snap = engine.snap_caret(world, Bias::Right);
        assert_eq!(
            ch_at(mixed, snap),
            'w',
            "output must not swallow following text, got {} {:?}",
            snap,
            ch_at(mixed, snap)
        );
        let hello = mixed.find("hello").expect("hello");
        assert_eq!(
            ch_at(mixed, engine.snap_caret(hello, Bias::Right)),
            'h',
            "text before <output> stays a caret home"
        );
    }

    /// `<datalist>` outer is not dest-chrome (inner `<option>` is).
    /// `<picture>` / `<summary>` / `<search>` / `<slot>` stay flow.
    #[test]
    fn html_datalist_picture_summary_search_slot_are_not_dest_chrome_widgets() {
        let option_inside = "hello <datalist><option>a</option></datalist>\n";
        let (_doc, engine) = engine_for(option_inside);
        let chrome = first_html_script_range(&engine);
        let slice = &option_inside[chrome.clone()];
        assert!(
            slice.to_ascii_lowercase().contains("<option"),
            "inner option must be the widget, got {slice:?}"
        );
        assert!(
            !slice.to_ascii_lowercase().contains("<datalist"),
            "datalist outer must not be the widget, got {slice:?}"
        );
        let a = option_inside.find(">a<").map(|i| i + 1).expect("option a");
        let snap = engine.snap_caret(a, Bias::Right);
        assert!(
            snap == chrome.start || snap == chrome.end,
            "caret must skip option inner inside datalist, snap={} {:?}",
            snap,
            ch_at(option_inside, snap)
        );
        let hello = option_inside.find("hello").expect("hello");
        assert_eq!(
            ch_at(option_inside, engine.snap_caret(hello, Bias::Right)),
            'h',
            "text before datalist stays a caret home"
        );

        for (source, needle, ch) in [
            ("<datalist>choices</datalist>\n", "choices", 'c'),
            ("<summary>Title</summary>\n", "Title", 'T'),
            ("<search>query</search>\n", "query", 'q'),
            ("<slot>fallback</slot>\n", "fallback", 'f'),
            ("hello <picture>alt</picture>\n", "alt", 'a'),
        ] {
            let (_doc, engine) = engine_for(source);
            let inner = source.find(needle).expect(needle);
            let snap = engine.snap_caret(inner, Bias::Right);
            assert_eq!(
                ch_at(source, snap),
                ch,
                "inner of {source:?} must stay a caret home, got {} {:?}",
                snap,
                ch_at(source, snap)
            );
        }
    }

    fn first_opaque_block(engine: &RichEngine) -> &Block {
        fn walk(blocks: &[Block]) -> Option<&Block> {
            for b in blocks {
                if matches!(b.kind, BlockKind::Opaque { .. }) {
                    return Some(b);
                }
                if let Some(found) = walk(&b.children) {
                    return Some(found);
                }
            }
            None
        }
        walk(&engine.tree().blocks).expect("opaque html")
    }

    fn first_html_script_range(engine: &RichEngine) -> Range<usize> {
        fn walk(blocks: &[Block]) -> Option<Range<usize>> {
            for b in blocks {
                if let Some(r) = super::html_block_script_range(b) {
                    return Some(r);
                }
                if let Some(r) = super::tagfilter_inline_widget_ranges(b).into_iter().next() {
                    return Some(r);
                }
                if let Some(found) = walk(&b.children) {
                    return Some(found);
                }
            }
            None
        }
        walk(&engine.tree().blocks).expect("html widget")
    }

    /// CommonMark indented code: the opening 4 spaces / tab are dest chrome
    /// like fence ticks. Comrak sourcepos drops them; import recovers so
    /// Home/click skip onto the body (quoted / list / tab too).
    #[test]
    fn snap_caret_skips_indented_code_indent() {
        for source in [
            "    indented\n",
            "\tindented\n",
            ">     indented\n",
            "    line1\n    line2\n",
        ] {
            let (_doc, engine) = engine_for(source);
            let body = source
                .find("indented")
                .or_else(|| source.find("line1"))
                .expect("body");
            let home = click_home(&engine, source, 0);
            assert_eq!(
                home,
                body,
                "Home/click must skip indented-code indent onto the body, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
            assert_ne!(ch_at(source, home), ' ');
            assert_ne!(ch_at(source, home), '\t');
            assert_ne!(ch_at(source, home), '>');
            if let Some(l2) = source.find("line2") {
                let next = engine.next_caret(source, source.find("line1").expect("line1") + 5);
                let onto = if ch_at(source, next) == 'l' {
                    next
                } else {
                    click_home(&engine, source, next)
                };
                assert_eq!(
                    onto, l2,
                    "Right at the wrap must skip continuation indent onto `l`, {source:?} got {} {:?}",
                    onto,
                    ch_at(source, onto)
                );
                assert_ne!(ch_at(source, onto), ' ');
            }
        }

        let nested = "- item\n\n    nested\n";
        let (_doc, engine) = engine_for(nested);
        let body = nested.find("nested").expect("nested");
        let indent = nested[..body].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let landed = click_home(&engine, nested, indent);
        assert_eq!(
            landed,
            body,
            "click on list-following indent must skip onto `nested`, got {} {:?}",
            landed,
            ch_at(nested, landed)
        );
        assert_ne!(ch_at(nested, landed), ' ');
        assert_ne!(ch_at(nested, landed), '-');
    }

    /// CommonMark 0–3 spaces before ATX `#`, a setext title, a fence, or a
    /// thematic break are dest chrome like list-marker padding. Comrak
    /// sourcepos starts at the marker / first title byte; import recovers so
    /// Home/click skip onto Title / body / first `-`. Quoted `>  # Title`
    /// keep `>`. Four spaces stay indented code.
    #[test]
    fn snap_caret_skips_cm_opening_indent() {
        for (source, needle) in [
            (" # Title\n", "Title"),
            ("  # Title\n", "Title"),
            ("   # Title\n", "Title"),
            (" ```\nfoo\n```\n", "foo"),
            ("  ```\n  foo\n  ```\n", "foo"),
            (" ---\n", "---"),
            ("  ***\n", "***"),
            ("   ___\n", "___"),
            (">  # Title\n", "Title"),
            (">  ```\n>  foo\n>  ```\n", "foo"),
            (">  ---\n", "---"),
            (" Title\n ===\n", "Title"),
            ("  Title\n  ---\n", "Title"),
        ] {
            let (_doc, engine) = engine_for(source);
            let body = source.find(needle).expect(needle);
            let home = click_home(&engine, source, 0);
            assert_eq!(
                home,
                body,
                "Home must skip opening indent onto {needle:?}, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
            assert_ne!(
                ch_at(source, home),
                ' ',
                "Home must not sit on opening indent, {source:?}"
            );
            assert_ne!(
                ch_at(source, home),
                '>',
                "Home must not sit on quote chrome, {source:?}"
            );
            let click = click_home(&engine, source, body);
            assert_eq!(
                click,
                body,
                "click on body must stay on {needle:?}, {source:?} got {} {:?}",
                click,
                ch_at(source, click)
            );
        }

        let four = "    # Title\n";
        let (_doc, engine) = engine_for(four);
        match &engine.tree().blocks[0].kind {
            BlockKind::CodeBlock { fence: None, .. } => {}
            other => panic!("four spaces must stay indented code, got {other:?}"),
        }
        let hash = four.find('#').expect("#");
        let home = click_home(&engine, four, 0);
        assert_eq!(
            home,
            hash,
            "four-space `    # Title` is indented code; Home skips onto `#`, got {} {:?}",
            home,
            ch_at(four, home)
        );
        assert_ne!(ch_at(four, home), ' ');
    }

    fn tree_has_table(engine: &RichEngine) -> bool {
        fn walk(blocks: &[Block]) -> bool {
            blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Table { .. }) || walk(&b.children))
        }
        walk(&engine.tree().blocks)
    }

    /// GFM tables may omit the leading/trailing `|` (`foo|bar` / `---|---` /
    /// `baz|bim`). The pipe is the left cell's exclusive end, so it used to
    /// be a caret stop. Home/click/Left/Right skip it like `| a | b |`.
    #[test]
    fn snap_caret_skips_compact_gfm_table_pipe_chrome() {
        for source in [
            "foo | bar\n--- | ---\nbaz | bim\n",
            "foo|bar\n---|---\nbaz|bim\n",
            "| abc | defghi\n:-: | -----------:\nbar | baz\n",
            "> foo|bar\n> ---|---\n> baz|bim\n",
        ] {
            let (_doc, engine) = engine_for(source);
            assert!(
                tree_has_table(&engine),
                "must import as a GFM table, {source:?}"
            );
            let first = source
                .find("foo")
                .or_else(|| source.find("abc"))
                .expect("first cell");
            let home = click_home(&engine, source, 0);
            assert_eq!(
                home, first,
                "Home/click on a compact table must skip onto the first cell, {source:?} got {} {:?}",
                home,
                ch_at(source, home)
            );
            assert_ne!(ch_at(source, home), '|');
            assert_ne!(ch_at(source, home), '>');
            assert_ne!(ch_at(source, home), '-');

            for (i, _) in source.match_indices('|') {
                let landed = click_home(&engine, source, i);
                assert_ne!(
                    ch_at(source, landed),
                    '|',
                    "click/Home on `|`@{i} must skip dest chrome, {source:?} got {} {:?}",
                    landed,
                    ch_at(source, landed)
                );
                let snap_r = engine.snap_caret(i, Bias::Right);
                assert_ne!(
                    ch_at(source, snap_r),
                    '|',
                    "snap Right on `|`@{i} must not sit on the pipe, {source:?} got {} {:?}",
                    snap_r,
                    ch_at(source, snap_r)
                );
            }

            if let Some(dash) = source.find("---").or_else(|| source.find(":-:")) {
                let landed = click_home(&engine, source, dash);
                assert_ne!(
                    ch_at(source, landed),
                    '-',
                    "click on alignment dashes must skip dest chrome, {source:?} got {} {:?}",
                    landed,
                    ch_at(source, landed)
                );
                assert_ne!(ch_at(source, landed), '|');
                assert_ne!(ch_at(source, landed), ':');
            }

            let second = source
                .find("bar")
                .or_else(|| source.find("defghi"))
                .expect("second cell");
            let mut at = first;
            let mut hops = 0;
            while at < second && hops < 16 {
                let next = engine.next_caret(source, at);
                assert_ne!(
                    ch_at(source, next),
                    '|',
                    "Right must not sit on `|`, {source:?} from {} {:?} to {} {:?}",
                    at,
                    ch_at(source, at),
                    next,
                    ch_at(source, next)
                );
                assert!(
                    next > at,
                    "Right must advance toward the next cell, {source:?} stuck at {} {:?}",
                    next,
                    ch_at(source, next)
                );
                at = next;
                hops += 1;
            }
            assert_eq!(
                at,
                second,
                "Right from the first cell must reach the next cell, {source:?} got {} {:?}",
                at,
                ch_at(source, at)
            );
            let prev = engine.prev_caret(source, second);
            assert_ne!(
                ch_at(source, prev),
                '|',
                "Left at the second cell must not sit on `|`, {source:?} got {} {:?}",
                prev,
                ch_at(source, prev)
            );
        }
    }
}
