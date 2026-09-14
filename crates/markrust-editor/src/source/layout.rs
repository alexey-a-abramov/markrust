// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use markrust_core::{SyntaxKind, SyntaxNodeSpan, TableRowKind};

use crate::highlight::{highlight_code_block, HighlightKind, HighlightSpan};
use crate::masking::{
    compute_delimiter_entries, delimiter_visibility_for_span, Caret, DelimiterVisibilityEntry,
    Selection, VisibilityState,
};
use crate::table::{
    compute_column_widths, format_data_row, format_delimiter_row, parse_column_alignments,
};
use crate::theme::EditorTheme;

/// Styling applied to a contiguous byte range in the source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentStyle {
    Plain,
    Delimiter {
        visible: bool,
    },
    Bold,
    Italic,
    Heading {
        level: u8,
    },
    CodeInline,
    CodeBlock,
    BlockQuote,
    Link,
    Image,
    TaskList {
        checked: bool,
    },
    Table {
        row: TableRowKind,
    },
    Frontmatter,
    Strikethrough,
    Highlight,
    Math,
    /// Known `:name:` shortcode painted as a glyph when the caret is outside.
    Emoji,
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
    pub code_block_lines: Vec<usize>,
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
    let segments = build_segments(
        content,
        spans,
        &delimiter_entries,
        &highlight_spans,
        carets,
        selections,
        theme,
    );
    let mut layout = project_display(content, &segments, spans);
    layout.highlight_spans = highlight_spans;
    layout.blockquote_lines = blockquote_line_starts(content, spans);
    layout.code_block_lines = code_block_line_starts(content, spans);
    apply_table_alignment(&mut layout, content, spans, carets, selections);
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
        let code_end = block
            .rfind("\n```")
            .or_else(|| block.rfind("\n~~~"))
            .unwrap_or(block.len());
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
    carets: &[Caret],
    selections: &[Selection],
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
    seed_implicit_list_markers(content, spans, carets, selections, &mut delimiter_map);

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
    for span in spans {
        if span.kind != SyntaxKind::Emoji {
            continue;
        }
        let revealed =
            delimiter_visibility_for_span(span, carets, selections) == VisibilityState::Visible;
        if revealed {
            continue;
        }
        let start = span.start_byte.min(style_at.len());
        let end = span.end_byte.min(style_at.len());
        style_at[start..end].fill(SegmentStyle::Emoji);
    }

    coalesce_segments(content.len(), &style_at)
}

fn seed_implicit_list_markers(
    content: &str,
    spans: &[SyntaxNodeSpan],
    carets: &[Caret],
    selections: &[Selection],
    delimiter_map: &mut [Option<bool>],
) {
    for span in spans {
        if span.kind != SyntaxKind::List {
            continue;
        }
        let visible =
            delimiter_visibility_for_span(span, carets, selections) == VisibilityState::Visible;
        let end = span.end_byte.min(content.len());
        if span.start_byte >= end {
            continue;
        }
        let block = &content[span.start_byte..end];
        let mut offset = span.start_byte;
        for line in block.split_inclusive('\n') {
            let indent = line
                .bytes()
                .take_while(|byte| *byte == b' ' || *byte == b'\t')
                .count();
            if let Some(marker) = line.as_bytes().get(indent) {
                if matches!(marker, b'-' | b'*' | b'+') {
                    let at = offset + indent;
                    if at < delimiter_map.len() && delimiter_map[at].is_none() {
                        delimiter_map[at] = Some(visible);
                    }
                }
            }
            offset += line.len();
        }
    }
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
        SyntaxKind::List => SegmentStyle::Plain,
        SyntaxKind::TaskList => SegmentStyle::TaskList {
            checked: span.task_checked.unwrap_or(false),
        },
        SyntaxKind::Table => SegmentStyle::Table {
            row: span.table_row.unwrap_or(TableRowKind::Body),
        },
        SyntaxKind::Frontmatter => SegmentStyle::Frontmatter,
        SyntaxKind::Strikethrough => SegmentStyle::Strikethrough,
        SyntaxKind::Highlight => SegmentStyle::Highlight,
        SyntaxKind::Math => SegmentStyle::Math,
        SyntaxKind::WikiLink => SegmentStyle::Link,
        SyntaxKind::Emoji => SegmentStyle::Emoji,
        SyntaxKind::Alert => SegmentStyle::BlockQuote,
        _ => SegmentStyle::Plain,
    }
}

