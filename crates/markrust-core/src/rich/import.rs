// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Markdown → [`RichTree`] import via comrak.
//!
//! comrak is the single markdown grammar for the editor (the HTML exporter in
//! `export.rs` uses the same extension set, so the two can never disagree).
//! Block source positions come from comrak sourcepos; inline positions are
//! advisory and verified against the raw slice before use.

use comrak::nodes::{
    AlertType, AstNode, ListDelimType, ListType, NodeValue, Sourcepos, TableAlignment,
};
use comrak::{parse_document, Arena, Options};

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use super::tree::{
    find_alert_chrome, is_toc_marker, trailing_blank_gap, AlertKind, Block, BlockKind, BreakStyle,
    ColumnAlign, FenceFidelity, Frontmatter, HeadingStyle, IdGen, Inline, LinkAttrs, MarkFidelity,
    MarkSet, NodeId, RichTree,
};

/// Parse options shared with `export::markdown_to_html_gfm` (parse-relevant
/// subset) plus sourcepos tracking. Also used by the background span extractor
/// so source-mode masking and the rich tree share one grammar.
///
/// Footnotes and description lists are Typora extras (not GFM). They import
/// as nested containers so body Markdown is a rich tree; Preserve identity
/// still uses the top-level source slice.
pub(crate) fn parse_options() -> Options<'static> {
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    options.extension.footnotes = true;
    options.extension.description_lists = true;
    // Typora extras (not GFM). `==highlight==` is not a comrak node; see
    // `apply_eqeq_highlight`. Do not enable `underline` — it would steal GFM `__bold__`.
    // GitHub alerts (`> [!NOTE]`, …) are enabled so WYSIWYG can paint callouts.
    options.extension.superscript = true;
    options.extension.subscript = true;
    options.extension.math_dollars = true;
    options.extension.alerts = true;
    // Typora `[[target]]` / `[[target|label]]` (title after pipe).
    options.extension.wikilinks_title_after_pipe = true;
    options.extension.front_matter_delimiter = Some("---".into());
    options.render.sourcepos = true;
    options
}

/// Byte offsets of every line start, for sourcepos conversion.
pub(crate) struct LineStarts(Vec<usize>);

impl LineStarts {
    pub(crate) fn new(source: &str) -> Self {
        let mut starts = vec![0];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        LineStarts(starts)
    }

    /// comrak sourcepos: 1-based line, 1-based *byte* column, end-inclusive.
    /// (Byte-column semantics are pinned by `sourcepos_columns_are_bytes`.)
    pub(crate) fn range(&self, sp: Sourcepos, source_len: usize) -> std::ops::Range<usize> {
        let start = self
            .0
            .get(sp.start.line.saturating_sub(1))
            .map(|ls| ls + sp.start.column.saturating_sub(1))
            .unwrap_or(source_len)
            .min(source_len);
        let end = self
            .0
            .get(sp.end.line.saturating_sub(1))
            .map(|ls| ls + sp.end.column)
            .unwrap_or(source_len)
            .min(source_len);
        start..end.max(start)
    }
}

/// Comrak indented-code sourcepos often starts at the first content column,
/// dropping the opening 4 spaces / tab. Recover that indent so Home/click
/// can skip it as dest chrome like fence ticks. Stops at the line start,
/// previously consumed bytes, or a non-space (does not steal `>` / `- `).
pub(crate) fn recover_indented_code_range(
    source: &str,
    reported: Range<usize>,
    consumed: usize,
) -> Range<usize> {
    recover_leading_indent(source, reported, consumed, 4, true, 0)
}

