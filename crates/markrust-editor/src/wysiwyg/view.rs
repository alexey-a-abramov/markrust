// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The WYSIWYG editor view: a virtualized list of rendered blocks kept in
//! sync with the document through [`RichEngine`].

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    canvas, div, list, prelude::*, px, App, Bounds, Context, CursorStyle, Entity,
    EntityInputHandler, FocusHandle, Focusable, ListAlignment, ListState, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Render, SharedString, Subscription, Task,
    UTF16Selection, Window,
};
use markrust_core::rich::{
    apply_rich_command, caret_for_click_below_content, place_caret_for_click_below, Bias,
    CaretState, MarkSet, NodeId, RichCommand, RichEngine, RichOutcome,
};
use markrust_core::Document;

use super::block_text::{hit_test_leaf, LeafLayout, OverlayTarget, WidgetOverlay, WysiwygHost};
use super::blocks::{render_top_block, RenderSnapshot};
use super::image::{
    cache_path_for_url, collect_remote_image_urls, default_image_cache_dir, fetch_remote_image,
};
use super::ime::{ImeLeafHit, ImeOriginState};
use crate::headless::{next_word_end, prev_word_start, CaretMove, EditorCommand, EditorOutcome};
use crate::theme::EditorTheme;
use crate::wrap::{wrap_selection, WrapKind};

#[derive(Debug, Clone, Default)]
enum WidgetEdit {
    #[default]
    Idle,
    CodeInfo {
        id: NodeId,
        draft: String,
        caret: usize,
    },
    ImageAlt {
        range: Range<usize>,
        draft: String,
        caret: usize,
    },
    Frontmatter {
        key: &'static str,
        draft: String,
        caret: usize,
    },
    FrontmatterYaml {
        draft: String,
        caret: usize,
    },
}

impl WidgetEdit {
    fn caret(&self) -> usize {
        match self {
            Self::Idle => 0,
            Self::CodeInfo { caret, .. }
            | Self::ImageAlt { caret, .. }
            | Self::Frontmatter { caret, .. }
            | Self::FrontmatterYaml { caret, .. } => *caret,
        }
    }

    fn draft_caret_mut(&mut self) -> Option<(&mut String, &mut usize)> {
        match self {
            Self::Idle => None,
            Self::CodeInfo { draft, caret, .. }
            | Self::ImageAlt { draft, caret, .. }
            | Self::Frontmatter { draft, caret, .. }
            | Self::FrontmatterYaml { draft, caret } => Some((draft, caret)),
        }
    }

    fn draft_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Idle => None,
            Self::CodeInfo { draft, .. }
            | Self::ImageAlt { draft, .. }
            | Self::Frontmatter { draft, .. }
            | Self::FrontmatterYaml { draft, .. } => Some(draft),
        }
    }

    fn allows_newline(&self) -> bool {
        matches!(self, Self::FrontmatterYaml { .. })
    }

    fn set_caret(&mut self, offset: usize) {
        if let Some((draft, caret)) = self.draft_caret_mut() {
            let mut at = offset.min(draft.len());
            if at > 0 && !draft.is_char_boundary(at) {
                at = draft
                    .char_indices()
                    .map(|(i, _)| i)
                    .take_while(|i| *i <= at)
                    .last()
                    .unwrap_or(0);
            }
            *caret = at;
        }
    }

    fn matches_overlay(&self, target: &OverlayTarget) -> bool {
        match (self, target) {
            (Self::CodeInfo { id, .. }, OverlayTarget::CodeInfo(tid)) => id == tid,
            (Self::ImageAlt { range, .. }, OverlayTarget::ImageAlt { range: r, .. }) => range == r,
            (Self::Frontmatter { key, .. }, OverlayTarget::Frontmatter { key: k, .. }) => key == k,
            (Self::FrontmatterYaml { .. }, OverlayTarget::FrontmatterYaml { .. }) => true,
            _ => false,
        }
    }
}

/// Tab / Shift-Tab while a language chip, image caption, or frontmatter
/// overlay is focused commits that overlay. The document body must not run
/// `IndentList` / `OutdentList`.
fn widget_owns_tab(edit: &WidgetEdit) -> bool {
    !matches!(edit, WidgetEdit::Idle)
}

/// Left/Right/Home/End/Up/Down (and Delete) stay inside the overlay; they
/// must not move or mutate the document body. Cmd+A / Undo / Redo use the
/// same gate.
fn widget_owns_caret(edit: &WidgetEdit) -> bool {
    !matches!(edit, WidgetEdit::Idle)
}

/// Cmd/Ctrl+B/I/E/K while a widget overlay is focused must not commit and
/// toggle marks on the document body.
fn widget_owns_wrap(edit: &WidgetEdit) -> bool {
    !matches!(edit, WidgetEdit::Idle)
}