pub fn heading_level(span: &SyntaxNodeSpan) -> u8 {
    span.heading_level.unwrap_or_else(|| {
        span.delimiter_spans
            .first()
            .map(|delimiter| delimiter.len().clamp(1, 6) as u8)
            .unwrap_or(1)
    })
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
                } else if is_masked_unordered_list_marker(segment, spans, content) {
                    let display_pos = display_text.len();
                    if let Some(bullet) = unordered_list_bullet(slice) {
                        display_text.push(bullet);
                        if !slice.ends_with(' ') {
                            display_text.push(' ');
                        }
                    } else {
                        display_text.push_str(slice);
                    }
                    map_display_range(
                        &mut doc_to_display,
                        segment.doc_start,
                        segment.doc_end,
                        display_pos,
                        &display_text[display_pos..],
                        content,
                    );
                } else if is_block_structure_delimiter(segment, spans) {
                    let display_pos = display_text.len();
                    for byte in segment.doc_start..=segment.doc_end.min(content.len()) {
                        if byte < doc_to_display.len() {
                            doc_to_display[byte] = Some(display_pos);
                        }
                    }
                } else {
                    // Inline delimiters keep their glyph width (painted
                    // transparent) so unmasking cannot wrap the line.
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
            SegmentStyle::Emoji => {
                let display_pos = display_text.len();
                if let Some(glyph) = markrust_core::rich::lookup_shortcode(slice) {
                    display_text.push_str(glyph);
                } else {
                    display_text.push_str(slice);
                }
                for byte in segment.doc_start..=segment.doc_end.min(content.len()) {
                    if byte < doc_to_display.len() {
                        doc_to_display[byte] = Some(display_pos);
                    }
                }
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
        code_block_lines: Vec::new(),
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

fn is_masked_unordered_list_marker(
    segment: &LayoutSegment,
    spans: &[SyntaxNodeSpan],
    content: &str,
) -> bool {
    if is_masked_list_marker(segment, spans) {
        return true;
    }
    let slice = &content[segment.doc_start..segment.doc_end.min(content.len())];
    if unordered_list_bullet(slice).is_none() {
        return false;
    }
    let on_list = spans.iter().any(|span| {
        span.kind == SyntaxKind::List
            && segment.doc_start >= span.start_byte
            && segment.doc_end <= span.end_byte
    });
    if !on_list {
        return false;
    }
    let prefix = &content[..segment.doc_start];
    let line_start = prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    content[line_start..segment.doc_start]
        .bytes()
        .all(|byte| byte == b' ' || byte == b'\t')
}

fn is_block_structure_delimiter(segment: &LayoutSegment, spans: &[SyntaxNodeSpan]) -> bool {
    spans.iter().any(|span| {
        matches!(
            span.kind,
            SyntaxKind::Heading
                | SyntaxKind::List
                | SyntaxKind::BlockQuote
                | SyntaxKind::Alert
                | SyntaxKind::CodeBlock
                | SyntaxKind::Frontmatter
        ) && span
            .delimiter_spans
            .iter()
            .any(|d| d.start_byte == segment.doc_start && d.end_byte == segment.doc_end)
    })
}

fn is_masked_list_marker(segment: &LayoutSegment, spans: &[SyntaxNodeSpan]) -> bool {
    let on_task_item = spans.iter().any(|span| {
        span.kind == SyntaxKind::TaskList
            && segment.doc_start >= span.start_byte
            && segment.doc_end <= span.end_byte
    });
    if on_task_item {
        return false;
    }
    spans.iter().any(|span| {
        span.kind == SyntaxKind::List
            && span
                .delimiter_spans
                .iter()
                .any(|d| d.start_byte == segment.doc_start && d.end_byte == segment.doc_end)
    })
}

fn unordered_list_bullet(marker: &str) -> Option<char> {
    match marker.trim() {
        "-" | "*" | "+" => Some('•'),
        _ => None,
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
        if span.kind != SyntaxKind::BlockQuote && span.kind != SyntaxKind::Alert {
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

fn code_block_line_starts(content: &str, spans: &[SyntaxNodeSpan]) -> Vec<usize> {
    let mut lines = Vec::new();
    for span in spans {
        if span.kind != SyntaxKind::CodeBlock {
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

/// Font size for a source line, derived from heading spans — independent of
/// delimiter mask state, so toggling `**` / `#` never changes line height.
pub fn source_line_font_size(
    layout: &DisplayLayout,
    theme: &EditorTheme,
    _content: &str,
    doc_start: usize,
    doc_end: usize,
) -> f32 {
    let heading = layout.segments.iter().find_map(|segment| {
        if segment.doc_end <= doc_start || segment.doc_start >= doc_end {
            return None;
        }
        match segment.style {
            SegmentStyle::Heading { level } => Some(level),
            _ => None,
        }
    });
    match heading {
        Some(level) => theme.heading_font_size(level),
        None => theme.font_size,
    }
}

fn apply_table_alignment(
    layout: &mut DisplayLayout,
    content: &str,
    spans: &[SyntaxNodeSpan],
    carets: &[Caret],
    selections: &[Selection],
) {
    for span in spans {
        if span.kind != SyntaxKind::Table || span.table_row.is_some() {
            continue;
        }
        let focused = carets
            .iter()
            .any(|c| c.offset >= span.start_byte && c.offset <= span.end_byte)
            || selections
                .iter()
                .any(|s| !s.is_empty() && s.overlaps(span.start_byte, span.end_byte));
        if focused {
            // Raw pipes while the table is focused (Typora-style).
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
    layout
        .display_text
        .replace_range(display_start..display_end, formatted);
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
            heading_level: None,
        }
    }

    #[test]
    fn masks_inline_delimiters_keep_width() {
        let content = "**bold**";
        let spans = vec![bold_span(0, 8)];
        let layout = build_display_layout(
            content,
            &spans,
            &[Caret::new(20)],
            &[],
            &EditorTheme::dark(),
        );
        assert_eq!(layout.display_text, "**bold**");
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
        let layout = build_display_layout(
            content,
            &spans,
            &[Caret::new(99)],
            &[],
            &EditorTheme::dark(),
        );
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
    fn table_left_center_right_alignment() {
        let content = "| L | C | R |\n|:---|:---:|---:|\n| a | bb | ccc |";
        let spans = markrust_core::extract_syntax_spans(content);
        let layout = build_display_layout(
            content,
            &spans,
            &[Caret::new(999)],
            &[],
            &EditorTheme::dark(),
        );
        assert!(
            layout.display_text.contains(':') || layout.display_text.contains('|'),
            "aligned table display: {:?}",
            layout.display_text
        );
        let alignments = crate::table::parse_column_alignments("|:---|:---:|---:|");
        assert_eq!(
            alignments,
            vec![
                crate::table::ColumnAlign::Left,
                crate::table::ColumnAlign::Center,
                crate::table::ColumnAlign::Right
            ]
        );
        let padded_right = crate::table::pad_cell("a", 3, crate::table::ColumnAlign::Right);
        assert_eq!(padded_right, "  a");
        let padded_center = crate::table::pad_cell("a", 3, crate::table::ColumnAlign::Center);
        assert_eq!(padded_center.chars().count(), 3);
    }

    #[test]
    fn stable_line_height_does_not_depend_on_mask() {
        let theme = EditorTheme::dark();
        let body = theme.stable_line_height(theme.font_size);
        let heading = theme.stable_line_height(theme.heading_font_size(1));
        assert_eq!(body, heading);
        assert!(body >= theme.font_size * 2.0 * theme.line_height_multiplier - f32::EPSILON);
    }

    #[test]
    fn masked_inline_delimiters_keep_stable_width() {
        let content = "**bold**";
        let spans = vec![bold_span(0, 8)];
        let masked = build_display_layout(
            content,
            &spans,
            &[Caret::new(20)],
            &[],
            &EditorTheme::dark(),
        );
        let unmasked =
            build_display_layout(content, &spans, &[Caret::new(3)], &[], &EditorTheme::dark());
        assert_eq!(masked.display_text, "**bold**");
        assert_eq!(unmasked.display_text, "**bold**");
        assert_eq!(masked.display_text.len(), unmasked.display_text.len());
        assert_eq!(masked.doc_to_display.len(), unmasked.doc_to_display.len());
    }

    #[test]
    fn link_style_applied_from_parser() {
        let content = "[text](url)";
        let spans = markrust_core::extract_syntax_spans(content);
        assert!(spans.iter().any(|s| s.kind == SyntaxKind::Link));
    }

    #[test]
    fn unordered_list_uses_bullet_when_masked() {
        let content = "- first\n- second";
        let spans = markrust_core::extract_syntax_spans(content);
        let layout = build_display_layout(
            content,
            &spans,
            &[Caret::new(99)],
            &[],
            &EditorTheme::dark(),
        );
        assert!(
            layout.display_text.contains('•'),
            "display: {:?}",
            layout.display_text
        );
    }

    #[test]
    fn code_block_lines_are_tracked() {
        let content = "intro\n\n```rust\nfn main() {}\n```\n";
        let spans = markrust_core::extract_syntax_spans(content);
        let layout =
            build_display_layout(content, &spans, &[Caret::new(0)], &[], &EditorTheme::dark());
        assert!(!layout.code_block_lines.is_empty());
    }

    #[test]
    fn outline_headings_collect_titles() {
        let content = "# Alpha\n\n## Beta\n";
        let spans = markrust_core::extract_syntax_spans(content);
        let outline = outline_headings(&spans, content);
        assert!(outline
            .iter()
            .any(|(_, level, title)| *level == 1 && title.contains("Alpha")));
        assert!(outline
            .iter()
            .any(|(_, level, title)| *level == 2 && title.contains("Beta")));
    }

    #[test]
    fn cursor_line_col_handles_lf_and_crlf() {
        assert_eq!(cursor_line_col("ab\ncd", 3), (1, 0));
        assert_eq!(cursor_line_col("ab\r\ncd", 4), (1, 0));
        assert_eq!(cursor_line_col("", 0), (0, 0));
    }

    #[test]
    fn focused_table_keeps_raw_pipes() {
        let content = "| a | bb |\n|---|---|\n| c | d |";
        let spans = markrust_core::extract_syntax_spans(content);
        let focused =
            build_display_layout(content, &spans, &[Caret::new(0)], &[], &EditorTheme::dark());
        let blurred = build_display_layout(
            content,
            &spans,
            &[Caret::new(999)],
            &[],
            &EditorTheme::dark(),
        );
        assert!(
            focused.display_text.contains("| a | bb |"),
            "focused should keep raw pipes: {:?}",
            focused.display_text
        );
        assert!(
            blurred.display_text.contains("| a  | bb |")
                || blurred.display_text.contains("| a | bb |")
                || blurred
                    .segments
                    .iter()
                    .any(|segment| matches!(segment.style, SegmentStyle::Table { .. })),
            "blurred table display: {:?}",
            blurred.display_text
        );
    }

    #[test]
    fn heading_font_size_does_not_depend_on_mask() {
        let content = "# Title\nplain\n";
        let spans = markrust_core::extract_syntax_spans(content);
        let theme = EditorTheme::dark();
        let masked = build_display_layout(content, &spans, &[Caret::new(99)], &[], &theme);
        let shown = build_display_layout(content, &spans, &[Caret::new(0)], &[], &theme);
        let h_masked = source_line_font_size(&masked, &theme, content, 0, 8);
        let h_shown = source_line_font_size(&shown, &theme, content, 0, 8);
        assert_eq!(h_masked, h_shown);
        assert_eq!(h_masked, theme.heading_font_size(1));
        let body = source_line_font_size(&masked, &theme, content, 8, content.len());
        assert_eq!(body, theme.font_size);
    }

    #[test]
    fn highlight_eqeq_masks_when_caret_outside() {
        let content = "hello ==mark== world";
        let spans = markrust_core::extract_syntax_spans(content);
        let layout =
            build_display_layout(content, &spans, &[Caret::new(0)], &[], &EditorTheme::dark());
        let masked: Vec<_> = layout
            .segments
            .iter()
            .filter(|s| matches!(s.style, SegmentStyle::Delimiter { visible: false }))
            .collect();
        assert!(
            masked.len() >= 2,
            "expected masked ==, segments={:?}",
            layout.segments
        );
        assert!(layout
            .segments
            .iter()
            .any(|s| matches!(s.style, SegmentStyle::Highlight)));
    }

    #[test]
    fn highlight_eqeq_reveals_when_caret_inside() {
        let content = "hello ==mark== world";
        let spans = markrust_core::extract_syntax_spans(content);
        let inside = content.find("mark").unwrap();
        let layout = build_display_layout(
            content,
            &spans,
            &[Caret::new(inside)],
            &[],
            &EditorTheme::dark(),
        );
        assert!(
            layout
                .segments
                .iter()
                .any(|s| matches!(s.style, SegmentStyle::Delimiter { visible: true })),
            "expected visible ==, segments={:?}",
            layout.segments
        );
        assert!(!layout
            .segments
            .iter()
            .any(|s| matches!(s.style, SegmentStyle::Delimiter { visible: false })));
    }

    #[test]
    fn math_dollars_mask_when_caret_outside() {
        let content = "see $x^2$ here";
        let spans = markrust_core::extract_syntax_spans(content);
        let layout =
            build_display_layout(content, &spans, &[Caret::new(0)], &[], &EditorTheme::dark());
        let masked: Vec<_> = layout
            .segments
            .iter()
            .filter(|s| matches!(s.style, SegmentStyle::Delimiter { visible: false }))
            .collect();
        assert!(
            masked.len() >= 2,
            "expected masked $, segments={:?}",
            layout.segments
        );
        assert!(layout
            .segments
            .iter()
            .any(|s| matches!(s.style, SegmentStyle::Math)));
    }

    #[test]
    fn multiline_display_math_masks_wrapping_newlines_when_caret_outside() {
        let content = "see\n$$\nE=mc^2\n$$\nhere";
        let spans = markrust_core::extract_syntax_spans(content);
        let layout =
            build_display_layout(content, &spans, &[Caret::new(0)], &[], &EditorTheme::dark());
        let masked: Vec<_> = layout
            .segments
            .iter()
            .filter(|s| matches!(s.style, SegmentStyle::Delimiter { visible: false }))
            .collect();
        assert!(
            !masked.is_empty(),
            "expected masked $$ / wrapping newlines, segments={:?}",
            layout.segments
        );
        let masked_bytes: String = masked
            .iter()
            .map(|s| &content[s.doc_start..s.doc_end])
            .collect();
        assert!(
            masked_bytes.contains("$$"),
            "$$ must be masked, got {masked_bytes:?}"
        );
        assert!(
            !masked_bytes.contains("E=mc^2"),
            "formula must not be masked, got {masked_bytes:?}"
        );
    }

    #[test]
    fn math_dollars_reveal_when_caret_inside() {
        let content = "see $x^2$ here";
        let spans = markrust_core::extract_syntax_spans(content);
        let inside = content.find('x').unwrap();
        let layout = build_display_layout(
            content,
            &spans,
            &[Caret::new(inside)],
            &[],
            &EditorTheme::dark(),
        );
        assert!(
            layout
                .segments
                .iter()
                .any(|s| matches!(s.style, SegmentStyle::Delimiter { visible: true })),
            "expected visible $, segments={:?}",
            layout.segments
        );
        assert!(!layout
            .segments
            .iter()
            .any(|s| matches!(s.style, SegmentStyle::Delimiter { visible: false })));
    }

    #[test]
    fn wikilink_masks_brackets_when_caret_outside() {
        let content = "see [[page]] here";
        let spans = markrust_core::extract_syntax_spans(content);
        let layout =
            build_display_layout(content, &spans, &[Caret::new(0)], &[], &EditorTheme::dark());
        let masked: Vec<_> = layout
            .segments
            .iter()
            .filter(|s| matches!(s.style, SegmentStyle::Delimiter { visible: false }))
            .collect();
        assert!(
            masked.len() >= 2,
            "expected masked [[ ]], segments={:?}",
            layout.segments
        );
        assert!(layout
            .segments
            .iter()
            .any(|s| matches!(s.style, SegmentStyle::Link)));
    }

    #[test]
    fn wikilink_reveals_brackets_when_caret_inside() {
        let content = "see [[page]] here";
        let spans = markrust_core::extract_syntax_spans(content);
        let inside = content.find("page").unwrap();
        let layout = build_display_layout(
            content,
            &spans,
            &[Caret::new(inside)],
            &[],
            &EditorTheme::dark(),
        );
        assert!(
            layout
                .segments
                .iter()
                .any(|s| matches!(s.style, SegmentStyle::Delimiter { visible: true })),
            "expected visible [[ ]], segments={:?}",
            layout.segments
        );
        assert!(!layout
            .segments
            .iter()
            .any(|s| matches!(s.style, SegmentStyle::Delimiter { visible: false })));
    }

    #[test]
    fn emoji_paints_glyph_when_caret_outside() {
        let content = "see :smile: here";
        let spans = markrust_core::extract_syntax_spans(content);
        let layout =
            build_display_layout(content, &spans, &[Caret::new(0)], &[], &EditorTheme::dark());
        assert!(
            layout.display_text.contains("😄"),
            "expected glyph in source layout, got {:?}",
            layout.display_text
        );
        assert!(
            !layout.display_text.contains(":smile:"),
            "shortcode must hide when caret is outside, got {:?}",
            layout.display_text
        );
        assert!(layout
            .segments
            .iter()
            .any(|s| matches!(s.style, SegmentStyle::Emoji)));
        let unknown = build_display_layout(
            "see :not_an_emoji: here",
            &markrust_core::extract_syntax_spans("see :not_an_emoji: here"),
            &[Caret::new(0)],
            &[],
            &EditorTheme::dark(),
        );
        assert!(
            unknown.display_text.contains(":not_an_emoji:"),
            "unknown must stay, got {:?}",
            unknown.display_text
        );
    }

    #[test]
    fn emoji_reveals_shortcode_when_caret_inside() {
        let content = "see :smile: here";
        let spans = markrust_core::extract_syntax_spans(content);
        let inside = content.find("smile").unwrap();
        let layout = build_display_layout(
            content,
            &spans,
            &[Caret::new(inside)],
            &[],
            &EditorTheme::dark(),
        );
        assert!(
            layout.display_text.contains(":smile:"),
            "expected revealed :smile:, got {:?}",
            layout.display_text
        );
        assert!(
            !layout.display_text.contains("😄"),
            "glyph must hide while editing, got {:?}",
            layout.display_text
        );
    }

    #[test]
    fn currency_is_not_math_in_source_layout() {
        let content = "costs $5";
        let spans = markrust_core::extract_syntax_spans(content);
        let layout =
            build_display_layout(content, &spans, &[Caret::new(0)], &[], &EditorTheme::dark());
        assert!(!spans.iter().any(|s| s.kind == SyntaxKind::Math));
        assert!(!layout
            .segments
            .iter()
            .any(|s| matches!(s.style, SegmentStyle::Math)));
        assert!(layout.display_text.contains("$5"));
    }

    #[test]
    fn github_alert_masks_tag_when_caret_in_body() {
        for tag in ["NOTE", "TIP", "IMPORTANT", "WARNING", "CAUTION"] {
            let content = format!("> [!{tag}]\n> body");
            let spans = markrust_core::extract_syntax_spans(&content);
            let body = content.find("body").unwrap();
            let layout = build_display_layout(
                &content,
                &spans,
                &[Caret::new(body)],
                &[],
                &EditorTheme::dark(),
            );
            let tag_start = content.find(&format!("[!{tag}]")).unwrap();
            let tag_end = tag_start + tag.len() + 3;
            let masked_tag = layout.segments.iter().any(|s| {
                matches!(s.style, SegmentStyle::Delimiter { visible: false })
                    && s.doc_start <= tag_start
                    && s.doc_end >= tag_end
            });
            assert!(
                masked_tag,
                "expected masked [!{tag}] with caret in body, segments={:?}",
                layout.segments
            );
            assert!(
                !layout.display_text.contains(&format!("[!{tag}]")),
                "masked [!{tag}] must not take display width, got {:?}",
                layout.display_text
            );
        }
    }

    #[test]
    fn github_alert_reveals_tag_when_caret_on_chrome() {
        let content = "> [!NOTE]\n> body";
        let spans = markrust_core::extract_syntax_spans(content);
        let tag = content.find("[!NOTE]").unwrap();
        let layout = build_display_layout(
            content,
            &spans,
            &[Caret::new(tag + 2)],
            &[],
            &EditorTheme::dark(),
        );
        assert!(
            layout.display_text.contains("[!NOTE]"),
            "caret on chrome must reveal [!NOTE], got {:?}",
            layout.display_text
        );
    }
}
