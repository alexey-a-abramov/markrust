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
use super::tree::{Block, BlockKind, IdGen, Inline, MarkSet, NodeId, RichTree};

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

    /// Deepest leaf block containing `byte` (falls back to the nearest block).
    pub fn block_at(&self, byte: usize) -> Option<NodeId> {
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
        descend(&self.tree.blocks, byte).or_else(|| {
            // Nearest block: last one starting before `byte`, else first.
            let mut prev = None;
            for b in &self.tree.blocks {
                if b.source_range.start <= byte {
                    prev = Some(b.id);
                }
            }
            prev.or_else(|| self.tree.blocks.first().map(|b| b.id))
        })
    }

    /// Valid WYSIWYG caret positions in a leaf block are the bytes covered by
    /// its inline runs (delimiters are not editable positions). Snap `byte`
    /// to the nearest valid position in the given direction.
    pub fn snap_caret(&self, byte: usize, bias: Bias) -> usize {
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
    /// between runs and treating backslash escapes as atomic.
    pub fn prev_caret(&self, source: &str, byte: usize) -> usize {
        let byte = self.snap_caret(byte, Bias::Left);
        let Some(block) = self.block_at(byte).and_then(|id| self.block(id)) else {
            return byte.saturating_sub(1);
        };
        let ranges = inline_ranges(block);
        let Some(idx) = ranges.iter().position(|r| r.start <= byte && byte <= r.end) else {
            return byte;
        };
        if byte > ranges[idx].start {
            return step_left_in_slice(source, ranges[idx].start, byte);
        }
        if idx > 0 {
            let prev = &ranges[idx - 1];
            return step_left_in_slice(source, prev.start, prev.end);
        }
        // Cross to the previous block.
        let prev_block = self.block_before(block.id);
        match prev_block {
            Some(pb) => {
                let pranges = inline_ranges(pb);
                pranges.last().map(|r| r.end).unwrap_or(pb.source_range.end)
            }
            None => byte,
        }
    }

    /// One visible-grapheme step right from `byte` (mirror of `prev_caret`).
    pub fn next_caret(&self, source: &str, byte: usize) -> usize {
        let byte = self.snap_caret(byte, Bias::Right);
        let Some(block) = self.block_at(byte).and_then(|id| self.block(id)) else {
            return (byte + 1).min(source.len());
        };
        let ranges = inline_ranges(block);
        let Some(idx) = ranges.iter().position(|r| r.start <= byte && byte <= r.end) else {
            return byte;
        };
        if byte < ranges[idx].end {
            return step_right_in_slice(source, byte, ranges[idx].end);
        }
        if idx + 1 < ranges.len() {
            let next = &ranges[idx + 1];
            return step_right_in_slice(source, next.start, next.end);
        }
        match self.block_after(block.id) {
            Some(nb) => {
                let nranges = inline_ranges(nb);
                nranges
                    .first()
                    .map(|r| r.start)
                    .unwrap_or(nb.source_range.start)
            }
            None => byte,
        }
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
}

/// Byte ranges of caret-valid inline content within a leaf block.
fn inline_ranges(block: &Block) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    match &block.kind {
        // Code blocks and opaque blocks are edited as raw text.
        BlockKind::CodeBlock { .. } | BlockKind::Opaque { .. } => {
            out.push(block.source_range.clone());
        }
        BlockKind::Alert { chrome_range, .. } => {
            if chrome_range.start < chrome_range.end {
                out.push(chrome_range.clone());
            } else {
                out.push(block.source_range.clone());
            }
        }
        _ => {
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
                    Inline::SoftBreak { .. } | Inline::HardBreak { .. } => {}
                }
            }
            if out.is_empty() {
                out.push(block.source_range.clone());
            }
        }
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

/// Step one grapheme-ish unit left within [start, byte); backslash escapes
/// ("\\X") are atomic.
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
fn step_right_in_slice(source: &str, byte: usize, end: usize) -> usize {
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
}
