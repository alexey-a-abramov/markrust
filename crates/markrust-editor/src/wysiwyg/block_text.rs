// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Interactive leaf text: wrap-aware hit-testing, caret, and selection paint.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    fill, point, px, relative, rgb, size, App, AvailableSpace, Bounds, Context, Element,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, Pixels, Point, SharedString, Style, TextRun, TextStyle,
    UnderlineStyle, Window, WrappedLine,
};
use markrust_core::html_visual::{
    classify_opaque_inline, html_block_is_dangerous, html_block_is_hidden_widget,
    html_block_is_preformatted, html_block_is_source_chrome, html_block_wrapper_tag_ranges,
    project_html_block, HtmlBlockVisual, HtmlPaintRun,
};
use markrust_core::rich::{
    code_body_source_map, code_span_visible_range, expand_link_and_html_chrome,
    grow_mark_delimiters, html_block_literal_source_map, import_markdown,
    is_decoded_backslash_escape, is_decoded_character_reference, line_prefix_parts,
    link_reference_def_chrome, markdown_link_chrome, tagfilter_widget_ranges_in, Block, BlockKind,
    BreakStyle, HeadingStyle, IdGen, Inline, LinkAttrs, MarkFidelity, MarkSet, MarkdownLinkChrome,
    NodeId, PrefixBlank, RichTree,
};

use crate::theme::EditorTheme;

use super::ime::{VisualCaretStop, VisualLine};

