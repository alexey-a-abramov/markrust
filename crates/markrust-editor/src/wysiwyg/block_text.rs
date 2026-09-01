// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Interactive leaf text: wrap-aware hit-testing, caret, and selection paint.

use std::ops::Range;
use std::sync::Arc;

use gpui::{
    fill, point, px, relative, size, App, Bounds, Context, Element, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    SharedString, Style, TextRun, TextStyle, UnderlineStyle, Window, WrappedLine,
};
use markrust_core::rich::{import_markdown, Block, BreakStyle, IdGen, Inline, MarkSet, NodeId};

use crate::theme::EditorTheme;

/// Host implemented by [`super::view::RichEditorView`].
pub trait WysiwygHost: gpui::Render + EntityInputHandler + 'static {
    fn click_source(
        &mut self,
        source: usize,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    );
    fn drag_source(&mut self, source: usize, cx: &mut Context<Self>);
    fn end_drag(&mut self, cx: &mut Context<Self>);
    fn selected_range(&self) -> Range<usize>;
    fn caret_offset(&self) -> usize;
    fn caret_visible(&self) -> bool;
    fn is_selecting(&self) -> bool;
    fn focused(&self, window: &Window) -> bool;
    fn widget_editing(&self) -> bool;
    fn input_focus_handle(&self) -> FocusHandle;
    fn toggle_task(&mut self, id: NodeId, cx: &mut Context<Self>);
    fn edit_code_info(&mut self, id: NodeId, cx: &mut Context<Self>);
    fn edit_image_alt(&mut self, source_range: Range<usize>, alt: &str, cx: &mut Context<Self>);
    fn edit_frontmatter_field(&mut self, key: &'static str, current: &str, cx: &mut Context<Self>);
    fn edit_frontmatter_yaml(&mut self, current: &str, cx: &mut Context<Self>);
    fn open_table_menu(&mut self, source: usize, window: &mut Window, cx: &mut Context<Self>);
    fn finish_widget(&mut self, cx: &mut Context<Self>);
    fn preedit(&self) -> Option<&str>;
    fn report_widget_bounds(&mut self, bounds: Bounds<Pixels>);
    fn report_leaf(
        &mut self,
        layout: Arc<LeafLayout>,
        element_bounds: Bounds<Pixels>,
        font_size: f32,
        line_height: f32,
        caret_bounds: Option<Bounds<Pixels>>,
    );
    fn report_painted_bounds(&mut self, bounds: Bounds<Pixels>);
    /// Push the resolved IME origin to the platform (not only `bounds_for_range`).
    fn sync_ime_cursor(&mut self, window: &mut Window);
}

/// Visible text of a leaf block plus a map back to source bytes.
#[derive(Clone)]
pub struct LeafLayout {
    pub text: String,
    pub runs: Vec<TextRun>,
    /// Source byte for each UTF-8 offset in `text` (length `text.len() + 1`).
    pub source_at: Vec<usize>,
    pub block_start: usize,
}

impl LeafLayout {
    pub fn source_for_visible(&self, vis: usize) -> usize {
        let vis = vis.min(self.text.len());
        let mut i = vis;
        while i > 0 && !self.text.is_char_boundary(i) {
            i -= 1;
        }
        self.source_at
            .get(i)
            .copied()
            .or_else(|| self.source_at.last().copied())
            .unwrap_or(self.block_start)
    }

    pub fn visible_for_source(&self, src: usize) -> usize {
        let mut best = 0usize;
        for (i, s) in self.source_at.iter().enumerate() {
            if *s <= src {
                best = i;
            } else {
                break;
            }
        }
        best.min(self.text.len())
    }

    pub fn contains_source(&self, src: usize) -> bool {
        let start = *self.source_at.first().unwrap_or(&self.block_start);
        let end = *self.source_at.last().unwrap_or(&self.block_start);
        src >= start && src <= end
    }

    pub fn source_span_len(&self) -> usize {
        let start = *self.source_at.first().unwrap_or(&self.block_start);
        let end = *self.source_at.last().unwrap_or(&self.block_start);
        end.saturating_sub(start)
    }
}

/// Caret/selection used to reveal `$` / `$$` / `[[wiki]]` / `:emoji:` in
/// WYSIWYG (Typora: hide chrome unless the caret or a non-empty selection
/// intersects the span).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevealState {
    pub caret: usize,
    pub selection: Range<usize>,
}

impl RevealState {
    pub const HIDDEN: Self = Self {
        caret: usize::MAX,
        selection: Range { start: 0, end: 0 },
    };

    pub fn intersects(&self, range: &Range<usize>) -> bool {
        if self.caret >= range.start && self.caret <= range.end {
            return true;
        }
        !self.selection.is_empty()
            && self.selection.start < range.end
            && self.selection.end > range.start
    }
}

fn visible_for_html(s: &str, paint: &markrust_core::html_visual::HtmlPaint) -> String {
    use markrust_core::html_visual::{map_subscript, map_superscript};
    if paint.sup && !paint.sub {
        return map_superscript(s);
    }
    if paint.sub && !paint.sup {
        return map_subscript(s);
    }
    s.to_string()
}

fn merge_html_paint(
    marks: MarkSet,
    html: &markrust_core::html_visual::HtmlPaint,
) -> markrust_core::html_visual::HtmlPaint {
    let mut paint = html.clone();
    if marks.contains(MarkSet::HIGHLIGHT) {
        paint.mark = true;
    }
    if marks.contains(MarkSet::SUP) {
        paint.sup = true;
    }
    if marks.contains(MarkSet::SUB) {
        paint.sub = true;
    }
    paint
}

fn style_run(
    text_style: &TextStyle,
    theme: &EditorTheme,
    base_weight: gpui::FontWeight,
    marks: MarkSet,
    md_link: bool,
    paint: &markrust_core::html_visual::HtmlPaint,
) -> TextRun {
    let mut run = text_style.to_run(0);
    let bold = paint.bold || marks.contains(MarkSet::BOLD);
    let italic = paint.italic || marks.contains(MarkSet::ITALIC);
    let strike = paint.strike || marks.contains(MarkSet::STRIKE);
    let code = paint.code || marks.contains(MarkSet::CODE);
    run.font.weight = if bold {
        gpui::FontWeight::BOLD
    } else {
        base_weight
    };
    if italic {
        run.font.style = gpui::FontStyle::Italic;
    }
    if strike {
        // Inherit the run color so strike-through stays on the
        // glyph (Typora-style), including links and emphasis.
        run.strikethrough = Some(gpui::StrikethroughStyle {
            thickness: px(1.),
            color: None,
        });
    }
    if code {
        run.font.family = theme.code_font_family.clone().into();
        run.background_color = Some(theme.code_bg);
    } else if paint.mark || marks.contains(MarkSet::HIGHLIGHT) {
        run.background_color = Some(theme.accent.opacity(0.22));
    }
    if paint.underline {
        run.underline = Some(gpui::UnderlineStyle {
            thickness: px(1.),
            color: Some(theme.text),
            wavy: false,
        });
    }
    if md_link || paint.href.is_some() {
        run.color = theme.link;
        run.underline = Some(gpui::UnderlineStyle {
            thickness: px(1.),
            color: Some(theme.link),
            wavy: false,
        });
    }
    run
}

fn math_style_run(
    text_style: &TextStyle,
    theme: &EditorTheme,
    base_weight: gpui::FontWeight,
    marks: MarkSet,
    paint: &markrust_core::html_visual::HtmlPaint,
) -> TextRun {
    let mut run = style_run(text_style, theme, base_weight, marks, false, paint);
    run.font.family = theme.code_font_family.clone().into();
    run.font.style = gpui::FontStyle::Italic;
    if !(paint.mark
        || marks.contains(MarkSet::HIGHLIGHT)
        || paint.code
        || marks.contains(MarkSet::CODE))
    {
        run.background_color = None;
    }
    run
}

fn math_delim_run(text_style: &TextStyle, theme: &EditorTheme) -> TextRun {
    let mut run = text_style.to_run(0);
    run.font.family = theme.code_font_family.clone().into();
    run.color = theme.delimiter;
    run
}

fn wiki_delim_run(text_style: &TextStyle, theme: &EditorTheme) -> TextRun {
    let mut run = text_style.to_run(0);
    run.color = theme.delimiter;
    run
}

