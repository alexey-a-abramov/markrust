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

use super::hit_test::{click_byte_offset, invert_doc_to_display};
use crate::editor::MarkdownEditor;
use crate::highlight::HighlightKind;
use crate::layout::{
    build_display_layout, line_byte_ranges, source_line_font_size, DisplayLayout, SegmentStyle,
};
use crate::theme::EditorTheme;
use markrust_core::TableRowKind;

/// GPUI custom element that lays out and paints the Markdown editor surface.
pub struct EditorElement {
    pub editor: Entity<MarkdownEditor>,
}

const EDITOR_GUTTER: f32 = 16.0;

#[derive(Clone)]
struct LinePaintData {
    shaped: ShapedLine,
    doc_line_start: usize,
    display_line_start: usize,
    height: Pixels,
    y: Pixels,
    is_code_block: bool,
    is_blockquote: bool,
}

pub struct EditorPrepaint {
    lines: Vec<LinePaintData>,
    selection: Option<PaintQuad>,
    cursor: Option<PaintQuad>,
    caret_bounds: Bounds<Pixels>,
    blockquote_borders: Vec<PaintQuad>,
    code_block_backgrounds: Vec<PaintQuad>,
    display_to_doc: Vec<usize>,
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
        let content = editor.content(cx);
        let spans = editor.document.read(cx).syntax_spans.clone();
        let carets = editor.carets();
        let selections = editor.selections();
        let display_layout = build_display_layout(&content, &spans, &carets, &selections, theme);
        let total_height = total_layout_height(&display_layout, theme, &content);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height =
            px(total_height.max(theme.line_height_for_font_size(theme.font_size))).into();
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

        let lines = shape_lines(window, &display_layout, &theme, &content);

        let cursor_display = display_layout.display_offset_for_doc(cursor_doc);
        let caret_bounds = caret_bounds_for_display(&lines, cursor_display, bounds);
        let selection = if selected_range.is_empty() {
            None
        } else {
            Some(selection_quad(
                &lines,
                &display_layout,
                &selected_range,
                bounds,
                theme.selection,
            ))
        };

        let cursor = if is_focused && cursor_visible {
            Some(cursor_quad(&lines, cursor_display, bounds, theme.caret))
        } else {
            None
        };

        let blockquote_borders = blockquote_border_quads(&lines, bounds, theme.blockquote_border);
        let code_block_backgrounds =
            code_block_background_quads(&lines, bounds, theme.code_block_bg);

        EditorPrepaint {
            lines,
            selection,
            cursor,
            caret_bounds,
            blockquote_borders,
            code_block_backgrounds,
            display_to_doc: invert_doc_to_display(&display_layout),
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

        self.editor.update(cx, |editor, _| {
            editor.report_caret_bounds(prepaint.caret_bounds);
            editor.sync_ime_cursor(window);
        });

        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }

        for background in prepaint.code_block_backgrounds.drain(..) {
            window.paint_quad(background);
        }

        for border in prepaint.blockquote_borders.drain(..) {
            window.paint_quad(border);
        }

        let gutter = px(EDITOR_GUTTER);
        for line in prepaint.lines.iter() {
            let origin = point(bounds.left() + gutter, bounds.top() + line.y);
            line.shaped
                .paint(origin, line.height, TextAlign::Left, None, window, cx)
                .unwrap();
        }

        if let Some(cursor) = prepaint.cursor.take() {
            window.paint_quad(cursor);
        }

        self.editor.update(cx, |editor, _| {
            editor.last_bounds_line_height = prepaint
                .lines
                .first()
                .map(|line| line.height.into())
                .unwrap_or(0.0);
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
            editor.layout_cache.line_heights = prepaint
                .lines
                .iter()
                .map(|line| f32::from(line.height))
                .collect();
            editor.layout_cache.line_x_at = prepaint
                .lines
                .iter()
                .map(|line| x_positions_for_shaped(&line.shaped))
                .collect();
            editor.layout_cache.display_to_doc = prepaint.display_to_doc.clone();
        });
    }
}

fn total_layout_height(layout: &DisplayLayout, theme: &EditorTheme, content: &str) -> f32 {
    let doc_line_ranges = line_byte_ranges(content);
    let display_ranges = line_byte_ranges(&layout.display_text);
    let line_count = display_ranges.len().max(doc_line_ranges.len()).max(1);
    let mut total = 0.0;
    for index in 0..line_count {
        let (doc_start, doc_end) = doc_line_ranges
            .get(index)
            .copied()
            .unwrap_or((0, content.len()));
        let font_size = source_line_font_size(layout, theme, content, doc_start, doc_end);
        total += theme.line_height_for_font_size(font_size);
    }
    total
}

fn x_positions_for_shaped(shaped: &ShapedLine) -> Vec<f32> {
    let n = shaped.text.len();
    let mut xs = vec![0.0f32; n + 1];
    for i in 0..=n {
        if i == n || shaped.text.is_char_boundary(i) {
            xs[i] = f32::from(shaped.x_for_index(i));
        } else if i > 0 {
            xs[i] = xs[i - 1];
        }
    }
    xs
}

