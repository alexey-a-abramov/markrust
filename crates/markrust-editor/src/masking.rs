// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use markrust_core::{DelimiterSpan, SyntaxNodeSpan};

/// Whether a delimiter is painted normally or masked (zero glyph advance, alpha 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VisibilityState {
    /// Delimiter glyphs are hidden in the WYSIWYG view.
    #[default]
    Masked,
    /// Delimiter glyphs are shown (typically in a muted accent color).
    Visible,
}

/// A caret position as a UTF-8 byte offset into the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caret {
    pub offset: usize,
}

impl Caret {
    pub fn new(offset: usize) -> Self {
        Self { offset }
    }
}

/// An inclusive-exclusive byte range selection `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub start: usize,
    pub end: usize,
}

impl Selection {
    pub fn new(start: usize, end: usize) -> Self {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        Self { start, end }
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub fn overlaps(&self, span_start: usize, span_end: usize) -> bool {
        self.start < span_end && self.end > span_start
    }
}

/// Convenience alias for byte ranges used in tests and layout code.
pub type ByteRange = Selection;

/// Returns whether a syntax span should reveal its delimiters for the given cursors.
pub fn delimiter_visibility_for_span(
    span: &SyntaxNodeSpan,
    carets: &[Caret],
    selections: &[Selection],
) -> VisibilityState {
    let caret_inside = carets
        .iter()
        .any(|caret| span.contains_offset(caret.offset));
    if caret_inside {
        return VisibilityState::Visible;
    }

    let selection_overlaps = selections
        .iter()
        .any(|sel| !sel.is_empty() && sel.overlaps(span.start_byte, span.end_byte));
    if selection_overlaps {
        return VisibilityState::Visible;
    }

    VisibilityState::Masked
}

/// Compute delimiter visibility for every delimiter across all syntax spans.
///
/// The returned vector has one entry per delimiter in span order (flattened left-to-right).
pub fn compute_visibility(
    carets: &[Caret],
    selections: &[Selection],
    spans: &[SyntaxNodeSpan],
) -> Vec<VisibilityState> {
    let mut result = Vec::new();
    for span in spans {
        let state = delimiter_visibility_for_span(span, carets, selections);
        for _delimiter in &span.delimiter_spans {
            result.push(state);
        }
    }
    result
}

/// Per-delimiter visibility with source span metadata for layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelimiterVisibilityEntry {
    pub span_index: usize,
    pub delimiter: DelimiterSpan,
    pub state: VisibilityState,
}

