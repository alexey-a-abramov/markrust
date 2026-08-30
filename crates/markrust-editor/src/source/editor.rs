// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::ops::Range;
use std::time::Duration;

use gpui::{
    actions, App, Bounds, Context, Entity, EntityInputHandler, FocusHandle, Focusable, Pixels,
    Subscription, Task, UTF16Selection, Window,
};
use markrust_core::Document;

use crate::headless::{apply_editor_command, CaretMove, EditorCommand, EditorOutcome, EditorState};
use crate::masking::{Caret, Selection};
use crate::theme::EditorTheme;
use crate::wrap::WrapKind;

actions!(
    markrust_editor,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        Up,
        Down,
        SelectUp,
        SelectDown,
        Home,
        End,
        SelectHome,
        SelectEnd,
        PageUp,
        PageDown,
        SelectAll,
        Enter,
        ToggleBold,
        ToggleItalic,
        ToggleCode,
        ToggleLink,
        Indent,
        Outdent,
        Escape
    ]
);

/// Per-line layout cache used for hit-testing and caret positioning.
#[derive(Debug, Clone, Default)]
pub struct LineLayoutCache {
    pub line_starts: Vec<usize>,
    pub display_line_starts: Vec<usize>,
    pub line_heights: Vec<f32>,
    pub line_x_at: Vec<Vec<f32>>,
    pub display_to_doc: Vec<usize>,
}

/// GPUI editor state: caret, selection, and document binding.
pub struct MarkdownEditor {
    pub document: Entity<Document>,
    pub theme: EditorTheme,
    pub focus_handle: FocusHandle,
    pub selected_range: Range<usize>,
    pub selection_reversed: bool,
    pub marked_range: Option<Range<usize>>,
    pub is_selecting: bool,
    pub cursor_visible: bool,
    pub layout_cache: LineLayoutCache,
    pub last_bounds_line_height: f32,
    last_caret_bounds: Option<Bounds<Pixels>>,
    last_ime_origin: Option<Bounds<Pixels>>,
    _blink_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl MarkdownEditor {
    pub fn new(
        document: Entity<Document>,
        theme: EditorTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let focus_sub = cx.on_focus(&focus_handle, window, |this, _window, cx| {
            this.start_blink(cx);
        });
        let blur_sub = cx.on_blur(&focus_handle, window, |this, _window, cx| {
            this.stop_blink(cx);
        });
        let doc_sub = cx.observe(&document, |_, _, cx| cx.notify());

        Self {
            document,
            theme,
            focus_handle,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            is_selecting: false,
            cursor_visible: false,
            layout_cache: LineLayoutCache::default(),
            last_bounds_line_height: 0.0,
            last_caret_bounds: None,
            last_ime_origin: None,
            _blink_task: Task::ready(()),
            _subscriptions: vec![focus_sub, blur_sub, doc_sub],
        }
    }

    pub fn content(&self, cx: &App) -> String {
        self.document.read(cx).buffer.content()
    }

    fn editor_state(&self) -> EditorState {
        EditorState {
            selected_range: self.selected_range.clone(),
            selection_reversed: self.selection_reversed,
        }
    }

    fn restore_state(&mut self, state: EditorState) {
        self.selected_range = state.selected_range;
        self.selection_reversed = state.selection_reversed;
    }

    /// Map a headless command through the GPUI document entity.
    pub fn apply_command(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> EditorOutcome {
        let mut state = self.editor_state();
        let mut outcome = EditorOutcome::Noop;
        self.document.update(cx, |doc, cx| {
            if let Ok(result) = apply_editor_command(doc, &mut state, command) {
                outcome = result;
                cx.notify();
            }
        });
        self.restore_state(state);
        if outcome != EditorOutcome::Noop {
            self.reset_blink(cx);
        }
        cx.notify();
        outcome
    }

    pub fn carets(&self) -> Vec<Caret> {
        self.editor_state().carets()
    }

    pub fn selections(&self) -> Vec<Selection> {
        self.editor_state().selections()
    }

    pub fn cursor_offset(&self) -> usize {
        self.editor_state().cursor_offset()
    }

    pub fn set_cursor(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::JumpTo(offset), cx);
    }

    /// Insert plain text at the current caret, replacing any active selection.
    pub fn insert_text(&mut self, text: &str, _window: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::InsertText(text.to_string()), cx);
    }

