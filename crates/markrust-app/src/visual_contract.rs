// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Baseline-independent assertions joining source selection to painted glyphs.
//!
//! Expected highlights are derived from the actual text shaper's source-backed
//! caret stops. Actual highlights come from the scene quads, independently of
//! the selection painter's own intermediate rectangles.

use std::ops::Range;

use anyhow::{ensure, Context as _, Result};
use gpui::{Bounds, Hsla, Pixels, Window};
use markrust_editor::element::SourcePaintRow;
use markrust_editor::wysiwyg::PaintedLeafGeometry;

const TOLERANCE: f32 = 1.;

/// Window-relative logical pixels. Scene bounds must be divided by scale first.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Rect {
    pub(crate) fn from_bounds(bounds: Bounds<Pixels>) -> Self {
        Self {
            left: f32::from(bounds.left()),
            top: f32::from(bounds.top()),
            right: f32::from(bounds.right()),
            bottom: f32::from(bounds.bottom()),
        }
    }

    pub(crate) fn intersection(self, other: Self) -> Option<Self> {
        let intersection = Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        };
        (intersection.right > intersection.left && intersection.bottom > intersection.top)
            .then_some(intersection)
    }

    fn valid(self, allow_zero_width: bool) -> bool {
        [self.left, self.top, self.right, self.bottom]
            .iter()
            .all(|coordinate| coordinate.is_finite())
            && self.bottom > self.top
            && if allow_zero_width {
                self.right >= self.left
            } else {
                self.right > self.left
            }
    }

    fn matches(self, other: Self) -> bool {
        (self.left - other.left).abs() <= TOLERANCE
            && (self.top - other.top).abs() <= TOLERANCE
            && (self.right - other.right).abs() <= TOLERANCE
            && (self.bottom - other.bottom).abs() <= TOLERANCE
    }
}

/// Selection-colored scene fragments visible in one editor pane. All clipping
/// and scale conversion are taken from GPUI's completed native paint pass.
pub(crate) fn painted_selection_rectangles(
    window: &Window,
    color: Hsla,
    viewport: Rect,
) -> Vec<Rect> {
    let scale = window.scale_factor();
    window
        .painted_quads()
        .into_iter()
        .filter(|quad| quad.background == color.into())
        .filter_map(|quad| {
            Rect {
                left: quad.bounds.left().0.max(quad.content_mask.bounds.left().0) / scale,
                top: quad.bounds.top().0.max(quad.content_mask.bounds.top().0) / scale,
                right: quad
                    .bounds
                    .right()
                    .0
                    .min(quad.content_mask.bounds.right().0)
                    / scale,
                bottom: quad
                    .bounds
                    .bottom()
                    .0
                    .min(quad.content_mask.bounds.bottom().0)
                    / scale,
            }
            .intersection(viewport)
        })
        .collect()
}

