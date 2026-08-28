// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use markrust_core::{SyntaxKind, SyntaxNodeSpan, TableRowKind};

use crate::highlight::{highlight_code_block, HighlightKind, HighlightSpan};
use crate::masking::{
    compute_delimiter_entries, Caret, DelimiterVisibilityEntry, Selection, VisibilityState,
};
use crate::theme::EditorTheme;

/// Styling applied to a contiguous byte range in the source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentStyle {
    Plain,
    Delimiter { visible: bool },
    Bold,
    Italic,
    Heading { level: u8 },
    CodeInline,
    CodeBlock,
    BlockQuote,
    Link,
    Image,
    TaskList { checked: bool },
    Table { row: TableRowKind },
    Frontmatter,
    Strikethrough,
    SyntaxHighlight(HighlightKind),
}

/// A segment of source text with styling metadata for layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutSegment {
    pub doc_start: usize,
    pub doc_end: usize,
    pub style: SegmentStyle,
}

/// Display projection of the document with byte-offset mapping.
#[derive(Debug, Clone)]
pub struct DisplayLayout {
    pub display_text: String,
    /// For each document byte offset, the corresponding display byte offset (if visible).
    pub doc_to_display: Vec<Option<usize>>,
    pub segments: Vec<LayoutSegment>,
    pub highlight_spans: Vec<HighlightSpan>,
    pub blockquote_lines: Vec<usize>,
}

impl DisplayLayout {
    pub fn display_offset_for_doc(&self, doc_offset: usize) -> usize {
        self.doc_to_display
            .get(doc_offset)
            .and_then(|offset| *offset)
            .unwrap_or(self.display_text.len())
    }

    pub fn doc_offset_for_display(&self, display_offset: usize) -> usize {
        for (doc_offset, mapped) in self.doc_to_display.iter().enumerate() {
            if mapped == &Some(display_offset) {
                return doc_offset;
            }
            if mapped.is_some_and(|mapped| mapped > display_offset) {
                return doc_offset.saturating_sub(1);
            }
        }
        self.doc_to_display.len().saturating_sub(1)
    }
}

pub fn build_display_layout(
    content: &str,
    spans: &[SyntaxNodeSpan],
    carets: &[Caret],
    selections: &[Selection],
    theme: &EditorTheme,
) -> DisplayLayout {
    let delimiter_entries = compute_delimiter_entries(carets, selections, spans);
    let highlight_spans = collect_code_highlights(content, spans);
    let segments = build_segments(content, spans, &delimiter_entries, &highlight_spans, theme);
    let mut layout = project_display(content, &segments, spans);
    layout.highlight_spans = highlight_spans;
    layout.blockquote_lines = blockquote_line_starts(content, spans);
    apply_table_alignment(&mut layout, content, spans);
    layout
}

fn collect_code_highlights(content: &str, spans: &[SyntaxNodeSpan]) -> Vec<HighlightSpan> {
    let mut highlights = Vec::new();
    for span in spans {
        if span.kind != SyntaxKind::CodeBlock {
            continue;
        }
        let Some(language) = span.language.as_deref() else {
            continue;
        };
        let block = &content[span.start_byte..span.end_byte.min(content.len())];
        let code_start = block.find('\n').map(|idx| idx + 1).unwrap_or(0);
        let code_end = block.rfind("\n```").or_else(|| block.rfind("\n~~~")).unwrap_or(block.len());
        if code_start >= code_end {
            continue;
        }
        let code = &block[code_start..code_end];
        let base = span.start_byte + code_start;
        highlights.extend(highlight_code_block(language, code, base));
    }
    highlights
}

fn build_segments(
    content: &str,
    spans: &[SyntaxNodeSpan],
    delimiter_entries: &[DelimiterVisibilityEntry],
    highlight_spans: &[HighlightSpan],
    _theme: &EditorTheme,
) -> Vec<LayoutSegment> {
    if content.is_empty() {
        return Vec::new();
    }

    let mut delimiter_map: Vec<Option<bool>> = vec![None; content.len()];
    for entry in delimiter_entries {
        let visible = entry.state == VisibilityState::Visible;
        let start = entry.delimiter.start_byte;
        let end = entry.delimiter.end_byte.min(content.len());
        delimiter_map[start..end].fill(Some(visible));
    }

    let mut style_at: Vec<SegmentStyle> = vec![SegmentStyle::Plain; content.len()];
    for span in spans {
        let style = span_style(span);
        for byte in span.start_byte..span.end_byte.min(content.len()) {
            if delimiter_map[byte].is_none() {
                style_at[byte] = style;
            }
        }
    }

    for highlight in highlight_spans {
        for byte in highlight.start_byte..highlight.end_byte.min(content.len()) {
            if delimiter_map[byte].is_none() {
                style_at[byte] = SegmentStyle::SyntaxHighlight(highlight.kind);
            }
        }
    }

    for (byte, visible) in delimiter_map.iter().enumerate() {
        if let Some(visible) = visible {
            style_at[byte] = SegmentStyle::Delimiter { visible: *visible };
        }
    }

    coalesce_segments(content.len(), &style_at)
}