/// CommonMark 0–3 spaces before ATX `#`, a setext title, a fence, or a
/// thematic break. Comrak sourcepos often starts at the marker / first
/// title byte, dropping that indent so Home/click sit on an orphan gap.
/// Recover it as dest chrome (same helper as indented-code indent). Stops
/// at the quote/list prefix, consumed bytes, or a non-space (quoted
/// `>  # Title` keeps `>`). Four spaces is indented code — do not steal
/// that.
pub(crate) fn recover_cm_opening_indent(
    source: &str,
    reported: Range<usize>,
    consumed: usize,
) -> Range<usize> {
    let start = reported.start.min(source.len());
    let line_start = source[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = source[start..]
        .find('\n')
        .map(|i| start + i)
        .unwrap_or(source.len());
    let line = source.get(line_start..line_end).unwrap_or("");
    let prefix = super::engine::quote_list_prefix_on_line(line).len();
    recover_leading_indent(source, reported, consumed, 3, false, prefix)
}

/// Pull leading indent into `reported` so dest-chrome skip can see it.
/// `max_spaces` is 4 for indented code, 3 for ATX / setext / fence / thematic.
/// `allow_tab` recovers one opening tab (indented code only).
/// `container_prefix` is quote/list bytes from the line start that must
/// not be stolen (`>` / `- `).
fn recover_leading_indent(
    source: &str,
    reported: Range<usize>,
    consumed: usize,
    max_spaces: usize,
    allow_tab: bool,
    container_prefix: usize,
) -> Range<usize> {
    let start = reported.start.min(source.len());
    let end = reported.end.min(source.len()).max(start);
    if start == 0 {
        return start..end;
    }
    let line_start = source[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let floor = line_start
        .saturating_add(container_prefix)
        .max(consumed)
        .min(start);
    let bytes = source.as_bytes();
    if allow_tab && start > floor && bytes[start - 1] == b'\t' {
        return (start - 1)..end;
    }
    let mut i = start;
    let mut spaces = 0;
    while i > floor && bytes[i - 1] == b' ' && spaces < max_spaces {
        i -= 1;
        spaces += 1;
    }
    i..end
}

struct Importer<'s> {
    source: &'s str,
    lines: LineStarts,
    link_groups: std::cell::Cell<u64>,
    /// End of the last recovered block. Comrak HTML-block sourcepos is often
    /// empty (`0..0` / `2..2`); search for the literal from here so two
    /// identical `<!-- a -->` comments stay distinct.
    consumed: std::cell::Cell<usize>,
}

/// Import markdown into a fresh [`RichTree`]; ids come from `ids`.
pub fn import_markdown(source: &str, ids: &mut IdGen) -> RichTree {
    let arena = Arena::new();
    let parse_input = crate::frontmatter::comrak_parse_input(source);
    let root = parse_document(&arena, parse_input.as_ref(), &parse_options());
    let importer = Importer {
        source,
        lines: LineStarts::new(source),
        link_groups: std::cell::Cell::new(0),
        consumed: std::cell::Cell::new(0),
    };

    let mut frontmatter = crate::parse_frontmatter(source).map(|info| Frontmatter {
        raw: source
            .get(info.start_byte..info.end_byte)
            .unwrap_or("")
            .to_string(),
        source_range: info.start_byte..info.end_byte,
    });
    let mut blocks = Vec::new();
    for child in root.children() {
        if let NodeValue::FrontMatter(raw) = &child.data.borrow().value {
            if frontmatter.is_none() {
                let source_range = importer.node_range(child);
                frontmatter = Some(Frontmatter {
                    raw: raw.clone(),
                    source_range,
                });
            }
            continue;
        }
        blocks.push(importer.import_block(child, ids));
    }
    if let Some(fm) = &frontmatter {
        let end = fm.source_range.end;
        blocks.retain(|b| b.source_range.start >= end);
    }
    let mut tree = RichTree {
        frontmatter,
        blocks,
        source_len: source.len(),
        trailing_blank: trailing_blank_gap(source),
        empty_prefix_homes: Vec::new(),
    };
    recover_link_reference_definitions(&mut tree, source, ids);
    tree.empty_prefix_homes = super::engine::collect_empty_prefix_homes(source, &tree);
    tree
}

impl<'s> Importer<'s> {
    fn node_range<'a>(&self, node: &'a AstNode<'a>) -> std::ops::Range<usize> {
        self.lines
            .range(node.data.borrow().sourcepos, self.source.len())
    }

    fn slice(&self, range: &std::ops::Range<usize>) -> &'s str {
        self.source.get(range.clone()).unwrap_or("")
    }

    /// Soft/hard break sourcepos can be empty; recover the newline (and any
    /// hard-break marker) so WYSIWYG click/IME do not map onto the paragraph start.
    fn break_source_range(&self, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
        if range.end > range.start {
            return range.start..range.end.min(self.source.len());
        }
        let start = range.start.min(self.source.len());
        let bytes = self.source.as_bytes();
        if bytes.get(start) == Some(&b'\n') {
            return start..start + 1;
        }
        if start > 0 && bytes[start - 1] == b'\n' {
            let mut lo = start - 1;
            while lo > 0 && matches!(bytes[lo - 1], b' ' | b'\\') {
                lo -= 1;
                if bytes[lo] == b'\\' {
                    break;
                }
            }
            return lo..start;
        }
        start..self.source.len().min(start + 1)
    }

    /// Include the two-space / backslash marker so click maps onto the break
    /// (end of the previous run), not only the newline (which snap would
    /// send onto the next line).
    fn expand_hard_break(&self, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
        let start = range.start.min(self.source.len());
        let end = range.end.min(self.source.len()).max(start);
        let bytes = self.source.as_bytes();
        if start >= 1 && bytes[start - 1] == b'\\' {
            return start - 1..end.max(start);
        }
        if start >= 2 && bytes[start - 2] == b' ' && bytes[start - 1] == b' ' {
            return start - 2..end.max(start);
        }
        start..end
    }

    fn import_block<'a>(&self, node: &'a AstNode<'a>, ids: &mut IdGen) -> Block {
        let mut source_range = self.node_range(node);
        let id = ids.next_id();
        let value = &node.data.borrow().value;

        let (kind, container) = match value {
            NodeValue::Paragraph => (BlockKind::Paragraph, false),
            NodeValue::Heading(h) => {
                source_range =
                    recover_cm_opening_indent(self.source, source_range, self.consumed.get());
                (
                    BlockKind::Heading {
                        level: h.level,
                        style: if h.setext {
                            HeadingStyle::Setext
                        } else {
                            HeadingStyle::Atx
                        },
                    },
                    false,
                )
            }
            NodeValue::CodeBlock(cb) => {
                if !cb.fenced {
                    source_range =
                        recover_indented_code_range(self.source, source_range, self.consumed.get());
                } else {
                    source_range =
                        recover_cm_opening_indent(self.source, source_range, self.consumed.get());
                }
                (
                    BlockKind::CodeBlock {
                        info: cb.info.clone(),
                        fence: cb.fenced.then_some(FenceFidelity {
                            fence_char: cb.fence_char,
                            fence_length: cb.fence_length,
                            fence_offset: cb.fence_offset,
                        }),
                        literal: cb.literal.clone(),
                    },
                    false,
                )
            }
            NodeValue::BlockQuote => (BlockKind::BlockQuote, true),
            NodeValue::Alert(alert) => {
                let kind = match alert.alert_type {
                    AlertType::Note => AlertKind::Note,
                    AlertType::Tip => AlertKind::Tip,
                    AlertType::Important => AlertKind::Important,
                    AlertType::Warning => AlertKind::Warning,
                    AlertType::Caution => AlertKind::Caution,
                };
                let (tag_range, chrome_range) =
                    match find_alert_chrome(self.source, source_range.clone()) {
                        Some(c) => (c.tag_range, c.chrome_range),
                        None => (
                            source_range.start..source_range.start,
                            source_range.start..source_range.start,
                        ),
                    };
                (
                    BlockKind::Alert {
                        kind,
                        title: alert.title.clone(),
                        tag_range,
                        chrome_range,
                    },
                    true,
                )
            }
            NodeValue::List(l) => (
                match l.list_type {
                    ListType::Bullet => BlockKind::BulletList {
                        tight: l.tight,
                        marker: l.bullet_char,
                    },
                    ListType::Ordered => BlockKind::OrderedList {
                        start: l.start,
                        tight: l.tight,
                        delimiter: match l.delimiter {
                            ListDelimType::Period => b'.',
                            ListDelimType::Paren => b')',
                        },
                    },
                },
                true,
            ),
            NodeValue::Item(_) => (BlockKind::ListItem { task: None }, true),
            NodeValue::TaskItem(symbol) => (
                BlockKind::ListItem {
                    task: Some(symbol.is_some()),
                },
                true,
            ),
            NodeValue::Table(t) => (
                BlockKind::Table {
                    alignments: t
                        .alignments
                        .iter()
                        .map(|a| match a {
                            TableAlignment::None => ColumnAlign::None,
                            TableAlignment::Left => ColumnAlign::Left,
                            TableAlignment::Center => ColumnAlign::Center,
                            TableAlignment::Right => ColumnAlign::Right,
                        })
                        .collect(),
                },
                true,
            ),
            NodeValue::TableRow(header) => (BlockKind::TableRow { header: *header }, true),
            NodeValue::TableCell => (BlockKind::TableCell, false),
            NodeValue::ThematicBreak => {
                source_range =
                    recover_cm_opening_indent(self.source, source_range, self.consumed.get());
                (BlockKind::ThematicBreak, false)
            }
            NodeValue::FootnoteDefinition(f) => (
                BlockKind::FootnoteDefinition {
                    label: f.name.clone(),
                },
                true,
            ),
            NodeValue::DescriptionList => (BlockKind::DefinitionList, true),
            NodeValue::DescriptionItem(di) => (BlockKind::DefinitionItem { tight: di.tight }, true),
            NodeValue::DescriptionTerm => (BlockKind::DefinitionTerm, true),
            NodeValue::DescriptionDetails => (BlockKind::DefinitionDetails, true),
            // Everything else is inert and round-trips verbatim. comrak's
            // sourcepos for HTML blocks is unreliable (comments are often
            // `0..0` / `2..2`), so prefer the literal and recover the span.
            NodeValue::HtmlBlock(h) => {
                let raw = h.literal.trim_end_matches('\n').to_string();
                source_range = self.recover_html_block_range(source_range, &raw);
                (BlockKind::Opaque { raw }, false)
            }
            _ => (
                BlockKind::Opaque {
                    raw: self.slice(&source_range).to_string(),
                },
                false,
            ),
        };

        let content_hash = super::engine::hash_str(self.slice(&source_range));
        let mut block = Block {
            id,
            source_range,
            content_hash,
            kind,
            children: Vec::new(),
            inlines: Vec::new(),
        };

        if matches!(
            block.kind,
            BlockKind::Opaque { .. } | BlockKind::ThematicBreak
        ) {
            return self.finish_block(block);
        }
        if matches!(block.kind, BlockKind::CodeBlock { .. }) {
            let text = match &block.kind {
                BlockKind::CodeBlock { literal, .. } => {
                    literal.strip_suffix('\n').unwrap_or(literal).to_string()
                }
                _ => unreachable!(),
            };
            let body = block.code_body_range(self.source);
            block.inlines.push(Inline::Run {
                text,
                raw: None,
                source_range: body,
                marks: MarkSet::CODE,
                link: None,
                fidelity: MarkFidelity::default(),
            });
            return self.finish_block(block);
        }

        if container {
            for child in node.children() {
                block.children.push(self.import_block(child, ids));
            }
            // comrak sourcepos for DescriptionTerm/Details/Item is known-bad;
            // expand to the children's real ranges so caret descent works.
            if matches!(
                block.kind,
                BlockKind::DefinitionTerm
                    | BlockKind::DefinitionDetails
                    | BlockKind::DefinitionItem { .. }
            ) {
                cover_children(&mut block);
                block.content_hash = super::engine::hash_str(self.slice(&block.source_range));
            }
        } else {
            let mut ctx = InlineCtx {
                marks: MarkSet::empty(),
                link: None,
                fidelity: MarkFidelity::default(),
            };
            for child in node.children() {
                self.import_inline(child, &mut ctx, &mut block.inlines);
            }
            repair_gfm_autolink_inlines(
                self.source,
                block.source_range.clone(),
                &mut block.inlines,
            );
            super::entities::split_character_reference_inlines(
                self.source,
                block.source_range.clone(),
                &mut block.inlines,
            );
            apply_eqeq_highlight(&mut block.inlines, &self.link_groups);
            super::emoji::apply_emoji_shortcodes(&mut block.inlines);
            apply_unmatched_footnote_refs(&mut block.inlines);
            merge_inline_svg(&mut block.inlines);
            insert_missing_source_breaks(self.source, &mut block.inlines);
        }
        if matches!(block.kind, BlockKind::Paragraph)
            && is_toc_marker(self.slice(&block.source_range))
        {
            let wiki = self
                .slice(&block.source_range)
                .trim()
                .eq_ignore_ascii_case("[[toc]]");
            block.kind = BlockKind::Toc { wiki };
        }
        // Comrak treats `[x]` at EOL as a task (EOF sentinel). GFM requires a
        // space after `]`, so restore the slot as list-item text.
        self.demote_nongfm_eol_task(&mut block, ids);
        self.finish_block(block)
    }

    fn finish_block(&self, block: Block) -> Block {
        if block.source_range.end > self.consumed.get() {
            self.consumed.set(block.source_range.end);
        }
        block
    }

    /// Comrak HTML-block sourcepos is often empty or pointed at a later
    /// occurrence (CM 152: `  <!-- foo -->` vs indented code `    <!-- foo -->`).
    /// Always search from the last consumed offset so the literal matches in
    /// document order.
    fn recover_html_block_range(
        &self,
        reported: std::ops::Range<usize>,
        literal: &str,
    ) -> std::ops::Range<usize> {
        let lit = literal.trim_end_matches('\n');
        if lit.is_empty() {
            return reported;
        }
        let from = self.consumed.get().min(self.source.len());
        if let Some(rel) = self.source.get(from..).and_then(|s| s.find(lit)) {
            let start = from + rel;
            return start..start + lit.len();
        }
        if let Some(start) = self.source.find(lit) {
            return start..start + lit.len();
        }
        reported
    }

    /// `[x]` / `[ ]` / `[X]` with no following space or tab is not a GFM task.
    fn demote_nongfm_eol_task(&self, item: &mut Block, ids: &mut IdGen) {
        let Some(slot) = nongfm_eol_checkbox_slot(self.source, item) else {
            return;
        };
        item.kind = BlockKind::ListItem { task: None };
        let text = self.slice(&slot).to_string();
        let raw = (!text.is_empty()).then(|| Box::<str>::from(text.as_str()));
        let run = Inline::Run {
            text,
            raw,
            source_range: slot.clone(),
            marks: MarkSet::empty(),
            link: None,
            fidelity: MarkFidelity::default(),
        };
        if let Some(para) = item
            .children
            .iter_mut()
            .find(|child| matches!(child.kind, BlockKind::Paragraph))
        {
            if para
                .inlines
                .iter()
                .all(|inline| inline.source_range().start >= slot.end)
            {
                para.inlines.insert(0, run);
            }
            if para.source_range.start > slot.start {
                para.source_range.start = slot.start;
            }
            if para.source_range.end < slot.end {
                para.source_range.end = slot.end;
            }
            para.content_hash = super::engine::hash_str(self.slice(&para.source_range));
        } else {
            item.children.insert(
                0,
                Block {
                    id: ids.next_id(),
                    source_range: slot.clone(),
                    content_hash: super::engine::hash_str(self.slice(&slot)),
                    kind: BlockKind::Paragraph,
                    children: Vec::new(),
                    inlines: vec![run],
                },
            );
        }
        item.content_hash = super::engine::hash_str(self.slice(&item.source_range));
    }

    fn import_inline<'a>(&self, node: &'a AstNode<'a>, ctx: &mut InlineCtx, out: &mut Vec<Inline>) {
        let range = self.node_range(node);
        let value = &node.data.borrow().value;
        match value {
            NodeValue::Text(text) => {
                // Keep the raw slice only when it plausibly maps back to this
                // text (inline sourcepos is advisory).
                let raw = {
                    let slice = self.slice(&range);
                    (!slice.is_empty()).then(|| Box::<str>::from(slice))
                };
                out.push(Inline::Run {
                    text: text.clone(),
                    raw,
                    source_range: range,
                    marks: ctx.marks,
                    link: ctx.link.clone(),
                    fidelity: ctx.fidelity,
                });
            }
            NodeValue::Emph => {
                let saved = (ctx.marks, ctx.fidelity);
                ctx.marks = ctx.marks.with(MarkSet::ITALIC);
                self.link_groups.set(self.link_groups.get() + 1);
                ctx.fidelity.emph_group = self.link_groups.get();
                if let Some(d) = self.slice(&range).bytes().next() {
                    if d == b'*' || d == b'_' {
                        ctx.fidelity.emph_delim = d;
                    }
                }
                for child in node.children() {
                    self.import_inline(child, ctx, out);
                }
                (ctx.marks, ctx.fidelity) = saved;
            }
            NodeValue::Strong => {
                let saved = (ctx.marks, ctx.fidelity);
                ctx.marks = ctx.marks.with(MarkSet::BOLD);
                self.link_groups.set(self.link_groups.get() + 1);
                ctx.fidelity.strong_group = self.link_groups.get();
                if let Some(d) = self.slice(&range).bytes().next() {
                    if d == b'*' || d == b'_' {
                        ctx.fidelity.strong_delim = d;
                    }
                }
                for child in node.children() {
                    self.import_inline(child, ctx, out);
                }
                (ctx.marks, ctx.fidelity) = saved;
            }
            NodeValue::Strikethrough => {
                let saved = (ctx.marks, ctx.fidelity);
                ctx.marks = ctx.marks.with(MarkSet::STRIKE);
                self.link_groups.set(self.link_groups.get() + 1);
                ctx.fidelity.strike_group = self.link_groups.get();
                for child in node.children() {
                    self.import_inline(child, ctx, out);
                }
                (ctx.marks, ctx.fidelity) = saved;
            }
            NodeValue::Superscript => {
                let saved = (ctx.marks, ctx.fidelity);
                ctx.marks = ctx.marks.with(MarkSet::SUP);
                self.link_groups.set(self.link_groups.get() + 1);
                ctx.fidelity.sup_group = self.link_groups.get();
                for child in node.children() {
                    self.import_inline(child, ctx, out);
                }
                (ctx.marks, ctx.fidelity) = saved;
            }
            NodeValue::Subscript => {
                let saved = (ctx.marks, ctx.fidelity);
                ctx.marks = ctx.marks.with(MarkSet::SUB);
                self.link_groups.set(self.link_groups.get() + 1);
                ctx.fidelity.sub_group = self.link_groups.get();
                for child in node.children() {
                    self.import_inline(child, ctx, out);
                }
                (ctx.marks, ctx.fidelity) = saved;
            }
            NodeValue::Code(code) => {
                let mut fidelity = ctx.fidelity;
                fidelity.code_backticks = code.num_backticks;
                out.push(Inline::Run {
                    text: code.literal.clone(),
                    raw: None,
                    source_range: range,
                    marks: ctx.marks.with(MarkSet::CODE),
                    link: ctx.link.clone(),
                    fidelity,
                });
            }
            NodeValue::Link(link) => {
                let slice = self.slice(&range);
                // True autolinks: <url> form, a bare url (autolink ext), or a
                // single text child identical to the destination. GFM `www.`
                // children are `www.example.com` while the dest is
                // `http://www.example.com`; comrak's link sourcepos is often
                // `0..1` so `slice` is not the URL.
                let child_text = {
                    let mut children = node.children();
                    match (children.next(), children.next()) {
                        (Some(c), None) => match &c.data.borrow().value {
                            NodeValue::Text(t) => Some(t.clone()),
                            _ => None,
                        },
                        _ => None,
                    }
                };
                let only_child_is_url = child_text.as_deref() == Some(link.url.as_str());
                let inner = self.link_inner_range(node, range.clone());
                // Do not treat `mailto:` / `http://www.` dests as autolink
                // (Normalize would emit `<mailto:…>` / `<http://www.…>` and
                // change HTML). Repair recovers the source span separately.
                let autolink = (slice.starts_with('<') && slice.ends_with('>'))
                    || slice == link.url
                    || only_child_is_url;
                self.link_groups.set(self.link_groups.get() + 1);
                let saved = ctx.link.take();
                ctx.link = Some(LinkAttrs {
                    url: link.url.clone(),
                    title: (!link.title.is_empty()).then(|| link.title.clone()),
                    autolink,
                    angle: wrapped_in_angle_brackets(self.source, &inner)
                        || wrapped_in_angle_brackets(self.source, &range),
                    group: self.link_groups.get(),
                });
                let before = out.len();
                for child in node.children() {
                    self.import_inline(child, ctx, out);
                }
                if out.len() == before {
                    out.push(Inline::Run {
                        text: String::new(),
                        raw: None,
                        source_range: range.clone(),
                        marks: ctx.marks,
                        link: ctx.link.clone(),
                        fidelity: ctx.fidelity,
                    });
                }
                ctx.link = saved;
            }
            NodeValue::Image(link) => {
                let mut alt = String::new();
                collect_text(node, &mut alt);
                out.push(Inline::Image {
                    alt,
                    url: link.url.clone(),
                    title: (!link.title.is_empty()).then(|| link.title.clone()),
                    source_range: range,
                    marks: ctx.marks,
                    link: ctx.link.clone(),
                });
            }
            NodeValue::SoftBreak => out.push(Inline::SoftBreak {
                source_range: self.break_source_range(range),
            }),
            NodeValue::LineBreak => {
                let source_range = self.expand_hard_break(self.break_source_range(range));
                trim_inlines_ending_in_hard_break(self.source, out, &source_range);
                let style = if self.slice(&source_range).contains('\\') {
                    BreakStyle::Backslash
                } else {
                    BreakStyle::TwoSpaces
                };
                out.push(Inline::HardBreak {
                    style,
                    source_range,
                });
            }
            NodeValue::Math(math) => {
                let display = math.display_math;
                let literal = math.literal.clone();
                let source_range = math_outer_range(self.source, range, display);
                let raw = Box::<str>::from(self.slice(&source_range));
                out.push(Inline::Math {
                    literal,
                    display,
                    raw,
                    source_range,
                    marks: ctx.marks,
                });
            }
            NodeValue::WikiLink(link) => {
                let mut label = String::new();
                collect_text(node, &mut label);
                if label.is_empty() {
                    label = link.url.clone();
                }
                let raw = Box::<str>::from(self.slice(&range));
                out.push(Inline::WikiLink {
                    target: link.url.clone(),
                    label,
                    raw,
                    source_range: range,
                    marks: ctx.marks,
                });
            }
            // Inline HTML, footnote refs, leftover unknowns: verbatim.
            _ => {
                let slice = self.slice(&range);
                out.push(Inline::OpaqueInline {
                    raw: Box::from(slice),
                    source_range: range,
                    marks: ctx.marks,
                });
            }
        }
    }

    /// Inner destination range of a link (child text), falling back to the
    /// link node's sourcepos when comrak omits children.
    fn link_inner_range<'a>(
        &self,
        node: &'a AstNode<'a>,
        fallback: std::ops::Range<usize>,
    ) -> std::ops::Range<usize> {
        let mut start: Option<usize> = None;
        let mut end: Option<usize> = None;
        for child in node.children() {
            let r = self.node_range(child);
            start = Some(start.map_or(r.start, |s| s.min(r.start)));
            end = Some(end.map_or(r.end, |e| e.max(r.end)));
        }
        match (start, end) {
            (Some(s), Some(e)) if e >= s => s..e,
            _ => fallback,
        }
    }
}

