// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Markdown → [`RichTree`] import via comrak.
//!
//! comrak is the single markdown grammar for the editor (the HTML exporter in
//! `export.rs` uses the same extension set, so the two can never disagree).
//! Block source positions come from comrak sourcepos; inline positions are
//! advisory and verified against the raw slice before use.

use comrak::nodes::{AstNode, ListDelimType, ListType, NodeValue, Sourcepos, TableAlignment};
use comrak::{parse_document, Arena, Options};

use super::tree::{
    Block, BlockKind, BreakStyle, ColumnAlign, FenceFidelity, Frontmatter, HeadingStyle, IdGen,
    Inline, LinkAttrs, MarkFidelity, MarkSet, RichTree,
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
            NodeValue::SoftBreak => out.push(Inline::SoftBreak),
            NodeValue::LineBreak => {
                let style = if self.slice(&range).contains('\\') {
                    BreakStyle::Backslash
                } else {
                    BreakStyle::TwoSpaces
                };
                out.push(Inline::HardBreak { style });
            }
            // Inline HTML, footnote refs, math, wikilinks, superscript, ...
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
