// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`RichTree`] → markdown serialization.
//!
//! Port of tui.editor's `ToMdConvertorState` delimiter-stack design, adapted
//! to the source-primary model: in [`SerializeMode::Preserve`] any block whose
//! subtree is untouched is emitted as its raw source slice (byte-exact by
//! construction), and only dirty blocks are re-derived from the tree honoring
//! captured delimiter fidelity. [`SerializeMode::Normalize`] re-derives the
//! whole document in house style.

use std::collections::HashSet;

use super::escape::{escape_text, wrap_inline_code, EscapeContext};
use super::tree::{
    Block, BlockKind, BreakStyle, ColumnAlign, HeadingStyle, Inline, LinkAttrs, MarkSet, NodeId,
    RichTree,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerializeMode {
    /// Untouched blocks keep their exact original bytes; dirty blocks are
    /// re-serialized with delimiter fidelity.
    Preserve,
    /// Everything re-serialized in house style (fidelity ignored).
    Normalize,
}

/// Serialize the whole tree. `dirty` marks blocks (by id) whose content no
/// longer matches their `source_range` slice and must come from the tree.
pub fn serialize_tree(
    tree: &RichTree,
    source: &str,
    mode: SerializeMode,
    dirty: &HashSet<NodeId>,
) -> String {
    let mut ser = Ser {
        source,
        mode,
        dirty,
        out: String::with_capacity(source.len() + 64),
        delim: String::new(),
    };

    match mode {
        SerializeMode::Preserve => {
            let mut cursor = 0usize;
            for block in &tree.blocks {
                let start = block.source_range.start.min(source.len());
                if start > cursor {
                    ser.out.push_str(&source[cursor..start]);
                }
                ser.emit_block(block, false);
                cursor = block.source_range.end.min(source.len()).max(cursor);
            }
            if cursor < source.len() {
                ser.out.push_str(&source[cursor..]);
            }
            if tree.blocks.is_empty() && ser.out.is_empty() {
                ser.out.push_str(source);
            }
        }
        SerializeMode::Normalize => {
            if let Some(fm) = &tree.frontmatter {
                ser.out.push_str(fm.raw.trim_end_matches('\n'));
                ser.out.push('\n');
                if !tree.blocks.is_empty() {
                    ser.out.push('\n');
                }
            }
            for (i, block) in tree.blocks.iter().enumerate() {
                if i > 0 {
                    ser.blank_sep();
                }
                ser.emit_block(block, true);
            }
            if !ser.out.is_empty() && !ser.out.ends_with('\n') {
                ser.out.push('\n');
            }
        }
    }
    ser.out
}

struct Ser<'a> {
    source: &'a str,
    mode: SerializeMode,
    dirty: &'a HashSet<NodeId>,
    out: String,
    delim: String,
}

