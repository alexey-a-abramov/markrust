// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::ops::Range;

use gpui::{
    div, fill, point, prelude::*, px, relative, size, App, Bounds, Context, CursorStyle, Element,
    ElementInputHandler, Entity, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render, ShapedLine,
    SharedString, Style, TextAlign, TextRun, Window,
};

use crate::editor::MarkdownEditor;
use crate::highlight::HighlightKind;
use crate::layout::{build_display_layout, line_byte_ranges, DisplayLayout, SegmentStyle};
use crate::theme::EditorTheme;
use markrust_core::TableRowKind;

/// GPUI custom element that lays out and paints the Markdown editor surface.
pub struct EditorElement {
    pub editor: Entity<MarkdownEditor>,
}

#[derive(Clone)]
struct LinePaintData {
    shaped: ShapedLine,
    doc_line_start: usize,
    display_line_start: usize,
}

pub struct EditorPrepaint {
    lines: Vec<LinePaintData>,
    selection: Option<PaintQuad>,
    cursor: Option<PaintQuad>,
    blockquote_borders: Vec<PaintQuad>,
    line_height: Pixels,
}

impl EditorElement {
    pub fn new(editor: Entity<MarkdownEditor>) -> Self {
        Self { editor }
    }
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = EditorPrepaint;

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
        let editor = self.editor.read(cx);
        let theme = &editor.theme;
        let line_height = px(theme.stable_line_height(theme.font_size));
        let content = editor.content(cx);
        let line_count = line_byte_ranges(&content).len().max(1);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (line_height * line_count as f32).into();
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
        let editor = self.editor.read(cx);
        let theme = editor.theme.clone();
        let content = editor.content(cx);
        let carets = editor.carets();
        let selections = editor.selections();
        let cursor_doc = editor.cursor_offset();
        let selected_range = editor.selected_range.clone();
        let cursor_visible = editor.cursor_visible;
        let is_focused = editor.focus_handle.is_focused(window);

        let spans = {
            let mut spans = Vec::new();
            self.editor.update(cx, |editor, cx| {
                editor.document.update(cx, |doc, _| {
                    doc.apply_pending_parse();
                    if doc.mode.parses_markdown() {
                        spans = doc.syntax_spans.clone();
                    }
                });
            });
            spans
        };

        let display_layout = build_display_layout(&content, &spans, &carets, &selections, &theme);

        let line_height = px(theme.stable_line_height(theme.font_size));
        let font_size = px(theme.font_size);
        let lines = shape_lines(window, &display_layout, &theme, font_size, &content);

        let cursor_display = display_layout.display_offset_for_doc(cursor_doc);
        let selection = if selected_range.is_empty() {
            None
        } else {
            Some(selection_quad(
                &lines,
                &display_layout,
                &selected_range,
                bounds,
                line_height,
                theme.selection,
            ))
        };

        let cursor = if is_focused && cursor_visible {
            Some(cursor_quad(
                &lines,
                cursor_display,
                bounds,
                line_height,
                theme.caret,
            ))
        } else {
            None
        };

        let blockquote_borders = blockquote_border_quads(
            &display_layout,
            &lines,
            bounds,
            line_height,
            theme.blockquote_border,
        );

        EditorPrepaint {
            lines,
            selection,
            cursor,
            blockquote_borders,
            line_height,
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
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );

        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }

        for border in prepaint.blockquote_borders.drain(..) {
            window.paint_quad(border);
        }

        for (index, line) in prepaint.lines.iter().enumerate() {
            let origin = point(
                bounds.left(),
                bounds.top() + prepaint.line_height * index as f32,
            );
            line.shaped
                .paint(
                    origin,
                    prepaint.line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                )
                .unwrap();
        }

        if let Some(cursor) = prepaint.cursor.take() {
            window.paint_quad(cursor);
        }

        self.editor.update(cx, |editor, _| {
            editor.last_bounds_line_height = prepaint.line_height.into();
            editor.layout_cache.line_starts = prepaint
                .lines
                .iter()
                .map(|line| line.doc_line_start)
                .collect();
            editor.layout_cache.display_line_starts = prepaint
                .lines
                .iter()
                .map(|line| line.display_line_start)
                .collect();
        });
    }
}

