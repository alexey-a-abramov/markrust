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
use super::tree::{Block, BlockKind, IdGen, Inline, NodeId, RichTree};

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
        fn walk(blocks: &[Block], out: &mut Vec<(usize, u8, String)>) {
            for b in blocks {
                if let BlockKind::Heading { level, .. } = b.kind {
                    let mut text = String::new();
                    for inline in &b.inlines {
                        if let Inline::Run { text: t, .. } = inline {
                            text.push_str(t);
                        }
                    }
                    out.push((b.source_range.start, level, text));
                }
                walk(&b.children, out);
            }
        }
        let mut out = Vec::new();
        walk(&self.tree.blocks, &mut out);
        out
    }
}

/// Byte ranges of caret-valid inline content within a leaf block.
fn inline_ranges(block: &Block) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    match &block.kind {
        // Code blocks and opaque blocks are edited as raw text.
        BlockKind::CodeBlock { .. } | BlockKind::Opaque => {
            out.push(block.source_range.clone());
        }
        _ => {
            for inline in &block.inlines {
                match inline {
                    Inline::Run { source_range, .. }
                    | Inline::Image { source_range, .. }
                    | Inline::OpaqueInline { source_range, .. } => {
                        out.push(source_range.clone());
                    }
                    Inline::SoftBreak | Inline::HardBreak { .. } => {}
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