/// Comrak text sourcepos for `a  \nb` covers the trailing two spaces that
/// `expand_hard_break` claims as dest chrome. Trim the previous run so
/// Left/Right do not sit on those spaces.
fn trim_inlines_ending_in_hard_break(
    source: &str,
    out: &mut [Inline],
    hard: &std::ops::Range<usize>,
) {
    let Some(prev) = out.last_mut() else {
        return;
    };
    let range = match prev {
        Inline::Run { source_range, .. }
        | Inline::Image { source_range, .. }
        | Inline::Math { source_range, .. }
        | Inline::WikiLink { source_range, .. }
        | Inline::Emoji { source_range, .. }
        | Inline::OpaqueInline { source_range, .. } => source_range,
        Inline::SoftBreak { .. } | Inline::HardBreak { .. } => return,
    };
    if range.end <= hard.start || range.start >= hard.end {
        return;
    }
    range.end = hard.start.max(range.start);
    if let Inline::Run {
        source_range,
        raw: Some(r),
        ..
    } = prev
    {
        *r = Box::<str>::from(source.get(source_range.clone()).unwrap_or(""));
    }
}

/// CommonMark `<https://…>` / `<user@host>` wrapping, whether comrak's
/// link sourcepos includes the brackets or only the inner destination.
/// HTML phrasing (`<b>hello</b>`, `<a href>`) is not autolink `<>`.
fn wrapped_in_angle_brackets(source: &str, range: &std::ops::Range<usize>) -> bool {
    let bytes = source.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let start = range.start.min(bytes.len());
    let end = range.end.min(bytes.len()).max(start);
    if end > start + 1 && bytes[start] == b'<' && bytes[end - 1] == b'>' {
        return !angle_wrappers_are_html(&source[start..end]);
    }
    let open = start > 0 && bytes[start - 1] == b'<';
    let close = bytes.get(end) == Some(&b'>');
    if open && close {
        let lo = start - 1;
        let hi = (end + 1).min(source.len());
        return !angle_wrappers_are_html(&source[lo..hi]);
    }
    false
}

/// GFM extended autolinks (`https://`, `ftp://`, `www.`, bare email).
/// Comrak often reports the link (and neighbor text) sourcepos as `0..1`
/// or a list/quote marker, so caret walks `source[4..1]` and panics.
fn is_gfm_extended_autolink(text: &str, url: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    if url == text {
        return text.contains("://") || has_www_prefix(text);
    }
    if has_www_prefix(text) && url == format!("http://{text}") {
        return true;
    }
    text.contains('@') && !text.contains([' ', '\n']) && url == format!("mailto:{text}")
}

fn has_www_prefix(text: &str) -> bool {
    text.len() >= 4 && text.as_bytes()[..4].eq_ignore_ascii_case(b"www.")
}

fn is_markdown_link_label_at(source: &str, start: usize, end: usize) -> bool {
    let bytes = source.as_bytes();
    start > 0 && bytes[start - 1] == b'[' && bytes.get(end) == Some(&b']')
}

fn find_literal_from(
    source: &str,
    needle: &str,
    from: usize,
    hi: usize,
) -> Option<std::ops::Range<usize>> {
    if needle.is_empty() || from >= hi {
        return None;
    }
    let window = source.get(from..hi)?;
    let rel = window.find(needle)?;
    Some(from + rel..from + rel + needle.len())
}

fn find_gfm_autolink_literal(
    source: &str,
    needle: &str,
    from: usize,
    hi: usize,
) -> Option<std::ops::Range<usize>> {
    let mut search = from;
    while let Some(range) = find_literal_from(source, needle, search, hi) {
        if !is_markdown_link_label_at(source, range.start, range.end) {
            return Some(range);
        }
        search = range.start.saturating_add(1);
        if search >= hi {
            break;
        }
    }
    None
}

fn mark_gfm_extended_autolink(
    source: &str,
    range: &std::ops::Range<usize>,
    text: &str,
    link: &mut LinkAttrs,
) {
    if is_markdown_link_label_at(source, range.start, range.end) {
        if is_gfm_extended_autolink(text, &link.url) {
            link.autolink = false;
            link.angle = false;
        }
        return;
    }
    if wrapped_in_angle_brackets(source, range) {
        link.angle = true;
        // `<https://…>` visible text equals dest. `<email>` dest is
        // `mailto:email` — leaving autolink false keeps Normalize HTML.
        link.autolink = link.url == text;
        return;
    }
    if is_gfm_extended_autolink(text, &link.url) {
        link.angle = false;
        link.autolink = link.url == text;
    }
}

/// Recover GFM extended-autolink (and overlapping neighbor) source ranges
/// when comrak left `0..1` / marker sourcepos. Character references and
/// backslash escapes are split afterwards (`split_character_reference_inlines`).
fn repair_gfm_autolink_inlines(
    source: &str,
    block_range: std::ops::Range<usize>,
    inlines: &mut [Inline],
) {
    let lo = block_range.start.min(source.len());
    let hi = block_range.end.min(source.len()).max(lo);
    let mut cursor = lo;
    for inline in inlines.iter_mut() {
        match inline {
            Inline::Run {
                text,
                raw,
                source_range,
                marks,
                link,
                ..
            } => {
                let slice_matches = source.get(source_range.clone()) == Some(text.as_str());
                if slice_matches && source_range.start >= cursor {
                    if let Some(link) = link.as_mut() {
                        mark_gfm_extended_autolink(source, source_range, text, link);
                    }
                    cursor = source_range.end;
                    continue;
                }
                if marks.contains(MarkSet::CODE) {
                    cursor = cursor.max(source_range.end);
                    continue;
                }
                let found = if link
                    .as_ref()
                    .is_some_and(|l| is_gfm_extended_autolink(text, &l.url))
                {
                    find_gfm_autolink_literal(source, text, cursor, hi)
                        .or_else(|| find_literal_from(source, text, cursor, hi))
                } else {
                    find_literal_from(source, text, cursor, hi)
                };
                let Some(found) = found else {
                    cursor = cursor.max(source_range.end);
                    continue;
                };
                *source_range = found.clone();
                *raw = Some(Box::<str>::from(source.get(found.clone()).unwrap_or("")));
                if let Some(link) = link.as_mut() {
                    mark_gfm_extended_autolink(source, &found, text, link);
                }
                cursor = found.end;
            }
            other => {
                cursor = cursor.max(other.source_range().end);
            }
        }
    }
}

fn angle_wrappers_are_html(slice: &str) -> bool {
    let t = slice.trim();
    if t.len() < 2 || !t.starts_with('<') || !t.ends_with('>') {
        return false;
    }
    if t.contains("</") {
        return true;
    }
    crate::html_visual::opaque_inline_is_caret_chrome(t)
        || crate::html_visual::html_inline_break(t)
        || crate::html_visual::html_inline_image(t).is_some()
}

/// Expand comrak's inner math sourcepos to include `$` / `$$`.
///
/// Quoted / list display math places `>` or continuation indent between the
/// inner TeX and the closing `$$` (`> $$\n> E=mc^2\n> $$`). Those prefixes
/// are skipped so the outer span still covers both delimiters.
pub(crate) fn math_outer_range(
    source: &str,
    inner: std::ops::Range<usize>,
    display: bool,
) -> std::ops::Range<usize> {
    let delim = if display { "$$" } else { "$" };
    let start = math_delim_before(source, inner.start, delim).unwrap_or(inner.start);
    let end = math_delim_after(source, inner.end, delim).unwrap_or(inner.end);
    start..end
}

fn math_delim_before(source: &str, at: usize, delim: &str) -> Option<usize> {
    let w = delim.len();
    if at >= w && source.get(at - w..at) == Some(delim) {
        Some(at - w)
    } else {
        None
    }
}

fn math_delim_after(source: &str, at: usize, delim: &str) -> Option<usize> {
    let w = delim.len();
    if source.get(at..at.saturating_add(w)) == Some(delim) {
        return Some(at + w);
    }
    let skipped = super::tree::skip_math_line_prefix(source, at, source.len());
    if skipped > at && source.get(skipped..skipped.saturating_add(w)) == Some(delim) {
        Some(skipped + w)
    } else {
        None
    }
}

struct InlineCtx {
    marks: MarkSet,
    link: Option<LinkAttrs>,
    fidelity: MarkFidelity,
}

