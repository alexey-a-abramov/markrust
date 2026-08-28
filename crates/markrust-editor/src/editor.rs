// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::ops::Range;
use std::time::Duration;

use gpui::{
    actions, App, Context, Entity, EntityInputHandler, FocusHandle, Focusable, Subscription, Task,
    UTF16Selection, Window,
};
use markrust_core::Document;
use unicode_segmentation::UnicodeSegmentation;

use crate::masking::{Caret, Selection};
use crate::theme::EditorTheme;

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
        SelectAll
    ]
);

/// Per-line layout cache used for hit-testing and caret positioning.
#[derive(Debug, Clone, Default)]
pub struct LineLayoutCache {
    pub line_starts: Vec<usize>,
    pub display_line_starts: Vec<usize>,
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
            _blink_task: Task::ready(()),
            _subscriptions: vec![focus_sub, blur_sub, doc_sub],
        }
    }

    pub fn content(&self, cx: &App) -> String {
        self.document.read(cx).buffer.content()
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

    pub fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    pub fn set_cursor(&mut self, offset: usize, cx: &mut Context<Self>) {
        let len = 0; // clamped in move_to
        let _ = len;
        self.move_to(offset, cx);
    }

    /// Insert plain text at the current caret, replacing any active selection.
    pub fn insert_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let offset = self.cursor_offset();
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.replace_text_in_range(None, text, window, cx);
    }

    pub fn jump_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.move_to(offset, cx);
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
        let len = self.content(cx).len();
        let offset = offset.min(len);
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify();
    }

    pub fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let len = self.content(cx).len();
        let offset = offset.min(len);
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn previous_boundary(&self, content: &str, offset: usize) -> usize {
        content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, content: &str, offset: usize) -> usize {
        content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(content.len())
    }

    fn line_start(&self, content: &str, offset: usize) -> usize {
        content[..offset.min(content.len())]
            .rfind('\n')
            .map(|idx| idx + 1)
            .unwrap_or(0)
    }

    fn line_end(&self, content: &str, offset: usize) -> usize {
        content[offset.min(content.len())..]
            .find('\n')
            .map(|idx| offset + idx)
            .unwrap_or(content.len())
    }

    fn move_vertical(
        &mut self,
        content: &str,
        delta: i32,
        selecting: bool,
        cx: &mut Context<Self>,
    ) {
        let (line, col) = crate::layout::cursor_line_col(content, self.cursor_offset());
        let target_line = if delta < 0 {
            line.saturating_sub((-delta) as usize)
        } else {
            line.saturating_add(delta as usize)
        };
        let line_index = self.document.read(cx).buffer.line_index();
        let target_offset =
            line_index.offset_of_line_col(target_line, col, self.document.read(cx).buffer.text());
        if selecting {
            self.select_to(target_offset, cx);
        } else {
            self.move_to(target_offset, cx);
        }
        self.reset_blink(cx);
    }

    pub fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let content = self.content(cx);
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(&content, self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
        self.reset_blink(cx);
    }

    pub fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let content = self.content(cx);
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(&content, self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
        self.reset_blink(cx);
    }

    pub fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        let content = self.content(cx);
        self.select_to(self.previous_boundary(&content, self.cursor_offset()), cx);
        self.reset_blink(cx);
    }

    pub fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        let content = self.content(cx);
        self.select_to(self.next_boundary(&content, self.cursor_offset()), cx);
        self.reset_blink(cx);
    }

    pub fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        let content = self.content(cx);
        self.move_vertical(&content, -1, false, cx);
    }

    pub fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        let content = self.content(cx);
        self.move_vertical(&content, 1, false, cx);
    }

    pub fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let content = self.content(cx);
        self.move_vertical(&content, -1, true, cx);
    }

    pub fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let content = self.content(cx);
        self.move_vertical(&content, 1, true, cx);
    }

    pub fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let content = self.content(cx);
        self.move_to(self.line_start(&content, self.cursor_offset()), cx);
        self.reset_blink(cx);
    }

    pub fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let content = self.content(cx);
        self.move_to(self.line_end(&content, self.cursor_offset()), cx);
        self.reset_blink(cx);
    }

    pub fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        let content = self.content(cx);
        self.select_to(self.line_start(&content, self.cursor_offset()), cx);
        self.reset_blink(cx);
    }

    pub fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        let content = self.content(cx);
        self.select_to(self.line_end(&content, self.cursor_offset()), cx);
        self.reset_blink(cx);
    }

    pub fn page_up(&mut self, _: &PageUp, window: &mut Window, cx: &mut Context<Self>) {
        let lines = (window.bounds().size.height / window.line_height()).floor() as i32;
        let content = self.content(cx);
        self.move_vertical(&content, -lines.max(1), false, cx);
    }

    pub fn page_down(&mut self, _: &PageDown, window: &mut Window, cx: &mut Context<Self>) {
        let lines = (window.bounds().size.height / window.line_height()).floor() as i32;
        let content = self.content(cx);
        self.move_vertical(&content, lines.max(1), false, cx);
    }

    pub fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.content(cx).len();
        self.move_to(0, cx);
        self.select_to(len, cx);
        self.reset_blink(cx);
    }

    pub fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        let content = self.content(cx);
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(&content, self.cursor_offset());
            if prev == self.cursor_offset() {
                window.play_system_bell();
                return;
            }
            self.select_to(prev, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    pub fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        let content = self.content(cx);
        if self.selected_range.is_empty() {
            let next = self.next_boundary(&content, self.cursor_offset());
            if next == self.cursor_offset() {
                window.play_system_bell();
                return;
            }
            self.select_to(next, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    pub fn undo(&mut self, cx: &mut Context<Self>) {
        self.document.update(cx, |doc, cx| {
            doc.apply_pending_parse();
            if doc.undo() {
                doc.apply_pending_parse();
                cx.notify();
            }
        });
        self.clamp_cursor(cx);
        self.reset_blink(cx);
        cx.notify();
    }

    pub fn redo(&mut self, cx: &mut Context<Self>) {
        self.document.update(cx, |doc, cx| {
            doc.apply_pending_parse();
            if doc.redo() {
                doc.apply_pending_parse();
                cx.notify();
            }
        });
        self.clamp_cursor(cx);
        self.reset_blink(cx);
        cx.notify();
    }

    fn clamp_cursor(&mut self, cx: &mut Context<Self>) {
        let len = self.content(cx).len();
        let cursor = self.cursor_offset().min(len);
        self.selected_range = cursor..cursor;
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

        self.document.update(cx, |doc, cx| {
            doc.apply_pending_parse();
            if !range.is_empty() {
                doc.delete(range.start, range.end);
            }
            if !new_text.is_empty() {
                doc.insert(range.start, new_text);
            }
            doc.apply_pending_parse();
            cx.notify();
        });

        let new_cursor = range.start + new_text.len();
        self.selected_range = new_cursor..new_cursor;
        self.marked_range = None;
        self.reset_blink(cx);
        cx.notify();
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
        _bounds: gpui::Bounds<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        None
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