pub fn compute_delimiter_entries(
    carets: &[Caret],
    selections: &[Selection],
    spans: &[SyntaxNodeSpan],
) -> Vec<DelimiterVisibilityEntry> {
    spans
        .iter()
        .enumerate()
        .flat_map(|(span_index, span)| {
            let state = delimiter_visibility_for_span(span, carets, selections);
            span.delimiter_spans
                .iter()
                .copied()
                .map(move |delimiter| DelimiterVisibilityEntry {
                    span_index,
                    delimiter,
                    state,
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use markrust_core::{DelimiterSpan, SyntaxKind, SyntaxNodeSpan};

    fn bold_span(start: usize, end: usize) -> SyntaxNodeSpan {
        SyntaxNodeSpan {
            kind: SyntaxKind::Bold,
            start_byte: start,
            end_byte: end,
            delimiter_spans: vec![
                DelimiterSpan::new(start, start + 2),
                DelimiterSpan::new(end - 2, end),
            ],
            language: None,
            task_checked: None,
            table_row: None,
        }
    }

    fn italic_span(start: usize, end: usize) -> SyntaxNodeSpan {
        SyntaxNodeSpan {
            kind: SyntaxKind::Italic,
            start_byte: start,
            end_byte: end,
            delimiter_spans: vec![
                DelimiterSpan::new(start, start + 1),
                DelimiterSpan::new(end - 1, end),
            ],
            language: None,
            task_checked: None,
            table_row: None,
        }
    }

    #[test]
    fn masks_delimiters_when_caret_outside() {
        let spans = vec![bold_span(0, 8)];
        let carets = vec![Caret::new(20)];
        let vis = compute_visibility(&carets, &[], &spans);
        assert_eq!(vis, vec![VisibilityState::Masked, VisibilityState::Masked]);
    }

    #[test]
    fn reveals_when_caret_inside_span() {
        let spans = vec![bold_span(0, 8)];
        let carets = vec![Caret::new(4)];
        let vis = compute_visibility(&carets, &[], &spans);
        assert_eq!(
            vis,
            vec![VisibilityState::Visible, VisibilityState::Visible]
        );
    }

    #[test]
    fn reveals_on_boundary_start_and_end() {
        let spans = vec![bold_span(0, 8)];
        for offset in [0, 8] {
            let carets = vec![Caret::new(offset)];
            let vis = compute_visibility(&carets, &[], &spans);
            assert_eq!(
                vis,
                vec![VisibilityState::Visible, VisibilityState::Visible],
                "offset {offset}"
            );
        }
    }

    #[test]
    fn reveals_when_selection_overlaps_without_caret_inside() {
        let spans = vec![bold_span(10, 18)];
        let carets = vec![Caret::new(0)];
        let selections = vec![Selection::new(12, 14)];
        let vis = compute_visibility(&carets, &selections, &spans);
        assert_eq!(
            vis,
            vec![VisibilityState::Visible, VisibilityState::Visible]
        );
    }

    #[test]
    fn masks_when_selection_adjacent_but_not_overlapping() {
        let spans = vec![bold_span(10, 18)];
        let carets = vec![Caret::new(0)];
        let selections = vec![Selection::new(0, 10)];
        let vis = compute_visibility(&carets, &selections, &spans);
        assert_eq!(vis, vec![VisibilityState::Masked, VisibilityState::Masked]);
    }

    #[test]
    fn multi_cursor_any_inside_reveals() {
        let spans = vec![bold_span(10, 18)];
        let carets = vec![Caret::new(0), Caret::new(14)];
        let vis = compute_visibility(&carets, &[], &spans);
        assert_eq!(
            vis,
            vec![VisibilityState::Visible, VisibilityState::Visible]
        );
    }

    #[test]
    fn multiple_spans_mixed_visibility() {
        let spans = vec![bold_span(0, 8), italic_span(12, 20)];
        let carets = vec![Caret::new(14)];
        let vis = compute_visibility(&carets, &[], &spans);
        assert_eq!(
            vis,
            vec![
                VisibilityState::Masked,
                VisibilityState::Masked,
                VisibilityState::Visible,
                VisibilityState::Visible,
            ]
        );
    }

    #[test]
    fn selection_drag_partial_overlap() {
        let spans = vec![bold_span(5, 13)];
        let selections = vec![Selection::new(0, 6)];
        let vis = compute_visibility(&[], &selections, &spans);
        assert_eq!(
            vis,
            vec![VisibilityState::Visible, VisibilityState::Visible]
        );
    }

    #[test]
    fn empty_selection_does_not_reveal() {
        let spans = vec![bold_span(0, 8)];
        let selections = vec![Selection::new(4, 4)];
        let vis = compute_visibility(&[Caret::new(20)], &selections, &spans);
        assert_eq!(vis, vec![VisibilityState::Masked, VisibilityState::Masked]);
    }

    #[test]
    fn integration_with_extracted_spans() {
        let source = "**bold** plain *italic*";
        let spans = markrust_core::extract_syntax_spans(source);
        let carets = vec![Caret::new(source.find("italic").unwrap())];
        let vis = compute_visibility(&carets, &[], &spans);
        assert!(vis.contains(&VisibilityState::Visible));
        assert!(vis.contains(&VisibilityState::Masked));
    }
}