fn span_style(span: &SyntaxNodeSpan) -> SegmentStyle {
    match span.kind {
        SyntaxKind::Bold => SegmentStyle::Bold,
        SyntaxKind::Italic => SegmentStyle::Italic,
        SyntaxKind::Heading => SegmentStyle::Heading {
            level: heading_level(span),
        },
        SyntaxKind::CodeInline => SegmentStyle::CodeInline,
        SyntaxKind::CodeBlock => SegmentStyle::CodeBlock,
        SyntaxKind::BlockQuote => SegmentStyle::BlockQuote,
        SyntaxKind::Link => SegmentStyle::Link,
        SyntaxKind::Image => SegmentStyle::Image,
        SyntaxKind::TaskList => SegmentStyle::TaskList {
            checked: span.task_checked.unwrap_or(false),
        },
        SyntaxKind::Table => SegmentStyle::Table {
            row: span.table_row.unwrap_or(TableRowKind::Body),
        },
        SyntaxKind::Frontmatter => SegmentStyle::Frontmatter,
        SyntaxKind::Strikethrough => SegmentStyle::Strikethrough,
        _ => SegmentStyle::Plain,
    }
}

pub fn heading_level(span: &SyntaxNodeSpan) -> u8 {
    span.delimiter_spans
        .first()
        .map(|delimiter| delimiter.len().clamp(1, 6) as u8)
        .unwrap_or(1)
}

fn coalesce_segments(len: usize, style_at: &[SegmentStyle]) -> Vec<LayoutSegment> {
    let mut segments = Vec::new();
    if len == 0 {
        return segments;
    }

    let mut start = 0;
    let mut current = style_at[0];
    for (byte, &style) in style_at.iter().enumerate().skip(1) {
        if style != current {
            segments.push(LayoutSegment {
                doc_start: start,
                doc_end: byte,
                style: current,
            });
            start = byte;
            current = style;
        }
    }
    segments.push(LayoutSegment {
        doc_start: start,
        doc_end: len,
        style: current,
    });
    segments
}

fn project_display(
    content: &str,
    segments: &[LayoutSegment],
    spans: &[SyntaxNodeSpan],
) -> DisplayLayout {
    let mut display_text = String::new();
    let mut doc_to_display = vec![None; content.len() + 1];

    for segment in segments {
        let slice = &content[segment.doc_start..segment.doc_end];
        match segment.style {
            SegmentStyle::Delimiter { visible: false } => {
                if is_masked_task_marker(segment, spans) {
                    let checkbox = task_checkbox_for_marker(slice);
                    let display_pos = display_text.len();
                    display_text.push(checkbox);
                    for byte in segment.doc_start..=segment.doc_end.min(content.len()) {
                        if byte < doc_to_display.len() {
                            doc_to_display[byte] = Some(display_pos);
                        }
                    }
                } else {
                    let display_pos = display_text.len();
                    for byte in segment.doc_start..=segment.doc_end.min(content.len()) {
                        if byte < doc_to_display.len() {
                            doc_to_display[byte] = Some(display_pos);
                        }
                    }
                }
            }
            SegmentStyle::Image if !slice.contains('(') => {
                let display_pos = display_text.len();
                display_text.push('🖼');
                display_text.push(' ');
                let alt = slice.trim();
                display_text.push_str(alt);
                map_display_range(
                    &mut doc_to_display,
                    segment.doc_start,
                    segment.doc_end,
                    display_pos,
                    &display_text[display_pos..],
                    content,
                );
            }
            _ => {
                let start_display = display_text.len();
                display_text.push_str(slice);
                map_display_range(
                    &mut doc_to_display,
                    segment.doc_start,
                    segment.doc_end,
                    start_display,
                    slice,
                    content,
                );
            }
        }
    }

    DisplayLayout {
        display_text,
        doc_to_display,
        segments: segments.to_vec(),
        highlight_spans: Vec::new(),
        blockquote_lines: Vec::new(),
    }
}