fn line_is_style(
    layout: &DisplayLayout,
    doc_start: usize,
    doc_end: usize,
    pred: impl Fn(SegmentStyle) -> bool,
) -> bool {
    layout.segments.iter().any(|segment| {
        segment.doc_end > doc_start && segment.doc_start < doc_end && pred(segment.style)
    })
}

fn shape_lines(
    window: &mut Window,
    layout: &DisplayLayout,
    theme: &EditorTheme,
    content: &str,
) -> Vec<LinePaintData> {
    let line_ranges = line_byte_ranges(&layout.display_text);
    let doc_line_ranges = line_byte_ranges(content);
    let mut lines = Vec::new();
    let mut y = px(0.);

    for (line_idx, (display_start, display_end)) in line_ranges.iter().enumerate() {
        let line_text: SharedString = layout.display_text[*display_start..*display_end]
            .trim_end_matches('\n')
            .into();
        let (doc_start, doc_end) = doc_line_ranges
            .get(line_idx)
            .copied()
            .unwrap_or((0, content.len()));
        let font_size = source_line_font_size(layout, theme, content, doc_start, doc_end);
        let height = px(theme.line_height_for_font_size(font_size));
        let runs = build_runs_for_line(layout, theme, *display_start, *display_end, &line_text);

        let shaped = window
            .text_system()
            .shape_line(line_text.clone(), px(font_size), &runs, None);
        let is_code_block = line_is_style(layout, doc_start, doc_end, |style| {
            matches!(
                style,
                SegmentStyle::CodeBlock | SegmentStyle::SyntaxHighlight(_)
            )
        }) || layout
            .code_block_lines
            .iter()
            .any(|&start| start >= doc_start && start < doc_end.max(doc_start + 1));
        let is_blockquote = line_is_style(layout, doc_start, doc_end, |style| {
            matches!(style, SegmentStyle::BlockQuote)
        }) || layout
            .blockquote_lines
            .iter()
            .any(|&start| start >= doc_start && start < doc_end.max(doc_start + 1));
        lines.push(LinePaintData {
            shaped,
            doc_line_start: doc_start,
            display_line_start: *display_start,
            height,
            y,
            is_code_block,
            is_blockquote,
        });
        y += height;
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
        let height = px(theme.line_height_for_font_size(theme.font_size));
        lines.push(LinePaintData {
            shaped: window
                .text_system()
                .shape_line("".into(), px(theme.font_size), &runs, None),
            doc_line_start: 0,
            display_line_start: 0,
            height,
            y: px(0.),
            is_code_block: false,
            is_blockquote: false,
        });
    }

    lines
}

fn build_runs_for_line(
    layout: &DisplayLayout,
    theme: &EditorTheme,
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
        // Cap at the REMAINING shaped-text length: line_text has the trailing
        // newline trimmed, and run lengths must sum to exactly its length.
        let emitted: usize = pos - display_start;
        let remaining = line_text.len().saturating_sub(emitted);
        let mut len = (run_end - pos).min(remaining);
        // The display projection substitutes multi-byte glyphs (bullets,
        // checkboxes, image markers); a segment boundary can land inside one.
        // Snap forward to the next char boundary of the shaped text.
        while len < remaining && !line_text.is_char_boundary(emitted + len) {
            len += 1;
        }
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

    let covered: usize = runs.iter().map(|r| r.len).sum();
    if covered < line_text.len() {
        runs.push(TextRun {
            len: line_text.len() - covered,
            font: body_font(theme),
            color: theme.text,
            background_color: None,
            underline: None,
            strikethrough: None,
        });
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
        fallbacks: Some(EditorTheme::system_font_fallbacks()),
        weight,
        style: font_style,
    }
}

fn body_font(theme: &EditorTheme) -> gpui::Font {
    gpui::Font {
        family: theme.font_family.clone().into(),
        features: gpui::FontFeatures::default(),
        fallbacks: Some(EditorTheme::system_font_fallbacks()),
        weight: gpui::FontWeight::NORMAL,
        style: gpui::FontStyle::Normal,
    }
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
        SegmentStyle::CodeInline | SegmentStyle::Highlight => Some(theme.code_bg),
        _ => None,
    }
}

pub(crate) fn syntax_color(theme: &EditorTheme, kind: HighlightKind) -> gpui::Hsla {
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
    lines: &[LinePaintData],
    bounds: Bounds<Pixels>,
    color: gpui::Hsla,
) -> Vec<PaintQuad> {
    lines
        .iter()
        .filter(|line| line.is_blockquote)
        .map(|line| {
            fill(
                Bounds::new(
                    point(bounds.left() + px(4.), bounds.top() + line.y),
                    size(px(3.), line.height),
                ),
                color,
            )
        })
        .collect()
}

