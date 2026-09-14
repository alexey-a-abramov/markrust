// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IME candidate origin for the WYSIWYG surface.
//!
//! Two platform paths must agree on the same rectangle:
//!
//! - **Pull:** GPUI asks [`EntityInputHandler::bounds_for_range`] (macOS
//!   `firstRectForCharacterRange:` at `gpui_macos` `window.rs`) for the OS
//!   candidate window. AppKit has no push-caret API.
//! - **Push:** after a caret move, widget focus, or composition change, the
//!   view calls `Window::invalidate_character_coordinates` (GPUI's equivalent
//!   of `set_ime_cursor_position`). That schedules a next-frame
//!   `InputHandler::selected_bounds` → `PlatformWindow::update_ime_position`.
//!   On macOS `update_ime_position` still **discards the Bounds** and only
//!   calls `NSTextInputContext invalidateCharacterCoordinates`; the OS then
//!   pulls `firstRectForCharacterRange:` → `bounds_for_range`. Linux/Windows
//!   use the pushed bounds. GPUI's `TestWindow::update_ime_position` is a
//!   no-op, so tests assert the payload via
//!   [`ImeOriginState::take_platform_push`] plus
//!   [`gpui::PlatformInputHandler::compute_ime_candidate_bounds`].
//!
//! `Render::render` calls [`ImeOriginState::begin_frame`] before children
//! paint. The macOS pull can land in that gap, so the last **painted** caret
//! (body, wrapped line, table cell, or overlay inner `|`) is sticky across
//! `begin_frame`. It must not fall back to the leaf top-left or the overlay
//! trailing edge. An unfocused leaf must not push that leftover sticky rect
//! to Linux/Windows.
//!
//! Leaves and chip/caption/frontmatter widgets report geometry as they paint;
//! this module resolves that noise into **one** caret rect from the focused
//! widget or the leaf that owns the document caret — not whichever text leaf
//! happened to paint last. Blink-off frames keep the last painted caret
//! instead of jumping to the leaf origin.
//!
//! Composition tests drive the same [`gpui::EntityInputHandler`] methods the OS
//! IME uses (`replace_and_mark_text_in_range`, `selected_text_range`,
//! `bounds_for_range`, `replace_text_in_range`). That is not a real CJK
//! candidate window.

use std::ops::Range;
use std::sync::Arc;

use gpui::{point, px, size, Bounds, Pixels, Point, UTF16Selection};
use unicode_segmentation::UnicodeSegmentation;

use super::block_text::LeafLayout;

/// One source-backed caret stop on a painted visual line.
///
/// `visible` is the byte offset in [`LeafLayout::text`]; `source` is the
/// corresponding Markdown byte offset. Keeping both avoids treating hidden
/// Markdown chrome as a visible column during vertical navigation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisualCaretStop {
    pub visible: usize,
    pub source: usize,
    /// Window-relative horizontal glyph position in pixels.
    pub x: f32,
}

/// Geometry for one rendered, soft-wrapped text row.
///
/// This is recorded while the text leaf paints. It deliberately stores only
/// lightweight source/position data rather than a GPUI `WrappedLine`, so key
/// handling can use the most recent paint without reshaping text or needing a
/// `Window` borrow.
#[derive(Debug, Clone, PartialEq)]
pub struct VisualLine {
    /// Inclusive visible caret bounds for this row. These are offsets, not a
    /// slicing range: a soft-wrap boundary belongs to both adjacent rows.
    pub visible_start: usize,
    pub visible_end: usize,
    /// Window-relative top edge in pixels.
    pub top: f32,
    pub height: f32,
    pub stops: Vec<VisualCaretStop>,
}

/// Result of moving by rendered rows. `source` is absent at the viewport
/// edge, where the caller should retain its source-level fallback.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisualVerticalTarget {
    pub source: Option<usize>,
    /// The original horizontal intent, kept even after landing on a short row.
    pub preferred_x: f32,
}

struct ImeVisualLine {
    layout: Arc<LeafLayout>,
    line: VisualLine,
}

/// Who owns the IME origin this frame.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImeOwner {
    Widget,
    Leaf,
}

/// One painted text leaf's layout plus the caret rect it computed (if any).
#[derive(Clone)]
pub struct ImeLeafHit {
    pub layout: Arc<LeafLayout>,
    pub bounds: Bounds<Pixels>,
    pub font_size: f32,
    pub line_height: f32,
    pub caret_bounds: Option<Bounds<Pixels>>,
}

/// Per-frame IME geometry. Leaf/widget hits are cleared at the start of each
/// render; sticky caret and generation persist across blink-off frames.
#[derive(Default)]
pub struct ImeOriginState {
    widget_focused: bool,
    widget_bounds: Option<Bounds<Pixels>>,
    /// Inner `|` / caret quad inside the overlay. Prefer this over the
    /// trailing overlay edge so a mid-draft caret is not the widget's right.
    widget_caret: Option<Bounds<Pixels>>,
    caret_source: usize,
    leaves: Vec<ImeLeafHit>,
    /// Recent rendered rows used by WYSIWYG Up/Down. Like `leaves`, this is
    /// frame-local: a virtualized, unpainted target falls back to the rich
    /// engine's source-level vertical move.
    visual_lines: Vec<ImeVisualLine>,
    /// Non-text painted surfaces (standalone images, thematic rules) so a
    /// leftover click is below them rather than on them.
    painted_bounds: Vec<Bounds<Pixels>>,
    /// Preedit string this frame (`None` = not composing). A change bumps
    /// [`Self::caret_generation`] so composition start/update/commit still
    /// invalidates the platform IME even when the caret rect is unchanged.
    composition: Option<String>,
    /// Last painted caret of the focused leaf or overlay inner `|`.
    /// Survives blink-off frames and `begin_frame` (hits cleared before
    /// paint) so macOS `firstRectForCharacterRange:` / `bounds_for_range`
    /// does not jump to the leaf top-left or overlay trailing edge.
    sticky_caret: Option<Bounds<Pixels>>,
    /// Bumps when the caret source, widget-focus flag, or composition key
    /// changes so a move still pushes even if two surfaces share a rectangle.
    caret_generation: u64,
    last_pushed_generation: Option<u64>,
    last_platform_origin: Option<Bounds<Pixels>>,
}

impl ImeOriginState {
    pub fn begin_frame(
        &mut self,
        widget_focused: bool,
        caret_source: usize,
        composition: Option<&str>,
    ) {
        let owner_changed = self.widget_focused != widget_focused;
        let caret_moved = owner_changed || self.caret_source != caret_source;
        let composition_changed = self.composition.as_deref() != composition;
        if caret_moved || composition_changed {
            self.caret_generation = self.caret_generation.wrapping_add(1);
        }
        // Snapshot the last painted caret before clearing per-frame hits.
        // Keep it across in-surface caret moves (slightly stale is the
        // previous glyph). Drop it when the IME owner changes so an overlay
        // rect cannot leak onto the body, and vice versa.
        if owner_changed {
            self.sticky_caret = None;
        } else if let Some(rect) = self.painted_caret_rect() {
            self.sticky_caret = Some(rect);
        }
        self.widget_focused = widget_focused;
        self.caret_source = caret_source;
        self.composition = composition.map(str::to_string);
        self.widget_bounds = None;
        self.widget_caret = None;
        self.leaves.clear();
        self.visual_lines.clear();
        self.painted_bounds.clear();
    }

    pub fn widget_focused(&self) -> bool {
        self.widget_focused
    }

    pub fn report_widget(&mut self, bounds: Bounds<Pixels>) {
        self.widget_bounds = Some(bounds);
    }

    pub fn report_widget_caret(&mut self, caret: Bounds<Pixels>) {
        self.widget_caret = Some(caret);
        if self.widget_focused {
            if let Some(rect) = self.painted_caret_rect() {
                self.sticky_caret = Some(rect);
            }
        }
    }

    pub fn report_leaf(&mut self, leaf: ImeLeafHit) {
        self.leaves.push(leaf);
    }

    /// Record the visual rows just painted for one source leaf.
    pub fn report_visual_lines(&mut self, layout: Arc<LeafLayout>, lines: Vec<VisualLine>) {
        self.visual_lines
            .extend(lines.into_iter().map(|line| ImeVisualLine {
                layout: layout.clone(),
                line,
            }));
    }

    /// Invalidate paint-derived navigation after the document or rendering
    /// metrics change. Using stale source maps for an edited document would
    /// be worse than the source-level fallback.
    pub fn clear_visual_navigation(&mut self) {
        self.visual_lines.clear();
    }

    /// Find the adjacent painted visual row and the caret stop closest to the
    /// requested x-coordinate. The caller persists `preferred_x` across
    /// repeated Up/Down presses, so a short row does not permanently pull the
    /// caret left.
    ///
    /// This intentionally operates only on freshly painted rows. A
    /// virtualized list does not necessarily have the next offscreen block's
    /// glyph geometry; returning `source: None` lets the editor use its
    /// existing source-level fallback instead of inventing a layout.
    pub fn visual_vertical_target(
        &self,
        caret: usize,
        delta: i32,
        preferred_x: Option<f32>,
    ) -> Option<VisualVerticalTarget> {
        if delta == 0 || self.visual_lines.is_empty() {
            return None;
        }

        let mut current: Option<(usize, usize, usize, bool)> = None;
        for (index, entry) in self.visual_lines.iter().enumerate() {
            let visible = entry.layout.visible_for_source(caret);
            if visible < entry.line.visible_start || visible > entry.line.visible_end {
                continue;
            }
            let span = entry.layout.source_span_len();
            // At a soft-wrap boundary, use the preceding row. GPUI's
            // `position_for_index` paints that shared boundary on the first
            // matching row, so navigation must use the same affinity.
            let ends_here = entry.line.visible_end == visible;
            let better = match current {
                None => true,
                Some((_, best_span, _, best_ends_here)) => {
                    span < best_span || (span == best_span && ends_here && !best_ends_here)
                }
            };
            if better {
                current = Some((index, span, visible, ends_here));
            }
        }
        let (current_index, _, current_visible, _) = current?;
        let current_line = &self.visual_lines[current_index].line;
        let current_x = current_line
            .stops
            .iter()
            .find(|stop| stop.visible == current_visible && stop.source == caret)
            .or_else(|| {
                current_line
                    .stops
                    .iter()
                    .find(|stop| stop.visible == current_visible)
            })
            .or_else(|| current_line.stops.first())
            .map(|stop| stop.x)?;
        let preferred_x = preferred_x.unwrap_or(current_x);

        // Several leaves can share a flex row (for example, text around an
        // image or table cells). Coalesce only exact/near-exact top edges;
        // this keeps ordinary adjacent wrapped rows separate.
        let mut ordered: Vec<usize> = (0..self.visual_lines.len()).collect();
        ordered.sort_by(|left, right| {
            self.visual_lines[*left]
                .line
                .top
                .total_cmp(&self.visual_lines[*right].line.top)
                .then_with(|| {
                    self.visual_lines[*left]
                        .line
                        .visible_start
                        .cmp(&self.visual_lines[*right].line.visible_start)
                })
        });
        let mut rows: Vec<Vec<usize>> = Vec::new();
        for index in ordered {
            let top = self.visual_lines[index].line.top;
            if let Some(last) = rows.last_mut() {
                let row_top = self.visual_lines[last[0]].line.top;
                if (top - row_top).abs() <= 0.5 {
                    last.push(index);
                    continue;
                }
            }
            rows.push(vec![index]);
        }
        let current_row = rows.iter().position(|row| row.contains(&current_index))?;
        let target_row = current_row as i64 + i64::from(delta);
        if target_row < 0 || target_row >= rows.len() as i64 {
            return Some(VisualVerticalTarget {
                source: None,
                preferred_x,
            });
        }
        let target = rows[target_row as usize]
            .iter()
            .flat_map(|index| self.visual_lines[*index].line.stops.iter())
            .min_by(|left, right| {
                (left.x - preferred_x)
                    .abs()
                    .total_cmp(&(right.x - preferred_x).abs())
                    .then_with(|| left.x.total_cmp(&right.x))
                    .then_with(|| left.source.cmp(&right.source))
            });
        Some(VisualVerticalTarget {
            source: target.map(|stop| stop.source),
            preferred_x,
        })
    }

