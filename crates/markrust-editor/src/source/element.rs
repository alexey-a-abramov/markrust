// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::cell::Cell;
#[cfg(feature = "gui-tests")]
use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    div, fill, point, prelude::*, px, relative, size, App, Bounds, Context, CursorStyle, Element,
    ElementInputHandler, Entity, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render,
    ScrollHandle, ShapedLine, SharedString, Style, TextAlign, TextRun, Window,
};

use super::hit_test::{click_byte_offset, invert_doc_to_display};
use crate::editor::{LineLayoutCache, MarkdownEditor};
use crate::highlight::HighlightKind;
use crate::layout::{
    build_display_layout, build_raw_display_layout, line_byte_ranges, source_line_font_size,
    DisplayLayout, SegmentStyle,
};
use crate::theme::EditorTheme;
use markrust_core::TableRowKind;
use unicode_segmentation::UnicodeSegmentation;

/// GPUI custom element that lays out and paints the Markdown editor surface.
type SharedViewportSize = Rc<Cell<(Pixels, Pixels)>>;

pub struct EditorElement {
    pub editor: Entity<MarkdownEditor>,
    scroll_handle: Option<(ScrollHandle, SharedViewportSize)>,
    reveal_caret: bool,
    #[cfg(feature = "gui-tests")]
    paint_observation: Option<Rc<RefCell<SourcePaintGeometry>>>,
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
    selection: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    shadow_cursor: Vec<PaintQuad>,
    shadow_selection: Vec<PaintQuad>,
    search_matches: Vec<PaintQuad>,
    search_reveal: Option<Bounds<Pixels>>,
    caret_bounds: Bounds<Pixels>,
    blockquote_borders: Vec<PaintQuad>,
    code_block_backgrounds: Vec<PaintQuad>,
    display_to_doc: Vec<usize>,
}

/// The same shaped source lines determine intrinsic width and painting.
pub struct EditorLayout {
    lines: Vec<LinePaintData>,
    display_layout: DisplayLayout,
    #[cfg(feature = "gui-tests")]
    revision: u64,
}

/// Source observations come from the same shaped rows and paint pass as the UI.
#[cfg(feature = "gui-tests")]
#[derive(Clone, Debug, Default)]
pub struct SourcePaintGeometry {
    pub revision: Option<u64>,
    pub rows: Vec<SourcePaintRow>,
    pub selection_bounds: Vec<Bounds<Pixels>>,
}

#[cfg(feature = "gui-tests")]
#[derive(Clone, Debug)]
pub struct SourcePaintRow {
    /// Source range including its hard newline, when present.
    pub source_range: Range<usize>,
    pub bounds: Bounds<Pixels>,
    /// Source-backed glyph caret stops in window coordinates.
    pub caret_stops: Vec<(usize, f32)>,
}

impl EditorElement {
    pub fn new(editor: Entity<MarkdownEditor>) -> Self {
        Self {
            editor,
            scroll_handle: None,
            reveal_caret: false,
            #[cfg(feature = "gui-tests")]
            paint_observation: None,
        }
    }

    fn with_scroll(
        mut self,
        handle: ScrollHandle,
        viewport_size: SharedViewportSize,
        reveal_caret: bool,
    ) -> Self {
        self.scroll_handle = Some((handle, viewport_size));
        self.reveal_caret = reveal_caret;
        self
    }

