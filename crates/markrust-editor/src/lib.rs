// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Editor surfaces for MarkRust: the source-mode (delimiter masking) editor
//! lives in `source`; shared headless commands, highlighting, tables, and
//! theming live at the top level.

pub mod headless;
pub mod highlight;
pub mod source;
pub mod table;
pub mod theme;
pub mod wrap;
pub mod wysiwyg;

// Compatibility aliases for the pre-`source` module paths.
pub use source::{editor, element, layout, masking};

pub use headless::{
    apply_editor_command, CaretMove, EditorCommand, EditorError, EditorOutcome, EditorState,
    HeadlessEditor,
};
pub use highlight::{highlight_code_block, HighlightKind, HighlightSpan};
pub use source::editor::MarkdownEditor;
pub use source::editor::{
    Backspace, Delete, DeleteToLineEnd, DeleteToLineStart, DeleteWordLeft, DeleteWordRight,
    DocumentEnd, DocumentHome, Down, End, Enter, Escape, Home, Indent, Left, Outdent, PageDown,
    PageUp, Right, SelectAll, SelectDocumentEnd, SelectDocumentHome, SelectDown, SelectEnd,
    SelectHome, SelectLeft, SelectPageDown, SelectPageUp, SelectRight, SelectUp, SelectWordLeft,
    SelectWordRight, ToggleBold, ToggleCode, ToggleItalic, ToggleLink, Up, WordLeft, WordRight,
};
pub use source::element::{EditorElement, MarkdownEditorView};
pub use source::layout::{
    build_display_layout, cursor_line_col, outline_headings, DisplayLayout, LayoutSegment,
    SegmentStyle,
};
pub use source::masking::{
    compute_delimiter_entries, compute_visibility, delimiter_visibility_for_span, ByteRange, Caret,
    DelimiterVisibilityEntry, Selection, VisibilityState,
};
pub use table::{parse_column_alignments, ColumnAlign};
pub use theme::EditorTheme;
pub use wrap::WrapKind;
pub use wysiwyg::RichEditorView;
