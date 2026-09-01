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

use super::tree::{
    find_alert_chrome, is_toc_marker, trailing_blank_gap, AlertKind, Block, BlockKind, BreakStyle,
    ColumnAlign, FenceFidelity, Frontmatter, HeadingStyle, IdGen, Inline, LinkAttrs, MarkFidelity,
    MarkSet, RichTree,
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

struct Importer<'s> {
    source: &'s str,
    lines: LineStarts,
    link_groups: std::cell::Cell<u64>,
}

/// Import markdown into a fresh [`RichTree`]; ids come from `ids`.
pub fn import_markdown(source: &str, ids: &mut IdGen) -> RichTree {
    let arena = Arena::new();
    let root = parse_document(&arena, source, &parse_options());
    let importer = Importer {
        source,
        lines: LineStarts::new(source),
        link_groups: std::cell::Cell::new(0),
    };

    let mut frontmatter = None;
    let mut blocks = Vec::new();
    for child in root.children() {
        if let NodeValue::FrontMatter(raw) = &child.data.borrow().value {
            let source_range = importer.node_range(child);
            frontmatter = Some(Frontmatter {
                raw: raw.clone(),
                source_range,
            });
            continue;
        }
        blocks.push(importer.import_block(child, ids));
    }
    RichTree {
        frontmatter,
        blocks,
        source_len: source.len(),
        trailing_blank: trailing_blank_gap(source),
    }
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
        let source_range = self.node_range(node);
        let id = ids.next_id();
        let value = &node.data.borrow().value;

        let (kind, container) = match value {
            NodeValue::Paragraph => (BlockKind::Paragraph, false),
            NodeValue::Heading(h) => (
                BlockKind::Heading {
                    level: h.level,
                    style: if h.setext {
                        HeadingStyle::Setext
                    } else {
                        HeadingStyle::Atx
                    },
                },
                false,
            ),
            NodeValue::CodeBlock(cb) => (
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
            ),
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
            NodeValue::ThematicBreak => (BlockKind::ThematicBreak, false),
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
            // sourcepos for HTML blocks is unreliable, so prefer the literal.
            NodeValue::HtmlBlock(h) => (
                BlockKind::Opaque {
                    raw: h.literal.trim_end_matches('\n').to_string(),
                },
                false,
            ),
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
            return block;
        }
        if matches!(block.kind, BlockKind::CodeBlock { .. }) {
            if let BlockKind::CodeBlock { literal, .. } = &block.kind {
                let text = literal.strip_suffix('\n').unwrap_or(literal).to_string();
                block.inlines.push(Inline::Run {
                    text,
                    raw: None,
                    source_range: block.source_range.clone(),
                    marks: MarkSet::CODE,
                    link: None,
                    fidelity: MarkFidelity::default(),
                });
            }
            return block;
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
            apply_eqeq_highlight(&mut block.inlines, &self.link_groups);
            super::emoji::apply_emoji_shortcodes(&mut block.inlines);
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
        block
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
                // single text child identical to the destination.
                let only_child_is_url = {
                    let mut children = node.children();
                    match (children.next(), children.next()) {
                        (Some(c), None) => matches!(
                            &c.data.borrow().value,
                            NodeValue::Text(t) if *t == link.url
                        ),
                        _ => false,
                    }
                };
                let autolink = (slice.starts_with('<') && slice.ends_with('>'))
                    || slice == link.url
                    || link.url == format!("mailto:{slice}")
                    || only_child_is_url;
                self.link_groups.set(self.link_groups.get() + 1);
                let saved = ctx.link.take();
                ctx.link = Some(LinkAttrs {
                    url: link.url.clone(),
                    title: (!link.title.is_empty()).then(|| link.title.clone()),
                    autolink,
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
}

/// Expand comrak's inner math sourcepos to include `$` / `$$`.
pub(crate) fn math_outer_range(
    source: &str,
    inner: std::ops::Range<usize>,
    display: bool,
) -> std::ops::Range<usize> {
    let width = super::tree::math_delim_width(display);
    let delim = if display { "$$" } else { "$" };
    let start = inner.start.saturating_sub(width);
    let end = inner.end.saturating_add(width).min(source.len());
    if source.get(start..inner.start) == Some(delim) && source.get(inner.end..end) == Some(delim) {
        start..end
    } else {
        inner
    }
}

struct InlineCtx {
    marks: MarkSet,
    link: Option<LinkAttrs>,
    fidelity: MarkFidelity,
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
                ..
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
                        raw: None,
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