    #[cfg(feature = "gui-tests")]
    fn with_paint_observation(mut self, observation: Rc<RefCell<SourcePaintGeometry>>) -> Self {
        self.paint_observation = Some(observation);
        self
    }
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = EditorLayout;
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
        self.editor.update(cx, |editor, cx| {
            editor.document.update(cx, |doc, _cx| {
                doc.apply_pending_parse();
            });
        });
        let editor = self.editor.read(cx);
        let theme = &editor.theme;
        let content = editor.content(cx);
        let doc = editor.document.read(cx);
        #[cfg(feature = "gui-tests")]
        let revision = doc.revision();
        let spans = if doc.mode.parses_markdown() {
            doc.syntax_spans.clone()
        } else {
            Vec::new()
        };
        let carets = editor.carets();
        let selections = editor.selections();
        let display_layout = if editor.raw_source() {
            build_raw_display_layout(&content, &spans, &carets, &selections, theme)
        } else {
            build_display_layout(&content, &spans, &carets, &selections, theme)
        };
        let lines = shape_lines(
            window,
            &display_layout,
            theme,
            &content,
            editor.raw_source(),
        );
        let intrinsic_width = lines
            .iter()
            .map(|line| line.shaped.width)
            .fold(px(0.), Pixels::max)
            + px(EDITOR_GUTTER * 2.);
        let total_height = lines
            .last()
            .map(|line| line.y + line.height)
            .unwrap_or_else(|| px(theme.line_height_for_font_size(theme.font_size)));
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.min_size.width = intrinsic_width.ceil().into();
        style.size.height = total_height.into();
        style.flex_shrink = 0.;
        (
            window.request_layout(style, [], cx),
            EditorLayout {
                lines,
                display_layout,
                #[cfg(feature = "gui-tests")]
                revision,
            },
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
        let editor = self.editor.read(cx);
        let theme = editor.theme.clone();
        let cursor_doc = editor.cursor_offset();
        let selected_range = editor.selected_range.clone();
        let cursor_visible = editor.cursor_visible;
        let is_focused = editor.focus_handle.is_focused(window);
        let shadow = editor
            .shadow_selection()
            .filter(|shadow| !is_focused && shadow.revision == editor.document.read(cx).revision());

        let display_layout = &request_layout.display_layout;
        let lines = request_layout.lines.clone();

        let cursor_display = display_layout.display_offset_for_doc(cursor_doc);
        let caret_bounds = caret_bounds_for_display(&lines, cursor_display, bounds);
        let selection = if selected_range.is_empty() || shadow.is_some() {
            Vec::new()
        } else {
            selection_quads(
                &lines,
                display_layout,
                &selected_range,
                bounds,
                theme.selection,
            )
        };

        let cursor = if is_focused && cursor_visible && selected_range.is_empty() {
            Some(cursor_quad(&lines, cursor_display, bounds, theme.caret))
        } else {
            None
        };
        let shadow_cursor = shadow.map_or_else(Vec::new, |shadow| {
            crate::shadow::cursor_quads(caret_bounds_for_display(
                &lines,
                display_layout.display_offset_for_doc(shadow.caret()),
                bounds,
            ))
        });
        let shadow_selection = shadow.map_or_else(Vec::new, |shadow| {
            selection_quads(
                &lines,
                display_layout,
                &shadow.range,
                bounds,
                crate::shadow::selection_color(),
            )
        });
        let search_matches = editor
            .search_highlights(cx)
            .map_or_else(Vec::new, |search| {
                search
                    .ranges
                    .iter()
                    .enumerate()
                    .flat_map(|(index, range)| {
                        selection_quads(
                            &lines,
                            display_layout,
                            range,
                            bounds,
                            crate::search::match_color(search.active == Some(index)),
                        )
                    })
                    .collect()
            });
        let search_reveal = editor.pending_search_reveal.map(|offset| {
            caret_bounds_for_display(
                &lines,
                display_layout.display_offset_for_doc(offset),
                bounds,
            )
        });

        let blockquote_borders = blockquote_border_quads(&lines, bounds, theme.blockquote_border);
        let code_block_backgrounds =
            code_block_background_quads(&lines, bounds, theme.code_block_bg);

        EditorPrepaint {
            lines,
            selection,
            cursor,
            shadow_cursor,
            shadow_selection,
            search_matches,
            search_reveal,
            caret_bounds,
            blockquote_borders,
            code_block_backgrounds,
            display_to_doc: invert_doc_to_display(display_layout),
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
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

        #[cfg(not(feature = "gui-tests"))]
        let _ = request_layout;

        #[cfg(feature = "gui-tests")]
        if let Some(observation) = &self.paint_observation {
            let content_len = self.editor.read(cx).document.read(cx).buffer.len_bytes();
            let rows = prepaint
                .lines
                .iter()
                .enumerate()
                .map(|(index, line)| {
                    let source_end = prepaint
                        .lines
                        .get(index + 1)
                        .map(|next| next.doc_line_start)
                        .unwrap_or(content_len);
                    let caret_stops = line
                        .shaped
                        .text
                        .grapheme_indices(true)
                        .map(|(offset, _)| offset)
                        .chain([line.shaped.text.len()])
                        .map(|offset| {
                            let source = request_layout
                                .display_layout
                                .doc_offset_for_display(line.display_line_start + offset);
                            let x =
                                bounds.left() + px(EDITOR_GUTTER) + line.shaped.x_for_index(offset);
                            (source, f32::from(x))
                        })
                        .collect();
                    SourcePaintRow {
                        source_range: line.doc_line_start..source_end,
                        bounds: Bounds::new(
                            point(bounds.left() + px(EDITOR_GUTTER), bounds.top() + line.y),
                            size(line.shaped.width(), line.height),
                        ),
                        caret_stops,
                    }
                })
                .collect();
            *observation.borrow_mut() = SourcePaintGeometry {
                revision: Some(request_layout.revision),
                rows,
                selection_bounds: prepaint.selection.iter().map(|quad| quad.bounds).collect(),
            };
        }

        for background in prepaint.code_block_backgrounds.drain(..) {
            window.paint_quad(background);
        }

        for border in prepaint.blockquote_borders.drain(..) {
            window.paint_quad(border);
        }

        for selection in prepaint.search_matches.drain(..) {
            window.paint_quad(selection);
        }
        for selection in prepaint.selection.drain(..) {
            window.paint_quad(selection);
        }
        for selection in prepaint.shadow_selection.drain(..) {
            window.paint_quad(selection);
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
        for cursor in prepaint.shadow_cursor.drain(..) {
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

        // Mouse offsets must use the same content origin that painted the
        // glyphs, not the stationary scroll wrapper. The viewport admits
        // clicks below the last row while keeping other Split panes out.
        let mouse_bounds = self
            .scroll_handle
            .as_ref()
            .map(|(scroll, _)| scroll.bounds())
            .unwrap_or(bounds)
            .intersect(&window.content_mask().bounds);
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if !phase.bubble()
                    || event.button != MouseButton::Left
                    || !mouse_bounds.contains(&event.position)
                {
                    return;
                }
                editor.update(cx, |editor, cx| {
                    editor.focus_handle.focus(window, cx);
                    editor.on_mouse_down(event, bounds, cx);
                });
                window.prevent_default();
                cx.stop_propagation();
            }
        });
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if !phase.bubble()
                    || event.pressed_button != Some(MouseButton::Left)
                    || !editor.read(cx).is_selecting
                    || !editor.read(cx).focus_handle.is_focused(window)
                {
                    return;
                }
                editor.update(cx, |editor, cx| editor.on_mouse_move(event, bounds, cx));
                window.prevent_default();
                cx.stop_propagation();
            }
        });
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |event: &MouseUpEvent, phase, window, cx| {
                if phase.bubble()
                    && event.button == MouseButton::Left
                    && editor.read(cx).is_selecting
                    && editor.read(cx).focus_handle.is_focused(window)
                {
                    editor.update(cx, |editor, cx| editor.on_mouse_up(event, bounds, cx));
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }
        });

        if let Some((scroll, last_viewport_size)) = &self.scroll_handle {
            let viewport = scroll.bounds();
            let size = (viewport.size.width, viewport.size.height);
            let resized = last_viewport_size.replace(size) != size;
            let reveal_bounds = prepaint.search_reveal.or_else(|| {
                ((self.reveal_caret || resized) && focus_handle.is_focused(window))
                    .then_some(prepaint.caret_bounds)
            });
            if let Some(reveal_bounds) = reveal_bounds {
                let offset = horizontal_caret_scroll_offset(
                    scroll.offset(),
                    viewport,
                    reveal_bounds,
                    scroll.max_offset().x,
                );
                let offset = vertical_caret_scroll_offset(
                    offset,
                    viewport,
                    reveal_bounds,
                    scroll.max_offset().y,
                );
                if offset != scroll.offset() {
                    scroll.set_offset(offset);
                    self.editor.update(cx, |_editor, cx| cx.notify());
                }
                if prepaint.search_reveal.is_some() {
                    self.editor
                        .update(cx, |editor, _| editor.pending_search_reveal = None);
                }
            }
        }
    }
}