/// Layout for a projected HTML block (tags stripped). `source_at` is relative
/// to the HTML literal; `block_start` is the document offset of that literal.
/// Inner Markdown (`**bold**`, links, code) is parsed so it does not paint as
/// source chrome; HTML phrasing (`<mark>`, `<sub>`, …) is merged as marks.
pub fn build_html_block_layout(
    text: &str,
    source_at: &[usize],
    paints: &[markrust_core::html_visual::HtmlPaintRun],
    source: &str,
    block: &Block,
    text_style: &TextStyle,
    theme: &EditorTheme,
) -> LeafLayout {
    let literal_map = html_literal_source_map(source, block);
    let block_start = *literal_map.first().unwrap_or(&block.source_range.start);
    let mut inlines = inlines_from_inner_markdown(text);
    let markdown_visible = if inlines.is_empty() {
        false
    } else {
        let probe = build_leaf_layout_inlines(
            &inlines,
            0..text.len(),
            text_style,
            theme,
            gpui::FontWeight::NORMAL,
            &RevealState::HIDDEN,
        );
        !probe.text.trim().is_empty()
    };
    if !markdown_visible && !text.is_empty() {
        inlines = vec![Inline::Run {
            text: text.to_string(),
            raw: None,
            source_range: 0..text.len(),
            marks: MarkSet::empty(),
            link: None,
            fidelity: markrust_core::rich::MarkFidelity::default(),
        }];
    }
    inlines = split_inlines_by_html_paints(inlines, paints);
    let mut layout = build_leaf_layout_inlines(
        &inlines,
        0..text.len(),
        text_style,
        theme,
        gpui::FontWeight::NORMAL,
        &RevealState::HIDDEN,
    );
    if layout.text.is_empty() && !text.is_empty() {
        return html_flow_layout(
            text,
            source_at,
            paints,
            &literal_map,
            block_start,
            text_style,
            theme,
        );
    }
    remap_html_sources(
        &mut layout,
        source_at,
        &literal_map,
        block_start,
        text.len(),
    );
    layout
}

fn html_flow_layout(
    text: &str,
    source_at: &[usize],
    paints: &[markrust_core::html_visual::HtmlPaintRun],
    literal_map: &[usize],
    block_start: usize,
    text_style: &TextStyle,
    theme: &EditorTheme,
) -> LeafLayout {
    let mut runs = Vec::new();
    for pr in paints {
        let mut run = style_run(
            text_style,
            theme,
            gpui::FontWeight::NORMAL,
            MarkSet::empty(),
            false,
            &pr.paint,
        );
        run.len = pr.len;
        runs.push(run);
    }
    let mut mapped: Vec<usize> = source_at
        .iter()
        .map(|o| doc_offset_for_html_literal(literal_map, *o, block_start))
        .collect();
    if text.is_empty() {
        mapped = vec![block_start, block_start];
        runs = vec![text_style.to_run(1)];
    } else if mapped.len() < text.len() + 1 {
        while mapped.len() < text.len() + 1 {
            mapped.push(*mapped.last().unwrap_or(&block_start));
        }
    }
    mapped.truncate(text.len() + 1);
    let covered: usize = runs.iter().map(|r| r.len).sum();
    if covered != text.len() && !text.is_empty() {
        runs = vec![text_style.to_run(text.len())];
    }
    if runs.is_empty() {
        runs.push(text_style.to_run(text.len().max(1)));
    }
    LeafLayout {
        text: text.to_string(),
        runs,
        source_at: mapped,
        block_start,
    }
}

fn html_literal_source_map(source: &str, block: &Block) -> Vec<usize> {
    let body = block.code_body_range(source);
    markrust_core::rich::code_body_source_map(source, block, body.len())
}

fn doc_offset_for_html_literal(literal_map: &[usize], html: usize, fallback: usize) -> usize {
    literal_map
        .get(html)
        .copied()
        .or_else(|| literal_map.last().copied())
        .unwrap_or(fallback)
}

fn inlines_from_inner_markdown(source: &str) -> Vec<Inline> {
    let tree = import_markdown(source, &mut IdGen::default());
    let mut out = Vec::new();
    fn walk(blocks: &[Block], out: &mut Vec<Inline>, first: &mut bool) {
        for b in blocks {
            if !b.inlines.is_empty() {
                if !*first {
                    let at = out.last().map(|i| i.source_range().end).unwrap_or(0);
                    out.push(Inline::HardBreak {
                        style: BreakStyle::Backslash,
                        source_range: at..at,
                    });
                }
                *first = false;
                out.extend(b.inlines.iter().cloned());
            }
            walk(&b.children, out, first);
        }
    }
    let mut first = true;
    walk(&tree.blocks, &mut out, &mut first);
    out
}

fn html_paint_marks(
    p: &markrust_core::html_visual::HtmlPaint,
) -> (MarkSet, Option<markrust_core::rich::LinkAttrs>) {
    let mut m = MarkSet::empty();
    if p.bold {
        m = m.with(MarkSet::BOLD);
    }
    if p.italic {
        m = m.with(MarkSet::ITALIC);
    }
    if p.strike {
        m = m.with(MarkSet::STRIKE);
    }
    if p.code {
        m = m.with(MarkSet::CODE);
    }
    if p.mark {
        m = m.with(MarkSet::HIGHLIGHT);
    }
    if p.sup {
        m = m.with(MarkSet::SUP);
    }
    if p.sub {
        m = m.with(MarkSet::SUB);
    }
    let link = p.href.as_ref().map(|url| markrust_core::rich::LinkAttrs {
        url: url.clone(),
        title: None,
        autolink: false,
        group: 0,
    });
    (m, link)
}

fn paint_covering(
    paints: &[markrust_core::html_visual::HtmlPaintRun],
    off: usize,
) -> markrust_core::html_visual::HtmlPaint {
    let mut cur = 0usize;
    for p in paints {
        if off < cur + p.len {
            return p.paint.clone();
        }
        cur += p.len;
    }
    markrust_core::html_visual::HtmlPaint::default()
}

fn split_inlines_by_html_paints(
    inlines: Vec<Inline>,
    paints: &[markrust_core::html_visual::HtmlPaintRun],
) -> Vec<Inline> {
    if paints.is_empty() {
        return inlines;
    }
    let mut out = Vec::with_capacity(inlines.len());
    for inline in inlines {
        match inline {
            Inline::Run {
                text,
                source_range,
                marks,
                link,
                fidelity,
                raw,
            } => {
                if text.is_empty() {
                    continue;
                }
                let mut start = 0usize;
                while start < text.len() {
                    let src_off = if source_range.len() == text.len() {
                        source_range.start + start
                    } else {
                        source_range.start + (source_range.len() * start / text.len().max(1))
                    };
                    let html = paint_covering(paints, src_off);
                    let (extra, html_link) = html_paint_marks(&html);
                    let mut end = start;
                    for (rel, ch) in text[start..].char_indices() {
                        let abs = start + rel;
                        if rel > 0 {
                            let off = if source_range.len() == text.len() {
                                source_range.start + abs
                            } else {
                                source_range.start + (source_range.len() * abs / text.len().max(1))
                            };
                            if paint_covering(paints, off) != html {
                                break;
                            }
                        }
                        end = abs + ch.len_utf8();
                    }
                    if end <= start {
                        break;
                    }
                    let src_start = if source_range.len() == text.len() {
                        source_range.start + start
                    } else {
                        source_range.start + (source_range.len() * start / text.len().max(1))
                    };
                    let src_end = if source_range.len() == text.len() {
                        source_range.start + end
                    } else {
                        source_range.start + (source_range.len() * end / text.len().max(1))
                    };
                    out.push(Inline::Run {
                        text: text[start..end].to_string(),
                        raw: raw.clone(),
                        source_range: src_start..src_end.max(src_start),
                        marks: marks.with(extra),
                        link: link.clone().or(html_link),
                        fidelity,
                    });
                    start = end;
                }
            }
            other => out.push(other),
        }
    }
    out
}

