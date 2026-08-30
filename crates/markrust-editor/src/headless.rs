// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::ops::Range;

use markrust_core::Document;
use unicode_segmentation::UnicodeSegmentation;

use crate::layout::{build_display_layout, cursor_line_col, outline_headings, DisplayLayout};
use crate::masking::{compute_visibility, Caret, Selection, VisibilityState};
use crate::theme::EditorTheme;

/// Caret / selection movement relative to the current cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaretMove {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    /// Move by a signed number of visual lines (page up/down, etc.).
    Vertical {
        delta_lines: i32,
    },
}

/// Headless editor operations. GPUI maps keys/IME onto this enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorCommand {
    InsertText(String),
    Backspace,
    Delete,
    Move(CaretMove),
    Select(CaretMove),
    SelectAll,
    SetSelection { start: usize, end: usize },
    Undo,
    Redo,
    JumpTo(usize),
}

/// Outcome of applying an [`EditorCommand`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorOutcome {
    Changed,
    CaretMoved,
    Noop,
}

/// Recoverable editor command failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorError {
    InvalidRange,
}

/// Caret and selection independent of GPUI view state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorState {
    pub selected_range: Range<usize>,
    pub selection_reversed: bool,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            selected_range: 0..0,
            selection_reversed: false,
        }
    }
}

impl EditorState {
    pub fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    pub fn carets(&self) -> Vec<Caret> {
        vec![Caret::new(self.cursor_offset())]
    }

    pub fn selections(&self) -> Vec<Selection> {
        if self.selected_range.is_empty() {
            Vec::new()
        } else {
            vec![Selection::new(
                self.selected_range.start,
                self.selected_range.end,
            )]
        }
    }

    fn clamp_to(&mut self, len: usize) {
        let start = self.selected_range.start.min(len);
        let end = self.selected_range.end.min(len);
        if start <= end {
            self.selected_range = start..end;
        } else {
            self.selected_range = end..start;
            self.selection_reversed = !self.selection_reversed;
        }
    }
}

/// [`Document`] plus caret/selection, with no GPUI `Window` / `Entity` types.
pub struct HeadlessEditor {
    document: Document,
    state: EditorState,
}

impl HeadlessEditor {
    pub fn new(content: &str) -> Self {
        Self {
            document: Document::new(content),
            state: EditorState::default(),
        }
    }

    pub fn from_document(document: Document) -> Self {
        Self {
            document,
            state: EditorState::default(),
        }
    }

