// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The WYSIWYG editor view: a virtualized list of rendered blocks kept in
//! sync with the document through [`RichEngine`].

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, list, prelude::*, px, App, Bounds, Context, CursorStyle, Entity, EntityInputHandler,
    FocusHandle, Focusable, ListAlignment, ListState, Pixels, Render, Subscription, Task,
    UTF16Selection, Window,
};
use markrust_core::rich::{
    apply_rich_command, Bias, CaretState, MarkSet, NodeId, RichCommand, RichEngine, RichOutcome,
};
use markrust_core::Document;

use super::block_text::WysiwygHost;
use super::blocks::{render_top_block, RenderSnapshot};
use crate::headless::{CaretMove, EditorCommand, EditorOutcome};
use crate::theme::EditorTheme;

pub struct RichEditorView {
    document: Entity<Document>,
    pub theme: EditorTheme,
    engine: RichEngine,
    list_state: ListState,
    snapshot: Option<Arc<RenderSnapshot>>,
    synced_revision: Option<u64>,
    pub selected_range: Range<usize>,
    pub selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    preedit: Option<String>,
    is_selecting: bool,
    cursor_visible: bool,
    focus_handle: FocusHandle,
    last_ime_bounds: Option<Bounds<Pixels>>,
    _blink_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl RichEditorView {
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
            engine: RichEngine::new(),
            list_state: ListState::new(0, ListAlignment::Top, px(512.)),
            snapshot: None,
            synced_revision: None,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            preedit: None,
            is_selecting: false,
            cursor_visible: true,
            focus_handle,
            last_ime_bounds: None,
            _blink_task: Task::ready(()),
            _subscriptions: vec![focus_sub, blur_sub, doc_sub],
        }
    }

    pub fn set_theme(&mut self, theme: EditorTheme, cx: &mut Context<Self>) {
        self.theme = theme;
        self.snapshot = None;
        self.synced_revision = None;
        cx.notify();
    }

    pub fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    pub fn jump_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.apply_editor_command(EditorCommand::JumpTo(offset), cx);
    }

    fn caret_state(&self) -> CaretState {
        CaretState {
            range: self.selected_range.clone(),
            reversed: self.selection_reversed,
        }
    }

    fn restore_caret(&mut self, caret: CaretState) {
        self.selected_range = caret.range;
        self.selection_reversed = caret.reversed;
    }

    pub fn apply_rich(&mut self, command: RichCommand, cx: &mut Context<Self>) -> RichOutcome {
        let mut caret = self.caret_state();
        let mut outcome = RichOutcome::Noop;
        self.document.update(cx, |doc, cx| {
            if let Ok(result) = apply_rich_command(doc, &mut self.engine, &mut caret, command) {
                outcome = result;
                cx.notify();
            }
        });
        self.restore_caret(caret);
        if outcome != RichOutcome::Noop {
            self.reset_blink(cx);
            self.snapshot = None;
            self.synced_revision = None;
        }
        cx.notify();
        outcome
    }

    pub fn apply_editor_command(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> EditorOutcome {
        match command {
            EditorCommand::InsertText(text) => {
                self.apply_rich(RichCommand::InsertText(text), cx);
                EditorOutcome::Changed
            }
            EditorCommand::Backspace => {
                if self.apply_rich(RichCommand::Backspace, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::Delete => {
                if self.apply_rich(RichCommand::Delete, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::Undo => self.undo(cx),
            EditorCommand::Redo => self.redo(cx),
            EditorCommand::JumpTo(offset) => {
                self.move_to(offset, false, cx);
                EditorOutcome::CaretMoved
            }
            EditorCommand::SetSelection { start, end } => {
                let len = self.document.read(cx).buffer.len_bytes();
                self.selected_range = start.min(len)..end.min(len);
                self.selection_reversed = start > end;
                cx.notify();
                EditorOutcome::CaretMoved
            }
            EditorCommand::SelectAll => {
                let len = self.document.read(cx).buffer.len_bytes();
                self.selected_range = 0..len;
                self.selection_reversed = false;
                cx.notify();
                EditorOutcome::CaretMoved
            }
            EditorCommand::Move(movement) => {
                self.move_caret(movement, false, cx);
                EditorOutcome::CaretMoved
            }
            EditorCommand::Select(movement) => {
                self.move_caret(movement, true, cx);
                EditorOutcome::CaretMoved
            }
            EditorCommand::Wrap(kind) => {
                let cmd = match kind {
                    crate::wrap::WrapKind::Bold => RichCommand::ToggleMark(MarkSet::BOLD),
                    crate::wrap::WrapKind::Italic => RichCommand::ToggleMark(MarkSet::ITALIC),
                    crate::wrap::WrapKind::Code => RichCommand::ToggleMark(MarkSet::CODE),
                    crate::wrap::WrapKind::Link => RichCommand::ToggleLink,
                };
                if self.apply_rich(cmd, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::Indent => {
                if self.apply_rich(RichCommand::IndentList, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::Outdent => {
                if self.apply_rich(RichCommand::OutdentList, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
        }
    }

    fn undo(&mut self, cx: &mut Context<Self>) -> EditorOutcome {
        let mut restored = None;
        self.document.update(cx, |doc, cx| {
            if let Some(tx) = doc.undo_tx() {
                restored = Some(tx.selection_after);
                cx.notify();
            }
        });
        if let Some(snap) = restored {
            self.selected_range = snap.range();
            self.selection_reversed = snap.reversed;
            self.engine.invalidate();
            self.snapshot = None;
            self.synced_revision = None;
            cx.notify();
            EditorOutcome::Changed
        } else {
            EditorOutcome::Noop
        }
    }

    fn redo(&mut self, cx: &mut Context<Self>) -> EditorOutcome {
        let mut restored = None;
        self.document.update(cx, |doc, cx| {
            if let Some(tx) = doc.redo_tx() {
                restored = Some(tx.selection_after);
                cx.notify();
            }
        });
        if let Some(snap) = restored {
            self.selected_range = snap.range();
            self.selection_reversed = snap.reversed;
            self.engine.invalidate();
            self.snapshot = None;
            self.synced_revision = None;
            cx.notify();
            EditorOutcome::Changed
        } else {
            EditorOutcome::Noop
        }
    }

    fn move_to(&mut self, offset: usize, extend: bool, cx: &mut Context<Self>) {
        let len = self.document.read(cx).buffer.len_bytes();
        let source = self.document.read(cx).buffer.content();
        self.engine.sync(self.document.read(cx));
        let offset = self.engine.snap_caret(offset.min(len), Bias::Left);
        if extend {
            let anchor = if self.selection_reversed {
                self.selected_range.end
            } else {
                self.selected_range.start
            };
            if offset < anchor {
                self.selected_range = offset..anchor;
                self.selection_reversed = true;
            } else {
                self.selected_range = anchor..offset;
                self.selection_reversed = false;
            }
        } else {
            self.selected_range = offset..offset;
            self.selection_reversed = false;
        }
        let _ = source;
        self.reset_blink(cx);
        cx.notify();
    }

    fn move_caret(&mut self, movement: CaretMove, extend: bool, cx: &mut Context<Self>) {
        let source = self.document.read(cx).buffer.content();
        self.engine.sync(self.document.read(cx));
        let cursor = self.cursor_offset();
        let target = match movement {
            CaretMove::Left => {
                if !extend && !self.selected_range.is_empty() {
                    self.selected_range.start
                } else {
                    self.engine.prev_caret(&source, cursor)
                }
            }
            CaretMove::Right => {
                if !extend && !self.selected_range.is_empty() {
                    self.selected_range.end
                } else {
                    self.engine.next_caret(&source, cursor)
                }
            }
            CaretMove::Home => {
                let start = source[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
                self.engine.snap_caret(start, Bias::Right)
            }
            CaretMove::End => {
                let end = source[cursor..]
                    .find('\n')
                    .map(|i| cursor + i)
                    .unwrap_or(source.len());
                self.engine.snap_caret(end, Bias::Left)
            }
            CaretMove::Up => self.vertical(&source, cursor, -1),
            CaretMove::Down => self.vertical(&source, cursor, 1),
            CaretMove::Vertical { delta_lines } => self.vertical(&source, cursor, delta_lines),
        };
        self.move_to(target, extend, cx);
    }

    fn vertical(&self, source: &str, cursor: usize, delta: i32) -> usize {
        if delta == 0 {
            return cursor;
        }
        let line_start = source[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let col = cursor - line_start;
        let mut line = 0i32;
        let mut idx = 0usize;
        let mut starts = vec![0usize];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
                if i < cursor {
                    line += 1;
                }
                idx = i;
            }
            let _ = idx;
        }
        let target_line = (line + delta).clamp(0, starts.len().saturating_sub(1) as i32) as usize;
        let start = starts[target_line];
        let end = starts
            .get(target_line + 1)
            .copied()
            .unwrap_or(source.len())
            .saturating_sub(1)
            .max(start);
        self.engine.snap_caret((start + col).min(end), Bias::Left)
    }

    fn start_blink(&mut self, cx: &mut Context<Self>) {
        self.cursor_visible = true;
        self._blink_task = Self::spawn_blink_task(cx);
        cx.notify();
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

    fn reset_blink(&mut self, cx: &mut Context<Self>) {
        self.cursor_visible = true;
        self._blink_task = Self::spawn_blink_task(cx);
    }

    fn sync_snapshot(&mut self, cx: &mut Context<Self>) -> Arc<RenderSnapshot> {
        let doc = self.document.read(cx);
        let revision = doc.revision();
        if self.synced_revision == Some(revision) {
            if let Some(snapshot) = &self.snapshot {
                return snapshot.clone();
            }
        }
        let base_dir = doc
            .path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf());
        let old_count = self.engine.tree().blocks.len();
        self.engine.sync(doc);
        let new_count = self.engine.tree().blocks.len();
        match self.engine.last_splice() {
            Some(splice) if self.synced_revision.is_some() => {
                self.list_state
                    .splice(splice.range.clone(), splice.new_count);
            }
            _ => {
                self.list_state
                    .splice(0..old_count.min(new_count.max(old_count)), new_count);
                self.list_state = ListState::new(new_count, ListAlignment::Top, px(512.));
            }
        }
        let snapshot = Arc::new(RenderSnapshot {
            tree: self.engine.tree().clone(),
            theme: self.theme.clone(),
            base_dir,
        });
        self.snapshot = Some(snapshot.clone());
        self.synced_revision = Some(revision);
        snapshot
    }

    fn offset_from_utf16(content: &str, offset: usize) -> usize {
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

    fn offset_to_utf16(content: &str, offset: usize) -> usize {
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
}

impl WysiwygHost for RichEditorView {
    fn click_source(
        &mut self,
        source: usize,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;
        self.focus_handle.focus(window, cx);
        self.move_to(source, extend, cx);
    }

    fn drag_source(&mut self, source: usize, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.move_to(source, true, cx);
        }
    }

    fn end_drag(&mut self, _cx: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn selected_range(&self) -> Range<usize> {
        self.selected_range.clone()
    }

    fn caret_offset(&self) -> usize {
        self.cursor_offset()
    }

    fn caret_visible(&self) -> bool {
        self.cursor_visible
    }

    fn is_selecting(&self) -> bool {
        self.is_selecting
    }

    fn focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    fn input_focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn toggle_task(&mut self, id: NodeId, cx: &mut Context<Self>) {
        let checked = self.engine.block(id).and_then(|block| match block.kind {
            markrust_core::rich::BlockKind::ListItem { task: Some(c) } => Some(!c),
            _ => None,
        });
        if let Some(checked) = checked {
            self.apply_rich(RichCommand::SetTaskChecked { id, checked }, cx);
        }
    }
}

impl Focusable for RichEditorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for RichEditorView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let content = self.document.read(cx).buffer.content();
        let start = Self::offset_from_utf16(&content, range_utf16.start);
        let end = Self::offset_from_utf16(&content, range_utf16.end);
        actual_range
            .replace(Self::offset_to_utf16(&content, start)..Self::offset_to_utf16(&content, end));
        Some(content.get(start..end).unwrap_or("").to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let content = self.document.read(cx).buffer.content();
        Some(UTF16Selection {
            range: Self::offset_to_utf16(&content, self.selected_range.start)
                ..Self::offset_to_utf16(&content, self.selected_range.end),
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
        self.preedit = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(range_utf16) = range_utf16 {
            let content = self.document.read(cx).buffer.content();
            let start = Self::offset_from_utf16(&content, range_utf16.start);
            let end = Self::offset_from_utf16(&content, range_utf16.end);
            self.selected_range = start..end;
            self.selection_reversed = false;
        } else if let Some(marked) = self.marked_range.take() {
            self.selected_range = marked;
            self.selection_reversed = false;
        }
        self.preedit = None;
        self.apply_rich(RichCommand::InsertText(new_text.to_string()), cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Preedit is display-only; the model is untouched until commit.
        self.preedit = if new_text.is_empty() {
            None
        } else {
            Some(new_text.to_string())
        };
        self.marked_range = Some(self.cursor_offset()..self.cursor_offset());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.last_ime_bounds = Some(bounds);
        Some(Bounds {
            origin: bounds.origin,
            size: gpui::size(px(2.), bounds.size.height.min(px(24.))),
        })
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for RichEditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.sync_snapshot(cx);
        let theme = self.theme.clone();
        let editor = cx.entity();
        let focus = self.focus_handle.clone();
        div()
            .size_full()
            .bg(theme.editor_bg)
            .key_context("RichEditor")
            .track_focus(&focus)
            .cursor(CursorStyle::IBeam)
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Backspace, window, cx| {
                    editor.update(cx, |e, cx| {
                        if e.apply_editor_command(EditorCommand::Backspace, cx)
                            == EditorOutcome::Noop
                        {
                            window.play_system_bell();
                        }
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Delete, window, cx| {
                    editor.update(cx, |e, cx| {
                        if e.apply_editor_command(EditorCommand::Delete, cx) == EditorOutcome::Noop
                        {
                            window.play_system_bell();
                        }
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Left, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::Left), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Right, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::Right), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Up, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::Up), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Down, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::Down), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectLeft, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::Left), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectRight, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::Right), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Home, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::Home), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::End, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::End), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectAll, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::SelectAll, cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Enter, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_rich(RichCommand::SplitBlock, cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::ToggleBold, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_rich(RichCommand::ToggleMark(MarkSet::BOLD), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::ToggleItalic, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_rich(RichCommand::ToggleMark(MarkSet::ITALIC), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::ToggleCode, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_rich(RichCommand::ToggleMark(MarkSet::CODE), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::ToggleLink, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_rich(RichCommand::ToggleLink, cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Indent, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_rich(RichCommand::IndentList, cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Outdent, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_rich(RichCommand::OutdentList, cx);
                    });
                }
            })
            .child(
                list(self.list_state.clone(), move |index, _window, _cx| {
                    render_top_block(&snapshot, index, editor.clone())
                })
                .size_full()
                .py(px(16.)),
            )
    }
}
