//! MarkRust core: rope-backed document buffer, undo/redo, Markdown parsing, syntax spans.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub mod buffer;
pub mod document;
pub mod export;
pub mod frontmatter;
pub mod line_index;
pub mod mode;
pub mod parser;
pub mod spans;
pub mod undo;

pub use buffer::DocumentBuffer;
pub use document::Document;
pub use export::{
    export_content_to_html, export_file_to_html, markdown_to_html_gfm, write_markdown_to_html_file,
};
pub use frontmatter::{parse_frontmatter, FrontmatterInfo};
pub use line_index::LineIndex;
pub use mode::DocumentProcessingMode;
pub use parser::{extract_syntax_spans, BackgroundMarkdownParser, ParseSnapshot, ParseUpdate};
pub use spans::{DelimiterSpan, SyntaxKind, SyntaxNodeSpan, TableRowKind};
pub use undo::{EditOperation, UndoStack};
