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
    FocusHandle, Focusable, ListAlignment, ListState, Pixels, Render, SharedString, Subscription,
    Task, UTF16Selection, Window,
};
use markrust_core::rich::{
    apply_rich_command, Bias, CaretState, MarkSet, NodeId, RichCommand, RichEngine, RichOutcome,
};
use markrust_core::Document;

use super::block_text::{hit_test_leaf, LeafLayout, WidgetImeSink, WysiwygHost};
use super::blocks::{render_top_block, RenderSnapshot};
use super::ime::{caret_from_element_bounds, widget_caret_rect, ImeLeafHit, ImeOriginState};
use crate::headless::{CaretMove, EditorCommand, EditorOutcome};
use crate::theme::EditorTheme;

#[derive(Debug, Clone, Default)]
enum WidgetEdit {
    #[default]
    Idle,
    CodeInfo {
        id: NodeId,
        draft: String,
    },
    ImageAlt {
        range: Range<usize>,
        draft: String,
    },
    Frontmatter {
        key: &'static str,
        draft: String,
    },
    FrontmatterYaml {
        draft: String,
    },
}

impl WidgetEdit {
    fn draft_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Idle => None,
            Self::CodeInfo { draft, .. }
            | Self::ImageAlt { draft, .. }
            | Self::Frontmatter { draft, .. }
            | Self::FrontmatterYaml { draft } => Some(draft),
        }
    }

    fn allows_newline(&self) -> bool {
        matches!(self, Self::FrontmatterYaml { .. })
    }
}

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
    ime: ImeOriginState,
    widget_edit: WidgetEdit,
    widget_preedit: Option<String>,
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
            this.commit_widget_edit(cx);
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
            ime: ImeOriginState::default(),
            widget_edit: WidgetEdit::Idle,
            widget_preedit: None,
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

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
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
        if let RichCommand::InsertText(text) = &command {
            if self.widget_insert(text, cx) {
                return RichOutcome::Changed;
            }
        } else {
            self.commit_widget_edit(cx);
        }
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
                if self.widget_insert(&text, cx) {
                    EditorOutcome::Changed
                } else {
                    self.apply_rich(RichCommand::InsertText(text), cx);
                    EditorOutcome::Changed
                }
            }
            EditorCommand::Backspace => {
                if self.widget_backspace(cx) {
                    EditorOutcome::Changed
                } else if self.apply_rich(RichCommand::Backspace, cx) == RichOutcome::Noop {
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
                self.engine.sync(self.document.read(cx));
                if self.engine.in_table(self.cursor_offset()) {
                    if self.apply_rich(RichCommand::TableTab { reverse: false }, cx)
                        == RichOutcome::Noop
                    {
                        EditorOutcome::Noop
                    } else {
                        EditorOutcome::Changed
                    }
                } else if self.apply_rich(RichCommand::IndentList, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::Outdent => {
                self.engine.sync(self.document.read(cx));
                if self.engine.in_table(self.cursor_offset()) {
                    if self.apply_rich(RichCommand::TableTab { reverse: true }, cx)
                        == RichOutcome::Noop
                    {
                        EditorOutcome::Noop
                    } else {
                        EditorOutcome::Changed
                    }
                } else if self.apply_rich(RichCommand::OutdentList, cx) == RichOutcome::Noop {
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
        let widget_only = self.synced_revision == Some(revision) && self.snapshot.is_none();
        let base_dir = doc
            .path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf());
        let old_count = self.engine.tree().blocks.len();
        self.engine.sync(doc);
        let new_count = self.engine.tree().blocks.len();
        if !widget_only {
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
        }
        let snapshot = Arc::new(RenderSnapshot {
            tree: self.engine.tree().clone(),
            source: doc.buffer.content(),
            theme: self.theme.clone(),
            base_dir,
            editing_code: match &self.widget_edit {
                WidgetEdit::CodeInfo { id, draft } => Some((*id, draft.clone())),
                _ => None,
            },
            editing_image: match &self.widget_edit {
                WidgetEdit::ImageAlt { range, draft } => Some((range.clone(), draft.clone())),
                _ => None,
            },
            widget_preedit: self.widget_preedit.clone(),
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

    fn widget_insert(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        if matches!(self.widget_edit, WidgetEdit::Idle) {
            return false;
        }
        if text.contains('\n') && !self.widget_edit.allows_newline() {
            self.commit_widget_edit(cx);
            return true;
        }
        self.widget_preedit = None;
        if let Some(draft) = self.widget_edit.draft_mut() {
            draft.push_str(text);
        }
        self.snapshot = None;
        cx.notify();
        true
    }

    fn widget_backspace(&mut self, cx: &mut Context<Self>) -> bool {
        if matches!(self.widget_edit, WidgetEdit::Idle) {
            return false;
        }
        self.widget_preedit = None;
        if let Some(draft) = self.widget_edit.draft_mut() {
            draft.pop();
        }
        self.snapshot = None;
        cx.notify();
        true
    }

    fn widget_draft_mut(&mut self) -> Option<&mut String> {
        self.widget_edit.draft_mut()
    }

    fn widget_display(&self) -> Option<(String, Option<String>)> {
        let draft = match &self.widget_edit {
            WidgetEdit::Idle => return None,
            WidgetEdit::CodeInfo { draft, .. }
            | WidgetEdit::ImageAlt { draft, .. }
            | WidgetEdit::Frontmatter { draft, .. }
            | WidgetEdit::FrontmatterYaml { draft } => draft.clone(),
        };
        Some((draft, self.widget_preedit.clone()))
    }

    fn cancel_widget_edit(&mut self, cx: &mut Context<Self>) -> bool {
        if matches!(self.widget_edit, WidgetEdit::Idle) {
            return false;
        }
        self.widget_edit = WidgetEdit::Idle;
        self.widget_preedit = None;
        self.snapshot = None;
        cx.notify();
        true
    }

    fn commit_widget_edit(&mut self, cx: &mut Context<Self>) -> bool {
        self.widget_preedit = None;
        let edit = std::mem::take(&mut self.widget_edit);
        match edit {
            WidgetEdit::Idle => false,
            WidgetEdit::CodeInfo { id, draft } => {
                self.apply_rich(RichCommand::SetCodeInfo { id, info: draft }, cx);
                true
            }
            WidgetEdit::ImageAlt { range, draft } => {
                self.apply_rich(
                    RichCommand::SetImageAlt {
                        source_range: range,
                        alt: draft,
                    },
                    cx,
                );
                true
            }
            WidgetEdit::Frontmatter { key, draft } => {
                self.apply_rich(
                    RichCommand::SetFrontmatterField {
                        key: key.to_string(),
                        value: draft,
                    },
                    cx,
                );
                true
            }
            WidgetEdit::FrontmatterYaml { draft } => {
                self.apply_rich(RichCommand::SetFrontmatter { raw: draft }, cx);
                true
            }
        }
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
        self.commit_widget_edit(cx);
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

    fn widget_editing(&self) -> bool {
        !matches!(self.widget_edit, WidgetEdit::Idle)
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

    fn edit_code_info(&mut self, id: NodeId, cx: &mut Context<Self>) {
        self.commit_widget_edit(cx);
        self.engine.sync(self.document.read(cx));
        let draft = self
            .engine
            .block(id)
            .and_then(|b| match &b.kind {
                markrust_core::rich::BlockKind::CodeBlock { info, .. } => Some(info.clone()),
                _ => None,
            })
            .unwrap_or_default();
        self.widget_edit = WidgetEdit::CodeInfo { id, draft };
        self.widget_preedit = None;
        self.snapshot = None;
        cx.notify();
    }

    fn edit_image_alt(&mut self, source_range: Range<usize>, alt: &str, cx: &mut Context<Self>) {
        self.commit_widget_edit(cx);
        self.widget_edit = WidgetEdit::ImageAlt {
            range: source_range,
            draft: alt.to_string(),
        };
        self.widget_preedit = None;
        self.snapshot = None;
        cx.notify();
    }

    fn edit_frontmatter_field(&mut self, key: &'static str, current: &str, cx: &mut Context<Self>) {
        self.commit_widget_edit(cx);
        self.widget_edit = WidgetEdit::Frontmatter {
            key,
            draft: current.to_string(),
        };
        self.widget_preedit = None;
        self.snapshot = None;
        cx.notify();
    }

    fn edit_frontmatter_yaml(&mut self, current: &str, cx: &mut Context<Self>) {
        self.commit_widget_edit(cx);
        self.widget_edit = WidgetEdit::FrontmatterYaml {
            draft: current.to_string(),
        };
        self.widget_preedit = None;
        self.snapshot = None;
        cx.notify();
    }

    fn open_table_menu(&mut self, source: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_widget_edit(cx);
        self.focus_handle.focus(window, cx);
        self.move_to(source, false, cx);
        cx.notify();
    }

    fn finish_widget(&mut self, cx: &mut Context<Self>) {
        self.commit_widget_edit(cx);
    }

    fn report_widget_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.ime.report_widget(bounds);
    }

    fn preedit(&self) -> Option<&str> {
        if matches!(self.widget_edit, WidgetEdit::Idle) {
            self.preedit.as_deref()
        } else {
            None
        }
    }

    fn report_leaf(
        &mut self,
        layout: Arc<LeafLayout>,
        element_bounds: Bounds<Pixels>,
        font_size: f32,
        line_height: f32,
        caret_bounds: Option<Bounds<Pixels>>,
    ) {
        self.ime.report_leaf(ImeLeafHit {
            layout,
            bounds: element_bounds,
            font_size,
            line_height,
            caret_bounds,
        });
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
        if let Some((draft, preedit)) = self.widget_display() {
            let content = format!("{}{}", draft, preedit.unwrap_or_default());
            let start = Self::offset_from_utf16(&content, range_utf16.start);
            let end = Self::offset_from_utf16(&content, range_utf16.end);
            actual_range.replace(
                Self::offset_to_utf16(&content, start)..Self::offset_to_utf16(&content, end),
            );
            return Some(content.get(start..end).unwrap_or("").to_string());
        }
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
        if let Some((draft, preedit)) = self.widget_display() {
            let content = format!("{}{}", draft, preedit.unwrap_or_default());
            let n = Self::offset_to_utf16(&content, content.len());
            return Some(UTF16Selection {
                range: n..n,
                reversed: false,
            });
        }
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
        self.widget_preedit = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.widget_edit, WidgetEdit::Idle) {
            self.widget_preedit = None;
            if let Some(draft) = self.widget_draft_mut() {
                if let Some(range_utf16) = range_utf16 {
                    let content = draft.clone();
                    let start = Self::offset_from_utf16(&content, range_utf16.start);
                    let end = Self::offset_from_utf16(&content, range_utf16.end);
                    let start = start.min(draft.len());
                    let end = end.min(draft.len()).max(start);
                    draft.replace_range(start..end, new_text);
                } else {
                    draft.push_str(new_text);
                }
            }
            self.snapshot = None;
            cx.notify();
            return;
        }
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
        if !matches!(self.widget_edit, WidgetEdit::Idle) {
            self.widget_preedit = if new_text.is_empty() {
                None
            } else {
                Some(new_text.to_string())
            };
            self.snapshot = None;
            cx.notify();
            return;
        }
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
        if let Some(caret) = self.ime.caret_rect() {
            return Some(caret);
        }
        if self.ime.widget_focused() {
            return Some(widget_caret_rect(bounds));
        }
        Some(caret_from_element_bounds(bounds))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        if self.ime.widget_focused() {
            if let Some((draft, preedit)) = self.widget_display() {
                let content = format!("{}{}", draft, preedit.unwrap_or_default());
                return Some(Self::offset_to_utf16(&content, content.len()));
            }
        }
        let theme = self.theme.clone();
        let leaf = self.ime.leaf_at_point(point)?;
        let layout = leaf.layout.clone();
        let bounds = leaf.bounds;
        let font_size = leaf.font_size;
        let line_height = leaf.line_height;
        let vis = hit_test_leaf(
            &layout,
            bounds,
            point,
            window,
            font_size,
            line_height,
            &theme,
        );
        let src = layout.source_for_visible(vis);
        let content = self.document.read(cx).buffer.content();
        Some(Self::offset_to_utf16(&content, src))
    }
}

impl Render for RichEditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ime.begin_frame(
            !matches!(self.widget_edit, WidgetEdit::Idle),
            self.cursor_offset(),
        );
        let snapshot = self.sync_snapshot(cx);
        let theme = self.theme.clone();
        let editor = cx.entity();
        let focus = self.focus_handle.clone();
        let fm_info = markrust_core::parse_frontmatter(&snapshot.source);
        let editing_fm = match &self.widget_edit {
            WidgetEdit::Frontmatter { key, draft } => Some((*key, draft.clone())),
            _ => None,
        };
        let editing_yaml = match &self.widget_edit {
            WidgetEdit::FrontmatterYaml { draft } => Some(draft.clone()),
            _ => None,
        };
        let widget_preedit = self.widget_preedit.clone();
        let in_table = self.engine.table_pos(self.cursor_offset()).is_some();
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
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
                        if matches!(e.widget_edit, WidgetEdit::FrontmatterYaml { .. }) {
                            let _ = e.widget_insert("\n", cx);
                        } else if !e.commit_widget_edit(cx) {
                            e.apply_rich(RichCommand::SplitBlock, cx);
                        }
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Escape, _, cx| {
                    editor.update(cx, |e, cx| {
                        let _ = e.cancel_widget_edit(cx);
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
            .children(fm_info.map(|info| {
                frontmatter_panel(
                    editor.clone(),
                    &theme,
                    &info,
                    editing_fm.as_ref().map(|(k, d)| (*k, d.as_str())),
                    editing_yaml.as_deref(),
                    widget_preedit.as_deref(),
                )
            }))
            .when(in_table, |root| {
                root.child(table_toolbar(editor.clone(), &theme))
            })
            .child(
                list(self.list_state.clone(), move |index, _window, _cx| {
                    render_top_block(&snapshot, index, editor.clone())
                })
                .flex_1()
                .size_full()
                .py(px(16.)),
            )
    }
}

fn frontmatter_panel(
    editor: gpui::Entity<RichEditorView>,
    theme: &crate::theme::EditorTheme,
    info: &markrust_core::FrontmatterInfo,
    editing_fm: Option<(&'static str, &str)>,
    editing_yaml: Option<&str>,
    widget_preedit: Option<&str>,
) -> gpui::AnyElement {
    let pre = widget_preedit.unwrap_or("");
    let field = |key: &'static str, placeholder: &str, stored: Option<&str>| -> String {
        match editing_fm {
            Some((k, draft)) if k == key => format!("{draft}{pre}|"),
            _ => stored
                .filter(|s| !s.is_empty())
                .unwrap_or(placeholder)
                .to_string(),
        }
    };
    let title_value = field("title", "Add a title", info.title.as_deref());
    let desc_value = field(
        "description",
        "Add a description",
        info.description.as_deref(),
    );
    let tags_value = field("tags", "Add tags", info.tags.as_deref());
    let yaml_editing = editing_yaml.is_some();
    let yaml_value = match editing_yaml {
        Some(draft) => format!("{draft}{pre}|"),
        None => {
            let body = info.yaml_body.trim();
            if body.is_empty() {
                "Add YAML".to_string()
            } else {
                body.to_string()
            }
        }
    };
    div()
        .id("wysiwyg-frontmatter")
        .px(px(24.))
        .pt(px(12.))
        .child(
            div()
                .px(px(12.))
                .py(px(8.))
                .rounded_md()
                .border_1()
                .border_color(theme.separator)
                .bg(theme.sidebar_bg)
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.secondary_text)
                        .child("Frontmatter"),
                )
                .child(frontmatter_field_row(FmField {
                    editor: editor.clone(),
                    theme,
                    id: "fm-title",
                    key: "title",
                    label: "Title",
                    display: title_value,
                    current: info.title.clone().unwrap_or_default(),
                    editing: matches!(editing_fm, Some(("title", _))),
                    color: theme.frontmatter_text,
                }))
                .child(frontmatter_field_row(FmField {
                    editor: editor.clone(),
                    theme,
                    id: "fm-description",
                    key: "description",
                    label: "Description",
                    display: desc_value,
                    current: info.description.clone().unwrap_or_default(),
                    editing: matches!(editing_fm, Some(("description", _))),
                    color: theme.secondary_text,
                }))
                .child(frontmatter_field_row(FmField {
                    editor: editor.clone(),
                    theme,
                    id: "fm-tags",
                    key: "tags",
                    label: "Tags",
                    display: tags_value,
                    current: info.tags.clone().unwrap_or_default(),
                    editing: matches!(editing_fm, Some(("tags", _))),
                    color: theme.secondary_text,
                }))
                .child(frontmatter_yaml_row(
                    editor,
                    theme,
                    yaml_value,
                    info.yaml_body.clone(),
                    yaml_editing,
                )),
        )
        .into_any_element()
}

struct FmField<'a> {
    editor: gpui::Entity<RichEditorView>,
    theme: &'a crate::theme::EditorTheme,
    id: &'static str,
    key: &'static str,
    label: &'static str,
    display: String,
    current: String,
    editing: bool,
    color: gpui::Hsla,
}

fn frontmatter_field_row(field: FmField<'_>) -> gpui::AnyElement {
    let FmField {
        editor,
        theme,
        id,
        key,
        label,
        display,
        current,
        editing,
        color,
    } = field;
    let editor_click = editor.clone();
    let row = div()
        .id(id)
        .text_sm()
        .text_color(color)
        .cursor(CursorStyle::PointingHand)
        .when(editing, |el| el.border_b_1().border_color(theme.accent))
        .child(SharedString::from(format!("{label}: {display}")))
        .on_click(move |_, _, cx| {
            editor_click.update(cx, |host, cx| {
                host.edit_frontmatter_field(key, &current, cx);
            });
        });
    div()
        .relative()
        .child(row)
        .when(editing, |el| {
            el.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .child(WidgetImeSink { editor }),
            )
        })
        .into_any_element()
}

fn frontmatter_yaml_row(
    editor: gpui::Entity<RichEditorView>,
    theme: &crate::theme::EditorTheme,
    display: String,
    current: String,
    editing: bool,
) -> gpui::AnyElement {
    let editor_click = editor.clone();
    let lines: Vec<gpui::AnyElement> = display
        .lines()
        .map(|line| {
            div()
                .child(SharedString::from(line.to_string()))
                .into_any_element()
        })
        .collect();
    let block = div()
        .id("fm-yaml")
        .text_xs()
        .font_family(theme.code_font_family.clone())
        .text_color(theme.frontmatter_text)
        .cursor(CursorStyle::PointingHand)
        .when(editing, |el| el.border_b_1().border_color(theme.accent))
        .child(
            div()
                .text_xs()
                .text_color(theme.secondary_text)
                .font_family(theme.font_family.clone())
                .child("YAML"),
        )
        .children(lines)
        .on_click(move |_, _, cx| {
            editor_click.update(cx, |host, cx| {
                host.edit_frontmatter_yaml(&current, cx);
            });
        });
    div()
        .relative()
        .mt(px(4.))
        .child(block)
        .when(editing, |el| {
            el.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .child(WidgetImeSink { editor }),
            )
        })
        .into_any_element()
}

fn table_toolbar(
    editor: gpui::Entity<RichEditorView>,
    theme: &crate::theme::EditorTheme,
) -> gpui::AnyElement {
    let items: [(&str, &'static str, RichCommand); 6] = [
        (
            "Row below",
            "tbl-row-below",
            RichCommand::InsertTableRow { after: true },
        ),
        (
            "Row above",
            "tbl-row-above",
            RichCommand::InsertTableRow { after: false },
        ),
        ("Delete row", "tbl-row-del", RichCommand::DeleteTableRow),
        (
            "Col right",
            "tbl-col-right",
            RichCommand::InsertTableColumn { after: true },
        ),
        (
            "Col left",
            "tbl-col-left",
            RichCommand::InsertTableColumn { after: false },
        ),
        ("Delete col", "tbl-col-del", RichCommand::DeleteTableColumn),
    ];
    div()
        .id("wysiwyg-table-toolbar")
        .px(px(24.))
        .pt(px(8.))
        .child(
            div()
                .px(px(8.))
                .py(px(4.))
                .rounded_md()
                .border_1()
                .border_color(theme.separator)
                .bg(theme.sidebar_bg)
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(4.))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.secondary_text)
                        .px(px(6.))
                        .child("Table"),
                )
                .children(items.into_iter().map(|(label, id, cmd)| {
                    let editor = editor.clone();
                    let theme = theme.clone();
                    div()
                        .id(id)
                        .px(px(8.))
                        .py(px(3.))
                        .rounded_md()
                        .text_xs()
                        .text_color(theme.text)
                        .cursor(CursorStyle::PointingHand)
                        .hover(move |s| s.bg(theme.sidebar_hover))
                        .child(SharedString::from(label))
                        .on_click(move |_, _, cx| {
                            editor.update(cx, |view, cx| {
                                view.apply_rich(cmd.clone(), cx);
                            });
                        })
                })),
        )
        .into_any_element()
}