fn horizontal_caret_scroll_offset(
    offset: Point<Pixels>,
    viewport: Bounds<Pixels>,
    caret: Bounds<Pixels>,
    max_offset: Pixels,
) -> Point<Pixels> {
    let margin = px(EDITOR_GUTTER).min(viewport.size.width / 2.);
    let adjustment = if caret.left() >= viewport.left() && caret.right() <= viewport.right() {
        // A visible caret is not a request to move the user's viewport.
        px(0.)
    } else if caret.left() < viewport.left() + margin {
        viewport.left() + margin - caret.left()
    } else if caret.right() > viewport.right() - margin {
        viewport.right() - margin - caret.right()
    } else {
        px(0.)
    };
    point((offset.x + adjustment).clamp(-max_offset, px(0.)), offset.y)
}

fn vertical_caret_scroll_offset(
    offset: Point<Pixels>,
    viewport: Bounds<Pixels>,
    caret: Bounds<Pixels>,
    max_offset: Pixels,
) -> Point<Pixels> {
    let margin = px(EDITOR_GUTTER).min(viewport.size.height / 2.);
    let adjustment = if caret.top() >= viewport.top() && caret.bottom() <= viewport.bottom() {
        px(0.)
    } else if caret.top() < viewport.top() + margin {
        viewport.top() + margin - caret.top()
    } else if caret.bottom() > viewport.bottom() - margin {
        viewport.bottom() - margin - caret.bottom()
    } else {
        px(0.)
    };
    point(offset.x, (offset.y + adjustment).clamp(-max_offset, px(0.)))
}