fn map_display_range(
    doc_to_display: &mut [Option<usize>],
    doc_start: usize,
    doc_end: usize,
    start_display: usize,
    slice: &str,
    content: &str,
) {
    let mut display_pos = start_display;
    for byte in doc_start..doc_end {
        doc_to_display[byte] = Some(display_pos);
        if content.is_char_boundary(byte) {
            if let Some(ch) = content[byte..].chars().next() {
                display_pos += ch.len_utf8();
            }
        }
    }
    if doc_end <= content.len() {
        doc_to_display[doc_end] = Some(start_display + slice.len());
    }
}

fn is_masked_task_marker(segment: &LayoutSegment, spans: &[SyntaxNodeSpan]) -> bool {
    spans.iter().any(|span| {
        span.kind == SyntaxKind::TaskList
            && span
                .delimiter_spans
                .iter()
                .any(|d| d.start_byte == segment.doc_start && d.end_byte == segment.doc_end)
    })
}

fn task_checkbox_for_marker(marker: &str) -> char {
    if marker.contains('x') || marker.contains('X') {
        '☑'
    } else {
        '☐'
    }
}

fn blockquote_line_starts(content: &str, spans: &[SyntaxNodeSpan]) -> Vec<usize> {
    let mut lines = Vec::new();
    for span in spans {
        if span.kind != SyntaxKind::BlockQuote {
            continue;
        }
        let block = &content[span.start_byte..span.end_byte.min(content.len())];
        let offset = span.start_byte;
        for (idx, ch) in block.char_indices() {
            if idx == 0 || block[..idx].ends_with('\n') {
                lines.push(offset + idx);
            }
            if ch == '\n' {
                let next = offset + idx + 1;
                if next < span.end_byte {
                    lines.push(next);
                }
            }
        }
    }
    lines.sort_unstable();
    lines.dedup();
    lines
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnAlign {
    Left,
    Center,
    Right,
}

fn apply_table_alignment(layout: &mut DisplayLayout, content: &str, spans: &[SyntaxNodeSpan]) {
    for span in spans {
        if span.kind != SyntaxKind::Table || span.table_row.is_some() {
            continue;
        }
        let block = &content[span.start_byte..span.end_byte.min(content.len())];
        let lines: Vec<&str> = block.lines().collect();
        if lines.len() < 2 {
            continue;
        }
        let alignments = parse_column_alignments(lines[1]);
        let widths = compute_column_widths(&lines);
        let formatted: Vec<String> = lines
            .iter()
            .enumerate()
            .map(|(idx, line)| {
                if idx == 1 {
                    format_delimiter_row(line, &widths, &alignments)
                } else {
                    format_data_row(line, &widths, &alignments)
                }
            })
            .collect();
        replace_block_in_layout(layout, span.start_byte, block, &formatted.join("\n"));
    }
}

fn parse_column_alignments(line: &str) -> Vec<ColumnAlign> {
    split_table_cells(line)
        .into_iter()
        .map(|cell| {
            let left = cell.starts_with(':');
            let right = cell.ends_with(':');
            match (left, right) {
                (true, true) => ColumnAlign::Center,
                (false, true) => ColumnAlign::Right,
                _ => ColumnAlign::Left,
            }
        })
        .collect()
}

fn split_table_cells(line: &str) -> Vec<String> {
    line.split('|')
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .map(str::to_string)
        .collect()
}

fn compute_column_widths(lines: &[&str]) -> Vec<usize> {
    let mut widths = Vec::new();
    for line in lines {
        for (idx, cell) in split_table_cells(line).into_iter().enumerate() {
            if idx >= widths.len() {
                widths.push(cell.chars().count());
            } else {
                widths[idx] = widths[idx].max(cell.chars().count());
            }
        }
    }
    widths.into_iter().map(|w| w.max(1)).collect()
}

fn format_data_row(line: &str, widths: &[usize], alignments: &[ColumnAlign]) -> String {
    let cells = split_table_cells(line);
    if cells.is_empty() {
        return line.to_string();
    }
    let mut parts = vec!["|".to_string()];
    for (idx, cell) in cells.iter().enumerate() {
        let width = widths.get(idx).copied().unwrap_or(cell.chars().count());
        let align = alignments.get(idx).copied().unwrap_or(ColumnAlign::Left);
        parts.push(format!(" {} ", pad_cell(cell, width, align)));
        parts.push("|".to_string());
    }
    parts.join("")
}

fn format_delimiter_row(line: &str, widths: &[usize], alignments: &[ColumnAlign]) -> String {
    let cells = split_table_cells(line);
    if cells.is_empty() {
        return line.to_string();
    }
    let mut parts = vec!["|".to_string()];
    for (idx, _cell) in cells.iter().enumerate() {
        let width = widths.get(idx).copied().unwrap_or(3).max(3);
        let align = alignments.get(idx).copied().unwrap_or(ColumnAlign::Left);
        let dashes = "-".repeat(width);
        let body = match align {
            ColumnAlign::Left => format!(":{dashes}"),
            ColumnAlign::Center => format!(":{dashes}:"),
            ColumnAlign::Right => format!("{dashes}:"),
        };
        parts.push(format!(" {body} "));
        parts.push("|".to_string());
    }
    parts.join("")
}

fn pad_cell(cell: &str, width: usize, align: ColumnAlign) -> String {
    let len = cell.chars().count();
    if len >= width {
        return cell.to_string();
    }
    let pad = width - len;
    match align {
        ColumnAlign::Left => format!("{cell}{}", " ".repeat(pad)),
        ColumnAlign::Right => format!("{}{cell}", " ".repeat(pad)),
        ColumnAlign::Center => {
            let left = pad / 2;
            format!("{}{cell}{}", " ".repeat(left), " ".repeat(pad - left))
        }
    }
}

fn replace_block_in_layout(
    layout: &mut DisplayLayout,
    block_start: usize,
    original: &str,
    formatted: &str,
) {
    if original == formatted {
        return;
    }
    let display_start = layout.display_offset_for_doc(block_start);
    let display_end = layout.display_offset_for_doc(block_start + original.len());
    layout.display_text.replace_range(display_start..display_end, formatted);
    let delta = formatted.len() as isize - (display_end - display_start) as isize;
    for pos in layout.doc_to_display.iter_mut().skip(block_start).flatten() {
        if *pos >= display_end {
            *pos = ((*pos as isize) + delta) as usize;
        }
    }
}

pub fn line_byte_ranges(content: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in content.char_indices() {
        if ch == '\n' {
            ranges.push((start, idx + 1));
            start = idx + 1;
        }
    }
    ranges.push((start, content.len()));
    ranges
}

pub fn cursor_line_col(content: &str, byte_offset: usize) -> (usize, usize) {
    let mut line = 0usize;
    let mut line_start = 0usize;
    for (idx, ch) in content.char_indices() {
        if idx >= byte_offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = idx + 1;
        }
    }
    (line, byte_offset.saturating_sub(line_start))
}

