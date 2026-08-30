// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IME candidate origin for the WYSIWYG surface.
//!
//! GPUI asks [`EntityInputHandler::bounds_for_range`] for the rectangle the OS
//! should pin the IME candidate window to. Leaves and chip/caption/frontmatter
//! widgets report geometry as they paint; this module resolves that noise into
//! **one** caret rect from the focused widget or the leaf that owns the
//! document caret — not whichever text leaf happened to paint last.

use std::sync::Arc;

use gpui::{point, px, size, Bounds, Pixels, Point};

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
}

impl ImeOriginState {
    pub fn begin_frame(&mut self, widget_focused: bool, caret_source: usize) {
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
}