fn code_block_background_quads(
    lines: &[LinePaintData],
    bounds: Bounds<Pixels>,
    color: gpui::Hsla,
) -> Vec<PaintQuad> {
    lines
        .iter()
        .filter(|line| line.is_code_block)
        .map(|line| {
            fill(
                Bounds::new(
                    point(bounds.left(), bounds.top() + line.y),
                    size(bounds.size.width, line.height),
                ),
                color,
            )
        })
        .collect()
}

fn caret_bounds_for_display(
    lines: &[LinePaintData],
    display_offset: usize,
    bounds: Bounds<Pixels>,
) -> Bounds<Pixels> {
    let (x, y, height) = position_for_display_offset(lines, display_offset);
    Bounds::new(
        point(bounds.left() + px(EDITOR_GUTTER) + x, bounds.top() + y),
        size(px(2.), height),
    )
}

fn cursor_quad(
    lines: &[LinePaintData],
    display_offset: usize,
    bounds: Bounds<Pixels>,
    color: gpui::Hsla,
) -> PaintQuad {
    fill(
        caret_bounds_for_display(lines, display_offset, bounds),
        color,
    )
}

fn selection_quad(
    lines: &[LinePaintData],
    layout: &DisplayLayout,
    range: &Range<usize>,
    bounds: Bounds<Pixels>,
    color: gpui::Hsla,
) -> PaintQuad {
    let start = layout.display_offset_for_doc(range.start);
    let end = layout.display_offset_for_doc(range.end);
    let (start_x, start_y, start_height) = position_for_display_offset(lines, start);
    let (end_x, end_y, end_height) = position_for_display_offset(lines, end);
    let gutter = px(EDITOR_GUTTER);
    if (f32::from(start_y) - f32::from(end_y)).abs() < 0.5 {
        fill(
            Bounds::from_corners(
                point(bounds.left() + gutter + start_x, bounds.top() + start_y),
                point(
                    bounds.left() + gutter + end_x,
                    bounds.top() + start_y + start_height,
                ),
            ),
            color,
        )
    } else {
        fill(
            Bounds::from_corners(
                point(bounds.left() + gutter + start_x, bounds.top() + start_y),
                point(bounds.right(), bounds.top() + end_y + end_height),
            ),
            color,
        )
    }
}

fn position_for_display_offset(
    lines: &[LinePaintData],
    display_offset: usize,
) -> (Pixels, Pixels, Pixels) {
    for line in lines {
        let line_display_end = line.display_line_start + line.shaped.text.len();
        if display_offset <= line_display_end {
            let local = display_offset.saturating_sub(line.display_line_start);
            return (line.shaped.x_for_index(local), line.y, line.height);
        }
    }
    (
        lines
            .last()
            .map(|line| line.shaped.width())
            .unwrap_or(px(0.)),
        lines.last().map(|line| line.y).unwrap_or(px(0.)),
        lines.last().map(|line| line.height).unwrap_or(px(20.)),
    )
}

impl MarkdownEditor {
    pub fn index_for_mouse_position(
        &self,
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
    ) -> usize {
        let relative_x = f32::from(position.x - bounds.left()) - EDITOR_GUTTER;
        let relative_y = f32::from(position.y - bounds.top());
        if !self.layout_cache.line_x_at.is_empty() {
            return click_byte_offset(
                relative_x,
                relative_y,
                &self.layout_cache.line_heights,
                &self.layout_cache.display_line_starts,
                &self.layout_cache.line_x_at,
                &self.layout_cache.display_to_doc,
            );
        }
        if !self.layout_cache.line_heights.is_empty() {
            let mut y = 0.0;
            for (line_idx, height) in self.layout_cache.line_heights.iter().enumerate() {
                if relative_y < y + height || line_idx + 1 == self.layout_cache.line_heights.len() {
                    return self
                        .layout_cache
                        .line_starts
                        .get(line_idx)
                        .copied()
                        .unwrap_or(0);
                }
                y += height;
            }
        }
        let line_height = self.last_bounds_line_height.max(1.0);
        let line_idx = ((relative_y / line_height).floor() as usize)
            .min(self.layout_cache.line_starts.len().saturating_sub(1));
        self.layout_cache
            .line_starts
            .get(line_idx)
            .copied()
            .unwrap_or(0)
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
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::ToggleBold, window, cx| {
                    editor.update(cx, |e, cx| e.toggle_bold(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::ToggleItalic, window, cx| {
                    editor.update(cx, |e, cx| e.toggle_italic(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::ToggleCode, window, cx| {
                    editor.update(cx, |e, cx| e.toggle_code(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::ToggleLink, window, cx| {
                    editor.update(cx, |e, cx| e.toggle_link(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::Indent, window, cx| {
                    editor.update(cx, |e, cx| e.indent(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::Outdent, window, cx| {
                    editor.update(cx, |e, cx| e.outdent(action, window, cx))
                }
            })
            .child(EditorElement::new(editor))
    }
}
