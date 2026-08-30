// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IME candidate origin for the WYSIWYG surface.
//!
//! Two platform paths must agree on the same rectangle:
//!
//! - **Pull:** GPUI asks [`EntityInputHandler::bounds_for_range`] (macOS
//!   `firstRectForCharacterRange:`) for the OS candidate window.
//! - **Push:** after a caret move or widget focus, the view calls
//!   `Window::invalidate_character_coordinates` (GPUI's equivalent of
//!   `set_ime_cursor_position`) so the OS re-queries that origin instead of
//!   keeping a stale candidate window. Call it only after this frame's leaves
//!   and widgets have reported — `ImeOriginState` is last-paint geometry, not
//!   a live layout snapshot.
//!
//! Leaves and chip/caption/frontmatter widgets report geometry as they paint;
//! this module resolves that noise into **one** caret rect from the focused
//! widget or the leaf that owns the document caret — not whichever text leaf
//! happened to paint last.
//!
//! Composition tests drive the same [`gpui::EntityInputHandler`] methods the OS
//! IME uses (`replace_and_mark_text_in_range`, `selected_text_range`,
//! `bounds_for_range`, `replace_text_in_range`). That is not a real CJK
//! candidate window: GPUI's test platform does not record
//! `update_ime_position` / `set_ime_cursor_position`.

use std::ops::Range;
use std::sync::Arc;

use gpui::{point, px, size, Bounds, Pixels, Point, UTF16Selection};

use super::block_text::LeafLayout;

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

/// Per-frame IME geometry. Cleared at the start of each render.
#[derive(Default)]
pub struct ImeOriginState {
    widget_focused: bool,
    widget_bounds: Option<Bounds<Pixels>>,
    caret_source: usize,
    leaves: Vec<ImeLeafHit>,
    /// Bumps when the caret source or widget-focus flag changes so a caret
    /// move still pushes even if two surfaces happen to share a rectangle.
    caret_generation: u64,
    last_pushed_generation: Option<u64>,
    last_platform_origin: Option<Bounds<Pixels>>,
}

impl ImeOriginState {
    pub fn begin_frame(&mut self, widget_focused: bool, caret_source: usize) {
        if self.widget_focused != widget_focused || self.caret_source != caret_source {
            self.caret_generation = self.caret_generation.wrapping_add(1);
        }
        self.widget_focused = widget_focused;
        self.caret_source = caret_source;
        self.widget_bounds = None;
        self.leaves.clear();
    }

    pub fn widget_focused(&self) -> bool {
        self.widget_focused
    }

    pub fn report_widget(&mut self, bounds: Bounds<Pixels>) {
        self.widget_bounds = Some(bounds);
    }

