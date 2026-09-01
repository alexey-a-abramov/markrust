// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The engine keeps a [`RichTree`] in sync with a [`Document`] and answers
//! the view layer's position questions: which block a byte lives in, where a
//! caret may legally sit in WYSIWYG mode, and how the tree maps to source
//! lines. This is the contract the GPUI view consumes.

use std::ops::Range;

use crate::document::Document;

use super::import::import_markdown;
use super::tree::{
    math_delim_width, wiki_visible_range, Block, BlockKind, IdGen, Inline, LinkAttrs, MarkSet,
    NodeId, PrefixBlank, RichTree,
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
            BlockKind::CodeBlock { .. } | BlockKind::Opaque { .. } | BlockKind::Alert { .. } => {
                true
            }
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
            inline_ranges(cell)
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
        let ranges = inline_ranges(block);
        if ranges.is_empty() {
            return block.source_range.start;
        }
        for r in &ranges {
            if r.start <= byte && byte <= r.end {
                if range_is_atomic_image(block, r) && byte > r.start && byte < r.end {
                    return match bias {
                        Bias::Left => r.start,
                        Bias::Right => r.end,
                    };
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
                .unwrap_or_else(|| ranges.last().unwrap().end),
        }
    }

    /// One visible-grapheme step left from `byte`, skipping delimiter gaps
    /// between runs and treating backslash escapes as atomic. A blank gap
    /// between blocks is one stop (extra unused newlines are not extra steps).
    /// Quote markers, list markers, and HTML tags are not caret stops
    /// (same prefixes click/IME skip). Empty `> ` / `- ` lines are one stop.
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
        let ranges = inline_ranges(block);
        let Some(idx) = ranges.iter().position(|r| r.start <= byte && byte <= r.end) else {
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
                        let pranges = inline_ranges(pb);
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
                            let pranges = inline_ranges(pb);
                            pranges.last().map(|r| r.end).unwrap_or(pb.source_range.end)
                        }
                        None => byte,
                    };
                }
            }
        }
        clamped
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
        let ranges = inline_ranges(block);
        let Some(idx) = ranges.iter().position(|r| r.start <= byte && byte <= r.end) else {
            return byte;
        };
        let stepped = if byte < ranges[idx].end {
            step_right_caret(source, block, byte, &ranges[idx])
        } else if idx + 1 < ranges.len() {
            let next = &ranges[idx + 1];
            if range_is_atomic_image(block, next) && byte <= next.start {
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
                        let nranges = inline_ranges(nb);
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
        self.clamp_raw_prefix(source, stepped, Bias::Right)
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
        let Some(id) = self.block_at(byte) else {
            return byte;
        };
        let Some(block) = self.block(id) else {
            return byte;
        };
        let body = raw_body_range(block, source);
        let mut at = byte.clamp(body.start, body.end);
        let prefix = raw_container_prefix(source, block);
        if !prefix.is_empty() {
            let ls = source_line_start(source, at);
            let le = source_line_end_exclusive(source, at);
            let line = &source[ls..le];
            let skip = skip_line_prefix(line, &prefix);
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
        if matches!(block.kind, BlockKind::Opaque { .. }) {
            at = clamp_html_tag(source, at, body, bias);
        }
        let skipped = self.skip_inline_delimiter_chrome(source, at, bias);
        if skipped != at {
            return self.clamp_raw_prefix(source, skipped, bias);
        }
        at
    }

    /// `[` / `](url)` / `**` / ticks / autolink `<>` adjacent to a visible
    /// run, including wrapping dest around a linked image. `inner.end`
    /// (caret after the last visible letter) is not skipped so typing still
    /// extends the label / marked text.
    fn skip_inline_delimiter_chrome(&self, source: &str, byte: usize, bias: Bias) -> usize {
        let mut at = byte.min(source.len());
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
        let mut found: Option<(Range<usize>, Range<usize>)> = None;
        walk_inline_inner_outer(&self.tree.blocks, source, &mut |inner, outer| {
            if inner.start == outer.start && inner.end == outer.end {
                return;
            }
            if (byte >= outer.start && byte < inner.start) || (byte > inner.end && byte < outer.end)
            {
                found = Some((inner, outer));
            }
        });
        let Some((inner, outer)) = found else {
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
                    if outer.start == 0 {
                        inner.start
                    } else {
                        outer.start - 1
                    }
                } else {
                    inner.end
                }
            }
        }
    }

    /// True when `byte` is markdown chrome around a visible run (`[`, `](url)`,
    /// `**`, ticks, autolink `<>`). Includes the byte at `inner.end` (`]`) so
    /// Backspace/Delete do not nibble dest / closers.
    pub(crate) fn byte_is_inline_chrome(&self, source: &str, byte: usize) -> bool {
        let mut hit = false;
        walk_inline_inner_outer(&self.tree.blocks, source, &mut |inner, outer| {
            if inner.start == outer.start && inner.end == outer.end {
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

/// Byte ranges of caret-valid inline content within a leaf block.
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

/// Byte ranges of caret-valid inline content within a leaf block. Quote and
/// list containers with no inlines of their own use their descendants so
/// `>` / `- ` are not caret homes (Home / snap land on painted body).
fn inline_ranges(block: &Block) -> Vec<Range<usize>> {
    match &block.kind {
        BlockKind::CodeBlock { .. } => {
            if let Some(Inline::Run { source_range, .. }) = block.inlines.first() {
                vec![source_range.clone()]
            } else {
                vec![block.source_range.clone()]
            }
        }
        BlockKind::Opaque { .. } => vec![block.source_range.clone()],
        BlockKind::Alert { chrome_range, .. } => {
            let mut out = Vec::new();
            if chrome_range.start < chrome_range.end {
                out.push(chrome_range.clone());
            }
            for child in &block.children {
                out.extend(inline_ranges(child));
            }
            if out.is_empty() {
                out.push(block.source_range.clone());
            }
            out
        }
        BlockKind::Table { .. } | BlockKind::TableRow { .. } => {
            vec![block.source_range.clone()]
        }
        _ if block.inlines.is_empty() && !block.children.is_empty() => {
            let mut out = Vec::new();
            for child in &block.children {
                out.extend(inline_ranges(child));
            }
            if out.is_empty() {
                out.push(block.source_range.clone());
            }
            out
        }
        _ => leaf_inline_ranges(block),
    }
}

/// Markdown `![alt](url)` and safe HTML `<img>` are one caret/selection step
/// (pixels in WYSIWYG, not `!` / `[` / `)`).
fn atomic_image_range(inline: &Inline) -> Option<Range<usize>> {
    match inline {
        Inline::Image { source_range, .. } => Some(source_range.clone()),
        Inline::OpaqueInline {
            raw, source_range, ..
        } if crate::html_visual::html_inline_image(raw).is_some() => Some(source_range.clone()),
        _ => None,
    }
}

fn range_is_atomic_image(block: &Block, range: &Range<usize>) -> bool {
    fn walk(block: &Block, range: &Range<usize>) -> bool {
        if block
            .inlines
            .iter()
            .any(|inline| atomic_image_range(inline).as_ref() == Some(range))
        {
            return true;
        }
        block.children.iter().any(|child| walk(child, range))
    }
    walk(block, range)
}

fn step_left_caret(source: &str, block: &Block, range: &Range<usize>, byte: usize) -> usize {
    if range_is_atomic_image(block, range) && byte > range.start {
        return range.start;
    }
    step_left_in_slice(source, range.start, byte)
}

fn step_right_caret(source: &str, block: &Block, byte: usize, range: &Range<usize>) -> usize {
    if range_is_atomic_image(block, range) && byte < range.end {
        return range.end;
    }
    step_right_in_slice(source, byte, range.end)
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

/// Editable inner range: fence body between ticks, else the whole raw block.
pub(crate) fn raw_body_range(block: &Block, source: &str) -> Range<usize> {
    match &block.kind {
        BlockKind::CodeBlock { fence: Some(_), .. } => block.code_body_range(source),
        _ => block.source_range.clone(),
    }
}

/// Quote markers, list marker (`- ` / `1. ` / task), and continuation indent
/// of a block's opening line. Click/IME/arrows skip this prefix; Tab/Enter
/// keep it on fences. Unquoted paragraphs have an empty prefix (1:1).
pub(crate) fn raw_container_prefix(source: &str, block: &Block) -> String {
    let start = block.source_range.start.min(source.len());
    let line_start = source_line_start(source, start);
    let line_end = source_line_end_exclusive(source, start);
    let line = &source[line_start..line_end];
    let quote = quote_marker_on_line(line);
    let after = &line[quote.len()..];
    let marker = list_marker_on_line(after);
    let rest = &after[marker.len()..];
    let indent = rest
        .bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .count();
    format!("{quote}{marker}{}", &rest[..indent])
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
fn list_marker_on_line(line: &str) -> &str {
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
        return "";
    }
    let mut take = indent_len + marker_len;
    let after = line.get(take..).unwrap_or("");
    if after.starts_with("[ ]") || after.starts_with("[x]") || after.starts_with("[X]") {
        take = (take + 4).min(line.len());
    }
    &line[..take.min(line.len())]
}

/// Quote markers plus list marker (`- ` / `1. ` / task), without extra
/// body indent. Empty `> ` / `- ` lines use this as the caret skip width.
fn quote_list_prefix_on_line(line: &str) -> &str {
    let quote = quote_marker_on_line(line);
    let marker = list_marker_on_line(&line[quote.len()..]);
    &line[..quote.len() + marker.len()]
}

/// Bytes to skip at the start of `line` so click/arrows land on painted
/// body. Opening-line prefix (`- `, `> `) on matching lines, or GFM
/// continuation indent (`  world` after `- hello`).
fn skip_line_prefix(line: &str, prefix: &str) -> usize {
    if prefix.is_empty() {
        return 0;
    }
    if line.starts_with(prefix) {
        return prefix.len();
    }
    let quote = quote_marker_on_line(line);
    let rest = &line[quote.len()..];
    let budget = prefix.len().saturating_sub(quote.len());
    let indent = rest
        .bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .count()
        .min(budget);
    quote.len() + indent
}

/// Empty `> ` / `- ` / `1. ` / task lines that are not fenced/HTML body.
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
            if prefix_blank_is_quote_or_list(tree, home) {
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
fn html_tag_bounds(source: &str, offset: usize, body: Range<usize>) -> Option<Range<usize>> {
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
    map_visible_skipping_prefix(
        source,
        body,
        &raw_container_prefix(source, block),
        painted_len,
    )
}

/// Map painted body bytes onto `source[body]`, skipping `prefix` at the start
/// of each line (newlines stay in the painted stream).
fn map_visible_skipping_prefix(
    source: &str,
    body: Range<usize>,
    prefix: &str,
    painted_len: usize,
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
        let skip = skip_line_prefix(line, prefix).min(nl);
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

/// Inline-run sources of a leaf block (delimiter gaps are not caret homes).
fn leaf_inline_ranges(block: &Block) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    for inline in &block.inlines {
        match inline {
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
                if !crate::html_visual::opaque_inline_is_caret_chrome(raw) {
                    out.push(source_range.clone());
                }
            }
            Inline::SoftBreak { .. } | Inline::HardBreak { .. } => {
                // Not extra caret homes. WYSIWYG maps the painted space / newline
                // onto `source_range.start`, which sits at the previous run's end.
            }
        }
    }
    if out.is_empty() {
        out.push(block.source_range.clone());
    }
    out
}

/// Drop `|` separators that comrak sometimes includes at a cell's edges.
fn trim_cell_pipes(source: &str, range: Range<usize>) -> Range<usize> {
    let bytes = source.as_bytes();
    let mut start = range.start.min(source.len());
    let mut end = range.end.min(source.len());
    while start < end && bytes[start] == b'|' {
        start += 1;
    }
    while end > start && bytes[end - 1] == b'|' {
        end -= 1;
    }
    start..end
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
    expand_block_chrome_for_copy(&tree.blocks, start..end, &mut start, &mut end);
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
    expand_block_chrome_for_copy(&tree.blocks, selected.clone(), &mut start, &mut end);
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
                            let mut wrapped = img;
                            if let Some(link) = link.as_ref() {
                                wrapped = expand_link_chrome(source, wrapped, link);
                            }
                            *start = (*start).min(wrapped.start);
                            *end = (*end).max(wrapped.end);
                        }
                    }
                }
                Inline::OpaqueInline { .. } => {
                    if let Some(img) = atomic_image_range(inline) {
                        if selected.start < img.end && selected.end > img.start {
                            *start = (*start).min(img.start);
                            *end = (*end).max(img.end);
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
                    let w = math_delim_width(*display);
                    let vis_start = source_range.start.saturating_add(w);
                    let vis_end = source_range.end.saturating_sub(w).max(vis_start);
                    if selected.start <= vis_start && selected.end >= vis_end {
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
                    source_range,
                    marks,
                    link,
                    ..
                } => {
                    if selected.start <= source_range.start && selected.end >= source_range.end {
                        let mut wrapped = if marks.is_empty() {
                            source_range.clone()
                        } else {
                            expand_mark_delimiters(source, block, source_range)
                        };
                        if let Some(link) = link {
                            wrapped = expand_link_chrome(source, wrapped, link);
                        }
                        *start = (*start).min(wrapped.start);
                        *end = (*end).max(wrapped.end);
                    }
                }
                Inline::SoftBreak { .. } | Inline::HardBreak { .. } => {}
            }
        }
        expand_inlines_for_copy(&block.children, source, selected.clone(), start, end);
    }
}

fn expand_block_chrome_for_copy(
    blocks: &[Block],
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
            expand_block_chrome_for_copy(&block.children, selected.clone(), start, _end);
            continue;
        }
        let ranges = inline_ranges(block);
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
        expand_block_chrome_for_copy(&block.children, selected.clone(), start, _end);
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

fn expand_around_link(source: &str, mut range: Range<usize>) -> Range<usize> {
    if range.start > 0 && source.as_bytes()[range.start - 1] == b'[' {
        range.start -= 1;
    }
    if range.end < source.len() && source.as_bytes()[range.end] == b']' {
        range.end += 1;
        if range.end < source.len() && source.as_bytes()[range.end] == b'(' {
            range.end += 1;
            let mut depth = 1i32;
            while range.end < source.len() && depth > 0 {
                match source.as_bytes()[range.end] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                range.end += 1;
            }
        } else if range.end < source.len() && source.as_bytes()[range.end] == b'[' {
            // GFM reference: `[label][ref]` / collapsed `[label][]`.
            let open = range.end;
            range.end += 1;
            while range.end < source.len() && source.as_bytes()[range.end] != b']' {
                range.end += 1;
            }
            if range.end < source.len() && source.as_bytes()[range.end] == b']' {
                range.end += 1;
            } else {
                range.end = open;
            }
        }
    }
    range
}

fn expand_around_autolink(source: &str, mut range: Range<usize>) -> Range<usize> {
    if range.start > 0
        && source.as_bytes()[range.start - 1] == b'<'
        && range.end < source.len()
        && source.as_bytes()[range.end] == b'>'
    {
        range.start -= 1;
        range.end += 1;
    }
    range
}

/// `<>` around an autolink, else `[…](url)` / `[…][ref]`. Email autolinks
/// often have `autolink: false` (comrak child text is the address without
/// `mailto:`); wrapping `<>` is still dest chrome.
fn expand_link_chrome(source: &str, range: Range<usize>, link: &LinkAttrs) -> Range<usize> {
    let auto = expand_around_autolink(source, range.clone());
    if link.autolink || auto != range {
        auto
    } else {
        expand_around_link(source, range)
    }
}

fn inline_inner_outer(
    source: &str,
    block: &Block,
    inline: &Inline,
) -> Option<(Range<usize>, Range<usize>)> {
    match inline {
        Inline::Run {
            source_range,
            marks,
            link,
            ..
        } => {
            let inner = source_range.clone();
            let mut outer = inner.clone();
            if !marks.is_empty() {
                outer = expand_mark_delimiters(source, block, &inner);
            }
            if let Some(link) = link {
                outer = expand_link_chrome(source, outer, link);
            }
            Some((inner, outer))
        }
        Inline::Image {
            source_range,
            marks,
            link,
            ..
        } => {
            let inner = source_range.clone();
            let mut outer = inner.clone();
            if !marks.is_empty() {
                outer = expand_mark_delimiters(source, block, &inner);
            }
            if let Some(link) = link {
                outer = expand_link_chrome(source, outer, link);
            }
            Some((inner, outer))
        }
        _ => None,
    }
}

fn walk_inline_inner_outer(
    blocks: &[Block],
    source: &str,
    f: &mut impl FnMut(Range<usize>, Range<usize>),
) {
    for block in blocks {
        for inline in &block.inlines {
            if let Some((inner, outer)) = inline_inner_outer(source, block, inline) {
                f(inner, outer);
            }
        }
        walk_inline_inner_outer(&block.children, source, f);
    }
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

/// Step one grapheme-ish unit left within [start, byte); backslash escapes
/// ("\\X") are atomic.
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

fn step_left_in_slice(source: &str, start: usize, byte: usize) -> usize {
    let slice = &source[start..byte];
    let Some(last) = slice.char_indices().last() else {
        return start;
    };
    let mut pos = start + last.0;
    if pos > start && source.as_bytes().get(pos - 1) == Some(&b'\\') {
        pos -= 1;
    }
    pos
}

/// Step one grapheme-ish unit right within (byte, end].
pub(crate) fn step_right_in_slice(source: &str, byte: usize, end: usize) -> usize {
    let slice = &source[byte..end];
    let mut it = slice.char_indices();
    let Some((_, first)) = it.next() else {
        return end;
    };
    let mut adv = first.len_utf8();
    if first == '\\' {
        if let Some((_, c2)) = it.next() {
            adv += c2.len_utf8();
        }
    }
    (byte + adv).min(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    fn engine_for(source: &str) -> (Document, RichEngine) {
        let doc = Document::new(source);
        let mut engine = RichEngine::new();
        engine.sync(&doc);
        (doc, engine)
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
    fn snap_caret_skips_inline_html_tags() {
        let source = "hello <b>bold</b>!\n";
        let (_doc, engine) = engine_for(source);
        let tag = source.find("<b>").unwrap();
        assert_eq!(engine.snap_caret(tag + 1, Bias::Right), tag + 3);
        let close = source.find("</b>").unwrap();
        assert_eq!(engine.snap_caret(close + 1, Bias::Left), close);
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
    fn snap_caret_sits_on_alert_tag() {
        let source = "> [!NOTE]\n> body\n";
        let (_doc, engine) = engine_for(source);
        let tag = source.find("[!NOTE]").unwrap();
        assert_eq!(engine.snap_caret(tag, Bias::Right), tag);
        assert_eq!(engine.snap_caret(tag + 3, Bias::Left), tag + 3);
        assert!(engine.in_raw_context(tag));
        let body = source.find("body").unwrap();
        assert_eq!(engine.snap_caret(body, Bias::Right), body);
        assert!(!engine.in_raw_context(body));
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

    fn first_image_range(engine: &RichEngine) -> Range<usize> {
        fn walk(blocks: &[Block]) -> Option<Range<usize>> {
            for b in blocks {
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

    #[test]
    fn document_home_skips_frontmatter_yaml() {
        let source = "---\ntitle: Hello\n---\n\n# Body\n";
        let (_doc, engine) = engine_for(source);
        let fm_end = frontmatter_body_start(engine.tree());
        let home = engine.clamp_raw_prefix(source, engine.snap_caret(0, Bias::Right), Bias::Right);
        assert!(
            home >= fm_end,
            "Cmd-Up / document start must sit after YAML, got {home} fm_end={fm_end}"
        );
        let body = source.find("Body").expect("Body");
        assert!(
            home <= body,
            "document start must not overshoot `# Body`, got {home} body={body}"
        );
        let title = source.find("Hello").expect("title");
        assert!(
            home > title,
            "document start must not land in the YAML title, home={home} title={title}"
        );
        let end = engine.clamp_raw_prefix(
            source,
            engine.snap_caret(source.len(), Bias::Left),
            Bias::Left,
        );
        assert!(end >= body, "document end must stay in the body, got {end}");
    }
}
