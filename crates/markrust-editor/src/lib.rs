// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Delimiter masking and GPUI editor surface for MarkRust.

pub mod editor;
pub mod element;
pub mod highlight;
pub mod layout;
pub mod masking;
pub mod theme;

pub use editor::MarkdownEditor;
pub use element::{EditorElement, MarkdownEditorView};
pub use highlight::{highlight_code_block, HighlightKind, HighlightSpan};
pub use layout::{
    build_display_layout, cursor_line_col, outline_headings, DisplayLayout, LayoutSegment,
    SegmentStyle,
};
pub use masking::{
    compute_delimiter_entries, compute_visibility, delimiter_visibility_for_span, ByteRange, Caret,
    DelimiterVisibilityEntry, Selection, VisibilityState,
};
pub use theme::EditorTheme;