    pub fn plain_text(content: &str) -> Self {
        Self {
            document: Document::plain_text(content),
            state: EditorState::default(),
        }
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    pub fn document_mut(&mut self) -> &mut Document {
        &mut self.document
    }

    pub fn state(&self) -> EditorState {
        self.state.clone()
    }

    pub fn content(&self) -> String {
        self.document.buffer.content()
    }

    /// Replace the whole buffer from a UI widget (egui TextEdit). Groups as one undo step.
    pub fn set_content_from_ui(&mut self, text: &str) {
        if self.content() == text {
            return;
        }
        let len = self.document.buffer.len_bytes();
        self.document.replace_range(0, len, text);
        self.state.clamp_to(self.document.buffer.len_bytes());
        self.sync_parse();
    }

    pub fn cursor_offset(&self) -> usize {
        self.state.cursor_offset()
    }

    pub fn carets(&self) -> Vec<Caret> {
        self.state.carets()
    }

    pub fn selections(&self) -> Vec<Selection> {
        self.state.selections()
    }

    pub fn word_count(&self) -> usize {
        self.document.word_count()
    }

    pub fn visibility(&mut self) -> Vec<VisibilityState> {
        self.sync_parse();
        compute_visibility(
            &self.carets(),
            &self.selections(),
            &self.document.syntax_spans,
        )
    }

    pub fn layout(&mut self) -> DisplayLayout {
        self.sync_parse();
        let content = self.content();
        build_display_layout(
            &content,
            &self.document.syntax_spans,
            &self.carets(),
            &self.selections(),
            &EditorTheme::dark(),
        )
    }

    pub fn outline(&mut self) -> Vec<(usize, u8, String)> {
        self.sync_parse();
        outline_headings(&self.document.syntax_spans, &self.content())
    }

    fn sync_parse(&mut self) {
        self.document.apply_pending_parse();
        if self.document.mode.parses_markdown() {
            let content = self.document.buffer.content();
            self.document.syntax_spans = markrust_core::extract_syntax_spans(&content);
            self.document.parsed_revision = self.document.revision();
        }
    }

    pub fn apply(&mut self, command: EditorCommand) -> Result<EditorOutcome, EditorError> {
        apply_editor_command(&mut self.document, &mut self.state, command)
    }
}

/// Apply an editor command to a document and caret/selection. Shared by the
/// headless session and the GPUI `MarkdownEditor` adapter.
pub fn apply_editor_command(
    document: &mut Document,
    state: &mut EditorState,
    command: EditorCommand,
) -> Result<EditorOutcome, EditorError> {
    match command {
        EditorCommand::InsertText(text) => {
            replace_selection(document, state, &text);
            Ok(EditorOutcome::Changed)
        }
        EditorCommand::Backspace => Ok(backspace(document, state)),
        EditorCommand::Delete => Ok(delete_forward(document, state)),
        EditorCommand::Move(movement) => {
            let content = document.buffer.content();
            move_caret(&content, document, state, movement, false);
            Ok(EditorOutcome::CaretMoved)
        }
        EditorCommand::Select(movement) => {
            let content = document.buffer.content();
            move_caret(&content, document, state, movement, true);
            Ok(EditorOutcome::CaretMoved)
        }
        EditorCommand::SelectAll => {
            let len = document.buffer.len_bytes();
            state.selected_range = 0..len;
            state.selection_reversed = false;
            Ok(EditorOutcome::CaretMoved)
        }
        EditorCommand::SetSelection { start, end } => {
            let len = document.buffer.len_bytes();
            if start > len || end > len {
                return Err(EditorError::InvalidRange);
            }
            let selection = Selection::new(start, end);
            state.selected_range = selection.start..selection.end;
            state.selection_reversed = start > end;
            Ok(EditorOutcome::CaretMoved)
        }
        EditorCommand::Undo => {
            document.apply_pending_parse();
            if document.undo() {
                document.apply_pending_parse();
                collapse_and_clamp(state, document.buffer.len_bytes());
                Ok(EditorOutcome::Changed)
            } else {
                Ok(EditorOutcome::Noop)
            }
        }
        EditorCommand::Redo => {
            document.apply_pending_parse();
            if document.redo() {
                document.apply_pending_parse();
                collapse_and_clamp(state, document.buffer.len_bytes());
                Ok(EditorOutcome::Changed)
            } else {
                Ok(EditorOutcome::Noop)
            }
        }
        EditorCommand::JumpTo(offset) => {
            let len = document.buffer.len_bytes();
            let offset = offset.min(len);
            state.selected_range = offset..offset;
            state.selection_reversed = false;
            Ok(EditorOutcome::CaretMoved)
        }
    }
}

fn collapse_and_clamp(state: &mut EditorState, len: usize) {
    let cursor = state.cursor_offset().min(len);
    state.selected_range = cursor..cursor;
    state.selection_reversed = false;
}

fn replace_selection(document: &mut Document, state: &mut EditorState, text: &str) {
    document.apply_pending_parse();
    let range = state.selected_range.clone();
    document.replace_range(range.start, range.end, text);
    document.apply_pending_parse();
    let new_cursor = range.start + text.len();
    state.selected_range = new_cursor..new_cursor;
    state.selection_reversed = false;
}

fn backspace(document: &mut Document, state: &mut EditorState) -> EditorOutcome {
    let content = document.buffer.content();
    if state.selected_range.is_empty() {
        let prev = previous_boundary(&content, state.cursor_offset());
        if prev == state.cursor_offset() {
            return EditorOutcome::Noop;
        }
        state.selected_range = prev..state.cursor_offset();
        state.selection_reversed = true;
    }
    replace_selection(document, state, "");
    EditorOutcome::Changed
}

fn delete_forward(document: &mut Document, state: &mut EditorState) -> EditorOutcome {
    let content = document.buffer.content();
    if state.selected_range.is_empty() {
        let next = next_boundary(&content, state.cursor_offset());
        if next == state.cursor_offset() {
            return EditorOutcome::Noop;
        }
        state.selected_range = state.cursor_offset()..next;
        state.selection_reversed = false;
    }
    replace_selection(document, state, "");
    EditorOutcome::Changed
}

fn move_caret(
    content: &str,
    document: &Document,
    state: &mut EditorState,
    movement: CaretMove,
    selecting: bool,
) {
    let target = match movement {
        CaretMove::Left => {
            if !selecting && !state.selected_range.is_empty() {
                state.selected_range.start
            } else {
                previous_boundary(content, state.cursor_offset())
            }
        }
        CaretMove::Right => {
            if !selecting && !state.selected_range.is_empty() {
                state.selected_range.end
            } else {
                next_boundary(content, state.cursor_offset())
            }
        }
        CaretMove::Home => line_start(content, state.cursor_offset()),
        CaretMove::End => line_end(content, state.cursor_offset()),
        CaretMove::Up => vertical_offset(content, document, state.cursor_offset(), -1),
        CaretMove::Down => vertical_offset(content, document, state.cursor_offset(), 1),
        CaretMove::Vertical { delta_lines } => {
            vertical_offset(content, document, state.cursor_offset(), delta_lines)
        }
    };

    if selecting {
        select_to(state, content.len(), target);
    } else {
        let offset = target.min(content.len());
        state.selected_range = offset..offset;
        state.selection_reversed = false;
    }
    state.clamp_to(content.len());
}

fn select_to(state: &mut EditorState, len: usize, offset: usize) {
    let offset = offset.min(len);
    if state.selection_reversed {
        state.selected_range.start = offset;
    } else {
        state.selected_range.end = offset;
    }
    if state.selected_range.end < state.selected_range.start {
        state.selection_reversed = !state.selection_reversed;
        state.selected_range = state.selected_range.end..state.selected_range.start;
    }
}

pub fn previous_boundary(content: &str, offset: usize) -> usize {
    content
        .grapheme_indices(true)
        .rev()
        .find_map(|(idx, _)| (idx < offset).then_some(idx))
        .unwrap_or(0)
}

pub fn next_boundary(content: &str, offset: usize) -> usize {
    content
        .grapheme_indices(true)
        .find_map(|(idx, _)| (idx > offset).then_some(idx))
        .unwrap_or(content.len())
}

fn line_start(content: &str, offset: usize) -> usize {
    content[..offset.min(content.len())]
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0)
}

