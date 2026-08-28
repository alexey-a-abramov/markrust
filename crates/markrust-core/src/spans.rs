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
}

impl SyntaxNodeSpan {
    pub fn contains_offset(&self, offset: usize) -> bool {
        offset >= self.start_byte && offset <= self.end_byte
    }

    pub fn overlaps_range(&self, start: usize, end: usize) -> bool {
        start < self.end_byte && end > self.start_byte
    }
}