fn remap_html_sources(
    layout: &mut LeafLayout,
    html_source_at: &[usize],
    literal_map: &[usize],
    block_start: usize,
    inner_len: usize,
) {
    for slot in &mut layout.source_at {
        let inner = (*slot).min(inner_len);
        let html = html_source_at
            .get(inner)
            .copied()
            .or_else(|| html_source_at.last().copied())
            .unwrap_or(0);
        *slot = doc_offset_for_html_literal(literal_map, html, block_start);
    }
    layout.block_start = block_start;
}

pub fn build_leaf_layout_revealed(
    block: &Block,
    text_style: &TextStyle,
    theme: &EditorTheme,
    base_weight: gpui::FontWeight,
    reveal: &RevealState,
) -> LeafLayout {
    build_leaf_layout_inlines(
        &block.inlines,
        block.source_range.clone(),
        text_style,
        theme,
        base_weight,
        reveal,
    )
}

/// Painted soft/hard breaks are one visible character; map that glyph onto
/// the break's source bytes (never the paragraph start).
fn paint_break_src(range: &Range<usize>) -> Range<usize> {
    if range.end > range.start {
        range.clone()
    } else {
        range.start..range.start.saturating_add(1)
    }
}

/// Visible text for a slice of inlines. Images are omitted here; the block
/// renderer paints files as `img()` elements (local paths and cached remotes)
/// instead of alt placeholders.
pub fn build_leaf_layout_inlines(
    inlines: &[Inline],
    block_range: Range<usize>,
    text_style: &TextStyle,
    theme: &EditorTheme,
    base_weight: gpui::FontWeight,
    reveal: &RevealState,
) -> LeafLayout {
    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    let mut source_at = Vec::new();

    let push = |text: &mut String,
                runs: &mut Vec<TextRun>,
                source_at: &mut Vec<usize>,
                s: &str,
                src: Range<usize>,
                mut run: TextRun| {
        if s.is_empty() {
            return;
        }
        run.len = s.len();
        if source_at.len() < text.len() + 1 {
            source_at.resize(text.len() + 1, src.start);
        }
        source_at[text.len()] = src.start;
        let nchars = s.chars().count().max(1);
        for (i, (off, _)) in s.char_indices().enumerate() {
            if off == 0 {
                continue;
            }
            let mapped = if src.len() == s.len() {
                src.start + off
            } else {
                src.start + (src.len() * i / nchars)
            };
            source_at.push(mapped);
        }
        text.push_str(s);
        runs.push(run);
    };

    let mut html = markrust_core::html_visual::HtmlStack::default();

    for inline in inlines {
        match inline {
            Inline::Run {
                text: t,
                marks,
                link,
                source_range,
                ..
            } => {
                if html.hidden() {
                    continue;
                }
                let paint = merge_html_paint(*marks, &html.paint());
                let visible = visible_for_html(t, &paint);
                let run = style_run(
                    text_style,
                    theme,
                    base_weight,
                    *marks,
                    link.is_some(),
                    &paint,
                );
                push(
                    &mut text,
                    &mut runs,
                    &mut source_at,
                    &visible,
                    source_range.clone(),
                    run,
                );
            }
            Inline::Image { .. } => {
                // Pixels are sibling flex items in a line-box; this slice is
                // surrounding visible text only.
            }
            Inline::SoftBreak { source_range } => {
                if html.hidden() {
                    continue;
                }
                let run = style_run(
                    text_style,
                    theme,
                    base_weight,
                    MarkSet::empty(),
                    false,
                    &html.paint(),
                );
                push(
                    &mut text,
                    &mut runs,
                    &mut source_at,
                    " ",
                    paint_break_src(source_range),
                    run,
                );
            }
            Inline::HardBreak {
                style: BreakStyle::TwoSpaces | BreakStyle::Backslash,
                source_range,
            } => {
                if html.hidden() {
                    continue;
                }
                let run = text_style.to_run(0);
                push(
                    &mut text,
                    &mut runs,
                    &mut source_at,
                    "\n",
                    paint_break_src(source_range),
                    run,
                );
            }
            Inline::Math {
                literal,
                display,
                raw,
                source_range,
                marks,
            } => {
                if html.hidden() {
                    continue;
                }
                let paint = merge_html_paint(*marks, &html.paint());
                let width = markrust_core::rich::tree::math_delim_width(*display);
                if reveal.intersects(source_range) {
                    let raw_s = raw.as_ref();
                    if raw_s.len() >= width * 2 {
                        let open = &raw_s[..width];
                        let inner = &raw_s[width..raw_s.len() - width];
                        let close = &raw_s[raw_s.len() - width..];
                        let delim = math_delim_run(text_style, theme);
                        push(
                            &mut text,
                            &mut runs,
                            &mut source_at,
                            open,
                            source_range.start..source_range.start + width,
                            delim.clone(),
                        );
                        push(
                            &mut text,
                            &mut runs,
                            &mut source_at,
                            inner,
                            source_range.start + width..source_range.end.saturating_sub(width),
                            math_style_run(text_style, theme, base_weight, *marks, &paint),
                        );
                        push(
                            &mut text,
                            &mut runs,
                            &mut source_at,
                            close,
                            source_range.end.saturating_sub(width)..source_range.end,
                            delim,
                        );
                    } else {
                        push(
                            &mut text,
                            &mut runs,
                            &mut source_at,
                            raw_s,
                            source_range.clone(),
                            math_style_run(text_style, theme, base_weight, *marks, &paint),
                        );
                    }
                } else {
                    let inner_start = source_range.start.saturating_add(width);
                    let inner_end = source_range.end.saturating_sub(width).max(inner_start);
                    push(
                        &mut text,
                        &mut runs,
                        &mut source_at,
                        literal,
                        inner_start..inner_end,
                        math_style_run(text_style, theme, base_weight, *marks, &paint),
                    );
                }
            }
            Inline::WikiLink {
                label,
                raw,
                source_range,
                marks,
                ..
            } => {
                if html.hidden() {
                    continue;
                }
                let paint = merge_html_paint(*marks, &html.paint());
                let link_run = style_run(text_style, theme, base_weight, *marks, true, &paint);
                if reveal.intersects(source_range) {
                    let raw_s = raw.as_ref();
                    if raw_s.len() >= 4 && raw_s.starts_with("[[") && raw_s.ends_with("]]") {
                        let delim = wiki_delim_run(text_style, theme);
                        push(
                            &mut text,
                            &mut runs,
                            &mut source_at,
                            &raw_s[..2],
                            source_range.start..source_range.start + 2,
                            delim.clone(),
                        );
                        push(
                            &mut text,
                            &mut runs,
                            &mut source_at,
                            &raw_s[2..raw_s.len() - 2],
                            source_range.start + 2..source_range.end.saturating_sub(2),
                            link_run,
                        );
                        push(
                            &mut text,
                            &mut runs,
                            &mut source_at,
                            &raw_s[raw_s.len() - 2..],
                            source_range.end.saturating_sub(2)..source_range.end,
                            delim,
                        );
                    } else {
                        push(
                            &mut text,
                            &mut runs,
                            &mut source_at,
                            raw_s,
                            source_range.clone(),
                            link_run,
                        );
                    }
                } else {
                    let vis_range =
                        markrust_core::rich::wiki_visible_range(raw, source_range.clone());
                    let rel_start = vis_range.start.saturating_sub(source_range.start);
                    let rel_end = vis_range
                        .end
                        .saturating_sub(source_range.start)
                        .min(raw.len())
                        .max(rel_start);
                    let visible = raw.get(rel_start..rel_end).filter(|s| !s.is_empty());
                    push(
                        &mut text,
                        &mut runs,
                        &mut source_at,
                        visible.unwrap_or(label),
                        vis_range,
                        link_run,
                    );
                }
            }
            Inline::Emoji {
                glyph,
                raw,
                source_range,
                marks,
                link,
                ..
            } => {
                if html.hidden() {
                    continue;
                }
                let paint = merge_html_paint(*marks, &html.paint());
                let run = style_run(
                    text_style,
                    theme,
                    base_weight,
                    *marks,
                    link.is_some(),
                    &paint,
                );
                if reveal.intersects(source_range) {
                    push(
                        &mut text,
                        &mut runs,
                        &mut source_at,
                        raw,
                        source_range.clone(),
                        run,
                    );
                } else {
                    push(
                        &mut text,
                        &mut runs,
                        &mut source_at,
                        glyph,
                        source_range.clone(),
                        run,
                    );
                }
            }
            Inline::OpaqueInline {
                raw, source_range, ..
            } => match markrust_core::html_visual::classify_opaque_inline(raw, &mut html) {
                markrust_core::html_visual::InlineHtmlAction::Hide
                | markrust_core::html_visual::InlineHtmlAction::Image { .. } => {}
                markrust_core::html_visual::InlineHtmlAction::Break => {
                    let run = text_style.to_run(0);
                    push(
                        &mut text,
                        &mut runs,
                        &mut source_at,
                        "\n",
                        source_range.clone(),
                        run,
                    );
                }
                markrust_core::html_visual::InlineHtmlAction::FootnoteRef { label } => {
                    let paint = html.paint();
                    let visible =
                        markrust_core::html_visual::to_superscript(&label).unwrap_or(label);
                    let mut run = style_run(
                        text_style,
                        theme,
                        base_weight,
                        MarkSet::empty(),
                        true,
                        &paint,
                    );
                    run.underline = None;
                    push(
                        &mut text,
                        &mut runs,
                        &mut source_at,
                        &visible,
                        source_range.clone(),
                        run,
                    );
                }
                markrust_core::html_visual::InlineHtmlAction::Raw => {
                    let mut run = text_style.to_run(0);
                    run.color = theme.secondary_text;
                    run.font.family = theme.code_font_family.clone().into();
                    push(
                        &mut text,
                        &mut runs,
                        &mut source_at,
                        raw,
                        source_range.clone(),
                        run,
                    );
                }
            },
        }
    }

    if text.is_empty() {
        source_at = vec![block_range.start, block_range.start];
    } else if source_at.len() == text.len() {
        source_at.push(
            inlines
                .iter()
                .rev()
                .find_map(|i| match i {
                    Inline::Run { source_range, .. }
                    | Inline::Image { source_range, .. }
                    | Inline::Math { source_range, .. }
                    | Inline::WikiLink { source_range, .. }
                    | Inline::Emoji { source_range, .. }
                    | Inline::OpaqueInline { source_range, .. } => Some(source_range.end),
                    _ => None,
                })
                .unwrap_or(block_range.end),
        );
    }
    while source_at.len() < text.len() + 1 {
        source_at.push(*source_at.last().unwrap_or(&block_range.end));
    }
    source_at.truncate(text.len() + 1);

    // TextRun lengths must tile `text` on char boundaries.
    let covered: usize = runs.iter().map(|r| r.len).sum();
    if covered != text.len() && !text.is_empty() {
        runs = vec![text_style.to_run(text.len())];
    }

    LeafLayout {
        text,
        runs,
        source_at,
        block_start: block_range.start,
    }
}