/// Caption and frontmatter are real text buffers; the language chip is not.
fn widget_wraps_draft(edit: &WidgetEdit) -> bool {
    matches!(
        edit,
        WidgetEdit::ImageAlt { .. }
            | WidgetEdit::Frontmatter { .. }
            | WidgetEdit::FrontmatterYaml { .. }
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WidgetWrapResult {
    /// No overlay; wrap the body.
    NotFocused,
    /// Overlay consumed the shortcut without changing the draft (language chip).
    Ignored,
    /// Draft was wrapped (or unwrapped).
    Applied,
}

fn apply_wrap_to_widget(edit: &mut WidgetEdit, kind: WrapKind) -> WidgetWrapResult {
    if matches!(edit, WidgetEdit::Idle) {
        return WidgetWrapResult::NotFocused;
    }
    if !widget_wraps_draft(edit) {
        return WidgetWrapResult::Ignored;
    }
    if let Some((draft, caret)) = edit.draft_caret_mut() {
        let wrapped = wrap_selection(draft, 0..draft.len(), kind);
        *draft = wrapped.text;
        *caret = wrapped.selection.end.min(draft.len());
    }
    WidgetWrapResult::Applied
}

fn widget_range(caret: usize, anchor: usize, len: usize) -> Range<usize> {
    let a = caret.min(anchor).min(len);
    let b = caret.max(anchor).min(len);
    a..b
}

/// Overlay Copy: a non-empty inner selection copies that slice; an empty
/// caret copies the whole draft (language chip / caption / frontmatter).
fn overlay_copy_text(draft: &str, sel: Range<usize>) -> Option<String> {
    let text = if sel.start < sel.end {
        let end = sel.end.min(draft.len());
        let start = sel.start.min(end);
        draft.get(start..end).filter(|s| !s.is_empty())?
    } else if draft.is_empty() {
        return None;
    } else {
        draft
    };
    Some(text.to_string())
}

fn insert_into_widget(edit: &mut WidgetEdit, anchor: &mut usize, text: &str) {
    let Some((draft, caret)) = edit.draft_caret_mut() else {
        return;
    };
    let range = widget_range(*caret, *anchor, draft.len());
    if !draft.is_char_boundary(range.start) || !draft.is_char_boundary(range.end) {
        return;
    }
    let start = range.start;
    draft.replace_range(range, text);
    *caret = start + text.len();
    *anchor = *caret;
}

fn delete_before_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) {
    let Some((draft, caret)) = edit.draft_caret_mut() else {
        return;
    };
    let range = widget_range(*caret, *anchor, draft.len());
    if range.start != range.end {
        if !draft.is_char_boundary(range.start) || !draft.is_char_boundary(range.end) {
            return;
        }
        let start = range.start;
        draft.replace_range(range, "");
        *caret = start;
        *anchor = start;
        return;
    }
    let at = range.start;
    if at == 0 {
        return;
    }
    let prev = draft[..at]
        .chars()
        .next_back()
        .map(|c| c.len_utf8())
        .unwrap_or(0);
    let start = at - prev;
    draft.replace_range(start..at, "");
    *caret = start;
    *anchor = start;
}

fn delete_after_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) {
    let Some((draft, caret)) = edit.draft_caret_mut() else {
        return;
    };
    let range = widget_range(*caret, *anchor, draft.len());
    if range.start != range.end {
        if !draft.is_char_boundary(range.start) || !draft.is_char_boundary(range.end) {
            return;
        }
        let start = range.start;
        draft.replace_range(range, "");
        *caret = start;
        *anchor = start;
        return;
    }
    let at = range.start;
    if at >= draft.len() {
        return;
    }
    let next = draft[at..]
        .chars()
        .next()
        .map(|c| c.len_utf8())
        .unwrap_or(0);
    draft.replace_range(at..at + next, "");
}

/// Cmd+A selects the overlay draft. The document body is not touched.
fn select_all_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) -> bool {
    let Some((draft, caret)) = edit.draft_caret_mut() else {
        return false;
    };
    *anchor = 0;
    *caret = draft.len();
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WidgetDraftSnap {
    draft: String,
    caret: usize,
    anchor: usize,
}

fn widget_snap(edit: &WidgetEdit, anchor: usize) -> Option<WidgetDraftSnap> {
    match edit {
        WidgetEdit::Idle => None,
        WidgetEdit::CodeInfo { draft, caret, .. }
        | WidgetEdit::ImageAlt { draft, caret, .. }
        | WidgetEdit::Frontmatter { draft, caret, .. }
        | WidgetEdit::FrontmatterYaml { draft, caret, .. } => Some(WidgetDraftSnap {
            draft: draft.clone(),
            caret: *caret,
            anchor,
        }),
    }
}

fn apply_widget_snap(edit: &mut WidgetEdit, snap: &WidgetDraftSnap, anchor: &mut usize) {
    if let Some((draft, caret)) = edit.draft_caret_mut() {
        *draft = snap.draft.clone();
        let mut at = snap.caret.min(draft.len());
        if at > 0 && !draft.is_char_boundary(at) {
            at = draft.len();
        }
        *caret = at;
    }
    *anchor = snap.anchor.min(snap.draft.len());
}

const WIDGET_UNDO_LIMIT: usize = 64;

fn push_widget_history(
    edit: &WidgetEdit,
    anchor: usize,
    undo: &mut Vec<WidgetDraftSnap>,
    redo: &mut Vec<WidgetDraftSnap>,
) {
    let Some(snap) = widget_snap(edit, anchor) else {
        return;
    };
    if undo.len() >= WIDGET_UNDO_LIMIT {
        undo.remove(0);
    }
    undo.push(snap);
    redo.clear();
}

fn undo_widget_history(
    edit: &mut WidgetEdit,
    anchor: &mut usize,
    undo: &mut Vec<WidgetDraftSnap>,
    redo: &mut Vec<WidgetDraftSnap>,
) -> bool {
    let Some(prev) = undo.pop() else {
        return false;
    };
    if let Some(current) = widget_snap(edit, *anchor) {
        redo.push(current);
    }
    apply_widget_snap(edit, &prev, anchor);
    true
}

fn redo_widget_history(
    edit: &mut WidgetEdit,
    anchor: &mut usize,
    undo: &mut Vec<WidgetDraftSnap>,
    redo: &mut Vec<WidgetDraftSnap>,
) -> bool {
    let Some(next) = redo.pop() else {
        return false;
    };
    if let Some(current) = widget_snap(edit, *anchor) {
        if undo.len() >= WIDGET_UNDO_LIMIT {
            undo.remove(0);
        }
        undo.push(current);
    }
    apply_widget_snap(edit, &next, anchor);
    true
}

/// Byte offset on the previous/next `\n` line, same character column.
fn vertical_in_draft(draft: &str, at: usize, down: bool) -> usize {
    let mut at = at.min(draft.len());
    if at > 0 && !draft.is_char_boundary(at) {
        at = draft
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|i| *i <= at)
            .last()
            .unwrap_or(0);
    }
    let line_start = draft[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col = draft[line_start..at].chars().count();
    let (dest_start, dest_end) = if down {
        let line_end = draft[at..]
            .find('\n')
            .map(|i| at + i)
            .unwrap_or(draft.len());
        if line_end >= draft.len() {
            return at;
        }
        let next_start = line_end + 1;
        let next_end = draft[next_start..]
            .find('\n')
            .map(|i| next_start + i)
            .unwrap_or(draft.len());
        (next_start, next_end)
    } else if line_start == 0 {
        return at;
    } else {
        let prev_end = line_start - 1;
        let prev_start = draft[..prev_end].rfind('\n').map(|i| i + 1).unwrap_or(0);
        (prev_start, prev_end)
    };
    let line = &draft[dest_start..dest_end];
    for (n, (i, _)) in line.char_indices().enumerate() {
        if n == col {
            return dest_start + i;
        }
    }
    dest_end
}

/// Byte offset of the previous char boundary (or the preceding boundary when
/// `at` is already inside a multi-byte char).
fn prev_char_boundary(draft: &str, at: usize) -> usize {
    let at = at.min(draft.len());
    if at == 0 {
        return 0;
    }
    if draft.is_char_boundary(at) {
        draft[..at]
            .chars()
            .next_back()
            .map(|c| at - c.len_utf8())
            .unwrap_or(0)
    } else {
        draft
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|i| *i < at)
            .last()
            .unwrap_or(0)
    }
}