fn line_end(content: &str, offset: usize) -> usize {
    let offset = offset.min(content.len());
    content[offset..]
        .find('\n')
        .map(|idx| offset + idx)
        .unwrap_or(content.len())
}

fn vertical_offset(content: &str, document: &Document, offset: usize, delta: i32) -> usize {
    let (line, col) = cursor_line_col(content, offset);
    let target_line = if delta < 0 {
        line.saturating_sub((-delta) as usize)
    } else {
        line.saturating_add(delta as usize)
    };
    document
        .buffer
        .line_index()
        .offset_of_line_col(target_line, col, document.buffer.text())
}

#[cfg(test)]
mod tests {
    use super::*;
    use markrust_core::DocumentProcessingMode;

    #[test]
    fn insert_delete_undo_and_caret() {
        let mut editor = HeadlessEditor::new("");
        editor
            .apply(EditorCommand::InsertText("hello".into()))
            .unwrap();
        assert_eq!(editor.content(), "hello");
        assert_eq!(editor.cursor_offset(), 5);
        editor.apply(EditorCommand::Backspace).unwrap();
        assert_eq!(editor.content(), "hell");
        editor.apply(EditorCommand::Undo).unwrap();
        assert_eq!(editor.content(), "hello");
        editor.apply(EditorCommand::Redo).unwrap();
        assert_eq!(editor.content(), "hell");
        editor.apply(EditorCommand::Move(CaretMove::Home)).unwrap();
        editor.apply(EditorCommand::Delete).unwrap();
        assert_eq!(editor.content(), "ell");
    }