/// Layout for a fenced code body: 1:1 map from visible bytes to source.
pub fn build_code_block_layout(
    body: &str,
    source: &str,
    block: &Block,
    text_style: &TextStyle,
    theme: &EditorTheme,
) -> LeafLayout {
    let source_at = markrust_core::rich::code_body_source_map(source, block, body.len());
    let block_start = block.code_body_range(source).start;
    finish_code_layout(body, source_at, block_start, text_style, theme)
}

fn finish_code_layout(
    body: &str,
    mut source_at: Vec<usize>,
    block_start: usize,
    text_style: &TextStyle,
    theme: &EditorTheme,
) -> LeafLayout {
    let mut runs = vec![text_style.to_run(body.len())];
    if !body.is_empty() {
        runs[0].font.family = theme.code_font_family.clone().into();
    }
    if body.is_empty() {
        let at = source_at.first().copied().unwrap_or(block_start);
        source_at = vec![at, at];
        runs = vec![text_style.to_run(1)];
    } else {
        while source_at.len() < body.len() + 1 {
            let last = source_at.last().copied().unwrap_or(block_start);
            source_at.push(last);
        }
        source_at.truncate(body.len() + 1);
    }
    LeafLayout {
        text: body.to_string(),
        runs,
        source_at,
        block_start,
    }
}

/// One visual empty line for a Comrak-less blank (leading newlines or a
/// standard / extra block separator). Click and caret map onto `range`
/// (exclusive of the following block).
pub fn build_blank_gap_layout(range: Range<usize>) -> LeafLayout {
    let start = range.start;
    let last = range.end.saturating_sub(1).max(start);
    LeafLayout {
        text: String::new(),
        runs: Vec::new(),
        source_at: vec![start, last],
        block_start: start,
    }
}

pub fn hit_test_leaf(
    layout: &LeafLayout,
    bounds: Bounds<Pixels>,
    position: gpui::Point<Pixels>,
    window: &mut Window,
    font_size: f32,
    line_height: f32,
    theme: &EditorTheme,
) -> usize {
    let lines = shape_layout(
        layout,
        window,
        bounds.size.width,
        font_size,
        line_height,
        theme,
    );
    visible_index_at(&lines, bounds, position, px(line_height))
}

pub struct BlockTextElement<H: WysiwygHost> {
    pub editor: Entity<H>,
    pub layout: Arc<LeafLayout>,
    pub font_size: f32,
    pub line_height: f32,
    pub theme: EditorTheme,
    /// Size to unwrapped text width so mixed paragraphs can sit on one flex row.
    pub hug_width: bool,
}

pub struct Prepaint<H: WysiwygHost> {
    lines: Vec<WrappedLine>,
    cursor: Option<PaintQuad>,
    caret_bounds: Option<Bounds<Pixels>>,
    selection: Option<PaintQuad>,
    _host: std::marker::PhantomData<H>,
}

impl<H: WysiwygHost> IntoElement for BlockTextElement<H> {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl<H: WysiwygHost> Element for BlockTextElement<H> {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint<H>;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let wrap = (window.viewport_size().width - px(80.)).max(px(120.));
        let mut style = Style::default();
        let width = if self.hug_width {
            let measured = self.measure_unwrapped_width(window);
            measured.min(wrap).max(px(1.))
        } else {
            wrap
        };
        let height = self.measure_height(window, width);
        if self.hug_width {
            style.size.width = width.into();
            style.flex_shrink = 0.;
        } else {
            style.size.width = relative(1.).into();
        }
        style.size.height = height.max(px(self.line_height)).into();
        style.min_size.height = px(self.line_height).into();
        let _ = cx;
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let lines = self.shape(window, bounds.size.width);
        let host = self.editor.read(cx);
        let caret = host.caret_offset();
        let selected = host.selected_range();
        let focused = host.focused(window);
        let caret_visible = host.caret_visible();
        let vis_caret = self.layout.visible_for_source(caret);
        let vis_sel = self.layout.visible_for_source(selected.start)
            ..self.layout.visible_for_source(selected.end);

        let line_height = px(self.line_height);
        let (selection, cursor, caret_bounds) = paint_carets(
            &lines,
            bounds,
            line_height,
            vis_caret,
            vis_sel,
            self.layout.text.len(),
            focused && caret_visible && source_in_leaf(&self.layout, caret),
            selected.start != selected.end && ranges_touch_leaf(&self.layout, &selected),
            self.theme.caret,
            self.theme.selection,
        );

        Prepaint {
            lines,
            cursor,
            caret_bounds,
            selection,
            _host: std::marker::PhantomData,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.editor.read(cx).input_focus_handle();
        let caret = self.editor.read(cx).caret_offset();
        let preedit = self.editor.read(cx).preedit().map(str::to_string);
        let layout = self.layout.clone();
        let font_size = self.font_size;
        let line_height_px = self.line_height;
        let caret_in_leaf = source_in_leaf(&self.layout, caret);
        let widget_editing = self.editor.read(cx).widget_editing();
        if caret_in_leaf && !widget_editing {
            window.handle_input(
                &focus,
                ElementInputHandler::new(bounds, self.editor.clone()),
                cx,
            );
        }
        self.editor.update(cx, |host, _cx| {
            host.report_leaf(
                layout,
                bounds,
                font_size,
                line_height_px,
                if caret_in_leaf {
                    prepaint.caret_bounds
                } else {
                    None
                },
            );
            host.sync_ime_cursor(window);
        });

        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }

        let origin = bounds.origin;
        let mut y = origin.y;
        let line_height = px(self.line_height);
        for line in &prepaint.lines {
            let _ = line.paint(
                point(origin.x, y),
                line_height,
                gpui::TextAlign::Left,
                Some(bounds),
                window,
                cx,
            );
            y += line.size(line_height).height.max(line_height);
        }

        if let (Some(preedit), Some(caret_bounds)) = (preedit.as_deref(), prepaint.caret_bounds) {
            if !preedit.is_empty() && caret_in_leaf {
                let style = TextStyle {
                    color: self.theme.text,
                    font_family: self.theme.font_family.clone().into(),
                    font_size: px(self.font_size).into(),
                    line_height: px(self.line_height).into(),
                    underline: Some(UnderlineStyle {
                        thickness: px(1.),
                        color: Some(self.theme.accent),
                        wavy: false,
                    }),
                    ..Default::default()
                };
                let run = style.to_run(preedit.len());
                let shaped = window.text_system().shape_line(
                    SharedString::from(preedit.to_string()),
                    px(self.font_size),
                    &[run],
                    None,
                );
                let _ = shaped.paint(
                    caret_bounds.origin,
                    px(self.line_height),
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
        }

        if let Some(cursor) = prepaint.cursor.take() {
            window.paint_quad(cursor);
        }

        let editor = self.editor.clone();
        let layout = self.layout.clone();
        let line_height = self.line_height;
        let font_size = self.font_size;
        let theme = self.theme.clone();
        window.on_mouse_event({
            let editor = editor.clone();
            let layout = layout.clone();
            let theme = theme.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if !phase.bubble() || event.button != MouseButton::Left {
                    return;
                }
                if !bounds.contains(&event.position) {
                    return;
                }
                let element = BlockTextElement {
                    editor: editor.clone(),
                    layout: layout.clone(),
                    font_size,
                    line_height,
                    theme: theme.clone(),
                    hug_width: false,
                };
                let lines = element.shape(window, bounds.size.width);
                let vis = visible_index_at(&lines, bounds, event.position, px(line_height));
                let source = layout.source_for_visible(vis);
                editor.update(cx, |host, cx| {
                    host.click_source(source, event.modifiers.shift, window, cx);
                });
                window.prevent_default();
            }
        });
        window.on_mouse_event({
            let editor = editor.clone();
            let layout = layout.clone();
            let theme = theme.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if !phase.bubble() {
                    return;
                }
                let selecting = editor.read(cx).is_selecting();
                if !selecting || !event.pressed_button.is_some_and(|b| b == MouseButton::Left) {
                    return;
                }
                let element = BlockTextElement {
                    editor: editor.clone(),
                    layout: layout.clone(),
                    font_size,
                    line_height,
                    theme: theme.clone(),
                    hug_width: false,
                };
                let lines = element.shape(window, bounds.size.width);
                let vis = visible_index_at(&lines, bounds, event.position, px(line_height));
                let source = layout.source_for_visible(vis);
                editor.update(cx, |host, cx| host.drag_source(source, cx));
            }
        });
        window.on_mouse_event({
            move |event: &MouseUpEvent, phase, _, cx| {
                if phase.bubble() && event.button == MouseButton::Left {
                    editor.update(cx, |host, cx| host.end_drag(cx));
                }
            }
        });
    }
}

impl<H: WysiwygHost> BlockTextElement<H> {
    fn shape(&self, window: &mut Window, wrap_width: Pixels) -> Vec<WrappedLine> {
        shape_layout(
            &self.layout,
            window,
            wrap_width,
            self.font_size,
            self.line_height,
            &self.theme,
        )
    }

    fn measure_height(&self, window: &mut Window, wrap: Pixels) -> Pixels {
        let lines = self.shape(window, wrap);
        let lh = px(self.line_height);
        lines
            .iter()
            .map(|l| l.size(lh).height.max(lh))
            .fold(px(0.), |a, b| a + b)
            .max(lh)
    }

    fn measure_unwrapped_width(&self, window: &mut Window) -> Pixels {
        let lines = self.shape(window, px(100_000.));
        lines
            .iter()
            .map(|l| l.width())
            .fold(px(0.), |a, b| a.max(b))
    }
}

fn shape_layout(
    layout: &LeafLayout,
    window: &mut Window,
    wrap_width: Pixels,
    font_size: f32,
    line_height: f32,
    theme: &EditorTheme,
) -> Vec<WrappedLine> {
    let display = if layout.text.is_empty() {
        SharedString::from(" ")
    } else {
        layout.text.clone().into()
    };
    let mut runs = layout.runs.clone();
    if display.as_ref() == " "
        && (runs.is_empty() || runs.iter().map(|r| r.len).sum::<usize>() != 1)
    {
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(font_size).into(),
            line_height: px(line_height).into(),
            ..Default::default()
        };
        runs = vec![style.to_run(1)];
    }
    let covered: usize = runs.iter().map(|r| r.len).sum();
    if covered != display.len() {
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(font_size).into(),
            line_height: px(line_height).into(),
            ..Default::default()
        };
        runs = vec![style.to_run(display.len())];
    }
    window
        .text_system()
        .shape_text(
            display,
            px(font_size),
            &runs,
            Some(wrap_width.max(px(40.))),
            None,
        )
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn source_in_leaf(layout: &LeafLayout, src: usize) -> bool {
    layout.contains_source(src)
}

fn ranges_touch_leaf(layout: &LeafLayout, sel: &Range<usize>) -> bool {
    let start = *layout.source_at.first().unwrap_or(&layout.block_start);
    let end = *layout.source_at.last().unwrap_or(&layout.block_start);
    sel.start < end && sel.end > start || sel.start == sel.end && source_in_leaf(layout, sel.start)
}

fn visible_index_at(
    lines: &[WrappedLine],
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    line_height: Pixels,
) -> usize {
    let mut y = bounds.origin.y;
    let mut offset = 0usize;
    for line in lines {
        let h = line.size(line_height).height.max(line_height);
        let next_y = y + h;
        if position.y < next_y || std::ptr::eq(line, lines.last().unwrap()) {
            let max_y = (h - px(0.5)).max(px(0.));
            let local_y = (position.y - y).max(px(0.)).min(max_y);
            let local = point(position.x - bounds.origin.x, local_y);
            let idx = line
                .closest_index_for_position(local, line_height)
                .unwrap_or_else(|e| e);
            return offset + idx;
        }
        offset += line.len();
        y = next_y;
    }
    offset
}

#[allow(clippy::too_many_arguments)]
fn paint_carets(
    lines: &[WrappedLine],
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    vis_caret: usize,
    vis_sel: Range<usize>,
    text_len: usize,
    show_caret: bool,
    show_sel: bool,
    caret_color: gpui::Hsla,
    sel_color: gpui::Hsla,
) -> (Option<PaintQuad>, Option<PaintQuad>, Option<Bounds<Pixels>>) {
    let _ = text_len;
    let mut cursor = None;
    let mut caret_bounds = None;
    let mut selection = None;
    let mut y = bounds.origin.y;
    let mut offset = 0usize;
    for line in lines {
        let h = line.size(line_height).height.max(line_height);
        let line_end = offset + line.len();
        if show_caret && vis_caret >= offset && vis_caret <= line_end {
            if let Some(pos) =
                line.position_for_index(vis_caret.saturating_sub(offset), line_height)
            {
                let rect = Bounds::new(
                    point(bounds.origin.x + pos.x, y + pos.y),
                    size(px(2.), line_height),
                );
                caret_bounds = Some(rect);
                cursor = Some(fill(rect, caret_color));
            }
        }
        if show_sel {
            let a = vis_sel.start.max(offset);
            let b = vis_sel.end.min(line_end);
            if a < b {
                let pa = line
                    .position_for_index(a.saturating_sub(offset), line_height)
                    .unwrap_or(point(px(0.), px(0.)));
                let pb = line
                    .position_for_index(b.saturating_sub(offset), line_height)
                    .unwrap_or(point(px(0.), px(0.)));
                selection = Some(fill(
                    Bounds::from_corners(
                        point(bounds.origin.x + pa.x, y),
                        point(bounds.origin.x + pb.x.max(pa.x + px(4.)), y + h),
                    ),
                    sel_color,
                ));
            }
        }
        offset = line_end;
        y += h;
    }
    (selection, cursor, caret_bounds)
}