/// Byte offset of the next char boundary (or the following boundary when
/// `at` is already inside a multi-byte char).
fn next_char_boundary(draft: &str, at: usize) -> usize {
    let at = at.min(draft.len());
    if at >= draft.len() {
        return draft.len();
    }
    if draft.is_char_boundary(at) {
        draft[at..]
            .chars()
            .next()
            .map(|c| at + c.len_utf8())
            .unwrap_or(at)
    } else {
        draft
            .char_indices()
            .map(|(i, _)| i)
            .skip_while(|i| *i <= at)
            .next()
            .unwrap_or(draft.len())
    }
}

/// Move or extend the overlay caret. Returns false when no overlay is
/// focused so the body caret can run. The body selection is never touched.
fn move_in_widget(
    edit: &mut WidgetEdit,
    anchor: &mut usize,
    movement: CaretMove,
    extend: bool,
) -> bool {
    if matches!(edit, WidgetEdit::Idle) {
        return false;
    }
    let Some((draft, caret)) = edit.draft_caret_mut() else {
        return false;
    };
    let has_sel = *caret != *anchor;
    let at = (*caret).min(draft.len());
    let target = match movement {
        CaretMove::Left => {
            if !extend && has_sel {
                at.min(*anchor)
            } else {
                prev_char_boundary(draft, at)
            }
        }
        CaretMove::Right => {
            if !extend && has_sel {
                at.max(*anchor)
            } else {
                next_char_boundary(draft, at)
            }
        }
        CaretMove::Home => draft[..at].rfind('\n').map(|i| i + 1).unwrap_or(0),
        CaretMove::End => draft[at..]
            .find('\n')
            .map(|i| at + i)
            .unwrap_or(draft.len()),
        CaretMove::WordLeft => prev_word_start(draft, at),
        CaretMove::WordRight => next_word_end(draft, at),
        CaretMove::DocumentHome => 0,
        CaretMove::DocumentEnd => draft.len(),
        CaretMove::Up => vertical_in_draft(draft, at, false),
        CaretMove::Down => vertical_in_draft(draft, at, true),
        CaretMove::Vertical { delta_lines } => {
            let mut pos = at;
            if delta_lines > 0 {
                for _ in 0..delta_lines {
                    pos = vertical_in_draft(draft, pos, true);
                }
            } else {
                for _ in 0..delta_lines.unsigned_abs() {
                    pos = vertical_in_draft(draft, pos, false);
                }
            }
            pos
        }
    };
    *caret = target.min(draft.len());
    if !extend {
        *anchor = *caret;
    }
    true
}

fn wrap_kind_from_rich(command: &RichCommand) -> Option<WrapKind> {
    match command {
        RichCommand::ToggleMark(mark) if *mark == MarkSet::BOLD => Some(WrapKind::Bold),
        RichCommand::ToggleMark(mark) if *mark == MarkSet::ITALIC => Some(WrapKind::Italic),
        RichCommand::ToggleMark(mark) if *mark == MarkSet::CODE => Some(WrapKind::Code),
        RichCommand::ToggleLink => Some(WrapKind::Link),
        _ => None,
    }
}