/// Comrak's tasklist scanner matches `[x]` at EOF. GFM requires a space after
/// `]`, so `[x]` / `[ ]` / `[X]` with no following space or tab is not a task.
fn nongfm_eol_checkbox_slot(source: &str, item: &Block) -> Option<std::ops::Range<usize>> {
    if !matches!(item.kind, BlockKind::ListItem { task: Some(_) }) {
        return None;
    }
    let prefix = super::engine::raw_container_prefix(source, item);
    let start = item.source_range.start.min(source.len());
    let ls = source[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let le = source[ls..]
        .find('\n')
        .map(|i| ls + i)
        .unwrap_or(source.len());
    let line = source.get(ls..le)?;
    if !line.starts_with(&prefix) {
        return None;
    }
    let after = &line[prefix.len()..];
    if !(after.starts_with("[ ]") || after.starts_with("[x]") || after.starts_with("[X]")) {
        return None;
    }
    match after.as_bytes().get(3) {
        Some(b' ' | b'\t') => None,
        _ => {
            let slot = ls + prefix.len();
            Some(slot..slot + 3)
        }
    }
}

fn cover_children(block: &mut Block) {
    for child in &block.children {
        if child.source_range.start < block.source_range.start {
            block.source_range.start = child.source_range.start;
        }
        if child.source_range.end > block.source_range.end {
            block.source_range.end = child.source_range.end;
        }
    }
}

/// Comrak only emits FootnoteReference when a matching `[^label]:` definition
/// exists. Split unmatched `[^1]` out of text runs so WYSIWYG can paint and
/// delete them as the same widget as matched refs.
fn apply_unmatched_footnote_refs(inlines: &mut Vec<Inline>) {
    if !inlines.iter().any(run_may_have_unmatched_footnote_ref) {
        return;
    }
    let mut out = Vec::with_capacity(inlines.len());
    for inline in inlines.drain(..) {
        match inline {
            Inline::Run {
                text,
                raw,
                source_range,
                marks,
                link: None,
                fidelity,
            } if !marks.contains(MarkSet::CODE) && source_range.len() == text.len() => {
                let found = crate::html_visual::find_unmatched_footnote_refs(&text);
                if found.is_empty() {
                    out.push(Inline::Run {
                        text,
                        raw,
                        source_range,
                        marks,
                        link: None,
                        fidelity,
                    });
                    continue;
                }
                let _ = raw;
                let mut cur = 0usize;
                for range in found {
                    if range.start > cur {
                        let src_start = source_range.start + cur;
                        let src_end = source_range.start + range.start;
                        out.push(Inline::Run {
                            text: text[cur..range.start].to_string(),
                            raw: None,
                            source_range: src_start..src_end,
                            marks,
                            link: None,
                            fidelity,
                        });
                    }
                    let src_start = source_range.start + range.start;
                    let src_end = source_range.start + range.end;
                    out.push(Inline::OpaqueInline {
                        raw: Box::from(&text[range.clone()]),
                        source_range: src_start..src_end,
                        marks,
                    });
                    cur = range.end;
                }
                if cur < text.len() {
                    let src_start = source_range.start + cur;
                    out.push(Inline::Run {
                        text: text[cur..].to_string(),
                        raw: None,
                        source_range: src_start..source_range.end,
                        marks,
                        link: None,
                        fidelity,
                    });
                }
            }
            other => out.push(other),
        }
    }
    *inlines = out;
}

fn run_may_have_unmatched_footnote_ref(inline: &Inline) -> bool {
    match inline {
        Inline::Run {
            text,
            marks,
            link: None,
            ..
        } => !marks.contains(MarkSet::CODE) && text.contains("[^"),
        _ => false,
    }
}

/// Comrak splits `<svg>…</svg>` into open tag / inner / close HtmlInlines.
/// Join a complete safe SVG into one opaque widget so it paints as an image.
fn merge_inline_svg(inlines: &mut Vec<Inline>) {
    let mut i = 0;
    while i < inlines.len() {
        let (raw0, start, marks) = match &inlines[i] {
            Inline::OpaqueInline {
                raw,
                source_range,
                marks,
            } if starts_svg_open(raw) => {
                if crate::html_visual::html_inline_svg(raw).is_some() {
                    i += 1;
                    continue;
                }
                (raw.to_string(), source_range.start, *marks)
            }
            _ => {
                i += 1;
                continue;
            }
        };
        let mut combined = raw0;
        let mut end = inlines[i].source_range().end;
        let mut j = i + 1;
        while j < inlines.len() {
            match &inlines[j] {
                Inline::OpaqueInline {
                    raw, source_range, ..
                } => {
                    combined.push_str(raw);
                    end = source_range.end;
                    j += 1;
                }
                Inline::Run {
                    text,
                    source_range,
                    marks: run_marks,
                    link: None,
                    ..
                } if *run_marks == marks => {
                    combined.push_str(text);
                    end = source_range.end;
                    j += 1;
                }
                _ => break,
            }
            if crate::html_visual::html_inline_svg(&combined).is_some() {
                break;
            }
        }
        if crate::html_visual::html_inline_svg(&combined).is_some() {
            inlines.splice(
                i..j,
                [Inline::OpaqueInline {
                    raw: combined.into(),
                    source_range: start..end,
                    marks,
                }],
            );
        }
        i += 1;
    }
}

fn starts_svg_open(raw: &str) -> bool {
    let t = raw.trim_start().as_bytes();
    t.len() >= 4
        && t[..4].eq_ignore_ascii_case(b"<svg")
        && (t.len() == 4 || matches!(t[4], b'>' | b'/' | b'\t' | b'\n' | b'\r' | b' '))
}

#[derive(Clone, Copy)]
struct EqDelim {
    inline_i: usize,
    off: usize,
}

/// Typora `==highlight==` is not a comrak node. Split matched pairs out of
/// text runs (delimiters stay in the source gap, like `**`) and set
/// [`MarkSet::HIGHLIGHT`]. Unmatched `==` stays visible. Empty `====` is
/// not a pair. Spans that contain a break are left alone (single-line).
fn apply_eqeq_highlight(inlines: &mut Vec<Inline>, groups: &std::cell::Cell<u64>) {
    let delims = find_eqeq_delims(inlines);
    let mut pairs: Vec<(EqDelim, EqDelim, u64)> = Vec::new();
    let mut i = 0;
    while i + 1 < delims.len() {
        let a = delims[i];
        let b = delims[i + 1];
        let empty = a.inline_i == b.inline_i && b.off == a.off + 2;
        let across_break = b.inline_i > a.inline_i
            && inlines[a.inline_i + 1..b.inline_i]
                .iter()
                .any(|n| matches!(n, Inline::SoftBreak { .. } | Inline::HardBreak { .. }));
        if empty || across_break {
            i += 1;
            continue;
        }
        groups.set(groups.get() + 1);
        pairs.push((a, b, groups.get()));
        i += 2;
    }
    if pairs.is_empty() {
        return;
    }
    *inlines = rebuild_with_highlight(inlines, &pairs);
}

fn find_eqeq_delims(inlines: &[Inline]) -> Vec<EqDelim> {
    let mut out = Vec::new();
    for (inline_i, inline) in inlines.iter().enumerate() {
        let Inline::Run { text, marks, .. } = inline else {
            continue;
        };
        if marks.contains(MarkSet::CODE) {
            continue;
        }
        let bytes = text.as_bytes();
        let mut j = 0;
        while j + 1 < bytes.len() {
            if bytes[j] == b'=' && bytes[j + 1] == b'=' {
                if j > 0 && bytes[j - 1] == b'\\' {
                    j += 1;
                    continue;
                }
                out.push(EqDelim { inline_i, off: j });
                j += 2;
            } else {
                j += 1;
            }
        }
    }
    out
}

fn is_eqeq_chrome(pairs: &[(EqDelim, EqDelim, u64)], inline_i: usize, off: usize) -> bool {
    pairs.iter().any(|(a, b, _)| {
        (a.inline_i == inline_i && a.off == off) || (b.inline_i == inline_i && b.off == off)
    })
}

fn highlight_group_at(
    pairs: &[(EqDelim, EqDelim, u64)],
    inline_i: usize,
    off: usize,
) -> Option<u64> {
    pairs.iter().find_map(|(a, b, g)| {
        let inside = if a.inline_i == b.inline_i {
            inline_i == a.inline_i && off >= a.off + 2 && off < b.off
        } else if inline_i == a.inline_i {
            off >= a.off + 2
        } else if inline_i == b.inline_i {
            off < b.off
        } else {
            inline_i > a.inline_i && inline_i < b.inline_i
        };
        inside.then_some(*g)
    })
}

fn highlight_group_for_inline(pairs: &[(EqDelim, EqDelim, u64)], inline_i: usize) -> Option<u64> {
    pairs
        .iter()
        .find_map(|(a, b, g)| (inline_i > a.inline_i && inline_i < b.inline_i).then_some(*g))
}

fn mapped_source(src: &std::ops::Range<usize>, text_len: usize, off: usize) -> usize {
    if src.len() == text_len {
        src.start + off.min(text_len)
    } else {
        src.len()
            .saturating_mul(off.min(text_len))
            .checked_div(text_len)
            .map(|n| src.start + n)
            .unwrap_or(src.start)
    }
}

fn rebuild_with_highlight(inlines: &[Inline], pairs: &[(EqDelim, EqDelim, u64)]) -> Vec<Inline> {
    let mut out = Vec::with_capacity(inlines.len());
    for (inline_i, inline) in inlines.iter().enumerate() {
        match inline {
            Inline::Run {
                text,
                source_range,
                marks,
                link,
                fidelity,
                raw,
            } => {
                let mut start = 0usize;
                while start < text.len() {
                    if is_eqeq_chrome(pairs, inline_i, start) {
                        start += 2;
                        continue;
                    }
                    let group = highlight_group_at(pairs, inline_i, start);
                    let mut end = start;
                    for (rel, ch) in text[start..].char_indices() {
                        let abs = start + rel;
                        if rel > 0
                            && (is_eqeq_chrome(pairs, inline_i, abs)
                                || highlight_group_at(pairs, inline_i, abs) != group)
                        {
                            break;
                        }
                        end = abs + ch.len_utf8();
                    }
                    if end <= start {
                        break;
                    }
                    let mut fid = *fidelity;
                    let mut m = *marks;
                    if let Some(g) = group {
                        m = m.with(MarkSet::HIGHLIGHT);
                        fid.highlight_group = g;
                    }
                    let src_start = mapped_source(source_range, text.len(), start);
                    let src_end = mapped_source(source_range, text.len(), end);
                    out.push(Inline::Run {
                        text: text[start..end].to_string(),
                        raw: if start == 0 && end == text.len() {
                            raw.clone()
                        } else {
                            None
                        },
                        source_range: src_start..src_end.max(src_start),
                        marks: m,
                        link: link.clone(),
                        fidelity: fid,
                    });
                    start = end;
                }
            }
            Inline::Image {
                alt,
                url,
                title,
                source_range,
                marks,
                link,
            } => {
                let mut m = *marks;
                if highlight_group_for_inline(pairs, inline_i).is_some() {
                    m = m.with(MarkSet::HIGHLIGHT);
                }
                out.push(Inline::Image {
                    alt: alt.clone(),
                    url: url.clone(),
                    title: title.clone(),
                    source_range: source_range.clone(),
                    marks: m,
                    link: link.clone(),
                });
            }
            Inline::OpaqueInline {
                raw,
                source_range,
                marks,
            } => {
                let mut m = *marks;
                if highlight_group_for_inline(pairs, inline_i).is_some() {
                    m = m.with(MarkSet::HIGHLIGHT);
                }
                out.push(Inline::OpaqueInline {
                    raw: raw.clone(),
                    source_range: source_range.clone(),
                    marks: m,
                });
            }
            Inline::Math {
                literal,
                display,
                raw,
                source_range,
                marks,
            } => {
                let mut m = *marks;
                if highlight_group_for_inline(pairs, inline_i).is_some() {
                    m = m.with(MarkSet::HIGHLIGHT);
                }
                out.push(Inline::Math {
                    literal: literal.clone(),
                    display: *display,
                    raw: raw.clone(),
                    source_range: source_range.clone(),
                    marks: m,
                });
            }
            Inline::WikiLink {
                target,
                label,
                raw,
                source_range,
                marks,
            } => {
                let mut m = *marks;
                if highlight_group_for_inline(pairs, inline_i).is_some() {
                    m = m.with(MarkSet::HIGHLIGHT);
                }
                out.push(Inline::WikiLink {
                    target: target.clone(),
                    label: label.clone(),
                    raw: raw.clone(),
                    source_range: source_range.clone(),
                    marks: m,
                });
            }
            Inline::Emoji {
                name,
                glyph,
                raw,
                source_range,
                marks,
                link,
                fidelity,
            } => {
                let mut m = *marks;
                let mut fid = *fidelity;
                if let Some(g) = highlight_group_for_inline(pairs, inline_i) {
                    m = m.with(MarkSet::HIGHLIGHT);
                    fid.highlight_group = g;
                }
                out.push(Inline::Emoji {
                    name: name.clone(),
                    glyph: glyph.clone(),
                    raw: raw.clone(),
                    source_range: source_range.clone(),
                    marks: m,
                    link: link.clone(),
                    fidelity: fid,
                });
            }
            other => out.push(other.clone()),
        }
    }
    out
}

fn collect_text<'a>(node: &'a AstNode<'a>, out: &mut String) {
    for child in node.children() {
        match &child.data.borrow().value {
            NodeValue::Text(t) => out.push_str(t),
            NodeValue::Code(c) => out.push_str(&c.literal),
            NodeValue::SoftBreak | NodeValue::LineBreak => out.push(' '),
            _ => {}
        }
        collect_text(child, out);
    }
}