    pub fn report_leaf(&mut self, leaf: ImeLeafHit) {
        self.leaves.push(leaf);
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
    pub fn caret_rect(&self) -> Option<Bounds<Pixels>> {
        if self.widget_focused {
            return self.widget_bounds.map(widget_caret_rect);
        }
        self.focused_leaf().map(leaf_caret_or_fallback)
    }

    /// Resolved origin to push to the platform IME cursor API.
    ///
    /// Returns `Some` when the origin changed since the last push (caret
    /// move, widget focus, or a new painted rect). The view must call
    /// [`gpui::Window::invalidate_character_coordinates`] with this — not only
    /// wait for `bounds_for_range`.
    pub fn take_platform_push(&mut self) -> Option<Bounds<Pixels>> {
        let rect = self.caret_rect()?;
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
}

/// Trailing edge of the focused chip / caption / frontmatter overlay.
///
/// Widgets append a `|` at the end of the draft (including wrapped YAML), so
/// the bottom-right of the overlay is the caret, not the top-left of a body
/// text leaf painted later in the same frame.
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

/// Pull path used by [`gpui::EntityInputHandler::bounds_for_range`].
pub(super) fn ime_origin_bounds(
    ime: &ImeOriginState,
    element_bounds: Bounds<Pixels>,
) -> Bounds<Pixels> {
    if let Some(caret) = ime.caret_rect() {
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

pub(super) fn widget_selected_text_range(draft: &str, preedit: Option<&str>) -> UTF16Selection {
    let content = format!("{}{}", draft, preedit.unwrap_or_default());
    let n = offset_to_utf16(&content, content.len());
    UTF16Selection {
        range: n..n,
        reversed: false,
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
    range_utf16: Option<Range<usize>>,
    new_text: &str,
) {
    if let Some(range_utf16) = range_utf16 {
        let content = draft.clone();
        let start = offset_from_utf16(&content, range_utf16.start).min(draft.len());
        let end = offset_from_utf16(&content, range_utf16.end)
            .min(draft.len())
            .max(start);
        draft.replace_range(start..end, new_text);
    } else {
        draft.push_str(new_text);
    }
}

fn leaf_caret_or_fallback(leaf: &ImeLeafHit) -> Bounds<Pixels> {
    leaf.caret_bounds.unwrap_or_else(|| Bounds {
        origin: leaf.bounds.origin,
        size: size(
            px(2.),
            px(leaf.line_height)
                .min(leaf.bounds.size.height)
                .max(px(2.)),
        ),
    })
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
    use markrust_core::rich::{apply_rich_command, RichCommand};

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
        ime.begin_frame(false, caret);
        for leaf in leaves {
            ime.report_leaf(leaf);
        }
        ime
    }

    fn widget_frame(
        widget: Bounds<Pixels>,
        decoy_leaves: Vec<ImeLeafHit>,
        caret_in_body: usize,
    ) -> ImeOriginState {
        let mut ime = ImeOriginState::default();
        ime.begin_frame(true, caret_in_body);
        ime.report_widget(widget);
        for leaf in decoy_leaves {
            ime.report_leaf(leaf);
        }
        ime
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
        ime.begin_frame(true, 0);
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
        ime.begin_frame(true, 0);
        ime.report_widget(rect(24.0, 8.0, 100.0, 18.0));
        assert_eq!(ime.owner(), Some(ImeOwner::Widget));

        let body = rect(24.0, 96.0, 2.0, 22.0);
        ime.begin_frame(false, 4);
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
    ) {
        ime.begin_frame(widget.is_some(), caret);
        if let Some(bounds) = widget {
            ime.report_widget(bounds);
        }
        let mut y = 10.0;
        report_tree_leaves(ime, &engine.tree().blocks, caret, &mut y);
    }

    #[test]
    fn platform_push_fires_once_per_origin_change() {
        let mut ime = ImeOriginState::default();
        let body = rect(10.0, 40.0, 2.0, 22.0);
        ime.begin_frame(false, 3);
        ime.report_leaf(hit("hello", 0, rect(8.0, 40.0, 200.0, 22.0), Some(body)));
        assert_eq!(ime.take_platform_push(), Some(body));
        assert_eq!(
            ime.take_platform_push(),
            None,
            "same origin must not re-push"
        );
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
        paint_engine_frame(&mut ime, &engine, body_caret, None);
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

        paint_engine_frame(&mut ime, &engine, cell_caret, None);
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
        paint_engine_frame(&mut ime, &engine, body_caret, None);
        let origin_body = ime.caret_rect().expect("body origin after edit");
        assert_eq!(ime.take_platform_push(), Some(origin_body));

        let caption = rect(24.0, 260.0, 160.0, 16.0);
        paint_engine_frame(&mut ime, &engine, body_caret, Some(caption));
        let origin_caption = ime.caret_rect().expect("caption origin");
        assert_eq!(ime.owner(), Some(ImeOwner::Widget));
        assert_eq!(origin_caption, widget_caret_rect(caption));
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

    /// Headless stand-in for [`gpui::EntityInputHandler`] on the WYSIWYG view.
    ///
    /// GPUI's `TestWindow::update_ime_position` is a no-op, so there is no
    /// `set_ime_cursor_position` readout. After paint we assert
    /// [`ImeOriginState::take_platform_push`] — the value `sync_ime_cursor`
    /// uses before `Window::invalidate_character_coordinates`.
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
        widget_preedit: Option<String>,
        widget_bounds: Option<Bounds<Pixels>>,
        ime: ImeOriginState,
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
                widget_preedit: None,
                widget_bounds: None,
                ime: ImeOriginState::default(),
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
            paint_engine_frame(&mut self.ime, &self.engine, caret, self.widget_bounds);
            self.ime.take_platform_push()
        }

        /// `EntityInputHandler::selected_text_range`
        fn selected_text_range(&self) -> UTF16Selection {
            if let Some(draft) = self.widget_draft.as_deref() {
                return widget_selected_text_range(draft, self.widget_preedit.as_deref());
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
                replace_in_widget_draft(draft, range_utf16, new_text);
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

        /// GPUI's IME candidate-rect helper (`PlatformInputHandler`).
        ///
        /// Not `set_ime_cursor_position`: the test window does not record that.
        fn candidate_bounds(&self) -> Bounds<Pixels> {
            let selection = self.selected_text_range();
            gpui::PlatformInputHandler::compute_ime_candidate_bounds(
                self.marked_text_range(),
                &selection,
                |range| Some(self.bounds_for_range(range)),
            )
            .expect("IME candidate origin from handler")
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

        fn focus_widget(&mut self, bounds: Bounds<Pixels>, draft: &str) {
            self.widget_draft = Some(draft.to_string());
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
        assert_eq!(
            ime.paint(),
            None,
            "same caret during preedit must not re-push"
        );
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
        ime.focus_widget(caption, "cat");
        let pushed_caption = ime
            .paint()
            .expect("caption focus must push a new IME origin");
        assert_eq!(ime.ime.owner(), Some(ImeOwner::Widget));
        assert_eq!(pushed_caption, widget_caret_rect(caption));
        assert_ne!(pushed_caption, pushed_body);

        let source_before = ime.source();
        ime.replace_and_mark_text_in_range("ni");
        assert_eq!(
            ime.source(),
            source_before,
            "widget preedit is display-only"
        );
        ime.assert_preedit_origin(pushed_caption, "after caption focus");
        assert_eq!(ime.paint(), None);
    }

    #[test]
    fn composition_after_language_chip_focus_tracks_widget_via_handler() {
        let mut ime = ImeHandlerProbe::open("hello\n\n```rust\nfn main() {}\n```\n", 5);
        ime.replace_text_in_range(None, "!");
        let pushed_body = ime.paint().expect("body origin after insert");

        let chip = rect(24.0, 120.0, 48.0, 18.0);
        ime.focus_widget(chip, "rust");
        let pushed_chip = ime.paint().expect("chip focus must push a new IME origin");
        assert_eq!(ime.ime.owner(), Some(ImeOwner::Widget));
        assert_eq!(pushed_chip, widget_caret_rect(chip));
        assert_ne!(pushed_chip, pushed_body);

        ime.replace_and_mark_text_in_range("ni");
        ime.assert_preedit_origin(pushed_chip, "after language-chip focus");
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
        ime.focus_widget(title, "Hi");
        let pushed_fm = ime
            .paint()
            .expect("frontmatter focus must push a new IME origin");
        assert_eq!(ime.ime.owner(), Some(ImeOwner::Widget));
        assert_eq!(pushed_fm, widget_caret_rect(title));
        assert_ne!(pushed_fm, pushed_body);

        ime.replace_and_mark_text_in_range("ni");
        ime.assert_preedit_origin(pushed_fm, "after frontmatter focus");
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