/// Keep `anchor` and move the caret to `target` (Shift-arrows / Shift-Home).
fn extend_selection_range(
    selected: Range<usize>,
    reversed: bool,
    target: usize,
) -> (Range<usize>, bool) {
    let anchor = if reversed {
        selected.end
    } else {
        selected.start
    };
    if target < anchor {
        (target..anchor, true)
    } else {
        (anchor..target, false)
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
    widget_selecting: bool,
    widget_anchor: usize,
    cursor_visible: bool,
    focus_handle: FocusHandle,
    ime: ImeOriginState,
    widget_edit: WidgetEdit,
    widget_preedit: Option<String>,
    widget_undo: Vec<WidgetDraftSnap>,
    widget_redo: Vec<WidgetDraftSnap>,
    remote_pending: HashSet<String>,
    remote_failed: HashSet<String>,
    _blink_task: Task<()>,
    _remote_fetch_tasks: Vec<Task<()>>,
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
            widget_selecting: false,
            widget_anchor: 0,
            cursor_visible: true,
            focus_handle,
            ime: ImeOriginState::default(),
            widget_edit: WidgetEdit::Idle,
            widget_preedit: None,
            widget_undo: Vec::new(),
            widget_redo: Vec::new(),
            remote_pending: HashSet::new(),
            remote_failed: HashSet::new(),
            _blink_task: Task::ready(()),
            _remote_fetch_tasks: Vec::new(),
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
        } else if let Some(outcome) = self.widget_try_wrap(&command, cx) {
            return outcome;
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
                if self.widget_delete(cx) {
                    EditorOutcome::Changed
                } else if self.apply_rich(RichCommand::Delete, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::DeleteWordLeft => {
                if self.apply_rich(RichCommand::DeleteWordLeft, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::DeleteWordRight => {
                if self.apply_rich(RichCommand::DeleteWordRight, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::DeleteToLineStart => {
                if self.apply_rich(RichCommand::DeleteToLineStart, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::DeleteToLineEnd => {
                if self.apply_rich(RichCommand::DeleteToLineEnd, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::Undo => {
                if widget_owns_caret(&self.widget_edit) {
                    self.undo_widget(cx)
                } else {
                    self.undo(cx)
                }
            }
            EditorCommand::Redo => {
                if widget_owns_caret(&self.widget_edit) {
                    self.redo_widget(cx)
                } else {
                    self.redo(cx)
                }
            }
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
                if select_all_in_widget(&mut self.widget_edit, &mut self.widget_anchor) {
                    self.reset_blink(cx);
                    self.snapshot = None;
                    cx.notify();
                    EditorOutcome::CaretMoved
                } else {
                    let len = self.document.read(cx).buffer.len_bytes();
                    self.selected_range = 0..len;
                    self.selection_reversed = false;
                    cx.notify();
                    EditorOutcome::CaretMoved
                }
            }
            EditorCommand::Move(movement) => {
                if move_in_widget(
                    &mut self.widget_edit,
                    &mut self.widget_anchor,
                    movement,
                    false,
                ) {
                    self.reset_blink(cx);
                    self.snapshot = None;
                    cx.notify();
                    EditorOutcome::CaretMoved
                } else {
                    self.move_caret(movement, false, cx);
                    EditorOutcome::CaretMoved
                }
            }
            EditorCommand::Select(movement) => {
                if move_in_widget(
                    &mut self.widget_edit,
                    &mut self.widget_anchor,
                    movement,
                    true,
                ) {
                    self.reset_blink(cx);
                    self.snapshot = None;
                    cx.notify();
                    EditorOutcome::CaretMoved
                } else {
                    self.move_caret(movement, true, cx);
                    EditorOutcome::CaretMoved
                }
            }
            EditorCommand::Wrap(kind) => {
                if widget_wraps_draft(&self.widget_edit) {
                    self.record_widget_edit();
                }
                match apply_wrap_to_widget(&mut self.widget_edit, kind) {
                    WidgetWrapResult::NotFocused => {
                        let cmd = match kind {
                            WrapKind::Bold => RichCommand::ToggleMark(MarkSet::BOLD),
                            WrapKind::Italic => RichCommand::ToggleMark(MarkSet::ITALIC),
                            WrapKind::Code => RichCommand::ToggleMark(MarkSet::CODE),
                            WrapKind::Link => RichCommand::ToggleLink,
                        };
                        if self.apply_rich(cmd, cx) == RichOutcome::Noop {
                            EditorOutcome::Noop
                        } else {
                            EditorOutcome::Changed
                        }
                    }
                    WidgetWrapResult::Ignored => EditorOutcome::Noop,
                    WidgetWrapResult::Applied => {
                        self.widget_preedit = None;
                        self.widget_anchor = self.widget_edit.caret();
                        self.snapshot = None;
                        cx.notify();
                        EditorOutcome::Changed
                    }
                }
            }
            EditorCommand::Indent => {
                // Widget overlays own Tab: commit, do not indent the body.
                if widget_owns_tab(&self.widget_edit) {
                    self.commit_widget_edit(cx);
                    EditorOutcome::Changed
                } else {
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
            }
            EditorCommand::Outdent => {
                if widget_owns_tab(&self.widget_edit) {
                    self.commit_widget_edit(cx);
                    EditorOutcome::Changed
                } else {
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

    fn undo_widget(&mut self, cx: &mut Context<Self>) -> EditorOutcome {
        if undo_widget_history(
            &mut self.widget_edit,
            &mut self.widget_anchor,
            &mut self.widget_undo,
            &mut self.widget_redo,
        ) {
            self.widget_preedit = None;
            self.reset_blink(cx);
            self.snapshot = None;
            cx.notify();
            EditorOutcome::Changed
        } else {
            EditorOutcome::Noop
        }
    }

    fn redo_widget(&mut self, cx: &mut Context<Self>) -> EditorOutcome {
        if redo_widget_history(
            &mut self.widget_edit,
            &mut self.widget_anchor,
            &mut self.widget_undo,
            &mut self.widget_redo,
        ) {
            self.widget_preedit = None;
            self.reset_blink(cx);
            self.snapshot = None;
            cx.notify();
            EditorOutcome::Changed
        } else {
            EditorOutcome::Noop
        }
    }

    fn record_widget_edit(&mut self) {
        push_widget_history(
            &self.widget_edit,
            self.widget_anchor,
            &mut self.widget_undo,
            &mut self.widget_redo,
        );
    }

    fn clear_widget_history(&mut self) {
        self.widget_undo.clear();
        self.widget_redo.clear();
    }

    fn move_to(&mut self, offset: usize, extend: bool, cx: &mut Context<Self>) {
        // While an overlay is editing, keep jumps inside the draft.
        if !matches!(self.widget_edit, WidgetEdit::Idle) {
            self.widget_edit.set_caret(offset);
            if !extend {
                self.widget_anchor = self.widget_edit.caret();
            }
            self.reset_blink(cx);
            cx.notify();
            return;
        }
        let len = self.document.read(cx).buffer.len_bytes();
        let source = self.document.read(cx).buffer.content();
        self.engine.sync(self.document.read(cx));
        let offset = self.engine.snap_caret(offset.min(len), Bias::Left);
        let offset = self.engine.clamp_raw_prefix(&source, offset, Bias::Left);
        if extend {
            let (range, reversed) = extend_selection_range(
                self.selected_range.clone(),
                self.selection_reversed,
                offset,
            );
            self.selected_range = range;
            self.selection_reversed = reversed;
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
            CaretMove::WordLeft => self.engine.prev_word_caret(&source, cursor),
            CaretMove::WordRight => self.engine.next_word_caret(&source, cursor),
            CaretMove::DocumentHome => self.engine.clamp_raw_prefix(
                &source,
                self.engine.snap_caret(0, Bias::Right),
                Bias::Right,
            ),
            CaretMove::DocumentEnd => self.engine.clamp_raw_prefix(
                &source,
                self.engine.snap_caret(source.len(), Bias::Left),
                Bias::Left,
            ),
            CaretMove::Up => self.engine.vertical_caret(&source, cursor, -1),
            CaretMove::Down => self.engine.vertical_caret(&source, cursor, 1),
            CaretMove::Vertical { delta_lines } => {
                self.engine.vertical_caret(&source, cursor, delta_lines)
            }
        };
        self.move_to(target, extend, cx);
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
        let revision = self.document.read(cx).revision();
        let caret = self.cursor_offset();
        let selected_range = self.selected_range.clone();
        if self.synced_revision == Some(revision) {
            if let Some(snapshot) = &self.snapshot {
                if snapshot.caret == caret && snapshot.selected_range == selected_range {
                    return snapshot.clone();
                }
                let mut next = (**snapshot).clone();
                next.caret = caret;
                next.selected_range = selected_range;
                let snapshot = Arc::new(next);
                self.snapshot = Some(snapshot.clone());
                return snapshot;
            }
        }
        let widget_only = self.synced_revision == Some(revision) && self.snapshot.is_none();
        let (base_dir, source, old_count) = {
            let doc = self.document.read(cx);
            let base_dir = doc
                .path
                .as_ref()
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf());
            let source = doc.buffer.content();
            let old_count = self.engine.tree().blocks.len();
            self.engine.sync(doc);
            (base_dir, source, old_count)
        };
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
        self.enqueue_remote_images(cx);
        let snapshot = Arc::new(RenderSnapshot {
            tree: self.engine.tree().clone(),
            source,
            theme: self.theme.clone(),
            base_dir,
            editing_code: match &self.widget_edit {
                WidgetEdit::CodeInfo { id, draft, .. } => Some((*id, draft.clone())),
                _ => None,
            },
            editing_image: match &self.widget_edit {
                WidgetEdit::ImageAlt { range, draft, .. } => Some((range.clone(), draft.clone())),
                _ => None,
            },
            caret,
            selected_range,
        });
        self.snapshot = Some(snapshot.clone());
        self.synced_revision = Some(revision);
        snapshot
    }

    fn enqueue_remote_images(&mut self, cx: &mut Context<Self>) {
        let cache_dir = default_image_cache_dir();
        for url in collect_remote_image_urls(self.engine.tree()) {
            if self.remote_pending.contains(&url) || self.remote_failed.contains(&url) {
                continue;
            }
            let dest = cache_path_for_url(&cache_dir, &url);
            if dest.is_file() {
                continue;
            }
            self.remote_pending.insert(url.clone());
            let url_fetch = url.clone();
            let url_status = url;
            let dest_fetch = dest;
            let task = cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { fetch_remote_image(&url_fetch, &dest_fetch) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.remote_pending.remove(&url_status);
                    if result.is_err() {
                        this.remote_failed.insert(url_status);
                    }
                    this.snapshot = None;
                    cx.notify();
                });
            });
            self._remote_fetch_tasks.push(task);
        }
    }

    fn offset_from_utf16(content: &str, offset: usize) -> usize {
        super::ime::offset_from_utf16(content, offset)
    }

    fn offset_to_utf16(content: &str, offset: usize) -> usize {
        super::ime::offset_to_utf16(content, offset)
    }

    fn widget_try_wrap(
        &mut self,
        command: &RichCommand,
        cx: &mut Context<Self>,
    ) -> Option<RichOutcome> {
        if !widget_owns_wrap(&self.widget_edit) {
            return None;
        }
        if !matches!(
            command,
            RichCommand::ToggleMark(_) | RichCommand::ToggleLink
        ) {
            return None;
        }
        let Some(kind) = wrap_kind_from_rich(command) else {
            // Other marks (strike, …): consume so the body is not rewritten.
            return Some(RichOutcome::Noop);
        };
        if widget_wraps_draft(&self.widget_edit) {
            self.record_widget_edit();
        }
        match apply_wrap_to_widget(&mut self.widget_edit, kind) {
            WidgetWrapResult::NotFocused => None,
            WidgetWrapResult::Ignored => Some(RichOutcome::Noop),
            WidgetWrapResult::Applied => {
                self.widget_preedit = None;
                self.widget_anchor = self.widget_edit.caret();
                self.snapshot = None;
                cx.notify();
                Some(RichOutcome::Changed)
            }
        }
    }

    fn widget_insert(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        if matches!(self.widget_edit, WidgetEdit::Idle) {
            return false;
        }
        if text.contains('\n') && !self.widget_edit.allows_newline() {
            self.commit_widget_edit(cx);
            return true;
        }
        self.record_widget_edit();
        self.widget_preedit = None;
        insert_into_widget(&mut self.widget_edit, &mut self.widget_anchor, text);
        self.snapshot = None;
        cx.notify();
        true
    }

    fn widget_backspace(&mut self, cx: &mut Context<Self>) -> bool {
        if matches!(self.widget_edit, WidgetEdit::Idle) {
            return false;
        }
        if let Some(snap) = widget_snap(&self.widget_edit, self.widget_anchor) {
            let range = widget_range(snap.caret, snap.anchor, snap.draft.len());
            if range.start == range.end && range.start == 0 {
                return true;
            }
        }
        self.record_widget_edit();
        self.widget_preedit = None;
        delete_before_in_widget(&mut self.widget_edit, &mut self.widget_anchor);
        self.snapshot = None;
        cx.notify();
        true
    }

    fn widget_delete(&mut self, cx: &mut Context<Self>) -> bool {
        if !widget_owns_caret(&self.widget_edit) {
            return false;
        }
        if let Some(snap) = widget_snap(&self.widget_edit, self.widget_anchor) {
            let range = widget_range(snap.caret, snap.anchor, snap.draft.len());
            if range.start == range.end && range.end == snap.draft.len() {
                return true;
            }
        }
        self.record_widget_edit();
        self.widget_preedit = None;
        delete_after_in_widget(&mut self.widget_edit, &mut self.widget_anchor);
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
            | WidgetEdit::FrontmatterYaml { draft, .. } => draft.clone(),
        };
        Some((draft, self.widget_preedit.clone()))
    }

    fn cancel_widget_edit(&mut self, cx: &mut Context<Self>) -> bool {
        if matches!(self.widget_edit, WidgetEdit::Idle) {
            return false;
        }
        self.widget_edit = WidgetEdit::Idle;
        self.widget_preedit = None;
        self.widget_selecting = false;
        self.clear_widget_history();
        self.snapshot = None;
        cx.notify();
        true
    }

    fn commit_widget_edit(&mut self, cx: &mut Context<Self>) -> bool {
        self.widget_preedit = None;
        self.widget_selecting = false;
        self.clear_widget_history();
        let edit = std::mem::take(&mut self.widget_edit);
        match edit {
            WidgetEdit::Idle => false,
            WidgetEdit::CodeInfo { id, draft, .. } => {
                self.apply_rich(RichCommand::SetCodeInfo { id, info: draft }, cx);
                true
            }
            WidgetEdit::ImageAlt { range, draft, .. } => {
                self.apply_rich(
                    RichCommand::SetImageAlt {
                        source_range: range,
                        alt: draft,
                    },
                    cx,
                );
                true
            }
            WidgetEdit::Frontmatter { key, draft, .. } => {
                self.apply_rich(
                    RichCommand::SetFrontmatterField {
                        key: key.to_string(),
                        value: draft,
                    },
                    cx,
                );
                true
            }
            WidgetEdit::FrontmatterYaml { draft, .. } => {
                self.apply_rich(RichCommand::SetFrontmatter { raw: draft }, cx);
                true
            }
        }
    }

    /// Enter / Shift-Enter in YAML inserts a draft newline; other overlays commit.
    /// Returns true when the widget consumed the key (do not run a body command).
    fn consume_widget_newline(&mut self, cx: &mut Context<Self>) -> bool {
        if matches!(self.widget_edit, WidgetEdit::FrontmatterYaml { .. }) {
            self.widget_insert("\n", cx)
        } else {
            self.commit_widget_edit(cx)
        }
    }
    /// Click in leftover viewport below the last painted leaf or non-text
    /// widget (or anywhere on an unpainted / newlines-only document). Opens a
    /// trailing blank if the file has none, then places the caret there —
    /// never a no-op and never a hit-test onto the last paragraph.
    fn click_below_painted_content(
        &mut self,
        point: gpui::Point<Pixels>,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.ime.point_is_below_painted_content(point) {
            return false;
        }
        self.commit_widget_edit(cx);
        self.engine.sync(self.document.read(cx));
        if !extend {
            let mut caret = self.caret_state();
            let mut outcome = RichOutcome::Noop;
            self.document.update(cx, |doc, cx| {
                outcome = place_caret_for_click_below(doc, &mut self.engine, &mut caret);
                if outcome != RichOutcome::Noop {
                    cx.notify();
                }
            });
            self.restore_caret(caret);
            if outcome != RichOutcome::Noop {
                self.reset_blink(cx);
                self.snapshot = None;
                self.synced_revision = None;
            }
        }
        let source = caret_for_click_below_content(self.engine.tree());
        self.click_source(source, extend, window, cx);
        true
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
        let caret = draft.len();
        self.widget_edit = WidgetEdit::CodeInfo { id, draft, caret };
        self.widget_anchor = caret;
        self.widget_selecting = false;
        self.widget_preedit = None;
        self.snapshot = None;
        cx.notify();
    }

    fn edit_image_alt(&mut self, source_range: Range<usize>, alt: &str, cx: &mut Context<Self>) {
        self.commit_widget_edit(cx);
        let caret = alt.len();
        self.widget_edit = WidgetEdit::ImageAlt {
            range: source_range,
            draft: alt.to_string(),
            caret,
        };
        self.widget_anchor = caret;
        self.widget_selecting = false;
        self.widget_preedit = None;
        self.snapshot = None;
        cx.notify();
    }

    fn edit_frontmatter_field(&mut self, key: &'static str, current: &str, cx: &mut Context<Self>) {
        self.commit_widget_edit(cx);
        let caret = current.len();
        self.widget_edit = WidgetEdit::Frontmatter {
            key,
            draft: current.to_string(),
            caret,
        };
        self.widget_anchor = caret;
        self.widget_selecting = false;
        self.widget_preedit = None;
        self.snapshot = None;
        cx.notify();
    }

    fn edit_frontmatter_yaml(&mut self, current: &str, cx: &mut Context<Self>) {
        self.commit_widget_edit(cx);
        let caret = current.len();
        self.widget_edit = WidgetEdit::FrontmatterYaml {
            draft: current.to_string(),
            caret,
        };
        self.widget_anchor = caret;
        self.widget_selecting = false;
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

    fn report_widget_caret(&mut self, caret: Bounds<Pixels>) {
        self.ime.report_widget_caret(caret);
    }

    fn overlay_preedit(&self) -> Option<&str> {
        self.widget_preedit.as_deref()
    }

    fn widget_caret_offset(&self) -> usize {
        self.widget_edit.caret()
    }

    fn widget_sel(&self) -> Range<usize> {
        let c = self.widget_edit.caret();
        let a = self.widget_anchor.min(c);
        let b = self.widget_anchor.max(c);
        a..b
    }

    fn is_widget_selecting(&self) -> bool {
        self.widget_selecting
    }

    fn click_overlay(
        &mut self,
        target: OverlayTarget,
        offset: usize,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        if !self.widget_edit.matches_overlay(&target) {
            match &target {
                OverlayTarget::CodeInfo(id) => self.edit_code_info(*id, cx),
                OverlayTarget::ImageAlt { range, stored } => {
                    self.edit_image_alt(range.clone(), stored, cx);
                }
                OverlayTarget::Frontmatter { key, stored } => {
                    self.edit_frontmatter_field(key, stored, cx);
                }
                OverlayTarget::FrontmatterYaml { stored } => {
                    self.edit_frontmatter_yaml(stored, cx);
                }
            }
        }
        let at = offset;
        if extend {
            self.widget_edit.set_caret(at);
        } else {
            self.widget_edit.set_caret(at);
            self.widget_anchor = self.widget_edit.caret();
        }
        self.widget_selecting = true;
        self.snapshot = None;
        cx.notify();
    }

    fn drag_overlay(&mut self, offset: usize, cx: &mut Context<Self>) {
        if !self.widget_selecting {
            return;
        }
        self.widget_edit.set_caret(offset);
        self.snapshot = None;
        cx.notify();
    }

    fn end_overlay_drag(&mut self, _cx: &mut Context<Self>) {
        self.widget_selecting = false;
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

    fn report_painted_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.ime.report_painted_bounds(bounds);
    }

    fn sync_ime_cursor(&mut self, window: &mut Window) {
        if self.ime.take_platform_push().is_some() {
            window.invalidate_character_coordinates();
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
            return Some(super::ime::widget_selected_text_range(
                &draft,
                preedit.as_deref(),
            ));
        }
        let content = self.document.read(cx).buffer.content();
        Some(super::ime::body_selected_text_range(
            &content,
            self.selected_range.clone(),
            self.selection_reversed,
        ))
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range.clone()
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        super::ime::clear_composition(
            &mut self.preedit,
            &mut self.widget_preedit,
            &mut self.marked_range,
        );
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
                super::ime::replace_in_widget_draft(draft, range_utf16, new_text);
            }
            self.snapshot = None;
            cx.notify();
            return;
        }
        let content = self.document.read(cx).buffer.content();
        super::ime::apply_replace_range_to_selection(
            &content,
            range_utf16,
            &mut self.marked_range,
            &mut self.selected_range,
            &mut self.selection_reversed,
        );
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
            super::ime::set_preedit(&mut self.widget_preedit, new_text);
            self.snapshot = None;
            cx.notify();
            return;
        }
        // Preedit is display-only; the model is untouched until commit.
        let caret = self.cursor_offset();
        super::ime::begin_body_preedit(&mut self.preedit, &mut self.marked_range, new_text, caret);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(super::ime::ime_origin_bounds(&self.ime, bounds))
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
        let leaf = self.ime.leaf_at_point(point);
        if let Some(leaf) = leaf {
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
            return Some(Self::offset_to_utf16(&content, src));
        }
        if self.ime.point_is_below_painted_content(point) {
            self.engine.sync(self.document.read(cx));
            let src = caret_for_click_below_content(self.engine.tree());
            let content = self.document.read(cx).buffer.content();
            return Some(Self::offset_to_utf16(&content, src));
        }
        None
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
            WidgetEdit::Frontmatter { key, draft, .. } => Some((*key, draft.clone())),
            _ => None,
        };
        let editing_yaml = match &self.widget_edit {
            WidgetEdit::FrontmatterYaml { draft, .. } => Some(draft.clone()),
            _ => None,
        };
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
                move |_: &crate::editor::DeleteWordLeft, window, cx| {
                    editor.update(cx, |e, cx| {
                        if e.apply_editor_command(EditorCommand::DeleteWordLeft, cx)
                            == EditorOutcome::Noop
                        {
                            window.play_system_bell();
                        }
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::DeleteWordRight, window, cx| {
                    editor.update(cx, |e, cx| {
                        if e.apply_editor_command(EditorCommand::DeleteWordRight, cx)
                            == EditorOutcome::Noop
                        {
                            window.play_system_bell();
                        }
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::DeleteToLineStart, window, cx| {
                    editor.update(cx, |e, cx| {
                        if e.apply_editor_command(EditorCommand::DeleteToLineStart, cx)
                            == EditorOutcome::Noop
                        {
                            window.play_system_bell();
                        }
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::DeleteToLineEnd, window, cx| {
                    editor.update(cx, |e, cx| {
                        if e.apply_editor_command(EditorCommand::DeleteToLineEnd, cx)
                            == EditorOutcome::Noop
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
                move |_: &crate::editor::SelectUp, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::Up), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectDown, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::Down), cx);
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
                move |_: &crate::editor::SelectHome, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::Home), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectEnd, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::End), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::PageUp, window, cx| {
                    editor.update(cx, |e, cx| {
                        let lines =
                            (window.bounds().size.height / window.line_height()).floor() as i32;
                        e.apply_editor_command(
                            EditorCommand::Move(CaretMove::Vertical {
                                delta_lines: -lines.max(1),
                            }),
                            cx,
                        );
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::PageDown, window, cx| {
                    editor.update(cx, |e, cx| {
                        let lines =
                            (window.bounds().size.height / window.line_height()).floor() as i32;
                        e.apply_editor_command(
                            EditorCommand::Move(CaretMove::Vertical {
                                delta_lines: lines.max(1),
                            }),
                            cx,
                        );
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectPageUp, window, cx| {
                    editor.update(cx, |e, cx| {
                        let lines =
                            (window.bounds().size.height / window.line_height()).floor() as i32;
                        e.apply_editor_command(
                            EditorCommand::Select(CaretMove::Vertical {
                                delta_lines: -lines.max(1),
                            }),
                            cx,
                        );
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectPageDown, window, cx| {
                    editor.update(cx, |e, cx| {
                        let lines =
                            (window.bounds().size.height / window.line_height()).floor() as i32;
                        e.apply_editor_command(
                            EditorCommand::Select(CaretMove::Vertical {
                                delta_lines: lines.max(1),
                            }),
                            cx,
                        );
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::WordLeft, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::WordLeft), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::WordRight, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::WordRight), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectWordLeft, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::WordLeft), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectWordRight, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::WordRight), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::DocumentHome, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::DocumentHome), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::DocumentEnd, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::DocumentEnd), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectDocumentHome, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::DocumentHome), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectDocumentEnd, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::DocumentEnd), cx);
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
                        if !e.consume_widget_newline(cx) {
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
                        e.apply_editor_command(EditorCommand::Indent, cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Outdent, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Outdent, cx);
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
                )
            }))
            .when(in_table, |root| {
                root.child(table_toolbar(editor.clone(), &theme))
            })
            .child({
                let editor = editor.clone();
                let catcher = editor.clone();
                div()
                    .id("wysiwyg-body")
                    .flex_1()
                    .size_full()
                    .relative()
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, _cx| {
                                let editor = catcher.clone();
                                window.on_mouse_event({
                                    let editor = editor.clone();
                                    move |event: &MouseDownEvent, phase, window, cx| {
                                        if !phase.bubble() || event.button != MouseButton::Left {
                                            return;
                                        }
                                        if !bounds.contains(&event.position) {
                                            return;
                                        }
                                        let placed = editor.update(cx, |view, cx| {
                                            view.click_below_painted_content(
                                                event.position,
                                                event.modifiers.shift,
                                                window,
                                                cx,
                                            )
                                        });
                                        if placed {
                                            window.prevent_default();
                                        }
                                    }
                                });
                                window.on_mouse_event({
                                    let editor = editor.clone();
                                    move |event: &MouseMoveEvent, phase, _window, cx| {
                                        if !phase.bubble() {
                                            return;
                                        }
                                        if !bounds.contains(&event.position) {
                                            return;
                                        }
                                        editor.update(cx, |view, cx| {
                                            if !view.is_selecting {
                                                return;
                                            }
                                            if !event
                                                .pressed_button
                                                .is_some_and(|b| b == MouseButton::Left)
                                            {
                                                return;
                                            }
                                            if !view
                                                .ime
                                                .point_is_below_painted_content(event.position)
                                            {
                                                return;
                                            }
                                            view.engine.sync(view.document.read(cx));
                                            let source =
                                                caret_for_click_below_content(view.engine.tree());
                                            view.drag_source(source, cx);
                                        });
                                    }
                                });
                            },
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .child(
                        list(self.list_state.clone(), move |index, _window, _cx| {
                            render_top_block(&snapshot, index, editor.clone())
                        })
                        .flex_1()
                        .size_full()
                        .py(px(16.)),
                    )
            })
    }
}

fn frontmatter_panel(
    editor: gpui::Entity<RichEditorView>,
    theme: &crate::theme::EditorTheme,
    info: &markrust_core::FrontmatterInfo,
    editing_fm: Option<(&'static str, &str)>,
    editing_yaml: Option<&str>,
) -> gpui::AnyElement {
    let field = |key: &'static str, placeholder: &str, stored: Option<&str>| -> String {
        match editing_fm {
            Some((k, draft)) if k == key => draft.to_string(),
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
        Some(draft) => draft.to_string(),
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
    let editor_away = editor.clone();
    let font_size = theme.font_size * 0.875;
    let line_height = theme.line_height_for_font_size(font_size);
    div()
        .id(id)
        .relative()
        .cursor(CursorStyle::PointingHand)
        .when(editing, |el| {
            el.border_b_1()
                .border_color(theme.accent)
                .on_mouse_down_out(move |_, _, cx| {
                    editor_away.update(cx, |host, cx| host.finish_widget(cx));
                })
        })
        .child(WidgetOverlay {
            editor,
            prefix: format!("{label}: "),
            text: display,
            editing,
            font_size,
            line_height,
            theme: theme.clone(),
            color,
            italic: false,
            monospace: false,
            hug_width: false,
            target: OverlayTarget::Frontmatter {
                key,
                stored: current,
            },
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
    let editor_away = editor.clone();
    let font_size = theme.font_size * 0.75;
    let line_height = theme.line_height_for_font_size(font_size);
    div()
        .id("fm-yaml")
        .relative()
        .mt(px(4.))
        .cursor(CursorStyle::PointingHand)
        .when(editing, |el| {
            el.border_b_1()
                .border_color(theme.accent)
                .on_mouse_down_out(move |_, _, cx| {
                    editor_away.update(cx, |host, cx| host.finish_widget(cx));
                })
        })
        .child(
            div()
                .text_xs()
                .text_color(theme.secondary_text)
                .font_family(theme.font_family.clone())
                .child("YAML"),
        )
        .child(WidgetOverlay {
            editor,
            prefix: String::new(),
            text: display,
            editing,
            font_size,
            line_height,
            theme: theme.clone(),
            color: theme.frontmatter_text,
            italic: false,
            monospace: true,
            hug_width: false,
            target: OverlayTarget::FrontmatterYaml { stored: current },
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