/// Comrak stores `[label]: dest` in a private refmap and detaches those
/// lines from the AST. Recover them as WYSIWYG leaves in source gaps so
/// they stay visible, editable, and round-trip.
///
/// A definition nested in the same paragraph as `[hello][ref]` (typical
/// list/quote lazy continuation) never makes the refmap, so comrak leaves
/// the link as a bracketed text run. After peeling the def, re-resolve
/// those runs so dest-chrome skip still sees `Link` attrs. Wrapping
/// `[![img](url)][ref]` / `[![alt][pic]][ref]` keeps the image plus the
/// outer dest (nested `[]` in link text is not treated as a shortcut).
fn recover_link_reference_definitions(tree: &mut RichTree, source: &str, ids: &mut IdGen) {
    let fm_end = super::engine::frontmatter_body_start(tree);
    let mut groups = 0u64;
    let mut peeled = HashSet::new();
    insert_link_reference_defs(
        &mut tree.blocks,
        fm_end..source.len(),
        source,
        ids,
        &mut groups,
        &mut peeled,
    );
    let defs = collect_link_reference_defs(&tree.blocks);
    if defs.is_empty() {
        return;
    }
    groups = groups.max(max_link_group(&tree.blocks));
    if !peeled.is_empty() {
        resolve_flattened_reference_links(&mut tree.blocks, source, &defs, &mut groups, &peeled);
    }
    refresh_stale_reference_urls(&mut tree.blocks, source, &defs);
    repair_source_breaks_in_blocks(&mut tree.blocks, source);
}

fn insert_link_reference_defs(
    blocks: &mut Vec<Block>,
    parent: std::ops::Range<usize>,
    source: &str,
    ids: &mut IdGen,
    groups: &mut u64,
    peeled: &mut HashSet<NodeId>,
) {
    for block in blocks.iter_mut() {
        if block.is_container() {
            let range = block.source_range.clone();
            insert_link_reference_defs(&mut block.children, range, source, ids, groups, peeled);
        }
    }
    let mut extras = Vec::new();
    let mut cursor = parent.start;
    for block in blocks.iter() {
        let hi = block.source_range.start.min(parent.end).max(cursor);
        if hi > cursor {
            scan_link_reference_defs(source, cursor..hi, ids, groups, &mut extras, 0);
        }
        cursor = block.source_range.end.max(cursor).min(parent.end);
    }
    if cursor < parent.end {
        scan_link_reference_defs(source, cursor..parent.end, ids, groups, &mut extras, 0);
    }
    for block in blocks.iter_mut() {
        let more = peel_trailing_link_defs(block, source, ids, groups);
        if !more.is_empty() {
            peeled.insert(block.id);
            extras.extend(more);
        }
    }
    if !extras.is_empty() {
        blocks.extend(extras);
        blocks.sort_by_key(|b| b.source_range.start);
    }
    promote_link_ref_defs(blocks, source, ids, groups);
    extend_wrapped_link_ref_defs(blocks, source, ids, groups);
}

fn scan_link_reference_defs(
    source: &str,
    gap: std::ops::Range<usize>,
    ids: &mut IdGen,
    groups: &mut u64,
    out: &mut Vec<Block>,
    base_column: usize,
) {
    if gap.start >= gap.end {
        return;
    }
    let mut ls = if gap.start == 0
        || source.as_bytes().get(gap.start.saturating_sub(1)).copied() == Some(b'\n')
    {
        gap.start
    } else {
        source[gap.start..gap.end]
            .find('\n')
            .map(|i| gap.start + i + 1)
            .unwrap_or(gap.end)
    };
    while ls < gap.end {
        match parse_link_reference_definition(source, ls, gap.end, base_column) {
            Some(parsed) if parsed.range.start >= gap.start && parsed.range.end <= gap.end => {
                let next = if parsed.range.end < source.len()
                    && source.as_bytes()[parsed.range.end] == b'\n'
                {
                    parsed.range.end + 1
                } else {
                    parsed.range.end
                };
                out.push(block_from_link_reference_def(parsed, source, ids, groups));
                ls = next.max(ls + 1);
            }
            _ => {
                ls = source[ls..gap.end]
                    .find('\n')
                    .map(|i| ls + i + 1)
                    .unwrap_or(gap.end);
            }
        }
    }
}