/// Invisible overlay that reports its layout bounds for IME candidate placement
/// while a chip / caption / frontmatter field is being edited.
pub struct WidgetImeSink<H: WysiwygHost> {
    pub editor: Entity<H>,
}

impl<H: WysiwygHost> IntoElement for WidgetImeSink<H> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<H: WysiwygHost> Element for WidgetImeSink<H> {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, Vec::new(), cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.editor.update(cx, |host, _cx| {
            host.report_widget_bounds(bounds);
        });
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.editor.read(cx).input_focus_handle();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
        self.editor.update(cx, |host, _cx| {
            host.sync_ime_cursor(window);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use markrust_core::rich::{
        blank_caret_gap_after_last, blank_caret_gap_before, import_markdown, AlertKind, Block,
        BlockKind, IdGen,
    };

    fn layout_for(source: &str) -> LeafLayout {
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let block = &tree.blocks[0];
        layout_of(block)
    }

    fn layout_of(block: &Block) -> LeafLayout {
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        build_leaf_layout_revealed(
            block,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            &RevealState::HIDDEN,
        )
    }

    fn first_paragraph(block: &Block) -> Option<&Block> {
        if matches!(block.kind, BlockKind::Paragraph) {
            return Some(block);
        }
        for child in &block.children {
            if let Some(p) = first_paragraph(child) {
                return Some(p);
            }
        }
        None
    }

    fn first_kind(blocks: &[Block], pred: impl Fn(&BlockKind) -> bool + Copy) -> Option<&Block> {
        for b in blocks {
            if pred(&b.kind) {
                return Some(b);
            }
            if let Some(found) = first_kind(&b.children, pred) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn visible_offset_maps_to_source_inside_bold() {
        let layout = layout_for("**bold**\n");
        assert_eq!(layout.text, "bold");
        assert_eq!(layout.source_for_visible(0), 2);
        assert_eq!(layout.visible_for_source(2), 0);
        assert_eq!(layout.visible_for_source(6), 4);
    }

    #[test]
    fn image_only_layout_omits_alt_placeholder() {
        let layout = layout_for("![cat](img.png)\n");
        assert!(
            !layout.text.contains("cat") && !layout.text.contains('🖼'),
            "pixels are a sibling img(); text layout must not use an alt placeholder, got {:?}",
            layout.text
        );
        assert!(layout.text.is_empty());
    }

    #[test]
    fn mixed_text_and_image_keeps_surrounding_visible_text() {
        let layout = layout_for("hello ![cat](img.png) world\n");
        assert_eq!(layout.text, "hello  world");
        assert!(!layout.text.contains('🖼'));
    }

    #[test]
    fn strikethrough_is_painted_on_the_visible_run() {
        let layout = layout_for("~~gone~~\n");
        assert_eq!(layout.text, "gone");
        assert!(
            layout.runs.iter().any(|run| run.strikethrough.is_some()),
            "expected strikethrough paint, runs={:?}",
            layout.runs
        );
    }

    #[test]
    fn autolink_is_painted_as_a_link() {
        let layout = layout_for("<https://example.com>\n");
        assert!(
            layout.text.contains("https://example.com"),
            "autolink visible text, got {:?}",
            layout.text
        );
        assert!(
            layout
                .runs
                .iter()
                .any(|run| run.underline.is_some() && run.color != EditorTheme::dark().text),
            "expected link underline + color, runs={:?}",
            layout.runs
        );
    }

    #[test]
    fn hard_break_is_a_visible_newline() {
        let two_spaces = layout_for("a  \nb\n");
        assert_eq!(two_spaces.text, "a\nb", "two-space hard break");
        let backslash = layout_for("a\\\nb\n");
        assert_eq!(backslash.text, "a\nb", "backslash hard break");
    }

    #[test]
    fn soft_break_click_maps_to_newline_not_paragraph_start() {
        let source = "hello\nworld\n";
        let layout = layout_for(source);
        assert_eq!(layout.text, "hello world", "soft break paints as a space");
        let space = layout.text.find(' ').expect("painted soft-break space");
        let mapped = layout.source_for_visible(space);
        assert_eq!(
            source.as_bytes().get(mapped).copied(),
            Some(b'\n'),
            "click on the wrap space must be the source newline, got {mapped} {:?}",
            source.get(mapped..mapped.saturating_add(1))
        );
        assert_ne!(
            mapped, layout.block_start,
            "soft break must not map onto the paragraph start"
        );
        let w = source.find("world").expect("world");
        let vis_w = layout.visible_for_source(w);
        assert_eq!(layout.source_for_visible(vis_w), w);
    }

    #[test]
    fn hard_break_click_maps_to_break_not_paragraph_start() {
        for source in ["a  \nb\n", "a\\\nb\n"] {
            let layout = layout_for(source);
            assert_eq!(layout.text, "a\nb", "{source:?}");
            let vis_nl = layout.text.find('\n').expect("painted hard break");
            let mapped = layout.source_for_visible(vis_nl);
            assert_ne!(
                mapped, layout.block_start,
                "hard break must not map onto paragraph start, {source:?} got {mapped}"
            );
            assert_ne!(
                mapped, 0,
                "hard break click must not jump to `a`, {source:?}"
            );
            let b = source.find('b').expect("b");
            assert!(
                mapped < b,
                "hard break must sit before `b`, {source:?} mapped={mapped}"
            );
            assert_eq!(
                layout.source_for_visible(layout.visible_for_source(b)),
                b,
                "{source:?}"
            );
        }
    }

    #[test]
    fn inline_html_tags_are_hidden_and_inner_text_is_styled() {
        let layout = layout_for("hello <b>bold</b> world\n");
        assert_eq!(layout.text, "hello bold world");
        assert!(
            !layout.text.contains('<'),
            "tags must not paint, got {:?}",
            layout.text
        );
        assert!(
            layout
                .runs
                .iter()
                .any(|run| run.font.weight == gpui::FontWeight::BOLD),
            "expected bold paint on inner HTML text, runs={:?}",
            layout.runs
        );
    }

    #[test]
    fn inline_html_br_is_a_visible_newline() {
        let layout = layout_for("a<br>b\n");
        assert_eq!(layout.text, "a\nb");
    }

    #[test]
    fn inline_html_comment_is_hidden() {
        let layout = layout_for("a<!-- secret -->b\n");
        assert_eq!(layout.text, "ab");
        assert!(!layout.text.contains("secret"));
    }

    #[test]
    fn footnote_ref_paints_as_superscript() {
        let layout = layout_for("Hello[^1]\n\n[^1]: the note\n");
        assert!(
            layout.text.contains('¹') || layout.text.contains('1'),
            "expected a footnote marker, got {:?}",
            layout.text
        );
        assert!(
            !layout.text.contains("[^"),
            "footnote syntax must not paint, got {:?}",
            layout.text
        );
    }

    #[test]
    fn footnote_def_body_paints_nested_bold() {
        let source = "See[^1]\n\n[^1]: **bold** note\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let def = tree
            .blocks
            .iter()
            .find(|b| matches!(b.kind, BlockKind::FootnoteDefinition { .. }))
            .expect("footnote def");
        let body = first_paragraph(def).expect("footnote body");
        let layout = layout_of(body);
        assert_eq!(layout.text, "bold note");
        assert!(
            !layout.text.contains('*'),
            "markers must not paint, got {:?}",
            layout.text
        );
        assert!(
            layout
                .runs
                .iter()
                .any(|run| run.font.weight == gpui::FontWeight::BOLD),
            "expected bold paint inside footnote, runs={:?}",
            layout.runs
        );
    }

    #[test]
    fn definition_details_paint_nested_bold() {
        let source = "Term\n\n: **bold** details\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let details = first_kind(&tree.blocks, |k| matches!(k, BlockKind::DefinitionDetails))
            .expect("definition details");
        let body = first_paragraph(details).expect("details body");
        let layout = layout_of(body);
        assert_eq!(layout.text, "bold details");
        assert!(
            !layout.text.contains('*'),
            "markers must not paint, got {:?}",
            layout.text
        );
        assert!(
            layout
                .runs
                .iter()
                .any(|run| run.font.weight == gpui::FontWeight::BOLD),
            "expected bold paint inside definition details, runs={:?}",
            layout.runs
        );
    }

    #[test]
    fn code_layout_maps_bytes_one_to_one() {
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.code_font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        let source = "```rust\nfn x() {}\n```\n";
        let tree = markrust_core::rich::import_markdown(source, &mut IdGen::default());
        let block = tree
            .blocks
            .iter()
            .find(|b| matches!(b.kind, BlockKind::CodeBlock { .. }))
            .expect("code block");
        let layout = build_code_block_layout("fn x() {}", source, block, &style, &theme);
        assert_eq!(layout.text, "fn x() {}");
        assert!(layout.source_for_visible(0) >= block.code_body_range(source).start);
        assert!(layout.contains_source(block.code_body_range(source).start + "fn".len()));
        assert!(!layout.contains_source(0));
    }

    fn html_block_layout(raw: &str) -> LeafLayout {
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        match markrust_core::html_visual::project_html_block(raw) {
            markrust_core::html_visual::HtmlBlockVisual::Flow {
                text,
                source_at,
                runs,
            } => build_html_block_layout(
                &text,
                &source_at,
                &runs,
                raw,
                &mk_opaque_block(raw),
                &style,
                &theme,
            ),
            other => panic!("expected flow, got {other:?}"),
        }
    }

    fn mk_opaque_block(raw: &str) -> Block {
        Block {
            id: NodeId(0),
            kind: BlockKind::Opaque {
                raw: raw.to_string(),
            },
            source_range: 0..raw.len(),
            inlines: Vec::new(),
            children: Vec::new(),
            content_hash: 0,
        }
    }

    #[test]
    fn highlight_eqeq_hides_delimiters_and_paints_background() {
        let layout = layout_for("hello ==mark== world\n");
        assert_eq!(layout.text, "hello mark world");
        assert!(
            !layout.text.contains('='),
            "highlight delimiters must not paint, got {:?}",
            layout.text
        );
        assert!(
            layout.runs.iter().any(|run| run.background_color.is_some()),
            "expected highlight background, runs={:?}",
            layout.runs
        );
    }

    #[test]
    fn highlight_eqeq_wraps_nested_bold() {
        let layout = layout_for("==**bold**==\n");
        assert_eq!(layout.text, "bold");
        assert!(
            layout
                .runs
                .iter()
                .any(|run| run.font.weight == gpui::FontWeight::BOLD),
            "expected bold inside highlight, runs={:?}",
            layout.runs
        );
        assert!(
            layout.runs.iter().any(|run| run.background_color.is_some()),
            "expected highlight background, runs={:?}",
            layout.runs
        );
    }

    fn layout_for_caret(source: &str, caret: usize) -> LeafLayout {
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let block = &tree.blocks[0];
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        build_leaf_layout_revealed(
            block,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            &RevealState {
                caret,
                selection: 0..0,
            },
        )
    }

    fn math_run_is_styled(run: &gpui::TextRun, theme: &EditorTheme) -> bool {
        run.font.style == gpui::FontStyle::Italic
            && run.font.family.as_ref() == theme.code_font_family.as_str()
    }

    #[test]
    fn math_hides_dollars_and_paints_formula_style() {
        let layout = layout_for("see $x^2$ here\n");
        assert_eq!(layout.text, "see x^2 here");
        assert!(
            !layout.text.contains('$'),
            "math delimiters must not paint when caret is outside, got {:?}",
            layout.text
        );
        let theme = EditorTheme::dark();
        assert!(
            layout
                .runs
                .iter()
                .any(|run| math_run_is_styled(run, &theme)),
            "expected italic monospace math, runs={:?}",
            layout.runs
        );
    }

    #[test]
    fn math_reveals_dollars_when_caret_intersects() {
        let source = "see $x^2$ here\n";
        let inside = source.find('x').unwrap();
        let layout = layout_for_caret(source, inside);
        assert!(
            layout.text.contains("$x^2$"),
            "expected revealed $…$, got {:?}",
            layout.text
        );
        assert_eq!(layout.text, "see $x^2$ here");
    }

    #[test]
    fn wikilink_hides_brackets_and_paints_as_link() {
        let layout = layout_for("see [[page]] here\n");
        assert_eq!(layout.text, "see page here");
        assert!(
            !layout.text.contains('[') && !layout.text.contains(']'),
            "wiki brackets must not paint when caret is outside, got {:?}",
            layout.text
        );
        assert!(
            layout.runs.iter().any(|run| run.underline.is_some()),
            "expected link underline, runs={:?}",
            layout.runs
        );
        let piped = layout_for("go [[page|Label]]\n");
        assert_eq!(piped.text, "go Label");
        assert!(
            !piped.text.contains("page"),
            "target must hide when labeled"
        );
    }

    #[test]
    fn wikilink_reveals_brackets_when_caret_intersects() {
        let source = "see [[page]] here\n";
        let inside = source.find("page").unwrap();
        let layout = layout_for_caret(source, inside);
        assert!(
            layout.text.contains("[[page]]"),
            "expected revealed [[…]], got {:?}",
            layout.text
        );
        assert_eq!(layout.text, "see [[page]] here");
    }

    #[test]
    fn emoji_hides_shortcode_and_paints_glyph() {
        let layout = layout_for("see :smile: here\n");
        assert_eq!(layout.text, "see 😄 here");
        assert!(
            !layout.text.contains(":smile:"),
            "shortcode must not paint when caret is outside, got {:?}",
            layout.text
        );
        let heart = layout_for("love :heart: now\n");
        assert!(
            heart.text.contains("❤️"),
            "expected heart glyph, got {:?}",
            heart.text
        );
        assert!(!heart.text.contains(":heart:"));
        let plus = layout_for(":+1:\n");
        assert_eq!(plus.text, "👍");
        let rocket = layout_for(":rocket:\n");
        assert_eq!(rocket.text, "🚀");
    }

    #[test]
    fn emoji_reveals_shortcode_when_caret_intersects() {
        let source = "see :smile: here\n";
        let inside = source.find("smile").unwrap();
        let layout = layout_for_caret(source, inside);
        assert!(
            layout.text.contains(":smile:"),
            "expected revealed :smile:, got {:?}",
            layout.text
        );
        assert!(
            !layout.text.contains("😄"),
            "glyph must hide while caret is in the shortcode, got {:?}",
            layout.text
        );
        assert_eq!(layout.text, "see :smile: here");
        let on_colon = layout_for_caret(source, source.find(":smile:").unwrap());
        assert!(
            on_colon.text.contains(":smile:"),
            "caret on opening colon must reveal, got {:?}",
            on_colon.text
        );
        let start = source.find(":smile:").unwrap();
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        let selected = build_leaf_layout_revealed(
            &tree.blocks[0],
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            &RevealState {
                caret: 0,
                selection: start..start + 7,
            },
        );
        assert!(
            selected.text.contains(":smile:"),
            "selection overlap must reveal, got {:?}",
            selected.text
        );
    }

    #[test]
    fn unknown_emoji_shortcode_stays_visible() {
        let layout = layout_for("see :not_an_emoji: here\n");
        assert_eq!(layout.text, "see :not_an_emoji: here");
        let mixed = layout_for("a :smile: and :foo: b\n");
        assert!(
            mixed.text.contains("😄"),
            "known name must paint, got {:?}",
            mixed.text
        );
        assert!(
            mixed.text.contains(":foo:"),
            "unknown name must stay, got {:?}",
            mixed.text
        );
        assert!(!mixed.text.contains(":smile:"));
    }

    #[test]
    fn display_math_hides_double_dollars() {
        let layout = layout_for("$$E=mc^2$$\n");
        assert!(
            !layout.text.contains("$$"),
            "display delimiters must not paint, got {:?}",
            layout.text
        );
        assert!(
            layout.text.contains("E=mc^2"),
            "expected formula body, got {:?}",
            layout.text
        );
        let revealed = layout_for_caret("$$E=mc^2$$\n", 2);
        assert!(
            revealed.text.contains("$$"),
            "caret inside display math must reveal $$, got {:?}",
            revealed.text
        );
    }

    #[test]
    fn currency_and_code_are_not_math_layout() {
        let five = layout_for("costs $5\n");
        assert_eq!(five.text, "costs $5");
        let theme = EditorTheme::dark();
        assert!(
            !five.runs.iter().any(|run| math_run_is_styled(run, &theme)),
            "currency must not use math style, runs={:?}",
            five.runs
        );
        let code = layout_for("`$x$`\n");
        assert_eq!(code.text, "$x$");
        assert!(
            code.runs.iter().any(|run| run.background_color.is_some()),
            "expected inline code, runs={:?}",
            code.runs
        );
    }

    #[test]
    fn html_mark_paints_background_not_tags() {
        let layout = layout_for("a <mark>hot</mark> b\n");
        assert_eq!(layout.text, "a hot b");
        assert!(!layout.text.contains('<'));
        assert!(
            layout.runs.iter().any(|run| run.background_color.is_some()),
            "expected <mark> background, runs={:?}",
            layout.runs
        );
    }

    #[test]
    fn html_sub_sup_use_unicode_when_mapped() {
        let layout = layout_for("H<sub>2</sub>O and x<sup>2</sup>\n");
        assert!(
            layout.text.contains('₂'),
            "expected subscript two, got {:?}",
            layout.text
        );
        assert!(
            layout.text.contains('²'),
            "expected superscript two, got {:?}",
            layout.text
        );
        assert!(!layout.text.contains("<sub"));
        assert!(!layout.text.contains("<sup"));
    }

    #[test]
    fn markdown_sub_sup_use_unicode() {
        let sub = layout_for("H~2~O\n");
        assert!(
            sub.text.contains('₂'),
            "expected ~2~ subscript, got {:?}",
            sub.text
        );
        assert!(
            !sub.text.contains('~'),
            "tilde must not paint, {:?}",
            sub.text
        );
        let sup = layout_for("mc^2^\n");
        assert!(
            sup.text.contains('²'),
            "expected ^2^ superscript, got {:?}",
            sup.text
        );
        assert!(
            !sup.text.contains('^'),
            "caret must not paint, {:?}",
            sup.text
        );
    }

    #[test]
    fn html_block_inner_markdown_paints_bold() {
        let layout = html_block_layout("<div>\n**bold** and `code`\n</div>");
        assert!(
            layout.text.contains("bold"),
            "inner text, got {:?}",
            layout.text
        );
        assert!(
            !layout.text.contains('*'),
            "markdown markers must not paint, got {:?}",
            layout.text
        );
        assert!(
            !layout.text.contains("<div"),
            "html tags must not paint, got {:?}",
            layout.text
        );
        assert!(
            layout
                .runs
                .iter()
                .any(|run| run.font.weight == gpui::FontWeight::BOLD),
            "expected nested bold in HTML block, runs={:?}",
            layout.runs
        );
    }

    fn alert_body_layout(source: &str, caret: usize) -> LeafLayout {
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let alert =
            first_kind(&tree.blocks, |k| matches!(k, BlockKind::Alert { .. })).expect("alert");
        let para = first_paragraph(alert).expect("alert body");
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        build_leaf_layout_revealed(
            para,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            &RevealState {
                caret,
                selection: 0..0,
            },
        )
    }

    #[test]
    fn github_alert_body_hides_tag_for_each_kind() {
        for kind in AlertKind::ALL {
            let source = format!("> [!{}]\n> hello {}\n", kind.tag(), kind.label());
            let layout = alert_body_layout(&source, usize::MAX);
            assert!(
                !layout.text.contains("[!"),
                "[!{}] must not paint in the body, got {:?}",
                kind.tag(),
                layout.text
            );
            assert!(
                layout.text.contains("hello"),
                "expected body text, got {:?}",
                layout.text
            );
            assert_eq!(kind.callout_label(None), kind.label(), "{}", kind.tag());
        }
    }

    #[test]
    fn github_alert_reveals_chrome_when_caret_intersects() {
        let source = "> [!NOTE]\n> hello\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let BlockKind::Alert {
            chrome_range,
            tag_range,
            kind,
            title,
        } = &tree.blocks[0].kind
        else {
            panic!("expected alert, got {:?}", tree.blocks[0].kind);
        };
        assert_eq!(*kind, AlertKind::Note);
        assert_eq!(kind.callout_label(title.as_deref()), "Note");
        assert_eq!(&source[tag_range.clone()], "[!NOTE]");
        let outside = RevealState::HIDDEN;
        assert!(!outside.intersects(chrome_range));
        let on_tag = RevealState {
            caret: tag_range.start,
            selection: 0..0,
        };
        assert!(on_tag.intersects(chrome_range));
        let body = alert_body_layout(source, source.find("hello").unwrap());
        assert_eq!(body.text, "hello");
        assert!(!body.text.contains("[!NOTE]"));
    }

    #[test]
    fn blank_gap_layout_hosts_caret_before_heading() {
        let source = "\n\n# Title";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let gap = blank_caret_gap_before(&tree, 0).expect("leading blank above heading");
        let layout = build_blank_gap_layout(gap);
        assert!(
            layout.contains_source(0),
            "caret on inserted newlines must paint on the blank, source_at={:?}",
            layout.source_at
        );
        let title = source.find("Title").expect("Title");
        assert!(
            !layout.contains_source(title),
            "blank layout must not claim `# Title`"
        );
        let heading = layout_of(&tree.blocks[0]);
        assert_eq!(heading.text, "Title");
        assert!(
            !heading.contains_source(0),
            "heading leaf must not steal the blank caret"
        );
        assert!(heading.contains_source(title));
    }

    #[test]
    fn blank_gap_layout_hosts_caret_between_paragraph_and_heading() {
        let source = "hello\n\n# Title";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let gap = blank_caret_gap_before(&tree, 1).expect("standard separator before heading");
        let layout = build_blank_gap_layout(gap.clone());
        assert!(
            layout.contains_source(gap.start),
            "click on the separator must map onto the gap, source_at={:?} gap={gap:?}",
            layout.source_at
        );
        let title = source.find("Title").expect("Title");
        assert!(
            !layout.contains_source(title),
            "separator layout must not claim `# Title`"
        );
        let heading = layout_of(&tree.blocks[1]);
        assert_eq!(heading.text, "Title");
        assert!(
            !heading.contains_source(gap.start),
            "heading leaf must not steal the separator caret"
        );
        assert!(heading.contains_source(title));
        assert!(
            blank_caret_gap_before(&tree, 0).is_none(),
            "must not paint a leading blank when the document starts with a paragraph"
        );
    }

    #[test]
    fn blank_gap_layout_hosts_caret_after_last_block() {
        let source = "hello\n\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let gap = blank_caret_gap_after_last(&tree).expect("trailing blank after last block");
        let layout = build_blank_gap_layout(gap.clone());
        assert!(
            layout.contains_source(gap.start),
            "click below the last block must map onto the trailing gap, source_at={:?} gap={gap:?}",
            layout.source_at
        );
        let hello = source.find("hello").expect("hello");
        assert!(
            !layout.contains_source(hello),
            "trailing layout must not claim the last paragraph"
        );
        let para = layout_of(&tree.blocks[0]);
        assert_eq!(para.text, "hello");
        assert!(
            !para.contains_source(gap.start),
            "last paragraph must not steal the trailing caret"
        );
        assert!(para.contains_source(hello));
    }

    #[test]
    fn blank_gap_layout_hosts_caret_on_newlines_only_document() {
        for source in ["", "\n", "\n\n"] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            assert!(
                tree.blocks.is_empty(),
                "newlines-only has no blocks, {source:?}"
            );
            let gap = blank_caret_gap_after_last(&tree)
                .expect("newlines-only document must paint a caret home");
            let layout = build_blank_gap_layout(gap.clone());
            assert!(
                layout.contains_source(0),
                "empty/newlines document must host a caret at 0, source_at={:?} {source:?}",
                layout.source_at
            );
            assert!(
                layout.contains_source(gap.start),
                "layout must cover the gap start, {source:?}"
            );
        }
    }

    #[test]
    fn blank_gap_layout_does_not_invent_trailing_without_blank() {
        for source in ["hello", "hello\n", "# Title"] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            assert!(
                blank_caret_gap_after_last(&tree).is_none(),
                "must not invent a trailing blank, {source:?}"
            );
        }
    }
}
