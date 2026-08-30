// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// Semantic Markdown construct represented in the syntax span map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyntaxKind {
    Heading,
    Bold,
    Italic,
    CodeInline,
    CodeBlock,
    Link,
    Image,
    BlockQuote,
    List,
    TaskList,
    Table,
    Strikethrough,
    Frontmatter,
    /// Typora `==highlight==` (not a comrak node; same pairing as rich import).
    Highlight,
    Superscript,
    Subscript,
    /// `$…$` / `$$…$$` (comrak `math_dollars`).
    Math,
    /// `[[target]]` / `[[target|label]]` (comrak wikilinks).
    WikiLink,
    /// GitHub/Typora `:smile:` (matched names only; unknown `:foo:` is text).
    Emoji,
    /// GitHub `> [!NOTE]` / TIP / IMPORTANT / WARNING / CAUTION.
    Alert,
    Other,
}

/// Row role inside a GFM pipe table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TableRowKind {
    Header,
    Delimiter,
    Body,
}

/// Byte range of a delimiter token (`**`, `` ` ``, `#`, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelimiterSpan {
    pub start_byte: usize,
    pub end_byte: usize,
}

impl DelimiterSpan {
    pub fn new(start_byte: usize, end_byte: usize) -> Self {
        Self {
            start_byte,
            end_byte,
        }
    }

    pub fn len(&self) -> usize {
        self.end_byte.saturating_sub(self.start_byte)
    }

    pub fn is_empty(&self) -> bool {
        self.start_byte >= self.end_byte
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_span(kind: SyntaxKind, start: usize, end: usize) -> SyntaxNodeSpan {
        SyntaxNodeSpan {
            kind,
            start_byte: start,
            end_byte: end,
            delimiter_spans: Vec::new(),
            language: None,
            task_checked: None,
            table_row: None,
            heading_level: None,
        }
    }

    #[test]
    fn delimiter_span_len_and_empty() {
        let span = DelimiterSpan::new(2, 4);
        assert_eq!(span.len(), 2);
        assert!(!span.is_empty());
        assert!(DelimiterSpan::new(4, 4).is_empty());
        assert!(DelimiterSpan::new(5, 1).is_empty());
        assert_eq!(DelimiterSpan::new(5, 1).len(), 0);
    }

    #[test]
    fn contains_offset_is_inclusive() {
        let span = sample_span(SyntaxKind::Bold, 10, 20);
        assert!(span.contains_offset(10));
        assert!(span.contains_offset(15));
        assert!(span.contains_offset(20));
        assert!(!span.contains_offset(9));
        assert!(!span.contains_offset(21));
    }

    #[test]
    fn overlaps_range_is_half_open() {
        let span = sample_span(SyntaxKind::Heading, 10, 20);
        assert!(span.overlaps_range(0, 11));
        assert!(span.overlaps_range(19, 25));
        assert!(span.overlaps_range(10, 20));
        assert!(!span.overlaps_range(0, 10));
        assert!(!span.overlaps_range(20, 30));
        assert!(!span.overlaps_range(20, 20));
    }
}

/// A syntax node with content and delimiter byte ranges for WYSIWYG masking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxNodeSpan {
    pub kind: SyntaxKind,
    pub start_byte: usize,
    pub end_byte: usize,
    pub delimiter_spans: Vec<DelimiterSpan>,
    /// Fenced code block language tag (e.g. `rust`, `json`).
    pub language: Option<String>,
    /// Task list checkbox state when `kind == TaskList`.
    pub task_checked: Option<bool>,
    /// Table row role when `kind == Table`.
    pub table_row: Option<TableRowKind>,
    /// ATX/setext heading level when `kind == Heading`.
    pub heading_level: Option<u8>,
}

impl SyntaxNodeSpan {
    pub fn contains_offset(&self, offset: usize) -> bool {
        offset >= self.start_byte && offset <= self.end_byte
    }

    pub fn overlaps_range(&self, start: usize, end: usize) -> bool {
        start < self.end_byte && end > self.start_byte
    }
}