/// Column where a lazy list/quote continuation begins on the line containing
/// `at`: quote markers plus the list marker (`- ` / `1. `), **not** a GFM
/// task `[x] ` and not the first-line body. Nested `    [ref]:` inside
/// `  - item` is 0 spaces of definition indent, not indented-code 4. A task
/// `- [x] [hello][ref]` still peels a 2-space `[ref]:` (the checkbox is only
/// on the first line).
fn line_content_column(source: &str, at: usize) -> usize {
    let at = at.min(source.len());
    let ls = source
        .get(..at)
        .and_then(|s| s.rfind('\n'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let parts = super::engine::line_prefix_parts(source, at);
    let mut end = parts.list.end;
    if parts.list.start == parts.list.end {
        end = parts.quote.end;
    } else if end >= 4 {
        let checkbox = source.get(end - 4..end).unwrap_or("");
        if matches!(
            checkbox,
            "[ ] " | "[x] " | "[X] " | "[ ]\t" | "[x]\t" | "[X]\t"
        ) {
            end -= 4;
        }
    }
    end.saturating_sub(ls)
}

/// Comrak may keep `[ref]: dest` inside a paragraph/list-item sourcepos after
/// stripping it from inlines (it cannot interrupt a paragraph, but finalize
/// still absorbs the definition). Peel those trailing lines into siblings.
fn peel_trailing_link_defs(
    block: &mut Block,
    source: &str,
    ids: &mut IdGen,
    groups: &mut u64,
) -> Vec<Block> {
    if block.is_container()
        || matches!(
            block.kind,
            BlockKind::CodeBlock { .. }
                | BlockKind::Opaque { .. }
                | BlockKind::ThematicBreak
                | BlockKind::LinkReferenceDefinition { .. }
                | BlockKind::TableCell
        )
    {
        return Vec::new();
    }
    let start = block.source_range.start.min(source.len());
    let end = block.source_range.end.min(source.len());
    if start >= end {
        return Vec::new();
    }
    let Some(rel) = source[start..end].find('\n') else {
        return Vec::new();
    };
    let tail = start + rel + 1..end;
    let mut extras = Vec::new();
    let base_column = line_content_column(source, start);
    scan_link_reference_defs(source, tail, ids, groups, &mut extras, base_column);
    extras.retain(|def| def.source_range.start >= start && def.source_range.end <= end);
    let Some(first) = extras.first() else {
        return Vec::new();
    };
    let mut new_end = first.source_range.start;
    if new_end > start && source.as_bytes().get(new_end - 1) == Some(&b'\n') {
        new_end -= 1;
    }
    if new_end < start {
        return Vec::new();
    }
    // CommonMark: a definition cannot interrupt a paragraph (`Foo\n[bar]: /baz`
    // then a later `[bar]` is not a link). Only peel when the leftover body
    // itself contains a matching `[label][ref]` / `![alt][ref]` / shortcut.
    let extras_defs = collect_link_reference_defs(&extras);
    if extras_defs.is_empty()
        || scan_recovered_ref_links(source, start..new_end, &extras_defs).is_empty()
    {
        return Vec::new();
    }
    block.source_range.end = new_end;
    block.content_hash =
        super::engine::hash_str(source.get(block.source_range.clone()).unwrap_or(""));
    clip_inlines_to_source_range(block);
    extras
}

/// Comrak may keep a wrapped or empty `[ref]: dest` as a paragraph (dest
/// cannot wrap on a bare `\n`, and empty dest is not CommonMark). A setext
/// heading may also swallow a preceding definition into its title. Dest
/// wrap `\\\n` often lands in the next sibling paragraph — parse with a
/// sibling limit and absorb those leaves.
fn leftover_is_only_setext_underline(rest: &str) -> bool {
    let t = rest.trim();
    if t.is_empty() {
        return false;
    }
    let bytes = t.as_bytes();
    let all_eq = bytes
        .iter()
        .all(|&b| b == b'=' || b == b'\n' || b == b' ' || b == b'\t');
    let all_dash = bytes
        .iter()
        .all(|&b| b == b'-' || b == b'\n' || b == b' ' || b == b'\t');
    (all_eq && t.contains('=')) || (all_dash && t.contains('-'))
}

fn skip_link_ref_promote(block: &Block) -> bool {
    block.is_container()
        || matches!(
            block.kind,
            BlockKind::CodeBlock { .. }
                | BlockKind::Opaque { .. }
                | BlockKind::ThematicBreak
                | BlockKind::LinkReferenceDefinition { .. }
                | BlockKind::TableCell
                | BlockKind::Toc { .. }
        )
}

fn promote_link_ref_defs(blocks: &mut Vec<Block>, source: &str, ids: &mut IdGen, groups: &mut u64) {
    let mut i = 0;
    while i < blocks.len() {
        if skip_link_ref_promote(&blocks[i]) {
            i += 1;
            continue;
        }
        let start = blocks[i].source_range.start.min(source.len());
        let sibling_limit = blocks[i..]
            .iter()
            .take_while(|b| !skip_link_ref_promote(b))
            .last()
            .map(|b| b.source_range.end)
            .unwrap_or(blocks[i].source_range.end)
            .min(source.len());
        let base_column = line_content_column(source, start);
        let Some(parsed) =
            parse_link_reference_definition(source, start, sibling_limit, base_column)
        else {
            i += 1;
            continue;
        };
        if parsed.range.start < start {
            i += 1;
            continue;
        }
        let mut last = i;
        for (k, b) in blocks.iter().enumerate().skip(i) {
            if b.source_range.start >= parsed.range.end {
                break;
            }
            last = k;
        }
        let first_end = blocks[i].source_range.end.min(source.len());
        if parsed.range.end <= first_end {
            let rest = source.get(parsed.range.end..first_end).unwrap_or("");
            let rest_body = rest.strip_prefix('\n').unwrap_or(rest);
            if rest_body.trim().is_empty() {
                blocks[i] = block_from_link_reference_def(parsed, source, ids, groups);
                i += 1;
                continue;
            }
            if matches!(blocks[i].kind, BlockKind::Heading { .. })
                && leftover_is_only_setext_underline(rest_body)
            {
                i += 1;
                continue;
            }
            let new_start = if source.as_bytes().get(parsed.range.end) == Some(&b'\n') {
                parsed.range.end + 1
            } else {
                parsed.range.end
            };
            if new_start >= first_end {
                blocks[i] = block_from_link_reference_def(parsed, source, ids, groups);
                i += 1;
                continue;
            }
            let def = block_from_link_reference_def(parsed, source, ids, groups);
            blocks[i].source_range.start = new_start;
            blocks[i].content_hash =
                super::engine::hash_str(source.get(blocks[i].source_range.clone()).unwrap_or(""));
            clip_inlines_to_source_range(&mut blocks[i]);
            blocks.insert(i, def);
            i += 2;
            continue;
        }
        let def = block_from_link_reference_def(parsed, source, ids, groups);
        blocks.drain(i..=last);
        blocks.insert(i, def);
        i += 1;
    }
}

/// Gap scan stops at the next block, so `[ref]: dest\\\nth` becomes a
/// definition whose dest still has the wrapping `\` plus a leftover
/// paragraph `th`. Re-parse with the sibling in range and absorb it.
fn extend_wrapped_link_ref_defs(
    blocks: &mut Vec<Block>,
    source: &str,
    ids: &mut IdGen,
    groups: &mut u64,
) {
    let mut i = 0;
    while i + 1 < blocks.len() {
        if !matches!(blocks[i].kind, BlockKind::LinkReferenceDefinition { .. }) {
            i += 1;
            continue;
        }
        if skip_link_ref_promote(&blocks[i + 1]) {
            i += 1;
            continue;
        }
        let start = blocks[i].source_range.start.min(source.len());
        let def_end = blocks[i].source_range.end.min(source.len());
        let sibling_limit = blocks[i + 1].source_range.end.min(source.len());
        if sibling_limit <= def_end {
            i += 1;
            continue;
        }
        let base_column = line_content_column(source, start);
        let Some(parsed) =
            parse_link_reference_definition(source, start, sibling_limit, base_column)
        else {
            i += 1;
            continue;
        };
        if parsed.range.end <= def_end {
            i += 1;
            continue;
        }
        let mut last = i + 1;
        for (k, b) in blocks.iter().enumerate().skip(i + 1) {
            if b.source_range.start >= parsed.range.end {
                break;
            }
            last = k;
        }
        let def = block_from_link_reference_def(parsed, source, ids, groups);
        blocks.drain(i..=last);
        blocks.insert(i, def);
        i += 1;
    }
}

/// Drop inlines that comrak left behind after a trailing `[ref]: dest` was
/// peeled out (including a bogus autolink whose sourcepos is the list marker).
fn clip_inlines_to_source_range(block: &mut Block) {
    let range = block.source_range.clone();
    block.inlines.retain(|inline| {
        let r = inline.source_range();
        r.start < r.end && r.start >= range.start && r.end <= range.end
    });
}

fn collect_link_reference_defs(blocks: &[Block]) -> HashMap<String, (String, Option<String>)> {
    let mut map = HashMap::new();
    collect_link_reference_defs_into(blocks, &mut map);
    map
}

fn collect_link_reference_defs_into(
    blocks: &[Block],
    map: &mut HashMap<String, (String, Option<String>)>,
) {
    for block in blocks {
        if let BlockKind::LinkReferenceDefinition { label, url, title } = &block.kind {
            if !url.is_empty() {
                map.entry(normalize_link_label(label))
                    .or_insert((url.clone(), title.clone()));
            }
        }
        collect_link_reference_defs_into(&block.children, map);
    }
}

fn max_link_group(blocks: &[Block]) -> u64 {
    let mut max = 0u64;
    for block in blocks {
        for inline in &block.inlines {
            if let Some(group) = inline_link_group(inline) {
                max = max.max(group);
            }
        }
        max = max.max(max_link_group(&block.children));
    }
    max
}

fn inline_link_group(inline: &Inline) -> Option<u64> {
    match inline {
        Inline::Run { link, .. } | Inline::Image { link, .. } | Inline::Emoji { link, .. } => {
            link.as_ref().map(|l| l.group)
        }
        _ => None,
    }
}

/// CommonMark link-label match key: trim, collapse whitespace, case-fold.
fn normalize_link_label(label: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for c in label.chars() {
        if c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        for d in c.to_lowercase() {
            out.push(d);
        }
    }
    out
}

fn resolve_flattened_reference_links(
    blocks: &mut [Block],
    source: &str,
    defs: &HashMap<String, (String, Option<String>)>,
    groups: &mut u64,
    peeled: &HashSet<NodeId>,
) {
    for block in blocks {
        if matches!(
            block.kind,
            BlockKind::LinkReferenceDefinition { .. }
                | BlockKind::CodeBlock { .. }
                | BlockKind::Opaque { .. }
                | BlockKind::Toc { .. }
        ) {
            continue;
        }
        if peeled.contains(&block.id) && !block.inlines.is_empty() {
            apply_recovered_reference_links(
                &mut block.inlines,
                source,
                defs,
                groups,
                block.source_range.clone(),
            );
        }
        resolve_flattened_reference_links(&mut block.children, source, defs, groups, peeled);
    }
}

/// Dest wrap concatenates `pa\\\nth` after comrak already resolved
/// `[hello][ref]` to `pa\`. Update those link attrs; do not invent new
/// refs inside HTML / code / `[foo][bar][baz]` (CommonMark 567).
fn refresh_stale_reference_urls(
    blocks: &mut [Block],
    source: &str,
    defs: &HashMap<String, (String, Option<String>)>,
) {
    for block in blocks {
        if matches!(
            block.kind,
            BlockKind::LinkReferenceDefinition { .. }
                | BlockKind::CodeBlock { .. }
                | BlockKind::Opaque { .. }
                | BlockKind::Toc { .. }
        ) {
            refresh_stale_reference_urls(&mut block.children, source, defs);
            continue;
        }
        if !block.inlines.is_empty() {
            refresh_stale_ref_urls_in(&mut block.inlines, source, defs, block.source_range.clone());
        }
        refresh_stale_reference_urls(&mut block.children, source, defs);
    }
}

fn repair_source_breaks_in_blocks(blocks: &mut [Block], source: &str) {
    for block in blocks {
        insert_missing_source_breaks(source, &mut block.inlines);
        repair_source_breaks_in_blocks(&mut block.children, source);
    }
}

fn refresh_stale_ref_urls_in(
    inlines: &mut [Inline],
    source: &str,
    defs: &HashMap<String, (String, Option<String>)>,
    scan: Range<usize>,
) {
    for rec in scan_recovered_ref_links(source, scan, defs) {
        if recovered_ref_is_inside_immune_span(inlines, &rec) {
            continue;
        }
        for inline in inlines.iter_mut() {
            let r = inline.source_range();
            if r.end <= rec.inner.start || r.start >= rec.outer.end {
                continue;
            }
            match inline {
                Inline::Run {
                    link: Some(link), ..
                }
                | Inline::Image {
                    link: Some(link), ..
                }
                | Inline::Emoji {
                    link: Some(link), ..
                } if !link.autolink => {
                    if link.url != rec.url {
                        link.url = rec.url.clone();
                        link.title = rec.title.clone();
                    }
                }
                Inline::Image { url, title, .. } if rec.image && url != &rec.url => {
                    *url = rec.url.clone();
                    *title = rec.title.clone();
                }
                _ => {}
            }
        }
    }
}

fn apply_recovered_reference_links(
    inlines: &mut Vec<Inline>,
    source: &str,
    defs: &HashMap<String, (String, Option<String>)>,
    groups: &mut u64,
    scan: Range<usize>,
) {
    if inlines.is_empty() {
        return;
    }
    let found: Vec<_> = scan_recovered_ref_links(source, scan, defs)
        .into_iter()
        .filter(|rec| {
            !recovered_ref_is_inside_immune_span(inlines, rec)
                && !recovered_ref_already_resolved(inlines, rec)
        })
        .collect();
    if found.is_empty() {
        return;
    }
    let mut remaining = std::mem::take(inlines);
    let mut out = Vec::new();
    for rec in &found {
        let (before, overlap, after) = partition_around_outer(remaining, &rec.outer, source);
        out.extend(before);
        out.extend(emit_recovered_ref(overlap, rec, source, defs, groups));
        remaining = after;
    }
    out.extend(remaining);
    *inlines = out;
}

fn recovered_ref_is_inside_immune_span(inlines: &[Inline], rec: &RecoveredRefLink) -> bool {
    inlines.iter().any(|inline| {
        let r = inline.source_range();
        if r.start > rec.outer.start || r.end < rec.outer.end {
            return false;
        }
        match inline {
            Inline::Run { marks, .. } if marks.contains(MarkSet::CODE) => true,
            Inline::Math { .. } | Inline::WikiLink { .. } | Inline::Emoji { .. } => true,
            _ => false,
        }
    })
}

/// Comrak already attached dest when the definition was in the refmap.
/// Re-emitting those runs would drop nested images / marks. Dest wrap
/// can leave a stale `pa\` URL — only skip when dest already matches.
fn recovered_ref_already_resolved(inlines: &[Inline], rec: &RecoveredRefLink) -> bool {
    inlines.iter().any(|inline| {
        let r = inline.source_range();
        if r.end <= rec.inner.start || r.start >= rec.outer.end {
            return false;
        }
        match inline {
            Inline::Run {
                link: Some(link), ..
            }
            | Inline::Image {
                link: Some(link), ..
            } => link.url == rec.url,
            Inline::Image { url, .. } if rec.image => url == &rec.url,
            _ => false,
        }
    })
}

fn partition_around_outer(
    inlines: Vec<Inline>,
    outer: &Range<usize>,
    source: &str,
) -> (Vec<Inline>, Vec<Inline>, Vec<Inline>) {
    let mut before = Vec::new();
    let mut overlap = Vec::new();
    let mut after = Vec::new();
    for inline in inlines {
        let r = inline.source_range();
        if r.end <= outer.start {
            before.push(inline);
            continue;
        }
        if r.start >= outer.end {
            after.push(inline);
            continue;
        }
        if r.start >= outer.start && r.end <= outer.end {
            overlap.push(inline);
            continue;
        }
        match inline {
            Inline::Run {
                marks, fidelity, ..
            } => {
                if r.start < outer.start {
                    push_source_run(&mut before, source, r.start..outer.start, marks, fidelity);
                }
                let mid = r.start.max(outer.start)..r.end.min(outer.end);
                if mid.start < mid.end {
                    push_source_run(&mut overlap, source, mid, marks, fidelity);
                }
                if r.end > outer.end {
                    push_source_run(&mut after, source, outer.end..r.end, marks, fidelity);
                }
            }
            other => overlap.push(other),
        }
    }
    (before, overlap, after)
}

fn range_inside(inner: &Range<usize>, outer: &Range<usize>) -> bool {
    inner.start >= outer.start && inner.end <= outer.end && inner.start < inner.end
}

fn emit_recovered_ref(
    overlap: Vec<Inline>,
    rec: &RecoveredRefLink,
    source: &str,
    defs: &HashMap<String, (String, Option<String>)>,
    groups: &mut u64,
) -> Vec<Inline> {
    if rec.image {
        return emit_recovered_image(overlap, rec, source, groups);
    }
    let overlap = if source.as_bytes().get(rec.inner.start) == Some(&b'!')
        && source.as_bytes().get(rec.inner.start + 1) == Some(&b'[')
        && !overlap.iter().any(|inline| {
            matches!(inline, Inline::Image { .. })
                && range_inside(&inline.source_range(), &rec.inner)
        }) {
        if let Some(img_rec) = match_recovered_ref_link(
            source,
            rec.inner.start + 1,
            rec.inner.start,
            rec.inner.end.min(source.len()),
            defs,
            true,
        ) {
            emit_recovered_image(overlap, &img_rec, source, groups)
        } else {
            overlap
        }
    } else {
        overlap
    };
    *groups += 1;
    let group = *groups;
    let inner: Vec<Inline> = overlap
        .into_iter()
        .filter(|inline| range_inside(&inline.source_range(), &rec.inner))
        .map(|inline| attach_recovered_link(inline, rec, group))
        .collect();
    if inner.is_empty() {
        vec![synthesize_recovered_link_run(source, rec, group)]
    } else {
        inner
    }
}

fn emit_recovered_image(
    overlap: Vec<Inline>,
    rec: &RecoveredRefLink,
    source: &str,
    groups: &mut u64,
) -> Vec<Inline> {
    *groups += 1;
    let mut alt = String::new();
    let mut marks = MarkSet::empty();
    for inline in &overlap {
        if range_inside(&inline.source_range(), &rec.inner) {
            alt.push_str(&inline_plain_text(inline));
            if let Some(m) = inline_marks(inline) {
                marks = marks.with(m);
            }
        }
    }
    if alt.is_empty() {
        alt = source.get(rec.inner.clone()).unwrap_or("").to_string();
    }
    vec![Inline::Image {
        alt,
        url: rec.url.clone(),
        title: rec.title.clone(),
        source_range: rec.outer.clone(),
        marks,
        link: None,
    }]
}

fn inline_plain_text(inline: &Inline) -> String {
    match inline {
        Inline::Run { text, .. } => text.clone(),
        Inline::Image { alt, .. } => alt.clone(),
        Inline::Emoji { glyph, .. } => glyph.clone(),
        _ => String::new(),
    }
}

fn inline_marks(inline: &Inline) -> Option<MarkSet> {
    match inline {
        Inline::Run { marks, .. }
        | Inline::Image { marks, .. }
        | Inline::Math { marks, .. }
        | Inline::WikiLink { marks, .. }
        | Inline::Emoji { marks, .. }
        | Inline::OpaqueInline { marks, .. } => Some(*marks),
        _ => None,
    }
}

fn recovered_link_attrs(rec: &RecoveredRefLink, group: u64) -> LinkAttrs {
    LinkAttrs {
        url: rec.url.clone(),
        title: rec.title.clone(),
        autolink: false,
        angle: false,
        group,
    }
}

fn attach_recovered_link(inline: Inline, rec: &RecoveredRefLink, group: u64) -> Inline {
    let attrs = recovered_link_attrs(rec, group);
    match inline {
        Inline::Run {
            text,
            raw,
            source_range,
            marks,
            link: _,
            fidelity,
        } => Inline::Run {
            text,
            raw,
            source_range,
            marks,
            link: Some(attrs),
            fidelity,
        },
        Inline::Image {
            alt,
            url,
            title,
            source_range,
            marks,
            link: _,
        } => Inline::Image {
            alt,
            url,
            title,
            source_range,
            marks,
            link: Some(attrs),
        },
        Inline::Emoji {
            name,
            glyph,
            raw,
            source_range,
            marks,
            link: _,
            fidelity,
        } => Inline::Emoji {
            name,
            glyph,
            raw,
            source_range,
            marks,
            link: Some(attrs),
            fidelity,
        },
        other => other,
    }
}

fn synthesize_recovered_link_run(source: &str, rec: &RecoveredRefLink, group: u64) -> Inline {
    let inner = source.get(rec.inner.clone()).unwrap_or("").to_string();
    let raw = (!inner.is_empty()).then(|| Box::<str>::from(inner.as_str()));
    Inline::Run {
        text: inner,
        raw,
        source_range: rec.inner.clone(),
        marks: MarkSet::empty(),
        link: Some(recovered_link_attrs(rec, group)),
        fidelity: MarkFidelity::default(),
    }
}

fn push_source_run(
    out: &mut Vec<Inline>,
    source: &str,
    range: Range<usize>,
    marks: MarkSet,
    fidelity: MarkFidelity,
) {
    if range.start >= range.end {
        return;
    }
    let text = source.get(range.clone()).unwrap_or("").to_string();
    if text.is_empty() {
        return;
    }
    let raw = Some(Box::<str>::from(text.as_str()));
    out.push(Inline::Run {
        text,
        raw,
        source_range: range,
        marks,
        link: None,
        fidelity,
    });
}

struct RecoveredRefLink {
    outer: Range<usize>,
    inner: Range<usize>,
    image: bool,
    url: String,
    title: Option<String>,
}

fn scan_recovered_ref_links(
    source: &str,
    range: Range<usize>,
    defs: &HashMap<String, (String, Option<String>)>,
) -> Vec<RecoveredRefLink> {
    let bytes = source.as_bytes();
    let limit = range.end.min(source.len());
    let mut i = range.start.min(limit);
    let mut out = Vec::new();
    while i < limit {
        if odd_backslash_escape(bytes, i) {
            i += 1;
            continue;
        }
        if bytes[i] == b'!' && bytes.get(i + 1) == Some(&b'[') {
            if odd_backslash_escape(bytes, i + 1) {
                i += 1;
                continue;
            }
            if let Some(found) = match_recovered_ref_link(source, i + 1, i, limit, defs, true) {
                i = found.outer.end.max(i + 1);
                out.push(found);
                continue;
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'[' {
            if i > 0 && bytes[i - 1] == b'[' {
                i += 1;
                continue;
            }
            if i > 0 && bytes[i - 1] == b'!' {
                i += 1;
                continue;
            }
            if let Some(found) = match_recovered_ref_link(source, i, i, limit, defs, false) {
                i = found.outer.end.max(i + 1);
                out.push(found);
                continue;
            }
        }
        i += 1;
    }
    out
}

fn match_recovered_ref_link(
    source: &str,
    bracket: usize,
    outer_start: usize,
    limit: usize,
    defs: &HashMap<String, (String, Option<String>)>,
    image: bool,
) -> Option<RecoveredRefLink> {
    let (inner, after_first) = scan_inline_link_text(source, bracket, limit)?;
    let first_key = normalize_link_label(source.get(inner.clone())?);
    if first_key.is_empty() {
        return None;
    }
    let i = skip_spaces(source, after_first, limit);
    if source.as_bytes().get(i) == Some(&b'[') {
        if source.as_bytes().get(i + 1) == Some(&b']') {
            let dest = lookup_ref_def(defs, &first_key)?;
            return Some(RecoveredRefLink {
                outer: outer_start..i + 2,
                inner,
                image,
                url: dest.0,
                title: dest.1,
            });
        }
        if let Some((dest_inner, after_second)) = scan_inline_link_label(source, i, limit) {
            let dest_key = normalize_link_label(source.get(dest_inner.clone())?);
            if let Some(dest) = lookup_ref_def(defs, &dest_key) {
                return Some(RecoveredRefLink {
                    outer: outer_start..after_second,
                    inner,
                    image,
                    url: dest.0,
                    title: dest.1,
                });
            }
            let dest = lookup_ref_def(defs, &first_key)?;
            return Some(RecoveredRefLink {
                outer: outer_start..after_first,
                inner,
                image,
                url: dest.0,
                title: dest.1,
            });
        }
        return None;
    }
    if source.as_bytes().get(i) == Some(&b'(') {
        return None;
    }
    if source.as_bytes().get(after_first) == Some(&b':') {
        return None;
    }
    let dest = lookup_ref_def(defs, &first_key)?;
    Some(RecoveredRefLink {
        outer: outer_start..after_first,
        inner,
        image,
        url: dest.0,
        title: dest.1,
    })
}

fn lookup_ref_def(
    defs: &HashMap<String, (String, Option<String>)>,
    key: &str,
) -> Option<(String, Option<String>)> {
    defs.get(key).cloned()
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

/// Link *text* starting at `open`: `[![img](url)][ref]` may nest `[]`.
/// Footnotes (`[^`) and empty text are rejected.
fn scan_inline_link_text(source: &str, open: usize, limit: usize) -> Option<(Range<usize>, usize)> {
    scan_inline_link_brackets(source, open, limit, true)
}

/// Link *label* starting at `open`. Dest `[ref]` / collapsed keys cannot
/// contain unescaped `[` / `]`. Footnotes (`[^`) and empty labels are rejected.
fn scan_inline_link_label(
    source: &str,
    open: usize,
    limit: usize,
) -> Option<(Range<usize>, usize)> {
    scan_inline_link_brackets(source, open, limit, false)
}

fn scan_inline_link_brackets(
    source: &str,
    open: usize,
    limit: usize,
    allow_nested: bool,
) -> Option<(Range<usize>, usize)> {
    let bytes = source.as_bytes();
    if bytes.get(open) != Some(&b'[') {
        return None;
    }
    if bytes.get(open + 1) == Some(&b'^') {
        return None;
    }
    let mut i = open + 1;
    let inner_start = i;
    let mut depth = 1usize;
    let mut len = 0usize;
    while i < limit {
        match bytes[i] {
            b']' => {
                depth -= 1;
                if depth == 0 {
                    let inner = inner_start..i;
                    let label = source.get(inner.clone())?;
                    if label.trim().is_empty() {
                        return None;
                    }
                    return Some((inner, i + 1));
                }
                i += 1;
                len += 1;
            }
            b'[' => {
                if !allow_nested {
                    return None;
                }
                depth += 1;
                i += 1;
                len += 1;
            }
            b'\n' => return None,
            b'\\' if i + 1 < limit => {
                i += 2;
                len += 2;
            }
            _ => {
                i += 1;
                len += 1;
            }
        }
        if len > 999 {
            return None;
        }
    }
    None
}

struct ParsedLinkRefDef {
    range: std::ops::Range<usize>,
    label: String,
    label_range: std::ops::Range<usize>,
    url: String,
    dest_range: std::ops::Range<usize>,
    dest_segments: Vec<std::ops::Range<usize>>,
    angle: bool,
    title: Option<String>,
    title_range: Option<std::ops::Range<usize>>,
    colon_end: usize,
}

fn parse_link_reference_definition(
    source: &str,
    line_start: usize,
    limit: usize,
    base_column: usize,
) -> Option<ParsedLinkRefDef> {
    let limit = limit.min(source.len());
    if line_start >= limit {
        return None;
    }
    let parts = super::engine::line_prefix_parts(source, line_start);
    let mut i = parts.list.end.max(line_start);
    if i > limit {
        return None;
    }
    // `line_start` may be the `[` after `> ` / `- ` (comrak sourcepos).
    // Indent is measured from the physical line, not that mid-line offset.
    let physical_line = source[..line_start].rfind('\n').map(|n| n + 1).unwrap_or(0);
    let col = i.saturating_sub(physical_line);
    if col < base_column {
        let need = base_column - col;
        let spaces = source
            .get(i..limit)
            .unwrap_or("")
            .bytes()
            .take_while(|&b| b == b' ')
            .count();
        if spaces < need {
            return None;
        }
        i += need;
    }
    if source.as_bytes().get(i) == Some(&b'\t') {
        return None;
    }
    let spaces = source[i..limit].bytes().take_while(|&b| b == b' ').count();
    if spaces > 3 {
        return None;
    }
    i += spaces;
    if source.as_bytes().get(i) != Some(&b'[') {
        return None;
    }
    if source.as_bytes().get(i + 1) == Some(&b'^') {
        return None;
    }
    i += 1;
    let label_start = i;
    let mut len = 0usize;
    while i < limit {
        match source.as_bytes()[i] {
            b']' => break,
            b'[' | b'\n' => return None,
            b'\\' if i + 1 < limit => {
                i += 2;
                len += 2;
            }
            _ => {
                i += 1;
                len += 1;
            }
        }
        if len > 999 {
            return None;
        }
    }
    if source.as_bytes().get(i) != Some(&b']') {
        return None;
    }
    let label_range = label_start..i;
    let label = source.get(label_range.clone())?.to_string();
    if label.trim().is_empty() {
        return None;
    }
    i += 1;
    if source.as_bytes().get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    let colon_end = skip_spaces(source, i, limit);
    // Dest may sit on the next line (`[ref]:\n/url`). Do not skip_spnl
    // first: that walks `[ref]: \n` onto the following block and misses
    // an empty dest the editor still paints as a definition.
    let dest_here = colon_end;
    let dest_start = if dest_here < limit && source.as_bytes()[dest_here] != b'\n' {
        dest_here
    } else {
        skip_spnl(source, i, limit)
    };
    let ScannedLinkDestination {
        url,
        inner: dest_range,
        angle,
        after: after_dest,
        segments: dest_segments,
    } = match scan_link_destination(source, dest_start, limit) {
        Some(found) => found,
        None => {
            let empty_at = dest_here;
            let line_end = source[empty_at.min(limit)..limit]
                .find('\n')
                .map(|n| empty_at + n)
                .unwrap_or(limit);
            if !source[empty_at..line_end]
                .bytes()
                .all(|b| b == b' ' || b == b'\t')
            {
                return None;
            }
            ScannedLinkDestination {
                url: String::new(),
                inner: empty_at..empty_at,
                angle: false,
                after: empty_at,
                segments: Vec::new(),
            }
        }
    };
    i = after_dest;
    let before_title = i;
    let after_ws = skip_spnl(source, i, limit);
    // CommonMark: a title is optional and must be separated from dest by
    // whitespace (`[foo]: <bar>(baz)` is not a definition).
    let (title, title_range, after_title) = if after_ws > before_title {
        match scan_link_title(source, after_ws, limit) {
            Some(found) => found,
            None => {
                i = skip_spaces(source, before_title, limit);
                (None, None, i)
            }
        }
    } else {
        (None, None, skip_spaces(source, before_title, limit))
    };
    i = after_title;
    i = skip_spaces(source, i, limit);
    let line_end = source[i.min(limit)..limit]
        .find('\n')
        .map(|n| i + n)
        .unwrap_or(limit);
    if i > line_end {
        return None;
    }
    if !source[i..line_end].bytes().all(|b| b == b' ' || b == b'\t') {
        return None;
    }
    let content_end = title_range
        .as_ref()
        .map(|r| r.end)
        .unwrap_or_else(|| {
            if dest_segments.is_empty() {
                colon_end
            } else if angle {
                dest_range.end + 1
            } else {
                dest_range.end
            }
        })
        .min(limit);
    let url = unescape_cm_link_text(&url);
    let title = title.map(|t| unescape_cm_link_text(&t));
    Some(ParsedLinkRefDef {
        range: line_start..content_end.max(line_start),
        label,
        label_range,
        url,
        dest_range,
        dest_segments,
        angle,
        title,
        title_range,
        colon_end,
    })
}

fn skip_spaces(source: &str, mut i: usize, limit: usize) -> usize {
    while i < limit && matches!(source.as_bytes()[i], b' ' | b'\t') {
        i += 1;
    }
    i
}

/// CommonMark dest/title: unescape ASCII punctuation after `\`; keep
/// `\b` as backslash+b. Drop `\\\n` dest wrap markers.
fn unescape_cm_link_text(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next == b'\n' {
                i += 2;
                continue;
            }
            if next.is_ascii_punctuation() {
                out.push(next as char);
            } else {
                out.push('\\');
                out.push(next as char);
            }
            i += 2;
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn skip_spnl(source: &str, mut i: usize, limit: usize) -> usize {
    i = skip_spaces(source, i, limit);
    if i < limit && source.as_bytes()[i] == b'\n' {
        i += 1;
        let parts = super::engine::line_prefix_parts(source, i);
        if parts.quote.start == i {
            i = parts.quote.end;
        }
        i = skip_spaces(source, i, limit);
    }
    i.min(limit)
}

struct ScannedLinkDestination {
    url: String,
    inner: Range<usize>,
    angle: bool,
    after: usize,
    segments: Vec<Range<usize>>,
}

fn scan_link_destination(
    source: &str,
    start: usize,
    limit: usize,
) -> Option<ScannedLinkDestination> {
    let bytes = source.as_bytes();
    if start >= limit {
        return None;
    }
    if bytes[start] == b'<' {
        let mut i = start + 1;
        while i < limit {
            match bytes[i] {
                b'>' => {
                    let inner = start + 1..i;
                    let url = source.get(inner.clone())?.to_string();
                    if url.is_empty() || url.contains('\n') {
                        return None;
                    }
                    return Some(ScannedLinkDestination {
                        url,
                        inner: inner.clone(),
                        angle: true,
                        after: i + 1,
                        segments: vec![inner],
                    });
                }
                b'\n' | b'<' => return None,
                b'\\' if i + 1 < limit => i += 2,
                _ => i += 1,
            }
        }
        return None;
    }
    let mut url = String::new();
    let mut segments = Vec::new();
    let mut seg_start = start;
    let mut i = start;
    let mut parens = 0i32;
    while i < limit {
        let b = bytes[i];
        if b == b'\\' && i + 1 < limit {
            if bytes[i + 1] == b'\n' {
                if let Some(cont) = dest_wrap_continue_at(source, i + 2, limit) {
                    if i > seg_start {
                        url.push_str(source.get(seg_start..i)?);
                        segments.push(seg_start..i);
                    }
                    i = cont;
                    seg_start = i;
                    continue;
                }
                // Keep `\` as dest; stop before the newline so
                // `[ref]: /url\` then a paragraph still parses.
                i += 1;
                break;
            }
            i += 2;
            continue;
        }
        if b == b'(' {
            parens += 1;
            if parens > 32 {
                return None;
            }
            i += 1;
            continue;
        }
        if b == b')' {
            if parens == 0 {
                break;
            }
            parens -= 1;
            i += 1;
            continue;
        }
        if b == b' ' || b == b'\t' || b == b'\n' || b.is_ascii_control() {
            break;
        }
        i += 1;
    }
    if i > seg_start {
        url.push_str(source.get(seg_start..i)?);
        segments.push(seg_start..i);
    }
    if url.is_empty() || parens != 0 || segments.is_empty() {
        return None;
    }
    let range = segments.first()?.start..segments.last()?.end;
    Some(ScannedLinkDestination {
        url,
        inner: range,
        angle: false,
        after: i,
        segments,
    })
}

/// After `\\\n` in a bare dest, continue dest on the next line when the
/// rest of that line is dest (quoted keep `>`). Reject a following
/// paragraph / title / `[ref]` so CommonMark `[foo]: /url` then `bar` /
/// `===` is not swallowed.
fn dest_wrap_continue_at(source: &str, after_nl: usize, limit: usize) -> Option<usize> {
    let mut j = after_nl.min(limit);
    if j < limit {
        let parts = super::engine::line_prefix_parts(source, j);
        if parts.quote.start == j {
            j = parts.quote.end;
        }
    }
    j = skip_spaces(source, j, limit);
    if j >= limit {
        return None;
    }
    let b = source.as_bytes()[j];
    if matches!(
        b,
        b'"' | b'\'' | b'(' | b'[' | b'<' | b'#' | b'>' | b'`' | b'\n'
    ) || b.is_ascii_control()
    {
        return None;
    }
    if matches!(b, b'-' | b'*' | b'+') && matches!(source.as_bytes().get(j + 1), Some(b' ' | b'\t'))
    {
        return None;
    }
    let end = dest_token_end(source, j, limit);
    if end == j {
        return None;
    }
    let after = skip_spaces(source, end, limit);
    let line_end = source[after.min(limit)..limit]
        .find('\n')
        .map(|n| after + n)
        .unwrap_or(limit);
    if source[after..line_end]
        .bytes()
        .all(|c| c == b' ' || c == b'\t')
        || scan_link_title(source, after, limit).is_some()
    {
        Some(j)
    } else {
        None
    }
}

fn dest_token_end(source: &str, start: usize, limit: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = start;
    let mut parens = 0i32;
    while i < limit {
        let b = bytes[i];
        if b == b'\\' && i + 1 < limit && bytes[i + 1] != b'\n' {
            i += 2;
            continue;
        }
        if b == b'(' {
            parens += 1;
            if parens > 32 {
                break;
            }
            i += 1;
            continue;
        }
        if b == b')' {
            if parens == 0 {
                break;
            }
            parens -= 1;
            i += 1;
            continue;
        }
        if b == b' ' || b == b'\t' || b == b'\n' || b.is_ascii_control() {
            break;
        }
        i += 1;
    }
    i
}

fn scan_link_title(
    source: &str,
    start: usize,
    limit: usize,
) -> Option<(Option<String>, Option<std::ops::Range<usize>>, usize)> {
    let bytes = source.as_bytes();
    let closer = match bytes.get(start).copied() {
        Some(b'"') => b'"',
        Some(b'\'') => b'\'',
        Some(b'(') => b')',
        _ => return None,
    };
    let mut i = start + 1;
    let mut saw_newline = false;
    while i < limit {
        let b = bytes[i];
        if b == b'\\' && i + 1 < limit {
            i += 2;
            continue;
        }
        if b == b'\n' {
            if saw_newline {
                return None;
            }
            saw_newline = true;
            i += 1;
            let parts = super::engine::line_prefix_parts(source, i);
            if parts.quote.start == i {
                i = parts.quote.end;
            }
            continue;
        }
        if b == closer {
            let inner = start + 1..i;
            let title = source.get(inner.clone())?.to_string();
            return Some((Some(title), Some(start..i + 1), i + 1));
        }
        i += 1;
    }
    None
}

/// Comrak sometimes omits SoftBreak between runs (`===\n[foo]` after a
/// recovered `[foo]: /url` def). Normalize would glue those runs.
fn insert_missing_source_breaks(source: &str, inlines: &mut Vec<Inline>) {
    if inlines.len() < 2 {
        return;
    }
    let mut out = Vec::with_capacity(inlines.len() + 4);
    for inline in inlines.drain(..) {
        if let Some(prev) = out.last() {
            if !matches!(prev, Inline::SoftBreak { .. } | Inline::HardBreak { .. })
                && !matches!(inline, Inline::SoftBreak { .. } | Inline::HardBreak { .. })
            {
                let lo = prev.source_range().end;
                let hi = inline.source_range().start;
                if hi > lo {
                    if let Some(br) = hard_break_between(lo, hi, source) {
                        out.push(Inline::HardBreak {
                            style: BreakStyle::Backslash,
                            source_range: br,
                        });
                    } else if let Some(nl) = newline_between(lo, hi, source) {
                        out.push(Inline::SoftBreak { source_range: nl });
                    }
                }
            }
        }
        out.push(inline);
    }
    *inlines = out;
}

fn newline_between(lo: usize, hi: usize, source: &str) -> Option<std::ops::Range<usize>> {
    if hi <= lo {
        return None;
    }
    let slice = source.get(lo..hi)?;
    let rel = slice.find('\n')?;
    Some(lo + rel..lo + rel + 1)
}

fn hard_break_between(lo: usize, hi: usize, source: &str) -> Option<std::ops::Range<usize>> {
    if hi <= lo {
        return None;
    }
    let slice = source.get(lo..hi)?;
    let rel = slice.find("\\\n")?;
    Some(lo + rel..lo + rel + 2)
}

fn block_from_link_reference_def(
    parsed: ParsedLinkRefDef,
    source: &str,
    ids: &mut IdGen,
    groups: &mut u64,
) -> Block {
    *groups += 1;
    let dest_link = (!parsed.url.is_empty()).then(|| LinkAttrs {
        url: parsed.url.clone(),
        title: parsed.title.clone(),
        autolink: false,
        angle: parsed.angle,
        group: *groups,
    });
    let mut inlines = vec![Inline::Run {
        text: parsed.label.clone(),
        raw: Some(Box::<str>::from(
            source.get(parsed.label_range.clone()).unwrap_or(""),
        )),
        source_range: parsed.label_range.clone(),
        marks: MarkSet::empty(),
        link: None,
        fidelity: MarkFidelity::default(),
    }];
    let dest_start = parsed
        .dest_segments
        .first()
        .map(|r| r.start)
        .unwrap_or(parsed.dest_range.start);
    if let Some(nl) = newline_between(parsed.colon_end, dest_start, source) {
        inlines.push(Inline::SoftBreak { source_range: nl });
    }
    for (i, seg) in parsed.dest_segments.iter().enumerate() {
        if i > 0 {
            let prev = parsed.dest_segments[i - 1].end;
            if let Some(br) = hard_break_between(prev, seg.start, source) {
                inlines.push(Inline::HardBreak {
                    style: BreakStyle::Backslash,
                    source_range: br,
                });
            } else if let Some(nl) = newline_between(prev, seg.start, source) {
                inlines.push(Inline::SoftBreak { source_range: nl });
            }
        }
        inlines.push(Inline::Run {
            text: source.get(seg.clone()).unwrap_or("").to_string(),
            raw: Some(Box::<str>::from(source.get(seg.clone()).unwrap_or(""))),
            source_range: seg.clone(),
            marks: MarkSet::empty(),
            link: dest_link.clone(),
            fidelity: MarkFidelity::default(),
        });
    }
    if let Some(title_range) = parsed.title_range.clone() {
        inlines.push(Inline::Run {
            text: source.get(title_range.clone()).unwrap_or("").to_string(),
            raw: Some(Box::<str>::from(
                source.get(title_range.clone()).unwrap_or(""),
            )),
            source_range: title_range,
            marks: MarkSet::empty(),
            link: None,
            fidelity: MarkFidelity::default(),
        });
    }
    let slice = source.get(parsed.range.clone()).unwrap_or("");
    Block {
        id: ids.next_id(),
        source_range: parsed.range.clone(),
        content_hash: super::engine::hash_str(slice),
        kind: BlockKind::LinkReferenceDefinition {
            label: parsed.label,
            url: parsed.url,
            title: parsed.title,
        },
        children: Vec::new(),
        inlines,
    }
}

#[cfg(test)]
mod link_ref_parse_tests {
    use super::*;

    #[test]
    fn parse_dest_backslash_wrap_concatenates() {
        let source = "[ref]: https://ex.com/pa\\\nth\n";
        let parsed =
            parse_link_reference_definition(source, 0, source.len(), 0).unwrap_or_else(|| {
                panic!(
                    "expected a definition, source bytes={:?}",
                    source.as_bytes()
                )
            });
        assert_eq!(
            parsed.url, "https://ex.com/path",
            "wrap url={:?} segments={:?}",
            parsed.url, parsed.dest_segments
        );
        let quoted = "> [hello][ref]\n>\n> [ref]: https://ex.com/pa\\\n> th\n";
        let at = quoted.find("[ref]:").expect("[ref]:");
        let col = line_content_column(quoted, at);
        let parsed = parse_link_reference_definition(quoted, at, quoted.len(), col)
            .unwrap_or_else(|| panic!("quoted wrap must parse, col={col}"));
        assert_eq!(parsed.url, "https://ex.com/path");
    }

    #[test]
    fn parse_rejects_title_glued_to_angle_dest() {
        let source = "[foo]: <bar>(baz)\n\n[foo]\n";
        assert!(
            parse_link_reference_definition(source, 0, source.len(), 0).is_none(),
            "CommonMark 170 is not a definition"
        );
    }

    #[test]
    fn parse_keeps_backslash_dest_and_title() {
        let source = "[foo]: /url\\bar\\*baz \"foo\\\"bar\\baz\"\n\n[foo]\n";
        let parsed = parse_link_reference_definition(source, 0, source.len(), 0)
            .expect("CommonMark 171 is a definition");
        assert!(
            parsed.url.contains("bar"),
            "dest must keep escaped bytes, got {:?}",
            parsed.url
        );
    }

    #[test]
    fn cm185_paragraph_keeps_newline_before_shortcut() {
        use crate::rich::{serialize_tree, SerializeMode};
        use std::collections::HashSet;
        let md = "[foo]: /url\n===\n[foo]\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(md, &mut ids);
        let para = tree
            .blocks
            .iter()
            .find(|b| matches!(b.kind, BlockKind::Paragraph))
            .expect("paragraph");
        let has_break = para
            .inlines
            .iter()
            .any(|i| matches!(i, Inline::SoftBreak { .. } | Inline::HardBreak { .. }));
        let once = serialize_tree(&tree, md, SerializeMode::Normalize, &HashSet::new());
        assert!(
            has_break,
            "CM 185 must keep the wrap between `===` and `[foo]`"
        );
        let html_orig = crate::markdown_to_html_gfm(md);
        let html_norm = crate::markdown_to_html_gfm(&once);
        assert_eq!(html_orig, html_norm, "once={once:?}");
    }
}