fn shape_lines(
    window: &mut Window,
    layout: &DisplayLayout,
    theme: &EditorTheme,
    font_size: Pixels,
    content: &str,
) -> Vec<LinePaintData> {
    let line_ranges = line_byte_ranges(&layout.display_text);
    let doc_line_ranges = line_byte_ranges(content);
    let mut lines = Vec::new();

    for (line_idx, (display_start, display_end)) in line_ranges.iter().enumerate() {
        let line_text: SharedString = layout.display_text[*display_start..*display_end]
            .trim_end_matches('\n')
            .into();
        let runs = build_runs_for_line(
            layout,
            theme,
            font_size,
            *display_start,
            *display_end,
            &line_text,
        );
        let shaped = window
            .text_system()
            .shape_line(line_text.clone(), font_size, &runs, None);
        let (doc_line_start, _) = doc_line_ranges
            .get(line_idx)
            .copied()
            .unwrap_or((0, content.len()));
        lines.push(LinePaintData {
            shaped,
            doc_line_start,
            display_line_start: *display_start,
        });
    }

    if lines.is_empty() {
        let runs = vec![TextRun {
            len: 0,
            font: body_font(theme),
            color: theme.text,
            background_color: None,
            underline: None,
            strikethrough: None,
        }];
        lines.push(LinePaintData {
            shaped: window
                .text_system()
                .shape_line("".into(), font_size, &runs, None),
            doc_line_start: 0,
            display_line_start: 0,
        });
    }

    lines
}

fn build_runs_for_line(
    layout: &DisplayLayout,
    theme: &EditorTheme,
    _font_size: Pixels,
    display_start: usize,
    display_end: usize,
    line_text: &str,
) -> Vec<TextRun> {
    let mut runs = Vec::new();
    let mut pos = display_start;
    while pos < display_end && pos < layout.display_text.len() {
        let doc_offset = layout.doc_offset_for_display(pos);
        let segment = layout
            .segments
            .iter()
            .find(|segment| doc_offset >= segment.doc_start && doc_offset < segment.doc_end);
        let style = segment.map(|s| s.style).unwrap_or(SegmentStyle::Plain);
        let segment_end_doc = segment
            .map(|s| s.doc_end)
            .unwrap_or(layout.doc_to_display.len());
        let segment_end_display = layout.display_offset_for_doc(segment_end_doc);
        let run_end = segment_end_display.min(display_end).max(pos + 1);
        let len = (run_end - pos).min(line_text.len());
        if len == 0 {
            break;
        }
        runs.push(TextRun {
            len,
            font: styled_font(theme, style),
            color: styled_color(theme, style),
            background_color: styled_background(theme, style),
            underline: styled_underline(style),
            strikethrough: styled_strikethrough(style),
        });
        pos += len;
    }

    if runs.is_empty() {
        runs.push(TextRun {
            len: line_text.len(),
            font: body_font(theme),
            color: theme.text,
            background_color: None,
            underline: None,
            strikethrough: None,
        });
    }
    runs
}

fn styled_font(theme: &EditorTheme, style: SegmentStyle) -> gpui::Font {
    let family = match style {
        SegmentStyle::CodeInline
        | SegmentStyle::CodeBlock
        | SegmentStyle::SyntaxHighlight(_)
        | SegmentStyle::Table { .. } => theme.code_font_family.clone(),
        _ => theme.font_family.clone(),
    };
    let weight = match style {
        SegmentStyle::Bold
        | SegmentStyle::Heading { .. }
        | SegmentStyle::Table {
            row: TableRowKind::Header,
        }
        | SegmentStyle::TaskList { checked: true } => gpui::FontWeight::BOLD,
        _ => gpui::FontWeight::NORMAL,
    };
    let font_style = match style {
        SegmentStyle::Italic | SegmentStyle::BlockQuote | SegmentStyle::Image => {
            gpui::FontStyle::Italic
        }
        _ => gpui::FontStyle::Normal,
    };
    gpui::Font {
        family: family.into(),
        features: gpui::FontFeatures::default(),
        fallbacks: None,
        weight,
        style: font_style,
    }
}