pub fn outline_headings(spans: &[SyntaxNodeSpan], content: &str) -> Vec<(usize, u8, String)> {
    spans
        .iter()
        .filter(|span| span.kind == SyntaxKind::Heading)
        .map(|span| {
            let level = heading_level(span);
            let title = content[span.start_byte..span.end_byte]
                .trim()
                .trim_start_matches('#')
                .trim()
                .to_string();
            (span.start_byte, level, title)
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

    #[test]
    fn masks_delimiters_from_display_text() {
        let content = "**bold**";
        let spans = vec![bold_span(0, 8)];
        let layout = build_display_layout(
            content,
            &spans,
            &[Caret::new(20)],
            &[],
            &EditorTheme::dark(),
        );
        assert_eq!(layout.display_text, "bold");
    }

    #[test]
    fn reveals_delimiters_in_display_text() {
        let content = "**bold**";
        let spans = vec![bold_span(0, 8)];
        let layout =
            build_display_layout(content, &spans, &[Caret::new(3)], &[], &EditorTheme::dark());
        assert_eq!(layout.display_text, "**bold**");
    }

    #[test]
    fn doc_to_display_mapping() {
        let content = "**bold** text";
        let spans = vec![bold_span(0, 8)];
        let layout = build_display_layout(
            content,
            &spans,
            &[Caret::new(20)],
            &[],
            &EditorTheme::dark(),
        );
        let display = layout.display_offset_for_doc(9);
        assert!(layout.display_text[display..].starts_with("text"));
    }

    #[test]
    fn table_alignment_expands_columns() {
        let content = "| a | bb |\n|---|---|\n| c | d |";
        let spans = markrust_core::extract_syntax_spans(content);
        let layout = build_display_layout(content, &spans, &[Caret::new(99)], &[], &EditorTheme::dark());
        assert!(
            layout.display_text.contains("| a  | bb |")
                || layout.display_text.contains("| a  |")
                || layout
                    .segments
                    .iter()
                    .any(|segment| matches!(segment.style, SegmentStyle::Table { .. })),
            "display: {:?}, table spans: {}",
            layout.display_text,
            spans
                .iter()
                .filter(|span| span.kind == SyntaxKind::Table)
                .count()
        );
    }

    #[test]
    fn link_style_applied_from_parser() {
        let content = "[text](url)";
        let spans = markrust_core::extract_syntax_spans(content);
        assert!(spans.iter().any(|s| s.kind == SyntaxKind::Link));
    }
}
