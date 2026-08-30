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
    SharedString, Style, TextRun, TextStyle, Window, WrappedLine,
};
use markrust_core::rich::{Block, BreakStyle, Inline, MarkSet, NodeId};

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
    fn input_focus_handle(&self) -> FocusHandle;
    fn toggle_task(&mut self, id: NodeId, cx: &mut Context<Self>);
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
}

pub fn build_leaf_layout(
    block: &Block,
    text_style: &TextStyle,
    theme: &EditorTheme,
    base_weight: gpui::FontWeight,
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

    for inline in &block.inlines {
        match inline {
            Inline::Run {
                text: t,
                marks,
                link,
                source_range,
                ..
            } => {
                let mut run = text_style.to_run(0);
                run.font.weight = if marks.contains(MarkSet::BOLD) {
                    gpui::FontWeight::BOLD
                } else {
                    base_weight
                };
                if marks.contains(MarkSet::ITALIC) {
                    run.font.style = gpui::FontStyle::Italic;
                }
                if marks.contains(MarkSet::STRIKE) {
                    run.strikethrough = Some(gpui::StrikethroughStyle {
                        thickness: px(1.),
                        color: Some(theme.secondary_text),
                    });
                }
                if marks.contains(MarkSet::CODE) {
                    run.font.family = theme.code_font_family.clone().into();
                    run.background_color = Some(theme.code_bg);
                }
                if link.is_some() {
                    run.color = theme.link;
                    run.underline = Some(gpui::UnderlineStyle {
                        thickness: px(1.),
                        color: Some(theme.link),
                        wavy: false,
                    });
                }
                push(
                    &mut text,
                    &mut runs,
                    &mut source_at,
                    t,
                    source_range.clone(),
                    run,
                );
            }
            Inline::Image {
                alt, source_range, ..
            } => {
                let label = format!("🖼 {alt}");
                let mut run = text_style.to_run(0);
                run.color = theme.image_text;
                run.font.style = gpui::FontStyle::Italic;
                push(
                    &mut text,
                    &mut runs,
                    &mut source_at,
                    &label,
                    source_range.clone(),
                    run,
                );
            }
            Inline::SoftBreak => {
                let run = text_style.to_run(0);
                let src = block.source_range.start;
                push(&mut text, &mut runs, &mut source_at, " ", src..src + 1, run);
            }
            Inline::HardBreak {
                style: BreakStyle::TwoSpaces | BreakStyle::Backslash,
            } => {
                let run = text_style.to_run(0);
                let src = block.source_range.start;
                push(
                    &mut text,
                    &mut runs,
                    &mut source_at,
                    "\n",
                    src..src + 1,
                    run,
                );
            }
            Inline::OpaqueInline {
                raw, source_range, ..
            } => {
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

    if text.is_empty() {
        source_at = vec![block.source_range.start, block.source_range.start];
    } else if source_at.len() == text.len() {
        source_at.push(
            block
                .inlines
                .iter()
                .rev()
                .find_map(|i| match i {
                    Inline::Run { source_range, .. }
                    | Inline::Image { source_range, .. }
                    | Inline::OpaqueInline { source_range, .. } => Some(source_range.end),
                    _ => None,
                })
                .unwrap_or(block.source_range.end),
        );
    }
    while source_at.len() < text.len() + 1 {
        source_at.push(*source_at.last().unwrap_or(&block.source_range.end));
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
        block_start: block.source_range.start,
    }
}

pub struct BlockTextElement<H: WysiwygHost> {
    pub editor: Entity<H>,
    pub layout: Arc<LeafLayout>,
    pub font_size: f32,
    pub line_height: f32,
    pub theme: EditorTheme,
}

pub struct Prepaint<H: WysiwygHost> {
    lines: Vec<WrappedLine>,
    cursor: Option<PaintQuad>,
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
        let height = self.measure_height(window, wrap);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
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
        let (selection, cursor) = paint_carets(
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
        if source_in_leaf(&self.layout, caret) {
            window.handle_input(
                &focus,
                ElementInputHandler::new(bounds, self.editor.clone()),
                cx,
            );
        }

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
        let display = if self.layout.text.is_empty() {
            SharedString::from(" ")
        } else {
            self.layout.text.clone().into()
        };
        let mut runs = self.layout.runs.clone();
        if display.as_ref() == " " && runs.is_empty() {
            let style = TextStyle {
                color: self.theme.text,
                font_family: self.theme.font_family.clone().into(),
                font_size: px(self.font_size).into(),
                line_height: px(self.line_height).into(),
                ..Default::default()
            };
            runs = vec![style.to_run(1)];
        }
        let covered: usize = runs.iter().map(|r| r.len).sum();
        if covered != display.len() {
            let style = TextStyle {
                color: self.theme.text,
                font_family: self.theme.font_family.clone().into(),
                font_size: px(self.font_size).into(),
                line_height: px(self.line_height).into(),
                ..Default::default()
            };
            runs = vec![style.to_run(display.len())];
        }
        window
            .text_system()
            .shape_text(
                display,
                px(self.font_size),
                &runs,
                Some(wrap_width.max(px(40.))),
                None,
            )
            .unwrap_or_default()
            .into_iter()
            .collect()
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
}

fn source_in_leaf(layout: &LeafLayout, src: usize) -> bool {
    let start = *layout.source_at.first().unwrap_or(&layout.block_start);
    let end = *layout.source_at.last().unwrap_or(&layout.block_start);
    src >= start && src <= end
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
            let local = point(position.x - bounds.origin.x, position.y - y);
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
) -> (Option<PaintQuad>, Option<PaintQuad>) {
    let _ = text_len;
    let mut cursor = None;
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
                cursor = Some(fill(
                    Bounds::new(
                        point(bounds.origin.x + pos.x, y + pos.y),
                        size(px(2.), line_height),
                    ),
                    caret_color,
                ));
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
    (selection, cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use markrust_core::rich::{import_markdown, IdGen};

    fn layout_for(source: &str) -> LeafLayout {
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
        build_leaf_layout(block, &style, &theme, gpui::FontWeight::NORMAL)
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
    fn image_alt_is_visible_placeholder() {
        let layout = layout_for("![cat](img.png)\n");
        assert!(
            layout.text.contains("cat"),
            "expected alt placeholder, got {:?}",
            layout.text
        );
    }
}