fn body_font(theme: &EditorTheme) -> gpui::Font {
    gpui::font(theme.font_family.clone())
}

fn styled_color(theme: &EditorTheme, style: SegmentStyle) -> gpui::Hsla {
    match style {
        SegmentStyle::Delimiter { visible: true } => theme.delimiter,
        SegmentStyle::Delimiter { visible: false } => gpui::transparent_black(),
        SegmentStyle::BlockQuote => theme.blockquote_text,
        SegmentStyle::Link => theme.link,
        SegmentStyle::Image => theme.image_text,
        SegmentStyle::Frontmatter => theme.frontmatter_text,
        SegmentStyle::Table {
            row: TableRowKind::Delimiter,
        } => theme.table_delimiter,
        SegmentStyle::TaskList { checked: false } => theme.text,
        SegmentStyle::SyntaxHighlight(kind) => syntax_color(theme, kind),
        _ => theme.text,
    }
}

fn styled_background(theme: &EditorTheme, style: SegmentStyle) -> Option<gpui::Hsla> {
    match style {
        SegmentStyle::Table {
            row: TableRowKind::Header,
        } => Some(theme.table_header_bg),
        SegmentStyle::CodeInline | SegmentStyle::CodeBlock => {
            Some(gpui::hsla(0., 0., 0.08, 1.))
        }
        _ => None,
    }
}

fn syntax_color(theme: &EditorTheme, kind: HighlightKind) -> gpui::Hsla {
    match kind {
        HighlightKind::Keyword => theme.syntax_keyword,
        HighlightKind::String => theme.syntax_string,
        HighlightKind::Number => theme.syntax_number,
        HighlightKind::Comment => theme.syntax_comment,
        HighlightKind::Function => theme.syntax_function,
        HighlightKind::Type | HighlightKind::Property => theme.syntax_type,
        HighlightKind::Punctuation | HighlightKind::Plain => theme.text,
    }
}

fn styled_underline(style: SegmentStyle) -> Option<gpui::UnderlineStyle> {
    if matches!(style, SegmentStyle::Link) {
        Some(gpui::UnderlineStyle {
            thickness: px(1.),
            color: None,
            wavy: false,
        })
    } else {
        None
    }
}

fn styled_strikethrough(style: SegmentStyle) -> Option<gpui::StrikethroughStyle> {
    if matches!(style, SegmentStyle::Strikethrough) {
        Some(gpui::StrikethroughStyle {
            thickness: px(1.),
            color: None,
        })
    } else {
        None
    }
}

fn blockquote_border_quads(
    layout: &DisplayLayout,
    lines: &[LinePaintData],
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    color: gpui::Hsla,
) -> Vec<PaintQuad> {
    let mut quads = Vec::new();
    for &line_start in &layout.blockquote_lines {
        let display_start = layout.display_offset_for_doc(line_start);
        let (line_idx, _) = position_for_display_offset(lines, display_start);
        quads.push(fill(
            Bounds::new(
                point(bounds.left(), bounds.top() + line_height * line_idx as f32),
                size(px(3.), line_height),
            ),
            color,
        ));
    }
    quads
}

fn cursor_quad(
    lines: &[LinePaintData],
    display_offset: usize,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    color: gpui::Hsla,
) -> PaintQuad {
    let (line_idx, x) = position_for_display_offset(lines, display_offset);
    fill(
        Bounds::new(
            point(
                bounds.left() + x,
                bounds.top() + line_height * line_idx as f32,
            ),
            size(px(2.), line_height),
        ),
        color,
    )
}

