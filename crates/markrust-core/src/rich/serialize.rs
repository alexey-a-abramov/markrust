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

/// Re-serialize a single block from the tree (fidelity honored). Used when a
/// rich command rewrites one top-level block and splices it back into source.
pub fn serialize_block(block: &Block, source: &str) -> String {
    let dirty = HashSet::from([block.id]);
    let mut ser = Ser {
        source,
        mode: SerializeMode::Preserve,
        dirty: &dirty,
        out: String::new(),
        delim: String::new(),
    };
    ser.emit_block(block, true);
    ser.out
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
                let multiline = block
                    .inlines
                    .iter()
                    .any(|i| matches!(i, Inline::SoftBreak | Inline::HardBreak { .. }));
                let setext = *level <= 2
                    && ((*style == HeadingStyle::Setext && !self.normalize()) || multiline);
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
                    let text_start = self.out.len();
                    self.emit_inlines_opts(&block.inlines, false, true);
                    escape_trailing_hashes(&mut self.out, text_start);
                }
            }
            BlockKind::CodeBlock {
                info,
                fence,
                literal,
            } => {
                let (ch, len) = match (self.normalize(), fence) {
                    (false, Some(f)) => (f.fence_char as char, f.fence_length.max(3)),
                    _ => {
                        // House fence is ``` but must not collide with fence
                        // runs inside the literal (or an info string that a
                        // backtick fence cannot carry).
                        let backtick_run = longest_line_start_run(literal, '`');
                        if backtick_run >= 3 || info.contains('`') {
                            ('~', (longest_line_start_run(literal, '~') + 1).max(3))
                        } else {
                            ('`', (backtick_run + 1).max(3))
                        }
                    }
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
                // The author's marker survives normalize too: rewriting all
                // bullets to one char would merge adjacent sibling lists.
                let marker = if matches!(*marker, b'-' | b'*' | b'+') {
                    *marker as char
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
                let delim_ch = *delimiter as char;
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
                    // "***": "---" would collide with frontmatter at document
                    // start and setext underlines after paragraphs.
                    self.out.push_str("***");
                } else {
                    let slice = self.slice(block);
                    self.out.push_str(if slice.is_empty() {
                        "---"
                    } else {
                        slice.trim_end()
                    });
                }
            }
            BlockKind::FootnoteDefinition { label } => {
                self.out.push_str("[^");
                self.out.push_str(label);
                self.out.push_str("]: ");
                let marker_width = 5 + label.len(); // `[^{label}]: `
                let saved = self.delim.len();
                self.delim.push_str(&" ".repeat(marker_width));
                self.emit_children(&block.children, false);
                self.delim.truncate(saved);
            }
            BlockKind::DefinitionList => {
                for (i, item) in block.children.iter().enumerate() {
                    if i > 0 {
                        self.blank_sep();
                    }
                    self.emit_definition_item(item);
                }
            }
            BlockKind::DefinitionItem { .. } => self.emit_definition_item(block),
            BlockKind::DefinitionTerm => {
                self.emit_children(&block.children, true);
            }
            BlockKind::DefinitionDetails => {
                self.out.push_str(": ");
                let saved = self.delim.len();
                self.delim.push_str("  ");
                self.emit_children(&block.children, false);
                self.delim.truncate(saved);
            }
            BlockKind::Opaque { raw } => {
                // Inert content; each continuation line re-prefixed so it
                // stays inside the current container (quote, list item).
                for (i, line) in raw.split('\n').enumerate() {
                    if i > 0 {
                        self.line_break();
                    }
                    self.out.push_str(line);
                }
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

    fn emit_definition_item(&mut self, item: &Block) {
        let tight = match &item.kind {
            BlockKind::DefinitionItem { tight } => *tight,
            _ => false,
        };
        let mut last_was_term = false;
        let mut saw_any = false;
        for child in &item.children {
            match &child.kind {
                BlockKind::DefinitionTerm => {
                    if saw_any {
                        self.line_break();
                    }
                    self.emit_block(child, true);
                    last_was_term = true;
                    saw_any = true;
                }
                BlockKind::DefinitionDetails => {
                    if saw_any {
                        if last_was_term && !tight {
                            self.blank_sep();
                        } else {
                            self.line_break();
                        }
                    }
                    self.out.push_str(": ");
                    let saved = self.delim.len();
                    self.delim.push_str("  ");
                    self.emit_children(&child.children, tight);
                    self.delim.truncate(saved);
                    last_was_term = false;
                    saw_any = true;
                }
                _ => {
                    if saw_any {
                        self.line_break();
                    }
                    self.emit_block(child, true);
                    saw_any = true;
                }
            }
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
            let marker = marker_for(i);
            self.out.push_str(&marker);
            // Continuation lines indent by the list-marker width only; a task
            // checkbox is item *content*, not part of the marker.
            let saved = self.delim.len();
            self.delim.push_str(&" ".repeat(marker.len()));
            if let BlockKind::ListItem {
                task: Some(checked),
            } = &item.kind
            {
                self.out.push_str(if *checked { "[x] " } else { "[ ] " });
            }
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
        self.emit_inlines_opts(inlines, in_table, false);
    }

    /// `single_line`: soft/hard breaks become spaces (ATX headings).
    fn emit_inlines_opts(&mut self, inlines: &[Inline], in_table: bool, single_line: bool) {
        let mut stack: Vec<(MarkKey, String)> = Vec::new(); // (key, close-delim)
        let mut at_line_start =
            self.out.is_empty() || self.out.ends_with('\n') || self.out.ends_with(&self.delim);
        let mut skip_autolink: Option<String> = None;

        for (i, inline) in inlines.iter().enumerate() {
            // Autolinks emit once for their whole run group.
            if let Inline::Run { link: Some(l), .. } = inline {
                if l.autolink {
                    if skip_autolink.as_deref() != Some(l.url.as_str()) {
                        close_down_to(self, &mut stack, 0);
                        self.out.push('<');
                        self.out.push_str(&l.url);
                        self.out.push('>');
                        at_line_start = false;
                        skip_autolink = Some(l.url.clone());
                    }
                    continue;
                }
            }
            skip_autolink = None;

            match inline {
                Inline::SoftBreak => {
                    close_before_break(self, &mut stack, inlines, i);
                    if in_table || single_line {
                        self.out.push(' ');
                    } else {
                        self.line_break();
                        at_line_start = true;
                    }
                    continue;
                }
                Inline::HardBreak { style } => {
                    close_before_break(self, &mut stack, inlines, i);
                    if in_table || single_line {
                        self.out.push(' ');
                    } else {
                        let marker = match (self.normalize(), style) {
                            (true, _) | (false, BreakStyle::Backslash) => "\\",
                            (false, BreakStyle::TwoSpaces) => "  ",
                        };
                        self.out.push_str(marker);
                        self.line_break();
                        at_line_start = true;
                    }
                    continue;
                }
                _ => {}
            }

            let wanted = inline_keys(inline);
            // Close entries (LIFO) until the stack is a subset of `wanted`.
            let keep = stack
                .iter()
                .take_while(|(k, _)| wanted.iter().any(|w| keys_match(k, w)))
                .count();
            close_down_to(self, &mut stack, keep);
            // Open missing keys, longest extent first (outermost).
            let mut missing: Vec<&MarkKey> = wanted
                .iter()
                .filter(|k| !stack.iter().any(|(sk, _)| keys_match(sk, k)))
                .collect();
            missing.sort_by_key(|k| std::cmp::Reverse(key_extent(inlines, i, k)));
            for key in missing {
                let (open, close) = key_delims(key, inline, self.normalize());
                if open.starts_with('[') && self.out.ends_with('!') && !self.out.ends_with("\\!") {
                    // "!" + "[" would form image syntax; escape the bang.
                    let bang = self.out.len() - 1;
                    self.out.insert(bang, '\\');
                }
                self.out.push_str(&open);
                stack.push((key.clone(), close));
                at_line_start = false;
            }

            match inline {
                Inline::Run {
                    text,
                    raw,
                    marks,
                    fidelity,
                    ..
                } => {
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
                        let ctx = EscapeContext {
                            in_table,
                            at_line_start: at_line_start && stack.is_empty(),
                        };
                        self.out.push_str(&escape_text(text, ctx));
                    }
                    if !text.is_empty() {
                        at_line_start = false;
                    }
                }
                Inline::Image {
                    alt, url, title, ..
                } => {
                    self.out.push_str("![");
                    self.out.push_str(alt);
                    self.out.push_str("](");
                    self.out.push_str(&printable_url(url));
                    if let Some(t) = title {
                        self.out.push_str(" \"");
                        self.out.push_str(&t.replace('"', "\\\""));
                        self.out.push('"');
                    }
                    self.out.push(')');
                    at_line_start = false;
                }
                Inline::OpaqueInline { raw, .. } => {
                    self.out.push_str(raw);
                    if !raw.is_empty() {
                        at_line_start = false;
                    }
                }
                _ => {}
            }
        }
        close_down_to(self, &mut stack, 0);
    }
}

/// A mark or link entry in the inline nesting stack.
#[derive(Debug, Clone, PartialEq)]
enum MarkKey {
    Bold(u64),
    Italic(u64),
    Strike(u64),
    Link(LinkAttrs),
}

fn inline_keys(inline: &Inline) -> Vec<MarkKey> {
    let (marks, link) = match inline {
        Inline::Run { marks, link, .. } => (*marks, link.clone()),
        Inline::Image { marks, link, .. } => (*marks, link.clone()),
        Inline::OpaqueInline { marks, .. } => (*marks, None),
        _ => (MarkSet::empty(), None),
    };
    let fidelity = match inline {
        Inline::Run { fidelity, .. } => *fidelity,
        _ => super::tree::MarkFidelity::default(),
    };
    let mut keys = Vec::new();
    if let Some(l) = link {
        if !l.autolink {
            keys.push(MarkKey::Link(l));
        }
    }
    if marks.contains(MarkSet::BOLD) {
        keys.push(MarkKey::Bold(fidelity.strong_group));
    }
    if marks.contains(MarkSet::ITALIC) {
        keys.push(MarkKey::Italic(fidelity.emph_group));
    }
    if marks.contains(MarkSet::STRIKE) {
        keys.push(MarkKey::Strike(fidelity.strike_group));
    }
    keys
}

/// Group 0 (images / inline HTML, which carry no fidelity) matches any group
/// of the same mark so it does not force a close/reopen around itself.
fn keys_match(stack_key: &MarkKey, wanted: &MarkKey) -> bool {
    match (stack_key, wanted) {
        (MarkKey::Bold(a), MarkKey::Bold(b))
        | (MarkKey::Italic(a), MarkKey::Italic(b))
        | (MarkKey::Strike(a), MarkKey::Strike(b)) => *a == *b || *a == 0 || *b == 0,
        (a, b) => a == b,
    }
}

/// How many consecutive inlines starting at `i` carry `key`.
fn key_extent(inlines: &[Inline], i: usize, key: &MarkKey) -> usize {
    inlines[i..]
        .iter()
        .take_while(|inline| {
            matches!(inline, Inline::SoftBreak | Inline::HardBreak { .. })
                || inline_keys(inline).iter().any(|k| keys_match(k, key))
        })
        .count()
}

fn key_delims(key: &MarkKey, inline: &Inline, normalize: bool) -> (String, String) {
    let fidelity = match inline {
        Inline::Run { fidelity, .. } => *fidelity,
        _ => super::tree::MarkFidelity::default(),
    };
    match key {
        MarkKey::Bold(_) => {
            let ch = if normalize {
                '*'
            } else {
                fidelity.strong_delim as char
            };
            let d: String = [ch, ch].iter().collect();
            (d.clone(), d)
        }
        MarkKey::Italic(_) => {
            let ch = if normalize {
                '*'
            } else {
                fidelity.emph_delim as char
            };
            (ch.to_string(), ch.to_string())
        }
        MarkKey::Strike(_) => ("~~".into(), "~~".into()),
        MarkKey::Link(l) => {
            let mut close = String::from("](");
            close.push_str(&printable_url(&l.url));
            if let Some(t) = &l.title {
                close.push_str(" \"");
                close.push_str(&t.replace('"', "\\\""));
                close.push('"');
            }
            close.push(')');
            ("[".into(), close)
        }
    }
}

/// Before emitting a line break, close stack entries that do not continue in
/// the next content inline (a link or emphasis must not swallow the break).
fn close_before_break(
    ser: &mut Ser,
    stack: &mut Vec<(MarkKey, String)>,
    inlines: &[Inline],
    i: usize,
) {
    let next_keys = inlines[i + 1..]
        .iter()
        .find(|n| !matches!(n, Inline::SoftBreak | Inline::HardBreak { .. }))
        .map(inline_keys)
        .unwrap_or_default();
    let keep = stack
        .iter()
        .take_while(|(k, _)| next_keys.iter().any(|w| keys_match(k, w)))
        .count();
    close_down_to(ser, stack, keep);
}

fn close_down_to(ser: &mut Ser, stack: &mut Vec<(MarkKey, String)>, keep: usize) {
    while stack.len() > keep {
        let (_, close) = stack.pop().unwrap();
        ser.out.push_str(&close);
    }
}

/// Destination form for a link/image URL: wrap in <> when it needs it.
fn printable_url(url: &str) -> String {
    let needs_brackets = url.is_empty()
        || url.chars().any(|c| c == ' ' || c.is_control())
        || url.matches('(').count() != url.matches(')').count();
    if needs_brackets {
        format!("<{url}>")
    } else {
        url.to_string()
    }
}

/// Longest run of `ch` found at the start of any line in `text`.
fn longest_line_start_run(text: &str, ch: char) -> usize {
    text.lines()
        .map(|line| line.chars().take_while(|c| *c == ch).count())
        .max()
        .unwrap_or(0)
}

/// Escape an unescaped trailing `#` sequence emitted for an ATX heading so
/// the reparse doesn't strip it as a closing sequence.
fn escape_trailing_hashes(out: &mut String, text_start: usize) {
    let text = &out[text_start..];
    let trailing = text.chars().rev().take_while(|c| *c == '#').count();
    if trailing == 0 {
        return;
    }
    let hash_start = out.len() - trailing;
    if out[text_start..hash_start].ends_with('\\') {
        return; // already escaped
    }
    out.insert(hash_start, '\\');
}