/// Which chip / caption / frontmatter overlay a click should focus.
#[derive(Debug, Clone)]
pub enum OverlayTarget {
    CodeInfo(NodeId),
    ImageAlt { range: Range<usize>, stored: String },
    Frontmatter { key: &'static str, stored: String },
    FrontmatterYaml { stored: String },
}

/// Map a shaped-text visible index onto the overlay draft.
pub fn overlay_draft_offset(
    prefix_len: usize,
    vis: usize,
    draft_len: usize,
    caret: usize,
    preedit_len: usize,
) -> usize {
    let vis = vis.saturating_sub(prefix_len);
    let caret = caret.min(draft_len);
    if vis <= caret {
        vis.min(draft_len)
    } else if vis <= caret + preedit_len {
        caret
    } else {
        vis.saturating_sub(preedit_len).min(draft_len)
    }
}

/// Host implemented by [`super::view::RichEditorView`].
pub trait WysiwygHost: gpui::Render + EntityInputHandler + 'static {
    fn click_source(
        &mut self,
        source: usize,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    );
    fn select_source_range(
        &mut self,
        range: Range<usize>,
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
    fn overlay_preedit(&self) -> Option<&str>;
    fn widget_caret_offset(&self) -> usize;
    fn widget_sel(&self) -> Range<usize>;
    fn is_widget_selecting(&self) -> bool;
    fn click_overlay(
        &mut self,
        target: OverlayTarget,
        offset: usize,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    );
    fn drag_overlay(&mut self, offset: usize, cx: &mut Context<Self>);
    fn end_overlay_drag(&mut self, cx: &mut Context<Self>);
    fn report_widget_bounds(&mut self, bounds: Bounds<Pixels>);
    fn report_widget_caret(&mut self, caret: Bounds<Pixels>);
    fn report_leaf(
        &mut self,
        layout: Arc<LeafLayout>,
        element_bounds: Bounds<Pixels>,
        font_size: f32,
        line_height: f32,
        caret_bounds: Option<Bounds<Pixels>>,
        visual_lines: Vec<VisualLine>,
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

/// Caret/selection used to reveal GFM wrap marks (`*` / `**` / `_` / `~~` /
/// ticks / `==`), ATX `#` / setext underlines, markdown link `[` `]` /
/// `](url)` dest, image `![]()`, `$` / `$$`, `[[wiki]]`, `:emoji:`,
/// autolink `<>`, list/task markers, quote `>`, fence ticks, table `|`,
/// thematic `---` / `<hr>` source, HTML-block `<div>` / `</div>`,
/// definition-list `: `, footnote-definition `[^1]:`, and footnote-ref
/// `[^1]` in WYSIWYG (Typora: hide chrome unless the caret or a non-empty
/// selection intersects the span). Link dest `(url)` stays hidden while the
/// caret is only in the label; it paints when the caret is in dest or a
/// selection overlaps the link node (source-masking intersect). Thematic
/// breaks still paint as a rule widget when the caret is outside. Footnote
/// refs paint as a superscript when the caret is outside.
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

/// Ancestor container ranges that control block-prefix / structural chrome.
/// List markers reveal per item; quote `>` for the whole quote; table `|`
/// for the whole table; definition-list `: ` per details block; footnote
/// `[^1]:` per definition (same intersect rule as source-mode masking).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChromeHosts {
    pub quote: Option<Range<usize>>,
    pub list_item: Option<Range<usize>>,
    pub table: Option<Range<usize>>,
    pub footnote_def: Option<Range<usize>>,
    pub definition_details: Option<Range<usize>>,
}

impl ChromeHosts {
    pub const NONE: Self = Self {
        quote: None,
        list_item: None,
        table: None,
        footnote_def: None,
        definition_details: None,
    };
}

pub fn chrome_hosts_for(tree: &RichTree, target: NodeId) -> ChromeHosts {
    fn walk(blocks: &[Block], target: NodeId, acc: &mut ChromeHosts) -> bool {
        for b in blocks {
            let saved = acc.clone();
            match &b.kind {
                BlockKind::BlockQuote => acc.quote = Some(b.source_range.clone()),
                BlockKind::ListItem { .. } => acc.list_item = Some(b.source_range.clone()),
                BlockKind::Table { .. } => acc.table = Some(b.source_range.clone()),
                BlockKind::FootnoteDefinition { .. } => {
                    acc.footnote_def = Some(b.source_range.clone())
                }
                BlockKind::DefinitionDetails => {
                    acc.definition_details = Some(b.source_range.clone())
                }
                _ => {}
            }
            if b.id == target || walk(&b.children, target, acc) {
                return true;
            }
            *acc = saved;
        }
        false
    }
    let mut acc = ChromeHosts::NONE;
    walk(&tree.blocks, target, &mut acc);
    acc
}

/// Wrap-mark kinds painted when [`RevealState`] intersects the outer span.
/// Inner-first so `***x***` peels italic `*` before bold `**`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum WrapKind {
    Code,
    Highlight,
    Sup,
    Sub,
    Italic,
    Strike,
    Bold,
}

#[derive(Debug, Clone)]
struct WrapGroup {
    span: Range<usize>,
    sides: HashMap<usize, WrapSides>,
}

#[derive(Debug, Clone, Default)]
struct WrapSides {
    open: Option<Range<usize>>,
    close: Option<Range<usize>>,
}

fn wrap_specs(marks: MarkSet, fid: &MarkFidelity) -> Vec<(WrapKind, u8, usize, u64)> {
    let mut out = Vec::new();
    if marks.contains(MarkSet::CODE) {
        out.push((WrapKind::Code, b'`', fid.code_backticks.max(1), 0));
    }
    if marks.contains(MarkSet::HIGHLIGHT) {
        out.push((WrapKind::Highlight, b'=', 2, fid.highlight_group));
    }
    if marks.contains(MarkSet::SUP) {
        out.push((WrapKind::Sup, b'^', 1, fid.sup_group));
    }
    if marks.contains(MarkSet::SUB) {
        out.push((WrapKind::Sub, b'~', 1, fid.sub_group));
    }
    if marks.contains(MarkSet::ITALIC) {
        out.push((WrapKind::Italic, fid.emph_delim, 1, fid.emph_group));
    }
    if marks.contains(MarkSet::STRIKE) {
        out.push((WrapKind::Strike, b'~', 2, fid.strike_group));
    }
    if marks.contains(MarkSet::BOLD) {
        out.push((WrapKind::Bold, fid.strong_delim, 2, fid.strong_group));
    }
    out
}

fn range_is_delim(source: &str, range: &Range<usize>, delim: u8, lo: usize, hi: usize) -> bool {
    if range.start < lo || range.end > hi || range.start >= range.end {
        return false;
    }
    let Some(slice) = source.get(range.clone()) else {
        return false;
    };
    !slice.is_empty() && slice.bytes().all(|b| b == delim)
}

fn peel_wrap(source: &str, mut inner: Range<usize>, delim: u8, width: usize) -> Range<usize> {
    if width == 0 || inner.end < inner.start.saturating_add(width.saturating_mul(2)) {
        return inner;
    }
    let open = inner.start..inner.start + width;
    let close = inner.end - width..inner.end;
    if range_is_delim(source, &open, delim, 0, source.len())
        && range_is_delim(source, &close, delim, 0, source.len())
    {
        inner.start += width;
        inner.end -= width;
    }
    inner
}

fn run_mark_info(inline: &Inline) -> Option<(Range<usize>, MarkSet, MarkFidelity)> {
    match inline {
        Inline::Run {
            source_range,
            marks,
            fidelity,
            ..
        } if !marks.is_empty() => Some((source_range.clone(), *marks, *fidelity)),
        Inline::Emoji {
            source_range,
            marks,
            fidelity,
            ..
        } if !marks.is_empty() => Some((source_range.clone(), *marks, *fidelity)),
        Inline::Image {
            source_range,
            marks,
            ..
        } if !marks.is_empty() => Some((source_range.clone(), *marks, MarkFidelity::default())),
        Inline::OpaqueInline {
            source_range,
            marks,
            raw,
            ..
        } if !marks.is_empty() && markrust_core::html_visual::html_inline_image(raw).is_some() => {
            Some((source_range.clone(), *marks, MarkFidelity::default()))
        }
        _ => None,
    }
}

fn adjacent_wrap_layers(
    source: &str,
    inner: Range<usize>,
    marks: MarkSet,
    fid: &MarkFidelity,
    lo: usize,
    hi: usize,
    link: Option<&LinkAttrs>,
) -> Vec<(WrapKind, WrapSides, Range<usize>)> {
    let specs = wrap_specs(marks, fid);
    let mut core = expand_link_and_html_chrome(source, inner, link, lo, hi);
    for (_kind, delim, width, _) in &specs {
        core = peel_wrap(source, core, *delim, *width);
    }
    let mut used = Vec::new();
    let mut layers = Vec::new();
    loop {
        let mut found = None;
        for (kind, delim, width, _) in &specs {
            if used.contains(kind) {
                continue;
            }
            let open = core.start.saturating_sub(*width)..core.start;
            let close = core.end..core.end.saturating_add(*width);
            let has_open = range_is_delim(source, &open, *delim, lo, hi);
            let has_close = range_is_delim(source, &close, *delim, lo, hi);
            if has_open || has_close {
                found = Some((
                    *kind,
                    WrapSides {
                        open: has_open.then_some(open),
                        close: has_close.then_some(close),
                    },
                    has_open,
                    has_close,
                    *width,
                ));
                break;
            }
        }
        let Some((kind, sides, has_open, has_close, width)) = found else {
            break;
        };
        if has_open {
            core.start = core.start.saturating_sub(width);
        }
        if has_close {
            core.end = core.end.saturating_add(width).min(hi);
        }
        let span_start = sides.open.as_ref().map(|r| r.start).unwrap_or(core.start);
        let span_end = sides.close.as_ref().map(|r| r.end).unwrap_or(core.end);
        used.push(kind);
        layers.push((kind, sides, span_start..span_end));
    }
    layers
}

fn wrap_group_key(kind: WrapKind, group: u64, inline_i: usize) -> (WrapKind, u64, usize) {
    if group == 0 {
        (kind, 0, inline_i)
    } else {
        (kind, group, usize::MAX)
    }
}

fn collect_wrap_groups(
    source: &str,
    inlines: &[Inline],
    block_range: &Range<usize>,
) -> HashMap<(WrapKind, u64, usize), WrapGroup> {
    let mut groups: HashMap<(WrapKind, u64, usize), WrapGroup> = HashMap::new();
    if source.is_empty() {
        return groups;
    }
    let lo = block_range.start;
    let hi = block_range.end.min(source.len());
    for (i, inline) in inlines.iter().enumerate() {
        let Some((range, marks, fid)) = run_mark_info(inline) else {
            continue;
        };
        let link = match inline {
            Inline::Run { link, .. } | Inline::Emoji { link, .. } | Inline::Image { link, .. } => {
                link.as_ref()
            }
            _ => None,
        };
        let specs = wrap_specs(marks, &fid);
        let layers = adjacent_wrap_layers(source, range, marks, &fid, lo, hi, link);
        for (kind, sides, layer_span) in layers {
            let group_id = specs
                .iter()
                .find(|(k, _, _, _)| *k == kind)
                .map(|(_, _, _, g)| *g)
                .unwrap_or(0);
            let key = wrap_group_key(kind, group_id, i);
            let entry = groups.entry(key).or_insert_with(|| WrapGroup {
                span: layer_span.clone(),
                sides: HashMap::new(),
            });
            entry.span.start = entry.span.start.min(layer_span.start);
            entry.span.end = entry.span.end.max(layer_span.end);
            entry.sides.insert(i, sides);
        }
    }
    groups
}

fn revealed_wrap_lists(
    groups: &HashMap<(WrapKind, u64, usize), WrapGroup>,
    inlines: &[Inline],
    inline_i: usize,
    reveal: &RevealState,
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let mut opens: Vec<Range<usize>> = Vec::new();
    let mut closes: Vec<Range<usize>> = Vec::new();
    for group in groups.values() {
        if !reveal.intersects(&group.span) {
            continue;
        }
        for sides in group.sides.values() {
            if let Some(open) = &sides.open {
                if first_inline_starting_at_or_after(inlines, open.end) == Some(inline_i) {
                    opens.push(open.clone());
                }
            }
            if let Some(close) = &sides.close {
                if last_inline_ending_at_or_before(inlines, close.start) == Some(inline_i) {
                    closes.push(close.clone());
                }
            }
        }
    }
    opens.sort_by_key(|r| r.start);
    opens.dedup();
    closes.sort_by_key(|r| r.start);
    closes.dedup();
    (opens, closes)
}

fn first_inline_starting_at_or_after(inlines: &[Inline], byte: usize) -> Option<usize> {
    inlines
        .iter()
        .enumerate()
        .filter(|(_, inline)| {
            let r = inline.source_range();
            r.start >= byte || r.end > byte
        })
        .min_by_key(|(_, inline)| {
            let r = inline.source_range();
            if r.start == byte {
                (0u8, r.start)
            } else if r.start < byte {
                (1, r.start)
            } else {
                (2, r.start)
            }
        })
        .map(|(i, _)| i)
}

fn last_inline_ending_at_or_before(inlines: &[Inline], byte: usize) -> Option<usize> {
    inlines
        .iter()
        .enumerate()
        .filter(|(_, inline)| {
            let r = inline.source_range();
            r.end <= byte || r.start < byte
        })
        .max_by_key(|(i, inline)| {
            let r = inline.source_range();
            if r.end == byte {
                (2u8, r.end, *i)
            } else if r.start < byte && r.end > byte {
                (1, r.end, *i)
            } else {
                (0, r.end, *i)
            }
        })
        .map(|(i, _)| i)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum LinkPaintKey {
    Group(u64),
    Image(usize),
}

struct LinkPaintGroup {
    chrome: MarkdownLinkChrome,
    members: Vec<usize>,
    /// Image widgets are one caret/click unit: dest paints with the node.
    atomic_dest: bool,
}

fn md_link_is_angle(link: &LinkAttrs) -> bool {
    link.autolink || link.angle
}

fn link_outer_for_inner(
    source: &str,
    inner: Range<usize>,
    link: &LinkAttrs,
    lo: usize,
    hi: usize,
) -> Range<usize> {
    let marked = grow_mark_delimiters(source, inner.start.max(lo)..inner.end.min(hi));
    expand_link_and_html_chrome(source, marked, Some(link), lo, hi)
}

fn collect_link_groups(
    source: &str,
    inlines: &[Inline],
    block_range: &Range<usize>,
) -> HashMap<LinkPaintKey, LinkPaintGroup> {
    let mut groups: HashMap<LinkPaintKey, LinkPaintGroup> = HashMap::new();
    if source.is_empty() {
        return groups;
    }
    let lo = block_range.start;
    let hi = block_range.end.min(source.len());
    for (i, inline) in inlines.iter().enumerate() {
        match inline {
            Inline::Run {
                source_range,
                link: Some(link),
                ..
            }
            | Inline::Emoji {
                source_range,
                link: Some(link),
                ..
            } if !md_link_is_angle(link) => {
                let outer = link_outer_for_inner(source, source_range.clone(), link, lo, hi);
                let outer = outer.start.max(lo)..outer.end.min(hi);
                if let Some(chrome) = markdown_link_chrome(source, outer) {
                    insert_link_group(
                        &mut groups,
                        LinkPaintKey::Group(link.group),
                        chrome,
                        i,
                        false,
                    );
                }
            }
            Inline::Image {
                source_range, link, ..
            } => {
                let img_span = source_range.start.max(lo)..source_range.end.min(hi);
                if let Some(chrome) = markdown_link_chrome(source, img_span) {
                    insert_link_group(&mut groups, LinkPaintKey::Image(i), chrome, i, true);
                }
                if let Some(link) = link.as_ref() {
                    if !md_link_is_angle(link) {
                        let outer =
                            link_outer_for_inner(source, source_range.clone(), link, lo, hi);
                        let outer = outer.start.max(lo)..outer.end.min(hi);
                        if let Some(chrome) = markdown_link_chrome(source, outer) {
                            insert_link_group(
                                &mut groups,
                                LinkPaintKey::Group(link.group),
                                chrome,
                                i,
                                false,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
    groups
}

fn insert_link_group(
    groups: &mut HashMap<LinkPaintKey, LinkPaintGroup>,
    key: LinkPaintKey,
    chrome: MarkdownLinkChrome,
    inline_i: usize,
    atomic_dest: bool,
) {
    let entry = groups.entry(key).or_insert_with(|| LinkPaintGroup {
        chrome: chrome.clone(),
        members: Vec::new(),
        atomic_dest,
    });
    entry.chrome.outer.start = entry.chrome.outer.start.min(chrome.outer.start);
    entry.chrome.outer.end = entry.chrome.outer.end.max(chrome.outer.end);
    if entry.members.is_empty() {
        entry.chrome.open = chrome.open.clone();
    }
    entry.chrome.close = chrome.close.clone();
    entry.chrome.dest = chrome.dest.clone();
    if !entry.members.contains(&inline_i) {
        entry.members.push(inline_i);
    }
}

fn reveal_link_dest(
    reveal: &RevealState,
    dest: &Range<usize>,
    outer: &Range<usize>,
    atomic: bool,
) -> bool {
    if dest.start >= dest.end {
        return false;
    }
    if atomic {
        return reveal.intersects(outer);
    }
    if reveal.intersects(dest) {
        return true;
    }
    !reveal.selection.is_empty() && reveal.intersects(outer)
}

fn revealed_link_sides(
    groups: &HashMap<LinkPaintKey, LinkPaintGroup>,
    inlines: &[Inline],
    inline_i: usize,
    reveal: &RevealState,
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let mut opens = Vec::new();
    let mut closes = Vec::new();
    for group in groups.values() {
        if !reveal.intersects(&group.chrome.outer) {
            continue;
        }
        if !group.chrome.open.is_empty()
            && first_inline_starting_at_or_after(inlines, group.chrome.open.end) == Some(inline_i)
        {
            opens.push(group.chrome.open.clone());
        }
        if !group.chrome.close.is_empty()
            && last_inline_ending_at_or_before(inlines, group.chrome.close.start) == Some(inline_i)
        {
            closes.push(group.chrome.close.clone());
        }
        if reveal_link_dest(
            reveal,
            &group.chrome.dest,
            &group.chrome.outer,
            group.atomic_dest,
        ) && !group.chrome.dest.is_empty()
            && last_inline_ending_at_or_before(inlines, group.chrome.dest.start) == Some(inline_i)
        {
            closes.push(group.chrome.dest.clone());
        }
    }
    opens.sort_by_key(|r| r.start);
    opens.dedup();
    closes.sort_by_key(|r| r.start);
    closes.dedup();
    (opens, closes)
}

struct HtmlPaintPair {
    span: Range<usize>,
    open: Range<usize>,
    close: Range<usize>,
    open_i: usize,
    close_i: Option<usize>,
}

struct HtmlSoloChrome {
    range: Range<usize>,
    inline_i: usize,
}

struct HtmlPaintGroups {
    pairs: Vec<HtmlPaintPair>,
    solos: Vec<HtmlSoloChrome>,
}

fn collect_html_groups(inlines: &[Inline]) -> HtmlPaintGroups {
    use markrust_core::html_visual::HtmlRevealKind;
    let mut stack: Vec<(String, usize, Range<usize>)> = Vec::new();
    let mut pairs = Vec::new();
    let mut solos = Vec::new();
    let mut last_end = 0usize;
    for (i, inline) in inlines.iter().enumerate() {
        last_end = last_end.max(inline.source_range().end);
        let Inline::OpaqueInline {
            raw, source_range, ..
        } = inline
        else {
            continue;
        };
        match markrust_core::html_visual::html_reveal_kind(raw) {
            HtmlRevealKind::Open(name) => stack.push((name, i, source_range.clone())),
            HtmlRevealKind::Close(name) => {
                let mut matched = false;
                while let Some((open_name, open_i, open_r)) = stack.pop() {
                    if open_name == name {
                        pairs.push(HtmlPaintPair {
                            span: open_r.start..source_range.end,
                            open: open_r,
                            close: source_range.clone(),
                            open_i,
                            close_i: Some(i),
                        });
                        matched = true;
                        break;
                    }
                    pairs.push(HtmlPaintPair {
                        span: open_r.start..source_range.start.max(open_r.end),
                        open: open_r,
                        close: source_range.start..source_range.start,
                        open_i,
                        close_i: None,
                    });
                }
                if !matched {
                    solos.push(HtmlSoloChrome {
                        range: source_range.clone(),
                        inline_i: i,
                    });
                }
            }
            HtmlRevealKind::Solo => solos.push(HtmlSoloChrome {
                range: source_range.clone(),
                inline_i: i,
            }),
            HtmlRevealKind::Skip => {}
        }
    }
    for (_name, open_i, open_r) in stack {
        pairs.push(HtmlPaintPair {
            span: open_r.start..last_end.max(open_r.end),
            open: open_r,
            close: last_end..last_end,
            open_i,
            close_i: None,
        });
    }
    HtmlPaintGroups { pairs, solos }
}

fn html_chrome_range_for(
    groups: &HtmlPaintGroups,
    inline_i: usize,
    reveal: &RevealState,
) -> Option<Range<usize>> {
    for pair in &groups.pairs {
        if !reveal.intersects(&pair.span) {
            continue;
        }
        if pair.open_i == inline_i && pair.open.start < pair.open.end {
            return Some(pair.open.clone());
        }
        if pair.close_i == Some(inline_i) && pair.close.start < pair.close.end {
            return Some(pair.close.clone());
        }
    }
    for solo in &groups.solos {
        if solo.inline_i == inline_i && reveal.intersects(&solo.range) {
            return Some(solo.range.clone());
        }
    }
    None
}

fn html_inner_revealed(
    groups: &HtmlPaintGroups,
    inner: &Range<usize>,
    reveal: &RevealState,
) -> bool {
    let innermost = groups
        .pairs
        .iter()
        .filter(|pair| inner.start >= pair.span.start && inner.end <= pair.span.end)
        .min_by_key(|pair| pair.span.end.saturating_sub(pair.span.start));
    innermost.is_some_and(|pair| reveal.intersects(&pair.span))
}

fn first_inline_source_start(block: &Block) -> Option<usize> {
    block.inlines.iter().map(|i| i.source_range().start).min()
}

fn last_inline_source_end(block: &Block) -> Option<usize> {
    block.inlines.iter().map(|i| i.source_range().end).max()
}

/// Opening ATX hashes plus the spaces before the title (`# Title`).
fn atx_open_range(source: &str, block: &Block) -> Option<Range<usize>> {
    let BlockKind::Heading {
        style: HeadingStyle::Atx,
        ..
    } = block.kind
    else {
        return None;
    };
    let lo = block.source_range.start;
    let hi = block.source_range.end.min(source.len());
    let bytes = source.as_bytes();
    let content = first_inline_source_start(block).unwrap_or_else(|| {
        let mut i = lo;
        while i < hi && matches!(bytes[i], b'>' | b' ' | b'\t') {
            i += 1;
        }
        i
    });
    if content < lo || content > hi {
        return None;
    }
    let mut i = content.min(hi);
    while i > lo && bytes[i - 1] == b' ' {
        i -= 1;
    }
    if i == 0 || bytes.get(i - 1) != Some(&b'#') {
        return None;
    }
    while i > lo && bytes[i - 1] == b'#' {
        i -= 1;
    }
    while i > lo && bytes[i - 1] == b' ' {
        i -= 1;
    }
    if i >= content {
        return None;
    }
    Some(i..content)
}

/// Closed ATX trailing ` #` after the title.
fn atx_close_range(source: &str, block: &Block) -> Option<Range<usize>> {
    let BlockKind::Heading {
        style: HeadingStyle::Atx,
        ..
    } = block.kind
    else {
        return None;
    };
    let hi = block.source_range.end.min(source.len());
    let last = last_inline_source_end(block)?;
    if last >= hi {
        return None;
    }
    let bytes = source.as_bytes();
    let mut i = last;
    while i < hi && bytes[i] == b' ' {
        i += 1;
    }
    if i >= hi || bytes[i] != b'#' {
        return None;
    }
    while i < hi && bytes[i] == b'#' {
        i += 1;
    }
    if i <= last {
        return None;
    }
    Some(last..i)
}

fn prepend_source_slice(layout: &mut LeafLayout, slice: &str, src: Range<usize>, mut run: TextRun) {
    if slice.is_empty() {
        return;
    }
    run.len = slice.len();
    let mut source_at = Vec::with_capacity(slice.len() + layout.source_at.len());
    for i in 0..slice.len() {
        source_at.push(src.start + i);
    }
    source_at.extend_from_slice(&layout.source_at);
    layout.source_at = source_at;
    layout.text.insert_str(0, slice);
    layout.runs.insert(0, run);
}

fn append_source_slice(layout: &mut LeafLayout, slice: &str, src: Range<usize>, mut run: TextRun) {
    if slice.is_empty() {
        return;
    }
    run.len = slice.len();
    if layout.source_at.len() == layout.text.len() + 1 {
        layout.source_at.pop();
    }
    for i in 0..slice.len() {
        layout.source_at.push(src.start + i);
    }
    layout.source_at.push(src.end);
    layout.text.push_str(slice);
    layout.runs.push(run);
}

fn paint_atx_chrome(
    block: &Block,
    source: &str,
    reveal: &RevealState,
    text_style: &TextStyle,
    theme: &EditorTheme,
    layout: &mut LeafLayout,
) {
    if source.is_empty()
        || !matches!(
            block.kind,
            BlockKind::Heading {
                style: HeadingStyle::Atx,
                ..
            }
        )
        || !reveal.intersects(&block.source_range)
    {
        return;
    }
    let delim = wiki_delim_run(text_style, theme);
    if let Some(prefix) = atx_open_range(source, block) {
        if let Some(slice) = source.get(prefix.clone()) {
            prepend_source_slice(layout, slice, prefix, delim.clone());
        }
    }
    if let Some(suffix) = atx_close_range(source, block) {
        if let Some(slice) = source.get(suffix.clone()) {
            append_source_slice(layout, slice, suffix, delim);
        }
    }
}

/// Setext underline (`===` / `---`) after the title, without quote `>`.
fn setext_underline_range(source: &str, block: &Block) -> Option<Range<usize>> {
    let BlockKind::Heading {
        style: HeadingStyle::Setext,
        ..
    } = block.kind
    else {
        return None;
    };
    let hi = block.source_range.end.min(source.len());
    let last = last_inline_source_end(block).unwrap_or(block.source_range.start);
    if last >= hi {
        return None;
    }
    let bytes = source.as_bytes();
    let mut i = last;
    while i < hi && matches!(bytes[i], b' ' | b'\t' | b'\r') {
        i += 1;
    }
    if i < hi && bytes[i] == b'\n' {
        i += 1;
    }
    while i < hi && matches!(bytes[i], b'>' | b' ' | b'\t') {
        i += 1;
    }
    let start = i;
    while i < hi && matches!(bytes[i], b'=' | b'-') {
        i += 1;
    }
    if i <= start {
        return None;
    }
    Some(start..i)
}

/// 0–3 spaces before a setext title (` Title` / `==`). Hide/show with the
/// underline the same way ATX indent hides with `#`.
fn setext_title_indent_range(source: &str, block: &Block) -> Option<Range<usize>> {
    let BlockKind::Heading {
        style: HeadingStyle::Setext,
        ..
    } = block.kind
    else {
        return None;
    };
    let lo = block.source_range.start;
    let content = first_inline_source_start(block)?;
    if content <= lo {
        return None;
    }
    let bytes = source.as_bytes();
    let mut i = content.min(source.len());
    while i > lo && bytes[i - 1] == b' ' {
        i -= 1;
    }
    (i < content).then_some(i..content)
}

fn paint_setext_chrome(
    block: &Block,
    source: &str,
    reveal: &RevealState,
    text_style: &TextStyle,
    theme: &EditorTheme,
    layout: &mut LeafLayout,
) {
    if source.is_empty()
        || !matches!(
            block.kind,
            BlockKind::Heading {
                style: HeadingStyle::Setext,
                ..
            }
        )
        || !reveal.intersects(&block.source_range)
    {
        return;
    }
    let Some(under) = setext_underline_range(source, block) else {
        return;
    };
    let Some(slice) = source.get(under.clone()) else {
        return;
    };
    let delim = wiki_delim_run(text_style, theme);
    if let Some(indent) = setext_title_indent_range(source, block) {
        if let Some(spaces) = source.get(indent.clone()) {
            prepend_source_slice(layout, spaces, indent, delim.clone());
        }
    }
    let nl = "\n";
    let nl_at = under.start.saturating_sub(1);
    if source.as_bytes().get(nl_at) == Some(&b'\n') {
        append_source_slice(layout, nl, nl_at..nl_at + 1, delim.clone());
    }
    append_source_slice(layout, slice, under, delim);
}

fn prefix_at(block: &Block) -> usize {
    first_inline_source_start(block).unwrap_or(block.source_range.start)
}

fn revealed_prefix_range(
    source: &str,
    at: usize,
    reveal: &RevealState,
    hosts: &ChromeHosts,
) -> Option<Range<usize>> {
    if source.is_empty() {
        return None;
    }
    let parts = line_prefix_parts(source, at);
    let show_list = parts.list.start < parts.list.end
        && hosts
            .list_item
            .as_ref()
            .is_some_and(|h| reveal.intersects(h));
    let show_fn = parts.footnote.start < parts.footnote.end
        && hosts
            .footnote_def
            .as_ref()
            .is_some_and(|h| reveal.intersects(h) || reveal.intersects(&parts.footnote));
    let show_details = parts.details.start < parts.details.end
        && hosts
            .definition_details
            .as_ref()
            .is_some_and(|h| reveal.intersects(h) || reveal.intersects(&parts.details));
    let show_quote = parts.quote.start < parts.quote.end
        && (hosts.quote.as_ref().is_some_and(|h| reveal.intersects(h)) || show_fn || show_details);
    union_shown_prefix([
        (show_quote, parts.quote),
        (show_list, parts.list),
        (show_fn, parts.footnote),
        (show_details, parts.details),
    ])
}

fn union_shown_prefix<const N: usize>(parts: [(bool, Range<usize>); N]) -> Option<Range<usize>> {
    let mut start = None;
    let mut end = None;
    for (show, range) in parts {
        if show && range.start < range.end {
            start = Some(start.map_or(range.start, |s: usize| s.min(range.start)));
            end = Some(end.map_or(range.end, |e: usize| e.max(range.end)));
        }
    }
    Some(start?..end?)
}

/// True when intersect-reveal will paint a definition-details `: ` on this line.
pub(crate) fn prefix_range_shows_details(
    source: &str,
    at: usize,
    reveal: &RevealState,
    hosts: &ChromeHosts,
) -> bool {
    let parts = line_prefix_parts(source, at);
    parts.details.start < parts.details.end
        && hosts
            .definition_details
            .as_ref()
            .is_some_and(|h| reveal.intersects(h) || reveal.intersects(&parts.details))
}

/// True when intersect-reveal will paint a footnote-def `[^1]:` on this line.
pub(crate) fn prefix_range_shows_footnote(
    source: &str,
    at: usize,
    reveal: &RevealState,
    hosts: &ChromeHosts,
) -> bool {
    let parts = line_prefix_parts(source, at);
    parts.footnote.start < parts.footnote.end
        && hosts
            .footnote_def
            .as_ref()
            .is_some_and(|h| reveal.intersects(h) || reveal.intersects(&parts.footnote))
}

fn paint_prefix_chrome(
    block: &Block,
    source: &str,
    reveal: &RevealState,
    hosts: &ChromeHosts,
    text_style: &TextStyle,
    theme: &EditorTheme,
    layout: &mut LeafLayout,
) {
    if source.is_empty()
        || matches!(
            block.kind,
            BlockKind::TableCell | BlockKind::CodeBlock { .. }
        )
    {
        return;
    }
    let Some(range) = revealed_prefix_range(source, prefix_at(block), reveal, hosts) else {
        return;
    };
    let Some(slice) = source.get(range.clone()) else {
        return;
    };
    prepend_source_slice(layout, slice, range, wiki_delim_run(text_style, theme));
}

fn fence_open_range(source: &str, block: &Block) -> Option<Range<usize>> {
    let BlockKind::CodeBlock { fence: Some(_), .. } = &block.kind else {
        return None;
    };
    let body = block.code_body_range(source);
    let start = block.source_range.start.min(source.len());
    if body.start <= start {
        return None;
    }
    Some(start..body.start)
}

fn fence_close_range(source: &str, block: &Block) -> Option<Range<usize>> {
    let BlockKind::CodeBlock { fence: Some(_), .. } = &block.kind else {
        return None;
    };
    let body = block.code_body_range(source);
    let end = block.source_range.end.min(source.len());
    if body.end >= end {
        return None;
    }
    let slice = source.get(body.end..end)?;
    let trimmed = slice.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return None;
    }
    Some(body.end..body.end + trimmed.len())
}

fn paint_fence_chrome(
    block: &Block,
    source: &str,
    reveal: &RevealState,
    text_style: &TextStyle,
    theme: &EditorTheme,
    layout: &mut LeafLayout,
) {
    if source.is_empty() || !reveal.intersects(&block.source_range) {
        return;
    }
    let delim = wiki_delim_run(text_style, theme);
    if let Some(open) = fence_open_range(source, block) {
        if let Some(slice) = source.get(open.clone()) {
            prepend_source_slice(layout, slice, open, delim.clone());
        }
    }
    if let Some(close) = fence_close_range(source, block) {
        if let Some(slice) = source.get(close.clone()) {
            append_source_slice(layout, slice, close, delim);
        }
    }
}

fn cell_inner_range(source: &str, cell: &Block) -> Range<usize> {
    let bytes = source.as_bytes();
    let mut start = cell.source_range.start.min(source.len());
    let mut end = cell.source_range.end.min(source.len());
    while start < end && bytes[start] == b'|' {
        start += 1;
    }
    while end > start && bytes[end - 1] == b'|' {
        end -= 1;
    }
    start..end
}

fn cell_leading_pipe(source: &str, inner_start: usize) -> Option<Range<usize>> {
    let bytes = source.as_bytes();
    let mut i = inner_start.min(source.len());
    while i > 0 && matches!(bytes[i - 1], b' ' | b'\t') {
        i -= 1;
    }
    if i == 0 || bytes[i - 1] != b'|' {
        return None;
    }
    Some(i - 1..i)
}

fn cell_trailing_pipe(source: &str, inner_end: usize) -> Option<Range<usize>> {
    let bytes = source.as_bytes();
    let mut j = inner_end.min(source.len());
    while j < source.len() && matches!(bytes[j], b' ' | b'\t') {
        j += 1;
    }
    if j >= source.len() || bytes[j] != b'|' {
        return None;
    }
    let after = j + 1;
    let last = after >= source.len() || matches!(bytes[after], b'\n' | b'\r');
    last.then_some(j..j + 1)
}

fn paint_table_pipes(
    block: &Block,
    source: &str,
    reveal: &RevealState,
    hosts: &ChromeHosts,
    text_style: &TextStyle,
    theme: &EditorTheme,
    layout: &mut LeafLayout,
) {
    if !matches!(block.kind, BlockKind::TableCell) || source.is_empty() {
        return;
    }
    let Some(table) = hosts.table.as_ref() else {
        return;
    };
    if !reveal.intersects(table) {
        return;
    }
    let inner = cell_inner_range(source, block);
    let delim = wiki_delim_run(text_style, theme);
    if let Some(trail) = cell_trailing_pipe(source, inner.end) {
        if let Some(slice) = source.get(trail.clone()) {
            append_source_slice(layout, slice, trail, delim.clone());
        }
    }
    if let Some(lead) = cell_leading_pipe(source, inner.start) {
        if let Some(slice) = source.get(lead.clone()) {
            prepend_source_slice(layout, slice, lead, delim);
        }
    }
}

/// Alignment row (`|---|---|`) inside a GFM table, without a leading quote
/// prefix. `None` when the table has no delimiter line.
pub fn table_alignment_line(source: &str, table: &Block) -> Option<Range<usize>> {
    if !matches!(table.kind, BlockKind::Table { .. }) {
        return None;
    }
    let start = table.source_range.start.min(source.len());
    let end = table.source_range.end.min(source.len());
    let slice = source.get(start..end)?;
    let mut offset = start;
    for line in slice.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let parts = line_prefix_parts(source, offset);
        let quote_len = parts.quote.end.saturating_sub(parts.quote.start);
        let body = trimmed.get(quote_len..).unwrap_or("");
        let t = body.trim();
        let is_delim = t.contains('|')
            && t.contains('-')
            && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'));
        if is_delim && !t.is_empty() {
            return Some(offset + quote_len..offset + trimmed.len());
        }
        offset += line.len();
    }
    None
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
    } else if let Some(bg) = paint.background {
        run.background_color = Some(css_hsla(bg));
    } else if paint.mark || marks.contains(MarkSet::HIGHLIGHT) {
        run.background_color = Some(theme.accent.opacity(0.22));
    }
    if paint.underline {
        run.underline = Some(gpui::UnderlineStyle {
            thickness: px(1.),
            color: Some(paint.color.map(css_hsla).unwrap_or(theme.text)),
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
    if let Some(c) = paint.color {
        run.color = css_hsla(c);
        if run.underline.is_some() && !(md_link || paint.href.is_some()) {
            run.underline = Some(gpui::UnderlineStyle {
                thickness: px(1.),
                color: Some(css_hsla(c)),
                wavy: false,
            });
        }
    }
    run
}

fn css_hsla(c: markrust_core::html_visual::CssColor) -> gpui::Hsla {
    gpui::Hsla::from(rgb(c.to_u32()))
}

fn html_css_is_mixed(paints: &[markrust_core::html_visual::HtmlPaintRun]) -> bool {
    let Some(first) = paints.first() else {
        return false;
    };
    let key = (first.paint.color, first.paint.background);
    paints
        .iter()
        .any(|p| (p.paint.color, p.paint.background) != key)
}

fn tint_layout_with_html_css(
    layout: &mut LeafLayout,
    paints: &[markrust_core::html_visual::HtmlPaintRun],
) {
    let Some(first) = paints.first() else {
        return;
    };
    let color = first.paint.color;
    let background = first.paint.background;
    if color.is_none() && background.is_none() {
        return;
    }
    for run in &mut layout.runs {
        if let Some(c) = color {
            run.color = css_hsla(c);
        }
        if let Some(c) = background {
            run.background_color = Some(css_hsla(c));
        }
    }
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

/// Layout for a projected HTML block (tags stripped unless `reveal`
/// intersects). `source_at` is relative to the HTML literal; quote/list
/// prefixes in `source` are skipped via [`code_body_source_map`] so a click
/// on painted body text does not land on `>`. Inner Markdown (`**bold**`,
/// links, code) is parsed so it does not paint as source chrome; HTML
/// phrasing (`<mark>`, `<sub>`, …) is merged as marks. Wrapper `<div>` /
/// `</div>` paint when [`RevealState`] intersects the block.
#[allow(clippy::too_many_arguments)]
pub fn build_html_block_layout(
    text: &str,
    source_at: &[usize],
    paints: &[markrust_core::html_visual::HtmlPaintRun],
    source: &str,
    block: &Block,
    text_style: &TextStyle,
    theme: &EditorTheme,
    reveal: &RevealState,
) -> LeafLayout {
    let literal_map = html_literal_source_map(source, block);
    let block_start = *literal_map.first().unwrap_or(&block.source_range.start);
    let raw = match &block.kind {
        BlockKind::Opaque { raw } => raw.as_str(),
        _ => "",
    };
    let mut inlines = if html_block_is_preformatted(raw) {
        if text.is_empty() {
            Vec::new()
        } else {
            vec![Inline::Run {
                text: text.to_string(),
                raw: None,
                source_range: 0..text.len(),
                marks: MarkSet::empty(),
                link: None,
                fidelity: markrust_core::rich::MarkFidelity::default(),
            }]
        }
    } else {
        inlines_from_inner_markdown(text)
    };
    let markdown_visible = if inlines.is_empty() {
        false
    } else {
        let probe = build_leaf_layout_inlines(
            &inlines,
            0,
            inlines.len(),
            0..text.len(),
            text,
            text_style,
            theme,
            gpui::FontWeight::NORMAL,
            &RevealState::HIDDEN,
            &ChromeHosts::NONE,
            None,
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
    if html_css_is_mixed(paints) {
        let mut layout = html_flow_layout(
            text,
            source_at,
            paints,
            &literal_map,
            block_start,
            text_style,
            theme,
        );
        paint_html_block_wrapper_tags(&mut layout, source, block, text_style, theme, reveal);
        return layout;
    }
    let mut layout = build_leaf_layout_inlines(
        &inlines,
        0,
        inlines.len(),
        0..text.len(),
        text,
        text_style,
        theme,
        gpui::FontWeight::NORMAL,
        &RevealState::HIDDEN,
        &ChromeHosts::NONE,
        None,
    );
    if layout.text.is_empty() && !text.is_empty() {
        let mut layout = html_flow_layout(
            text,
            source_at,
            paints,
            &literal_map,
            block_start,
            text_style,
            theme,
        );
        paint_html_block_wrapper_tags(&mut layout, source, block, text_style, theme, reveal);
        return layout;
    }
    remap_html_sources(
        &mut layout,
        source_at,
        &literal_map,
        block_start,
        text.len(),
    );
    tint_layout_with_html_css(&mut layout, paints);
    paint_html_block_wrapper_tags(&mut layout, source, block, text_style, theme, reveal);
    layout
}

/// Layout for any HTML-block visual, including Hidden comments / empty
/// wrappers that reveal source when [`RevealState`] intersects.
pub fn layout_html_block(
    source: &str,
    block: &Block,
    text_style: &TextStyle,
    theme: &EditorTheme,
    reveal: &RevealState,
) -> LeafLayout {
    let BlockKind::Opaque { raw } = &block.kind else {
        return LeafLayout {
            text: String::new(),
            runs: Vec::new(),
            source_at: vec![block.source_range.start],
            block_start: block.source_range.start,
        };
    };
    match project_html_block(raw) {
        HtmlBlockVisual::Flow {
            text,
            source_at,
            runs,
        } => build_html_block_layout(
            &text, &source_at, &runs, source, block, text_style, theme, reveal,
        ),
        HtmlBlockVisual::Hidden => {
            build_hidden_html_block_layout(source, block, raw, text_style, theme, reveal)
        }
        HtmlBlockVisual::ThematicBreak => build_thematic_break_layout(
            block,
            source,
            text_style,
            theme,
            reveal,
            &ChromeHosts::NONE,
        ),
        HtmlBlockVisual::Image { .. } => LeafLayout {
            text: String::new(),
            runs: Vec::new(),
            source_at: vec![block.source_range.start],
            block_start: block.source_range.start,
        },
    }
}

fn build_hidden_html_block_layout(
    source: &str,
    block: &Block,
    raw: &str,
    text_style: &TextStyle,
    theme: &EditorTheme,
    reveal: &RevealState,
) -> LeafLayout {
    let start = block.source_range.start;
    let mut layout = LeafLayout {
        text: String::new(),
        runs: Vec::new(),
        source_at: vec![start],
        block_start: start,
    };
    if !reveal.intersects(&block.source_range) {
        return layout;
    }
    if html_block_is_source_chrome(raw) {
        let literal_map = html_literal_source_map(source, block);
        let source_at: Vec<usize> = (0..=raw.len()).collect();
        let runs = [HtmlPaintRun {
            len: raw.len(),
            paint: markrust_core::html_visual::HtmlPaint::default(),
        }];
        layout = html_flow_layout(
            raw,
            &source_at,
            &runs,
            &literal_map,
            start,
            text_style,
            theme,
        );
        let mut run = wiki_delim_run(text_style, theme);
        run.len = layout.text.len().max(1);
        layout.runs = vec![run];
        return layout;
    }
    if html_block_is_dangerous(raw) {
        return layout;
    }
    paint_html_block_wrapper_tags(&mut layout, source, block, text_style, theme, reveal);
    layout
}

fn paint_html_block_wrapper_tags(
    layout: &mut LeafLayout,
    source: &str,
    block: &Block,
    text_style: &TextStyle,
    theme: &EditorTheme,
    reveal: &RevealState,
) {
    if !reveal.intersects(&block.source_range) {
        return;
    }
    let BlockKind::Opaque { raw } = &block.kind else {
        return;
    };
    let Some((open, close)) = html_block_wrapper_tag_ranges(raw) else {
        return;
    };
    let literal_map = html_literal_source_map(source, block);
    let fallback = block.source_range.start;
    let delim = wiki_delim_run(text_style, theme);
    if let Some(slice) = raw.get(open.clone()).filter(|s| !s.is_empty()) {
        let start = doc_offset_for_html_literal(&literal_map, open.start, fallback);
        prepend_source_slice(layout, slice, start..start + slice.len(), delim.clone());
    }
    if let Some(close) = close {
        if let Some(slice) = raw.get(close.clone()).filter(|s| !s.is_empty()) {
            let start = doc_offset_for_html_literal(&literal_map, close.start, fallback);
            append_source_slice(layout, slice, start..start + slice.len(), delim);
        }
    }
}

fn html_literal_source_map(source: &str, block: &Block) -> Vec<usize> {
    let painted_len = match &block.kind {
        markrust_core::rich::BlockKind::Opaque { raw } => raw.len(),
        _ => block
            .source_range
            .end
            .saturating_sub(block.source_range.start),
    };
    html_block_literal_source_map(source, block, painted_len)
}

fn doc_offset_for_html_literal(literal_map: &[usize], html: usize, fallback: usize) -> usize {
    literal_map
        .get(html)
        .copied()
        .or_else(|| literal_map.last().copied())
        .unwrap_or(fallback)
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
        angle: false,
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

pub fn block_paints_as_thematic_break(block: &Block) -> bool {
    match &block.kind {
        BlockKind::ThematicBreak => true,
        BlockKind::Opaque { raw } => matches!(
            project_html_block(raw.trim()),
            HtmlBlockVisual::ThematicBreak
        ),
        _ => false,
    }
}

/// Marker bytes (`---` / `***` / `<hr>`) without a trailing newline or
/// quote/list prefix. Markdown `* * *` / `- - -` keep every dash/star —
/// those are dest chrome, not a list marker.
fn thematic_break_marker_range(source: &str, block: &Block) -> Range<usize> {
    let mut start = block.source_range.start.min(source.len());
    let mut end = block.source_range.end.min(source.len());
    let bytes = source.as_bytes();
    while end > start && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    let parts = line_prefix_parts(source, start);
    let prefix_end = if matches!(block.kind, BlockKind::Opaque { .. }) {
        parts.list.end
    } else {
        parts.quote.end
    };
    if prefix_end > start && prefix_end <= end {
        start = prefix_end;
    }
    start..end
}

pub fn build_thematic_break_layout(
    block: &Block,
    source: &str,
    text_style: &TextStyle,
    theme: &EditorTheme,
    reveal: &RevealState,
    hosts: &ChromeHosts,
) -> LeafLayout {
    let marker = thematic_break_marker_range(source, block);
    let start = marker.start;
    let mut layout = LeafLayout {
        text: String::new(),
        runs: Vec::new(),
        source_at: vec![start],
        block_start: start,
    };
    if reveal.intersects(&block.source_range) {
        if let Some(slice) = source.get(marker.clone()).filter(|s| !s.is_empty()) {
            append_source_slice(
                &mut layout,
                slice,
                marker,
                wiki_delim_run(text_style, theme),
            );
        }
    }
    apply_structural_chrome(&mut layout, block, source, reveal, hosts, text_style, theme);
    layout
}

pub fn build_leaf_layout_revealed(
    block: &Block,
    source: &str,
    text_style: &TextStyle,
    theme: &EditorTheme,
    base_weight: gpui::FontWeight,
    reveal: &RevealState,
    hosts: &ChromeHosts,
) -> LeafLayout {
    if block_paints_as_thematic_break(block) {
        return build_thematic_break_layout(block, source, text_style, theme, reveal, hosts);
    }
    let def_chrome = link_reference_def_chrome(source, block);
    let mut layout = build_leaf_layout_inlines(
        &block.inlines,
        0,
        block.inlines.len(),
        block.source_range.clone(),
        source,
        text_style,
        theme,
        base_weight,
        reveal,
        hosts,
        def_chrome.as_ref(),
    );
    paint_atx_chrome(block, source, reveal, text_style, theme, &mut layout);
    paint_setext_chrome(block, source, reveal, text_style, theme, &mut layout);
    apply_structural_chrome(&mut layout, block, source, reveal, hosts, text_style, theme);
    layout
}

pub fn apply_structural_chrome(
    layout: &mut LeafLayout,
    block: &Block,
    source: &str,
    reveal: &RevealState,
    hosts: &ChromeHosts,
    text_style: &TextStyle,
    theme: &EditorTheme,
) {
    paint_prefix_chrome(block, source, reveal, hosts, text_style, theme, layout);
    paint_table_pipes(block, source, reveal, hosts, text_style, theme, layout);
    paint_fence_chrome(block, source, reveal, text_style, theme, layout);
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
/// renderer paints validated cache files as `img()` elements (local, `data:`,
/// and cached remote images) instead of alt placeholders.
/// `paint_start..paint_end` selects which
/// inlines to emit; wrap groups are collected from the whole `inlines` slice
/// so mixed text+image paragraphs still reveal one `**` span.
#[allow(clippy::too_many_arguments)]
pub fn build_leaf_layout_inlines(
    inlines: &[Inline],
    paint_start: usize,
    paint_end: usize,
    block_range: Range<usize>,
    source: &str,
    text_style: &TextStyle,
    theme: &EditorTheme,
    base_weight: gpui::FontWeight,
    reveal: &RevealState,
    hosts: &ChromeHosts,
    def_chrome: Option<&markrust_core::rich::LinkReferenceDefChrome>,
) -> LeafLayout {
    let wrap_groups = collect_wrap_groups(source, inlines, &block_range);
    let link_groups = collect_link_groups(source, inlines, &block_range);
    let html_groups = collect_html_groups(inlines);
    let tagfilter_widgets = tagfilter_widget_ranges_in(inlines);
    let mut revealed_tagfilter = HashSet::new();
    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    let mut source_at = Vec::new();
    let mut paint_end_src = block_range.end;
    {
        let mut push = |text: &mut String,
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
            paint_end_src = src.end;
        };

        let mut html = markrust_core::html_visual::HtmlStack::default();

        let paint_lo = paint_start.min(inlines.len());
        let paint_hi = paint_end.min(inlines.len()).max(paint_lo);
        for (inline_i, inline) in inlines.iter().enumerate().take(paint_hi).skip(paint_lo) {
            let span = inline.source_range();
            if let Some(widget) = tagfilter_widgets
                .iter()
                .find(|w| span.start >= w.start && span.start < w.end)
            {
                if let Inline::OpaqueInline { raw, .. } = inline {
                    let _ = classify_opaque_inline(raw, &mut html);
                }
                if revealed_tagfilter.insert(widget.start) {
                    if let Some(raw) = source.get(widget.clone()) {
                        if html_block_is_source_chrome(raw)
                            && !html_block_is_hidden_widget(raw)
                            && reveal.intersects(widget)
                        {
                            push(
                                &mut text,
                                &mut runs,
                                &mut source_at,
                                raw,
                                widget.clone(),
                                wiki_delim_run(text_style, theme),
                            );
                        }
                    }
                }
                continue;
            }
            match inline {
                Inline::Run {
                    text: t,
                    raw,
                    marks,
                    link,
                    source_range,
                    ..
                } => {
                    if html.hidden() {
                        continue;
                    }
                    let paint = merge_html_paint(*marks, &html.paint());
                    let entity_slice = raw.as_deref().or_else(|| source.get(source_range.clone()));
                    let is_entity = !marks.contains(MarkSet::CODE)
                        && entity_slice.is_some_and(|s| {
                            is_decoded_character_reference(s, t)
                                || is_decoded_backslash_escape(s, t)
                        });
                    let reveal_entity = is_entity && reveal.intersects(source_range);
                    let (wrap_opens, wrap_closes) =
                        revealed_wrap_lists(&wrap_groups, inlines, inline_i, reveal);
                    let code_vis = marks
                        .contains(MarkSet::CODE)
                        .then(|| code_span_visible_range(source, source_range.clone(), t));
                    let reveal_code_pad = code_vis.as_ref().is_some_and(|vis| {
                        vis != source_range
                            && (reveal.intersects(source_range)
                                || !wrap_opens.is_empty()
                                || !wrap_closes.is_empty())
                    });
                    let visible = if reveal_entity {
                        entity_slice.unwrap_or(t).to_string()
                    } else if reveal_code_pad {
                        source.get(source_range.clone()).unwrap_or(t).to_string()
                    } else if html_inner_revealed(&html_groups, source_range, reveal) {
                        t.clone()
                    } else {
                        visible_for_html(t, &paint)
                    };
                    let paint_src = if reveal_code_pad {
                        source_range.clone()
                    } else {
                        code_vis.clone().unwrap_or_else(|| source_range.clone())
                    };
                    let run = style_run(
                        text_style,
                        theme,
                        base_weight,
                        *marks,
                        link.is_some(),
                        &paint,
                    );
                    let angle = link
                        .as_ref()
                        .and_then(|l| l.angle_span(source_range))
                        .filter(|outer| reveal.intersects(outer));
                    let (link_opens, link_closes) =
                        revealed_link_sides(&link_groups, inlines, inline_i, reveal);
                    let mut opens = wrap_opens;
                    opens.extend(link_opens);
                    opens.sort_by_key(|r| r.start);
                    let mut closes = wrap_closes;
                    closes.extend(link_closes);
                    closes.sort_by_key(|r| r.start);
                    let delim = wiki_delim_run(text_style, theme);
                    let def_label = def_chrome.filter(|c| c.label == *source_range);
                    if let Some(chrome) = def_label {
                        if reveal.intersects(&chrome.outer) {
                            if let Some(slice) = source.get(chrome.open.clone()) {
                                push(
                                    &mut text,
                                    &mut runs,
                                    &mut source_at,
                                    slice,
                                    chrome.open.clone(),
                                    delim.clone(),
                                );
                            }
                        }
                    }
                    for open in &opens {
                        if let Some(slice) = source.get(open.clone()) {
                            push(
                                &mut text,
                                &mut runs,
                                &mut source_at,
                                slice,
                                open.clone(),
                                delim.clone(),
                            );
                        }
                    }
                    if let Some(outer) = angle {
                        push(
                            &mut text,
                            &mut runs,
                            &mut source_at,
                            "<",
                            outer.start..outer.start + 1,
                            delim.clone(),
                        );
                        push(
                            &mut text,
                            &mut runs,
                            &mut source_at,
                            &visible,
                            paint_src.clone(),
                            run,
                        );
                        push(
                            &mut text,
                            &mut runs,
                            &mut source_at,
                            ">",
                            outer.end.saturating_sub(1)..outer.end,
                            delim.clone(),
                        );
                    } else {
                        push(
                            &mut text,
                            &mut runs,
                            &mut source_at,
                            &visible,
                            paint_src,
                            run,
                        );
                    }
                    for close in &closes {
                        if let Some(slice) = source.get(close.clone()) {
                            push(
                                &mut text,
                                &mut runs,
                                &mut source_at,
                                slice,
                                close.clone(),
                                delim.clone(),
                            );
                        }
                    }
                    if let Some(chrome) = def_label {
                        if reveal.intersects(&chrome.outer) {
                            if let Some(slice) = source.get(chrome.close.clone()) {
                                push(
                                    &mut text,
                                    &mut runs,
                                    &mut source_at,
                                    slice,
                                    chrome.close.clone(),
                                    delim.clone(),
                                );
                            }
                        }
                        if let Some(slice) = source.get(chrome.colon.clone()) {
                            if !slice.is_empty() {
                                push(
                                    &mut text,
                                    &mut runs,
                                    &mut source_at,
                                    slice,
                                    chrome.colon.clone(),
                                    delim,
                                );
                            }
                        }
                    }
                }
                Inline::Image {
                    alt,
                    source_range,
                    marks,
                    ..
                } => {
                    if html.hidden() {
                        continue;
                    }
                    let (wrap_opens, wrap_closes) =
                        revealed_wrap_lists(&wrap_groups, inlines, inline_i, reveal);
                    let (link_opens, link_closes) =
                        revealed_link_sides(&link_groups, inlines, inline_i, reveal);
                    if wrap_opens.is_empty()
                        && wrap_closes.is_empty()
                        && link_opens.is_empty()
                        && link_closes.is_empty()
                        && !reveal.intersects(source_range)
                    {
                        continue;
                    }
                    let paint = merge_html_paint(*marks, &html.paint());
                    let alt_run = style_run(text_style, theme, base_weight, *marks, true, &paint);
                    let delim = wiki_delim_run(text_style, theme);
                    let mut opens = wrap_opens;
                    opens.extend(link_opens);
                    opens.sort_by_key(|r| r.start);
                    let mut closes = wrap_closes;
                    closes.extend(link_closes);
                    closes.sort_by_key(|r| r.start);
                    for open in &opens {
                        if let Some(slice) = source.get(open.clone()) {
                            push(
                                &mut text,
                                &mut runs,
                                &mut source_at,
                                slice,
                                open.clone(),
                                delim.clone(),
                            );
                        }
                    }
                    if reveal.intersects(source_range) {
                        let inner = link_groups
                            .get(&LinkPaintKey::Image(inline_i))
                            .map(|g| g.chrome.open.end..g.chrome.close.start)
                            .filter(|r| r.start < r.end)
                            .unwrap_or_else(|| source_range.clone());
                        let visible = source
                            .get(inner.clone())
                            .filter(|s| !s.is_empty())
                            .unwrap_or(alt.as_str());
                        if !visible.is_empty() {
                            push(
                                &mut text,
                                &mut runs,
                                &mut source_at,
                                visible,
                                inner,
                                alt_run,
                            );
                        }
                    }
                    for close in &closes {
                        if let Some(slice) = source.get(close.clone()) {
                            push(
                                &mut text,
                                &mut runs,
                                &mut source_at,
                                slice,
                                close.clone(),
                                delim.clone(),
                            );
                        }
                    }
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
                    if let Some(prefix) =
                        revealed_prefix_range(source, source_range.end, reveal, hosts)
                    {
                        push(
                            &mut text,
                            &mut runs,
                            &mut source_at,
                            "\n",
                            paint_break_src(source_range),
                            run,
                        );
                        if let Some(slice) = source.get(prefix.clone()) {
                            push(
                                &mut text,
                                &mut runs,
                                &mut source_at,
                                slice,
                                prefix,
                                wiki_delim_run(text_style, theme),
                            );
                        }
                    } else {
                        push(
                            &mut text,
                            &mut runs,
                            &mut source_at,
                            " ",
                            paint_break_src(source_range),
                            run,
                        );
                    }
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
                    if let Some(prefix) =
                        revealed_prefix_range(source, source_range.end, reveal, hosts)
                    {
                        if let Some(slice) = source.get(prefix.clone()) {
                            push(
                                &mut text,
                                &mut runs,
                                &mut source_at,
                                slice,
                                prefix,
                                wiki_delim_run(text_style, theme),
                            );
                        }
                    }
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
                        let vis = markrust_core::rich::tree::math_visible_range(
                            source,
                            *display,
                            source_range.clone(),
                        );
                        let painted = source.get(vis.clone()).unwrap_or(literal.as_str());
                        push(
                            &mut text,
                            &mut runs,
                            &mut source_at,
                            painted,
                            vis,
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
                    let (link_opens, link_closes) =
                        revealed_link_sides(&link_groups, inlines, inline_i, reveal);
                    let delim = wiki_delim_run(text_style, theme);
                    for open in &link_opens {
                        if let Some(slice) = source.get(open.clone()) {
                            push(
                                &mut text,
                                &mut runs,
                                &mut source_at,
                                slice,
                                open.clone(),
                                delim.clone(),
                            );
                        }
                    }
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
                        let vis_range =
                            markrust_core::rich::emoji_visible_range(raw, source_range.clone());
                        push(&mut text, &mut runs, &mut source_at, glyph, vis_range, run);
                    }
                    for close in &link_closes {
                        if let Some(slice) = source.get(close.clone()) {
                            push(
                                &mut text,
                                &mut runs,
                                &mut source_at,
                                slice,
                                close.clone(),
                                delim.clone(),
                            );
                        }
                    }
                }
                Inline::OpaqueInline {
                    raw, source_range, ..
                } => {
                    let was_hidden = html.hidden();
                    match markrust_core::html_visual::classify_opaque_inline(raw, &mut html) {
                        markrust_core::html_visual::InlineHtmlAction::Hide => {
                            if !was_hidden {
                                let (wrap_opens, wrap_closes) =
                                    revealed_wrap_lists(&wrap_groups, inlines, inline_i, reveal);
                                let (link_opens, link_closes) =
                                    revealed_link_sides(&link_groups, inlines, inline_i, reveal);
                                let mut opens = wrap_opens;
                                opens.extend(link_opens);
                                opens.sort_by_key(|r| r.start);
                                let mut closes = wrap_closes;
                                closes.extend(link_closes);
                                closes.sort_by_key(|r| r.start);
                                let tag = html_chrome_range_for(&html_groups, inline_i, reveal);
                                if !opens.is_empty() || !closes.is_empty() || tag.is_some() {
                                    let delim = wiki_delim_run(text_style, theme);
                                    for open in &opens {
                                        if let Some(slice) = source.get(open.clone()) {
                                            push(
                                                &mut text,
                                                &mut runs,
                                                &mut source_at,
                                                slice,
                                                open.clone(),
                                                delim.clone(),
                                            );
                                        }
                                    }
                                    if let Some(range) = tag {
                                        let slice = source
                                            .get(range.clone())
                                            .filter(|s| !s.is_empty())
                                            .unwrap_or(raw);
                                        if !slice.is_empty() {
                                            push(
                                                &mut text,
                                                &mut runs,
                                                &mut source_at,
                                                slice,
                                                range,
                                                delim.clone(),
                                            );
                                        }
                                    }
                                    for close in &closes {
                                        if let Some(slice) = source.get(close.clone()) {
                                            push(
                                                &mut text,
                                                &mut runs,
                                                &mut source_at,
                                                slice,
                                                close.clone(),
                                                delim.clone(),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        markrust_core::html_visual::InlineHtmlAction::Image { .. } => {
                            if !was_hidden {
                                let (wrap_opens, wrap_closes) =
                                    revealed_wrap_lists(&wrap_groups, inlines, inline_i, reveal);
                                let (link_opens, link_closes) =
                                    revealed_link_sides(&link_groups, inlines, inline_i, reveal);
                                let mut opens = wrap_opens;
                                opens.extend(link_opens);
                                opens.sort_by_key(|r| r.start);
                                let mut closes = wrap_closes;
                                closes.extend(link_closes);
                                closes.sort_by_key(|r| r.start);
                                if !opens.is_empty() || !closes.is_empty() {
                                    let delim = wiki_delim_run(text_style, theme);
                                    for open in &opens {
                                        if let Some(slice) = source.get(open.clone()) {
                                            push(
                                                &mut text,
                                                &mut runs,
                                                &mut source_at,
                                                slice,
                                                open.clone(),
                                                delim.clone(),
                                            );
                                        }
                                    }
                                    for close in &closes {
                                        if let Some(slice) = source.get(close.clone()) {
                                            push(
                                                &mut text,
                                                &mut runs,
                                                &mut source_at,
                                                slice,
                                                close.clone(),
                                                delim.clone(),
                                            );
                                        }
                                    }
                                }
                            }
                        }
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
                            if reveal.intersects(source_range) {
                                let slice = source
                                    .get(source_range.clone())
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or(raw);
                                push(
                                    &mut text,
                                    &mut runs,
                                    &mut source_at,
                                    slice,
                                    source_range.clone(),
                                    wiki_delim_run(text_style, theme),
                                );
                            } else {
                                let paint = html.paint();
                                let visible = markrust_core::html_visual::to_superscript(&label)
                                    .unwrap_or(label);
                                let mut run = style_run(
                                    text_style,
                                    theme,
                                    base_weight,
                                    MarkSet::empty(),
                                    true,
                                    &paint,
                                );
                                run.underline = None;
                                let inner = markrust_core::html_visual::footnote_ref_inner_range(
                                    raw,
                                    source_range.clone(),
                                )
                                .unwrap_or_else(|| source_range.clone());
                                push(&mut text, &mut runs, &mut source_at, &visible, inner, run);
                            }
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
                    }
                }
            }
        }
    }
    if text.is_empty() {
        source_at = vec![block_range.start, block_range.start];
    } else if source_at.len() == text.len() {
        source_at.push(paint_end_src);
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
pub fn build_code_layout(
    body: &str,
    source_start: usize,
    text_style: &TextStyle,
    theme: &EditorTheme,
) -> LeafLayout {
    let source_at: Vec<usize> = (0..=body.len()).map(|i| source_start + i).collect();
    finish_code_layout(body, source_at, source_start, text_style, theme)
}

/// Painted fence body with source offsets that skip quote/list prefixes
/// (same map `character_index_for_point` and IME origin use).
pub fn build_code_block_layout(
    body: &str,
    source: &str,
    block: &Block,
    text_style: &TextStyle,
    theme: &EditorTheme,
) -> LeafLayout {
    let source_at = code_body_source_map(source, block, body.len());
    let block_start = block.code_body_range(source).start;
    finish_code_layout(body, source_at, block_start, text_style, theme)
}

/// Opening/closing fence ticks (and the info string) when the caret or a
/// selection intersects the code block. Apply after body syntax highlighting
/// so those runs are not replaced.
pub fn apply_code_fence_reveal(
    layout: &mut LeafLayout,
    block: &Block,
    source: &str,
    reveal: &RevealState,
    text_style: &TextStyle,
    theme: &EditorTheme,
) {
    paint_fence_chrome(block, source, reveal, text_style, theme, layout);
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

/// Empty `[^1]: ` / `: ` (and quoted forms) paint opener chrome when the
/// caret or a selection hits that line. Empty `> ` / `- ` stay a blank
/// gap (pretty quote/list markers live in the block renderer).
pub fn build_prefix_blank_layout(
    source: &str,
    blank: &PrefixBlank,
    reveal: &RevealState,
    text_style: &TextStyle,
    theme: &EditorTheme,
) -> LeafLayout {
    let end = blank.home.max(blank.line.end);
    let mut layout = build_blank_gap_layout(blank.home..end);
    if source.is_empty() {
        return layout;
    }
    let parts = line_prefix_parts(source, blank.home);
    if parts.footnote.start == parts.footnote.end && parts.details.start == parts.details.end {
        return layout;
    }
    let opener = parts.footnote.start..parts.details.end.max(parts.footnote.end).max(blank.home);
    let hit = reveal.intersects(&blank.line)
        || reveal.intersects(&(blank.home..blank.home.saturating_add(1)))
        || reveal.intersects(&opener);
    if !hit {
        return layout;
    }
    let start = if parts.quote.start < parts.quote.end {
        parts.quote.start
    } else if parts.footnote.start < parts.footnote.end {
        parts.footnote.start
    } else {
        parts.details.start
    };
    let stop = if parts.details.start < parts.details.end {
        parts.details.end
    } else {
        parts.footnote.end
    };
    if start >= stop {
        return layout;
    }
    let Some(slice) = source.get(start..stop).filter(|s| !s.is_empty()) else {
        return layout;
    };
    prepend_source_slice(
        &mut layout,
        slice,
        start..stop,
        wiki_delim_run(text_style, theme),
    );
    layout
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
    visual_lines: Vec<VisualLine>,
    cursor: Option<PaintQuad>,
    caret_bounds: Option<Bounds<Pixels>>,
    selection: Vec<PaintQuad>,
    _host: std::marker::PhantomData<H>,
}

/// Keep the final measured glyph layout through prepaint. Rewrapping at
/// pixel-rounded bounds can otherwise add a line at a fractional threshold
/// after the layout engine has already reserved the paragraph's height.
#[derive(Default, Clone)]
pub struct MeasuredLeafText(Rc<RefCell<Option<Vec<WrappedLine>>>>);

impl<H: WysiwygHost> IntoElement for BlockTextElement<H> {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl<H: WysiwygHost> Element for BlockTextElement<H> {
    type RequestLayoutState = MeasuredLeafText;
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
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        request_leaf_text_layout(
            self.layout.clone(),
            self.font_size,
            self.line_height,
            self.theme.clone(),
            self.hug_width,
            window,
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let lines = request_layout
            .0
            .borrow_mut()
            .take()
            .unwrap_or_else(|| self.shape(window, bounds.size.width));
        let host = self.editor.read(cx);
        let caret = host.caret_offset();
        let selected = host.selected_range();
        let focused = host.focused(window);
        let caret_visible = host.caret_visible();
        let vis_caret = self.layout.visible_for_source(caret);
        let vis_sel = self.layout.visible_for_source(selected.start)
            ..self.layout.visible_for_source(selected.end);

        let line_height = px(self.line_height);
        // Always locate the caret for IME, even when the blink hides the quad.
        let caret_in_leaf = source_in_leaf(&self.layout, caret);
        let (selection, cursor, caret_bounds) = paint_carets(
            &lines,
            bounds,
            line_height,
            vis_caret,
            vis_sel,
            self.layout.text.len(),
            caret_in_leaf,
            focused && caret_visible && caret_in_leaf,
            selected.start != selected.end && ranges_touch_leaf(&self.layout, &selected),
            self.theme.caret,
            self.theme.selection,
        );

        Prepaint {
            visual_lines: visual_lines_for_paint(&self.layout, &lines, bounds, line_height),
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
                std::mem::take(&mut prepaint.visual_lines),
            );
            host.sync_ime_cursor(window);
        });

        for selection in prepaint.selection.drain(..) {
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
}

/// Measure at the width assigned by the containing block, not the window.
/// Table cells, list bodies, and the editor pane can all be substantially
/// narrower than the viewport. Reserving a viewport-sized text height and
/// only rewrapping during prepaint makes following rows overlap the glyphs.
fn request_leaf_text_layout(
    layout: Arc<LeafLayout>,
    font_size: f32,
    line_height: f32,
    theme: EditorTheme,
    hug_width: bool,
    window: &mut Window,
) -> (LayoutId, MeasuredLeafText) {
    let mut style = Style::default();
    if !hug_width {
        style.size.width = relative(1.).into();
    }
    style.min_size.width = px(0.).into();
    style.min_size.height = px(line_height).into();
    let measured_text = MeasuredLeafText::default();
    let shared_text = measured_text.clone();
    let layout_id = window.request_measured_layout(style, move |known, available, window, _cx| {
        let wrap_width = known.width.or(match available.width {
            AvailableSpace::Definite(width) => Some(width),
            AvailableSpace::MinContent => Some(px(1.)),
            AvailableSpace::MaxContent => None,
        });
        let lines =
            shape_layout_with_wrap(&layout, window, wrap_width, font_size, line_height, &theme);
        let lh = px(line_height);
        let mut measured = size(px(0.), px(0.));
        for line in &lines {
            let line_size = line.size(lh);
            measured.width = measured.width.max(line_size.width).ceil();
            measured.height += line_size.height.max(lh);
        }
        let width = known.width.unwrap_or_else(|| {
            if hug_width {
                wrap_width.map_or(measured.width, |width| measured.width.min(width))
            } else {
                wrap_width.unwrap_or(measured.width)
            }
        });
        *shared_text.0.borrow_mut() = Some(lines);
        size(width.max(px(0.)), measured.height.max(lh))
    });
    (layout_id, measured_text)
}

fn shape_layout(
    layout: &LeafLayout,
    window: &mut Window,
    wrap_width: Pixels,
    font_size: f32,
    line_height: f32,
    theme: &EditorTheme,
) -> Vec<WrappedLine> {
    shape_layout_with_wrap(
        layout,
        window,
        Some(wrap_width),
        font_size,
        line_height,
        theme,
    )
}

fn shape_layout_with_wrap(
    layout: &LeafLayout,
    window: &mut Window,
    wrap_width: Option<Pixels>,
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
            wrap_width.map(|width| width.max(px(1.))),
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

/// Capture the real glyph columns produced by GPUI's wrapping pass.
///
/// WYSIWYG navigation cannot derive these from source lines: Markdown chrome
/// may be hidden and a paragraph can soft-wrap at an arbitrary shaped glyph.
/// The source map carried by [`LeafLayout`] turns every visible caret stop
/// back into a Markdown byte offset for the editor model.
fn visual_lines_for_paint(
    layout: &LeafLayout,
    lines: &[WrappedLine],
    bounds: Bounds<Pixels>,
    line_height: Pixels,
) -> Vec<VisualLine> {
    let mut out = Vec::new();
    let mut y = bounds.origin.y;
    let mut offset = 0usize;
    for line in lines {
        // A GPUI `WrappedLine` represents one explicit newline-delimited
        // line and stores soft wraps internally. Split those boundaries here;
        // treating the whole object as one row would silently reintroduce
        // source-line Up/Down behavior.
        let mut wrap_ends: Vec<usize> = line
            .wrap_boundaries()
            .iter()
            .filter_map(|boundary| {
                line.runs()
                    .get(boundary.run_ix)
                    .and_then(|run| run.glyphs.get(boundary.glyph_ix))
                    .map(|glyph| glyph.index.min(line.len()))
            })
            .collect();
        wrap_ends.push(line.len());

        let mut row_start = 0usize;
        for (row_index, row_end) in wrap_ends.into_iter().enumerate() {
            let row_end = row_end.max(row_start).min(line.len());
            let visible_start = offset.saturating_add(row_start).min(layout.text.len());
            let visible_end = offset.saturating_add(row_end).min(layout.text.len());
            let mut visible_offsets = vec![visible_start];
            if let Some(text) = layout.text.get(visible_start..visible_end) {
                visible_offsets.extend(
                    text.char_indices()
                        .skip(1)
                        .map(|(relative, _)| visible_start + relative),
                );
            }
            if visible_offsets.last().copied() != Some(visible_end) {
                visible_offsets.push(visible_end);
            }
            let stops = visible_offsets
                .into_iter()
                .filter_map(|visible| {
                    let local = visible.saturating_sub(offset);
                    // `position_for_index` resolves a shared wrap boundary
                    // to the preceding row. At the beginning of each new
                    // row the actual visual caret is at its left edge.
                    let x = if local == row_start {
                        bounds.origin.x
                    } else {
                        line.position_for_index(local, line_height)?.x + bounds.origin.x
                    };
                    Some(VisualCaretStop {
                        visible,
                        source: layout.source_for_visible(visible),
                        x: f32::from(x),
                    })
                })
                .collect();
            out.push(VisualLine {
                visible_start,
                visible_end,
                top: f32::from(y + line_height * row_index as f32),
                height: f32::from(line_height),
                stops,
            });
            row_start = row_end;
        }
        offset = offset.saturating_add(line.len());
        y += line.size(line_height).height.max(line_height);
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn paint_carets(
    lines: &[WrappedLine],
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    vis_caret: usize,
    vis_sel: Range<usize>,
    text_len: usize,
    locate_caret: bool,
    show_caret: bool,
    show_sel: bool,
    caret_color: gpui::Hsla,
    sel_color: gpui::Hsla,
) -> (Vec<PaintQuad>, Option<PaintQuad>, Option<Bounds<Pixels>>) {
    let _ = text_len;
    let mut cursor = None;
    let mut caret_bounds = None;
    let mut selection = Vec::new();
    let mut y = bounds.origin.y;
    let mut offset = 0usize;
    for line in lines {
        let h = line.size(line_height).height.max(line_height);
        let line_end = offset + line.len();
        if locate_caret && vis_caret >= offset && vis_caret <= line_end {
            if let Some(pos) =
                line.position_for_index(vis_caret.saturating_sub(offset), line_height)
            {
                let rect = Bounds::new(
                    point(bounds.origin.x + pos.x, y + pos.y),
                    size(px(2.), line_height),
                );
                caret_bounds = Some(rect);
                if show_caret {
                    cursor = Some(fill(rect, caret_color));
                }
            }
        }
        if show_sel {
            let row_ends = line
                .wrap_boundaries()
                .iter()
                .map(|boundary| line.runs()[boundary.run_ix].glyphs[boundary.glyph_ix].index)
                .chain([line.len()]);
            for (row_index, row_start, selected) in selected_visual_rows(&vis_sel, offset, row_ends)
            {
                // A soft-wrap boundary has two visual caret positions.
                // Work in the unwrapped glyph coordinates of this row so
                // its start cannot resolve to the previous row's right edge.
                let start_x = line.unwrapped_layout.x_for_index(row_start);
                let left = (line.unwrapped_layout.x_for_index(selected.start) - start_x)
                    .max(px(0.))
                    .min(bounds.size.width);
                let right = (line.unwrapped_layout.x_for_index(selected.end) - start_x)
                    .max(left)
                    .min(bounds.size.width);
                if right > left {
                    selection.push(fill(
                        Bounds::new(
                            point(bounds.origin.x + left, y + line_height * row_index),
                            size(right - left, line_height),
                        ),
                        sel_color,
                    ));
                }
            }
        }
        offset = line_end;
        y += h;
    }
    (selection, cursor, caret_bounds)
}

/// Split a selection at visual wrap boundaries; each returned segment gets
/// its own one-line-high quad. A single bounding rectangle both highlights
/// unselected glyphs and loses earlier hard lines when overwritten.
fn selected_visual_rows(
    selection: &Range<usize>,
    line_offset: usize,
    row_ends: impl IntoIterator<Item = usize>,
) -> Vec<(usize, usize, Range<usize>)> {
    let selection =
        selection.start.saturating_sub(line_offset)..selection.end.saturating_sub(line_offset);
    let mut row_start = 0;
    let mut selected = Vec::new();
    for (row_index, row_end) in row_ends.into_iter().enumerate() {
        let start = selection.start.max(row_start);
        let end = selection.end.min(row_end);
        if start < end {
            selected.push((row_index, row_start, start..end));
        }
        row_start = row_end;
    }
    selected
}

/// Overlay text for a language chip, image caption, or frontmatter field.
///
/// Shapes the draft (body-quality hit-test), paints an inner caret, and
/// reports that caret as the IME origin. Clicks do not move the body caret.
pub struct WidgetOverlay<H: WysiwygHost> {
    pub editor: Entity<H>,
    pub prefix: String,
    pub text: String,
    pub editing: bool,
    pub font_size: f32,
    pub line_height: f32,
    pub theme: EditorTheme,
    pub color: gpui::Hsla,
    pub italic: bool,
    pub monospace: bool,
    pub hug_width: bool,
    pub target: OverlayTarget,
}

pub struct OverlayPrepaint {
    lines: Vec<WrappedLine>,
    display: String,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
}

impl<H: WysiwygHost> IntoElement for WidgetOverlay<H> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<H: WysiwygHost> Element for WidgetOverlay<H> {
    type RequestLayoutState = MeasuredLeafText;
    type PrepaintState = OverlayPrepaint;

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
        let (display, _) = self.display_and_caret(cx);
        let layout = overlay_leaf_layout(&display, self.text_style(), self.italic);
        request_leaf_text_layout(
            Arc::new(layout),
            self.font_size,
            self.line_height,
            self.theme.clone(),
            self.hug_width,
            window,
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let (display, vis_caret) = self.display_and_caret(cx);
        let vis_sel = self.display_sel(cx, display.len());
        let show_sel = self.editing && vis_sel.start != vis_sel.end;
        let layout = overlay_leaf_layout(&display, self.text_style(), self.italic);
        let lines = request_layout.0.borrow_mut().take().unwrap_or_else(|| {
            shape_layout(
                &layout,
                window,
                bounds.size.width,
                self.font_size,
                self.line_height,
                &self.theme,
            )
        });
        let focused = self.editor.read(cx).focused(window);
        let caret_visible = self.editor.read(cx).caret_visible();
        let (selection, cursor, caret_bounds) = paint_carets(
            &lines,
            bounds,
            px(self.line_height),
            vis_caret,
            vis_sel,
            display.len(),
            self.editing,
            self.editing && focused && caret_visible,
            show_sel,
            self.theme.caret,
            self.theme.selection,
        );
        self.editor.update(cx, |host, _cx| {
            host.report_widget_bounds(bounds);
            if let Some(caret) = caret_bounds {
                host.report_widget_caret(caret);
            }
        });
        OverlayPrepaint {
            lines,
            display,
            cursor,
            selection,
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
        if self.editing {
            let focus = self.editor.read(cx).input_focus_handle();
            window.handle_input(
                &focus,
                ElementInputHandler::new(bounds, self.editor.clone()),
                cx,
            );
        }

        for selection in prepaint.selection.drain(..) {
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

        if let Some(cursor) = prepaint.cursor.take() {
            window.paint_quad(cursor);
        }

        if self.editing {
            self.editor.update(cx, |host, _cx| {
                host.sync_ime_cursor(window);
            });
        }

        let editor = self.editor.clone();
        let target = self.target.clone();
        let prefix_len = self.prefix.len();
        let draft_len = self.text.len();
        let font_size = self.font_size;
        let line_height_px = self.line_height;
        let theme = self.theme.clone();
        let style = self.text_style();
        let display = prepaint.display.clone();
        let preedit_len = self
            .editor
            .read(cx)
            .overlay_preedit()
            .map(str::len)
            .unwrap_or(0);

        let italic = self.italic;
        let vis_caret_draft = self
            .editor
            .read(cx)
            .widget_caret_offset()
            .min(self.text.len());

        window.on_mouse_event({
            let editor = editor.clone();
            let target = target.clone();
            let theme = theme.clone();
            let display = display.clone();
            let style = style.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if !phase.bubble() || event.button != MouseButton::Left {
                    return;
                }
                if !bounds.contains(&event.position) {
                    return;
                }
                let layout = overlay_leaf_layout(&display, style.clone(), italic);
                let lines = shape_layout(
                    &layout,
                    window,
                    bounds.size.width,
                    font_size,
                    line_height_px,
                    &theme,
                );
                let vis = visible_index_at(&lines, bounds, event.position, px(line_height_px));
                let offset =
                    overlay_draft_offset(prefix_len, vis, draft_len, vis_caret_draft, preedit_len);
                editor.update(cx, |host, cx| {
                    host.click_overlay(target.clone(), offset, event.modifiers.shift, window, cx);
                });
                window.prevent_default();
            }
        });
        window.on_mouse_event({
            let editor = editor.clone();
            let theme = theme.clone();
            let display = display.clone();
            let style = style.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if !phase.bubble() {
                    return;
                }
                let selecting = editor.read(cx).is_widget_selecting();
                if !selecting || !event.pressed_button.is_some_and(|b| b == MouseButton::Left) {
                    return;
                }
                let layout = overlay_leaf_layout(&display, style.clone(), italic);
                let lines = shape_layout(
                    &layout,
                    window,
                    bounds.size.width,
                    font_size,
                    line_height_px,
                    &theme,
                );
                let vis = visible_index_at(&lines, bounds, event.position, px(line_height_px));
                let offset =
                    overlay_draft_offset(prefix_len, vis, draft_len, vis_caret_draft, preedit_len);
                editor.update(cx, |host, cx| host.drag_overlay(offset, cx));
            }
        });
        window.on_mouse_event({
            move |event: &MouseUpEvent, phase, _, cx| {
                if phase.bubble() && event.button == MouseButton::Left {
                    editor.update(cx, |host, cx| host.end_overlay_drag(cx));
                }
            }
        });
    }
}

impl<H: WysiwygHost> WidgetOverlay<H> {
    fn text_style(&self) -> TextStyle {
        TextStyle {
            color: self.color,
            font_family: if self.monospace {
                self.theme.code_font_family.clone().into()
            } else {
                self.theme.font_family.clone().into()
            },
            font_size: px(self.font_size).into(),
            line_height: px(self.line_height).into(),
            ..Default::default()
        }
    }

    fn display_and_caret(&self, cx: &App) -> (String, usize) {
        let prefix = &self.prefix;
        if !self.editing {
            return (format!("{prefix}{}", self.text), prefix.len());
        }
        let host = self.editor.read(cx);
        let caret = host.widget_caret_offset().min(self.text.len());
        let pre = host.overlay_preedit().unwrap_or("");
        let mut at = caret;
        if !self.text.is_char_boundary(at) {
            at = self.text.len();
        }
        let display = format!("{prefix}{}{}{}", &self.text[..at], pre, &self.text[at..]);
        (display, prefix.len() + at + pre.len())
    }

    fn display_sel(&self, cx: &App, display_len: usize) -> Range<usize> {
        if !self.editing {
            return 0..0;
        }
        let host = self.editor.read(cx);
        let pre_len = host.overlay_preedit().map(str::len).unwrap_or(0);
        let prefix = self.prefix.len();
        let sel = host.widget_sel();
        let start = (prefix + sel.start).min(display_len);
        let end = (prefix + sel.end + pre_len).min(display_len);
        start.min(end)..start.max(end)
    }
}

fn overlay_leaf_layout(text: &str, style: TextStyle, italic: bool) -> LeafLayout {
    let display = if text.is_empty() {
        " ".to_string()
    } else {
        text.to_string()
    };
    let mut run = style.to_run(display.len());
    if italic {
        run.font.style = gpui::FontStyle::Italic;
    }
    let mut source_at = Vec::with_capacity(display.len() + 1);
    for i in 0..=display.len() {
        source_at.push(i);
    }
    LeafLayout {
        text: display,
        runs: vec![run],
        source_at,
        block_start: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use markrust_core::rich::{
        blank_caret_gap_after_last, blank_caret_gap_before, import_markdown, AlertKind, Block,
        BlockKind, IdGen, Inline, RichTree,
    };

    #[test]
    fn selection_is_split_at_each_visual_row_without_filling_unselected_text() {
        assert_eq!(
            selected_visual_rows(&(4..24), 0, [10, 20, 30]),
            vec![(0, 0, 4..10), (1, 10, 10..20), (2, 20, 20..24)]
        );
    }

    #[test]
    fn selection_at_wrap_boundary_belongs_only_to_the_selected_row() {
        assert_eq!(
            selected_visual_rows(&(10..20), 0, [10, 20, 30]),
            vec![(1, 10, 10..20)]
        );
        assert!(selected_visual_rows(&(10..10), 0, [10, 20, 30]).is_empty());
        assert!(selected_visual_rows(&(31..40), 0, [10, 20, 30]).is_empty());
    }

    #[test]
    fn selection_spanning_the_leaf_clips_to_its_actual_visual_rows() {
        assert_eq!(
            selected_visual_rows(&(0..100), 0, [0, 10, 20]),
            vec![(1, 0, 0..10), (2, 10, 10..20)]
        );
    }

    #[test]
    fn selection_across_hard_lines_retains_all_of_their_soft_rows() {
        let selection = 4..28;
        let first_line = selected_visual_rows(&selection, 0, [10, 16]);
        let second_line = selected_visual_rows(&selection, 16, [6, 20]);
        assert_eq!(first_line, vec![(0, 0, 4..10), (1, 10, 10..16)]);
        assert_eq!(second_line, vec![(0, 0, 0..6), (1, 6, 6..12)]);
        assert_eq!(first_line.len() + second_line.len(), 4);
        assert!(selected_visual_rows(&selection, 36, [10]).is_empty());
    }

    fn layout_for(source: &str) -> LeafLayout {
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let block = &tree.blocks[0];
        layout_of_source(block, source, &RevealState::HIDDEN)
    }

    fn layout_of(block: &Block) -> LeafLayout {
        layout_of_source(block, "", &RevealState::HIDDEN)
    }

    fn layout_of_source(block: &Block, source: &str, reveal: &RevealState) -> LeafLayout {
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        build_leaf_layout_revealed(
            block,
            source,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            reveal,
            &ChromeHosts::NONE,
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

    fn find_paragraph_with<'a>(blocks: &'a [Block], needle: &str) -> Option<&'a Block> {
        for block in blocks {
            if matches!(block.kind, BlockKind::Paragraph)
                && block.inlines.iter().any(|inline| match inline {
                    Inline::Run { text, .. } => text.contains(needle),
                    Inline::Image { alt, .. } => alt.contains(needle),
                    _ => false,
                })
            {
                return Some(block);
            }
            if let Some(found) = find_paragraph_with(&block.children, needle) {
                return Some(found);
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
    fn overlay_draft_offset_matches_body_visible_index() {
        assert_eq!(overlay_draft_offset(0, 2, 5, 5, 0), 2);
        assert_eq!(overlay_draft_offset("Title: ".len(), 9, 5, 5, 0), 2);
        assert_eq!(
            overlay_draft_offset(0, 3, 4, 2, 2),
            2,
            "click inside preedit stays at the inner caret"
        );
        assert_eq!(overlay_draft_offset(0, 5, 4, 2, 2), 3);
        assert_eq!(
            overlay_draft_offset(0, 20, 3, 0, 0),
            3,
            "click past the draft clamps"
        );
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
    fn emphasis_reveals_delimiters_when_caret_intersects() {
        let source = "see **bold** here\n";
        let hidden = layout_for(source);
        assert_eq!(hidden.text, "see bold here");
        assert!(
            !hidden.text.contains('*'),
            "caret outside must hide `**`, got {:?}",
            hidden.text
        );
        let b = source.find("bold").expect("bold");
        let revealed = layout_for_caret(source, b);
        assert_eq!(
            revealed.text, "see **bold** here",
            "caret inside bold must reveal `**`, got {:?}",
            revealed.text
        );
        let vis_star = revealed.text.find('*').expect("painted *");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_star))
                .copied(),
            Some(b'*'),
            "click on painted `*` maps onto the delimiter"
        );
        let vis_b = revealed.text.find("bold").expect("painted bold");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_b))
                .copied(),
            Some(b'b'),
            "click on painted `bold` still maps onto `b`, not a phantom `**`"
        );
        assert_eq!(
            source
                .as_bytes()
                .get(hidden.source_for_visible(hidden.text.find('b').expect("b")))
                .copied(),
            Some(b'b'),
            "hidden click on painted bold maps into the word"
        );

        let italic = "go *hi* now\n";
        assert_eq!(layout_for(italic).text, "go hi now");
        let h = italic.find('h').expect("h");
        assert_eq!(layout_for_caret(italic, h).text, "go *hi* now");

        let under = "go _hi_ now\n";
        assert_eq!(layout_for(under).text, "go hi now");
        let h = under.find('h').expect("h");
        assert_eq!(layout_for_caret(under, h).text, "go _hi_ now");

        let strong = "go __hi__ now\n";
        assert_eq!(layout_for(strong).text, "go hi now");
        let h = strong.find('h').expect("h");
        assert_eq!(layout_for_caret(strong, h).text, "go __hi__ now");

        let strike = "go ~~gone~~ now\n";
        assert_eq!(layout_for(strike).text, "go gone now");
        let g = strike.find("gone").expect("gone");
        assert_eq!(layout_for_caret(strike, g).text, "go ~~gone~~ now");

        let code = "go `x` now\n";
        assert_eq!(layout_for(code).text, "go x now");
        let x = code.find('x').expect("x");
        assert_eq!(layout_for_caret(code, x).text, "go `x` now");

        let start = source.find("**bold**").expect("span");
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let selected = layout_of_source(
            &tree.blocks[0],
            source,
            &RevealState {
                caret: 0,
                selection: start..start + "**bold**".len(),
            },
        );
        assert!(
            selected.text.contains("**bold**"),
            "selection overlap must reveal `**`, got {:?}",
            selected.text
        );
    }

    #[test]
    fn nested_emphasis_reveals_per_span() {
        let source = "**bold *italic* x**\n";
        let hidden = layout_for(source);
        assert_eq!(hidden.text, "bold italic x");
        assert!(!hidden.text.contains('*'));

        let in_italic = source.find("italic").expect("italic");
        let both = layout_for_caret(source, in_italic);
        assert_eq!(
            both.text, "**bold *italic* x**",
            "caret in nested italic is inside both spans, got {:?}",
            both.text
        );

        let in_bold = source.find("bold").expect("bold");
        let outer = layout_for_caret(source, in_bold);
        assert_eq!(
            outer.text, "**bold italic x**",
            "caret in outer bold must not reveal nested italic stars, got {:?}",
            outer.text
        );
        assert!(outer.text.contains("**"));
        assert!(!outer.text.contains("*italic*"));
    }

    #[test]
    fn atx_reveals_hashes_when_caret_intersects() {
        let source = "# Title\n";
        let hidden = layout_for(source);
        assert_eq!(hidden.text, "Title");
        assert!(!hidden.text.contains('#'));
        let t = source.find('T').expect("T");
        let revealed = layout_for_caret(source, t);
        assert!(
            revealed.text.contains('#') && revealed.text.contains("Title"),
            "caret in ATX title must reveal hashes, got {:?}",
            revealed.text
        );
        assert_eq!(revealed.text, "# Title");
        let vis_hash = revealed.text.find('#').expect("#");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_hash))
                .copied(),
            Some(b'#'),
            "click on painted `#` maps onto the hash"
        );
        let vis_t = revealed.text.find('T').expect("T");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_t))
                .copied(),
            Some(b'T'),
            "click on painted Title still maps onto `T`"
        );

        let h2 = "## Sub\n";
        assert_eq!(layout_for(h2).text, "Sub");
        let s = h2.find('S').expect("S");
        assert_eq!(layout_for_caret(h2, s).text, "## Sub");

        let closed = "# Title #\n";
        assert_eq!(layout_for(closed).text, "Title");
        let t = closed.find('T').expect("T");
        let closed_rev = layout_for_caret(closed, t);
        assert!(
            closed_rev.text.starts_with('#') && closed_rev.text.contains("Title"),
            "closed ATX must reveal opening hashes, got {:?}",
            closed_rev.text
        );
        assert!(
            closed_rev.text.ends_with('#'),
            "closed ATX must reveal trailing hashes, got {:?}",
            closed_rev.text
        );
    }

    /// CommonMark 0–3 spaces before ATX `#` / fence ticks / thematic `---`
    /// hide with the marker and reveal on intersect. Click on body maps
    /// onto the word. Quoted keep `>`. Four spaces stay indented code.
    #[test]
    fn cm_opening_indent_hides_and_reveals_with_the_marker() {
        for (source, body, hidden, revealed) in [
            (" # Title\n", "Title", "Title", " # Title"),
            ("  # Title\n", "Title", "Title", "  # Title"),
            ("   # Title\n", "Title", "Title", "   # Title"),
        ] {
            let hidden_layout = layout_for(source);
            assert_eq!(
                hidden_layout.text, hidden,
                "caret outside must hide opening indent with `#`, {source:?} got {:?}",
                hidden_layout.text
            );
            let at = source.find(body).expect(body);
            let rev = layout_for_caret(source, at);
            assert_eq!(
                rev.text, revealed,
                "caret in the title must reveal indent+`#`, {source:?} got {:?}",
                rev.text
            );
            let vis = rev.text.find(body).expect(body);
            assert_eq!(
                source.as_bytes().get(rev.source_for_visible(vis)).copied(),
                Some(body.as_bytes()[0]),
                "click on body maps onto the word, {source:?}"
            );
        }

        let quoted = ">  # Title\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(quoted, &mut ids);
        let heading = first_kind(&tree.blocks, |k| matches!(k, BlockKind::Heading { .. }))
            .expect("quoted heading");
        let hidden_q = layout_of_source(heading, quoted, &RevealState::HIDDEN);
        assert_eq!(
            hidden_q.text, "Title",
            "quoted caret outside must hide indent+`#` (keep `>` as quote chrome), got {:?}",
            hidden_q.text
        );
        let t = quoted.find("Title").expect("Title");
        let rev_q = layout_for_caret(quoted, t);
        assert!(
            rev_q.text.contains('>') && rev_q.text.contains('#') && rev_q.text.contains("Title"),
            "quoted caret in title must keep `>` and reveal indent+`#`, got {:?}",
            rev_q.text
        );
        assert!(
            rev_q.text.contains("  #") || rev_q.text.contains(">  #"),
            "quoted reveal must include the extra indent with `#`, got {:?}",
            rev_q.text
        );

        let setext = " Title\n===\n";
        assert_eq!(
            layout_for(setext).text,
            "Title",
            "caret outside must hide setext indent with the underline, got {:?}",
            layout_for(setext).text
        );
        let st = setext.find("Title").expect("Title");
        let setext_rev = layout_for_caret(setext, st);
        assert!(
            setext_rev.text.starts_with(' ')
                && setext_rev.text.contains("Title")
                && setext_rev.text.contains('='),
            "caret in setext title must reveal indent with underline, got {:?}",
            setext_rev.text
        );
        let vis_t = setext_rev.text.find('T').expect("T");
        assert_eq!(
            setext
                .as_bytes()
                .get(setext_rev.source_for_visible(vis_t))
                .copied(),
            Some(b'T'),
            "click on painted Title still maps onto `T`"
        );

        let fence = "  ```\n  foo\n  ```\n";
        let hidden_fence = fenced_code_layout(fence);
        assert_eq!(
            hidden_fence.text, "foo",
            "caret outside must hide fence indent and ticks, got {:?}",
            hidden_fence.text
        );
        let f = fence.find("foo").expect("foo");
        assert_eq!(
            hidden_fence.source_for_visible(0),
            f,
            "click on painted `foo` must map onto `f`, not fence_offset spaces"
        );
        let mut ids_f = IdGen::default();
        let tree_f = import_markdown(fence, &mut ids_f);
        let fence_block = first_kind(&tree_f.blocks, |k| {
            matches!(k, BlockKind::CodeBlock { fence: Some(_), .. })
        })
        .expect("fence");
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.code_font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        let mut fence_rev = fenced_code_layout(fence);
        super::apply_code_fence_reveal(
            &mut fence_rev,
            fence_block,
            fence,
            &RevealState {
                caret: f,
                selection: 0..0,
            },
            &style,
            &theme,
        );
        assert!(
            fence_rev.text.contains("  ```") && fence_rev.text.contains("foo"),
            "caret in fence body must reveal indent with ticks, got {:?}",
            fence_rev.text
        );
        let vis_tick = fence_rev.text.find('`').expect("`");
        assert_eq!(
            fence
                .as_bytes()
                .get(fence_rev.source_for_visible(vis_tick))
                .copied(),
            Some(b'`'),
            "click on painted ticks maps onto a tick"
        );
        let vis_f = fence_rev.text.find('f').expect("f");
        assert_eq!(
            fence
                .as_bytes()
                .get(fence_rev.source_for_visible(vis_f))
                .copied(),
            Some(b'f'),
            "click on painted foo still maps onto `f`"
        );

        let rule = " ---\n";
        let hidden_rule = layout_thematic(rule, &RevealState::HIDDEN);
        assert!(
            hidden_rule.text.is_empty(),
            "caret outside must paint a rule, got {:?}",
            hidden_rule.text
        );
        let dash = rule.find('-').expect("-");
        let rev_rule = layout_thematic(
            rule,
            &RevealState {
                caret: dash,
                selection: 0..0,
            },
        );
        assert_eq!(
            rev_rule.text, " ---",
            "caret on the rule must reveal indent with `---`, got {:?}",
            rev_rule.text
        );
        let vis_dash = rev_rule.text.find('-').expect("-");
        assert_eq!(
            rule.as_bytes()
                .get(rev_rule.source_for_visible(vis_dash))
                .copied(),
            Some(b'-'),
            "click on painted `-` maps onto the marker"
        );
    }

    #[test]
    fn setext_reveals_underline_when_caret_intersects() {
        let source = "Title\n=====\n";
        let hidden = layout_for(source);
        assert_eq!(hidden.text, "Title");
        assert!(
            !hidden.text.contains('='),
            "setext underline must hide when caret is outside, got {:?}",
            hidden.text
        );
        let t = source.find('T').expect("T");
        let revealed = layout_for_caret(source, t);
        assert!(
            revealed.text.contains("Title") && revealed.text.contains('='),
            "caret in setext title must reveal the underline, got {:?}",
            revealed.text
        );
        let vis_eq = revealed.text.find('=').expect("=");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_eq))
                .copied(),
            Some(b'='),
            "click on painted `=` maps onto the underline"
        );
        let vis_t = revealed.text.find('T').expect("T");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_t))
                .copied(),
            Some(b'T'),
            "click on painted Title still maps onto `T`"
        );
    }

    #[test]
    fn link_reveals_brackets_hides_dest_when_caret_in_label() {
        let source = "see [label](https://e.com) now\n";
        let hidden = layout_for(source);
        assert_eq!(hidden.text, "see label now");
        assert!(
            !hidden.text.contains('[') && !hidden.text.contains(']'),
            "link chrome must hide when caret is outside, got {:?}",
            hidden.text
        );

        let l = source.find("label").expect("label");
        let in_label = layout_for_caret(source, l);
        assert_eq!(
            in_label.text, "see [label] now",
            "caret in the label reveals `[` `]` and hides dest, got {:?}",
            in_label.text
        );
        assert!(
            !in_label.text.contains("https://e.com") && !in_label.text.contains('('),
            "dest must stay hidden while the caret is only in the label, got {:?}",
            in_label.text
        );
        let vis_l = in_label.text.find("label").expect("painted label");
        assert_eq!(
            source
                .as_bytes()
                .get(in_label.source_for_visible(vis_l))
                .copied(),
            Some(b'l'),
            "click on painted label maps onto `l`, not a phantom `[`"
        );
        let vis_br = in_label.text.find('[').expect("painted [");
        assert_eq!(
            source
                .as_bytes()
                .get(in_label.source_for_visible(vis_br))
                .copied(),
            Some(b'['),
            "click on revealed `[` maps onto that source byte"
        );

        let dest = source.find("https").expect("dest");
        let in_dest = layout_for_caret(source, dest);
        assert!(
            in_dest.text.contains("[label](https://e.com)"),
            "caret in dest must reveal `](url)`, got {:?}",
            in_dest.text
        );

        let titled = "see [label](https://e.com \"title\") now\n";
        let titled_dest = titled.find("https").expect("titled dest");
        let in_titled = layout_for_caret(titled, titled_dest);
        assert!(
            in_titled.text.contains("[label](https://e.com \"title\")"),
            "caret in titled dest must reveal dest+title, got {:?}",
            in_titled.text
        );

        let start = source.find("[label](https://e.com)").expect("span");
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let selected = layout_of_source(
            &tree.blocks[0],
            source,
            &RevealState {
                caret: 0,
                selection: start..start + "[label](https://e.com)".len(),
            },
        );
        assert!(
            selected.text.contains("[label](https://e.com)"),
            "selection overlap must reveal dest (source masking), got {:?}",
            selected.text
        );

        let bold_link = "[**bold**](https://e.com)\n";
        assert_eq!(layout_for(bold_link).text, "bold");
        let b = bold_link.find("bold").expect("bold");
        let both = layout_for_caret(bold_link, b);
        assert_eq!(
            both.text, "[**bold**]",
            "caret in a bold label reveals wrap marks and `[` `]`, not dest, got {:?}",
            both.text
        );

        let wrapped = "see **[hello](https://e.com)** now\n";
        let hidden_wrap = layout_for(wrapped);
        assert_eq!(
            hidden_wrap.text, "see hello now",
            "caret outside must hide wrapping `**` and link chrome, got {:?}",
            hidden_wrap.text
        );
        assert!(
            !hidden_wrap.text.contains('*') && !hidden_wrap.text.contains('['),
            "wrapping emphasis around a link must hide like dest chrome, got {:?}",
            hidden_wrap.text
        );
        let h = wrapped.find("hello").expect("hello");
        let in_wrap = layout_for_caret(wrapped, h);
        assert_eq!(
            in_wrap.text, "see **[hello]** now",
            "caret in a bold-wrapped link reveals wrap marks and `[` `]`, not dest, got {:?}",
            in_wrap.text
        );
        let vis_h = in_wrap.text.find("hello").expect("painted hello");
        assert_eq!(
            wrapped
                .as_bytes()
                .get(in_wrap.source_for_visible(vis_h))
                .copied(),
            Some(b'h'),
            "click on painted `hello` still maps onto `h`"
        );
        let vis_star = in_wrap.text.find('*').expect("painted *");
        assert_eq!(
            wrapped
                .as_bytes()
                .get(in_wrap.source_for_visible(vis_star))
                .copied(),
            Some(b'*'),
            "click on revealed wrapping `*` maps onto that source byte"
        );

        let italic = "see *[hello](https://e.com)* now\n";
        assert_eq!(layout_for(italic).text, "see hello now");
        let ih = italic.find("hello").expect("hello");
        assert_eq!(
            layout_for_caret(italic, ih).text,
            "see *[hello]* now",
            "italic wrapping a link reveals `*` and `[` `]`, got {:?}",
            layout_for_caret(italic, ih).text
        );

        let strike = "see ~~[hello](https://e.com)~~ now\n";
        assert_eq!(layout_for(strike).text, "see hello now");
        let sh = strike.find("hello").expect("hello");
        assert_eq!(
            layout_for_caret(strike, sh).text,
            "see ~~[hello]~~ now",
            "strike wrapping a link reveals `~~` and `[` `]`, got {:?}",
            layout_for_caret(strike, sh).text
        );

        let nested = "see ***[hello](https://e.com)*** now\n";
        assert_eq!(layout_for(nested).text, "see hello now");
        let nh = nested.find("hello").expect("hello");
        assert_eq!(
            layout_for_caret(nested, nh).text,
            "see ***[hello]*** now",
            "nested `***` wrapping a link reveals wrap marks and `[` `]`, got {:?}",
            layout_for_caret(nested, nh).text
        );

        let html_wrap = "see **<b>hello</b>** now\n";
        assert_eq!(
            layout_for(html_wrap).text,
            "see hello now",
            "caret outside must hide wrapping `**` and HTML tags, got {:?}",
            layout_for(html_wrap).text
        );
        let hh = html_wrap.find("hello").expect("hello");
        assert_eq!(
            layout_for_caret(html_wrap, hh).text,
            "see **<b>hello</b>** now",
            "caret in HTML wrapped by `**` reveals marks and tags, got {:?}",
            layout_for_caret(html_wrap, hh).text
        );

        let linked_html = "see [<b>hello</b>](https://e.com) now\n";
        assert_eq!(
            layout_for(linked_html).text,
            "see hello now",
            "caret outside must hide `[` and HTML tags around a linked label, got {:?}",
            layout_for(linked_html).text
        );
        let lh = linked_html.find("hello").expect("hello");
        assert_eq!(
            layout_for_caret(linked_html, lh).text,
            "see [<b>hello</b>] now",
            "caret in HTML inside a link reveals `[` `]` and tags, not dest, got {:?}",
            layout_for_caret(linked_html, lh).text
        );

        let img_wrap = "see **![alt](a.png)** now\n";
        assert_eq!(
            layout_for(img_wrap).text,
            "see  now",
            "caret outside must hide wrapping `**` around an image, got {:?}",
            layout_for(img_wrap).text
        );
        let ia = img_wrap.find("alt").expect("alt");
        let img_in = layout_for_caret(img_wrap, ia);
        assert!(
            img_in.text.contains("**") && img_in.text.contains("![alt]"),
            "caret on an image wrapped by `**` reveals wrapping marks, got {:?}",
            img_in.text
        );

        let html_img = "see **<img src=\"a.png\">** now\n";
        assert_eq!(
            layout_for(html_img).text,
            "see  now",
            "caret outside must hide wrapping `**` around HTML `<img>`, got {:?}",
            layout_for(html_img).text
        );
        let src = html_img.find("src").expect("src");
        let html_img_in = layout_for_caret(html_img, src);
        assert!(
            html_img_in.text.contains("**") && !html_img_in.text.contains('<'),
            "caret on HTML `<img>` wrapped by `**` reveals wrapping marks, not the tag, got {:?}",
            html_img_in.text
        );

        let linked_img = "see *[![cat](a.png)](https://e.com)* now\n";
        assert!(
            !layout_for(linked_img).text.contains('*')
                && !layout_for(linked_img).text.contains('['),
            "caret outside must hide italic wrap around a linked image, got {:?}",
            layout_for(linked_img).text
        );
        let cat = linked_img.find("cat").expect("cat");
        let linked_in = layout_for_caret(linked_img, cat);
        assert!(
            linked_in.text.contains('*')
                && linked_in.text.contains("[![cat]")
                && !linked_in.text.contains("https://e.com"),
            "caret on a linked image wrapped by `*` reveals wrap and `[` `]`, not dest, got {:?}",
            linked_in.text
        );

        let strike_ref = "see ~~[hello][ref]~~ now\n\n[ref]: https://e.com\n";
        assert_eq!(layout_for(strike_ref).text, "see hello now");
        let srh = strike_ref.find("hello").expect("hello");
        assert_eq!(
            layout_for_caret(strike_ref, srh).text,
            "see ~~[hello]~~ now",
            "strike wrapping a reference link reveals `~~` and `[` `]`, got {:?}",
            layout_for_caret(strike_ref, srh).text
        );

        let mixed = "see **hello [link](https://e.com)** now\n";
        assert_eq!(
            layout_for(mixed).text,
            "see hello link now",
            "bold spanning a trailing link must hide wrap and dest, got {:?}",
            layout_for(mixed).text
        );
        let lk = mixed.find("link").expect("link");
        let mixed_in = layout_for_caret(mixed, lk);
        assert!(
            mixed_in.text.contains("**")
                && mixed_in.text.contains("[link]")
                && !mixed_in.text.contains("https://e.com"),
            "caret in the trailing link of a bold span reveals wrapping `**` and `[` `]`, got {:?}",
            mixed_in.text
        );

        let reference = "see [label][ref] now\n\n[ref]: https://e.com\n";
        let r = reference.find("label").expect("label");
        let ref_layout = layout_for_caret(reference, r);
        assert!(
            ref_layout.text.contains("[label]") && !ref_layout.text.contains("[ref]"),
            "caret in a reference label hides `[ref]` dest, got {:?}",
            ref_layout.text
        );
    }

    #[test]
    fn image_reveals_bang_brackets_when_caret_intersects() {
        let source = "![cat](img.png)\n";
        let hidden = layout_for(source);
        assert!(
            hidden.text.is_empty(),
            "hidden image leaf stays empty (pixels are a sibling), got {:?}",
            hidden.text
        );
        let c = source.find("cat").expect("cat");
        let revealed = layout_for_caret(source, c);
        assert_eq!(
            revealed.text, "![cat](img.png)",
            "caret on an image paints `![]()` including dest (atomic), got {:?}",
            revealed.text
        );
        let vis_c = revealed.text.find("cat").expect("painted cat");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_c))
                .copied(),
            Some(b'c'),
            "click on painted alt maps onto `c`"
        );
        let vis_bang = revealed.text.find('!').expect("!");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_bang))
                .copied(),
            Some(b'!'),
            "click on revealed `!` maps onto that source byte"
        );

        let mixed = "hello ![cat](img.png) world\n";
        assert_eq!(layout_for(mixed).text, "hello  world");
        let in_img = layout_for_caret(mixed, mixed.find("cat").expect("cat"));
        assert!(
            in_img.text.contains("![cat](img.png)"),
            "caret on a mixed image must paint image chrome, got {:?}",
            in_img.text
        );
        assert!(
            in_img.text.contains("hello") && in_img.text.contains("world"),
            "surrounding text stays, got {:?}",
            in_img.text
        );

        let wrapped = "[![alt](img.png)](https://e.com)\n";
        let a = wrapped.find("alt").expect("alt");
        let on_img = layout_for_caret(wrapped, a);
        assert!(
            on_img.text.contains("![alt](img.png)"),
            "linked image must paint image chrome, got {:?}",
            on_img.text
        );
        assert!(
            on_img.text.starts_with('[') && on_img.text.contains(']'),
            "wrapping `[` `]` reveal when the image (label) is intersected, got {:?}",
            on_img.text
        );
        assert!(
            !on_img.text.contains("https://e.com"),
            "wrapping dest stays hidden while the caret is in the image label, got {:?}",
            on_img.text
        );
        let dest = wrapped.find("https").expect("dest");
        let on_dest = layout_for_caret(wrapped, dest);
        assert!(
            on_dest.text.contains("](https://e.com)") || on_dest.text.contains("(https://e.com)"),
            "caret in wrapping dest must reveal dest, got {:?}",
            on_dest.text
        );
    }

    fn first_thematic(blocks: &[Block]) -> Option<&Block> {
        for b in blocks {
            if super::block_paints_as_thematic_break(b) {
                return Some(b);
            }
            if let Some(found) = first_thematic(&b.children) {
                return Some(found);
            }
        }
        None
    }

    fn layout_thematic(source: &str, reveal: &RevealState) -> LeafLayout {
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let block = first_thematic(&tree.blocks).expect("thematic break");
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        build_leaf_layout_revealed(
            block,
            source,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            reveal,
            &chrome_hosts_for(&tree, block.id),
        )
    }

    #[test]
    fn thematic_break_reveals_source_when_caret_intersects() {
        for source in [
            "hello\n\n---\n\nworld\n",
            "hello\n\n***\n\nworld\n",
            "hello\n\n___\n\nworld\n",
            "hello\n\n* * *\n\nworld\n",
            "hello\n\n- - -\n\nworld\n",
            "hello\n\n<hr>\n\nworld\n",
            "hello\n\n<hr/>\n\nworld\n",
        ] {
            let hidden = layout_thematic(source, &RevealState::HIDDEN);
            assert!(
                hidden.text.is_empty(),
                "caret outside must paint a rule (empty leaf), got {:?} in {source:?}",
                hidden.text
            );
            let marker = if source.contains("<hr") {
                source.find("<hr").expect("hr")
            } else if source.contains("---") {
                source.find("---").expect("---")
            } else if source.contains("***") {
                source.find("***").expect("***")
            } else if source.contains("___") {
                source.find("___").expect("___")
            } else if source.contains("* * *") {
                source.find("* * *").expect("* * *")
            } else {
                source.find("- - -").expect("- - -")
            };
            let revealed = layout_thematic(
                source,
                &RevealState {
                    caret: marker,
                    selection: 0..0,
                },
            );
            let want = if source.contains("<hr/>") {
                "<hr/>"
            } else if source.contains("<hr>") {
                "<hr>"
            } else if source.contains("---") {
                "---"
            } else if source.contains("***") {
                "***"
            } else if source.contains("___") {
                "___"
            } else if source.contains("* * *") {
                "* * *"
            } else {
                "- - -"
            };
            assert!(
                revealed.text.contains(want),
                "caret on the break must reveal {want:?}, got {:?} in {source:?}",
                revealed.text
            );
            let needle = want.as_bytes()[0];
            let vis = revealed
                .text
                .as_bytes()
                .iter()
                .position(|&b| b == needle)
                .expect("painted marker");
            assert_eq!(
                source
                    .as_bytes()
                    .get(revealed.source_for_visible(vis))
                    .copied(),
                Some(needle),
                "click on revealed source maps onto that byte in {source:?}"
            );
        }

        let quoted = "> ---\n";
        let dash = quoted.find('-').expect("-");
        let on_rule = layout_thematic(
            quoted,
            &RevealState {
                caret: dash,
                selection: 0..0,
            },
        );
        assert!(
            on_rule.text.contains("---"),
            "quoted break must reveal `---`, got {:?}",
            on_rule.text
        );
        assert!(
            on_rule.text.contains('>'),
            "quoted break must reveal `>` when intersected, got {:?}",
            on_rule.text
        );
        let vis_dash = on_rule.text.find('-').expect("painted -");
        assert_eq!(
            quoted
                .as_bytes()
                .get(on_rule.source_for_visible(vis_dash))
                .copied(),
            Some(b'-'),
            "click on revealed `-` maps onto the dash, not `>`"
        );

        let listed = "- <hr>\n";
        let lt = listed.find('<').expect("<");
        let on_hr = layout_thematic(
            listed,
            &RevealState {
                caret: lt,
                selection: 0..0,
            },
        );
        assert!(
            on_hr.text.contains("<hr>"),
            "list `<hr>` must reveal the tag, got {:?}",
            on_hr.text
        );
        let vis_lt = on_hr.text.find('<').expect("painted <");
        assert_eq!(
            listed
                .as_bytes()
                .get(on_hr.source_for_visible(vis_lt))
                .copied(),
            Some(b'<'),
            "click on revealed `<` maps onto the tag"
        );

        let source = "hello\n\n---\n\nworld\n";
        let start = source.find("---").expect("---");
        let selected = layout_thematic(
            source,
            &RevealState {
                caret: 0,
                selection: start..start + 3,
            },
        );
        assert!(
            selected.text.contains("---"),
            "selection overlap must reveal `---`, got {:?}",
            selected.text
        );
    }

    #[test]
    fn list_quote_fence_table_reveal_chrome_when_caret_intersects() {
        let list = "- hello\n- world\n";
        let h = list.find("hello").expect("hello");
        let mut ids = IdGen::default();
        let tree = import_markdown(list, &mut ids);
        let first_para = first_paragraph(&tree.blocks[0]).expect("first item");
        assert_eq!(
            layout_of(first_para).text,
            "hello",
            "caret outside hides list chrome, got {:?}",
            layout_of(first_para).text
        );
        let in_item = layout_for_caret(list, h);
        assert_eq!(
            in_item.text, "- hello",
            "caret in a list item must reveal `- `, got {:?}",
            in_item.text
        );
        let vis_dash = in_item.text.find('-').expect("-");
        assert_eq!(
            list.as_bytes()
                .get(in_item.source_for_visible(vis_dash))
                .copied(),
            Some(b'-'),
            "click on painted `-` maps onto the marker"
        );
        let vis_h = in_item.text.find('h').expect("h");
        assert_eq!(
            list.as_bytes()
                .get(in_item.source_for_visible(vis_h))
                .copied(),
            Some(b'h'),
            "click on body still maps onto `h`, not a phantom `-`"
        );

        let w = list.find("world").expect("world");
        let first_while_on_second = layout_in_tree(
            &tree,
            first_para,
            list,
            &RevealState {
                caret: w,
                selection: 0..0,
            },
        );
        assert_eq!(
            first_while_on_second.text, "hello",
            "unfocused item must keep the marker hidden, got {:?}",
            first_while_on_second.text
        );
        let start = list.find("- hello").expect("item");
        let selected = layout_in_tree(
            &tree,
            first_para,
            list,
            &RevealState {
                caret: 0,
                selection: start..start + "- hello".len(),
            },
        );
        assert!(
            selected.text.contains("- hello"),
            "selection overlap must reveal the list marker, got {:?}",
            selected.text
        );

        let ordered = "1. hello\n";
        let mut ids = IdGen::default();
        let ordered_tree = import_markdown(ordered, &mut ids);
        let ordered_para = first_paragraph(&ordered_tree.blocks[0]).expect("ordered");
        assert_eq!(layout_of(ordered_para).text, "hello");
        let oh = ordered.find('h').expect("h");
        assert_eq!(layout_for_caret(ordered, oh).text, "1. hello");

        let task = "- [x] done\n";
        let mut ids = IdGen::default();
        let task_tree = import_markdown(task, &mut ids);
        let task_para = first_paragraph(&task_tree.blocks[0]).expect("task");
        assert_eq!(layout_of(task_para).text, "done");
        let d = task.find("done").expect("done");
        let task_rev = layout_for_caret(task, d);
        assert_eq!(
            task_rev.text, "- [x] done",
            "caret in a task item reveals `- [x] `, got {:?}",
            task_rev.text
        );
        let vis_d = task_rev.text.find('d').expect("d");
        assert_eq!(
            task.as_bytes()
                .get(task_rev.source_for_visible(vis_d))
                .copied(),
            Some(b'd'),
            "click on task body maps onto `d`"
        );

        let quote = "> quoted\n";
        let mut ids = IdGen::default();
        let quote_tree = import_markdown(quote, &mut ids);
        let quote_para = first_paragraph(&quote_tree.blocks[0]).expect("quote");
        assert_eq!(layout_of(quote_para).text, "quoted");
        let qh = quote.find('q').expect("q");
        let quote_rev = layout_for_caret(quote, qh);
        assert_eq!(
            quote_rev.text, "> quoted",
            "caret in a quote must reveal `>`, got {:?}",
            quote_rev.text
        );
        let vis_gt = quote_rev.text.find('>').expect(">");
        assert_eq!(
            quote
                .as_bytes()
                .get(quote_rev.source_for_visible(vis_gt))
                .copied(),
            Some(b'>'),
            "click on painted `>` maps onto the marker"
        );
        let vis_q = quote_rev.text.find('q').expect("q");
        assert_eq!(
            quote
                .as_bytes()
                .get(quote_rev.source_for_visible(vis_q))
                .copied(),
            Some(b'q'),
            "click on quoted body maps onto `q`"
        );

        let wrap = "> hello\n> world\n";
        let wrap_h = wrap.find('h').expect("h");
        let wrap_rev = layout_for_caret(wrap, wrap_h);
        assert!(
            wrap_rev.text.contains("> hello") && wrap_rev.text.contains("> world"),
            "caret in a wrapped quote reveals continuation `>`, got {:?}",
            wrap_rev.text
        );
        let vis_w = wrap_rev.text.find("world").expect("world");
        assert_eq!(
            wrap.as_bytes()
                .get(wrap_rev.source_for_visible(vis_w))
                .copied(),
            Some(b'w'),
            "click on continuation body maps onto `w`"
        );

        let quoted_list = "> - item\n";
        let i = quoted_list.find("item").expect("item");
        let ql = layout_for_caret(quoted_list, i);
        assert_eq!(
            ql.text, "> - item",
            "quoted list item reveals `>` and `- `, got {:?}",
            ql.text
        );

        let fence = "```rust\nfn main() {}\n```\n";
        let hidden_fence = layout_for(fence);
        assert_eq!(hidden_fence.text, "fn main() {}");
        assert!(!hidden_fence.text.contains('`'));
        let f = fence.find("fn").expect("fn");
        let fence_rev = layout_for_caret(fence, f);
        assert!(
            fence_rev.text.contains("```")
                && fence_rev.text.contains("rust")
                && fence_rev.text.contains("fn main() {}"),
            "caret in a fence must reveal ticks and language, got {:?}",
            fence_rev.text
        );
        let vis_tick = fence_rev.text.find('`').expect("`");
        assert_eq!(
            fence
                .as_bytes()
                .get(fence_rev.source_for_visible(vis_tick))
                .copied(),
            Some(b'`'),
            "click on painted ticks maps onto the fence"
        );
        let vis_fn = fence_rev.text.find("fn").expect("fn");
        assert_eq!(
            fence
                .as_bytes()
                .get(fence_rev.source_for_visible(vis_fn))
                .copied(),
            Some(b'f'),
            "click on fence body still maps onto `f`"
        );

        let table = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let mut ids = IdGen::default();
        let table_tree = import_markdown(table, &mut ids);
        let cell =
            first_kind(&table_tree.blocks, |k| matches!(k, BlockKind::TableCell)).expect("cell");
        let hidden_cell = layout_of(cell);
        assert_eq!(hidden_cell.text, "a");
        assert!(!hidden_cell.text.contains('|'));
        let a = table.find('a').expect("a");
        let cell_rev = layout_for_caret(table, a);
        assert!(
            cell_rev.text.contains('|') && cell_rev.text.contains('a'),
            "caret in a table cell must reveal `|`, got {:?}",
            cell_rev.text
        );
        let vis_a = cell_rev.text.find('a').expect("a");
        assert_eq!(
            table
                .as_bytes()
                .get(cell_rev.source_for_visible(vis_a))
                .copied(),
            Some(b'a'),
            "click on cell text still maps onto `a`"
        );
        let one = table.find('1').expect("1");
        let other_cell = layout_for_caret(table, one);
        assert!(
            other_cell.text.contains('|'),
            "caret anywhere in the table reveals pipes on that cell, got {:?}",
            other_cell.text
        );
        let align = table_alignment_line(table, &table_tree.blocks[0]).expect("align");
        assert!(
            table[align.clone()].contains('-') && table[align.clone()].contains('|'),
            "alignment row is the delimiter line, got {:?}",
            &table[align]
        );
    }

    /// CommonMark tab / 1–4 space list-marker padding hides with the marker
    /// and reveals on intersect. Click on body still maps onto the word.
    #[test]
    fn list_marker_padding_hides_and_reveals_with_the_marker() {
        for (source, body, hidden, revealed) in [
            ("-\titem\n", "item", "item", "-\titem"),
            ("-   item\n", "item", "item", "-   item"),
            ("1.\titem\n", "item", "item", "1.\titem"),
            ("1.   item\n", "item", "item", "1.   item"),
            ("> -\titem\n", "item", "item", "> -\titem"),
            ("-\t[ ] hello\n", "hello", "hello", "-\t[ ] hello"),
            ("-   [x] done\n", "done", "done", "-   [x] done"),
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let para = first_paragraph(&tree.blocks[0]).expect(source);
            assert_eq!(
                layout_of(para).text,
                hidden,
                "caret outside must hide padded marker, {source:?} got {:?}",
                layout_of(para).text
            );
            let at = source.find(body).expect(body);
            let rev = layout_for_caret(source, at);
            assert_eq!(
                rev.text, revealed,
                "caret in the item must reveal padded marker, {source:?} got {:?}",
                rev.text
            );
            let vis = rev.text.find(body).expect(body);
            assert_eq!(
                source.as_bytes().get(rev.source_for_visible(vis)).copied(),
                Some(body.as_bytes()[0]),
                "click on body maps onto the word, {source:?}"
            );
        }
    }

    /// CommonMark code-span padding spaces hide with the ticks. Click on
    /// painted `foo` still maps into the word.
    #[test]
    fn code_span_padding_hides_and_reveals_with_the_ticks() {
        for (source, body, hidden, revealed) in [
            ("` foo `\n", "foo", "foo", "` foo `"),
            ("`` foo`bar ``\n", "foo", "foo`bar", "`` foo`bar ``"),
            ("> ` foo `\n", "foo", "foo", "> ` foo `"),
            ("- ` foo `\n", "foo", "foo", "- ` foo `"),
            ("see ` foo ` now\n", "foo", "see foo now", "see ` foo ` now"),
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let para = first_paragraph(&tree.blocks[0]).expect(source);
            let hidden_layout = layout_of_source(para, source, &RevealState::HIDDEN);
            assert_eq!(
                hidden_layout.text, hidden,
                "caret outside must hide ticks and stripped spaces, {source:?} got {:?}",
                hidden_layout.text
            );
            let at = source.find(body).expect(body);
            let rev = layout_for_caret(source, at);
            assert_eq!(
                rev.text, revealed,
                "caret in the span must reveal ticks and padding, {source:?} got {:?}",
                rev.text
            );
            let vis = rev.text.find(body).expect(body);
            assert_eq!(
                source.as_bytes().get(rev.source_for_visible(vis)).copied(),
                Some(body.as_bytes()[0]),
                "click on body maps onto the word, {source:?}"
            );
        }
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
    fn html_phrasing_reveals_tags_when_caret_intersects() {
        let source = "hello <b>bold</b> world\n";
        let hidden = layout_for(source);
        assert_eq!(hidden.text, "hello bold world");
        assert!(
            !hidden.text.contains('<'),
            "caret outside must hide `<b>`, got {:?}",
            hidden.text
        );
        let vis_h = hidden.text.find('b').expect("hidden bold");
        assert_eq!(
            source
                .as_bytes()
                .get(hidden.source_for_visible(vis_h))
                .copied(),
            Some(b'b'),
            "hidden click on painted bold maps into the word"
        );

        let b = source.find("bold").expect("bold");
        let revealed = layout_for_caret(source, b);
        assert_eq!(
            revealed.text, "hello <b>bold</b> world",
            "caret inside HTML phrasing must reveal tags, got {:?}",
            revealed.text
        );
        assert!(
            revealed
                .runs
                .iter()
                .any(|run| run.font.weight == gpui::FontWeight::BOLD),
            "inner text stays bold when tags reveal, runs={:?}",
            revealed.runs
        );
        let vis_lt = revealed.text.find('<').expect("painted <");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_lt))
                .copied(),
            Some(b'<'),
            "click on painted `<` maps onto the tag"
        );
        let vis_b = revealed.text.find("bold").expect("painted bold");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_b))
                .copied(),
            Some(b'b'),
            "click on painted `bold` still maps onto `b`, not a phantom `<b>`"
        );

        let start = source.find("<b>").expect("open");
        let end = source.find("</b>").expect("close") + "</b>".len();
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let selected = layout_of_source(
            &tree.blocks[0],
            source,
            &RevealState {
                caret: 0,
                selection: start..end,
            },
        );
        assert!(
            selected.text.contains("<b>bold</b>"),
            "selection overlap must reveal HTML tags, got {:?}",
            selected.text
        );

        let nested = "go <b>bold <i>x</i> y</b> now\n";
        assert_eq!(layout_for(nested).text, "go bold x y now");
        let in_i = nested.find('x').expect("x");
        let both = layout_for_caret(nested, in_i);
        assert_eq!(
            both.text, "go <b>bold <i>x</i> y</b> now",
            "caret in nested italic HTML is inside both tags, got {:?}",
            both.text
        );
        let in_b = nested.find("bold").expect("bold");
        let outer = layout_for_caret(nested, in_b);
        assert_eq!(
            outer.text, "go <b>bold x y</b> now",
            "caret in outer `<b>` must not reveal nested `<i>`, got {:?}",
            outer.text
        );
        assert!(outer.text.contains("<b>"));
        assert!(!outer.text.contains("<i>"));

        let anchor = "see <a href=\"https://e.com\">label</a> now\n";
        assert_eq!(layout_for(anchor).text, "see label now");
        let l = anchor.find("label").expect("label");
        let a_rev = layout_for_caret(anchor, l);
        assert!(
            a_rev.text.contains("<a href=\"https://e.com\">label</a>"),
            "caret in an HTML anchor must reveal the tags, got {:?}",
            a_rev.text
        );
        let vis_l = a_rev.text.find("label").expect("painted label");
        assert_eq!(
            anchor
                .as_bytes()
                .get(a_rev.source_for_visible(vis_l))
                .copied(),
            Some(b'l'),
            "click on painted `label` maps into the word"
        );

        let mark = "a <mark>hot</mark> b\n";
        let m = mark.find("hot").expect("hot");
        assert_eq!(layout_for_caret(mark, m).text, "a <mark>hot</mark> b");

        let quoted = "> hello <b>bold</b>\n";
        let q = quoted.find("bold").expect("bold");
        let q_rev = layout_for_caret(quoted, q);
        assert_eq!(
            q_rev.text, "> hello <b>bold</b>",
            "quoted HTML reveals `>` and tags, got {:?}",
            q_rev.text
        );
        let vis_q = q_rev.text.find("bold").expect("quoted bold");
        assert_eq!(
            quoted
                .as_bytes()
                .get(q_rev.source_for_visible(vis_q))
                .copied(),
            Some(b'b'),
            "click on quoted HTML inner text maps onto `b`"
        );

        let listed = "- hello <b>bold</b>\n";
        let lb = listed.find("bold").expect("bold");
        assert_eq!(
            layout_for_caret(listed, lb).text,
            "- hello <b>bold</b>",
            "list HTML reveals `- ` and tags"
        );

        let sub = "H<sub>2</sub>O\n";
        assert!(
            layout_for(sub).text.contains('₂'),
            "hidden sub stays unicode, got {:?}",
            layout_for(sub).text
        );
        let two = sub.find('2').expect("2");
        let sub_rev = layout_for_caret(sub, two);
        assert_eq!(
            sub_rev.text, "H<sub>2</sub>O",
            "caret in `<sub>` must reveal tags and source digits, got {:?}",
            sub_rev.text
        );

        let comment = "a<!-- secret -->b\n";
        assert_eq!(layout_for(comment).text, "ab");
        let after = comment.find('b').expect("b");
        let c_rev = layout_for_caret(comment, after);
        assert!(
            c_rev.text.contains("<!-- secret -->"),
            "caret at the comment boundary must reveal it, got {:?}",
            c_rev.text
        );

        let script = "hello <script>alert(1)</script> world\n";
        let hidden_script = layout_for(script);
        assert!(
            !hidden_script.text.contains("alert") && !hidden_script.text.contains("<script"),
            "script stays hidden, got {:?}",
            hidden_script.text
        );
        let w = script.find("world").expect("world");
        let on_world = layout_for_caret(script, w);
        assert!(
            !on_world.text.contains("<script") && !on_world.text.contains("alert"),
            "script must not reveal on a neighboring caret, got {:?}",
            on_world.text
        );

        let br = "a<br>b\n";
        assert_eq!(layout_for(br).text, "a\nb");
        let br_b = br.find('b').expect("b");
        assert_eq!(
            layout_for_caret(br, br_b).text,
            "a\nb",
            "<br> stays a newline widget, got {:?}",
            layout_for_caret(br, br_b).text
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
        for source in ["Hello[^1]\n\n[^1]: the note\n", "Hello[^1] world\n"] {
            let layout = layout_for(source);
            assert!(
                layout.text.contains('¹') || layout.text.contains('1'),
                "expected a footnote marker, got {:?} in {source:?}",
                layout.text
            );
            assert!(
                !layout.text.contains("[^"),
                "footnote syntax must not paint, got {:?} in {source:?}",
                layout.text
            );
            let vis = layout
                .text
                .find('¹')
                .or_else(|| layout.text.find('1'))
                .expect("painted mark");
            let src = layout.source_for_visible(vis);
            assert_eq!(
                &source[src..src + 1],
                "1",
                "click on painted footnote must map to the label, not `[` / `]`, got {src} in {source:?}"
            );
            assert_ne!(src, source.find('[').expect("["));
        }
    }

    #[test]
    fn footnote_ref_reveals_source_when_caret_intersects() {
        let source = "Hello[^1] world\n";
        let one = source.find('1').expect("1");
        let revealed = layout_for_caret(source, one);
        assert!(
            revealed.text.contains("[^1]"),
            "caret in a footnote ref must reveal `[^1]`, got {:?}",
            revealed.text
        );
        assert!(
            !revealed.text.contains('¹'),
            "superscript must yield to source chrome, got {:?}",
            revealed.text
        );
        let vis_open = revealed.text.find('[').expect("[");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_open))
                .copied(),
            Some(b'['),
            "click on revealed `[` maps onto the opener"
        );
        let vis_one = revealed.text.find('1').expect("1");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_one))
                .copied(),
            Some(b'1'),
            "click on the label still maps onto `1`"
        );

        let h = source.find('H').expect("H");
        let outside = layout_for_caret(source, h);
        assert!(
            !outside.text.contains("[^"),
            "caret on neighboring text must keep footnote chrome hidden, got {:?}",
            outside.text
        );
        assert!(
            outside.text.contains('¹') || outside.text.contains('1'),
            "unfocused footnote ref stays a mark, got {:?}",
            outside.text
        );

        let start = source.find("[^1]").expect("ref");
        let selected = {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let para = tree.blocks.first().expect("para");
            layout_in_tree(
                &tree,
                para,
                source,
                &RevealState {
                    caret: 0,
                    selection: start..start + 4,
                },
            )
        };
        assert!(
            selected.text.contains("[^1]"),
            "selection overlap must reveal the footnote ref, got {:?}",
            selected.text
        );

        let matched = "Hello[^1]\n\n[^1]: the note\n";
        let m1 = matched.find('1').expect("1");
        assert!(
            layout_for_caret(matched, m1).text.contains("[^1]"),
            "matched footnote ref also reveals source, got {:?}",
            layout_for_caret(matched, m1).text
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
    fn definition_details_reveal_colon_when_caret_intersects() {
        let source = "Term\n: details\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let details = first_kind(&tree.blocks, |k| matches!(k, BlockKind::DefinitionDetails))
            .expect("definition details");
        let body = first_paragraph(details).expect("details body");
        let hidden = layout_in_tree(&tree, body, source, &RevealState::HIDDEN);
        assert_eq!(
            hidden.text, "details",
            "caret outside hides `: `, got {:?}",
            hidden.text
        );
        let d = source.find("details").expect("details");
        let revealed = layout_for_caret(source, d);
        assert_eq!(
            revealed.text, ": details",
            "caret in details must reveal `: `, got {:?}",
            revealed.text
        );
        let vis_colon = revealed.text.find(':').expect(":");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_colon))
                .copied(),
            Some(b':'),
            "click on revealed `:` maps onto the marker"
        );
        let vis_d = revealed.text.find('d').expect("d");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_d))
                .copied(),
            Some(b'd'),
            "click on details body maps onto `d`, not a phantom `:`"
        );

        let term_at = source.find("Term").expect("Term");
        let details_while_on_term = layout_in_tree(
            &tree,
            body,
            source,
            &RevealState {
                caret: term_at,
                selection: 0..0,
            },
        );
        assert_eq!(
            details_while_on_term.text, "details",
            "caret on the term must keep `: ` hidden, got {:?}",
            details_while_on_term.text
        );

        let colon = source.find(": details").expect(":");
        let selected = layout_in_tree(
            &tree,
            body,
            source,
            &RevealState {
                caret: 0,
                selection: colon..colon + ": details".len(),
            },
        );
        assert!(
            selected.text.contains(": details"),
            "selection overlap must reveal `: `, got {:?}",
            selected.text
        );

        let quoted = "> Term\n> : details\n";
        let qd = quoted.find("details").expect("details");
        let q_rev = layout_for_caret(quoted, qd);
        assert_eq!(
            q_rev.text, "> : details",
            "quoted details must reveal `> : `, got {:?}",
            q_rev.text
        );
        let vis_gt = q_rev.text.find('>').expect(">");
        assert_eq!(
            quoted
                .as_bytes()
                .get(q_rev.source_for_visible(vis_gt))
                .copied(),
            Some(b'>')
        );
        let vis_qd = q_rev.text.find('d').expect("d");
        assert_eq!(
            quoted
                .as_bytes()
                .get(q_rev.source_for_visible(vis_qd))
                .copied(),
            Some(b'd'),
            "quoted details body maps onto `d`"
        );

        let bold = "Term\n: **bold** details\n";
        let b = bold.find("bold").expect("bold");
        let bold_rev = layout_for_caret(bold, b);
        assert_eq!(
            bold_rev.text, ": **bold** details",
            "intersected details keep wrap marks, got {:?}",
            bold_rev.text
        );

        let two = "One\n: alpha\n\nTwo\n: beta\n";
        let mut ids = IdGen::default();
        let two_tree = import_markdown(two, &mut ids);
        let first_details = first_kind(&two_tree.blocks, |k| {
            matches!(k, BlockKind::DefinitionDetails)
        })
        .expect("first details");
        let first_body = first_paragraph(first_details).expect("first body");
        let beta = two.find("beta").expect("beta");
        let first_while_on_second = layout_in_tree(
            &two_tree,
            first_body,
            two,
            &RevealState {
                caret: beta,
                selection: 0..0,
            },
        );
        assert_eq!(
            first_while_on_second.text, "alpha",
            "unfocused details must keep `: ` hidden, got {:?}",
            first_while_on_second.text
        );
    }

    #[test]
    fn footnote_def_reveals_marker_when_caret_intersects() {
        let source = "See[^1]\n\n[^1]: the note\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let def = tree
            .blocks
            .iter()
            .find(|b| matches!(b.kind, BlockKind::FootnoteDefinition { .. }))
            .expect("footnote def");
        let body = first_paragraph(def).expect("footnote body");
        let hidden = layout_in_tree(&tree, body, source, &RevealState::HIDDEN);
        assert_eq!(
            hidden.text, "the note",
            "caret outside hides `[^1]:`, got {:?}",
            hidden.text
        );
        let t = source.find("the note").expect("the");
        let revealed = layout_for_caret(source, t);
        assert_eq!(
            revealed.text, "[^1]: the note",
            "caret in a footnote def must reveal `[^1]: `, got {:?}",
            revealed.text
        );
        let vis_open = revealed.text.find('[').expect("[");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_open))
                .copied(),
            Some(b'['),
            "click on revealed `[` maps onto the opener"
        );
        let vis_t = revealed.text.find('t').expect("t");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_t))
                .copied(),
            Some(b't'),
            "click on footnote body maps onto `t`, not a phantom `[`"
        );

        let see = source.find("See").expect("See");
        let def_while_on_ref = layout_in_tree(
            &tree,
            body,
            source,
            &RevealState {
                caret: see,
                selection: 0..0,
            },
        );
        assert_eq!(
            def_while_on_ref.text, "the note",
            "caret on the footnote ref must keep def chrome hidden, got {:?}",
            def_while_on_ref.text
        );

        let quoted = "> See[^1]\n\n> [^1]: the note\n";
        let qt = quoted.rfind("the note").expect("the");
        let q_rev = layout_for_caret(quoted, qt);
        assert_eq!(
            q_rev.text, "> [^1]: the note",
            "quoted footnote def must reveal `> [^1]: `, got {:?}",
            q_rev.text
        );

        let note = "See[^note]\n\n[^note]: the note\n";
        let nt = note.find("the note").expect("the");
        assert_eq!(layout_for_caret(note, nt).text, "[^note]: the note");
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
        let layout = build_code_layout("fn x() {}", 10, &style, &theme);
        assert_eq!(layout.text, "fn x() {}");
        assert_eq!(layout.source_for_visible(0), 10);
        assert_eq!(layout.source_for_visible(5), 15);
        assert!(layout.contains_source(10));
        assert!(layout.contains_source(19));
        assert!(!layout.contains_source(9));
        assert!(!layout.contains_source(20));
    }

    fn fenced_code_layout(source: &str) -> LeafLayout {
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let block = first_kind(&tree.blocks, |k| matches!(k, BlockKind::CodeBlock { .. }))
            .expect("code block");
        let body = match &block.kind {
            BlockKind::CodeBlock { literal, .. } => {
                literal.strip_suffix('\n').unwrap_or(literal).to_string()
            }
            _ => unreachable!(),
        };
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.code_font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        build_code_block_layout(&body, source, block, &style, &theme)
    }

    #[test]
    fn quoted_fence_click_maps_past_quote_prefix() {
        let source = "> ```\n> code\n> ```\n";
        let layout = fenced_code_layout(source);
        let c = source.find("code").expect("code");
        let gt = source.find('>').expect(">");
        assert_eq!(layout.text, "code");
        assert_eq!(
            layout.source_for_visible(0),
            c,
            "click x on the first painted body character"
        );
        assert_ne!(
            layout.source_for_visible(0),
            gt,
            "must not equal the `>` byte"
        );
        assert_eq!(layout.visible_for_source(c), 0);
        assert_eq!(&source[layout.source_for_visible(0)..][..1], "c");
    }

    #[test]
    fn list_nested_fence_click_maps_past_indent() {
        let source = "- item\n  ```\n  code\n  ```\n";
        let layout = fenced_code_layout(source);
        let c = source.find("code").expect("code");
        let indent = source.find("  code").expect("indent");
        assert_eq!(layout.text, "code");
        assert_eq!(layout.source_for_visible(0), c);
        assert_ne!(
            layout.source_for_visible(0),
            indent,
            "click on painted `code` must not land on list indent"
        );
        assert_eq!(layout.visible_for_source(c), 0);
    }

    #[test]
    fn unquoted_fence_click_stays_one_to_one() {
        let source = "```\ncode\n```\n";
        let layout = fenced_code_layout(source);
        let c = source.find("code").expect("code");
        assert_eq!(layout.text, "code");
        assert_eq!(layout.source_for_visible(0), c);
        assert_eq!(layout.source_for_visible(2), c + 2);
        assert_eq!(layout.visible_for_source(c), 0);
    }

    #[test]
    fn quoted_list_nested_fence_click_maps_past_quote_and_indent() {
        let source = "> - item\n>   ```\n>   code\n>   ```\n";
        let layout = fenced_code_layout(source);
        let c = source.find("code").expect("code");
        let gt = source.rfind(">   code").expect("quoted body line");
        assert_eq!(layout.text, "code");
        assert_eq!(layout.source_for_visible(0), c);
        assert_ne!(layout.source_for_visible(0), gt);
        assert_eq!(layout.visible_for_source(c), 0);
    }

    fn html_block_layout(raw: &str) -> LeafLayout {
        let source = if raw.ends_with('\n') {
            raw.to_string()
        } else {
            format!("{raw}\n")
        };
        html_block_layout_from_source(&source)
    }

    fn html_block_layout_from_source(source: &str) -> LeafLayout {
        html_block_layout_from_source_reveal(source, &RevealState::HIDDEN)
    }

    fn html_block_layout_from_source_reveal(source: &str, reveal: &RevealState) -> LeafLayout {
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let block = first_kind(&tree.blocks, |k| matches!(k, BlockKind::Opaque { .. }))
            .expect("html block");
        let raw = match &block.kind {
            BlockKind::Opaque { raw } => raw.as_str(),
            _ => unreachable!(),
        };
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
                &text, &source_at, &runs, source, block, &style, &theme, reveal,
            ),
            other => panic!("expected flow, got {other:?}"),
        }
    }

    fn html_block_layout_or_source(
        source: &str,
        block: &Block,
        style: &TextStyle,
        theme: &EditorTheme,
        reveal: &RevealState,
    ) -> LeafLayout {
        layout_html_block(source, block, style, theme, reveal)
    }

    #[test]
    fn quoted_html_click_maps_past_quote_prefix() {
        let source = "> <div>\n> x\n> </div>\n";
        let layout = html_block_layout_from_source(source);
        let x = source.find('x').expect("x");
        let gt = source.find('>').expect(">");
        assert!(
            layout.text.contains('x'),
            "HTML body must paint, got {:?}",
            layout.text
        );
        assert!(
            !layout.text.contains('>'),
            "quote prefix must not paint, got {:?}",
            layout.text
        );
        let vis = layout.visible_for_source(x);
        assert_eq!(
            layout.source_for_visible(vis),
            x,
            "click on painted `x` must map to the `x` byte"
        );
        assert_ne!(
            layout.source_for_visible(0),
            gt,
            "first painted HTML body character must not be the `>` byte"
        );
        assert_eq!(&source[layout.source_for_visible(vis)..][..1], "x");
    }

    #[test]
    fn unquoted_html_click_stays_on_body() {
        let source = "<div>\nx\n</div>\n";
        let layout = html_block_layout_from_source(source);
        let x = source.find('x').expect("x");
        let vis = layout.visible_for_source(x);
        assert_eq!(layout.source_for_visible(vis), x);
        assert_ne!(layout.source_for_visible(0), source.find('<').expect("<"));
    }

    #[test]
    fn quoted_list_nested_html_click_maps_past_quote_and_indent() {
        let source = "> - item\n>\n>   <div>\n>   x\n>   </div>\n";
        let layout = html_block_layout_from_source(source);
        let x = source.find('x').expect("x");
        let gt = source.find('>').expect(">");
        let vis = layout.visible_for_source(x);
        assert_eq!(layout.source_for_visible(vis), x);
        assert_ne!(
            layout.source_for_visible(0),
            gt,
            "first painted HTML body character must not be the `>` byte"
        );
    }

    #[test]
    fn quoted_paragraph_click_maps_past_quote_prefix() {
        let source = "> hello\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let para = first_paragraph(&tree.blocks[0]).expect("quote paragraph");
        let layout = layout_of(para);
        let h = source.find('h').expect("h");
        let gt = source.find('>').expect(">");
        assert_eq!(layout.text, "hello");
        assert_eq!(
            layout.source_for_visible(0),
            h,
            "click on first painted character must be `h`"
        );
        assert_ne!(layout.source_for_visible(0), gt);
        assert_eq!(&source[layout.source_for_visible(0)..][..1], "h");
    }

    #[test]
    fn quoted_wrapped_paragraph_click_skips_continuation_quote() {
        let source = "> hello\n> world\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let para = first_paragraph(&tree.blocks[0]).expect("quote paragraph");
        let layout = layout_of(para);
        let w = source.find("world").expect("world");
        let gt = source.rfind("> world").expect("continuation");
        assert!(
            layout.text.contains("hello") && layout.text.contains("world"),
            "both lines must paint, got {:?}",
            layout.text
        );
        let vis = layout.visible_for_source(w);
        assert_eq!(
            layout.source_for_visible(vis),
            w,
            "click on painted `w` must map to `w`, not `>`"
        );
        assert_ne!(layout.source_for_visible(vis), gt);
    }

    #[test]
    fn list_item_click_maps_past_marker() {
        let source = "- hello\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let para = first_paragraph(&tree.blocks[0]).expect("list paragraph");
        let layout = layout_of(para);
        let h = source.find('h').expect("h");
        let dash = source.find('-').expect("-");
        assert_eq!(layout.text, "hello");
        assert_eq!(layout.source_for_visible(0), h);
        assert_ne!(layout.source_for_visible(0), dash);
    }

    #[test]
    fn quoted_list_click_maps_past_quote_and_marker() {
        let source = "> - hello\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let para = first_paragraph(&tree.blocks[0]).expect("quoted list paragraph");
        let layout = layout_of(para);
        let h = source.find('h').expect("h");
        assert_eq!(layout.text, "hello");
        assert_eq!(layout.source_for_visible(0), h);
        assert_ne!(&source[layout.source_for_visible(0)..][..1], ">");
        assert_ne!(&source[layout.source_for_visible(0)..][..1], "-");
    }

    #[test]
    fn nested_quote_click_maps_past_inner_marker() {
        let source = "> > hello\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let para = first_paragraph(&tree.blocks[0]).expect("nested quote paragraph");
        let layout = layout_of(para);
        let h = source.find('h').expect("h");
        assert_eq!(layout.text, "hello");
        assert_eq!(
            layout.source_for_visible(0),
            h,
            "first painted character must be `h`, not inner `>`"
        );
        assert_ne!(&source[layout.source_for_visible(0)..][..1], ">");
    }

    #[test]
    fn ordered_list_click_maps_past_marker() {
        let source = "1. hello\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let para = first_paragraph(&tree.blocks[0]).expect("ordered paragraph");
        let layout = layout_of(para);
        let h = source.find('h').expect("h");
        assert_eq!(layout.text, "hello");
        assert_eq!(layout.source_for_visible(0), h);
        assert_ne!(&source[layout.source_for_visible(0)..][..1], "1");
    }

    #[test]
    fn unchecked_task_click_maps_past_checkbox() {
        let source = "- [ ] hello\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let para = first_paragraph(&tree.blocks[0]).expect("task paragraph");
        let layout = layout_of(para);
        let h = source.find('h').expect("h");
        assert_eq!(layout.text, "hello");
        assert_eq!(layout.source_for_visible(0), h);
        assert_ne!(&source[layout.source_for_visible(0)..][..1], "-");
        assert_ne!(&source[layout.source_for_visible(0)..][..1], "[");
    }

    #[test]
    fn list_item_link_with_x_label_click_maps_to_label_not_dest() {
        let source = "- [x](https://e.com)\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let para = first_paragraph(&tree.blocks[0]).expect("list paragraph");
        let layout = layout_of(para);
        let x = source.find("[x]").expect("label") + 1;
        assert_eq!(layout.text, "x");
        assert_eq!(
            layout.source_for_visible(0),
            x,
            "painted label must map onto `x`, not dest, got {:?}",
            source
                .get(layout.source_for_visible(0)..layout.source_for_visible(0).saturating_add(1))
        );
        assert_ne!(&source[layout.source_for_visible(0)..][..1], "(");
        assert_ne!(&source[layout.source_for_visible(0)..][..1], "h");
        assert_ne!(&source[layout.source_for_visible(0)..][..1], "-");
    }

    #[test]
    fn list_item_shortcut_ref_x_at_eol_click_maps_to_label() {
        let source = "- [x]\n\n[x]: https://e.com\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let para = first_paragraph(&tree.blocks[0]).expect("list paragraph");
        let layout = layout_of(para);
        let x = source.find("[x]").expect("label") + 1;
        assert_eq!(layout.text, "x");
        assert_eq!(
            layout.source_for_visible(0),
            x,
            "painted shortcut-ref must map onto `x`, not past `]`, got {:?}",
            source
                .get(layout.source_for_visible(0)..layout.source_for_visible(0).saturating_add(1))
        );
        assert_ne!(&source[layout.source_for_visible(0)..][..1], "]");
        assert_ne!(&source[layout.source_for_visible(0)..][..1], "-");
    }

    #[test]
    fn list_item_nested_reference_link_click_maps_to_label() {
        for source in [
            "- [hello][ref]\n  [ref]: https://e.com\n",
            "> - [hello][ref]\n>   [ref]: https://e.com\n",
            "- [x] [hello][ref]\n  [ref]: https://e.com\n",
            "- [**hello**][ref]\n  [ref]: https://e.com\n",
            "- outer\n  - [hello][ref]\n    [ref]: https://e.com\n",
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let para = find_paragraph_with(&tree.blocks, "hello").expect("link paragraph");
            let layout = layout_of_source(para, source, &RevealState::HIDDEN);
            let h = source.find("hello").expect("hello");
            assert_eq!(
                layout.text, "hello",
                "nested `[hello][ref]` must paint the label, got {:?} in {source:?}",
                layout.text
            );
            assert!(
                !layout.text.contains('[') && !layout.text.contains(']'),
                "nested reference chrome must not paint, got {:?} in {source:?}",
                layout.text
            );
            let mapped = layout.source_for_visible(0);
            assert_eq!(
                mapped,
                h,
                "click on nested reference link must be `h`, {source:?} got {mapped} {:?}",
                source.get(mapped..mapped.saturating_add(1))
            );
        }

        let task = "- [x] done\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(task, &mut ids);
        let para = first_paragraph(&tree.blocks[0]).expect("task paragraph");
        let layout = layout_of(para);
        assert_eq!(layout.text, "done");
        assert_eq!(layout.source_for_visible(0), task.find('d').expect("d"));

        for source in [
            "- [![cat](a.png)][ref]\n  [ref]: https://e.com\n",
            "- outer\n  - [![cat](a.png)][ref]\n    [ref]: https://e.com\n",
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let para = find_paragraph_with(&tree.blocks, "cat").expect("image paragraph");
            let layout = layout_of_source(para, source, &RevealState::HIDDEN);
            assert!(
                !layout.text.contains('[') && !layout.text.contains("ref"),
                "wrapping dest `[ref]` must not paint as a shortcut, got {:?} in {source:?}",
                layout.text
            );
        }
    }

    #[test]
    fn empty_quote_click_homes_after_prefix() {
        let source = "> ";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let blank = tree.empty_prefix_homes.first().expect("empty quote home");
        assert_eq!(blank.home, 2);
        let layout = build_blank_gap_layout(blank.home..blank.home);
        assert_eq!(layout.source_for_visible(0), 2);
        assert_ne!(layout.source_for_visible(0), source.find('>').expect(">"));
    }

    #[test]
    fn empty_list_click_homes_after_marker() {
        let source = "- ";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let blank = tree.empty_prefix_homes.first().expect("empty list home");
        assert_eq!(blank.home, 2);
        let layout = build_blank_gap_layout(blank.home..blank.home);
        assert_eq!(layout.source_for_visible(0), 2);
        assert_ne!(layout.source_for_visible(0), source.find('-').expect("-"));
    }

    #[test]
    fn empty_footnote_def_click_homes_after_marker() {
        let source = "Hello[^1]\n\n[^1]: ";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let blank = tree
            .empty_prefix_homes
            .iter()
            .find(|h| source.get(h.line.clone()).is_some_and(|l| l.contains("]:")))
            .expect("empty footnote def home");
        assert_ne!(
            source.as_bytes().get(blank.home).copied(),
            Some(b'['),
            "click home must sit after `[^1]: `, home={}",
            blank.home
        );
        let layout = build_blank_gap_layout(blank.home..blank.home);
        assert_eq!(layout.source_for_visible(0), blank.home);
        assert_ne!(
            layout.source_for_visible(0),
            source.rfind("[^").expect("def")
        );
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        let hidden = build_prefix_blank_layout(source, blank, &RevealState::HIDDEN, &style, &theme);
        assert!(
            !hidden.text.contains("[^"),
            "empty def hides `[^1]:` when caret is elsewhere, got {:?}",
            hidden.text
        );
        let revealed = build_prefix_blank_layout(
            source,
            blank,
            &RevealState {
                caret: blank.home,
                selection: 0..0,
            },
            &style,
            &theme,
        );
        assert!(
            revealed.text.contains("[^1]:"),
            "empty `[^1]: ` must reveal the opener, got {:?}",
            revealed.text
        );
        let vis_open = revealed.text.find('[').expect("[");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_open))
                .copied(),
            Some(b'[')
        );
        assert_eq!(
            revealed.source_for_visible(revealed.text.len()),
            blank.home,
            "click after revealed chrome still homes after the marker"
        );
    }

    #[test]
    fn empty_definition_details_reveal_colon_when_caret_intersects() {
        let source = "Term\n: ";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let blank = tree
            .empty_prefix_homes
            .iter()
            .find(|h| {
                source.get(h.line.clone()).is_some_and(|line| {
                    let after = line.trim_start_matches(['>', ' ', '\t']);
                    after.starts_with(':')
                })
            })
            .expect("empty details home");
        assert_ne!(
            source.as_bytes().get(blank.home).copied(),
            Some(b':'),
            "click home must sit after `: `, home={}",
            blank.home
        );
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        let hidden = build_prefix_blank_layout(source, blank, &RevealState::HIDDEN, &style, &theme);
        assert!(
            !hidden.text.contains(':'),
            "empty `: ` hides the marker when caret is elsewhere, got {:?}",
            hidden.text
        );
        let revealed = build_prefix_blank_layout(
            source,
            blank,
            &RevealState {
                caret: blank.home,
                selection: 0..0,
            },
            &style,
            &theme,
        );
        assert!(
            revealed.text.starts_with(": "),
            "empty `: ` must reveal the opener, got {:?}",
            revealed.text
        );
        let vis_colon = revealed.text.find(':').expect(":");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_colon))
                .copied(),
            Some(b':')
        );
        assert_eq!(
            revealed.source_for_visible(revealed.text.len()),
            blank.home,
            "click after revealed `: ` still homes after the marker"
        );

        let quoted = "> Term\n> : ";
        let mut ids = IdGen::default();
        let qtree = import_markdown(quoted, &mut ids);
        let qblank = qtree
            .empty_prefix_homes
            .iter()
            .find(|h| {
                quoted.get(h.line.clone()).is_some_and(|line| {
                    let after = line.trim_start_matches(['>', ' ', '\t']);
                    after.starts_with(':')
                })
            })
            .expect("quoted empty details");
        let qrev = build_prefix_blank_layout(
            quoted,
            qblank,
            &RevealState {
                caret: qblank.home,
                selection: 0..0,
            },
            &style,
            &theme,
        );
        assert!(
            qrev.text.contains("> :"),
            "quoted empty details must reveal `> : `, got {:?}",
            qrev.text
        );
    }

    #[test]
    fn wrapped_list_continuation_click_maps_to_world() {
        let source = "- hello\n  world\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let para = first_paragraph(&tree.blocks[0]).expect("list paragraph");
        let layout = layout_of(para);
        let w = source.find("world").expect("world");
        let vis = layout.visible_for_source(w);
        assert_eq!(layout.source_for_visible(vis), w);
        assert_ne!(
            &source[layout.source_for_visible(vis)..][..1],
            " ",
            "click on painted `w` must not be continuation indent"
        );
    }

    #[test]
    fn unquoted_paragraph_click_stays_one_to_one() {
        let layout = layout_for("hello\n");
        assert_eq!(layout.text, "hello");
        assert_eq!(layout.source_for_visible(0), 0);
        assert_eq!(layout.source_for_visible(1), 1);
        assert_eq!(layout.source_for_visible(4), 4);
    }

    #[test]
    fn quoted_soft_break_click_maps_to_newline_not_quote() {
        let source = "> hello\n> world\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let para = first_paragraph(&tree.blocks[0]).expect("quote paragraph");
        let layout = layout_of(para);
        assert!(
            layout.text.contains("hello") && layout.text.contains("world"),
            "both lines must paint, got {:?}",
            layout.text
        );
        let space = layout.text.find(' ').expect("painted soft-break space");
        let mapped = layout.source_for_visible(space);
        assert_eq!(
            source.as_bytes().get(mapped).copied(),
            Some(b'\n'),
            "click on the wrap space must be the newline, got {mapped} {:?}",
            source.get(mapped..mapped.saturating_add(1))
        );
        assert_ne!(
            &source[mapped..mapped + 1],
            ">",
            "soft break must not map onto `>`"
        );
        assert_ne!(
            mapped, layout.block_start,
            "soft break must not map onto the paragraph start"
        );
    }

    #[test]
    fn list_soft_break_click_maps_to_newline_not_marker() {
        let source = "- hello\n  world\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let para = first_paragraph(&tree.blocks[0]).expect("list paragraph");
        let layout = layout_of(para);
        let space = layout.text.find(' ').expect("painted soft-break space");
        let mapped = layout.source_for_visible(space);
        assert_eq!(
            source.as_bytes().get(mapped).copied(),
            Some(b'\n'),
            "click on the wrap space must be the newline, got {mapped} {:?}",
            source.get(mapped..mapped.saturating_add(1))
        );
        assert_ne!(&source[mapped..mapped + 1], "-");
        assert_ne!(mapped, layout.block_start);
    }

    #[test]
    fn heading_click_maps_to_title_not_hash() {
        let source = "# Title\n";
        let layout = layout_for(source);
        let t = source.find('T').expect("T");
        assert_eq!(layout.text, "Title");
        assert_eq!(layout.source_for_visible(0), t);
        assert_ne!(&source[layout.source_for_visible(0)..][..1], "#");
    }

    #[test]
    fn table_cell_click_stays_on_cell_text() {
        let source = "| a | b |\n| - | - |\n| c | d |\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let cell = first_kind(&tree.blocks, |k| matches!(k, BlockKind::TableCell)).expect("cell");
        let layout = layout_of(cell);
        let a = source.find('a').expect("a");
        assert_eq!(layout.text, "a");
        assert_eq!(layout.source_for_visible(0), a);
        assert!(!layout.text.contains('|'));
    }

    #[test]
    fn compact_gfm_table_cell_click_stays_on_cell_text() {
        let source = "foo|bar\n---|---\nbaz|bim\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let cell = first_kind(&tree.blocks, |k| matches!(k, BlockKind::TableCell)).expect("cell");
        let layout = layout_of(cell);
        let f = source.find('f').expect("f");
        assert_eq!(layout.text, "foo");
        assert_eq!(layout.source_for_visible(0), f);
        assert!(!layout.text.contains('|'));
        let revealed = layout_for_caret(source, f);
        assert!(
            revealed.text.contains("foo"),
            "caret in a compact cell must paint the cell, got {:?}",
            revealed.text
        );
        let vis_f = revealed.text.find('f').expect("painted f");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_f))
                .copied(),
            Some(b'f'),
            "click on compact cell text still maps onto `f`"
        );
    }

    #[test]
    fn indented_code_click_maps_past_indent() {
        let source = "    code\n";
        let layout = fenced_code_layout(source);
        let c = source.find("code").expect("code");
        let indent = source.find("    code").expect("indent");
        assert_eq!(layout.text, "code");
        assert_eq!(layout.source_for_visible(0), c);
        assert_ne!(
            layout.source_for_visible(0),
            indent,
            "click on painted `code` must not land on the indent"
        );
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
        let block = leaf_at(&tree.blocks, caret)
            .or_else(|| tree.blocks.first())
            .expect("block");
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        build_leaf_layout_revealed(
            block,
            source,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            &RevealState {
                caret,
                selection: 0..0,
            },
            &chrome_hosts_for(&tree, block.id),
        )
    }

    fn leaf_at(blocks: &[Block], caret: usize) -> Option<&Block> {
        for b in blocks {
            if caret < b.source_range.start || caret > b.source_range.end {
                continue;
            }
            if let Some(inner) = leaf_at(&b.children, caret) {
                return Some(inner);
            }
            if !b.inlines.is_empty()
                || matches!(
                    b.kind,
                    BlockKind::Paragraph
                        | BlockKind::Heading { .. }
                        | BlockKind::TableCell
                        | BlockKind::CodeBlock { .. }
                )
            {
                return Some(b);
            }
        }
        None
    }

    fn layout_in_tree(
        tree: &RichTree,
        block: &Block,
        source: &str,
        reveal: &RevealState,
    ) -> LeafLayout {
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        build_leaf_layout_revealed(
            block,
            source,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            reveal,
            &chrome_hosts_for(tree, block.id),
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
            source,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            &RevealState {
                caret: 0,
                selection: start..start + 7,
            },
            &ChromeHosts::NONE,
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
    fn autolink_hides_angle_brackets_unless_caret_intersects() {
        let source = "see <https://example.com> here\n";
        let hidden = layout_for(source);
        assert_eq!(hidden.text, "see https://example.com here");
        assert!(
            !hidden.text.contains('<') && !hidden.text.contains('>'),
            "autolink `<>` must not paint when caret is outside, got {:?}",
            hidden.text
        );
        let inside = source.find("example").expect("host");
        let revealed = layout_for_caret(source, inside);
        assert!(
            revealed.text.contains("<https://example.com>"),
            "caret inside an autolink must reveal `<>`, got {:?}",
            revealed.text
        );
        assert_eq!(revealed.text, "see <https://example.com> here");
        let open = source.find('<').expect("<");
        let on_bracket = layout_for_caret(source, open);
        assert!(
            on_bracket.text.contains("<https://example.com>"),
            "caret on `<` must reveal, got {:?}",
            on_bracket.text
        );
        let vis_lt = revealed.text.find('<').expect("painted <");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_lt))
                .copied(),
            Some(b'<'),
            "revealed `<` maps onto the source bracket"
        );
        let vis_h = revealed.text.find("https").expect("https");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_h))
                .copied(),
            Some(b'h'),
            "revealed URL still maps onto `h`"
        );
        let start = source.find("<https://example.com>").expect("span");
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
            source,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            &RevealState {
                caret: 0,
                selection: start..start + "<https://example.com>".len(),
            },
            &ChromeHosts::NONE,
        );
        assert!(
            selected.text.contains("<https://example.com>"),
            "selection overlap must reveal `<>`, got {:?}",
            selected.text
        );
    }

    #[test]
    fn email_autolink_reveals_angle_brackets_when_caret_intersects() {
        let source = "see <user@example.com> now\n";
        let hidden = layout_for(source);
        assert!(
            hidden.text.contains("user@example.com")
                && !hidden.text.contains('<')
                && !hidden.text.contains('>'),
            "email autolink `<>` must not paint when caret is outside, got {:?}",
            hidden.text
        );
        let inside = source.find("user").expect("user");
        let revealed = layout_for_caret(source, inside);
        assert!(
            revealed.text.contains("<user@example.com>"),
            "caret inside an email autolink must reveal `<>`, got {:?}",
            revealed.text
        );
    }

    #[test]
    fn bare_url_autolink_does_not_invent_angle_brackets() {
        let source = "see https://example.com now\n";
        let hidden = layout_for(source);
        assert!(
            hidden.text.contains("https://example.com"),
            "bare URL must paint, got {:?}",
            hidden.text
        );
        assert!(
            !hidden.text.contains('<') && !hidden.text.contains('>'),
            "bare GFM autolink has no `<>`, got {:?}",
            hidden.text
        );
        let inside = source.find("example").expect("host");
        let revealed = layout_for_caret(source, inside);
        assert!(
            !revealed.text.contains('<') && !revealed.text.contains('>'),
            "caret inside a bare URL must not invent `<>`, got {:?}",
            revealed.text
        );
        let vis = hidden.text.find("https").expect("painted url");
        assert_eq!(
            source
                .as_bytes()
                .get(hidden.source_for_visible(vis))
                .copied(),
            Some(b'h'),
            "click on a bare GFM autolink must map onto the URL, not the paragraph start"
        );
    }

    #[test]
    fn www_autolink_click_maps_onto_the_url_not_paragraph_start() {
        for source in [
            "see www.example.com now\n",
            "> www.example.com\n",
            "- www.example.com\n",
            "see user@example.com now\n",
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let block = first_paragraph(&tree.blocks[0]).unwrap_or(&tree.blocks[0]);
            let layout = layout_of_source(block, source, &RevealState::HIDDEN);
            let needle = if source.contains("user@") {
                "user@"
            } else {
                "www."
            };
            let vis = layout
                .text
                .find(needle)
                .unwrap_or_else(|| panic!("painted {needle} in {:?}", layout.text));
            let mapped = layout.source_for_visible(vis);
            assert_eq!(
                source.get(mapped..mapped + needle.len()).unwrap_or(""),
                needle,
                "click must map onto {needle}, {source:?} got {mapped} {:?}",
                source.get(mapped..mapped.saturating_add(1))
            );
            if source.as_bytes().first() == Some(&b'>') {
                assert_ne!(mapped, 0, "must not map onto `>`, {source:?}");
            }
            if source.as_bytes().first() == Some(&b'-') {
                assert_ne!(mapped, 0, "must not map onto `-`, {source:?}");
            }
            if source.starts_with("see ") {
                assert!(mapped > 0, "must not map onto paragraph start, {source:?}");
            }
            assert!(
                !layout.text.contains('<') && !layout.text.contains('>'),
                "must not invent `<>`, {source:?} got {:?}",
                layout.text
            );
        }
    }

    #[test]
    fn quoted_autolink_hides_and_reveals_angle_brackets() {
        let source = "> see <https://example.com>\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let para = first_paragraph(&tree.blocks[0]).expect("quote paragraph");
        let hidden = layout_of(para);
        assert!(
            hidden.text.contains("https://example.com")
                && !hidden.text.contains('<')
                && !hidden.text.contains('>'),
            "quoted autolink `<>` must not paint, got {:?}",
            hidden.text
        );
        let inside = source.find("example").expect("host");
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        let revealed = build_leaf_layout_revealed(
            para,
            source,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            &RevealState {
                caret: inside,
                selection: 0..0,
            },
            &ChromeHosts::NONE,
        );
        assert!(
            revealed.text.contains("<https://example.com>"),
            "caret inside a quoted autolink must reveal `<>`, got {:?}",
            revealed.text
        );
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
    fn multiline_display_math_hides_dollars_and_wrapping_newlines() {
        let src = "$$\nE=mc^2\n$$\n";
        let layout = layout_for(src);
        assert!(
            !layout.text.contains("$$"),
            "display delimiters must not paint, got {:?}",
            layout.text
        );
        assert!(
            !layout.text.starts_with('\n'),
            "wrapping newline after `$$` must not paint, got {:?}",
            layout.text
        );
        assert!(
            layout.text.contains("E=mc^2"),
            "expected formula body, got {:?}",
            layout.text
        );
        let vis = layout.text.find('E').expect("painted E");
        let mapped = layout.source_for_visible(vis);
        assert_eq!(
            src.as_bytes().get(mapped).copied(),
            Some(b'E'),
            "click on painted formula must be `E`, not `$$` / `\\n`, got {mapped}"
        );
        assert_ne!(mapped, src.find("$$").expect("$$"));
        let revealed = layout_for_caret(src, src.find('E').expect("E"));
        assert!(
            revealed.text.contains("$$"),
            "caret inside multiline display math must reveal $$, got {:?}",
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

    #[test]
    fn html_block_reveals_div_tags_when_caret_intersects() {
        let source = "<div>\n**bold** and `code`\n</div>\n";
        let hidden = html_block_layout_from_source(source);
        assert!(
            hidden.text.contains("bold"),
            "inner markdown must still paint, got {:?}",
            hidden.text
        );
        assert!(
            !hidden.text.contains("<div") && !hidden.text.contains("</div>"),
            "caret outside must hide wrapper tags, got {:?}",
            hidden.text
        );
        assert!(
            hidden
                .runs
                .iter()
                .any(|run| run.font.weight == gpui::FontWeight::BOLD),
            "inner bold stays when tags are hidden, runs={:?}",
            hidden.runs
        );
        let vis_b = hidden.text.find("bold").expect("hidden bold");
        assert_eq!(
            source
                .as_bytes()
                .get(hidden.source_for_visible(vis_b))
                .copied(),
            Some(b'b'),
            "hidden click on painted bold maps into the word"
        );

        let b = source.find("bold").expect("bold");
        let revealed = html_block_layout_from_source_reveal(
            source,
            &RevealState {
                caret: b,
                selection: 0..0,
            },
        );
        assert!(
            revealed.text.contains("<div>") && revealed.text.contains("</div>"),
            "caret in the HTML block must reveal wrapper tags, got {:?}",
            revealed.text
        );
        assert!(
            revealed.text.contains("bold") && !revealed.text.contains('*'),
            "inner markdown still paints as rich text, got {:?}",
            revealed.text
        );
        assert!(
            revealed
                .runs
                .iter()
                .any(|run| run.font.weight == gpui::FontWeight::BOLD),
            "inner bold stays when tags reveal, runs={:?}",
            revealed.runs
        );
        let vis_lt = revealed.text.find('<').expect("painted <");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_lt))
                .copied(),
            Some(b'<'),
            "click on revealed `<div>` maps onto the tag"
        );
        let vis_b = revealed.text.find("bold").expect("painted bold");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis_b))
                .copied(),
            Some(b'b'),
            "click on painted `bold` still maps onto `b`, not a phantom `<div>`"
        );
    }

    #[test]
    fn html_block_comment_reveals_when_caret_intersects() {
        let source = "<!-- secret -->\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let block = first_kind(&tree.blocks, |k| matches!(k, BlockKind::Opaque { .. }))
            .expect("comment block");
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        let hidden =
            html_block_layout_or_source(source, block, &style, &theme, &RevealState::HIDDEN);
        assert!(
            !hidden.text.contains("secret") && !hidden.text.contains("<!--"),
            "caret outside must hide a comment block, got {:?}",
            hidden.text
        );
        let caret = source.find("secret").expect("secret");
        let revealed = html_block_layout_or_source(
            source,
            block,
            &style,
            &theme,
            &RevealState {
                caret,
                selection: 0..0,
            },
        );
        assert!(
            revealed.text.contains("<!--") && revealed.text.contains("secret"),
            "caret in an HTML-block comment must reveal source, got {:?}",
            revealed.text
        );
        let vis = revealed.text.find('<').expect("painted <");
        assert_eq!(
            source
                .as_bytes()
                .get(revealed.source_for_visible(vis))
                .copied(),
            Some(b'<'),
            "click on a revealed comment maps onto the source"
        );

        for wrapped in ["> <!-- secret -->\n", "- <!-- secret -->\n"] {
            let mut ids = IdGen::default();
            let tree = import_markdown(wrapped, &mut ids);
            let block = first_kind(&tree.blocks, |k| matches!(k, BlockKind::Opaque { .. }))
                .expect("comment block");
            let caret = wrapped.find("secret").expect("secret");
            let revealed = html_block_layout_or_source(
                wrapped,
                block,
                &style,
                &theme,
                &RevealState {
                    caret,
                    selection: 0..0,
                },
            );
            assert!(
                revealed.text.contains("<!--") && revealed.text.contains("secret"),
                "quoted/list HTML-block comments must reveal, {wrapped:?} got {:?}",
                revealed.text
            );
        }
    }

    #[test]
    fn html_block_pi_and_cdata_reveal_when_caret_intersects() {
        for source in [
            "<?php if ($a > $b) echo 1; ?>\n",
            "<![CDATA[a > b]]>\n",
            "> <?php if ($a > $b) echo 1; ?>\n",
            "- <![CDATA[a > b]]>\n",
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let block = first_kind(&tree.blocks, |k| matches!(k, BlockKind::Opaque { .. }))
                .expect("html chrome block");
            let theme = EditorTheme::dark();
            let style = TextStyle {
                color: theme.text,
                font_family: theme.font_family.clone().into(),
                font_size: px(theme.font_size).into(),
                ..Default::default()
            };
            let hidden =
                html_block_layout_or_source(source, block, &style, &theme, &RevealState::HIDDEN);
            assert!(
                !hidden.text.contains("php")
                    && !hidden.text.contains("CDATA")
                    && !hidden.text.contains('$'),
                "caret outside must hide PI/CDATA, {source:?} got {:?}",
                hidden.text
            );
            let caret = source
                .find(" > ")
                .map(|i| i + 1)
                .unwrap_or(block.source_range.start);
            let revealed = html_block_layout_or_source(
                source,
                block,
                &style,
                &theme,
                &RevealState {
                    caret,
                    selection: 0..0,
                },
            );
            assert!(
                revealed.text.contains("<?") || revealed.text.contains("CDATA"),
                "caret in PI/CDATA must reveal source, {source:?} got {:?}",
                revealed.text
            );
            assert!(
                revealed.text.contains('>'),
                "revealed source must keep the inner `>`, {source:?} got {:?}",
                revealed.text
            );
            let vis = revealed
                .text
                .find('<')
                .or_else(|| revealed.text.find('C'))
                .expect("painted opener");
            let mapped = revealed.source_for_visible(vis);
            let ch = source.as_bytes().get(mapped).copied();
            assert!(
                ch == Some(b'<') || ch == Some(b'C'),
                "click on revealed PI/CDATA maps onto source, {source:?} got {ch:?}"
            );
        }
    }

    #[test]
    fn html_block_empty_div_reveals_tags_when_caret_intersects() {
        let source = "<div></div>\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let block =
            first_kind(&tree.blocks, |k| matches!(k, BlockKind::Opaque { .. })).expect("div");
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        let hidden =
            html_block_layout_or_source(source, block, &style, &theme, &RevealState::HIDDEN);
        assert!(
            !hidden.text.contains("<div") && !hidden.text.contains("</div>"),
            "caret outside must hide empty wrapper tags, got {:?}",
            hidden.text
        );
        let revealed = html_block_layout_or_source(
            source,
            block,
            &style,
            &theme,
            &RevealState {
                caret: 0,
                selection: 0..0,
            },
        );
        assert!(
            revealed.text.contains("<div") && revealed.text.contains("</div>"),
            "caret in an empty `<div>` must reveal tags, got {:?}",
            revealed.text
        );
    }

    #[test]
    fn html_block_script_stays_hidden_when_caret_intersects() {
        let source = "<script>alert(1)</script>\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let block =
            first_kind(&tree.blocks, |k| matches!(k, BlockKind::Opaque { .. })).expect("script");
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        let caret = source.find("alert").expect("alert");
        let revealed = html_block_layout_or_source(
            source,
            block,
            &style,
            &theme,
            &RevealState {
                caret,
                selection: 0..0,
            },
        );
        assert!(
            !revealed.text.contains("alert") && !revealed.text.contains("<script"),
            "script blocks must stay hidden even when the caret intersects, got {:?}",
            revealed.text
        );
    }

    #[test]
    fn html_block_iframe_stays_hidden_when_caret_intersects() {
        for source in [
            "<iframe src=\"https://e.com\"></iframe>\n",
            "<iframe src=\"https://e.com\"><p>nested</p></iframe>\n",
            "- <iframe src=\"https://e.com\"></iframe>\n",
            "<noframes>fallback</noframes>\n",
            "<noembed>\nfallback\n</noembed>\n",
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let block = first_kind(&tree.blocks, |k| matches!(k, BlockKind::Opaque { .. }))
                .expect("iframe-like");
            let theme = EditorTheme::dark();
            let style = TextStyle {
                color: theme.text,
                font_family: theme.font_family.clone().into(),
                font_size: px(theme.font_size).into(),
                ..Default::default()
            };
            let caret = source
                .find("nested")
                .or_else(|| source.find("fallback"))
                .or_else(|| source.find("e.com"))
                .unwrap_or(block.source_range.start);
            let revealed = html_block_layout_or_source(
                source,
                block,
                &style,
                &theme,
                &RevealState {
                    caret,
                    selection: 0..0,
                },
            );
            assert!(
                !revealed.text.contains("nested")
                    && !revealed.text.contains("fallback")
                    && !revealed.text.contains("iframe")
                    && !revealed.text.contains("<p")
                    && !revealed.text.contains("e.com"),
                "iframe/noembed/noframes must stay hidden (no nested document), {source:?} got {:?}",
                revealed.text
            );
        }
    }

    #[test]
    fn html_block_title_xmp_reveal_source_when_caret_intersects() {
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        for source in [
            "<title>Doc title</title>\n",
            "<xmp>\nraw <b>html</b>\n</xmp>\n",
            "<plaintext>\nraw text\n",
            "> <title>Doc title</title>\n",
            "> <xmp>\n> raw\n> </xmp>\n",
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let block = first_kind(&tree.blocks, |k| matches!(k, BlockKind::Opaque { .. }))
                .expect("tagfilter source");
            let hidden =
                html_block_layout_or_source(source, block, &style, &theme, &RevealState::HIDDEN);
            assert!(
                !hidden.text.contains("Doc title")
                    && !hidden.text.contains("raw")
                    && !hidden.text.contains("<title")
                    && !hidden.text.contains("<xmp")
                    && !hidden.text.contains("<plaintext"),
                "caret outside must hide title/xmp/plaintext, {source:?} got {:?}",
                hidden.text
            );
            let inner = source
                .find("Doc title")
                .or_else(|| source.find("raw"))
                .unwrap_or(block.source_range.start);
            let revealed = html_block_layout_or_source(
                source,
                block,
                &style,
                &theme,
                &RevealState {
                    caret: inner,
                    selection: 0..0,
                },
            );
            let open = if source.contains("<title") {
                "<title"
            } else if source.contains("<xmp") {
                "<xmp"
            } else {
                "<plaintext"
            };
            assert!(
                revealed.text.contains(open)
                    && (revealed.text.contains("Doc title")
                        || revealed.text.contains("raw")
                        || revealed.text.contains("raw text")),
                "caret in title/xmp/plaintext must reveal source, {source:?} got {:?}",
                revealed.text
            );
            if source.contains("<xmp") {
                assert!(
                    revealed.text.contains("<b>html</b>") || revealed.text.contains("raw"),
                    "xmp must paint raw source, not nested HTML, {source:?} got {:?}",
                    revealed.text
                );
            }
            let vis = revealed.text.find('<').expect("painted <");
            assert_eq!(
                source
                    .as_bytes()
                    .get(revealed.source_for_visible(vis))
                    .copied(),
                Some(b'<'),
                "click on revealed title/xmp source maps onto the tag, {source:?}"
            );
        }
    }

    #[test]
    fn html_block_details_reveal_source_when_caret_intersects() {
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        for source in [
            "<details><summary>Title</summary>body</details>\n",
            "<details>\n<summary>Title</summary>\nhidden\n</details>\n",
            "> <details><summary>Title</summary>body</details>\n",
            "- <details><summary>Title</summary>body</details>\n",
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let block = first_kind(&tree.blocks, |k| matches!(k, BlockKind::Opaque { .. }))
                .expect("details html");
            let hidden =
                html_block_layout_or_source(source, block, &style, &theme, &RevealState::HIDDEN);
            assert!(
                !hidden.text.contains("Title")
                    && !hidden.text.contains("body")
                    && !hidden.text.contains("hidden")
                    && !hidden.text.contains("<details")
                    && !hidden.text.contains("<summary"),
                "caret outside must hide details source, {source:?} got {:?}",
                hidden.text
            );
            let inner = source
                .find("Title")
                .or_else(|| source.find("body"))
                .unwrap_or(block.source_range.start);
            let revealed = html_block_layout_or_source(
                source,
                block,
                &style,
                &theme,
                &RevealState {
                    caret: inner,
                    selection: 0..0,
                },
            );
            assert!(
                revealed.text.contains("<details")
                    && (revealed.text.contains("Title")
                        || revealed.text.contains("body")
                        || revealed.text.contains("hidden")),
                "caret in details must reveal source, {source:?} got {:?}",
                revealed.text
            );
            assert!(
                revealed.text.contains("<summary") || revealed.text.contains("Title"),
                "details must paint raw source, not a disclosure widget, {source:?} got {:?}",
                revealed.text
            );
            let vis = revealed.text.find('<').expect("painted <");
            assert_eq!(
                source
                    .as_bytes()
                    .get(revealed.source_for_visible(vis))
                    .copied(),
                Some(b'<'),
                "click on revealed details source maps onto the tag, {source:?}"
            );
        }
    }

    #[test]
    fn html_dangerous_html_reveal_source_when_caret_intersects() {
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        for source in [
            "<video src=\"x.mp4\"></video>\n",
            "<video>\nhello\n</video>\n",
            "<dialog>hello</dialog>\n",
            "<form action=\"/x\">ok</form>\n",
            "<math>x^2</math>\n",
            "<canvas>fallback</canvas>\n",
            "- <video src=\"x.mp4\"></video>\n",
            "> <dialog>hello</dialog>\n",
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let block = first_kind(&tree.blocks, |k| {
                matches!(k, BlockKind::Opaque { .. } | BlockKind::Paragraph)
            })
            .expect("dangerous html");
            let hidden = if matches!(block.kind, BlockKind::Opaque { .. }) {
                html_block_layout_or_source(source, block, &style, &theme, &RevealState::HIDDEN)
            } else {
                build_leaf_layout_revealed(
                    block,
                    source,
                    &style,
                    &theme,
                    gpui::FontWeight::NORMAL,
                    &RevealState::HIDDEN,
                    &ChromeHosts::NONE,
                )
            };
            assert!(
                !hidden.text.contains("<video")
                    && !hidden.text.contains("<dialog")
                    && !hidden.text.contains("<form")
                    && !hidden.text.contains("<math")
                    && !hidden.text.contains("<canvas")
                    && !hidden.text.contains("hello")
                    && !hidden.text.contains("fallback")
                    && !hidden.text.contains("x.mp4"),
                "caret outside must hide dangerous HTML source, {source:?} got {:?}",
                hidden.text
            );
            let inner = source
                .find("hello")
                .or_else(|| source.find("ok"))
                .or_else(|| source.find("fallback"))
                .or_else(|| source.find("x.mp4"))
                .or_else(|| source.find("x^2"))
                .unwrap_or(block.source_range.start);
            let revealed = if matches!(block.kind, BlockKind::Opaque { .. }) {
                html_block_layout_or_source(
                    source,
                    block,
                    &style,
                    &theme,
                    &RevealState {
                        caret: inner,
                        selection: 0..0,
                    },
                )
            } else {
                build_leaf_layout_revealed(
                    block,
                    source,
                    &style,
                    &theme,
                    gpui::FontWeight::NORMAL,
                    &RevealState {
                        caret: inner,
                        selection: 0..0,
                    },
                    &ChromeHosts::NONE,
                )
            };
            assert!(
                revealed.text.contains("<video")
                    || revealed.text.contains("<dialog")
                    || revealed.text.contains("<form")
                    || revealed.text.contains("<math")
                    || revealed.text.contains("<canvas"),
                "caret in dangerous HTML must reveal source, {source:?} got {:?}",
                revealed.text
            );
            let vis = revealed.text.find('<').expect("painted <");
            assert_eq!(
                source
                    .as_bytes()
                    .get(revealed.source_for_visible(vis))
                    .copied(),
                Some(b'<'),
                "click on revealed dangerous HTML source maps onto the tag, {source:?}"
            );
        }

        for source in [
            "<object data=\"x\"></object>\n",
            "- <object data=\"x\"></object>\n",
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let block = first_kind(&tree.blocks, |k| {
                matches!(k, BlockKind::Opaque { .. } | BlockKind::Paragraph)
            })
            .expect("object html");
            let caret = source.find("object").unwrap_or(block.source_range.start);
            let revealed = if matches!(block.kind, BlockKind::Opaque { .. }) {
                html_block_layout_or_source(
                    source,
                    block,
                    &style,
                    &theme,
                    &RevealState {
                        caret,
                        selection: 0..0,
                    },
                )
            } else {
                build_leaf_layout_revealed(
                    block,
                    source,
                    &style,
                    &theme,
                    gpui::FontWeight::NORMAL,
                    &RevealState {
                        caret,
                        selection: 0..0,
                    },
                    &ChromeHosts::NONE,
                )
            };
            assert!(
                !revealed.text.contains("<object") && !revealed.text.contains("data"),
                "object must stay hidden like iframe, {source:?} got {:?}",
                revealed.text
            );
        }
    }

    #[test]
    fn html_form_controls_reveal_source_when_caret_intersects() {
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        for source in [
            "<button>click</button>\n",
            "<button>\nclick\n</button>\n",
            "- <button>click</button>\n",
            "> <button>click</button>\n",
            "<select><option>a</option></select>\n",
            "> <select><option>a</option></select>\n",
            "<input type=\"text\">\n",
            "- <input type=\"text\">\n",
            "<label>Name</label>\n",
            "<option>a</option>\n",
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let block = first_kind(&tree.blocks, |k| {
                matches!(k, BlockKind::Opaque { .. } | BlockKind::Paragraph)
            })
            .expect("form-control html");
            let hidden = if matches!(block.kind, BlockKind::Opaque { .. }) {
                html_block_layout_or_source(source, block, &style, &theme, &RevealState::HIDDEN)
            } else {
                build_leaf_layout_revealed(
                    block,
                    source,
                    &style,
                    &theme,
                    gpui::FontWeight::NORMAL,
                    &RevealState::HIDDEN,
                    &ChromeHosts::NONE,
                )
            };
            assert!(
                !hidden.text.contains("<button")
                    && !hidden.text.contains("<select")
                    && !hidden.text.contains("<input")
                    && !hidden.text.contains("<label")
                    && !hidden.text.contains("<option")
                    && !hidden.text.contains("click")
                    && !hidden.text.contains("Name"),
                "caret outside must hide form-control source, {source:?} got {:?}",
                hidden.text
            );
            let inner = source
                .find("click")
                .or_else(|| source.find("Name"))
                .or_else(|| source.find("type="))
                .or_else(|| source.find("<option"))
                .unwrap_or(block.source_range.start);
            let revealed = if matches!(block.kind, BlockKind::Opaque { .. }) {
                html_block_layout_or_source(
                    source,
                    block,
                    &style,
                    &theme,
                    &RevealState {
                        caret: inner,
                        selection: 0..0,
                    },
                )
            } else {
                build_leaf_layout_revealed(
                    block,
                    source,
                    &style,
                    &theme,
                    gpui::FontWeight::NORMAL,
                    &RevealState {
                        caret: inner,
                        selection: 0..0,
                    },
                    &ChromeHosts::NONE,
                )
            };
            assert!(
                revealed.text.contains("<button")
                    || revealed.text.contains("<select")
                    || revealed.text.contains("<input")
                    || revealed.text.contains("<label")
                    || revealed.text.contains("<option"),
                "caret in a form control must reveal source, not a live form UI, {source:?} got {:?}",
                revealed.text
            );
            let vis = revealed.text.find('<').expect("painted <");
            assert_eq!(
                source
                    .as_bytes()
                    .get(revealed.source_for_visible(vis))
                    .copied(),
                Some(b'<'),
                "click on revealed form-control source maps onto the tag, {source:?}"
            );
        }

        let mixed = "hello <button>click</button>\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(mixed, &mut ids);
        let block = first_kind(&tree.blocks, |k| matches!(k, BlockKind::Paragraph))
            .expect("mixed paragraph");
        let hidden = build_leaf_layout_revealed(
            block,
            mixed,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            &RevealState::HIDDEN,
            &ChromeHosts::NONE,
        );
        assert!(
            hidden.text.contains("hello")
                && !hidden.text.contains("click")
                && !hidden.text.contains("<button"),
            "mixed paragraph must paint hello and hide the button widget, got {:?}",
            hidden.text
        );
        let click = mixed.find("click").expect("click");
        let revealed = build_leaf_layout_revealed(
            block,
            mixed,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            &RevealState {
                caret: click,
                selection: 0..0,
            },
            &ChromeHosts::NONE,
        );
        assert!(
            revealed.text.contains("hello") && revealed.text.contains("<button"),
            "caret in the button must reveal source beside hello, got {:?}",
            revealed.text
        );
    }

    #[test]
    fn html_noscript_and_template_reveal_source_when_caret_intersects() {
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        for source in [
            "<noscript>fallback</noscript>\n",
            "<noscript>\nfallback\n</noscript>\n",
            "- <noscript>fallback</noscript>\n",
            "> <noscript>fallback</noscript>\n",
            "<template><p>slot</p></template>\n",
            "> <template><p>slot</p></template>\n",
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let block = first_kind(&tree.blocks, |k| {
                matches!(k, BlockKind::Opaque { .. } | BlockKind::Paragraph)
            })
            .expect("noscript/template html");
            let hidden = if matches!(block.kind, BlockKind::Opaque { .. }) {
                html_block_layout_or_source(source, block, &style, &theme, &RevealState::HIDDEN)
            } else {
                build_leaf_layout_revealed(
                    block,
                    source,
                    &style,
                    &theme,
                    gpui::FontWeight::NORMAL,
                    &RevealState::HIDDEN,
                    &ChromeHosts::NONE,
                )
            };
            assert!(
                !hidden.text.contains("<noscript")
                    && !hidden.text.contains("<template")
                    && !hidden.text.contains("fallback")
                    && !hidden.text.contains("slot"),
                "caret outside must hide noscript/template source, {source:?} got {:?}",
                hidden.text
            );
            let inner = source
                .find("fallback")
                .or_else(|| source.find("slot"))
                .unwrap_or(block.source_range.start);
            let revealed = if matches!(block.kind, BlockKind::Opaque { .. }) {
                html_block_layout_or_source(
                    source,
                    block,
                    &style,
                    &theme,
                    &RevealState {
                        caret: inner,
                        selection: 0..0,
                    },
                )
            } else {
                build_leaf_layout_revealed(
                    block,
                    source,
                    &style,
                    &theme,
                    gpui::FontWeight::NORMAL,
                    &RevealState {
                        caret: inner,
                        selection: 0..0,
                    },
                    &ChromeHosts::NONE,
                )
            };
            assert!(
                revealed.text.contains("<noscript") || revealed.text.contains("<template"),
                "caret in noscript/template must reveal source, not a nested document, {source:?} got {:?}",
                revealed.text
            );
            let vis = revealed.text.find('<').expect("painted <");
            assert_eq!(
                source
                    .as_bytes()
                    .get(revealed.source_for_visible(vis))
                    .copied(),
                Some(b'<'),
                "click on revealed noscript/template source maps onto the tag, {source:?}"
            );
        }
    }

    #[test]
    fn html_fieldset_legend_reveal_source_when_caret_intersects() {
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        for source in [
            "<fieldset><legend>Title</legend>body</fieldset>\n",
            "<fieldset>\n<legend>Title</legend>\nhidden\n</fieldset>\n",
            "> <fieldset><legend>Title</legend>body</fieldset>\n",
            "- <fieldset><legend>Title</legend>body</fieldset>\n",
            "<legend>Title</legend>\n",
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let block = first_kind(&tree.blocks, |k| {
                matches!(k, BlockKind::Opaque { .. } | BlockKind::Paragraph)
            })
            .expect("fieldset/legend html");
            let hidden = if matches!(block.kind, BlockKind::Opaque { .. }) {
                html_block_layout_or_source(source, block, &style, &theme, &RevealState::HIDDEN)
            } else {
                build_leaf_layout_revealed(
                    block,
                    source,
                    &style,
                    &theme,
                    gpui::FontWeight::NORMAL,
                    &RevealState::HIDDEN,
                    &ChromeHosts::NONE,
                )
            };
            assert!(
                !hidden.text.contains("Title")
                    && !hidden.text.contains("body")
                    && !hidden.text.contains("hidden")
                    && !hidden.text.contains("<fieldset")
                    && !hidden.text.contains("<legend"),
                "caret outside must hide fieldset/legend source, {source:?} got {:?}",
                hidden.text
            );
            let inner = source
                .find("Title")
                .or_else(|| source.find("body"))
                .unwrap_or(block.source_range.start);
            let revealed = if matches!(block.kind, BlockKind::Opaque { .. }) {
                html_block_layout_or_source(
                    source,
                    block,
                    &style,
                    &theme,
                    &RevealState {
                        caret: inner,
                        selection: 0..0,
                    },
                )
            } else {
                build_leaf_layout_revealed(
                    block,
                    source,
                    &style,
                    &theme,
                    gpui::FontWeight::NORMAL,
                    &RevealState {
                        caret: inner,
                        selection: 0..0,
                    },
                    &ChromeHosts::NONE,
                )
            };
            assert!(
                revealed.text.contains("<fieldset") || revealed.text.contains("<legend"),
                "caret in fieldset/legend must reveal source, not a live form UI, {source:?} got {:?}",
                revealed.text
            );
            let vis = revealed.text.find('<').expect("painted <");
            assert_eq!(
                source
                    .as_bytes()
                    .get(revealed.source_for_visible(vis))
                    .copied(),
                Some(b'<'),
                "click on revealed fieldset/legend source maps onto the tag, {source:?}"
            );
        }
    }

    #[test]
    fn html_output_progress_meter_reveal_source_when_caret_intersects() {
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        for source in [
            "<output>42</output>\n",
            "<output>\n42\n</output>\n",
            "- <output>42</output>\n",
            "> <output>42</output>\n",
            "<progress value=\"70\" max=\"100\">70%</progress>\n",
            "> <progress value=\"70\">70%</progress>\n",
            "<meter value=\"0.6\">60%</meter>\n",
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let block = first_kind(&tree.blocks, |k| {
                matches!(k, BlockKind::Opaque { .. } | BlockKind::Paragraph)
            })
            .expect("output/progress/meter html");
            let hidden = if matches!(block.kind, BlockKind::Opaque { .. }) {
                html_block_layout_or_source(source, block, &style, &theme, &RevealState::HIDDEN)
            } else {
                build_leaf_layout_revealed(
                    block,
                    source,
                    &style,
                    &theme,
                    gpui::FontWeight::NORMAL,
                    &RevealState::HIDDEN,
                    &ChromeHosts::NONE,
                )
            };
            assert!(
                !hidden.text.contains("<output")
                    && !hidden.text.contains("<progress")
                    && !hidden.text.contains("<meter")
                    && !hidden.text.contains("42")
                    && !hidden.text.contains("70%")
                    && !hidden.text.contains("60%"),
                "caret outside must hide output/progress/meter source, {source:?} got {:?}",
                hidden.text
            );
            let inner = source
                .find("42")
                .or_else(|| source.find("70%"))
                .or_else(|| source.find("60%"))
                .or_else(|| source.find("value="))
                .unwrap_or(block.source_range.start);
            let revealed = if matches!(block.kind, BlockKind::Opaque { .. }) {
                html_block_layout_or_source(
                    source,
                    block,
                    &style,
                    &theme,
                    &RevealState {
                        caret: inner,
                        selection: 0..0,
                    },
                )
            } else {
                build_leaf_layout_revealed(
                    block,
                    source,
                    &style,
                    &theme,
                    gpui::FontWeight::NORMAL,
                    &RevealState {
                        caret: inner,
                        selection: 0..0,
                    },
                    &ChromeHosts::NONE,
                )
            };
            assert!(
                revealed.text.contains("<output")
                    || revealed.text.contains("<progress")
                    || revealed.text.contains("<meter"),
                "caret in output/progress/meter must reveal source, not a live UI, {source:?} got {:?}",
                revealed.text
            );
            let vis = revealed.text.find('<').expect("painted <");
            assert_eq!(
                source
                    .as_bytes()
                    .get(revealed.source_for_visible(vis))
                    .copied(),
                Some(b'<'),
                "click on revealed output/progress/meter source maps onto the tag, {source:?}"
            );
        }

        let mixed = "hello <output>42</output>\n";
        let mut ids = IdGen::default();
        let tree = import_markdown(mixed, &mut ids);
        let block = first_kind(&tree.blocks, |k| matches!(k, BlockKind::Paragraph))
            .expect("mixed paragraph");
        let hidden = build_leaf_layout_revealed(
            block,
            mixed,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            &RevealState::HIDDEN,
            &ChromeHosts::NONE,
        );
        assert!(
            hidden.text.contains("hello")
                && !hidden.text.contains("42")
                && !hidden.text.contains("<output"),
            "mixed paragraph must paint hello and hide the output widget, got {:?}",
            hidden.text
        );
        let inner = mixed.find("42").expect("42");
        let revealed = build_leaf_layout_revealed(
            block,
            mixed,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            &RevealState {
                caret: inner,
                selection: 0..0,
            },
            &ChromeHosts::NONE,
        );
        assert!(
            revealed.text.contains("hello") && revealed.text.contains("<output"),
            "caret in the output must reveal source beside hello, got {:?}",
            revealed.text
        );
    }

    #[test]
    fn html_block_style_and_textarea_reveal_when_caret_intersects() {
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        for source in [
            "<style>body { color: red }</style>\n",
            "<style>body > p { color: red }</style>\n",
            "<textarea>hello</textarea>\n",
            "<textarea>a > b</textarea>\n",
            "> <style>body { color: red }</style>\n",
            "- <textarea>hello</textarea>\n",
        ] {
            let mut ids = IdGen::default();
            let tree = import_markdown(source, &mut ids);
            let block = first_kind(&tree.blocks, |k| matches!(k, BlockKind::Opaque { .. }))
                .expect("type-1 html");
            let hidden =
                html_block_layout_or_source(source, block, &style, &theme, &RevealState::HIDDEN);
            assert!(
                !hidden.text.contains("body")
                    && !hidden.text.contains("hello")
                    && !hidden.text.contains("<style")
                    && !hidden.text.contains("<textarea"),
                "caret outside must hide Type-1 style/textarea, {source:?} got {:?}",
                hidden.text
            );
            let inner = source
                .find("body")
                .or_else(|| source.find("hello"))
                .or_else(|| source.find("a > b"))
                .expect("inner");
            let revealed = html_block_layout_or_source(
                source,
                block,
                &style,
                &theme,
                &RevealState {
                    caret: inner,
                    selection: 0..0,
                },
            );
            let open = if source.contains("<style") {
                "<style"
            } else {
                "<textarea"
            };
            assert!(
                revealed.text.contains(open)
                    && (revealed.text.contains("body")
                        || revealed.text.contains("hello")
                        || revealed.text.contains("a > b")),
                "caret in Type-1 style/textarea must reveal source, {source:?} got {:?}",
                revealed.text
            );
            let vis = revealed.text.find('<').expect("painted <");
            assert_eq!(
                source
                    .as_bytes()
                    .get(revealed.source_for_visible(vis))
                    .copied(),
                Some(b'<'),
                "click on revealed Type-1 source maps onto the tag, {source:?}"
            );
        }
    }

    #[test]
    fn html_pre_does_not_parse_inner_markdown() {
        for raw in ["<pre>**bold**</pre>", "<pre>\n**bold**\n</pre>"] {
            let layout = html_block_layout(raw);
            assert!(
                layout.text.contains("**"),
                "<pre> must paint literal asterisks, {raw:?} got {:?}",
                layout.text
            );
            assert!(
                !layout
                    .runs
                    .iter()
                    .any(|run| run.font.weight == gpui::FontWeight::BOLD),
                "<pre> must not interpret inner Markdown as rich text, {raw:?} runs={:?}",
                layout.runs
            );
        }

        let quoted = "> <pre>**x**</pre>\n";
        let layout = html_block_layout_from_source(quoted);
        assert!(
            layout.text.contains("**") || layout.text.contains('x'),
            "quoted <pre> must paint, got {:?}",
            layout.text
        );
        if layout.text.contains('x') {
            assert!(
                layout.text.contains("**")
                    || !layout
                        .runs
                        .iter()
                        .any(|run| run.font.weight == gpui::FontWeight::BOLD),
                "quoted <pre> must not parse Markdown, got {:?} runs={:?}",
                layout.text,
                layout.runs
            );
        }
    }

    #[test]
    fn inline_html_style_color_paints() {
        let layout = layout_for("hello <span style=\"color: #ff0000\">red</span> world\n");
        assert_eq!(layout.text, "hello red world");
        assert!(
            !layout.text.contains('<'),
            "style tags must not paint, got {:?}",
            layout.text
        );
        let theme_text = EditorTheme::dark().text;
        assert!(
            layout.runs.iter().any(|run| run.color != theme_text),
            "expected style= color on inner text, runs={:?}",
            layout.runs
        );
    }

    #[test]
    fn inline_svg_layout_omits_tag_bytes() {
        let layout = layout_for(
            "hello <svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\"><rect width=\"8\" height=\"8\" fill=\"#f00\"/></svg> world\n",
        );
        assert!(
            layout.text.contains("hello") && layout.text.contains("world"),
            "{:?}",
            layout.text
        );
        assert!(
            !layout.text.contains("<svg") && !layout.text.contains("fill="),
            "svg source must not paint as text, got {:?}",
            layout.text
        );
    }

    #[test]
    fn html_block_style_color_paints() {
        let layout = html_block_layout("<p style=\"color: #0000ff; font-weight: bold\">blue</p>");
        assert!(layout.text.contains("blue"), "{:?}", layout.text);
        assert!(
            !layout.text.contains("<p"),
            "html tags must not paint, got {:?}",
            layout.text
        );
        assert!(
            layout
                .runs
                .iter()
                .any(|run| run.font.weight == gpui::FontWeight::BOLD),
            "expected style font-weight bold, runs={:?}",
            layout.runs
        );
        let theme_text = EditorTheme::dark().text;
        assert!(
            layout.runs.iter().any(|run| run.color != theme_text),
            "expected style= color on HTML-block text, runs={:?}",
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
            source,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            &RevealState {
                caret,
                selection: 0..0,
            },
            &ChromeHosts::NONE,
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

    /// GFM GOAL: with the caret outside, WYSIWYG leaf text is the rendered
    /// body, not source chrome (`#`, `- `, `[]()`, `|`, `[x]`, fences, `>`).
    /// Intersect-reveal of wrap marks, ATX hashes, setext underlines,
    /// link/image chrome, list/quote prefixes, fence ticks, and table `|` is
    /// covered separately.
    #[test]
    fn gfm_wysiwyg_hides_source_chrome() {
        let atx = layout_for("# Title\n");
        assert_eq!(atx.text, "Title", "ATX heading painted {:?}", atx.text);
        assert!(
            !atx.text.contains('#'),
            "ATX hashes must not paint, got {:?}",
            atx.text
        );
        let h2 = layout_for("## Sub\n");
        assert_eq!(h2.text, "Sub");
        assert!(!h2.text.contains('#'));
        let closed = layout_for("# Title #\n");
        assert_eq!(closed.text, "Title");
        assert!(!closed.text.contains('#'));
        let closed_src = "# Title #\n";
        let end = closed.source_for_visible(closed.text.len());
        assert_ne!(
            closed_src.as_bytes().get(end).copied(),
            Some(b'#'),
            "click/IME at the end of a closed ATX title must not land on `#`, source_at={:?} end={end}",
            closed.source_at
        );
        let setext = layout_for("Title\n=====\n");
        assert_eq!(setext.text, "Title");
        assert!(!setext.text.contains('='));

        let italic = layout_for("*hi*\n");
        assert_eq!(italic.text, "hi");
        assert!(!italic.text.contains('*'));
        assert!(italic
            .runs
            .iter()
            .any(|run| run.font.style == gpui::FontStyle::Italic));
        let code = layout_for("`x`\n");
        assert_eq!(code.text, "x");
        assert!(!code.text.contains('`'));

        let link = layout_for("[label](https://e.com)\n");
        assert_eq!(link.text, "label");
        assert!(
            !link.text.contains('[') && !link.text.contains(']') && !link.text.contains('('),
            "link chrome must not paint, got {:?}",
            link.text
        );
        assert!(
            link.runs
                .iter()
                .any(|run| run.underline.is_some() && run.color != EditorTheme::dark().text),
            "expected link paint, runs={:?}",
            link.runs
        );

        let mut ids = IdGen::default();
        let list_tree = import_markdown("- item\n", &mut ids);
        let list_para = first_paragraph(&list_tree.blocks[0]).expect("list paragraph");
        let list = layout_of(list_para);
        assert_eq!(list.text, "item");
        assert!(
            !list.text.contains('-'),
            "list marker must not paint in the body, got {:?}",
            list.text
        );

        let mut ids = IdGen::default();
        let task_tree = import_markdown("- [x] done\n", &mut ids);
        let task_para = first_paragraph(&task_tree.blocks[0]).expect("task paragraph");
        let task = layout_of(task_para);
        assert_eq!(task.text, "done");
        assert!(
            !task.text.contains('[') && !task.text.contains(']'),
            "task checkbox syntax must not paint, got {:?}",
            task.text
        );

        let mut ids = IdGen::default();
        let table_tree = import_markdown("| a | b |\n|---|---|\n| 1 | 2 |\n", &mut ids);
        let cell = first_kind(&table_tree.blocks, |k| matches!(k, BlockKind::TableCell))
            .expect("table cell");
        let cell_layout = layout_of(cell);
        assert_eq!(cell_layout.text, "a");
        assert!(
            !cell_layout.text.contains('|') && !cell_layout.text.contains('-'),
            "table pipes must not paint, got {:?}",
            cell_layout.text
        );

        let mut ids = IdGen::default();
        let compact_tree = import_markdown("foo|bar\n---|---\nbaz|bim\n", &mut ids);
        let compact_cell = first_kind(&compact_tree.blocks, |k| matches!(k, BlockKind::TableCell))
            .expect("compact cell");
        let compact_layout = layout_of(compact_cell);
        assert_eq!(compact_layout.text, "foo");
        assert!(
            !compact_layout.text.contains('|') && !compact_layout.text.contains('-'),
            "compact GFM table pipes must not paint, got {:?}",
            compact_layout.text
        );

        let mut ids = IdGen::default();
        let fence_tree = import_markdown("```rust\nfn main() {}\n```\n", &mut ids);
        match &fence_tree.blocks[0].kind {
            BlockKind::CodeBlock { info, literal, .. } => {
                assert_eq!(info, "rust");
                assert_eq!(
                    literal.strip_suffix('\n').unwrap_or(literal),
                    "fn main() {}"
                );
                assert!(
                    !literal.contains('`'),
                    "fence ticks must not be in the body literal, got {literal:?}"
                );
            }
            other => panic!("expected code block, got {other:?}"),
        }
        let fence_layout = layout_of(&fence_tree.blocks[0]);
        assert_eq!(fence_layout.text, "fn main() {}");
        assert!(!fence_layout.text.contains('`'));

        let mut ids = IdGen::default();
        let quote_tree = import_markdown("> quoted\n", &mut ids);
        let quote_para = first_paragraph(&quote_tree.blocks[0]).expect("quote paragraph");
        let quote = layout_of(quote_para);
        assert_eq!(quote.text, "quoted");
        assert!(
            !quote.text.contains('>'),
            "blockquote marker must not paint, got {:?}",
            quote.text
        );

        let mut ids = IdGen::default();
        let fm_tree = import_markdown("---\ntitle: X\n---\n\n# Hi\n", &mut ids);
        assert!(
            fm_tree.frontmatter.is_some(),
            "frontmatter must not be a body block"
        );
        assert!(matches!(fm_tree.blocks[0].kind, BlockKind::Heading { .. }));
        let after_fm = layout_of(&fm_tree.blocks[0]);
        assert_eq!(after_fm.text, "Hi");
        assert!(!after_fm.text.contains("---"));
        assert!(!after_fm.text.contains('#'));
    }

    /// Click/IME on painted GFM inlines must land on the visible body, not
    /// delimiter bytes (` `` `, `[`, `<`).
    #[test]
    fn gfm_inline_click_maps_past_chrome() {
        let code_src = "`x`\n";
        let code = layout_for(code_src);
        assert_eq!(code.text, "x");
        let mapped = code.source_for_visible(0);
        assert_eq!(
            code_src.as_bytes().get(mapped).copied(),
            Some(b'x'),
            "click on painted inline code must be `x`, not a backtick, got {mapped} {:?}",
            code_src.get(mapped..mapped.saturating_add(1))
        );
        assert_ne!(
            mapped,
            code_src.find('`').expect("tick"),
            "click must not land on the opening backtick"
        );

        let link_src = "[label](https://e.com)\n";
        let link = layout_for(link_src);
        assert_eq!(link.text, "label");
        let mapped = link.source_for_visible(0);
        assert_eq!(
            link_src.as_bytes().get(mapped).copied(),
            Some(b'l'),
            "click on painted link text must be `l`, not `[`, got {mapped} {:?}",
            link_src.get(mapped..mapped.saturating_add(1))
        );

        let auto_src = "<https://example.com>\n";
        let auto = layout_for(auto_src);
        assert!(auto.text.contains("https://example.com"));
        assert!(
            !auto.text.contains('<') && !auto.text.contains('>'),
            "autolink `<>` must not paint, got {:?}",
            auto.text
        );
        let mapped = auto.source_for_visible(0);
        assert_ne!(
            auto_src.as_bytes().get(mapped).copied(),
            Some(b'<'),
            "click on painted autolink must not land on `<`, got {mapped}"
        );
        assert_eq!(
            auto_src.as_bytes().get(mapped).copied(),
            Some(b'h'),
            "click on painted autolink must be `h` of https, got {mapped} {:?}",
            auto_src.get(mapped..mapped.saturating_add(1))
        );

        let email_src = "<user@example.com>\n";
        let email = layout_for(email_src);
        assert!(
            email.text.contains("user@example.com"),
            "email autolink visible text, got {:?}",
            email.text
        );
        assert!(
            !email.text.contains('<') && !email.text.contains('>'),
            "email autolink `<>` must not paint, got {:?}",
            email.text
        );
        let mapped = email.source_for_visible(0);
        assert_ne!(
            email_src.as_bytes().get(mapped).copied(),
            Some(b'<'),
            "click on painted email autolink must not land on `<`, got {mapped}"
        );
        assert_eq!(
            email_src.as_bytes().get(mapped).copied(),
            Some(b'u'),
            "click on painted email autolink must be `u` of user, got {mapped} {:?}",
            email_src.get(mapped..mapped.saturating_add(1))
        );

        let ref_src = "[label][ref]\n\n[ref]: https://e.com\n";
        let ref_link = layout_for(ref_src);
        assert_eq!(
            ref_link.text, "label",
            "reference link must paint the label, not `[ref]`, got {:?}",
            ref_link.text
        );
        assert!(
            !ref_link.text.contains('[') && !ref_link.text.contains(']'),
            "reference chrome must not paint, got {:?}",
            ref_link.text
        );
        let mapped = ref_link.source_for_visible(0);
        assert_eq!(
            ref_src.as_bytes().get(mapped).copied(),
            Some(b'l'),
            "click on a reference link must be `l`, got {mapped} {:?}",
            ref_src.get(mapped..mapped.saturating_add(1))
        );

        let mut ids = IdGen::default();
        let def_tree = import_markdown(ref_src, &mut ids);
        let def = def_tree
            .blocks
            .iter()
            .find(|b| matches!(b.kind, BlockKind::LinkReferenceDefinition { .. }))
            .expect("definition block");
        let hidden = layout_of_source(def, ref_src, &RevealState::HIDDEN);
        assert!(
            hidden.text.contains("ref") && hidden.text.contains("https://e.com"),
            "definition must paint label and dest, got {:?}",
            hidden.text
        );
        assert!(
            !hidden.text.contains('[') && !hidden.text.contains(']'),
            "definition `[` `]` must hide until intersect, got {:?}",
            hidden.text
        );
        assert!(
            hidden.text.contains(':'),
            "definition colon stays visible, got {:?}",
            hidden.text
        );
        let vis = hidden.text.find("ref").expect("painted label");
        let mapped = hidden.source_for_visible(vis);
        assert_eq!(
            ref_src.as_bytes().get(mapped).copied(),
            Some(b'r'),
            "click on painted definition label must be `r`, got {mapped}"
        );
        let dest_vis = hidden.text.find("https://e.com").expect("painted dest");
        let mapped_dest = hidden.source_for_visible(dest_vis);
        assert_eq!(
            ref_src.as_bytes().get(mapped_dest).copied(),
            Some(b'h'),
            "click on painted dest must be `h`, got {mapped_dest}"
        );
        let dest_byte = ref_src.find("https://e.com").expect("dest");
        let colon_vis = hidden.text.find(':').expect("painted colon");
        let mapped_colon = hidden.source_for_visible(colon_vis);
        assert_eq!(
            ref_src.as_bytes().get(mapped_colon).copied(),
            Some(b':'),
            "painted colon maps onto `:`; caret skip onto dest is engine dest chrome"
        );
        let revealed = layout_for_caret(ref_src, dest_byte);
        assert!(
            revealed.text.contains("[ref]: https://e.com"),
            "caret in dest must reveal `[ref]:`, got {:?}",
            revealed.text
        );

        for (src, label) in [
            ("[foo][]\n\n[foo]: https://e.com\n", "foo"),
            ("[foo]\n\n[foo]: https://e.com\n", "foo"),
        ] {
            let layout = layout_for(src);
            assert_eq!(
                layout.text, label,
                "collapsed/shortcut ref must paint the label, got {:?}",
                layout.text
            );
            assert!(
                !layout.text.contains('[') && !layout.text.contains(']'),
                "collapsed/shortcut dest chrome must not paint, got {:?}",
                layout.text
            );
            let mapped = layout.source_for_visible(0);
            assert_eq!(
                src.as_bytes().get(mapped).copied(),
                Some(b'f'),
                "click on {src:?} must be `f`, got {mapped}"
            );
        }

        let setext_src = "Title\n=====\n";
        let setext = layout_for(setext_src);
        assert_eq!(setext.text, "Title");
        let mapped = setext.source_for_visible(0);
        assert_eq!(
            setext_src.as_bytes().get(mapped).copied(),
            Some(b'T'),
            "setext click must be `T`, got {mapped}"
        );
        let end = setext.source_for_visible(setext.text.len());
        assert_ne!(
            setext_src.as_bytes().get(end).copied(),
            Some(b'='),
            "click/IME at the end of a setext title must not land on `=`, source_at={:?} end={end}",
            setext.source_at
        );

        let math_src = "see $x^2$ here\n";
        let math = layout_for(math_src);
        assert_eq!(math.text, "see x^2 here");
        let vis_x = math.text.find('x').expect("painted x");
        let mapped = math.source_for_visible(vis_x);
        assert_eq!(
            math_src.as_bytes().get(mapped).copied(),
            Some(b'x'),
            "click on painted math must be `x`, not `$`, got {mapped} {:?}",
            math_src.get(mapped..mapped.saturating_add(1))
        );
        assert_ne!(
            mapped,
            math_src.find('$').expect("$"),
            "click must not land on the opening `$`"
        );

        let display_src = "see $$x^2$$ here\n";
        let display = layout_for(display_src);
        let vis_dx = display.text.find('x').expect("painted x");
        assert_eq!(
            display_src
                .as_bytes()
                .get(display.source_for_visible(vis_dx))
                .copied(),
            Some(b'x'),
            "click on display math must be `x`, not `$`"
        );

        let wiki_src = "see [[page]] here\n";
        let wiki = layout_for(wiki_src);
        assert_eq!(wiki.text, "see page here");
        let vis_p = wiki.text.find('p').expect("painted p");
        let mapped = wiki.source_for_visible(vis_p);
        assert_eq!(
            wiki_src.as_bytes().get(mapped).copied(),
            Some(b'p'),
            "click on painted wiki must be `p`, not `[`, got {mapped}"
        );
        assert_ne!(mapped, wiki_src.find('[').expect("["));

        let piped_src = "go [[page|Label]]\n";
        let piped = layout_for(piped_src);
        assert_eq!(piped.text, "go Label");
        let vis_l = piped.text.find('L').expect("painted L");
        assert_eq!(
            piped_src
                .as_bytes()
                .get(piped.source_for_visible(vis_l))
                .copied(),
            Some(b'L'),
            "click on a piped wiki must be `L`, not `[`"
        );

        let emoji_src = "see :smile: here\n";
        let emoji = layout_for(emoji_src);
        assert_eq!(emoji.text, "see 😄 here");
        let vis_g = emoji.text.find('😄').expect("glyph");
        let mapped = emoji.source_for_visible(vis_g);
        assert_ne!(
            emoji_src.as_bytes().get(mapped).copied(),
            Some(b':'),
            "click on painted emoji must not land on `:`, got {mapped}"
        );
        assert_eq!(
            emoji_src.as_bytes().get(mapped).copied(),
            Some(b's'),
            "click on painted emoji must map onto `smile`, got {mapped} {:?}",
            emoji_src.get(mapped..mapped.saturating_add(1))
        );
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

    #[test]
    fn character_reference_paints_decoded_glyph_not_entity_bytes() {
        let cases = [
            ("A&amp;B\n", "&amp;", "&", 'A'),
            ("A&amp;\n", "&amp;", "&", 'A'),
            ("A&lt;B\n", "&lt;", "<", 'A'),
            ("A&gt;B\n", "&gt;", ">", 'A'),
            ("A&quot;B\n", "&quot;", "\"", 'A'),
            ("A&#39;B\n", "&#39;", "'", 'A'),
            ("A&#123;B\n", "&#123;", "{", 'A'),
            ("A&#x7B;B\n", "&#x7B;", "{", 'A'),
        ];
        for (source, literal, decoded, prev) in cases {
            let layout = layout_for(source);
            assert!(
                layout.text.contains(decoded) && layout.text.contains(prev),
                "must paint the decoded glyph, {source:?} got {:?}",
                layout.text
            );
            assert!(
                !layout.text.contains(literal),
                "entity dest chrome must hide until intersect, {source:?} got {:?}",
                layout.text
            );
            let vis = layout.text.find(decoded).expect("painted glyph");
            let mapped = layout.source_for_visible(vis);
            assert_eq!(
                source.as_bytes().get(mapped).copied(),
                Some(b'&'),
                "click on painted {decoded:?} must be `&` of {literal}, {source:?} got {mapped} {:?}",
                source.get(mapped..mapped.saturating_add(1))
            );
            assert_ne!(
                source.as_bytes().get(mapped).copied(),
                Some(b'a'),
                "click must not nibble hidden entity bytes, {source:?}"
            );
        }

        let mut ids = IdGen::default();
        let quote_tree = import_markdown("> A&amp;B\n", &mut ids);
        let quote = first_paragraph(&quote_tree.blocks[0]).expect("quote");
        let quote_layout = layout_of_source(quote, "> A&amp;B\n", &RevealState::HIDDEN);
        assert_eq!(quote_layout.text, "A&B");
        let mapped = quote_layout.source_for_visible(quote_layout.text.find('&').unwrap());
        assert_eq!("> A&amp;B\n".as_bytes().get(mapped).copied(), Some(b'&'));

        let mut ids = IdGen::default();
        let list_tree = import_markdown("- A&amp;B\n", &mut ids);
        let list = first_paragraph(&list_tree.blocks[0]).expect("list");
        let list_layout = layout_of_source(list, "- A&amp;B\n", &RevealState::HIDDEN);
        assert_eq!(list_layout.text, "A&B");

        let mut ids = IdGen::default();
        let table_src = "| A&amp;B | x |\n| --- | --- |\n";
        let table_tree = import_markdown(table_src, &mut ids);
        let cell =
            first_kind(&table_tree.blocks, |k| matches!(k, BlockKind::TableCell)).expect("cell");
        let cell_layout = layout_of_source(cell, table_src, &RevealState::HIDDEN);
        assert!(
            cell_layout.text.contains('A') && cell_layout.text.contains('&'),
            "table cell must paint decoded entity, got {:?}",
            cell_layout.text
        );
        assert!(
            !cell_layout.text.contains("&amp;"),
            "table cell must hide `&amp;`, got {:?}",
            cell_layout.text
        );

        let link_src = "[A&amp;B](https://e.com)\n";
        let link = layout_for(link_src);
        assert_eq!(link.text, "A&B");
        let mapped = link.source_for_visible(link.text.find('&').unwrap());
        assert_eq!(
            link_src.as_bytes().get(mapped).copied(),
            Some(b'&'),
            "click on a link-label entity must be `&`"
        );

        let code_src = "`A&amp;B`\n";
        let code = layout_for(code_src);
        assert!(
            code.text.contains("A&amp;B"),
            "code spans must paint the entity literal, got {:?}",
            code.text
        );
        let vis_a = code.text.find("&amp;").expect("literal");
        let mapped = code.source_for_visible(vis_a + 1);
        assert_eq!(
            code_src.as_bytes().get(mapped).copied(),
            Some(b'a'),
            "click in a code span must walk `amp` bytes, got {mapped}"
        );

        let src = "A&amp;B\n";
        let entity = src.find("&amp;").unwrap();
        let revealed = layout_for_caret(src, entity);
        assert!(
            revealed.text.contains("&amp;"),
            "intersect-reveal may show `&amp;`, got {:?}",
            revealed.text
        );
    }

    #[test]
    fn backslash_escape_paints_decoded_glyph_not_slash() {
        let source = "A\\*B\n";
        let layout = layout_for(source);
        assert!(
            layout.text.contains('*') && layout.text.contains('A'),
            "must paint the escaped glyph, got {:?}",
            layout.text
        );
        assert!(
            !layout.text.contains('\\'),
            "backslash dest chrome must hide until intersect, got {:?}",
            layout.text
        );
        let vis = layout.text.find('*').expect("painted *");
        let mapped = layout.source_for_visible(vis);
        assert_eq!(
            source.as_bytes().get(mapped).copied(),
            Some(b'\\'),
            "click on painted `*` must be `\\` of `\\*`, got {mapped} {:?}",
            source.get(mapped..mapped.saturating_add(1))
        );

        let slash = source.find('\\').unwrap();
        let revealed = layout_for_caret(source, slash);
        assert!(
            revealed.text.contains('\\') && revealed.text.contains('*'),
            "intersect-reveal must show `\\*`, got {:?}",
            revealed.text
        );

        let code = "`A\\*B`\n";
        let code_layout = layout_for(code);
        assert!(
            code_layout.text.contains("A\\*B"),
            "code spans must paint the backslash literal, got {:?}",
            code_layout.text
        );
        let vis_slash = code_layout.text.find('\\').expect("literal slash");
        let mapped = code_layout.source_for_visible(vis_slash);
        assert_eq!(
            code.as_bytes().get(mapped).copied(),
            Some(b'\\'),
            "click in a code span must walk the backslash, got {mapped}"
        );

        let mut ids = IdGen::default();
        let quote_tree = import_markdown("> A\\*B\n", &mut ids);
        let quote = first_paragraph(&quote_tree.blocks[0]).expect("quote");
        let quote_layout = layout_of_source(quote, "> A\\*B\n", &RevealState::HIDDEN);
        assert_eq!(quote_layout.text, "A*B");
        let mapped = quote_layout.source_for_visible(quote_layout.text.find('*').unwrap());
        assert_eq!("> A\\*B\n".as_bytes().get(mapped).copied(), Some(b'\\'));
    }
}