    pub fn jump_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::JumpTo(offset), cx);
    }

    fn start_blink(&mut self, cx: &mut Context<Self>) {
        self.cursor_visible = true;
        self._blink_task = Self::spawn_blink_task(cx);
    }

    fn stop_blink(&mut self, cx: &mut Context<Self>) {
        self.cursor_visible = false;
        self._blink_task = Task::ready(());
        cx.notify();
    }

    fn spawn_blink_task(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            if this
                .update(cx, |editor, cx| {
                    editor.cursor_visible = !editor.cursor_visible;
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        })
    }

    pub fn reset_blink(&mut self, cx: &mut Context<Self>) {
        self.cursor_visible = true;
        self._blink_task = Self::spawn_blink_task(cx);
    }

    pub fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::JumpTo(offset), cx);
    }

    pub fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let anchor = if self.selection_reversed {
            self.selected_range.end
        } else {
            self.selected_range.start
        };
        self.apply_command(
            EditorCommand::SetSelection {
                start: anchor,
                end: offset,
            },
            cx,
        );
    }

    pub fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Move(CaretMove::Left), cx);
    }

    pub fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Move(CaretMove::Right), cx);
    }

    pub fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Select(CaretMove::Left), cx);
    }

    pub fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Select(CaretMove::Right), cx);
    }

    pub fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Move(CaretMove::Up), cx);
    }

    pub fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Move(CaretMove::Down), cx);
    }

    pub fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Select(CaretMove::Up), cx);
    }

    pub fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Select(CaretMove::Down), cx);
    }

    pub fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Move(CaretMove::Home), cx);
    }

    pub fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Move(CaretMove::End), cx);
    }

    pub fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Select(CaretMove::Home), cx);
    }

    pub fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Select(CaretMove::End), cx);
    }

    pub fn page_up(&mut self, _: &PageUp, window: &mut Window, cx: &mut Context<Self>) {
        let lines = (window.bounds().size.height / window.line_height()).floor() as i32;
        self.apply_command(
            EditorCommand::Move(CaretMove::Vertical {
                delta_lines: -lines.max(1),
            }),
            cx,
        );
    }

    pub fn page_down(&mut self, _: &PageDown, window: &mut Window, cx: &mut Context<Self>) {
        let lines = (window.bounds().size.height / window.line_height()).floor() as i32;
        self.apply_command(
            EditorCommand::Move(CaretMove::Vertical {
                delta_lines: lines.max(1),
            }),
            cx,
        );
    }

    pub fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::SelectAll, cx);
    }

    pub fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.apply_command(EditorCommand::Backspace, cx) == EditorOutcome::Noop {
            window.play_system_bell();
        }
    }

    pub fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.apply_command(EditorCommand::Delete, cx) == EditorOutcome::Noop {
            window.play_system_bell();
        }
    }

    pub fn toggle_bold(&mut self, _: &ToggleBold, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Wrap(WrapKind::Bold), cx);
    }

    pub fn toggle_italic(&mut self, _: &ToggleItalic, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Wrap(WrapKind::Italic), cx);
    }

    pub fn toggle_code(&mut self, _: &ToggleCode, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Wrap(WrapKind::Code), cx);
    }

    pub fn toggle_link(&mut self, _: &ToggleLink, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Wrap(WrapKind::Link), cx);
    }

    pub fn indent(&mut self, _: &Indent, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Indent, cx);
    }

    pub fn outdent(&mut self, _: &Outdent, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Outdent, cx);
    }

    pub fn undo(&mut self, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Undo, cx);
    }

    pub fn redo(&mut self, cx: &mut Context<Self>) {
        self.apply_command(EditorCommand::Redo, cx);
    }

    fn offset_from_utf16(&self, content: &str, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, content: &str, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, content: &str, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(content, range.start)..self.offset_to_utf16(content, range.end)
    }

    fn range_from_utf16(&self, content: &str, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(content, range_utf16.start)
            ..self.offset_from_utf16(content, range_utf16.end)
    }

    pub fn report_caret_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.last_caret_bounds = Some(bounds);
    }

    pub fn sync_ime_cursor(&mut self, window: &mut Window) {
        let Some(origin) = self.last_caret_bounds else {
            return;
        };
        if self.last_ime_origin == Some(origin) {
            return;
        }
        self.last_ime_origin = Some(origin);
        window.invalidate_character_coordinates();
    }
}

impl Focusable for MarkdownEditor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for MarkdownEditor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let content = self.content(cx);
        let range = self.range_from_utf16(&content, &range_utf16);
        actual_range.replace(self.range_to_utf16(&content, &range));
        Some(content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let content = self.content(cx);
        Some(UTF16Selection {
            range: self.range_to_utf16(&content, &self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range.clone()
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let content = self.content(cx);
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(&content, r))
            .or(self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());

        self.selected_range = range;
        self.selection_reversed = false;
        self.apply_command(EditorCommand::InsertText(new_text.to_string()), cx);
        self.marked_range = None;
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let content = self.content(cx);
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(&content, r))
            .or(self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());

        self.replace_text_in_range(range_utf16, new_text, window, cx);

        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        }
        if let Some(sel) = new_selected_range_utf16 {
            let content = self.content(cx);
            let mapped = self.range_from_utf16(&content, &sel);
            self.selected_range =
                range.start + mapped.start..range.start + mapped.end.min(new_text.len());
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        bounds: gpui::Bounds<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.last_caret_bounds.or_else(|| {
            Some(Bounds {
                origin: bounds.origin,
                size: gpui::size(gpui::px(2.), bounds.size.height.min(gpui::px(24.))),
            })
        })
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::headless::{CaretMove, EditorCommand, HeadlessEditor};

    #[test]
    fn gpui_editor_commands_run_on_headless_backend() {
        let mut editor = HeadlessEditor::new("alpha beta");
        editor
            .apply(EditorCommand::SetSelection { start: 0, end: 5 })
            .unwrap();
        editor
            .apply(EditorCommand::InsertText("gamma".into()))
            .unwrap();
        assert_eq!(editor.content(), "gamma beta");
        editor.apply(EditorCommand::Undo).unwrap();
        assert_eq!(editor.content(), "alpha beta");
        editor.apply(EditorCommand::Move(CaretMove::End)).unwrap();
        editor.apply(EditorCommand::Backspace).unwrap();
        assert_eq!(editor.content(), "alpha bet");
        assert_eq!(editor.word_count(), 2);
    }
}