fn x_positions_for_shaped(shaped: &ShapedLine) -> Vec<f32> {
    let n = shaped.text.len();
    // Non-caret bytes must not win a nearest-position tie inside a combining
    // sequence or ZWJ emoji. Hit testing skips nonfinite entries.
    let mut xs = vec![f32::NAN; n + 1];
    for i in shaped
        .text
        .grapheme_indices(true)
        .map(|(i, _)| i)
        .chain([n])
    {
        xs[i] = f32::from(shaped.x_for_index(i));
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
    raw_source: bool,
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
        let font_size =
            source_row_font_size(layout, theme, content, doc_start, doc_end, raw_source);
        let height = px(theme.line_height_for_font_size(font_size));
        let runs = build_runs_for_line(
            layout,
            theme,
            *display_start,
            *display_end,
            &line_text,
            raw_source,
        );

        let shaped = window
            .text_system()
            .shape_line(line_text.clone(), px(font_size), &runs, None);
        let is_code_block = !raw_source
            && (line_is_style(layout, doc_start, doc_end, |style| {
                matches!(
                    style,
                    SegmentStyle::CodeBlock | SegmentStyle::SyntaxHighlight(_)
                )
            }) || layout
                .code_block_lines
                .iter()
                .any(|&start| start >= doc_start && start < doc_end.max(doc_start + 1)));
        let is_blockquote = !raw_source
            && (line_is_style(layout, doc_start, doc_end, |style| {
                matches!(style, SegmentStyle::BlockQuote)
            }) || layout
                .blockquote_lines
                .iter()
                .any(|&start| start >= doc_start && start < doc_end.max(doc_start + 1)));
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
            font: source_font(theme, SegmentStyle::Plain, raw_source),
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

fn source_row_font_size(
    layout: &DisplayLayout,
    theme: &EditorTheme,
    content: &str,
    doc_start: usize,
    doc_end: usize,
    raw_source: bool,
) -> f32 {
    if raw_source {
        theme.font_size
    } else {
        source_line_font_size(layout, theme, content, doc_start, doc_end)
    }
}

fn build_runs_for_line(
    layout: &DisplayLayout,
    theme: &EditorTheme,
    display_start: usize,
    display_end: usize,
    line_text: &str,
    raw_source: bool,
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
            font: source_font(theme, style, raw_source),
            color: styled_color(theme, style),
            background_color: if raw_source {
                None
            } else {
                styled_background(theme, style)
            },
            underline: if raw_source {
                None
            } else {
                styled_underline(style)
            },
            strikethrough: if raw_source {
                None
            } else {
                styled_strikethrough(style)
            },
        });
        pos += len;
    }

    let covered: usize = runs.iter().map(|r| r.len).sum();
    if covered < line_text.len() {
        runs.push(TextRun {
            len: line_text.len() - covered,
            font: source_font(theme, SegmentStyle::Plain, raw_source),
            color: theme.text,
            background_color: None,
            underline: None,
            strikethrough: None,
        });
    }

    if runs.is_empty() {
        runs.push(TextRun {
            len: line_text.len(),
            font: source_font(theme, SegmentStyle::Plain, raw_source),
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
        | SegmentStyle::Table { .. }
        | SegmentStyle::Math => theme.code_font_family.clone(),
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
        SegmentStyle::Italic
        | SegmentStyle::BlockQuote
        | SegmentStyle::Image
        | SegmentStyle::Math => gpui::FontStyle::Italic,
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

fn source_font(theme: &EditorTheme, style: SegmentStyle, raw_source: bool) -> gpui::Font {
    if !raw_source {
        return styled_font(theme, style);
    }
    gpui::Font {
        family: theme.code_font_family.clone().into(),
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

fn selection_quads(
    lines: &[LinePaintData],
    layout: &DisplayLayout,
    range: &Range<usize>,
    bounds: Bounds<Pixels>,
    color: gpui::Hsla,
) -> Vec<PaintQuad> {
    let start = layout.display_offset_for_doc(range.start);
    let end = layout.display_offset_for_doc(range.end);
    let gutter = px(EDITOR_GUTTER);
    selected_source_rows(
        &(start..end),
        lines.iter().enumerate().map(|(index, line)| {
            (
                line.display_line_start,
                line.display_line_start + line.shaped.text.len(),
                lines
                    .get(index + 1)
                    .map(|next| next.display_line_start)
                    .unwrap_or(layout.display_text.len()),
            )
        }),
    )
    .into_iter()
    .map(|(index, selected, newline)| {
        let line = &lines[index];
        let left = line.shaped.x_for_index(selected.start);
        let right = line.shaped.x_for_index(selected.end) + if newline { px(2.) } else { px(0.) };
        fill(
            Bounds::new(
                point(bounds.left() + gutter + left, bounds.top() + line.y),
                size((right - left).max(px(0.)), line.height),
            ),
            color,
        )
    })
    .collect()
}

/// Keep partial first/last lines and selected newlines independent. A single
/// bounding rectangle highlights unselected text while missing later prefixes.
fn selected_source_rows(
    selection: &Range<usize>,
    rows: impl IntoIterator<Item = (usize, usize, usize)>,
) -> Vec<(usize, Range<usize>, bool)> {
    if selection.is_empty() {
        return Vec::new();
    }
    rows.into_iter()
        .enumerate()
        .filter_map(|(index, (start, text_end, next_start))| {
            let selected_start = selection.start.max(start).min(text_end);
            let selected_end = selection.end.min(text_end).max(start);
            let newline =
                text_end < next_start && selection.start <= text_end && selection.end > text_end;
            (selected_start < selected_end || newline).then_some((
                index,
                selected_start.saturating_sub(start)..selected_end.saturating_sub(start),
                newline,
            ))
        })
        .collect()
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
        source_offset_for_mouse_position(
            &self.layout_cache,
            self.last_bounds_line_height,
            position,
            bounds,
        )
    }

    pub fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;
        self.mouse_selection_anchor = None;
        let offset = self.index_for_mouse_position(event.position, bounds);
        if event.click_count >= 2 && !event.modifiers.shift {
            let content = self.content(cx);
            let range = source_range_for_mouse_click(&content, offset, event.click_count);
            self.mouse_selection_anchor = Some((range.clone(), event.click_count));
            self.apply_command(
                crate::EditorCommand::SetSelection {
                    start: range.start,
                    end: range.end,
                },
                cx,
            );
        } else if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
        self.reset_blink(cx);
    }

    pub fn on_mouse_up(
        &mut self,
        event: &MouseUpEvent,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.is_selecting {
            // MouseMove delivery can be coalesced. The release coordinate,
            // not the last intermediate event, defines the final extent.
            let offset = self.index_for_mouse_position(event.position, bounds);
            self.extend_mouse_selection_to(offset, cx);
        }
        self.is_selecting = false;
        self.mouse_selection_anchor = None;
    }

    pub fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.is_selecting {
            let offset = self.index_for_mouse_position(event.position, bounds);
            self.extend_mouse_selection_to(offset, cx);
        }
    }

    fn extend_mouse_selection_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if let Some((anchor, count)) = self.mouse_selection_anchor.clone() {
            let extent = source_range_for_mouse_click(&self.content(cx), offset, count);
            let (start, end) = multiclick_drag_selection(&anchor, &extent);
            self.apply_command(crate::EditorCommand::SetSelection { start, end }, cx);
        } else {
            self.select_to(offset, cx);
        }
    }
}

/// Keep the initially selected word/line intact despite pointer jitter, and
/// extend by the same unit while preserving the original drag direction.
fn multiclick_drag_selection(anchor: &Range<usize>, extent: &Range<usize>) -> (usize, usize) {
    if extent.start < anchor.start {
        (anchor.end, extent.start)
    } else {
        (anchor.start, anchor.end.max(extent.end))
    }
}

fn source_offset_for_mouse_position(
    cache: &LineLayoutCache,
    fallback_line_height: f32,
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
) -> usize {
    let relative_x = f32::from(position.x - bounds.left()) - EDITOR_GUTTER;
    let relative_y = f32::from(position.y - bounds.top());
    if !cache.line_x_at.is_empty() {
        return click_byte_offset(
            relative_x,
            relative_y,
            &cache.line_heights,
            &cache.display_line_starts,
            &cache.line_x_at,
            &cache.display_to_doc,
        );
    }
    if !cache.line_heights.is_empty() {
        let mut y = 0.0;
        for (line_idx, height) in cache.line_heights.iter().enumerate() {
            if relative_y < y + height || line_idx + 1 == cache.line_heights.len() {
                return cache.line_starts.get(line_idx).copied().unwrap_or(0);
            }
            y += height;
        }
    }
    let line_height = fallback_line_height.max(1.0);
    let line_idx = ((relative_y / line_height).floor() as usize)
        .min(cache.line_starts.len().saturating_sub(1));
    cache.line_starts.get(line_idx).copied().unwrap_or(0)
}

fn source_range_for_mouse_click(content: &str, offset: usize, click_count: usize) -> Range<usize> {
    let mut offset = offset.min(content.len());
    while !content.is_char_boundary(offset) {
        offset -= 1;
    }
    if click_count >= 3 {
        let start = content[..offset]
            .rfind('\n')
            .map_or(0, |newline| newline + 1);
        let end = content[offset..]
            .find('\n')
            .map_or(content.len(), |newline| offset + newline + 1);
        return start..end;
    }
    content
        .split_word_bound_indices()
        .find_map(|(start, word)| {
            let end = start + word.len();
            (start <= offset && (offset < end || offset == end && end == content.len()))
                .then_some(start..end)
        })
        .unwrap_or(offset..offset)
}

/// Render wrapper that attaches keyboard/mouse handlers to the editor element.
pub struct MarkdownEditorView {
    pub editor: Entity<MarkdownEditor>,
    scroll_handle: ScrollHandle,
    scroll_viewport_size: SharedViewportSize,
    last_revealed_caret: Option<(usize, u64, bool)>,
    preserve_next_focus_scroll: bool,
    #[cfg(feature = "gui-tests")]
    paint_observation: Rc<RefCell<SourcePaintGeometry>>,
}

impl MarkdownEditorView {
    pub fn new(editor: Entity<MarkdownEditor>) -> Self {
        Self {
            editor,
            scroll_handle: ScrollHandle::new(),
            scroll_viewport_size: Rc::new(Cell::new((px(0.), px(0.)))),
            last_revealed_caret: None,
            preserve_next_focus_scroll: false,
            #[cfg(feature = "gui-tests")]
            paint_observation: Rc::new(RefCell::new(SourcePaintGeometry::default())),
        }
    }

    /// Returning from an auxiliary input is not document navigation.
    pub fn preserve_scroll_on_next_focus(&mut self) {
        self.preserve_next_focus_scroll = true;
    }

    pub fn scroll_offset(&self) -> Point<Pixels> {
        self.scroll_handle.offset()
    }

    pub fn restore_scroll_offset(&mut self, offset: Point<Pixels>, cx: &mut Context<Self>) {
        self.scroll_handle.set_offset(offset);
        self.preserve_next_focus_scroll = true;
        cx.notify();
    }

    #[cfg(feature = "gui-tests")]
    pub fn horizontal_scroll_state(&self) -> (Bounds<Pixels>, Pixels, Point<Pixels>) {
        let viewport = self.scroll_handle.bounds();
        (
            viewport,
            viewport.size.width + self.scroll_handle.max_offset().x,
            self.scroll_handle.offset(),
        )
    }

    #[cfg(feature = "gui-tests")]
    pub fn painted_geometry(&self) -> SourcePaintGeometry {
        self.paint_observation.borrow().clone()
    }
}

impl Render for MarkdownEditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.editor.clone();
        let state = editor.read(cx);
        let caret_state = (
            state.cursor_offset(),
            state.document.read(cx).revision(),
            state.focus_handle.is_focused(window),
        );
        let reveal_caret = self.last_revealed_caret != Some(caret_state)
            && !std::mem::take(&mut self.preserve_next_focus_scroll);
        self.last_revealed_caret = Some(caret_state);
        div()
            .id("source-editor-scroll")
            .size_full()
            .min_w_0()
            .overflow_scroll()
            .track_scroll(&self.scroll_handle)
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
                move |action: &crate::editor::SelectPageUp, window, cx| {
                    editor.update(cx, |e, cx| e.select_page_up(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SelectPageDown, window, cx| {
                    editor.update(cx, |e, cx| e.select_page_down(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::WordLeft, window, cx| {
                    editor.update(cx, |e, cx| e.word_left(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::WordRight, window, cx| {
                    editor.update(cx, |e, cx| e.word_right(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SelectWordLeft, window, cx| {
                    editor.update(cx, |e, cx| e.select_word_left(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SelectWordRight, window, cx| {
                    editor.update(cx, |e, cx| e.select_word_right(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::DocumentHome, window, cx| {
                    editor.update(cx, |e, cx| e.document_home(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::DocumentEnd, window, cx| {
                    editor.update(cx, |e, cx| e.document_end(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SelectDocumentHome, window, cx| {
                    editor.update(cx, |e, cx| e.select_document_home(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SelectDocumentEnd, window, cx| {
                    editor.update(cx, |e, cx| e.select_document_end(action, window, cx))
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
                move |action: &crate::editor::Copy, window, cx| {
                    editor.update(cx, |e, cx| e.copy(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::Cut, window, cx| {
                    editor.update(cx, |e, cx| e.cut(action, window, cx))
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
                move |action: &crate::editor::DeleteWordLeft, window, cx| {
                    editor.update(cx, |e, cx| e.delete_word_left(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::DeleteWordRight, window, cx| {
                    editor.update(cx, |e, cx| e.delete_word_right(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::DeleteToLineStart, window, cx| {
                    editor.update(cx, |e, cx| e.delete_to_line_start(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::DeleteToLineEnd, window, cx| {
                    editor.update(cx, |e, cx| e.delete_to_line_end(action, window, cx))
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
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::InsertLineBreak, window, cx| {
                    editor.update(cx, |e, cx| e.insert_line_break(action, window, cx))
                }
            })
            // Block-level commands: the source editor routes these through
            // the headless dispatcher (Noop for the WYSIWYG-only ones), but
            // keeping the bindings live ensures menus and keyboard shortcuts
            // resolve without crashing even when Source mode is active.
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SetHeading1, window, cx| {
                    editor.update(cx, |e, cx| e.set_heading_1(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SetHeading2, window, cx| {
                    editor.update(cx, |e, cx| e.set_heading_2(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SetHeading3, window, cx| {
                    editor.update(cx, |e, cx| e.set_heading_3(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SetHeading4, window, cx| {
                    editor.update(cx, |e, cx| e.set_heading_4(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SetHeading5, window, cx| {
                    editor.update(cx, |e, cx| e.set_heading_5(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::SetHeading6, window, cx| {
                    editor.update(cx, |e, cx| e.set_heading_6(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::Paragraph, window, cx| {
                    editor.update(cx, |e, cx| e.paragraph(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::ToggleBlockquote, window, cx| {
                    editor.update(cx, |e, cx| e.toggle_blockquote(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::ToggleUnorderedList, window, cx| {
                    editor.update(cx, |e, cx| e.toggle_unordered_list(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::ToggleOrderedList, window, cx| {
                    editor.update(cx, |e, cx| e.toggle_ordered_list(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::ToggleTaskList, window, cx| {
                    editor.update(cx, |e, cx| e.toggle_task_list(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::ToggleStrikethrough, window, cx| {
                    editor.update(cx, |e, cx| e.toggle_strikethrough(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::InsertHorizontalRule, window, cx| {
                    editor.update(cx, |e, cx| e.insert_horizontal_rule(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::InsertCodeBlock, window, cx| {
                    editor.update(cx, |e, cx| e.insert_code_block(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::InsertImage, window, cx| {
                    editor.update(cx, |e, cx| e.insert_image(action, window, cx))
                }
            })
            .on_action({
                let editor = editor.clone();
                move |action: &crate::editor::InsertTable, window, cx| {
                    editor.update(cx, |e, cx| e.insert_table(action, window, cx))
                }
            })
            .child({
                let element = EditorElement::new(editor).with_scroll(
                    self.scroll_handle.clone(),
                    self.scroll_viewport_size.clone(),
                    reveal_caret,
                );
                #[cfg(feature = "gui-tests")]
                let element = element.with_paint_observation(self.paint_observation.clone());
                element
            })
    }
}

#[cfg(test)]
mod scroll_tests {
    use super::*;

    #[test]
    fn raw_source_has_uniform_monospace_metrics_and_only_color_styling() {
        use crate::layout::LayoutSegment;

        let theme = EditorTheme::dark();
        let content = "# Heading\n**bold** [link](url)";
        let layout = DisplayLayout {
            display_text: content.into(),
            doc_to_display: (0..=content.len()).map(Some).collect(),
            segments: vec![
                LayoutSegment {
                    doc_start: 0,
                    doc_end: 9,
                    style: SegmentStyle::Heading { level: 1 },
                },
                LayoutSegment {
                    doc_start: 10,
                    doc_end: 18,
                    style: SegmentStyle::Bold,
                },
                LayoutSegment {
                    doc_start: 18,
                    doc_end: content.len(),
                    style: SegmentStyle::Link,
                },
            ],
            highlight_spans: vec![],
            blockquote_lines: vec![],
            code_block_lines: vec![],
        };
        for (start, end) in line_byte_ranges(content) {
            assert_eq!(
                source_row_font_size(&layout, &theme, content, start, end, true),
                theme.font_size
            );
            let text = content[start..end].trim_end_matches('\n');
            let runs = build_runs_for_line(&layout, &theme, start, end, text, true);
            assert_eq!(runs.iter().map(|run| run.len).sum::<usize>(), text.len());
            for run in runs {
                assert_eq!(run.font.family.as_ref(), theme.code_font_family);
                assert_eq!(run.font.weight, gpui::FontWeight::NORMAL);
                assert_eq!(run.font.style, gpui::FontStyle::Normal);
                assert!(run.background_color.is_none());
                assert!(run.underline.is_none());
                assert!(run.strikethrough.is_none());
            }
        }
        assert!(source_row_font_size(&layout, &theme, content, 0, 9, false) > theme.font_size);
        assert_eq!(styled_color(&theme, SegmentStyle::Link), theme.link);
    }

    #[test]
    fn mouse_hit_uses_painted_origin_after_scroll_and_cyrillic_byte_stops() {
        let text = "# H\nЭто прекрасно аффы";
        let start = text.find("Это").unwrap();
        let word = text.find("аффы").unwrap();
        let mut xs = vec![0.; text.len() - start + 1];
        for (column, (byte, ch)) in text[start..].char_indices().enumerate() {
            for x in &mut xs[byte..byte + ch.len_utf8()] {
                *x = column as f32 * 8.;
            }
        }
        *xs.last_mut().unwrap() = text[start..].chars().count() as f32 * 8.;
        let word_x = text[start..word].chars().count() as f32 * 8.;
        let cache = LineLayoutCache {
            line_starts: vec![0, start],
            display_line_starts: vec![0, start],
            line_heights: vec![24., 24.],
            line_x_at: vec![vec![0., 8., 16., 24.], xs],
            display_to_doc: (0..=text.len()).collect(),
        };
        let origin = Bounds::new(point(px(-180.), px(-48.)), size(px(600.), px(48.)));
        for (offset, x) in [(word, word_x), (word + "аффы".len(), word_x + 32.)] {
            let position = point(
                origin.left() + px(EDITOR_GUTTER + x),
                origin.top() + px(30.),
            );
            assert_eq!(
                source_offset_for_mouse_position(&cache, 24., position, origin),
                offset
            );
        }
        let after = point(origin.left() + px(800.), origin.top() + px(500.));
        assert_eq!(
            source_offset_for_mouse_position(&cache, 24., after, origin),
            text.len()
        );
    }

    #[test]
    fn double_click_selects_cyrillic_word_and_triple_click_literal_source_line() {
        let text = "# Heading\nЭто прекрасно аффы\nlast";
        let word = text.find("аффы").unwrap();
        assert_eq!(
            source_range_for_mouse_click(text, word + 3, 2),
            word..word + "аффы".len()
        );
        let row = text.find("Это").unwrap();
        let row_end = text.find("\nlast").unwrap() + 1;
        assert_eq!(source_range_for_mouse_click(text, word, 3), row..row_end);
        assert_eq!(
            source_range_for_mouse_click(text, text.len(), 2),
            text.len() - 4..text.len()
        );
        assert_eq!(source_range_for_mouse_click("", 1, 2), 0..0);
    }

    #[test]
    fn double_click_jitter_keeps_whole_word_and_drag_extends_wordwise_both_directions() {
        let text = "Это аффы прекрасно";
        let word = text.find("аффы").unwrap();
        let anchor = source_range_for_mouse_click(text, word + 2, 2);
        for offset in word..word + "аффы".len() {
            let extent = source_range_for_mouse_click(text, offset, 2);
            assert_eq!(
                multiclick_drag_selection(&anchor, &extent),
                (anchor.start, anchor.end)
            );
        }
        let next = source_range_for_mouse_click(text, text.find("прекрасно").unwrap() + 2, 2);
        assert_eq!(
            multiclick_drag_selection(&anchor, &next),
            (anchor.start, text.len())
        );
        let previous = source_range_for_mouse_click(text, 2, 2);
        assert_eq!(
            multiclick_drag_selection(&anchor, &previous),
            (anchor.end, 0)
        );
    }

    #[test]
    fn triple_click_drag_preserves_complete_lines_in_both_directions() {
        let text = "first\nsecond\nthird";
        let anchor = source_range_for_mouse_click(text, 8, 3);
        let same = source_range_for_mouse_click(text, 10, 3);
        assert_eq!(multiclick_drag_selection(&anchor, &same), (6, 13));
        let previous = source_range_for_mouse_click(text, 2, 3);
        assert_eq!(multiclick_drag_selection(&anchor, &previous), (13, 0));
        let next = source_range_for_mouse_click(text, 15, 3);
        assert_eq!(multiclick_drag_selection(&anchor, &next), (6, text.len()));
    }

    #[test]
    fn multiline_selection_keeps_partial_edges_and_complete_middle_line() {
        assert_eq!(
            selected_source_rows(&(3..25), [(0, 10, 11), (11, 21, 22), (22, 32, 32)]),
            vec![(0, 3..10, true), (1, 0..10, true), (2, 0..3, false)]
        );
    }

    #[test]
    fn selection_ending_at_next_line_start_does_not_highlight_that_line() {
        assert_eq!(
            selected_source_rows(&(2..11), [(0, 10, 11), (11, 21, 21)]),
            vec![(0, 2..10, true)]
        );
    }

    #[test]
    fn selection_of_empty_line_or_only_a_newline_has_visible_marker() {
        assert_eq!(
            selected_source_rows(&(3..5), [(0, 3, 4), (4, 4, 5), (5, 8, 8)]),
            vec![(0, 3..3, true), (1, 0..0, true)]
        );
    }

    #[test]
    fn single_line_selection_is_limited_to_selected_glyphs() {
        assert_eq!(
            selected_source_rows(&(13..17), [(0, 10, 11), (11, 21, 21)]),
            vec![(1, 2..6, false)]
        );
        assert!(selected_source_rows(&(13..13), [(0, 10, 11), (11, 21, 21)]).is_empty());
    }

    #[test]
    fn unicode_selection_row_offsets_remain_byte_based() {
        let text = "Café 👩🏽‍💻\nnext";
        let newline = text.find('\n').unwrap();
        let emoji = text.find('👩').unwrap();
        assert_eq!(
            selected_source_rows(
                &(emoji..text.len() - 2),
                [
                    (0, newline, newline + 1),
                    (newline + 1, text.len(), text.len())
                ],
            ),
            vec![(0, emoji..newline, true), (1, 0..2, false)]
        );
    }

    #[test]
    fn long_line_caret_scrolls_into_view_and_home_restores_left_edge() {
        let viewport = Bounds::new(point(px(100.), px(50.)), size(px(300.), px(200.)));
        let end = Bounds::new(point(px(800.), px(70.)), size(px(2.), px(24.)));
        let offset =
            horizontal_caret_scroll_offset(point(px(0.), px(-40.)), viewport, end, px(600.));
        assert_eq!(offset, point(px(-418.), px(-40.)));
        let home = Bounds::new(point(px(116.) + offset.x, px(70.)), size(px(2.), px(24.)));
        assert_eq!(
            horizontal_caret_scroll_offset(offset, viewport, home, px(600.)),
            point(px(0.), px(-40.))
        );
    }

    #[test]
    fn visible_caret_preserves_manual_scroll_and_offsets_are_bounded() {
        let viewport = Bounds::new(point(px(0.), px(0.)), size(px(300.), px(200.)));
        let visible = Bounds::new(point(px(100.), px(50.)), size(px(2.), px(24.)));
        let offset = point(px(-100.), px(-30.));
        assert_eq!(
            horizontal_caret_scroll_offset(offset, viewport, visible, px(600.)),
            offset
        );
        let far = Bounds::new(point(px(2000.), px(50.)), size(px(2.), px(24.)));
        assert_eq!(
            horizontal_caret_scroll_offset(offset, viewport, far, px(600.)),
            point(px(-600.), px(-30.))
        );
    }

    #[test]
    fn vertical_caret_scrolls_into_view_without_resetting_horizontal_offset() {
        let viewport = Bounds::new(point(px(100.), px(50.)), size(px(300.), px(200.)));
        let bottom = Bounds::new(point(px(160.), px(500.)), size(px(2.), px(24.)));
        let offset =
            vertical_caret_scroll_offset(point(px(-40.), px(0.)), viewport, bottom, px(600.));
        assert_eq!(offset, point(px(-40.), px(-290.)));
        let top = Bounds::new(point(px(160.), px(66.) + offset.y), size(px(2.), px(24.)));
        assert_eq!(
            vertical_caret_scroll_offset(offset, viewport, top, px(600.)),
            point(px(-40.), px(0.))
        );
    }

    #[test]
    fn vertical_scroll_preserves_visible_caret_and_clamps_to_content() {
        let viewport = Bounds::new(point(px(0.), px(0.)), size(px(300.), px(200.)));
        let visible = Bounds::new(point(px(100.), px(50.)), size(px(2.), px(24.)));
        let offset = point(px(-30.), px(-100.));
        assert_eq!(
            vertical_caret_scroll_offset(offset, viewport, visible, px(600.)),
            offset
        );
        let far = Bounds::new(point(px(100.), px(2000.)), size(px(2.), px(24.)));
        assert_eq!(
            vertical_caret_scroll_offset(offset, viewport, far, px(600.)),
            point(px(-30.), px(-600.))
        );
    }

    #[test]
    fn fully_visible_caret_near_pane_edges_does_not_trigger_comfort_scroll() {
        let viewport = Bounds::new(point(px(20.), px(50.)), size(px(300.), px(200.)));
        let offset = point(px(-100.), px(-200.));
        for position in [
            point(px(21.), px(51.)),
            point(px(317.), px(225.)),
            point(px(20.), px(50.)),
            point(px(318.), px(226.)),
        ] {
            let caret = Bounds::new(position, size(px(2.), px(24.)));
            assert_eq!(
                horizontal_caret_scroll_offset(offset, viewport, caret, px(600.)),
                offset
            );
            assert_eq!(
                vertical_caret_scroll_offset(offset, viewport, caret, px(600.)),
                offset
            );
        }
    }
}