    pub fn report_painted_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.painted_bounds.push(bounds);
    }

    #[cfg(test)]
    pub fn owner(&self) -> Option<ImeOwner> {
        if self.widget_focused {
            if self.widget_bounds.is_some() {
                return Some(ImeOwner::Widget);
            }
            return None;
        }
        self.focused_leaf().map(|_| ImeOwner::Leaf)
    }

    /// Caret rectangle the OS IME should follow.
    ///
    /// Prefers this frame's painted inner caret / leaf caret, then the sticky
    /// origin from the last paint (macOS may pull `bounds_for_range` after
    /// [`Self::begin_frame`] and before children paint). Trailing-edge and
    /// leaf-top fallbacks are last resort only.
    pub fn caret_rect(&self) -> Option<Bounds<Pixels>> {
        if let Some(rect) = self.painted_caret_rect() {
            return Some(rect);
        }
        if let Some(rect) = self.sticky_caret {
            return Some(rect);
        }
        if self.widget_focused {
            return self.widget_bounds.map(widget_caret_rect);
        }
        self.focused_leaf().map(leaf_origin_fallback)
    }

    /// Inner overlay `|` or the focused leaf's painted caret this frame.
    ///
    /// Does not include sticky, trailing-edge, or leaf-top fallbacks.
    fn painted_caret_rect(&self) -> Option<Bounds<Pixels>> {
        if self.widget_focused {
            return self
                .widget_caret
                .filter(|bounds| !is_empty_ime_rect(*bounds));
        }
        self.focused_leaf()
            .and_then(|leaf| leaf.caret_bounds)
            .filter(|bounds| !is_empty_ime_rect(*bounds))
    }

    fn this_frame_reported_owner(&self) -> bool {
        if self.widget_focused {
            self.widget_caret.is_some()
        } else {
            self.focused_leaf().is_some()
        }
    }

    /// Resolved origin to push to the platform IME cursor API.
    ///
    /// Returns `Some` when the origin changed since the last push (caret
    /// move, widget focus, composition change, or a new painted rect). The
    /// view must call [`gpui::Window::invalidate_character_coordinates`] —
    /// not only wait for `bounds_for_range`. That is the GPUI equivalent of
    /// `set_ime_cursor_position`; the test window does not record it.
    ///
    /// Does not push a leftover sticky rect before this frame has reported
    /// the focused leaf or overlay caret (an earlier unfocused leaf must
    /// not send the previous glyph to Linux/Windows).
    pub fn take_platform_push(&mut self) -> Option<Bounds<Pixels>> {
        if let Some(painted) = self.painted_caret_rect() {
            self.sticky_caret = Some(painted);
        }
        if !self.this_frame_reported_owner() {
            return None;
        }
        let rect = self.caret_rect()?;
        if is_empty_ime_rect(rect) {
            return None;
        }
        let same_generation = self.last_pushed_generation == Some(self.caret_generation);
        let same_rect = self.last_platform_origin == Some(rect);
        if same_generation && same_rect {
            return None;
        }
        self.last_pushed_generation = Some(self.caret_generation);
        self.last_platform_origin = Some(rect);
        Some(rect)
    }

    pub fn focused_leaf(&self) -> Option<&ImeLeafHit> {
        focused_leaf_index(&self.leaves, self.caret_source).map(|i| &self.leaves[i])
    }

    pub fn leaf_at_point(&self, point: Point<Pixels>) -> Option<&ImeLeafHit> {
        self.leaves.iter().find(|leaf| leaf.bounds.contains(&point))
    }

    /// True when `point` is in leftover viewport below the last painted leaf
    /// or non-text widget (standalone image, thematic rule). Clicks on a
    /// leaf, those widgets, or a focused overlay are not leftover.
    pub fn point_is_below_painted_content(&self, point: Point<Pixels>) -> bool {
        if self.widget_bounds.is_some_and(|b| b.contains(&point)) {
            return false;
        }
        if self.leaves.iter().any(|leaf| leaf.bounds.contains(&point)) {
            return false;
        }
        if self.painted_bounds.iter().any(|b| b.contains(&point)) {
            return false;
        }
        let leaf_bottom = self.leaves.iter().map(|leaf| leaf.bounds.bottom()).max();
        let extra_bottom = self.painted_bounds.iter().map(|b| b.bottom()).max();
        match (leaf_bottom, extra_bottom) {
            (None, None) => true,
            (Some(a), Some(b)) => point.y >= a.max(b),
            (Some(a), None) => point.y >= a,
            (None, Some(b)) => point.y >= b,
        }
    }
}

/// Fallback origin: trailing edge of a chip / caption / frontmatter overlay.
///
/// Used only when the overlay has not reported an inner `|` / caret quad yet.
/// Production paint reports [`ImeOriginState::report_widget_caret`] so a
/// mid-draft caret is not this right-edge rectangle.
pub fn widget_caret_rect(widget: Bounds<Pixels>) -> Bounds<Pixels> {
    let h = widget.size.height.min(px(22.)).max(px(2.));
    Bounds {
        origin: point(
            widget.origin.x + widget.size.width,
            widget.origin.y + widget.size.height - h,
        ),
        size: size(px(2.), h),
    }
}

/// Last-resort origin when no leaf/widget reported a caret this frame.
pub fn caret_from_element_bounds(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds {
        origin: bounds.origin,
        size: size(px(2.), bounds.size.height.min(px(24.))),
    }
}

/// True when a rect cannot anchor an IME candidate window.
fn is_empty_ime_rect(bounds: Bounds<Pixels>) -> bool {
    bounds.size.width <= px(0.) || bounds.size.height <= px(0.)
}

/// Pull path used by [`gpui::EntityInputHandler::bounds_for_range`].
///
/// On macOS this is what `firstRectForCharacterRange:` forwards after
/// converting window-relative GPUI bounds to screen coordinates. Prefer a
/// sticky caret over the element fallback so a pull between `begin_frame`
/// and paint still sits on the `|`.
pub(super) fn ime_origin_bounds(
    ime: &ImeOriginState,
    element_bounds: Bounds<Pixels>,
) -> Bounds<Pixels> {
    if let Some(caret) = ime
        .caret_rect()
        .filter(|bounds| !is_empty_ime_rect(*bounds))
    {
        return caret;
    }
    if ime.widget_focused() {
        return widget_caret_rect(element_bounds);
    }
    caret_from_element_bounds(element_bounds)
}

pub(super) fn offset_from_utf16(content: &str, offset: usize) -> usize {
    let mut utf8_offset = 0;
    let mut utf16_count = 0;
    for ch in content.chars() {
        if utf16_count >= offset {
            break;
        }
        utf16_count += ch.len_utf16();
        utf8_offset += ch.len_utf8();
    }
    utf8_offset
}

pub(super) fn offset_to_utf16(content: &str, offset: usize) -> usize {
    let mut utf16_offset = 0;
    let mut utf8_count = 0;
    for ch in content.chars() {
        if utf8_count >= offset {
            break;
        }
        utf8_count += ch.len_utf8();
        utf16_offset += ch.len_utf16();
    }
    utf16_offset
}

pub(super) fn body_selected_text_range(
    content: &str,
    selected: Range<usize>,
    reversed: bool,
) -> UTF16Selection {
    UTF16Selection {
        range: offset_to_utf16(content, selected.start)..offset_to_utf16(content, selected.end),
        reversed,
    }
}

pub(super) fn widget_selected_text_range(
    draft: &str,
    caret: usize,
    anchor: usize,
    preedit: Option<&str>,
) -> UTF16Selection {
    let mut at = caret.min(draft.len());
    if !draft.is_char_boundary(at) {
        at = draft.len();
    }
    let pre_len = offset_to_utf16(preedit.unwrap_or(""), preedit.unwrap_or("").len());
    if pre_len > 0 {
        let start = offset_to_utf16(draft, at);
        return UTF16Selection {
            range: start..start + pre_len,
            reversed: false,
        };
    }
    let mut from = anchor.min(draft.len());
    if !draft.is_char_boundary(from) {
        from = draft.len();
    }
    let start = offset_to_utf16(draft, at.min(from));
    let end = offset_to_utf16(draft, at.max(from));
    UTF16Selection {
        range: start..end,
        reversed: at < from,
    }
}

pub(super) fn set_preedit(slot: &mut Option<String>, new_text: &str) {
    *slot = if new_text.is_empty() {
        None
    } else {
        Some(new_text.to_string())
    };
}

pub(super) fn begin_body_preedit(
    preedit: &mut Option<String>,
    marked_range: &mut Option<Range<usize>>,
    new_text: &str,
    caret: usize,
) {
    set_preedit(preedit, new_text);
    *marked_range = Some(caret..caret);
}

pub(super) fn clear_composition(
    preedit: &mut Option<String>,
    widget_preedit: &mut Option<String>,
    marked_range: &mut Option<Range<usize>>,
) {
    *preedit = None;
    *widget_preedit = None;
    *marked_range = None;
}

pub(super) fn apply_replace_range_to_selection(
    content: &str,
    range_utf16: Option<Range<usize>>,
    marked_range: &mut Option<Range<usize>>,
    selected: &mut Range<usize>,
    reversed: &mut bool,
) {
    if let Some(range_utf16) = range_utf16 {
        let start = offset_from_utf16(content, range_utf16.start);
        let end = offset_from_utf16(content, range_utf16.end);
        *selected = start..end;
        *reversed = false;
    } else if let Some(marked) = marked_range.take() {
        *selected = marked;
        *reversed = false;
    }
}

pub(super) fn replace_in_widget_draft(
    draft: &mut String,
    caret: &mut usize,
    range_utf16: Option<Range<usize>>,
    new_text: &str,
    fallback: Range<usize>,
) {
    let (start, end) = if let Some(range_utf16) = range_utf16 {
        let content = draft.clone();
        let start =
            clamp_grapheme_boundary(&content, offset_from_utf16(&content, range_utf16.start));
        let end = clamp_grapheme_boundary(&content, offset_from_utf16(&content, range_utf16.end))
            .max(start);
        (start, end)
    } else {
        let start = clamp_grapheme_boundary(draft, fallback.start);
        let end = clamp_grapheme_boundary(draft, fallback.end).max(start);
        (start, end)
    };
    draft.replace_range(start..end, new_text);
    *caret = start + new_text.len();
}