    #[test]
    fn selection_replace_and_select_all() {
        let mut editor = HeadlessEditor::new("abcdef");
        editor
            .apply(EditorCommand::SetSelection { start: 1, end: 4 })
            .unwrap();
        editor
            .apply(EditorCommand::InsertText("XY".into()))
            .unwrap();
        assert_eq!(editor.content(), "aXYef");
        editor.apply(EditorCommand::SelectAll).unwrap();
        assert_eq!(editor.state.selected_range, 0..5);
        editor.apply(EditorCommand::Backspace).unwrap();
        assert_eq!(editor.content(), "");
    }

    #[test]
    fn caret_movement_and_word_count() {
        let mut editor = HeadlessEditor::new("one\ntwo");
        assert_eq!(editor.word_count(), 2);
        editor.apply(EditorCommand::Move(CaretMove::End)).unwrap();
        assert_eq!(editor.cursor_offset(), 3);
        editor.apply(EditorCommand::Move(CaretMove::Down)).unwrap();
        let (line, _) = cursor_line_col(&editor.content(), editor.cursor_offset());
        assert_eq!(line, 1);
        editor.apply(EditorCommand::JumpTo(0)).unwrap();
        assert_eq!(editor.cursor_offset(), 0);
        editor
            .apply(EditorCommand::Select(CaretMove::Right))
            .unwrap();
        assert_eq!(editor.selections().len(), 1);
    }

    #[test]
    fn grapheme_backspace_and_empty_noop() {
        let mut editor = HeadlessEditor::new("");
        editor
            .apply(EditorCommand::InsertText("a👍".into()))
            .unwrap();
        editor.apply(EditorCommand::Backspace).unwrap();
        assert_eq!(editor.content(), "a");
        editor.apply(EditorCommand::Move(CaretMove::Home)).unwrap();
        assert_eq!(
            editor.apply(EditorCommand::Backspace).unwrap(),
            EditorOutcome::Noop
        );
        assert_eq!(editor.content(), "a");
    }

    #[test]
    fn visibility_masks_when_caret_outside_bold() {
        let mut editor = HeadlessEditor::new("hello **x** world");
        let bold_start = editor.content().find("**x**").unwrap();
        editor.apply(EditorCommand::JumpTo(bold_start + 3)).unwrap();
        let inside = editor.visibility();
        assert!(inside.contains(&VisibilityState::Visible));
        editor.apply(EditorCommand::JumpTo(0)).unwrap();
        let outside = editor.visibility();
        assert!(outside.contains(&VisibilityState::Masked));
        assert!(!outside.contains(&VisibilityState::Visible));
    }

    #[test]
    fn plaintext_skips_masking_spans() {
        let mut editor = HeadlessEditor::plain_text("**not bold**");
        assert_eq!(editor.document().mode, DocumentProcessingMode::PlainText);
        assert!(editor.visibility().is_empty());
        assert_eq!(editor.word_count(), 2);
    }

    #[test]
    fn set_content_from_ui_replaces_buffer_and_clamps_caret() {
        let mut editor = HeadlessEditor::new("hello");
        editor.apply(EditorCommand::JumpTo(5)).unwrap();
        editor.set_content_from_ui("# Title\n\nbody");
        assert_eq!(editor.content(), "# Title\n\nbody");
        assert!(editor.cursor_offset() <= editor.content().len());
        editor.set_content_from_ui("# Title\n\nbody");
        assert_eq!(editor.content(), "# Title\n\nbody");
    }
}
