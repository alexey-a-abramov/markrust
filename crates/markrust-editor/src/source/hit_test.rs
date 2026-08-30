// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pixel → source-byte hit testing for the source editor (no GPUI).

use super::layout::DisplayLayout;

/// Cached per-frame metrics used to map a click to a document byte.
#[derive(Debug, Clone, Default)]
pub struct ClickMap {
    pub line_heights: Vec<f32>,
    pub display_line_starts: Vec<usize>,
    /// Per-line x positions (pixels from the line origin) at each UTF-8 index
    /// in that display line, length `display_line_len + 1`.
    pub line_x_at: Vec<Vec<f32>>,
    /// Document byte for each display-text byte (length `display_text.len() + 1`).
    pub display_to_doc: Vec<usize>,
}

impl ClickMap {
    pub fn from_layout(layout: &DisplayLayout) -> Self {
        Self {
            line_heights: Vec::new(),
            display_line_starts: Vec::new(),
            line_x_at: Vec::new(),
            display_to_doc: invert_doc_to_display(layout),
        }
    }

    /// Map a click in editor-local pixels (origin at content top-left, after
    /// the gutter) to a source byte offset.
    pub fn offset_at(&self, relative_x: f32, relative_y: f32) -> usize {
        click_byte_offset(
            relative_x,
            relative_y,
            &self.line_heights,
            &self.display_line_starts,
            &self.line_x_at,
            &self.display_to_doc,
        )
    }
}

pub fn invert_doc_to_display(layout: &DisplayLayout) -> Vec<usize> {
    let n = layout.display_text.len();
    let mut slots: Vec<Option<usize>> = vec![None; n + 1];
    for (doc, mapped) in layout.doc_to_display.iter().enumerate() {
        if let Some(d) = *mapped {
            if d <= n {
                slots[d] = Some(doc);
            }
        }
    }
    let mut last = 0usize;
    for slot in slots.iter_mut() {
        if let Some(doc) = *slot {
            last = doc;
        } else {
            *slot = Some(last);
        }
    }
    slots.into_iter().map(|s| s.unwrap_or(0)).collect()
}

pub fn click_byte_offset(
    relative_x: f32,
    relative_y: f32,
    line_heights: &[f32],
    display_line_starts: &[usize],
    line_x_at: &[Vec<f32>],
    display_to_doc: &[usize],
) -> usize {
    if line_heights.is_empty() {
        return display_to_doc.first().copied().unwrap_or(0);
    }
    let mut y = 0.0f32;
    let mut line_idx = 0usize;
    for (i, height) in line_heights.iter().enumerate() {
        if relative_y < y + *height || i + 1 == line_heights.len() {
            line_idx = i;
            break;
        }
        y += height;
    }
    let display_start = display_line_starts.get(line_idx).copied().unwrap_or(0);
    let local = closest_index_in_xs(
        line_x_at.get(line_idx).map(Vec::as_slice).unwrap_or(&[]),
        relative_x.max(0.0),
    );
    let display = display_start.saturating_add(local);
    display_to_doc
        .get(display)
        .copied()
        .or_else(|| display_to_doc.last().copied())
        .unwrap_or(0)
}

pub fn closest_index_in_xs(xs: &[f32], x: f32) -> usize {
    if xs.is_empty() {
        return 0;
    }
    let mut best = 0usize;
    let mut best_dist = f32::MAX;
    for (i, &xi) in xs.iter().enumerate() {
        let d = (xi - x).abs();
        if d < best_dist {
            best_dist = d;
            best = i;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::build_display_layout;
    use crate::masking::Caret;
    use crate::theme::EditorTheme;

    #[test]
    fn click_x_maps_to_byte_not_line_start() {
        // One line, four cells at x = 0, 8, 16, 24, 32 → click near 20 → index 2 or 3.
        let offset = click_byte_offset(
            20.0,
            4.0,
            &[16.0],
            &[0],
            &[vec![0.0, 8.0, 16.0, 24.0, 32.0]],
            &[0, 1, 2, 3, 4],
        );
        assert_eq!(offset, 2);
    }

    #[test]
    fn click_y_selects_second_line() {
        let offset = click_byte_offset(
            0.0,
            20.0,
            &[16.0, 16.0],
            &[0, 4],
            &[vec![0.0, 8.0], vec![0.0, 8.0, 16.0]],
            &[0, 1, 2, 3, 4, 5, 6],
        );
        assert_eq!(offset, 4);
    }

    #[test]
    fn wrapped_visual_row_uses_that_row_not_the_last() {
        // One logical line wrapped into two visual rows (display 0..4 and 4..9).
        let offset = click_byte_offset(
            8.0,
            4.0,
            &[16.0, 16.0],
            &[0, 4],
            &[vec![0.0, 8.0, 16.0, 24.0, 32.0], vec![0.0, 8.0, 16.0, 24.0, 32.0]],
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        );
        assert_eq!(offset, 1);
        let offset = click_byte_offset(
            8.0,
            20.0,
            &[16.0, 16.0],
            &[0, 4],
            &[vec![0.0, 8.0, 16.0, 24.0, 32.0], vec![0.0, 8.0, 16.0, 24.0, 32.0]],
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        );
        assert_eq!(offset, 5);
    }

    fn bold_span(start: usize, end: usize) -> markrust_core::SyntaxNodeSpan {
        markrust_core::SyntaxNodeSpan {
            kind: markrust_core::SyntaxKind::Bold,
            start_byte: start,
            end_byte: end,
            delimiter_spans: vec![
                markrust_core::DelimiterSpan::new(start, start + 2),
                markrust_core::DelimiterSpan::new(end - 2, end),
            ],
            language: None,
            task_checked: None,
            table_row: None,
        }
    }

    #[test]
    fn masked_bold_click_maps_display_to_source() {
        let content = "**bold**";
        let spans = vec![bold_span(0, 8)];
        let layout = build_display_layout(
            content,
            &spans,
            &[Caret::new(99)],
            &[],
            &EditorTheme::dark(),
        );
        assert_eq!(layout.display_text, "**bold**");
        let map = invert_doc_to_display(&layout);
        // Display index 2 is the 'b' of bold, which is source byte 2.
        assert_eq!(
            map[2], 2,
            "display_to_doc={map:?} display={:?}",
            layout.display_text
        );
        let offset = click_byte_offset(
            0.0,
            0.0,
            &[20.0],
            &[0],
            &[vec![0.0, 8.0, 16.0, 24.0, 32.0, 40.0, 48.0, 56.0, 64.0]],
            &map,
        );
        assert_eq!(offset, 0);
    }
}