/// AppKit reports UTF-16 scalar offsets. Widget editing exposes visible text,
/// so an offset inside a combining sequence or ZWJ emoji must not split it.
fn clamp_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    if offset == text.len() {
        return offset;
    }
    text.grapheme_indices(true)
        .map(|(start, _)| start)
        .take_while(|start| *start <= offset)
        .last()
        .unwrap_or(0)
}

fn leaf_origin_fallback(leaf: &ImeLeafHit) -> Bounds<Pixels> {
    Bounds {
        origin: leaf.bounds.origin,
        size: size(
            px(2.),
            px(leaf.line_height)
                .min(leaf.bounds.size.height)
                .max(px(2.)),
        ),
    }
}

/// Pick the leaf that owns `caret`, ignoring paint order.
///
/// Prefer a leaf that actually painted a caret, then the tightest source
/// span, then the first reported leaf (document order). A later decoy
/// with a disjoint range cannot steal the origin.
fn focused_leaf_index(leaves: &[ImeLeafHit], caret: usize) -> Option<usize> {
    let mut best: Option<(usize, usize, bool)> = None;
    for (i, leaf) in leaves.iter().enumerate() {
        if !leaf.layout.contains_source(caret) {
            continue;
        }
        let span = leaf.layout.source_span_len();
        let has_caret = leaf.caret_bounds.is_some();
        let better = match best {
            None => true,
            Some((_, best_span, best_has)) => match (has_caret, best_has) {
                (true, false) => true,
                (false, true) => false,
                _ => span < best_span,
            },
        };
        if better {
            best = Some((i, span, has_caret));
        }
    }
    best.map(|(i, _, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use markrust_core::rich::{
        apply_rich_command, code_body_source_map, import_markdown, BlockKind, IdGen, RichCommand,
    };

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(w), px(h)),
        }
    }

    fn leaf_layout(text: &str, start: usize) -> Arc<LeafLayout> {
        let mut source_at = Vec::with_capacity(text.len() + 1);
        for i in 0..=text.len() {
            source_at.push(start + i);
        }
        if text.is_empty() {
            source_at = vec![start, start];
        }
        Arc::new(LeafLayout {
            text: text.to_string(),
            runs: Vec::new(),
            source_at,
            block_start: start,
        })
    }

    fn visual_line(visible: Range<usize>, top: f32) -> VisualLine {
        let start = visible.start;
        let stops = (visible.start..=visible.end)
            .map(|offset| VisualCaretStop {
                visible: offset,
                source: offset,
                x: (offset.saturating_sub(start) as f32) * 10.0,
            })
            .collect();
        VisualLine {
            visible_start: visible.start,
            visible_end: visible.end,
            top,
            height: 20.0,
            stops,
        }
    }

    fn hit(
        text: &str,
        start: usize,
        bounds: Bounds<Pixels>,
        caret: Option<Bounds<Pixels>>,
    ) -> ImeLeafHit {
        ImeLeafHit {
            layout: leaf_layout(text, start),
            bounds,
            font_size: 16.0,
            line_height: 22.0,
            caret_bounds: caret,
        }
    }

    fn body_frame(caret: usize, leaves: Vec<ImeLeafHit>) -> ImeOriginState {
        let mut ime = ImeOriginState::default();
        ime.begin_frame(false, caret, None);
        for leaf in leaves {
            ime.report_leaf(leaf);
        }
        ime
    }

    #[test]
    fn visual_vertical_navigation_uses_wrapped_rows_and_keeps_the_original_x() {
        let layout = leaf_layout("abcdefghijklmnopq", 0);
        let mut ime = ImeOriginState::default();
        ime.begin_frame(false, 4, None);
        // The middle row is deliberately short. A source-line move would not
        // see any of these wrap boundaries at all.
        ime.report_visual_lines(
            layout,
            vec![
                visual_line(0..5, 0.0),
                visual_line(5..7, 20.0),
                visual_line(7..12, 40.0),
            ],
        );

        let first = ime
            .visual_vertical_target(4, 1, None)
            .expect("first wrapped-row target");
        assert_eq!(first.source, Some(7));
        assert_eq!(first.preferred_x, 40.0);

        // The source offset 7 is the short row's visual end. The saved x
        // (40), rather than its current x (20), must carry through to the
        // following long row.
        let second = ime
            .visual_vertical_target(7, 1, Some(first.preferred_x))
            .expect("second wrapped-row target");
        assert_eq!(second.source, Some(11));
        assert_eq!(second.preferred_x, 40.0);

        let up = ime
            .visual_vertical_target(11, -1, Some(second.preferred_x))
            .expect("upward wrapped-row target");
        assert_eq!(up.source, Some(7));
    }

    fn widget_frame(
        widget: Bounds<Pixels>,
        decoy_leaves: Vec<ImeLeafHit>,
        caret_in_body: usize,
    ) -> ImeOriginState {
        widget_frame_with_inner(widget, None, decoy_leaves, caret_in_body)
    }

    fn widget_frame_with_inner(
        widget: Bounds<Pixels>,
        inner: Option<Bounds<Pixels>>,
        decoy_leaves: Vec<ImeLeafHit>,
        caret_in_body: usize,
    ) -> ImeOriginState {
        let mut ime = ImeOriginState::default();
        ime.begin_frame(true, caret_in_body, None);
        ime.report_widget(widget);
        if let Some(caret) = inner {
            ime.report_widget_caret(caret);
        }
        for leaf in decoy_leaves {
            ime.report_leaf(leaf);
        }
        ime
    }

    /// Approximate inner `|` rect from a draft offset (headless; production
    /// uses the shaped-glyph caret). Mid-draft is never the overlay right edge.
    fn inner_widget_caret_rect(
        widget: Bounds<Pixels>,
        draft: &str,
        caret: usize,
    ) -> Bounds<Pixels> {
        let h = widget.size.height.min(px(22.)).max(px(2.));
        let at = caret.min(draft.len());
        let before = &draft[..at];
        let line_idx = before.matches('\n').count() as f32;
        let col = before.rsplit('\n').next().unwrap_or("").chars().count() as f32;
        let x = f32::from(widget.origin.x) + col * 8.0;
        let y = f32::from(widget.origin.y) + line_idx * f32::from(h);
        let right = f32::from(widget.origin.x + widget.size.width);
        Bounds {
            origin: point(
                px(x.min(right - 2.0).max(f32::from(widget.origin.x))),
                px(y),
            ),
            size: size(px(2.), h),
        }
    }

    #[test]
    fn widget_ime_insert_uses_inner_caret_not_append() {
        let mut draft = "****".to_string();
        let mut caret = 2usize;
        let at = caret;
        replace_in_widget_draft(&mut draft, &mut caret, None, "x", at..at);
        assert_eq!(draft, "**x**");
        assert_eq!(caret, 3);
        let sel = widget_selected_text_range(&draft, caret, caret, None);
        assert_eq!(sel.range.start, sel.range.end);
        assert_eq!(sel.range.start, offset_to_utf16(&draft, 3));
    }

    #[test]
    fn widget_ime_insert_replaces_inner_selection() {
        let mut draft = "cat".to_string();
        let mut caret = 3usize;
        replace_in_widget_draft(&mut draft, &mut caret, None, "x", 1..3);
        assert_eq!(draft, "cx");
        assert_eq!(caret, 2);
    }

    #[test]
    fn widget_ime_replacements_preserve_extended_graphemes() {
        for (cluster, label) in [
            ("e\u{301}", "combining accent"),
            ("👩\u{200d}💻", "ZWJ emoji"),
        ] {
            let original = format!("{cluster}x");
            let inside = cluster.chars().next().expect(label).len_utf8();

            let mut draft = original.clone();
            let mut caret = 0;
            replace_in_widget_draft(&mut draft, &mut caret, None, "!", inside..inside);
            assert_eq!(
                draft,
                format!("!{cluster}x"),
                "IME insertion inside a {label} must normalize to its edge"
            );
            assert_eq!(caret, 1);

            let mut draft = original.clone();
            let mut caret = 0;
            let start_utf16 = offset_to_utf16(&original, inside);
            let end_utf16 = offset_to_utf16(&original, cluster.len());
            replace_in_widget_draft(
                &mut draft,
                &mut caret,
                Some(start_utf16..end_utf16),
                "q",
                0..0,
            );
            assert_eq!(
                draft, "qx",
                "IME replacement starting inside a {label} must replace the whole glyph"
            );
            assert_eq!(caret, 1);
        }
    }

    #[test]
    fn body_caret_ignores_later_unfocused_leaf() {
        let focused = rect(10.0, 40.0, 2.0, 22.0);
        let decoy = rect(10.0, 400.0, 2.0, 22.0);
        let ime = body_frame(
            3,
            vec![
                hit("hello", 0, rect(8.0, 40.0, 200.0, 22.0), Some(focused)),
                hit(
                    "later paragraph",
                    20,
                    rect(8.0, 400.0, 200.0, 22.0),
                    Some(decoy),
                ),
            ],
        );
        assert_eq!(ime.owner(), Some(ImeOwner::Leaf));
        assert_eq!(ime.caret_rect(), Some(focused));
    }

    #[test]
    fn wrapped_line_caret_uses_reported_rect_not_leaf_origin() {
        let leaf_bounds = rect(8.0, 10.0, 240.0, 66.0);
        let second_line = rect(8.0, 32.0, 2.0, 22.0);
        let ime = body_frame(
            40,
            vec![hit(
                "a long paragraph that wraps onto a second visual line here",
                0,
                leaf_bounds,
                Some(second_line),
            )],
        );
        let caret = ime.caret_rect().expect("wrapped caret");
        assert_eq!(caret, second_line);
        assert!(
            caret.origin.y > leaf_bounds.origin.y,
            "IME origin must sit on the wrapped line, not the leaf top"
        );
    }

    #[test]
    fn table_cell_caret_not_last_painted_cell() {
        let cell_a = rect(10.0, 80.0, 2.0, 20.0);
        let cell_b = rect(120.0, 80.0, 2.0, 20.0);
        let ime = body_frame(
            12,
            vec![
                hit("alpha", 10, rect(8.0, 80.0, 80.0, 22.0), Some(cell_a)),
                hit("bravo", 20, rect(100.0, 80.0, 80.0, 22.0), Some(cell_b)),
            ],
        );
        assert_eq!(ime.caret_rect(), Some(cell_a));
        assert_ne!(ime.caret_rect(), Some(cell_b));
    }

    #[test]
    fn language_chip_widget_beats_body_leaf() {
        let chip = rect(24.0, 120.0, 48.0, 18.0);
        let body = rect(24.0, 142.0, 2.0, 22.0);
        let ime = widget_frame(
            chip,
            vec![hit(
                "fn main() {}",
                12,
                rect(24.0, 142.0, 400.0, 80.0),
                Some(body),
            )],
            12,
        );
        assert_eq!(ime.owner(), Some(ImeOwner::Widget));
        assert_eq!(ime.caret_rect(), Some(widget_caret_rect(chip)));
        assert_ne!(ime.caret_rect(), Some(body));
    }

    #[test]
    fn image_caption_widget_beats_body_leaf() {
        let caption = rect(24.0, 260.0, 160.0, 16.0);
        let alt_placeholder = rect(24.0, 200.0, 2.0, 22.0);
        let ime = widget_frame(
            caption,
            vec![hit(
                "🖼 cat",
                0,
                rect(24.0, 200.0, 200.0, 22.0),
                Some(alt_placeholder),
            )],
            2,
        );
        assert_eq!(ime.caret_rect(), Some(widget_caret_rect(caption)));
        assert_ne!(ime.caret_rect(), Some(alt_placeholder));
    }

    #[test]
    fn frontmatter_field_widget_beats_body_leaf() {
        let title = rect(24.0, 8.0, 220.0, 20.0);
        let body = rect(24.0, 96.0, 2.0, 22.0);
        let ime = widget_frame(
            title,
            vec![hit("Hello", 40, rect(24.0, 96.0, 400.0, 22.0), Some(body))],
            40,
        );
        assert_eq!(ime.caret_rect(), Some(widget_caret_rect(title)));
        assert_ne!(ime.caret_rect(), Some(body));
    }

    #[test]
    fn widget_origin_ignores_leaf_reported_after_the_widget() {
        let yaml = rect(24.0, 48.0, 360.0, 72.0);
        let later = rect(24.0, 900.0, 2.0, 22.0);
        let mut ime = ImeOriginState::default();
        ime.begin_frame(true, 0, None);
        ime.report_widget(yaml);
        ime.report_leaf(hit(
            "body after frontmatter",
            80,
            rect(24.0, 900.0, 400.0, 22.0),
            Some(later),
        ));
        assert_eq!(ime.owner(), Some(ImeOwner::Widget));
        assert_eq!(ime.caret_rect(), Some(widget_caret_rect(yaml)));
    }

    #[test]
    fn begin_frame_clears_stale_widget_when_focus_returns_to_body() {
        let mut ime = ImeOriginState::default();
        ime.begin_frame(true, 0, None);
        ime.report_widget(rect(24.0, 8.0, 100.0, 18.0));
        assert_eq!(ime.owner(), Some(ImeOwner::Widget));

        let body = rect(24.0, 96.0, 2.0, 22.0);
        ime.begin_frame(false, 4, None);
        ime.report_leaf(hit("Hello", 0, rect(24.0, 96.0, 400.0, 22.0), Some(body)));
        assert_eq!(ime.owner(), Some(ImeOwner::Leaf));
        assert_eq!(ime.caret_rect(), Some(body));
    }

    #[test]
    fn hit_test_uses_leaf_under_point_not_last_painted() {
        let first = rect(8.0, 10.0, 200.0, 22.0);
        let second = rect(8.0, 40.0, 200.0, 22.0);
        let ime = body_frame(
            3,
            vec![
                hit("hello", 0, first, Some(rect(10.0, 10.0, 2.0, 22.0))),
                hit("world", 10, second, Some(rect(10.0, 40.0, 2.0, 22.0))),
            ],
        );
        let leaf = ime
            .leaf_at_point(point(px(20.0), px(14.0)))
            .expect("first leaf");
        assert_eq!(leaf.layout.text, "hello");
        let leaf = ime
            .leaf_at_point(point(px(20.0), px(48.0)))
            .expect("second leaf");
        assert_eq!(leaf.layout.text, "world");
    }

    #[test]
    fn leftover_viewport_below_last_leaf_is_below_painted_content() {
        let last = rect(8.0, 40.0, 200.0, 22.0);
        let ime = body_frame(
            0,
            vec![
                hit("hello", 0, rect(8.0, 10.0, 200.0, 22.0), None),
                hit("world", 10, last, None),
            ],
        );
        assert!(
            !ime.point_is_below_painted_content(point(px(20.0), px(48.0))),
            "click on the last leaf is not leftover viewport"
        );
        assert!(
            ime.point_is_below_painted_content(point(px(20.0), px(80.0))),
            "click below the last painted line is leftover viewport"
        );
        assert!(
            !ime.point_is_below_painted_content(point(px(20.0), px(14.0))),
            "click on an earlier leaf is not leftover"
        );
    }

    #[test]
    fn leftover_click_on_last_image_or_rule_is_not_below_content() {
        let para = rect(8.0, 10.0, 200.0, 22.0);
        let image = rect(8.0, 40.0, 200.0, 80.0);
        let mut ime = ImeOriginState::default();
        ime.begin_frame(false, 0, None);
        ime.report_leaf(hit("hello", 0, para, None));
        ime.report_painted_bounds(image);
        assert!(
            !ime.point_is_below_painted_content(point(px(20.0), px(70.0))),
            "click on a standalone image must not be leftover"
        );
        assert!(
            ime.point_is_below_painted_content(point(px(20.0), px(140.0))),
            "click below the image is leftover"
        );
        assert!(
            !ime.point_is_below_painted_content(point(px(20.0), px(14.0))),
            "click on the paragraph above is not leftover"
        );

        let rule = rect(8.0, 40.0, 400.0, 1.0);
        let mut ime = ImeOriginState::default();
        ime.begin_frame(false, 0, None);
        ime.report_painted_bounds(rule);
        assert!(
            !ime.point_is_below_painted_content(point(px(20.0), px(40.5))),
            "click on a thematic rule must not be leftover"
        );
        assert!(
            ime.point_is_below_painted_content(point(px(20.0), px(60.0))),
            "click below the rule is leftover"
        );
    }

    #[test]
    fn empty_paint_treats_any_point_as_below_content() {
        let ime = body_frame(0, vec![]);
        assert!(
            ime.point_is_below_painted_content(point(px(40.0), px(200.0))),
            "newlines-only / unpainted document must accept a leftover click"
        );
    }

    fn first_code(blocks: &[markrust_core::rich::Block]) -> Option<&markrust_core::rich::Block> {
        for b in blocks {
            if matches!(b.kind, BlockKind::CodeBlock { .. }) {
                return Some(b);
            }
            if let Some(found) = first_code(&b.children) {
                return Some(found);
            }
        }
        None
    }

    fn mapped_fence_leaf(source: &str, bounds: Bounds<Pixels>) -> ImeLeafHit {
        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let block = first_code(&tree.blocks).expect("code block");
        let body = match &block.kind {
            BlockKind::CodeBlock { literal, .. } => {
                literal.strip_suffix('\n').unwrap_or(literal).to_string()
            }
            _ => unreachable!(),
        };
        let source_at = code_body_source_map(source, block, body.len());
        let start = block.code_body_range(source).start;
        let c = source.find("code").unwrap_or(start);
        ImeLeafHit {
            layout: Arc::new(LeafLayout {
                text: body,
                runs: Vec::new(),
                source_at,
                block_start: start,
            }),
            bounds,
            font_size: 16.0,
            line_height: 22.0,
            caret_bounds: Some(caret_on(bounds, c, c)),
        }
    }

    #[test]
    fn quoted_fence_ime_character_index_skips_quote_prefix() {
        let source = "> ```\n> code\n> ```\n";
        let bounds = rect(8.0, 10.0, 200.0, 22.0);
        let leaf = mapped_fence_leaf(source, bounds);
        let c = source.find("code").expect("code");
        let gt = source.find('>').expect(">");
        assert_eq!(
            leaf.layout.source_for_visible(0),
            c,
            "character_index_for_point vis 0 must be the first painted body byte"
        );
        assert_ne!(
            leaf.layout.source_for_visible(0),
            gt,
            "click x on painted `code` must not equal the `>` byte"
        );
        assert_eq!(leaf.layout.visible_for_source(c), 0);

        let ime = body_frame(c, vec![leaf]);
        let focused = ime.focused_leaf().expect("code leaf owns the caret");
        assert_eq!(focused.layout.source_for_visible(0), c);
        assert_eq!(
            ime.leaf_at_point(point(px(20.0), px(14.0)))
                .expect("hit")
                .layout
                .source_for_visible(0),
            c,
            "IME origin hit-test uses the same prefix-skipping map"
        );
    }

    #[test]
    fn list_nested_fence_ime_character_index_skips_indent() {
        let source = "- item\n  ```\n  code\n  ```\n";
        let bounds = rect(8.0, 40.0, 200.0, 22.0);
        let leaf = mapped_fence_leaf(source, bounds);
        let c = source.find("code").expect("code");
        let indent = source.find("  code").expect("indent");
        assert_eq!(leaf.layout.source_for_visible(0), c);
        assert_ne!(leaf.layout.source_for_visible(0), indent);
        let ime = body_frame(c, vec![leaf]);
        assert_eq!(
            ime.focused_leaf()
                .expect("leaf")
                .layout
                .visible_for_source(c),
            0
        );
    }

    fn first_opaque(blocks: &[markrust_core::rich::Block]) -> Option<&markrust_core::rich::Block> {
        for b in blocks {
            if matches!(b.kind, BlockKind::Opaque { .. }) {
                return Some(b);
            }
            if let Some(found) = first_opaque(&b.children) {
                return Some(found);
            }
        }
        None
    }

    fn mapped_html_leaf(source: &str, bounds: Bounds<Pixels>) -> ImeLeafHit {
        use super::super::block_text::{build_html_block_layout, RevealState};
        use crate::theme::EditorTheme;
        use gpui::TextStyle;

        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let block = first_opaque(&tree.blocks).expect("html block");
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
        let layout = match markrust_core::html_visual::project_html_block(raw) {
            markrust_core::html_visual::HtmlBlockVisual::Flow {
                text,
                source_at,
                runs,
            } => build_html_block_layout(
                &text,
                &source_at,
                &runs,
                source,
                block,
                &style,
                &theme,
                &RevealState::HIDDEN,
            ),
            other => panic!("expected flow, got {other:?}"),
        };
        let x = source.find('x').unwrap_or(block.source_range.start);
        ImeLeafHit {
            layout: Arc::new(layout),
            bounds,
            font_size: 16.0,
            line_height: 22.0,
            caret_bounds: Some(caret_on(bounds, x, x)),
        }
    }

    #[test]
    fn quoted_html_ime_character_index_skips_quote_prefix() {
        let source = "> <div>\n> x\n> </div>\n";
        let bounds = rect(8.0, 10.0, 200.0, 22.0);
        let leaf = mapped_html_leaf(source, bounds);
        let x = source.find('x').expect("x");
        let gt = source.find('>').expect(">");
        assert_ne!(
            leaf.layout.source_for_visible(0),
            gt,
            "character_index_for_point vis 0 must not be the `>` byte"
        );
        let vis = leaf.layout.visible_for_source(x);
        assert_eq!(leaf.layout.source_for_visible(vis), x);
        let ime = body_frame(x, vec![leaf]);
        let focused = ime.focused_leaf().expect("html leaf owns the caret");
        assert_eq!(focused.layout.source_for_visible(vis), x);
        assert_eq!(
            ime.leaf_at_point(point(px(20.0), px(14.0)))
                .expect("hit")
                .layout
                .source_for_visible(0),
            focused.layout.source_for_visible(0),
            "IME origin hit-test uses the same prefix-skipping HTML map"
        );
    }

    fn first_paragraph(
        blocks: &[markrust_core::rich::Block],
    ) -> Option<&markrust_core::rich::Block> {
        for b in blocks {
            if matches!(b.kind, BlockKind::Paragraph) {
                return Some(b);
            }
            if let Some(found) = first_paragraph(&b.children) {
                return Some(found);
            }
        }
        None
    }

    fn mapped_para_leaf(source: &str, bounds: Bounds<Pixels>) -> ImeLeafHit {
        use super::super::block_text::{build_leaf_layout_revealed, ChromeHosts, RevealState};
        use crate::theme::EditorTheme;
        use gpui::TextStyle;

        let mut ids = IdGen::default();
        let tree = import_markdown(source, &mut ids);
        let block = first_paragraph(&tree.blocks).expect("paragraph");
        let theme = EditorTheme::dark();
        let style = TextStyle {
            color: theme.text,
            font_family: theme.font_family.clone().into(),
            font_size: px(theme.font_size).into(),
            ..Default::default()
        };
        let layout = build_leaf_layout_revealed(
            block,
            source,
            &style,
            &theme,
            gpui::FontWeight::NORMAL,
            &RevealState::HIDDEN,
            &ChromeHosts::NONE,
        );
        let h = source.find('h').unwrap_or(block.source_range.start);
        ImeLeafHit {
            layout: Arc::new(layout),
            bounds,
            font_size: 16.0,
            line_height: 22.0,
            caret_bounds: Some(caret_on(bounds, h, h)),
        }
    }

    #[test]
    fn quoted_paragraph_ime_character_index_skips_quote_prefix() {
        let source = "> hello\n";
        let bounds = rect(8.0, 10.0, 200.0, 22.0);
        let leaf = mapped_para_leaf(source, bounds);
        let h = source.find('h').expect("h");
        let gt = source.find('>').expect(">");
        assert_eq!(
            leaf.layout.source_for_visible(0),
            h,
            "character_index_for_point vis 0 must be `h`"
        );
        assert_ne!(leaf.layout.source_for_visible(0), gt);
        let ime = body_frame(h, vec![leaf]);
        assert_eq!(
            ime.focused_leaf()
                .expect("leaf")
                .layout
                .source_for_visible(0),
            h
        );
    }

    #[test]
    fn list_item_ime_character_index_skips_marker() {
        let source = "- hello\n";
        let bounds = rect(8.0, 10.0, 200.0, 22.0);
        let leaf = mapped_para_leaf(source, bounds);
        let h = source.find('h').expect("h");
        let dash = source.find('-').expect("-");
        assert_eq!(leaf.layout.source_for_visible(0), h);
        assert_ne!(leaf.layout.source_for_visible(0), dash);
        let ime = body_frame(h, vec![leaf]);
        assert_eq!(
            ime.leaf_at_point(point(px(20.0), px(14.0)))
                .expect("hit")
                .layout
                .source_for_visible(0),
            h,
            "IME origin hit-test uses the same prefix-skipping map"
        );
    }

    #[test]
    fn nested_quote_ime_character_index_skips_inner_marker() {
        let source = "> > hello\n";
        let bounds = rect(8.0, 10.0, 200.0, 22.0);
        let leaf = mapped_para_leaf(source, bounds);
        let h = source.find('h').expect("h");
        assert_eq!(leaf.layout.source_for_visible(0), h);
        assert_ne!(&source[leaf.layout.source_for_visible(0)..][..1], ">");
    }

    #[test]
    fn ordered_list_ime_character_index_skips_marker() {
        let source = "1. hello\n";
        let bounds = rect(8.0, 10.0, 200.0, 22.0);
        let leaf = mapped_para_leaf(source, bounds);
        let h = source.find('h').expect("h");
        assert_eq!(leaf.layout.source_for_visible(0), h);
        assert_ne!(&source[leaf.layout.source_for_visible(0)..][..1], "1");
    }

    #[test]
    fn unchecked_task_ime_character_index_skips_checkbox() {
        let source = "- [ ] hello\n";
        let bounds = rect(8.0, 10.0, 200.0, 22.0);
        let leaf = mapped_para_leaf(source, bounds);
        let h = source.find('h').expect("h");
        assert_eq!(leaf.layout.source_for_visible(0), h);
    }

    fn hit_for_range(
        start: usize,
        end: usize,
        bounds: Bounds<Pixels>,
        caret: Option<Bounds<Pixels>>,
    ) -> ImeLeafHit {
        let n = end.saturating_sub(start).max(1);
        hit(&"x".repeat(n), start, bounds, caret)
    }

    fn caret_on(bounds: Bounds<Pixels>, caret: usize, source_start: usize) -> Bounds<Pixels> {
        let dx = (caret.saturating_sub(source_start) as f32).min(22.0) * 8.0;
        rect(
            f32::from(bounds.origin.x) + 2.0 + dx,
            f32::from(bounds.origin.y),
            2.0,
            20.0,
        )
    }

    fn report_tree_leaves(
        ime: &mut ImeOriginState,
        blocks: &[markrust_core::rich::Block],
        caret: usize,
        y: &mut f32,
    ) {
        for block in blocks {
            if block.children.is_empty() {
                let bounds = rect(8.0, *y, 200.0, 22.0);
                *y += 24.0;
                let caret_bounds =
                    if block.source_range.start <= caret && caret <= block.source_range.end {
                        Some(caret_on(bounds, caret, block.source_range.start))
                    } else {
                        None
                    };
                ime.report_leaf(hit_for_range(
                    block.source_range.start,
                    block.source_range.end,
                    bounds,
                    caret_bounds,
                ));
            } else {
                report_tree_leaves(ime, &block.children, caret, y);
            }
        }
    }

    /// View protocol: `begin_frame` with the document caret, then report the
    /// leaves (and optional widget) that would paint this frame.
    fn paint_engine_frame(
        ime: &mut ImeOriginState,
        engine: &markrust_core::rich::RichEngine,
        caret: usize,
        widget: Option<Bounds<Pixels>>,
        widget_inner: Option<Bounds<Pixels>>,
        composition: Option<&str>,
    ) {
        ime.begin_frame(widget.is_some(), caret, composition);
        if let Some(bounds) = widget {
            ime.report_widget(bounds);
            if let Some(inner) = widget_inner {
                ime.report_widget_caret(inner);
            }
        }
        let mut y = 10.0;
        report_tree_leaves(ime, &engine.tree().blocks, caret, &mut y);
    }

    #[test]
    fn platform_push_fires_once_per_origin_change() {
        let mut ime = ImeOriginState::default();
        let body = rect(10.0, 40.0, 2.0, 22.0);
        ime.begin_frame(false, 3, None);
        ime.report_leaf(hit("hello", 0, rect(8.0, 40.0, 200.0, 22.0), Some(body)));
        assert_eq!(ime.take_platform_push(), Some(body));
        assert_eq!(
            ime.take_platform_push(),
            None,
            "same origin must not re-push"
        );
    }

    #[test]
    fn blink_off_keeps_caret_origin_not_leaf_top_left() {
        let mut ime = ImeOriginState::default();
        let caret = rect(80.0, 40.0, 2.0, 22.0);
        let leaf = rect(8.0, 40.0, 200.0, 22.0);
        ime.begin_frame(false, 3, None);
        ime.report_leaf(hit("hello", 0, leaf, Some(caret)));
        assert_eq!(ime.take_platform_push(), Some(caret));

        ime.begin_frame(false, 3, None);
        ime.report_leaf(hit("hello", 0, leaf, None));
        assert_eq!(
            ime.caret_rect(),
            Some(caret),
            "blink-off must keep the last caret, not the leaf origin"
        );
        assert_eq!(ime.take_platform_push(), None);
        assert_ne!(
            ime.caret_rect(),
            Some(rect(
                f32::from(leaf.origin.x),
                f32::from(leaf.origin.y),
                2.0,
                22.0
            )),
            "IME origin must not jump to the leaf top-left when the caret quad is hidden"
        );
    }

    #[test]
    fn begin_frame_before_paint_keeps_body_caret_for_os_pull() {
        let mut ime = ImeOriginState::default();
        let caret = rect(80.0, 40.0, 2.0, 22.0);
        let leaf = rect(8.0, 40.0, 200.0, 22.0);
        ime.begin_frame(false, 3, None);
        ime.report_leaf(hit("hello", 0, leaf, Some(caret)));
        assert_eq!(ime.take_platform_push(), Some(caret));

        ime.begin_frame(false, 3, Some("ni"));
        assert_eq!(
            ime.caret_rect(),
            Some(caret),
            "macOS firstRectForCharacterRange after begin_frame must keep the caret"
        );
        let pulled = ime_origin_bounds(&ime, decoy_element_bounds());
        assert_eq!(pulled, caret);
        assert_ne!(pulled, decoy_element_bounds());
        assert_ne!(
            pulled,
            rect(
                f32::from(leaf.origin.x),
                f32::from(leaf.origin.y),
                2.0,
                22.0
            ),
            "OS pull must not fall back to the leaf top-left before paint"
        );
        assert_eq!(
            ime.take_platform_push(),
            None,
            "unpainted frame must not push leftover sticky to Linux/Windows"
        );
    }

    #[test]
    fn begin_frame_before_paint_keeps_wrapped_line_caret_for_os_pull() {
        let mut ime = ImeOriginState::default();
        let leaf_bounds = rect(8.0, 10.0, 240.0, 66.0);
        let second_line = rect(8.0, 32.0, 2.0, 22.0);
        ime.begin_frame(false, 40, None);
        ime.report_leaf(hit(
            "a long paragraph that wraps onto a second visual line here",
            0,
            leaf_bounds,
            Some(second_line),
        ));
        assert_eq!(ime.take_platform_push(), Some(second_line));

        ime.begin_frame(false, 40, Some("ni"));
        let pulled = ime_origin_bounds(&ime, decoy_element_bounds());
        assert_eq!(pulled, second_line);
        assert!(
            f32::from(pulled.origin.y) > f32::from(leaf_bounds.origin.y),
            "OS pull must stay on the wrapped line, not the leaf top"
        );
    }

    #[test]
    fn begin_frame_before_paint_keeps_overlay_inner_caret_not_trailing_edge() {
        let overlay = rect(24.0, 260.0, 160.0, 16.0);
        let inner = inner_widget_caret_rect(overlay, "caption text", 4);
        let trailing = widget_caret_rect(overlay);
        let mut ime = ImeOriginState::default();
        ime.begin_frame(true, 0, None);
        ime.report_widget(overlay);
        ime.report_widget_caret(inner);
        assert_eq!(ime.take_platform_push(), Some(inner));

        ime.begin_frame(true, 4, Some("ni"));
        let pulled = ime_origin_bounds(&ime, decoy_element_bounds());
        assert_eq!(
            pulled, inner,
            "OS pull after begin_frame must keep the inner `|`"
        );
        assert_ne!(pulled, trailing);
        assert_ne!(pulled, decoy_element_bounds());
        assert_eq!(ime.take_platform_push(), None);
    }

    #[test]
    fn unfocused_leaf_does_not_push_stale_sticky_after_caret_move() {
        let first = rect(10.0, 40.0, 2.0, 22.0);
        let second = rect(10.0, 80.0, 2.0, 22.0);
        let mut ime = ImeOriginState::default();
        ime.begin_frame(false, 3, None);
        ime.report_leaf(hit("hello", 0, rect(8.0, 40.0, 200.0, 22.0), Some(first)));
        ime.report_leaf(hit("world", 6, rect(8.0, 80.0, 200.0, 22.0), None));
        assert_eq!(ime.take_platform_push(), Some(first));

        ime.begin_frame(false, 8, None);
        ime.report_leaf(hit("hello", 0, rect(8.0, 40.0, 200.0, 22.0), None));
        assert_eq!(
            ime.take_platform_push(),
            None,
            "the previous paragraph must not push its leftover sticky after the caret left"
        );
        ime.report_leaf(hit("world", 6, rect(8.0, 80.0, 200.0, 22.0), Some(second)));
        assert_eq!(ime.take_platform_push(), Some(second));
    }

    #[test]
    fn composition_start_pushes_even_when_caret_rect_is_unchanged() {
        let mut ime = ImeOriginState::default();
        let body = rect(10.0, 40.0, 2.0, 22.0);
        ime.begin_frame(false, 3, None);
        ime.report_leaf(hit("hello", 0, rect(8.0, 40.0, 200.0, 22.0), Some(body)));
        assert_eq!(ime.take_platform_push(), Some(body));

        ime.begin_frame(false, 3, Some("ni"));
        ime.report_leaf(hit("hello", 0, rect(8.0, 40.0, 200.0, 22.0), Some(body)));
        assert_eq!(
            ime.take_platform_push(),
            Some(body),
            "composition start must invalidate the platform IME even if the caret did not move"
        );
        assert_eq!(ime.take_platform_push(), None);
    }

    #[test]
    fn ime_origin_after_insert_then_caret_into_table_cell_is_not_stale() {
        let source = "hello\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let mut doc = markrust_core::Document::new(source);
        let mut engine = markrust_core::rich::RichEngine::new();
        engine.sync(&doc);
        let mut caret = markrust_core::rich::CaretState::collapsed(5);
        apply_rich_command(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("!".into()),
        )
        .expect("insert in paragraph");
        engine.sync(&doc);
        let body_caret = caret.cursor();
        assert!(
            !engine.in_table(body_caret),
            "edit must land in the paragraph, not the table"
        );

        let mut ime = ImeOriginState::default();
        paint_engine_frame(&mut ime, &engine, body_caret, None, None, None);
        let origin_body = ime.caret_rect().expect("body origin after edit");
        assert_eq!(ime.take_platform_push(), Some(origin_body));
        assert_eq!(ime.owner(), Some(ImeOwner::Leaf));

        let table = engine
            .tree()
            .blocks
            .iter()
            .find(|b| matches!(b.kind, markrust_core::rich::BlockKind::Table { .. }))
            .expect("table after edit");
        let cell_a = engine
            .cell_caret(table.id, 0, 0)
            .expect("header cell a after edit");
        caret.collapse_to(cell_a);
        apply_rich_command(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::TableTab { reverse: false },
        )
        .expect("tab into next cell");
        engine.sync(&doc);
        let cell_caret = caret.cursor();
        let pos = engine
            .table_pos(cell_caret)
            .expect("caret must sit in a table cell after tab");
        assert_eq!(pos.col, 1, "TableTab should land in column b");

        paint_engine_frame(&mut ime, &engine, cell_caret, None, None, None);
        let origin_cell = ime.caret_rect().expect("cell origin after caret move");
        assert_ne!(
            origin_cell, origin_body,
            "IME origin must follow the caret into the table cell after the edit, not keep the paragraph rect"
        );
        let leaf = ime.focused_leaf().expect("cell leaf");
        assert!(
            leaf.layout.contains_source(cell_caret),
            "origin leaf must own the post-edit cell caret {cell_caret}"
        );
        assert!(
            !leaf.layout.contains_source(body_caret),
            "cell origin must not still be the pre-tab paragraph caret {body_caret}"
        );
        assert_eq!(
            ime.take_platform_push(),
            Some(origin_cell),
            "caret move into a cell must push the new origin to the platform"
        );
        assert_eq!(ime.take_platform_push(), None);
    }

    #[test]
    fn ime_origin_after_insert_then_caption_focus_is_not_stale() {
        let source = "hello\n\n![cat](img.png)\n";
        let mut doc = markrust_core::Document::new(source);
        let mut engine = markrust_core::rich::RichEngine::new();
        engine.sync(&doc);
        let mut caret = markrust_core::rich::CaretState::collapsed(5);
        apply_rich_command(
            &mut doc,
            &mut engine,
            &mut caret,
            RichCommand::InsertText("!".into()),
        )
        .expect("insert in paragraph");
        engine.sync(&doc);
        let body_caret = caret.cursor();

        let mut ime = ImeOriginState::default();
        paint_engine_frame(&mut ime, &engine, body_caret, None, None, None);
        let origin_body = ime.caret_rect().expect("body origin after edit");
        assert_eq!(ime.take_platform_push(), Some(origin_body));

        let caption = rect(24.0, 260.0, 160.0, 16.0);
        let inner = inner_widget_caret_rect(caption, "cat", 1);
        paint_engine_frame(
            &mut ime,
            &engine,
            body_caret,
            Some(caption),
            Some(inner),
            None,
        );
        let origin_caption = ime.caret_rect().expect("caption origin");
        assert_eq!(ime.owner(), Some(ImeOwner::Widget));
        assert_eq!(origin_caption, inner);
        assert_ne!(
            origin_caption,
            widget_caret_rect(caption),
            "caption IME origin must be the inner caret, not the overlay right edge"
        );
        assert_ne!(
            origin_caption, origin_body,
            "IME origin must move to the caption overlay after the edit, not keep the body caret"
        );
        assert_eq!(
            ime.take_platform_push(),
            Some(origin_caption),
            "widget focus after an edit must push the caption origin"
        );
    }

    /// Decoy element bounds passed to `bounds_for_range`. A correct origin must
    /// not fall back to this — that would mean `ImeOriginState` missed the caret.
    fn decoy_element_bounds() -> Bounds<Pixels> {
        rect(0.0, 0.0, 800.0, 600.0)
    }

    /// In-repo stand-in for `PlatformWindow::update_ime_position`.
    ///
    /// GPUI's `TestWindow::update_ime_position` is a no-op (`_bounds` discarded),
    /// so there is no `set_ime_cursor_position` readout. This spy records the
    /// rectangle `Window::invalidate_character_coordinates` would pass on the
    /// next frame: `InputHandler::selected_bounds` →
    /// `compute_ime_candidate_bounds` → `bounds_for_range`.
    #[derive(Default)]
    struct PlatformImeSpy {
        requested: Vec<Bounds<Pixels>>,
    }

    impl PlatformImeSpy {
        fn record(&mut self, bounds: Bounds<Pixels>) {
            self.requested.push(bounds);
        }

        fn last(&self) -> Option<Bounds<Pixels>> {
            self.requested.last().copied()
        }
    }

    fn gpui_update_ime_position_payload(
        ime: &ImeOriginState,
        marked: Option<std::ops::Range<usize>>,
        selection: UTF16Selection,
    ) -> Bounds<Pixels> {
        gpui::PlatformInputHandler::compute_ime_candidate_bounds(marked, &selection, |_| {
            Some(ime_origin_bounds(ime, decoy_element_bounds()))
        })
        .expect("GPUI selected_bounds / update_ime_position payload")
    }

    /// Headless stand-in for [`gpui::EntityInputHandler`] on the WYSIWYG view.
    ///
    /// After paint we assert [`ImeOriginState::take_platform_push`] (what
    /// `sync_ime_cursor` uses before `invalidate_character_coordinates`) and
    /// [`PlatformImeSpy`] (the rect GPUI would pass to `update_ime_position`).
    ///
    /// This simulates composition. It does not prove the OS candidate window.
    struct ImeHandlerProbe {
        doc: markrust_core::Document,
        engine: markrust_core::rich::RichEngine,
        selected_range: std::ops::Range<usize>,
        selection_reversed: bool,
        marked_range: Option<std::ops::Range<usize>>,
        preedit: Option<String>,
        widget_draft: Option<String>,
        widget_caret: usize,
        widget_preedit: Option<String>,
        widget_bounds: Option<Bounds<Pixels>>,
        ime: ImeOriginState,
        platform_ime: PlatformImeSpy,
    }

    impl ImeHandlerProbe {
        fn open(source: &str, caret: usize) -> Self {
            let doc = markrust_core::Document::new(source);
            let mut engine = markrust_core::rich::RichEngine::new();
            engine.sync(&doc);
            Self {
                doc,
                engine,
                selected_range: caret..caret,
                selection_reversed: false,
                marked_range: None,
                preedit: None,
                widget_draft: None,
                widget_caret: 0,
                widget_preedit: None,
                widget_bounds: None,
                ime: ImeOriginState::default(),
                platform_ime: PlatformImeSpy::default(),
            }
        }

        fn cursor(&self) -> usize {
            if self.selection_reversed {
                self.selected_range.start
            } else {
                self.selected_range.end
            }
        }

        fn source(&self) -> String {
            self.doc.buffer.content()
        }

        fn composition_key(&self) -> Option<&str> {
            if self.widget_draft.is_some() {
                self.widget_preedit.as_deref()
            } else {
                self.preedit.as_deref()
            }
        }

        fn apply_rich(&mut self, command: RichCommand) {
            let mut caret = markrust_core::rich::CaretState {
                range: self.selected_range.clone(),
                reversed: self.selection_reversed,
            };
            apply_rich_command(&mut self.doc, &mut self.engine, &mut caret, command)
                .expect("rich command");
            self.selected_range = caret.range;
            self.selection_reversed = caret.reversed;
            self.engine.sync(&self.doc);
        }

        fn paint(&mut self) -> Option<Bounds<Pixels>> {
            let caret = self.cursor();
            let composition = self.composition_key().map(str::to_string);
            let widget = self.widget_bounds;
            let inner = self.widget_inner_caret();
            paint_engine_frame(
                &mut self.ime,
                &self.engine,
                caret,
                widget,
                inner,
                composition.as_deref(),
            );
            let local = self.ime.take_platform_push()?;
            let platform = self.candidate_bounds();
            assert_eq!(
                platform, local,
                "GPUI selected_bounds (update_ime_position payload) must match the origin sync_ime_cursor asked to push"
            );
            assert_ne!(
                platform,
                decoy_element_bounds(),
                "platform IME rect must not be the element fallback"
            );
            self.platform_ime.record(platform);
            Some(platform)
        }

        fn widget_inner_caret(&self) -> Option<Bounds<Pixels>> {
            let bounds = self.widget_bounds?;
            let draft = self.widget_draft.as_deref().unwrap_or("");
            Some(inner_widget_caret_rect(bounds, draft, self.widget_caret))
        }

        fn last_platform_request(&self) -> Option<Bounds<Pixels>> {
            self.platform_ime.last()
        }

        /// `EntityInputHandler::selected_text_range`
        fn selected_text_range(&self) -> UTF16Selection {
            if let Some(draft) = self.widget_draft.as_deref() {
                return widget_selected_text_range(
                    draft,
                    self.widget_caret,
                    self.widget_caret,
                    self.widget_preedit.as_deref(),
                );
            }
            let content = self.source();
            body_selected_text_range(
                &content,
                self.selected_range.clone(),
                self.selection_reversed,
            )
        }

        /// `EntityInputHandler::marked_text_range`
        fn marked_text_range(&self) -> Option<std::ops::Range<usize>> {
            self.marked_range.clone()
        }

        /// `EntityInputHandler::replace_and_mark_text_in_range` (preedit)
        fn replace_and_mark_text_in_range(&mut self, new_text: &str) {
            if self.widget_draft.is_some() {
                set_preedit(&mut self.widget_preedit, new_text);
                return;
            }
            let caret = self.cursor();
            begin_body_preedit(&mut self.preedit, &mut self.marked_range, new_text, caret);
        }

        /// `EntityInputHandler::replace_text_in_range` (commit / insert)
        fn replace_text_in_range(
            &mut self,
            range_utf16: Option<std::ops::Range<usize>>,
            new_text: &str,
        ) {
            if let Some(draft) = self.widget_draft.as_mut() {
                self.widget_preedit = None;
                let at = self.widget_caret;
                replace_in_widget_draft(
                    draft,
                    &mut self.widget_caret,
                    range_utf16,
                    new_text,
                    at..at,
                );
                return;
            }
            let content = self.source();
            apply_replace_range_to_selection(
                &content,
                range_utf16,
                &mut self.marked_range,
                &mut self.selected_range,
                &mut self.selection_reversed,
            );
            self.preedit = None;
            self.apply_rich(RichCommand::InsertText(new_text.to_string()));
        }

        /// `EntityInputHandler::unmark_text`
        fn unmark_text(&mut self) {
            clear_composition(
                &mut self.preedit,
                &mut self.widget_preedit,
                &mut self.marked_range,
            );
        }

        /// `EntityInputHandler::bounds_for_range`
        fn bounds_for_range(&self, _range_utf16: std::ops::Range<usize>) -> Bounds<Pixels> {
            ime_origin_bounds(&self.ime, decoy_element_bounds())
        }

        /// macOS `firstRectForCharacterRange:` → `bounds_for_range` after
        /// `Render::render` calls `begin_frame` and before children paint.
        /// Not an OS candidate window.
        fn macos_first_rect_after_render_start(&mut self) -> Bounds<Pixels> {
            let caret = self.cursor();
            let composition = self.composition_key().map(str::to_string);
            self.ime
                .begin_frame(self.widget_bounds.is_some(), caret, composition.as_deref());
            self.bounds_for_range(0..0)
        }

        fn assert_macos_pull_keeps(&mut self, expected: Bounds<Pixels>, label: &str) {
            let pulled = self.macos_first_rect_after_render_start();
            assert_eq!(
                pulled, expected,
                "{label}: macOS firstRect pull after begin_frame"
            );
            assert_ne!(
                pulled,
                decoy_element_bounds(),
                "{label}: OS pull must not be the element fallback"
            );
            assert!(
                f32::from(pulled.size.width) > 0. && f32::from(pulled.size.height) > 0.,
                "{label}: IME rect must be non-empty"
            );
        }

        /// GPUI's IME candidate-rect helper (`PlatformInputHandler`).
        ///
        /// Not `set_ime_cursor_position`: the test window does not record that.
        fn candidate_bounds(&self) -> Bounds<Pixels> {
            gpui_update_ime_position_payload(
                &self.ime,
                self.marked_text_range(),
                self.selected_text_range(),
            )
        }

        fn assert_preedit_origin(&self, expected: Bounds<Pixels>, label: &str) {
            assert!(
                self.preedit.is_some() || self.widget_preedit.is_some(),
                "{label}: composition must be active"
            );
            let pulled = self.bounds_for_range(0..0);
            assert_eq!(pulled, expected, "{label}: bounds_for_range during preedit");
            assert_ne!(
                pulled,
                decoy_element_bounds(),
                "{label}: origin must not fall back to the element bounds"
            );
            assert_eq!(
                self.candidate_bounds(),
                expected,
                "{label}: GPUI compute_ime_candidate_bounds during preedit"
            );
        }

        fn focus_widget_at(&mut self, bounds: Bounds<Pixels>, draft: &str, caret: usize) {
            self.widget_draft = Some(draft.to_string());
            self.widget_caret = caret.min(draft.len());
            self.widget_preedit = None;
            self.widget_bounds = Some(bounds);
        }

        fn jump_to(&mut self, caret: usize) {
            self.selected_range = caret..caret;
            self.selection_reversed = false;
        }
    }

    #[test]
    fn composition_after_insert_tracks_caret_via_handler() {
        let mut ime = ImeHandlerProbe::open("hello\n\n| a | b |\n|---|---|\n| 1 | 2 |\n", 5);
        let before = ime.source();
        let pushed = ime.paint();
        let origin_before = ime.ime.caret_rect().expect("caret before insert");
        assert_eq!(pushed, Some(origin_before));

        ime.replace_text_in_range(None, "!");
        assert_eq!(&ime.source()[..6], "hello!");
        let pushed = ime.paint().expect("insert must push a new IME origin");
        let origin_after = ime.ime.caret_rect().expect("caret after insert");
        assert_eq!(pushed, origin_after);
        assert_ne!(
            origin_after, origin_before,
            "insert must move the painted caret origin"
        );

        ime.replace_and_mark_text_in_range("ni");
        assert!(
            ime.source().starts_with("hello!"),
            "insert must land in the paragraph, got {:?}",
            ime.source()
        );
        assert!(
            !ime.source().contains("ni"),
            "preedit must not touch the model"
        );
        assert_ne!(ime.source(), before);
        let sel = ime.selected_text_range();
        assert_eq!(
            sel.range.start, sel.range.end,
            "collapsed caret during preedit"
        );
        assert!(ime.marked_text_range().is_some());
        ime.assert_preedit_origin(origin_after, "after insert");
        let composed = ime
            .paint()
            .expect("composition start must request a platform IME rect");
        assert_eq!(composed, origin_after);
        assert_eq!(ime.last_platform_request(), Some(origin_after));
        assert_eq!(ime.paint(), None, "unchanged preedit must not re-push");
        ime.assert_preedit_origin(origin_after, "after insert, still composing");
    }

    #[test]
    fn composition_after_table_tab_tracks_cell_via_handler() {
        let mut ime = ImeHandlerProbe::open("hello\n\n| a | b |\n|---|---|\n| 1 | 2 |\n", 5);
        ime.replace_text_in_range(None, "!");
        let pushed_body = ime.paint().expect("body origin after insert");
        ime.replace_and_mark_text_in_range("ni");
        ime.assert_preedit_origin(pushed_body, "preedit in paragraph");
        ime.unmark_text();

        let table = ime
            .engine
            .tree()
            .blocks
            .iter()
            .find(|b| matches!(b.kind, markrust_core::rich::BlockKind::Table { .. }))
            .expect("table");
        let cell_a = ime
            .engine
            .cell_caret(table.id, 0, 0)
            .expect("header cell a");
        ime.jump_to(cell_a);
        ime.apply_rich(RichCommand::TableTab { reverse: false });
        let cell_caret = ime.cursor();
        let pos = ime
            .engine
            .table_pos(cell_caret)
            .expect("caret in table after TableTab");
        assert_eq!(pos.col, 1, "TableTab should land in column b");

        let pushed_cell = ime.paint().expect("cell origin must push after TableTab");
        assert_ne!(
            pushed_cell, pushed_body,
            "IME origin must follow TableTab into the cell, not keep the paragraph rect"
        );
        ime.replace_and_mark_text_in_range("ni");
        assert!(
            !ime.source().contains("ni"),
            "preedit in a cell must not touch the model"
        );
        ime.assert_preedit_origin(pushed_cell, "after TableTab");
        let leaf = ime.ime.focused_leaf().expect("cell leaf");
        assert!(
            leaf.layout.contains_source(cell_caret),
            "origin leaf must own the cell caret during preedit"
        );
        let composed = ime
            .paint()
            .expect("cell composition start must request a platform IME rect");
        assert_eq!(composed, pushed_cell);
        assert_eq!(ime.last_platform_request(), Some(pushed_cell));
        assert_eq!(ime.paint(), None);
    }

    #[test]
    fn composition_after_caption_focus_tracks_widget_via_handler() {
        let mut ime = ImeHandlerProbe::open("hello\n\n![cat](img.png)\n", 5);
        ime.replace_text_in_range(None, "!");
        let pushed_body = ime.paint().expect("body origin after insert");
        ime.replace_and_mark_text_in_range("ni");
        ime.assert_preedit_origin(pushed_body, "preedit in body before caption");
        ime.unmark_text();

        let caption = rect(24.0, 260.0, 160.0, 16.0);
        ime.focus_widget_at(caption, "cat", 1);
        let inner = inner_widget_caret_rect(caption, "cat", 1);
        let pushed_caption = ime
            .paint()
            .expect("caption focus must push a new IME origin");
        assert_eq!(ime.ime.owner(), Some(ImeOwner::Widget));
        assert_eq!(pushed_caption, inner);
        assert_ne!(
            pushed_caption,
            widget_caret_rect(caption),
            "caption IME origin must not be the overlay right edge while the caret is mid-draft"
        );
        assert_ne!(pushed_caption, pushed_body);

        let source_before = ime.source();
        ime.replace_and_mark_text_in_range("ni");
        assert_eq!(
            ime.source(),
            source_before,
            "widget preedit is display-only"
        );
        ime.assert_preedit_origin(pushed_caption, "after caption focus");
        let composed = ime
            .paint()
            .expect("caption composition start must request a platform IME rect");
        assert_eq!(composed, pushed_caption);
        assert_eq!(ime.last_platform_request(), Some(pushed_caption));
        assert_eq!(ime.paint(), None);
    }

    #[test]
    fn composition_after_language_chip_focus_tracks_widget_via_handler() {
        let mut ime = ImeHandlerProbe::open("hello\n\n```rust\nfn main() {}\n```\n", 5);
        ime.replace_text_in_range(None, "!");
        let pushed_body = ime.paint().expect("body origin after insert");

        let chip = rect(24.0, 120.0, 48.0, 18.0);
        ime.focus_widget_at(chip, "rust", 2);
        let inner = inner_widget_caret_rect(chip, "rust", 2);
        let pushed_chip = ime.paint().expect("chip focus must push a new IME origin");
        assert_eq!(ime.ime.owner(), Some(ImeOwner::Widget));
        assert_eq!(pushed_chip, inner);
        assert_ne!(
            pushed_chip,
            widget_caret_rect(chip),
            "chip IME origin must not be the overlay right edge while the caret is mid-draft"
        );
        assert_ne!(pushed_chip, pushed_body);

        ime.replace_and_mark_text_in_range("ni");
        ime.assert_preedit_origin(pushed_chip, "after language-chip focus");
        let composed = ime
            .paint()
            .expect("chip composition start must request a platform IME rect");
        assert_eq!(composed, pushed_chip);
        assert_eq!(ime.last_platform_request(), Some(pushed_chip));
        assert_eq!(ime.paint(), None);
    }

    #[test]
    fn composition_after_frontmatter_focus_tracks_widget_via_handler() {
        let source = "---\ntitle: Hi\n---\n\nhello\n";
        let caret = source.find("hello").expect("body") + 5;
        let mut ime = ImeHandlerProbe::open(source, caret);
        ime.replace_text_in_range(None, "!");
        let pushed_body = ime.paint().expect("body origin after insert");

        let title = rect(24.0, 8.0, 220.0, 20.0);
        ime.focus_widget_at(title, "Hi", 1);
        let inner = inner_widget_caret_rect(title, "Hi", 1);
        let pushed_fm = ime
            .paint()
            .expect("frontmatter focus must push a new IME origin");
        assert_eq!(ime.ime.owner(), Some(ImeOwner::Widget));
        assert_eq!(pushed_fm, inner);
        assert_ne!(
            pushed_fm,
            widget_caret_rect(title),
            "frontmatter IME origin must not be the overlay right edge while the caret is mid-draft"
        );
        assert_ne!(pushed_fm, pushed_body);

        ime.replace_and_mark_text_in_range("ni");
        ime.assert_preedit_origin(pushed_fm, "after frontmatter focus");
        let composed = ime
            .paint()
            .expect("frontmatter composition start must request a platform IME rect");
        assert_eq!(composed, pushed_fm);
        assert_eq!(ime.last_platform_request(), Some(pushed_fm));
        assert_eq!(ime.paint(), None);
    }

    #[test]
    fn composition_after_yaml_frontmatter_tracks_inner_caret() {
        let source = "---\ntitle: Hi\n---\n\nhello\n";
        let caret = source.find("hello").expect("body") + 5;
        let mut ime = ImeHandlerProbe::open(source, caret);
        ime.replace_text_in_range(None, "!");
        let pushed_body = ime.paint().expect("body origin after insert");

        let yaml = rect(24.0, 48.0, 360.0, 72.0);
        let draft = "title: Hi\nmore: wrapped";
        ime.focus_widget_at(yaml, draft, draft.find("wrapped").expect("mid") + 3);
        let inner = inner_widget_caret_rect(yaml, draft, draft.find("wrapped").expect("mid") + 3);
        let pushed_yaml = ime.paint().expect("YAML focus must push a new IME origin");
        assert_eq!(ime.ime.owner(), Some(ImeOwner::Widget));
        assert_eq!(pushed_yaml, inner);
        assert_ne!(
            pushed_yaml,
            widget_caret_rect(yaml),
            "YAML IME origin must be the inner `|`, not the overlay right edge"
        );
        assert!(
            f32::from(pushed_yaml.origin.y) > f32::from(yaml.origin.y),
            "YAML inner caret must sit on the wrapped line, not the overlay top"
        );
        assert_ne!(pushed_yaml, pushed_body);
        assert_eq!(ime.last_platform_request(), Some(pushed_yaml));

        ime.replace_and_mark_text_in_range("ni");
        ime.assert_preedit_origin(pushed_yaml, "after YAML focus");
        let composed = ime
            .paint()
            .expect("YAML composition start must request a platform IME rect");
        assert_eq!(composed, pushed_yaml);
        assert_eq!(ime.paint(), None);
        ime.assert_macos_pull_keeps(pushed_yaml, "YAML after composition paint");
    }

    #[test]
    fn macos_first_rect_pull_after_begin_frame_follows_caret_on_each_surface() {
        let mut body = ImeHandlerProbe::open("hello\n", 5);
        let origin_body = body.paint().expect("body origin");
        body.assert_macos_pull_keeps(origin_body, "body paragraph");

        let mut table = ImeHandlerProbe::open("hello\n\n| a | b |\n|---|---|\n| 1 | 2 |\n", 5);
        table.replace_text_in_range(None, "!");
        let _ = table.paint();
        let table_block = table
            .engine
            .tree()
            .blocks
            .iter()
            .find(|b| matches!(b.kind, markrust_core::rich::BlockKind::Table { .. }))
            .expect("table");
        let cell_a = table
            .engine
            .cell_caret(table_block.id, 0, 0)
            .expect("header cell a");
        table.jump_to(cell_a);
        table.apply_rich(RichCommand::TableTab { reverse: false });
        let origin_cell = table.paint().expect("cell origin");
        table.assert_macos_pull_keeps(origin_cell, "table cell");

        let mut caption = ImeHandlerProbe::open("hello\n\n![cat](img.png)\n", 5);
        let overlay = rect(24.0, 260.0, 160.0, 16.0);
        caption.focus_widget_at(overlay, "cat", 1);
        let origin_caption = caption.paint().expect("caption origin");
        assert_ne!(origin_caption, widget_caret_rect(overlay));
        caption.assert_macos_pull_keeps(origin_caption, "image caption");

        let mut chip = ImeHandlerProbe::open("hello\n\n```rust\nfn main() {}\n```\n", 5);
        let chip_bounds = rect(24.0, 120.0, 48.0, 18.0);
        chip.focus_widget_at(chip_bounds, "rust", 2);
        let origin_chip = chip.paint().expect("chip origin");
        assert_ne!(origin_chip, widget_caret_rect(chip_bounds));
        chip.assert_macos_pull_keeps(origin_chip, "language chip");

        let source = "---\ntitle: Hi\n---\n\nhello\n";
        let caret = source.find("hello").expect("body") + 5;
        let mut title = ImeHandlerProbe::open(source, caret);
        let title_bounds = rect(24.0, 8.0, 220.0, 20.0);
        title.focus_widget_at(title_bounds, "Hi", 1);
        let origin_title = title.paint().expect("frontmatter title origin");
        assert_ne!(origin_title, widget_caret_rect(title_bounds));
        title.assert_macos_pull_keeps(origin_title, "frontmatter title");

        let mut yaml = ImeHandlerProbe::open(source, caret);
        let yaml_bounds = rect(24.0, 48.0, 360.0, 72.0);
        let draft = "title: Hi\nmore: wrapped";
        yaml.focus_widget_at(yaml_bounds, draft, draft.find("wrapped").expect("mid") + 3);
        let origin_yaml = yaml.paint().expect("YAML origin");
        assert_ne!(origin_yaml, widget_caret_rect(yaml_bounds));
        assert!(
            f32::from(origin_yaml.origin.y) > f32::from(yaml_bounds.origin.y),
            "YAML OS pull must sit on the wrapped inner line"
        );
        yaml.assert_macos_pull_keeps(origin_yaml, "frontmatter YAML");
    }

    #[test]
    fn widget_ime_origin_is_inner_caret_not_overlay_trailing_edge() {
        let overlay = rect(24.0, 260.0, 160.0, 16.0);
        let draft = "caption text";
        let caret = 4; // mid-draft
        let inner = inner_widget_caret_rect(overlay, draft, caret);
        let trailing = widget_caret_rect(overlay);
        assert!(
            f32::from(inner.origin.x) < f32::from(overlay.origin.x + overlay.size.width),
            "inner caret must sit inside the overlay, not on its right edge"
        );
        assert_ne!(inner, trailing);

        let mut ime = ImeOriginState::default();
        ime.begin_frame(true, 0, Some("ni"));
        ime.report_widget(overlay);
        ime.report_widget_caret(inner);
        assert_eq!(ime.owner(), Some(ImeOwner::Widget));
        assert_eq!(ime.caret_rect(), Some(inner));
        assert_ne!(
            ime.caret_rect(),
            Some(trailing),
            "must fail if origin is the overlay right edge while the caret is mid-draft"
        );

        let local = ime.take_platform_push().expect("inner origin push");
        let payload = gpui_update_ime_position_payload(
            &ime,
            Some(4..6),
            UTF16Selection {
                range: 6..6,
                reversed: false,
            },
        );
        let mut spy = PlatformImeSpy::default();
        spy.record(payload);
        assert_eq!(local, inner);
        assert_eq!(spy.last(), Some(inner));
        assert_eq!(
            payload, inner,
            "compute_ime_candidate_bounds must follow the inner `|` rect"
        );
        assert_ne!(
            payload, trailing,
            "platform IME rect must not be the overlay right edge while the caret is mid-draft"
        );
        assert_ne!(payload, decoy_element_bounds());
    }

    #[test]
    fn wrapped_line_platform_payload_is_second_line_caret() {
        let mut ime = ImeOriginState::default();
        let mut spy = PlatformImeSpy::default();
        let leaf_bounds = rect(8.0, 10.0, 240.0, 66.0);
        let second_line = rect(8.0, 32.0, 2.0, 22.0);
        ime.begin_frame(false, 40, Some("ni"));
        ime.report_leaf(hit(
            "a long paragraph that wraps onto a second visual line here",
            0,
            leaf_bounds,
            Some(second_line),
        ));
        let local = ime.take_platform_push().expect("wrapped origin");
        let payload = gpui_update_ime_position_payload(
            &ime,
            Some(40..42),
            UTF16Selection {
                range: 42..42,
                reversed: false,
            },
        );
        spy.record(payload);
        assert_eq!(local, second_line);
        assert_eq!(spy.last(), Some(second_line));
        assert!(
            f32::from(payload.origin.y) > f32::from(leaf_bounds.origin.y),
            "platform IME rect must sit on the wrapped line, not the leaf top"
        );
    }

    #[test]
    fn composition_update_re_requests_platform_ime_rect() {
        let mut ime = ImeHandlerProbe::open("hello\n", 5);
        let origin = ime.paint().expect("initial origin");
        ime.replace_and_mark_text_in_range("ni");
        assert_eq!(
            ime.paint().expect("composition start"),
            origin,
            "start must push the caret origin to the platform"
        );
        ime.replace_and_mark_text_in_range("nihongo");
        assert_eq!(ime.paint().expect("preedit update must re-push"), origin);
        assert_eq!(ime.last_platform_request(), Some(origin));
        assert_eq!(ime.paint(), None);
    }

    #[test]
    fn replace_text_in_range_commits_preedit_and_pushes_new_origin() {
        let mut ime = ImeHandlerProbe::open("hello\n", 5);
        ime.paint();
        ime.replace_and_mark_text_in_range("ni");
        assert_eq!(ime.source(), "hello\n");
        ime.replace_text_in_range(None, "!");
        assert!(ime.preedit.is_none());
        assert!(ime.marked_text_range().is_none());
        assert_eq!(ime.source(), "hello!\n");
        let pushed = ime
            .paint()
            .expect("commit must push the post-insert origin");
        assert_eq!(ime.ime.caret_rect(), Some(pushed));
        assert_ne!(pushed, decoy_element_bounds());
    }
}