fn selection_quad(
    lines: &[LinePaintData],
    layout: &DisplayLayout,
    range: &Range<usize>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    color: gpui::Hsla,
) -> PaintQuad {
    let start = layout.display_offset_for_doc(range.start);
    let end = layout.display_offset_for_doc(range.end);
    let (start_line, start_x) = position_for_display_offset(lines, start);
    let (end_line, end_x) = position_for_display_offset(lines, end);
    if start_line == end_line {
        fill(
            Bounds::from_corners(
                point(
                    bounds.left() + start_x,
                    bounds.top() + line_height * start_line as f32,
                ),
                point(
                    bounds.left() + end_x,
                    bounds.top() + line_height * (start_line + 1) as f32,
                ),
            ),
            color,
        )
    } else {
        fill(
            Bounds::from_corners(
                point(
                    bounds.left() + start_x,
                    bounds.top() + line_height * start_line as f32,
                ),
                point(
                    bounds.right(),
                    bounds.top() + line_height * (end_line + 1) as f32,
                ),
            ),
            color,
        )
    }
}

fn position_for_display_offset(lines: &[LinePaintData], display_offset: usize) -> (usize, Pixels) {
    for (index, line) in lines.iter().enumerate() {
        let line_display_end = line.display_line_start + line.shaped.text.len();
        if display_offset <= line_display_end {
            let local = display_offset.saturating_sub(line.display_line_start);
            return (index, line.shaped.x_for_index(local));
        }
    }
    let last = lines.len().saturating_sub(1);
    (
        last,
        lines
            .last()
            .map(|line| line.shaped.width())
            .unwrap_or(px(0.)),
    )
}

impl MarkdownEditor {
    pub fn index_for_mouse_position(
        &self,
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
    ) -> usize {
        let relative_y = position.y - bounds.top();
        let line_height = px(self.last_bounds_line_height.max(1.0));
        let line_idx = ((relative_y / line_height).floor() as usize)
            .min(self.layout_cache.line_starts.len().saturating_sub(1));
        let doc_line_start = self
            .layout_cache
            .line_starts
            .get(line_idx)
            .copied()
            .unwrap_or(0);
        doc_line_start
    }

    pub fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;
        let offset = self.index_for_mouse_position(event.position, bounds);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
        self.reset_blink(cx);
    }

    pub fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    pub fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.is_selecting {
            let offset = self.index_for_mouse_position(event.position, bounds);
            self.select_to(offset, cx);
        }
    }
}

/// Render wrapper that attaches keyboard/mouse handlers to the editor element.
pub struct MarkdownEditorView {
    pub editor: Entity<MarkdownEditor>,
}

impl MarkdownEditorView {
    pub fn new(editor: Entity<MarkdownEditor>) -> Self {
        Self { editor }
    }
}

impl Render for MarkdownEditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.editor.clone();
        div()
            .size_full()
            .bg(self.editor.read(cx).theme.background)
            .key_context("MarkdownEditor")
            .track_focus(&self.editor.read(cx).focus_handle.clone())
            .cursor(CursorStyle::IBeam)
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::Left, window, cx| {
                    editor.update(cx, |e, cx| e.left(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::Right, window, cx| {
                    editor.update(cx, |e, cx| e.right(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::Up, window, cx| {
                    editor.update(cx, |e, cx| e.up(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::Down, window, cx| {
                    editor.update(cx, |e, cx| e.down(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SelectLeft, window, cx| {
                    editor.update(cx, |e, cx| e.select_left(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SelectRight, window, cx| {
                    editor.update(cx, |e, cx| e.select_right(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SelectUp, window, cx| {
                    editor.update(cx, |e, cx| e.select_up(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SelectDown, window, cx| {
                    editor.update(cx, |e, cx| e.select_down(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::Home, window, cx| {
                    editor.update(cx, |e, cx| e.home(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::End, window, cx| {
                    editor.update(cx, |e, cx| e.end(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SelectHome, window, cx| {
                    editor.update(cx, |e, cx| e.select_home(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SelectEnd, window, cx| {
                    editor.update(cx, |e, cx| e.select_end(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::PageUp, window, cx| {
                    editor.update(cx, |e, cx| e.page_up(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::PageDown, window, cx| {
                    editor.update(cx, |e, cx| e.page_down(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SelectAll, window, cx| {
                    editor.update(cx, |e, cx| e.select_all(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::Backspace, window, cx| {
                    editor.update(cx, |e, cx| e.backspace(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::Delete, window, cx| {
                    editor.update(cx, |e, cx| e.delete(action, window, cx))
                }
            })
            .child(EditorElement::new(editor))
    }
}