impl<'a> Ser<'a> {
    fn slice(&self, block: &Block) -> &'a str {
        self.source
            .get(block.source_range.clone())
            .unwrap_or_default()
    }

    fn subtree_dirty(&self, block: &Block) -> bool {
        self.dirty.contains(&block.id) || block.children.iter().any(|c| self.subtree_dirty(c))
    }

    fn normalize(&self) -> bool {
        matches!(self.mode, SerializeMode::Normalize)
    }

    /// Newline within a block: continue with the current line prefix.
    fn line_break(&mut self) {
        self.out.push('\n');
        self.out.push_str(&self.delim);
    }

    /// Blank separator between sibling blocks (prefix kept, trailing spaces
    /// trimmed on the empty line), leaving the cursor after the prefix of the
    /// next content line.
    fn blank_sep(&mut self) {
        self.out.push('\n');
        let trimmed = self.delim.trim_end().to_string();
        self.out.push_str(&trimmed);
        self.out.push('\n');
        self.out.push_str(&self.delim);
    }

    /// Emit one block's content (no leading prefix, no trailing newline).
    /// `force_tree` is set once any ancestor was dirty (slices below a
    /// re-derived ancestor may carry stale prefixes).
    fn emit_block(&mut self, block: &Block, force_tree: bool) {
        let from_tree = self.normalize() || force_tree || self.subtree_dirty(block);
        if !from_tree {
            let slice = self.slice(block);
            self.out.push_str(slice);
            return;
        }

        match &block.kind {
            BlockKind::Paragraph | BlockKind::TableCell => {
                self.emit_inlines(&block.inlines, false);
            }
            BlockKind::Heading { level, style } => {
                let setext = !self.normalize() && *style == HeadingStyle::Setext && *level <= 2;
                if setext {
                    self.emit_inlines(&block.inlines, false);
                    self.line_break();
                    self.out
                        .push_str(if *level == 1 { "=======" } else { "-------" });
                } else {
                    for _ in 0..*level {
                        self.out.push('#');
                    }
                    self.out.push(' ');
                    self.emit_inlines(&block.inlines, false);
                }
            }
            BlockKind::CodeBlock {
                info,
                fence,
                literal,
            } => {
                let (ch, len) = match (self.normalize(), fence) {
                    (false, Some(f)) => (f.fence_char as char, f.fence_length.max(3)),
                    _ => ('`', 3usize),
                };
                let fence_str: String = std::iter::repeat_n(ch, len).collect();
                self.out.push_str(&fence_str);
                self.out.push_str(info);
                let body = literal.strip_suffix('\n').unwrap_or(literal);
                for line in body.split('\n') {
                    self.line_break();
                    self.out.push_str(line);
                }
                if body.is_empty() {
                    // avoid emitting a stray blank body line for empty blocks
                    let new_len = self.out.len() - 1 - self.delim.len();
                    self.out.truncate(new_len);
                }
                self.line_break();
                self.out.push_str(&fence_str);
            }
            BlockKind::BlockQuote => {
                self.out.push_str("> ");
                let saved = self.delim.len();
                self.delim.push_str("> ");
                self.emit_children(&block.children, false);
                self.delim.truncate(saved);
            }
            BlockKind::BulletList { tight, marker } => {
                let marker = if self.normalize() { b'-' } else { *marker };
                let marker = if matches!(marker, b'-' | b'*' | b'+') {
                    marker as char
                } else {
                    '-'
                };
                self.emit_list_items(block, *tight, |_i| format!("{marker} "));
            }
            BlockKind::OrderedList {
                start,
                tight,
                delimiter,
            } => {
                let delim_ch = if self.normalize() {
                    '.'
                } else {
                    *delimiter as char
                };
                let start = *start;
                self.emit_list_items(block, *tight, move |i| format!("{}{delim_ch} ", start + i));
            }
            BlockKind::ListItem { .. } => {
                // Reached only via emit_list_items, which handles markers.
                self.emit_children(&block.children, false);
            }
            BlockKind::Table { alignments } => {
                self.emit_table(block, alignments);
            }
            BlockKind::TableRow { .. } => {
                // Handled by emit_table.
            }
            BlockKind::ThematicBreak => {
                if self.normalize() {
                    self.out.push_str("---");
                } else {
                    let slice = self.slice(block);
                    self.out.push_str(if slice.is_empty() {
                        "---"
                    } else {
                        slice.trim_end()
                    });
                }
            }
            BlockKind::Opaque => {
                // Inert: always the raw slice, even under a dirty ancestor.
                let slice = self.slice(block);
                self.out.push_str(slice);
            }
        }
    }

    fn emit_children(&mut self, children: &[Block], tight: bool) {
        for (i, child) in children.iter().enumerate() {
            if i > 0 {
                if tight {
                    self.line_break();
                } else {
                    self.blank_sep();
                }
            }
            self.emit_block(child, true);
        }
    }

    fn emit_list_items(&mut self, list: &Block, tight: bool, marker_for: impl Fn(usize) -> String) {
        for (i, item) in list.children.iter().enumerate() {
            if i > 0 {
                if tight {
                    self.line_break();
                } else {
                    self.blank_sep();
                }
            }
            let mut marker = marker_for(i);
            if let BlockKind::ListItem {
                task: Some(checked),
            } = &item.kind
            {
                marker.push_str(if *checked { "[x] " } else { "[ ] " });
            }
            self.out.push_str(&marker);
            let saved = self.delim.len();
            self.delim.push_str(&" ".repeat(marker.len()));
            // Item children are tight when the list is tight (paragraphs not
            // separated by blank lines).
            self.emit_children(&item.children, tight);
            self.delim.truncate(saved);
        }
    }

    fn emit_table(&mut self, table: &Block, alignments: &[ColumnAlign]) {
        // Render every cell to text first to compute column widths.
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut header_at = 0usize;
        for row in &table.children {
            if let BlockKind::TableRow { header } = row.kind {
                if header {
                    header_at = rows.len();
                }
            }
            let mut cells = Vec::new();
            for cell in &row.children {
                let mut sub = Ser {
                    source: self.source,
                    mode: self.mode,
                    dirty: self.dirty,
                    out: String::new(),
                    delim: String::new(),
                };
                sub.emit_inlines(&cell.inlines, true);
                cells.push(sub.out.replace('\n', " "));
            }
            rows.push(cells);
        }
        let cols = alignments
            .len()
            .max(rows.iter().map(Vec::len).max().unwrap_or(0));
        let mut widths = vec![3usize; cols];
        for row in &rows {
            for (c, cell) in row.iter().enumerate() {
                widths[c] = widths[c].max(cell.chars().count());
            }
        }

        let emit_row = |ser: &mut Ser, cells: &[String]| {
            ser.out.push('|');
            for (c, width) in widths.iter().enumerate() {
                let text = cells.get(c).map(String::as_str).unwrap_or("");
                let pad = width.saturating_sub(text.chars().count());
                ser.out.push(' ');
                ser.out.push_str(text);
                ser.out.push_str(&" ".repeat(pad));
                ser.out.push_str(" |");
            }
        };

        for (r, cells) in rows.iter().enumerate() {
            if r > 0 {
                self.line_break();
            }
            emit_row(self, cells);
            if r == header_at {
                self.line_break();
                self.out.push('|');
                for (c, width) in widths.iter().copied().enumerate() {
                    let align = alignments.get(c).copied().unwrap_or(ColumnAlign::None);
                    let bar = match align {
                        ColumnAlign::None => format!(" {} ", "-".repeat(width)),
                        ColumnAlign::Left => format!(" :{} ", "-".repeat(width.saturating_sub(1))),
                        ColumnAlign::Right => format!(" {}: ", "-".repeat(width.saturating_sub(1))),
                        ColumnAlign::Center => {
                            format!(" :{}: ", "-".repeat(width.saturating_sub(2).max(1)))
                        }
                    };
                    self.out.push_str(&bar);
                    self.out.push('|');
                }
            }
        }
    }

    fn emit_inlines(&mut self, inlines: &[Inline], in_table: bool) {
        let mut open_marks: Vec<(MarkSet, String, String)> = Vec::new(); // (mark, open, close)
        let mut open_link: Option<LinkAttrs> = None;

        let close_all = |ser: &mut Ser, open_marks: &mut Vec<(MarkSet, String, String)>| {
            while let Some((_, _, close)) = open_marks.pop() {
                ser.out.push_str(&close);
            }
        };

        for (idx, inline) in inlines.iter().enumerate() {
            match inline {
                Inline::Run {
                    text,
                    raw,
                    marks,
                    link,
                    fidelity,
                    ..
                } => {
                    // Link transitions (outermost).
                    let link_changed = open_link.as_ref() != link.as_ref();
                    if link_changed {
                        close_all(self, &mut open_marks);
                        if let Some(prev) = open_link.take() {
                            self.close_link(&prev);
                        }
                        if let Some(next) = link {
                            if next.autolink {
                                // Autolinks emit their own form with the run text.
                            } else {
                                self.out.push('[');
                            }
                            open_link = Some(next.clone());
                        }
                    }

                    if let Some(l) = &open_link {
                        if l.autolink {
                            // Emit as autolink and skip mark handling.
                            self.out.push('<');
                            self.out.push_str(&l.url);
                            self.out.push('>');
                            // Only once: clear so consecutive runs don't duplicate.
                            open_link = None;
                            continue;
                        }
                    }

                    // Mark transitions (canonical order: BOLD, ITALIC, STRIKE).
                    let wanted = marks.without(MarkSet::CODE);
                    // Close marks not wanted (innermost first).
                    while let Some((m, _, close)) = open_marks.last().cloned() {
                        if wanted.contains(m) {
                            break;
                        }
                        self.out.push_str(&close);
                        open_marks.pop();
                    }
                    // Open missing marks in canonical order.
                    for (mark, open, close) in mark_delims(wanted, fidelity, self.normalize()) {
                        if !open_marks.iter().any(|(m, _, _)| *m == mark) {
                            self.out.push_str(&open);
                            open_marks.push((mark, open, close));
                        }
                    }

                    if marks.contains(MarkSet::CODE) {
                        let ticks = if self.normalize() {
                            1
                        } else {
                            fidelity.code_backticks.max(1)
                        };
                        self.out.push_str(&wrap_inline_code(text, ticks));
                    } else if let (false, Some(raw)) = (self.normalize(), raw) {
                        self.out.push_str(raw);
                    } else {
                        let at_line_start = idx == 0
                            && open_marks.is_empty()
                            && open_link.is_none()
                            && (self.out.is_empty() || self.out.ends_with(&self.delim));
                        let ctx = EscapeContext {
                            in_table,
                            at_line_start,
                        };
                        self.out.push_str(&escape_text(text, ctx));
                    }
                }
                Inline::Image {
                    alt, url, title, ..
                } => {
                    self.out.push_str("![");
                    self.out.push_str(alt);
                    self.out.push_str("](");
                    self.out.push_str(url);
                    if let Some(t) = title {
                        self.out.push_str(" \"");
                        self.out.push_str(t);
                        self.out.push('"');
                    }
                    self.out.push(')');
                }
                Inline::SoftBreak => {
                    if in_table {
                        self.out.push(' ');
                    } else {
                        self.line_break();
                    }
                }
                Inline::HardBreak { style } => {
                    if in_table {
                        self.out.push(' ');
                    } else {
                        let marker = match (self.normalize(), style) {
                            (true, _) | (false, BreakStyle::Backslash) => "\\",
                            (false, BreakStyle::TwoSpaces) => "  ",
                        };
                        self.out.push_str(marker);
                        self.line_break();
                    }
                }
                Inline::OpaqueInline { raw, .. } => {
                    self.out.push_str(raw);
                }
            }
        }
        close_all(self, &mut open_marks);
        if let Some(l) = open_link.take() {
            self.close_link(&l);
        }
    }

    fn close_link(&mut self, link: &LinkAttrs) {
        if link.autolink {
            return;
        }
        self.out.push_str("](");
        self.out.push_str(&link.url);
        if let Some(t) = &link.title {
            self.out.push_str(" \"");
            self.out.push_str(t);
            self.out.push('"');
        }
        self.out.push(')');
    }
}

/// Delimiters for each wanted mark in canonical nesting order.
fn mark_delims(
    wanted: MarkSet,
    fidelity: &super::tree::MarkFidelity,
    normalize: bool,
) -> Vec<(MarkSet, String, String)> {
    let mut out = Vec::new();
    if wanted.contains(MarkSet::BOLD) {
        let ch = if normalize {
            '*'
        } else {
            fidelity.strong_delim as char
        };
        let d: String = [ch, ch].iter().collect();
        out.push((MarkSet::BOLD, d.clone(), d));
    }
    if wanted.contains(MarkSet::ITALIC) {
        let ch = if normalize {
            '*'
        } else {
            fidelity.emph_delim as char
        };
        let d = ch.to_string();
        out.push((MarkSet::ITALIC, d.clone(), d));
    }
    if wanted.contains(MarkSet::STRIKE) {
        out.push((MarkSet::STRIKE, "~~".into(), "~~".into()));
    }
    out
}