/// Opaque editor backgrounds must precede highlights in the completed scene.
pub(crate) fn validate_selection_layering(
    window: &Window,
    selection_color: Hsla,
    background_color: Hsla,
    viewport: Rect,
    label: &str,
) -> Result<()> {
    let scale = window.scale_factor();
    let quads = window.painted_quads();
    let visible_rect = |quad: &gpui::Quad| {
        Rect {
            left: quad.bounds.left().0.max(quad.content_mask.bounds.left().0) / scale,
            top: quad.bounds.top().0.max(quad.content_mask.bounds.top().0) / scale,
            right: quad
                .bounds
                .right()
                .0
                .min(quad.content_mask.bounds.right().0)
                / scale,
            bottom: quad
                .bounds
                .bottom()
                .0
                .min(quad.content_mask.bounds.bottom().0)
                / scale,
        }
        .intersection(viewport)
    };
    for selection in quads
        .iter()
        .filter(|quad| quad.background == selection_color.into())
    {
        let Some(selected) = visible_rect(selection) else {
            continue;
        };
        for background in quads
            .iter()
            .filter(|quad| quad.background == background_color.into())
        {
            if visible_rect(background)
                .and_then(|background| selected.intersection(background))
                .is_some()
            {
                ensure!(selection.order > background.order,
                    "{label}: an opaque code background paints over the selection: selection order {:?}, background order {:?}", selection.order, background.order);
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_text_geometry(leaves: &[PaintedLeafGeometry], label: &str) -> Result<()> {
    ensure!(
        !leaves.is_empty(),
        "{label}: native paint produced no text leaves"
    );
    let mut rectangles = Vec::new();
    for (leaf_index, leaf) in leaves.iter().enumerate() {
        let allocated = Rect::from_bounds(leaf.bounds);
        ensure!(
            allocated.valid(true),
            "{label}: invalid leaf bounds: {allocated:?}"
        );
        ensure!(
            !leaf.lines.is_empty(),
            "{label}: leaf {leaf_index} has no shaped rows: {:?}",
            leaf.text
        );
        for (row_index, row) in leaf.lines.iter().enumerate() {
            let rect = Rect {
                left: row.left,
                top: row.top,
                right: row.right,
                bottom: row.top + row.height,
            };
            ensure!(
                rect.valid(true),
                "{label}: invalid glyph row {leaf_index}:{row_index}: {rect:?}"
            );
            ensure!(rect.top >= allocated.top - TOLERANCE && rect.bottom <= allocated.bottom + TOLERANCE,
                "{label}: glyph rows overflow allocated leaf height: leaf={:?}, allocated={allocated:?}, row={rect:?}", leaf.text);
            ensure!(rect.left >= allocated.left - TOLERANCE && rect.right <= allocated.right + TOLERANCE,
                "{label}: text escapes horizontal leaf bounds: leaf={:?}, allocated={allocated:?}, row={rect:?}", leaf.text);
            rectangles.push((leaf_index, row_index, rect));
        }
    }
    for (position, &(a_leaf, a_row, a)) in rectangles.iter().enumerate() {
        for &(b_leaf, b_row, b) in &rectangles[position + 1..] {
            // Wrapped rows in the same leaf must not overprint either.
            ensure!(a.right.min(b.right) - a.left.max(b.left) <= TOLERANCE
                || a.bottom.min(b.bottom) - a.top.max(b.top) <= TOLERANCE,
                "{label}: glyph rows overlap: {a_leaf}:{a_row} {a:?} ({:?}) and {b_leaf}:{b_row} {b:?} ({:?})",
                leaves[a_leaf].text, leaves[b_leaf].text);
        }
    }
    Ok(())
}

pub(crate) fn validate_selection_geometry(
    leaves: &[PaintedLeafGeometry],
    selection: &Range<usize>,
    viewport: Rect,
    painted: &[Rect],
    label: &str,
) -> Result<()> {
    ensure!(
        viewport.valid(false),
        "{label}: invalid selection viewport {viewport:?}"
    );
    ensure!(
        selection.start <= selection.end,
        "{label}: selection range is not normalized"
    );
    let mut expected = Vec::new();
    if !selection.is_empty() {
        for leaf in leaves {
            if selection.start >= leaf.source_range.end || selection.end <= leaf.source_range.start
            {
                continue;
            }
            // Match source offsets to visible glyph boundaries. Hidden Markdown
            // syntax can map many source positions to the same visual boundary.
            let projected = |source| {
                leaf.lines
                    .iter()
                    .flat_map(|row| &row.stops)
                    .filter(|stop| stop.source <= source)
                    .map(|stop| stop.visible)
                    .max()
                    .unwrap_or(0)
            };
            let selected = projected(selection.start)..projected(selection.end);
            for row in &leaf.lines {
                let start = selected.start.max(row.visible_start);
                let end = selected.end.min(row.visible_end);
                if start >= end {
                    continue;
                }
                let x_at = |visible| {
                    row.stops.iter().find(|stop| stop.visible == visible)
                        .map(|stop| stop.x)
                        .with_context(|| format!("{label}: selected byte has no shaped caret stop at visible {visible}"))
                };
                let rect = Rect {
                    left: x_at(start)?,
                    top: row.top,
                    right: x_at(end)?,
                    bottom: row.top + row.height,
                };
                if let Some(visible) = rect.intersection(viewport) {
                    expected.push(visible);
                }
            }
        }
    }
    match_selection_rectangles(&expected, painted, label)
}

pub(crate) fn validate_source_text_geometry(
    rows: &[SourcePaintRow],
    viewport: Rect,
    label: &str,
) -> Result<()> {
    ensure!(
        viewport.valid(false),
        "{label}: invalid source viewport {viewport:?}"
    );
    ensure!(
        !rows.is_empty(),
        "{label}: visible Source pane produced no native text rows"
    );
    for (index, row) in rows.iter().enumerate() {
        ensure!(
            Rect::from_bounds(row.bounds).valid(true),
            "{label}: invalid source row bounds at {index}"
        );
        ensure!(
            !row.caret_stops.is_empty(),
            "{label}: source row {index} has no shaped caret stops"
        );
    }
    Ok(())
}

pub(crate) fn validate_source_selection_geometry(
    rows: &[SourcePaintRow],
    selection: &Range<usize>,
    viewport: Rect,
    painted: &[Rect],
    label: &str,
) -> Result<()> {
    validate_source_text_geometry(rows, viewport, label)?;
    ensure!(
        selection.start <= selection.end,
        "{label}: selection range is not normalized"
    );
    let mut expected = Vec::new();
    if !selection.is_empty() {
        for row in rows {
            if selection.start >= row.source_range.end || selection.end <= row.source_range.start {
                continue;
            }
            let bounds = Rect::from_bounds(row.bounds);
            let first = row
                .caret_stops
                .first()
                .context("source row has no shaped caret stop")?;
            let last = row
                .caret_stops
                .last()
                .context("source row has no shaped caret stop")?;
            let x_at = |source| {
                row.caret_stops
                    .iter()
                    .rev()
                    .find(|(byte, _)| *byte <= source)
                    .unwrap_or(first)
                    .1
            };
            let left = x_at(selection.start.max(row.source_range.start));
            let selected_newline = last.0 < row.source_range.end
                && selection.start <= last.0
                && selection.end > last.0;
            let right = x_at(selection.end.min(last.0)) + if selected_newline { 2. } else { 0. };
            if let Some(visible) = (Rect {
                left,
                right,
                ..bounds
            })
            .intersection(viewport)
            {
                expected.push(visible);
            }
        }
    }
    match_selection_rectangles(&expected, painted, label)
}

fn match_selection_rectangles(expected: &[Rect], painted: &[Rect], label: &str) -> Result<()> {
    let mut matched = vec![false; expected.len()];
    for actual in painted {
        ensure!(
            actual.valid(false),
            "{label}: invalid painted selection rectangle: {actual:?}"
        );
        let matching = expected
            .iter()
            .enumerate()
            .find(|(index, rect)| !matched[*index] && actual.matches(**rect));
        let (index, _) = matching.with_context(|| {
            format!("{label}: unexpected selection highlight {actual:?}; expected source-backed spans {expected:?}")
        })?;
        matched[index] = true;
    }
    ensure!(matched.iter().all(|matched| *matched),
        "{label}: selected glyphs are missing highlights; expected {expected:?}, painted {painted:?}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use gpui::{point, px, size};
    use markrust_editor::wysiwyg::{PaintedLineGeometry, VisualCaretStop};

    use super::*;

    fn fixture() -> Vec<PaintedLeafGeometry> {
        vec![PaintedLeafGeometry {
            text: "abcd".into(),
            source_range: 10..14,
            bounds: Bounds::new(point(px(10.), px(10.)), size(px(20.), px(40.))),
            lines: [(0, 2, 10.), (2, 4, 30.)]
                .into_iter()
                .map(|(start, end, top)| PaintedLineGeometry {
                    visible_start: start,
                    visible_end: end,
                    top,
                    height: 20.,
                    left: 10.,
                    right: 30.,
                    stops: (start..=end)
                        .map(|visible| VisualCaretStop {
                            visible,
                            source: visible + 10,
                            x: 10. + (visible - start) as f32 * 10.,
                        })
                        .collect(),
                })
                .collect(),
        }]
    }

    fn viewport() -> Rect {
        Rect {
            left: 0.,
            top: 0.,
            right: 100.,
            bottom: 100.,
        }
    }

    fn row(top: f32, left: f32, right: f32) -> Rect {
        Rect {
            left,
            top,
            right,
            bottom: top + 20.,
        }
    }

    #[test]
    fn adjacent_wrapped_rows_are_valid_but_same_leaf_overlap_is_not() {
        let mut leaves = fixture();
        validate_text_geometry(&leaves, "valid").unwrap();
        leaves[0].lines[1].top = 20.;
        assert!(validate_text_geometry(&leaves, "overprint")
            .unwrap_err()
            .to_string()
            .contains("overlap"));
    }

    #[test]
    fn invalid_or_inverted_row_dimensions_are_rejected() {
        let mut leaves = fixture();
        leaves[0].lines[0].height = -2.;
        assert!(validate_text_geometry(&leaves, "negative").is_err());
        leaves[0].lines[0].height = f32::NAN;
        assert!(validate_text_geometry(&leaves, "nan").is_err());
    }

    #[test]
    fn wrapped_partial_selection_matches_each_selected_glyph_span() {
        validate_selection_geometry(
            &fixture(),
            &(11..13),
            viewport(),
            &[row(10., 20., 30.), row(30., 10., 20.)],
            "partial",
        )
        .unwrap();
    }

    #[test]
    fn missing_row_highlight_is_rejected() {
        let error = validate_selection_geometry(
            &fixture(),
            &(10..14),
            viewport(),
            &[row(10., 10., 30.)],
            "missing",
        )
        .unwrap_err();
        assert!(error.to_string().contains("missing highlights"));
    }

    #[test]
    fn wrong_horizontal_span_and_tall_multirow_rectangle_are_rejected() {
        assert!(validate_selection_geometry(
            &fixture(),
            &(11..13),
            viewport(),
            &[row(10., 10., 30.), row(30., 10., 20.)],
            "wrong column"
        )
        .is_err());
        assert!(validate_selection_geometry(
            &fixture(),
            &(10..14),
            viewport(),
            &[Rect {
                left: 10.,
                top: 10.,
                right: 30.,
                bottom: 50.
            }],
            "tall"
        )
        .is_err());
    }

    #[test]
    fn collapsed_selection_rejects_stale_and_duplicate_highlights() {
        validate_selection_geometry(&fixture(), &(11..11), viewport(), &[], "collapsed").unwrap();
        assert!(validate_selection_geometry(
            &fixture(),
            &(11..11),
            viewport(),
            &[row(10., 10., 30.)],
            "stale"
        )
        .is_err());
        assert!(validate_selection_geometry(
            &fixture(),
            &(10..14),
            viewport(),
            &[row(10., 10., 30.), row(10., 10., 30.), row(30., 10., 30.)],
            "duplicate"
        )
        .is_err());
    }

    #[test]
    fn selection_coverage_is_clipped_to_visible_viewport() {
        let viewport = Rect {
            top: 20.,
            bottom: 40.,
            ..viewport()
        };
        validate_selection_geometry(
            &fixture(),
            &(10..14),
            viewport,
            &[
                Rect {
                    left: 10.,
                    top: 20.,
                    right: 30.,
                    bottom: 30.,
                },
                Rect {
                    left: 10.,
                    top: 30.,
                    right: 30.,
                    bottom: 40.,
                },
            ],
            "clipped",
        )
        .unwrap();
    }

    fn source_fixture() -> Vec<SourcePaintRow> {
        [(10, 13, 10.), (13, 16, 30.), (16, 18, 50.)]
            .into_iter()
            .map(|(start, end, top)| SourcePaintRow {
                source_range: start..end,
                bounds: Bounds::new(point(px(10.), px(top)), size(px(20.), px(20.))),
                caret_stops: (start..=start + 2)
                    .map(|byte| (byte, 10. + (byte - start) as f32 * 10.))
                    .collect(),
            })
            .collect()
    }

    #[test]
    fn visible_source_requires_rows_even_without_selection_or_text() {
        assert!(
            validate_source_selection_geometry(&[], &(0..0), viewport(), &[], "blank render")
                .unwrap_err()
                .to_string()
                .contains("no native text rows")
        );
        let empty_document = [SourcePaintRow {
            source_range: 0..0,
            bounds: Bounds::new(point(px(10.), px(10.)), size(px(0.), px(20.))),
            caret_stops: vec![(0, 10.)],
        }];
        validate_source_selection_geometry(
            &empty_document,
            &(0..0),
            viewport(),
            &[],
            "empty document",
        )
        .unwrap();
    }

    #[test]
    fn source_partial_selection_covers_edges_middle_and_selected_newlines() {
        validate_source_selection_geometry(
            &source_fixture(),
            &(11..17),
            viewport(),
            &[row(10., 20., 32.), row(30., 10., 32.), row(50., 10., 20.)],
            "source partial",
        )
        .unwrap();
        assert!(validate_source_selection_geometry(
            &source_fixture(),
            &(11..17),
            viewport(),
            &[row(10., 20., 32.), row(50., 10., 20.)],
            "source missing middle"
        )
        .is_err());
    }

    #[test]
    fn source_newline_only_and_collapsed_selection_have_distinct_rendering() {
        validate_source_selection_geometry(
            &source_fixture(),
            &(12..13),
            viewport(),
            &[row(10., 30., 32.)],
            "newline",
        )
        .unwrap();
        validate_source_selection_geometry(
            &source_fixture(),
            &(13..13),
            viewport(),
            &[],
            "source collapsed",
        )
        .unwrap();
        assert!(validate_source_selection_geometry(
            &source_fixture(),
            &(13..13),
            viewport(),
            &[row(10., 30., 32.)],
            "source stale"
        )
        .is_err());
    }
}
