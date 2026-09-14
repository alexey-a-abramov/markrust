// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The WYSIWYG editor view: a virtualized list of rendered blocks kept in
//! sync with the document through [`RichEngine`].

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use unicode_segmentation::UnicodeSegmentation;

use gpui::{
    canvas, div, list, prelude::*, px, App, Bounds, ClipboardItem, Context, CursorStyle, Entity,
    EntityInputHandler, FocusHandle, Focusable, ListAlignment, ListState, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Render, SharedString, Subscription, Task,
    UTF16Selection, Window,
};
use markrust_core::rich::{
    apply_rich_command, caret_for_click_below_content, place_caret_for_click_below,
    table_select_all_range, Bias, CaretState, MarkSet, NodeId, RichCommand, RichEngine,
    RichOutcome,
};
use markrust_core::Document;

use super::block_text::{hit_test_leaf, LeafLayout, OverlayTarget, WidgetOverlay, WysiwygHost};
use super::blocks::{render_top_block, RenderSnapshot};
use super::image::{
    cache_path_for_url, collect_data_image_urls, collect_local_image_paths,
    collect_remote_image_urls, default_image_cache_dir, fetch_remote_image,
    materialize_safe_data_image, materialize_safe_local_image,
};
use super::ime::{ImeLeafHit, ImeOriginState, VisualLine};
use crate::headless::{
    next_boundary, next_word_end, prev_word_start, previous_boundary, CaretMove, EditorCommand,
    EditorOutcome,
};
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

    fn allows_newline(&self) -> bool {
        matches!(self, Self::FrontmatterYaml { .. })
    }

    fn set_caret(&mut self, offset: usize) {
        if let Some((draft, caret)) = self.draft_caret_mut() {
            *caret = clamp_grapheme_boundary(draft, offset);
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

/// Keep widget carets and selections on visible-character boundaries.
///
/// An offset produced by an IME or a stale drag can otherwise land between a
/// base character and a combining mark (or inside a ZWJ emoji), where editing
/// would visibly split one glyph.
fn clamp_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    if offset == text.len() {
        return offset;
    }
    text.grapheme_indices(true)
        .map(|(start, _)| start)
        .take_while(|start| *start <= offset)
        .last()
        .unwrap_or(0)
}

/// Tab / Shift-Tab while a language chip, image caption, or frontmatter
/// overlay is focused commits that overlay. The document body must not run
/// `IndentList` / `OutdentList`.
fn widget_owns_tab(edit: &WidgetEdit) -> bool {
    !matches!(edit, WidgetEdit::Idle)
}

/// Left/Right/word/Home/End/document/page (and Delete / word-delete / line-delete)
/// stay inside the overlay; they must not move or mutate the document body.
/// Cmd+A / Undo / Redo use the same gate.
fn widget_owns_caret(edit: &WidgetEdit) -> bool {
    !matches!(edit, WidgetEdit::Idle)
}

/// Cmd/Ctrl+B/I/E/K while a widget overlay is focused must not commit and
/// toggle marks on the document body.
fn widget_owns_wrap(edit: &WidgetEdit) -> bool {
    !matches!(edit, WidgetEdit::Idle)
}

/// Captions are Markdown text; frontmatter values are YAML and must never
/// receive Markdown delimiters from formatting shortcuts.
fn widget_wraps_draft(edit: &WidgetEdit) -> bool {
    matches!(edit, WidgetEdit::ImageAlt { .. })
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
    // Overlay click/drag places an inner caret (body-quality hit-test). Wrap
    // still targets the whole draft. Empty Cmd+B becomes `****` with the caret
    // between the marks so the next insert is `**x**`, not `****x`. IME origin
    // is the inner `|` / caret rect, not the overlay's trailing edge.
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
    *caret = clamp_grapheme_boundary(draft, *caret);
    *anchor = clamp_grapheme_boundary(draft, *anchor);
    let range = widget_range(*caret, *anchor, draft.len());
    let start = range.start;
    draft.replace_range(range, text);
    *caret = start + text.len();
    *anchor = *caret;
}

fn delete_before_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) {
    let Some((draft, caret)) = edit.draft_caret_mut() else {
        return;
    };
    *caret = clamp_grapheme_boundary(draft, *caret);
    *anchor = clamp_grapheme_boundary(draft, *anchor);
    let range = widget_range(*caret, *anchor, draft.len());
    if range.start != range.end {
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
    let start = draft[..at]
        .grapheme_indices(true)
        .next_back()
        .map(|(start, _)| start)
        .unwrap_or(0);
    draft.replace_range(start..at, "");
    *caret = start;
    *anchor = start;
}

fn delete_after_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) {
    let Some((draft, caret)) = edit.draft_caret_mut() else {
        return;
    };
    *caret = clamp_grapheme_boundary(draft, *caret);
    *anchor = clamp_grapheme_boundary(draft, *anchor);
    let range = widget_range(*caret, *anchor, draft.len());
    if range.start != range.end {
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
        .graphemes(true)
        .next()
        .map(str::len)
        .unwrap_or_default();
    draft.replace_range(at..at + next, "");
}

fn delete_toward_in_widget(
    edit: &mut WidgetEdit,
    anchor: &mut usize,
    target_of: impl Fn(&str, usize) -> usize,
) {
    let Some((draft, caret)) = edit.draft_caret_mut() else {
        return;
    };
    *caret = clamp_grapheme_boundary(draft, *caret);
    *anchor = clamp_grapheme_boundary(draft, *anchor);
    let range = widget_range(*caret, *anchor, draft.len());
    let (start, end) = if range.start != range.end {
        (range.start, range.end)
    } else {
        let at = range.start;
        let target = clamp_grapheme_boundary(draft, target_of(draft, at));
        if target <= at {
            (target, at)
        } else {
            (at, target)
        }
    };
    if start == end {
        return;
    }
    draft.replace_range(start..end, "");
    *caret = start;
    *anchor = start;
}

fn delete_word_before_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) {
    delete_toward_in_widget(edit, anchor, prev_word_start);
}

fn delete_word_after_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) {
    delete_toward_in_widget(edit, anchor, next_word_end);
}

fn delete_to_line_start_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) {
    delete_toward_in_widget(edit, anchor, overlay_line_start);
}

fn delete_to_line_end_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) {
    delete_toward_in_widget(edit, anchor, overlay_line_end);
}

fn overlay_line_start(draft: &str, at: usize) -> usize {
    let at = clamp_grapheme_boundary(draft, at);
    draft[..at].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

fn overlay_line_end(draft: &str, at: usize) -> usize {
    let at = clamp_grapheme_boundary(draft, at);
    draft[at..]
        .find('\n')
        .map(|i| at + i)
        .unwrap_or(draft.len())
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
        *caret = clamp_grapheme_boundary(draft, snap.caret);
    }
    *anchor = clamp_grapheme_boundary(&snap.draft, snap.anchor);
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
    let at = clamp_grapheme_boundary(draft, at);
    let line_start = draft[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col = draft[line_start..at].graphemes(true).count();
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
    for (n, (i, _)) in line.grapheme_indices(true).enumerate() {
        if n == col {
            return dest_start + i;
        }
    }
    dest_end
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
    let at = clamp_grapheme_boundary(draft, *caret);
    let anchored = clamp_grapheme_boundary(draft, *anchor);
    *caret = at;
    *anchor = anchored;
    let has_sel = at != anchored;
    let target = match movement {
        CaretMove::Left => {
            if !extend && has_sel {
                at.min(*anchor)
            } else {
                previous_boundary(draft, at)
            }
        }
        CaretMove::Right => {
            if !extend && has_sel {
                at.max(*anchor)
            } else {
                next_boundary(draft, at)
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

fn wrap_kind_from_rich(command: &RichCommand) -> Option<WrapKind> {
    match command {
        RichCommand::ToggleMark(mark) if *mark == MarkSet::BOLD => Some(WrapKind::Bold),
        RichCommand::ToggleMark(mark) if *mark == MarkSet::ITALIC => Some(WrapKind::Italic),
        RichCommand::ToggleMark(mark) if *mark == MarkSet::CODE => Some(WrapKind::Code),
        RichCommand::ToggleLink => Some(WrapKind::Link),
        _ => None,
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
    /// Horizontal intent for repeated rendered Up/Down moves. This is kept
    /// separately from the source selection because a short wrapped row must
    /// not permanently change the column restored on the next long row.
    vertical_preferred_x: Option<f32>,
    /// Cell body selected by the last Cmd-A. Empty cells are collapsed, so
    /// a second Cmd-A cannot be detected from the range alone.
    table_select_all_cell: Option<Range<usize>>,
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
    /// Kept alongside an invalid frontmatter draft so the user can correct
    /// it instead of losing the edit on Enter, Tab, blur, or click-away.
    frontmatter_error: Option<String>,
    widget_undo: Vec<WidgetDraftSnap>,
    widget_redo: Vec<WidgetDraftSnap>,
    /// Network requests are document-controlled content, so they need an
    /// explicit user gesture for each open editor tab.
    remote_images_authorized: bool,
    remote_pending: HashSet<String>,
    remote_failed: HashSet<String>,
    /// Local document-controlled images that have passed the background
    /// preflight. Every value is a validated content-addressed cache copy.
    local_image_paths: HashMap<std::path::PathBuf, std::path::PathBuf>,
    local_image_pending: HashSet<std::path::PathBuf>,
    local_image_checked_revision: HashMap<std::path::PathBuf, u64>,
    /// `data:` images use the same cache-only GPUI boundary as local files.
    /// Their full decode, validation, and write happen off the UI thread.
    data_image_paths: HashMap<String, std::path::PathBuf>,
    data_image_pending: HashSet<String>,
    data_image_checked_revision: HashMap<String, u64>,
    _blink_task: Task<()>,
    _remote_fetch_tasks: Vec<Task<()>>,
    _local_image_tasks: Vec<Task<()>>,
    _data_image_tasks: Vec<Task<()>>,
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
        let doc_sub = cx.observe(&document, |this, _, cx| {
            this.vertical_preferred_x = None;
            this.ime.clear_visual_navigation();
            cx.notify();
        });
        Self {
            document,
            theme,
            engine: RichEngine::new(),
            list_state: ListState::new(0, ListAlignment::Top, px(512.)),
            snapshot: None,
            synced_revision: None,
            selected_range: 0..0,
            selection_reversed: false,
            vertical_preferred_x: None,
            table_select_all_cell: None,
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
            frontmatter_error: None,
            widget_undo: Vec::new(),
            widget_redo: Vec::new(),
            remote_images_authorized: false,
            remote_pending: HashSet::new(),
            remote_failed: HashSet::new(),
            local_image_paths: HashMap::new(),
            local_image_pending: HashSet::new(),
            local_image_checked_revision: HashMap::new(),
            data_image_paths: HashMap::new(),
            data_image_pending: HashSet::new(),
            data_image_checked_revision: HashMap::new(),
            _blink_task: Task::ready(()),
            _remote_fetch_tasks: Vec::new(),
            _local_image_tasks: Vec::new(),
            _data_image_tasks: Vec::new(),
            _subscriptions: vec![focus_sub, blur_sub, doc_sub],
        }
    }

    pub fn set_theme(&mut self, theme: EditorTheme, cx: &mut Context<Self>) {
        self.theme = theme;
        self.vertical_preferred_x = None;
        self.ime.clear_visual_navigation();
        self.snapshot = None;
        self.synced_revision = None;
        cx.notify();
    }

    /// Authorize loading remote image URLs for this document tab.
    ///
    /// The permission deliberately is not persisted: opening a Markdown file
    /// must not send requests to URLs chosen by that file until the reader has
    /// explicitly asked to load them.
    pub fn load_remote_images(&mut self, cx: &mut Context<Self>) {
        self.remote_images_authorized = true;
        self.remote_failed.clear();
        self.snapshot = None;
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
        self.table_select_all_cell = None;
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
            let was_editing = !matches!(self.widget_edit, WidgetEdit::Idle);
            if was_editing && !self.commit_widget_edit(cx) {
                // Invalid YAML stays in its overlay; do not let the command
                // that tried to commit it fall through and mutate the body.
                return RichOutcome::Noop;
            }
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
            self.vertical_preferred_x = None;
            self.ime.clear_visual_navigation();
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
                if self.widget_delete_span(prev_word_start, delete_word_before_in_widget, cx) {
                    EditorOutcome::Changed
                } else if self.apply_rich(RichCommand::DeleteWordLeft, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::DeleteWordRight => {
                if self.widget_delete_span(next_word_end, delete_word_after_in_widget, cx) {
                    EditorOutcome::Changed
                } else if self.apply_rich(RichCommand::DeleteWordRight, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::DeleteToLineStart => {
                if self.widget_delete_span(overlay_line_start, delete_to_line_start_in_widget, cx) {
                    EditorOutcome::Changed
                } else if self.apply_rich(RichCommand::DeleteToLineStart, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::DeleteToLineEnd => {
                if self.widget_delete_span(overlay_line_end, delete_to_line_end_in_widget, cx) {
                    EditorOutcome::Changed
                } else if self.apply_rich(RichCommand::DeleteToLineEnd, cx) == RichOutcome::Noop {
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
                self.vertical_preferred_x = None;
                self.move_to(offset, false, cx);
                EditorOutcome::CaretMoved
            }
            EditorCommand::SetSelection { start, end } => {
                self.vertical_preferred_x = None;
                let len = self.document.read(cx).buffer.len_bytes();
                self.table_select_all_cell = None;
                self.selected_range = start.min(len)..end.min(len);
                self.selection_reversed = start > end;
                cx.notify();
                EditorOutcome::CaretMoved
            }
            EditorCommand::SelectAll => {
                self.vertical_preferred_x = None;
                if select_all_in_widget(&mut self.widget_edit, &mut self.widget_anchor) {
                    self.table_select_all_cell = None;
                    self.reset_blink(cx);
                    self.snapshot = None;
                    cx.notify();
                    EditorOutcome::CaretMoved
                } else {
                    let source = self.document.read(cx).buffer.content();
                    self.engine.sync(self.document.read(cx));
                    // Typora: first Cmd-A in a table selects the cell. A
                    // second Cmd-A (already that cell, including an empty
                    // collapsed body) takes the document.
                    match table_select_all_range(
                        &self.engine,
                        &source,
                        &self.selected_range,
                        self.table_select_all_cell.as_ref(),
                    ) {
                        Some(cell) => {
                            self.table_select_all_cell = Some(cell.clone());
                            self.selected_range = cell;
                        }
                        None => {
                            self.table_select_all_cell = None;
                            self.selected_range = 0..source.len();
                        }
                    }
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
                // IndentList owns table Tab (cell nav) vs list indent.
                if widget_owns_tab(&self.widget_edit) {
                    if self.commit_widget_edit(cx) {
                        EditorOutcome::Changed
                    } else {
                        EditorOutcome::Noop
                    }
                } else if self.apply_rich(RichCommand::IndentList, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::Outdent => {
                if widget_owns_tab(&self.widget_edit) {
                    if self.commit_widget_edit(cx) {
                        EditorOutcome::Changed
                    } else {
                        EditorOutcome::Noop
                    }
                } else if self.apply_rich(RichCommand::OutdentList, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
        }
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.widget_edit, WidgetEdit::Idle) {
            if let Some((draft, _)) = self.widget_display() {
                if let Some(text) = overlay_copy_text(&draft, self.widget_sel()) {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
            return;
        }
        let source = self.document.read(cx).buffer.content();
        self.engine.sync(self.document.read(cx));
        let text = self
            .engine
            .markdown_for_selection(&source, self.selected_range.clone());
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut_selection(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.widget_edit, WidgetEdit::Idle) {
            let sel = self.widget_sel();
            if sel.start < sel.end {
                if let Some((draft, _)) = self.widget_display() {
                    if let Some(text) = overlay_copy_text(&draft, sel) {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                }
                let _ = self.widget_backspace(cx);
            }
            return;
        }
        let source = self.document.read(cx).buffer.content();
        self.engine.sync(self.document.read(cx));
        let expanded = self
            .engine
            .expand_markdown_cut_selection(&source, self.selected_range.clone());
        if expanded.start == expanded.end {
            return;
        }
        if let Some(text) = source.get(expanded.clone()) {
            if !text.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
            }
        }
        self.selected_range = expanded;
        self.selection_reversed = false;
        self.apply_rich(RichCommand::Delete, cx);
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
            self.table_select_all_cell = None;
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
            self.table_select_all_cell = None;
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
            self.frontmatter_error = None;
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
            self.frontmatter_error = None;
            self.reset_blink(cx);
            self.snapshot = None;
            cx.notify();
            EditorOutcome::Changed
        } else {
            EditorOutcome::Noop
        }
    }

    fn record_widget_edit(&mut self) {
        self.frontmatter_error = None;
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
        self.table_select_all_cell = None;
        let len = self.document.read(cx).buffer.len_bytes();
        let source = self.document.read(cx).buffer.content();
        self.engine.sync(self.document.read(cx));
        let offset = self.engine.clamp_raw_prefix(
            &source,
            self.engine.snap_caret(offset.min(len), Bias::Left),
            Bias::Left,
        );
        if extend {
            let anchor = if self.selection_reversed {
                self.selected_range.end
            } else {
                self.selected_range.start
            };
            let offset = if let Some(cell) = self.engine.cell_edit_range(anchor, &source) {
                offset.clamp(cell.start, cell.end)
            } else {
                offset
            };
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
        self.reset_blink(cx);
        cx.notify();
    }

    /// Prefer the last painted WYSIWYG glyph geometry for a one-row vertical
    /// move. The rich engine remains the fallback when the virtualized list
    /// has not painted the relevant row yet (or during the first frame).
    fn rendered_vertical_caret(&mut self, source: &str, cursor: usize, delta: i32) -> usize {
        if let Some(target) =
            self.ime
                .visual_vertical_target(cursor, delta, self.vertical_preferred_x)
        {
            self.vertical_preferred_x = Some(target.preferred_x);
            if let Some(source) = target.source {
                return source;
            }
        }
        self.engine.vertical_caret(source, cursor, delta)
    }

    fn move_caret(&mut self, movement: CaretMove, extend: bool, cx: &mut Context<Self>) {
        let source = self.document.read(cx).buffer.content();
        self.engine.sync(self.document.read(cx));
        let cursor = self.cursor_offset();
        if !matches!(movement, CaretMove::Up | CaretMove::Down) {
            self.vertical_preferred_x = None;
        }
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
            CaretMove::Home => self.engine.line_start_caret(&source, cursor),
            CaretMove::End => self.engine.line_end_caret(&source, cursor),
            CaretMove::WordLeft => self.engine.prev_word_caret(&source, cursor),
            CaretMove::WordRight => self.engine.next_word_caret(&source, cursor),
            CaretMove::DocumentHome => self.engine.clamp_raw_prefix(
                &source,
                self.engine.snap_caret(0, Bias::Right),
                Bias::Right,
            ),
            CaretMove::DocumentEnd => self.engine.document_end_caret(&source, cursor),
            CaretMove::Up => self.rendered_vertical_caret(&source, cursor, -1),
            CaretMove::Down => self.rendered_vertical_caret(&source, cursor, 1),
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
        let (base_dir, source, old_real) = {
            let doc = self.document.read(cx);
            let base_dir = doc
                .path
                .as_ref()
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf());
            let source = doc.buffer.content();
            let old_real = self.engine.tree().blocks.len();
            self.engine.sync(doc);
            (base_dir, source, old_real)
        };
        let new_real = self.engine.tree().blocks.len();
        let new_count = new_real.max(1);
        if !widget_only {
            match self.engine.last_splice() {
                Some(splice) if self.synced_revision.is_some() && old_real > 0 && new_real > 0 => {
                    self.list_state
                        .splice(splice.range.clone(), splice.new_count);
                }
                _ => {
                    self.list_state.splice(
                        0..old_real.max(1).min(new_count.max(old_real.max(1))),
                        new_count,
                    );
                    self.list_state = ListState::new(new_count, ListAlignment::Top, px(512.));
                }
            }
        }
        self.enqueue_data_images(revision, cx);
        self.enqueue_local_images(base_dir.as_deref(), revision, cx);
        self.enqueue_remote_images(cx);
        let snapshot = Arc::new(RenderSnapshot {
            tree: self.engine.tree().clone(),
            source,
            theme: self.theme.clone(),
            base_dir,
            local_image_paths: self.local_image_paths.clone(),
            local_image_pending: self.local_image_pending.clone(),
            data_image_paths: self.data_image_paths.clone(),
            data_image_pending: self.data_image_pending.clone(),
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
        if !self.remote_images_authorized {
            return;
        }
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

    /// Decode and validate document `data:` images off the UI thread. Even a
    /// syntactically local data URL can be megabytes long, so the render path
    /// only observes this approved cache map and never calls `img(String)`.
    fn enqueue_data_images(&mut self, revision: u64, cx: &mut Context<Self>) {
        let urls = collect_data_image_urls(self.engine.tree());
        let active = urls.iter().cloned().collect::<HashSet<_>>();
        self.data_image_paths.retain(|url, _| active.contains(url));
        self.data_image_checked_revision
            .retain(|url, _| active.contains(url));

        let cache_dir = default_image_cache_dir();
        for url in urls {
            if self.data_image_pending.contains(&url)
                || self.data_image_checked_revision.get(&url) == Some(&revision)
            {
                continue;
            }
            self.data_image_pending.insert(url.clone());
            let url_read = url.clone();
            let url_status = url;
            let cache_dir = cache_dir.clone();
            let task = cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { materialize_safe_data_image(&url_read, &cache_dir) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.data_image_pending.remove(&url_status);
                    this.data_image_checked_revision
                        .insert(url_status.clone(), revision);
                    match result {
                        Some(approved) => {
                            this.data_image_paths.insert(url_status.clone(), approved);
                        }
                        None => {
                            this.data_image_paths.remove(&url_status);
                        }
                    }
                    this.snapshot = None;
                    cx.notify();
                });
            });
            self._data_image_tasks.push(task);
        }
    }

    /// Classify document-controlled local images without ever asking GPUI to
    /// infer their type first. GPUI treats every non-raster byte stream as an
    /// SVG candidate, so the preflight must look at content rather than an
    /// extension. Each document revision re-checks visible paths in the
    /// background; a previous safe SVG cache copy remains visible while that
    /// work is in flight.
    fn enqueue_local_images(
        &mut self,
        base_dir: Option<&std::path::Path>,
        revision: u64,
        cx: &mut Context<Self>,
    ) {
        let paths = collect_local_image_paths(self.engine.tree(), base_dir);
        let active = paths.iter().cloned().collect::<HashSet<_>>();
        self.local_image_paths
            .retain(|source, _| active.contains(source));
        self.local_image_checked_revision
            .retain(|source, _| active.contains(source));

        let cache_dir = default_image_cache_dir();
        for source in paths {
            if self.local_image_pending.contains(&source)
                || self.local_image_checked_revision.get(&source) == Some(&revision)
            {
                continue;
            }
            self.local_image_pending.insert(source.clone());
            let source_read = source.clone();
            let source_status = source;
            let cache_dir = cache_dir.clone();
            let task = cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { materialize_safe_local_image(&source_read, &cache_dir) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.local_image_pending.remove(&source_status);
                    this.local_image_checked_revision
                        .insert(source_status.clone(), revision);
                    match result {
                        Ok(approved) => {
                            this.local_image_paths
                                .insert(source_status.clone(), approved);
                        }
                        Err(_) => {
                            this.local_image_paths.remove(&source_status);
                        }
                    }
                    this.snapshot = None;
                    cx.notify();
                });
            });
            self._local_image_tasks.push(task);
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

    fn widget_delete_span(
        &mut self,
        target_of: impl Fn(&str, usize) -> usize,
        apply: fn(&mut WidgetEdit, &mut usize),
        cx: &mut Context<Self>,
    ) -> bool {
        if matches!(self.widget_edit, WidgetEdit::Idle) {
            return false;
        }
        if let Some(snap) = widget_snap(&self.widget_edit, self.widget_anchor) {
            let range = widget_range(snap.caret, snap.anchor, snap.draft.len());
            let (start, end) = if range.start != range.end {
                (range.start, range.end)
            } else {
                let at = range.start;
                let target = target_of(&snap.draft, at).min(snap.draft.len());
                if target <= at {
                    (target, at)
                } else {
                    (at, target)
                }
            };
            if start == end {
                return true;
            }
        }
        self.record_widget_edit();
        self.widget_preedit = None;
        apply(&mut self.widget_edit, &mut self.widget_anchor);
        self.snapshot = None;
        cx.notify();
        true
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
        self.frontmatter_error = None;
        self.widget_selecting = false;
        self.clear_widget_history();
        self.snapshot = None;
        cx.notify();
        true
    }

    fn commit_widget_edit(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some(error) = self.frontmatter_widget_error(cx) {
            // Do not take `widget_edit`: the draft remains visible and
            // editable, and a body command cannot accidentally follow it.
            self.widget_preedit = None;
            self.widget_selecting = false;
            self.frontmatter_error = Some(error);
            cx.notify();
            return false;
        }
        self.widget_preedit = None;
        self.widget_selecting = false;
        self.frontmatter_error = None;
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

    fn frontmatter_widget_error(&self, cx: &Context<Self>) -> Option<String> {
        let source = self.document.read(cx).buffer.content();
        match &self.widget_edit {
            WidgetEdit::Frontmatter { key, draft, .. } => {
                let raw = markrust_core::parse_frontmatter(&source)
                    .and_then(|info| source.get(info.start_byte..info.end_byte))
                    .unwrap_or("");
                markrust_core::upsert_yaml_key(raw, key, draft)
                    .err()
                    .map(|error| error.message().to_string())
            }
            WidgetEdit::FrontmatterYaml { draft, .. } => {
                markrust_core::validate_frontmatter_yaml(draft)
                    .err()
                    .map(|error| error.message().to_string())
            }
            WidgetEdit::Idle | WidgetEdit::CodeInfo { .. } | WidgetEdit::ImageAlt { .. } => None,
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

    fn finish_widget_before_switch(&mut self, cx: &mut Context<Self>) -> bool {
        matches!(self.widget_edit, WidgetEdit::Idle) || self.commit_widget_edit(cx)
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
        self.vertical_preferred_x = None;
        self.is_selecting = true;
        self.focus_handle.focus(window, cx);
        self.move_to(source, extend, cx);
    }

    fn select_source_range(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_widget_edit(cx);
        self.vertical_preferred_x = None;
        self.is_selecting = false;
        self.focus_handle.focus(window, cx);
        self.apply_editor_command(
            EditorCommand::SetSelection {
                start: range.start,
                end: range.end,
            },
            cx,
        );
    }

    fn drag_source(&mut self, source: usize, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.vertical_preferred_x = None;
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
        if !self.finish_widget_before_switch(cx) {
            return;
        }
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
        if !self.finish_widget_before_switch(cx) {
            return;
        }
        self.widget_edit = WidgetEdit::ImageAlt {
            range: source_range,
            draft: alt.to_string(),
            caret: alt.len(),
        };
        self.widget_anchor = alt.len();
        self.widget_selecting = false;
        self.widget_preedit = None;
        self.snapshot = None;
        cx.notify();
    }

    fn edit_frontmatter_field(&mut self, key: &'static str, current: &str, cx: &mut Context<Self>) {
        if !self.finish_widget_before_switch(cx) {
            return;
        }
        self.widget_edit = WidgetEdit::Frontmatter {
            key,
            draft: current.to_string(),
            caret: current.len(),
        };
        self.widget_anchor = current.len();
        self.widget_selecting = false;
        self.widget_preedit = None;
        self.frontmatter_error = None;
        self.snapshot = None;
        cx.notify();
    }

    fn edit_frontmatter_yaml(&mut self, current: &str, cx: &mut Context<Self>) {
        if !self.finish_widget_before_switch(cx) {
            return;
        }
        self.widget_edit = WidgetEdit::FrontmatterYaml {
            draft: current.to_string(),
            caret: current.len(),
        };
        self.widget_anchor = current.len();
        self.widget_selecting = false;
        self.widget_preedit = None;
        self.frontmatter_error = None;
        self.snapshot = None;
        cx.notify();
    }

    fn open_table_menu(&mut self, source: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.finish_widget_before_switch(cx) {
            return;
        }
        self.vertical_preferred_x = None;
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
        visual_lines: Vec<VisualLine>,
    ) {
        self.ime.report_visual_lines(layout.clone(), visual_lines);
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
        // GPUI: invalidate_character_coordinates → next frame selected_bounds
        // → PlatformWindow::update_ime_position. On macOS that call discards
        // the Bounds and invalidates; AppKit then pulls bounds_for_range
        // (firstRectForCharacterRange:). TestWindow swallows the push;
        // take_platform_push is the in-repo record of the request.
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
                self.widget_edit.caret(),
                self.widget_anchor,
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
            self.record_widget_edit();
            self.widget_preedit = None;
            let sel = self.widget_sel();
            if let Some((draft, caret)) = self.widget_edit.draft_caret_mut() {
                super::ime::replace_in_widget_draft(draft, caret, range_utf16, new_text, sel);
            }
            self.widget_anchor = self.widget_edit.caret();
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
                return Some(Self::offset_to_utf16(
                    &content,
                    self.widget_edit.caret().min(content.len()),
                ));
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
        let composing = if matches!(self.widget_edit, WidgetEdit::Idle) {
            self.preedit.as_deref()
        } else {
            self.widget_preedit.as_deref()
        };
        self.ime.begin_frame(
            !matches!(self.widget_edit, WidgetEdit::Idle),
            if matches!(self.widget_edit, WidgetEdit::Idle) {
                self.cursor_offset()
            } else {
                self.widget_edit.caret()
            },
            composing,
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
        let frontmatter_error = self.frontmatter_error.clone();
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
                move |_: &crate::editor::Copy, _, cx| {
                    editor.update(cx, |e, cx| e.copy_selection(cx));
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Cut, _, cx| {
                    editor.update(cx, |e, cx| e.cut_selection(cx));
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Enter, _, cx| {
                    editor.update(cx, |e, cx| {
                        if !e.consume_widget_newline(cx) {
                            // SplitBlock owns table Enter (`<br>`) vs paragraph split.
                            e.apply_rich(RichCommand::SplitBlock, cx);
                        }
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::InsertLineBreak, _, cx| {
                    editor.update(cx, |e, cx| {
                        if !e.consume_widget_newline(cx) {
                            e.apply_rich(RichCommand::InsertLineBreak, cx);
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
                    frontmatter_error.as_deref(),
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

#[cfg(test)]
fn widget_draft_with_caret(draft: &str, caret: usize, preedit: &str) -> String {
    let mut at = caret.min(draft.len());
    if !draft.is_char_boundary(at) {
        at = draft.len();
    }
    format!("{}{}|{}", &draft[..at], preedit, &draft[at..])
}

fn frontmatter_panel(
    editor: gpui::Entity<RichEditorView>,
    theme: &crate::theme::EditorTheme,
    info: &markrust_core::FrontmatterInfo,
    editing_fm: Option<(&'static str, &str)>,
    editing_yaml: Option<&str>,
    error: Option<&str>,
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
                .when_some(error, |panel, message| {
                    panel.child(
                        div()
                            .text_xs()
                            .text_color(theme.accent)
                            .child(message.to_string()),
                    )
                })
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

#[cfg(test)]
mod tests {
    use super::*;
    use markrust_core::rich::NodeId;

    fn chip() -> WidgetEdit {
        WidgetEdit::CodeInfo {
            id: NodeId(1),
            draft: "rust".into(),
            caret: 4,
        }
    }

    fn caption() -> WidgetEdit {
        WidgetEdit::ImageAlt {
            range: 0..8,
            draft: "cat".into(),
            caret: 3,
        }
    }

    fn empty_caption() -> WidgetEdit {
        WidgetEdit::ImageAlt {
            range: 0..8,
            draft: String::new(),
            caret: 0,
        }
    }

    fn frontmatter_title() -> WidgetEdit {
        WidgetEdit::Frontmatter {
            key: "title",
            draft: "Hi".into(),
            caret: 2,
        }
    }

    fn yaml() -> WidgetEdit {
        WidgetEdit::FrontmatterYaml {
            draft: "title: Hi".into(),
            caret: 9,
        }
    }

    #[test]
    fn tab_in_chip_caption_frontmatter_commits_instead_of_indenting_body() {
        assert!(
            !widget_owns_tab(&WidgetEdit::Idle),
            "body Tab still runs IndentList"
        );
        for (edit, label) in [
            (chip(), "language chip"),
            (caption(), "image caption"),
            (frontmatter_title(), "frontmatter field"),
            (yaml(), "frontmatter YAML"),
        ] {
            assert!(
                widget_owns_tab(&edit),
                "Tab in {label} must commit the overlay and not IndentList the body"
            );
        }
    }

    #[test]
    fn wrap_shortcuts_target_widget_text_not_the_body() {
        assert!(
            !widget_owns_wrap(&WidgetEdit::Idle),
            "body Cmd/Ctrl+B still toggles the document"
        );
        assert!(
            !widget_wraps_draft(&chip()),
            "language chip is an identifier, not a wrap target"
        );
        assert!(
            widget_owns_wrap(&caption()) && widget_wraps_draft(&caption()),
            "image captions are Markdown text and may be formatted"
        );
        for (edit, label) in [
            (frontmatter_title(), "frontmatter field"),
            (yaml(), "frontmatter YAML"),
        ] {
            assert!(
                widget_owns_wrap(&edit) && !widget_wraps_draft(&edit),
                "{label} must consume wrap without writing Markdown syntax into YAML"
            );
        }
        assert!(
            widget_owns_wrap(&chip()) && !widget_wraps_draft(&chip()),
            "language chip owns wrap as a no-op so the body is not bolded"
        );

        let mut chip_edit = chip();
        let before = match &chip_edit {
            WidgetEdit::CodeInfo { draft, .. } => draft.clone(),
            _ => unreachable!(),
        };
        assert_eq!(
            apply_wrap_to_widget(&mut chip_edit, WrapKind::Bold),
            WidgetWrapResult::Ignored
        );
        match &chip_edit {
            WidgetEdit::CodeInfo { draft, .. } => {
                assert_eq!(draft, &before, "chip draft must not gain **")
            }
            other => panic!("chip edit must stay CodeInfo, got {other:?}"),
        }

        let mut caption_edit = caption();
        assert_eq!(
            apply_wrap_to_widget(&mut caption_edit, WrapKind::Bold),
            WidgetWrapResult::Applied
        );
        match &caption_edit {
            WidgetEdit::ImageAlt { draft, .. } => {
                assert_eq!(draft, "**cat**", "caption must wrap, got {draft:?}")
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        assert_eq!(
            apply_wrap_to_widget(&mut caption_edit, WrapKind::Bold),
            WidgetWrapResult::Applied
        );
        match &caption_edit {
            WidgetEdit::ImageAlt { draft, .. } => {
                assert_eq!(draft, "cat", "second bold unwraps, got {draft:?}")
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }

        let mut title = frontmatter_title();
        assert_eq!(
            apply_wrap_to_widget(&mut title, WrapKind::Italic),
            WidgetWrapResult::Ignored
        );
        match &title {
            WidgetEdit::Frontmatter { draft, .. } => {
                assert_eq!(draft, "Hi", "title must stay a YAML scalar, got {draft:?}")
            }
            other => panic!("expected Frontmatter, got {other:?}"),
        }

        let mut yaml_edit = yaml();
        assert_eq!(
            apply_wrap_to_widget(&mut yaml_edit, WrapKind::Code),
            WidgetWrapResult::Ignored
        );
        match &yaml_edit {
            WidgetEdit::FrontmatterYaml { draft, .. } => {
                assert_eq!(
                    draft, "title: Hi",
                    "YAML must not gain backticks, got {draft:?}"
                )
            }
            other => panic!("expected FrontmatterYaml, got {other:?}"),
        }

        let mut link_caption = caption();
        assert_eq!(
            apply_wrap_to_widget(&mut link_caption, WrapKind::Link),
            WidgetWrapResult::Applied
        );
        match &link_caption {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "[cat]()", "caption link wrap, got {draft:?}");
                assert_eq!(
                    *caret,
                    "[cat](".len(),
                    "Cmd-K on a caption selection must leave the caret in the URL, got {caret}"
                );
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }

        assert_eq!(
            apply_wrap_to_widget(&mut WidgetEdit::Idle, WrapKind::Bold),
            WidgetWrapResult::NotFocused
        );
    }

    #[test]
    fn empty_caption_wrap_types_inside_the_marks() {
        let mut edit = empty_caption();
        assert_eq!(
            apply_wrap_to_widget(&mut edit, WrapKind::Bold),
            WidgetWrapResult::Applied
        );
        match &edit {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "****", "empty bold wrap, got {draft:?}");
                assert_eq!(*caret, 2, "caret must sit between the marks, got {caret}");
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        let mut wrap_anchor = edit.caret();
        insert_into_widget(&mut edit, &mut wrap_anchor, "x");
        match &edit {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(
                    draft, "**x**",
                    "typing after empty wrap must go inside the marks, not after closers; got {draft:?}"
                );
                assert_eq!(*caret, 3);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }

        let mut empty_fm = WidgetEdit::Frontmatter {
            key: "title",
            draft: String::new(),
            caret: 0,
        };
        assert_eq!(
            apply_wrap_to_widget(&mut empty_fm, WrapKind::Bold),
            WidgetWrapResult::Ignored
        );
        let mut fm_anchor = empty_fm.caret();
        insert_into_widget(&mut empty_fm, &mut fm_anchor, "Hi");
        match &empty_fm {
            WidgetEdit::Frontmatter { draft, .. } => {
                assert_eq!(
                    draft, "Hi",
                    "frontmatter typing remains a YAML scalar after a consumed wrap shortcut, got {draft:?}"
                )
            }
            other => panic!("expected Frontmatter, got {other:?}"),
        }

        // Overlay click/drag places this inner offset; wrap of a non-empty
        // draft still covers the whole field (not a body selection).
        let mut wrapped = caption();
        apply_wrap_to_widget(&mut wrapped, WrapKind::Bold);
        match &wrapped {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "**cat**");
                assert_eq!(
                    *caret, 5,
                    "whole-draft wrap leaves the insert offset before the closers"
                );
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
    }

    #[test]
    fn overlay_click_places_inner_caret_without_moving_body() {
        let mut edit = caption();
        let body = 12..12;
        edit.set_caret(1);
        assert_eq!(edit.caret(), 1, "click x must land mid-caption");
        assert_eq!(body, 12..12, "overlay click must not move the body caret");

        let mut chip_edit = chip();
        chip_edit.set_caret(2);
        assert_eq!(chip_edit.caret(), 2);

        let mut title = frontmatter_title();
        title.set_caret(1);
        assert_eq!(title.caret(), 1);

        let mut yaml_edit = yaml();
        yaml_edit.set_caret(3);
        assert_eq!(yaml_edit.caret(), 3);
    }

    #[test]
    fn overlay_drag_extends_inner_caret() {
        let mut edit = caption();
        edit.set_caret(1);
        let anchor = edit.caret();
        edit.set_caret(3);
        let caret = edit.caret();
        assert_eq!(anchor.min(caret)..anchor.max(caret), 1..3);
        assert_eq!(edit.caret(), 3);
    }

    #[test]
    fn overlay_arrows_move_inner_caret_not_body() {
        assert!(
            !widget_owns_caret(&WidgetEdit::Idle),
            "body Left still moves the document caret"
        );
        for (edit, label) in [
            (chip(), "language chip"),
            (caption(), "image caption"),
            (frontmatter_title(), "frontmatter field"),
            (yaml(), "frontmatter YAML"),
        ] {
            assert!(
                widget_owns_caret(&edit),
                "Left/Right in {label} must own the inner caret"
            );
        }

        let mut edit = caption();
        let body = 12..12;
        edit.set_caret(1);
        let mut anchor = edit.caret();
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Left,
            false
        ));
        assert_eq!(edit.caret(), 0, "Left must step the inner offset");
        assert_eq!(anchor, 0);
        assert_eq!(body, 12..12, "overlay Left must not move the body caret");

        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Right,
            false
        ));
        assert_eq!(edit.caret(), 1);
        assert_eq!(body, 12..12);

        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::End,
            false
        ));
        assert_eq!(edit.caret(), 3);
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Home,
            false
        ));
        assert_eq!(edit.caret(), 0);
        assert_eq!(body, 12..12);

        let mut chip_edit = chip();
        chip_edit.set_caret(2);
        let mut chip_anchor = 2;
        assert!(move_in_widget(
            &mut chip_edit,
            &mut chip_anchor,
            CaretMove::Left,
            false
        ));
        assert_eq!(chip_edit.caret(), 1);

        let mut title = frontmatter_title();
        title.set_caret(1);
        let mut title_anchor = 1;
        assert!(move_in_widget(
            &mut title,
            &mut title_anchor,
            CaretMove::Right,
            false
        ));
        assert_eq!(title.caret(), 2);

        let mut yaml_edit = WidgetEdit::FrontmatterYaml {
            draft: "ab\ncd".into(),
            caret: 4,
        };
        let mut yaml_anchor = 4;
        assert!(move_in_widget(
            &mut yaml_edit,
            &mut yaml_anchor,
            CaretMove::Home,
            false
        ));
        assert_eq!(yaml_edit.caret(), 3, "YAML Home is the wrapped line start");
        assert!(move_in_widget(
            &mut yaml_edit,
            &mut yaml_anchor,
            CaretMove::End,
            false
        ));
        assert_eq!(yaml_edit.caret(), 5);

        let mut unused = 0;
        assert!(!move_in_widget(
            &mut WidgetEdit::Idle,
            &mut unused,
            CaretMove::Left,
            false
        ));
    }

    #[test]
    fn overlay_shift_arrows_extend_inner_selection() {
        let mut edit = caption();
        edit.set_caret(3);
        let mut anchor = 3;
        let body = 12..12;
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Left,
            true
        ));
        assert_eq!(edit.caret(), 2);
        assert_eq!(anchor, 3, "Shift-Left must keep the inner anchor");
        assert_eq!(edit.caret().min(anchor)..edit.caret().max(anchor), 2..3);
        assert_eq!(body, 12..12, "Shift-Left must not move the body caret");

        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Right,
            true
        ));
        assert_eq!(edit.caret(), 3);
        assert_eq!(anchor, 3);

        edit.set_caret(1);
        anchor = 3;
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Left,
            false
        ));
        assert_eq!(
            edit.caret(),
            1,
            "Left with an inner selection collapses to the start"
        );
        assert_eq!(anchor, 1);
        assert_eq!(body, 12..12);
    }

    #[test]
    fn overlay_shift_up_down_home_end_extend_yaml_selection() {
        let mut edit = WidgetEdit::FrontmatterYaml {
            draft: "alpha\nbeta".into(),
            caret: 6,
        };
        let mut anchor = 6;
        assert!(move_in_widget(&mut edit, &mut anchor, CaretMove::Up, true));
        assert_eq!(edit.caret(), 0, "Shift-Up from `beta` lands on `alpha`");
        assert_eq!(anchor, 6, "Shift-Up must keep the inner YAML anchor");
        assert_eq!(edit.caret().min(anchor)..edit.caret().max(anchor), 0..6);

        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Down,
            true
        ));
        assert_eq!(edit.caret(), 6);
        assert_eq!(anchor, 6);

        edit.set_caret(8);
        anchor = 8;
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Home,
            true
        ));
        assert_eq!(edit.caret(), 6, "Shift-Home is line-local in YAML");
        assert_eq!(anchor, 8);
        assert!(move_in_widget(&mut edit, &mut anchor, CaretMove::End, true));
        assert_eq!(edit.caret(), 10);
        assert_eq!(anchor, 8);
    }

    #[test]
    fn overlay_word_and_document_keys_move_inner_draft() {
        let mut edit = WidgetEdit::FrontmatterYaml {
            draft: "alpha beta\ngamma".into(),
            caret: "alpha beta\ngamma".len(),
        };
        let mut anchor = edit.caret();
        let body = 12..12;
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::WordLeft,
            false
        ));
        assert_eq!(
            edit.caret(),
            "alpha beta\n".len(),
            "WordLeft is start of `gamma`"
        );
        assert_eq!(anchor, edit.caret());
        assert_eq!(
            body,
            12..12,
            "overlay WordLeft must not move the body caret"
        );

        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::WordLeft,
            false
        ));
        assert_eq!(edit.caret(), "alpha ".len());

        edit.set_caret("alpha ".len());
        anchor = edit.caret();
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::WordRight,
            true
        ));
        assert_eq!(
            edit.caret(),
            "alpha beta".len(),
            "Shift-WordRight extends to the end of `beta`"
        );
        assert_eq!(anchor, "alpha ".len());

        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::DocumentHome,
            false
        ));
        assert_eq!(edit.caret(), 0);
        assert_eq!(anchor, 0);
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::DocumentEnd,
            true
        ));
        assert_eq!(edit.caret(), "alpha beta\ngamma".len());
        assert_eq!(anchor, 0, "Cmd-Shift-Down keeps the overlay draft anchor");
        assert_eq!(body, 12..12);
    }

    #[test]
    fn overlay_word_and_line_delete_stay_in_the_draft() {
        let mut edit = WidgetEdit::FrontmatterYaml {
            draft: "alpha beta".into(),
            caret: "alpha beta".len(),
        };
        let mut anchor = edit.caret();
        let body = 12..12;
        delete_word_before_in_widget(&mut edit, &mut anchor);
        match &edit {
            WidgetEdit::FrontmatterYaml { draft, caret } => {
                assert_eq!(
                    draft, "alpha ",
                    "overlay Option-Backspace must delete the previous word, got {draft:?}"
                );
                assert_eq!(*caret, "alpha ".len());
            }
            other => panic!("expected FrontmatterYaml, got {other:?}"),
        }
        assert_eq!(anchor, "alpha ".len());
        assert_eq!(body, 12..12, "overlay word-delete must not mutate the body");

        let mut yaml = WidgetEdit::FrontmatterYaml {
            draft: "alpha beta\ngamma extra".into(),
            caret: "alpha beta\ngamma extra".len(),
        };
        let mut yaml_anchor = yaml.caret();
        delete_to_line_start_in_widget(&mut yaml, &mut yaml_anchor);
        match &yaml {
            WidgetEdit::FrontmatterYaml { draft, caret } => {
                assert_eq!(
                    draft, "alpha beta\n",
                    "overlay Cmd-Backspace is the current YAML line, got {draft:?}"
                );
                assert_eq!(*caret, "alpha beta\n".len());
            }
            other => panic!("expected FrontmatterYaml, got {other:?}"),
        }

        let mut selected = caption();
        selected.set_caret(1);
        let mut sel_anchor = 3;
        delete_word_before_in_widget(&mut selected, &mut sel_anchor);
        match &selected {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(
                    draft, "c",
                    "word-delete on a non-empty overlay selection deletes the range"
                );
                assert_eq!(*caret, 1);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }

        let mut forward = WidgetEdit::FrontmatterYaml {
            draft: "alpha beta".into(),
            caret: 0,
        };
        let mut fwd_anchor = 0;
        delete_word_after_in_widget(&mut forward, &mut fwd_anchor);
        match &forward {
            WidgetEdit::FrontmatterYaml { draft, .. } => {
                assert_eq!(
                    draft, " beta",
                    "overlay Option-Delete removes the next word, got {draft:?}"
                );
            }
            other => panic!("expected FrontmatterYaml, got {other:?}"),
        }
        assert_eq!(body, 12..12);
    }

    #[test]
    fn shift_up_extends_body_selection_across_paragraphs() {
        let source = "hello\n\nworld";
        let mut engine = RichEngine::new();
        let doc = Document::new(source);
        engine.sync(&doc);
        let w = source.find('w').expect("w");
        let up = engine.vertical_caret(source, w, -1);
        assert!(
            up < w,
            "Up from `world` must leave the second paragraph, got {up}"
        );
        let (sel, reversed) = extend_selection_range(w..w, false, up);
        assert!(reversed, "Shift-Up moves the caret before the anchor");
        assert_eq!(sel, up..w);
        assert!(
            source.get(sel.clone()).is_some_and(|s| s.contains('\n')),
            "Shift-Up must select across the block gap, got {:?}",
            source.get(sel)
        );
        let r = source.find('r').expect("r in world");
        let line_start = source[..r].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let home = engine.clamp_raw_prefix(
            source,
            engine.snap_caret(line_start, markrust_core::rich::Bias::Right),
            markrust_core::rich::Bias::Right,
        );
        assert_eq!(home, w, "Home on `world` is the first painted letter");
        let (to_home, reversed_home) = extend_selection_range(r..r, false, home);
        assert!(reversed_home);
        assert_eq!(
            to_home,
            home..r,
            "Shift-Home from inside `world` must select back to the line start"
        );
    }

    #[test]
    fn word_and_document_keys_extend_body_selection() {
        let source = "**hello** world\n\nnext";
        let mut engine = RichEngine::new();
        let doc = Document::new(source);
        engine.sync(&doc);
        let h = source.find('h').expect("h");
        let w = source.find('w').expect("w");
        let n = source.find("next").expect("next");
        let end_hello = engine.next_word_caret(source, h);
        assert_eq!(
            source.as_bytes().get(end_hello),
            Some(&b' '),
            "WordRight from bold `hello` lands on the painted space"
        );
        let (sel, reversed) = extend_selection_range(h..h, false, end_hello);
        assert!(!reversed);
        assert_eq!(sel, h..end_hello);

        let start_next = engine.prev_word_caret(source, source.len());
        assert_eq!(start_next, n, "WordLeft from EOF is the start of `next`");
        let start_world = engine.prev_word_caret(source, start_next);
        assert_eq!(
            start_world, w,
            "WordLeft from `next` skips the gap onto `world`"
        );
        let (to_world, rev_world) = extend_selection_range(n..n, false, start_world);
        assert!(rev_world);
        assert_eq!(to_world, w..n);

        let home = engine.clamp_raw_prefix(
            source,
            engine.snap_caret(0, markrust_core::rich::Bias::Right),
            markrust_core::rich::Bias::Right,
        );
        assert_eq!(
            home, h,
            "Cmd-Up / document start is the first painted letter"
        );
        let end = engine.clamp_raw_prefix(
            source,
            engine.snap_caret(source.len(), markrust_core::rich::Bias::Left),
            markrust_core::rich::Bias::Left,
        );
        assert!(
            end >= n,
            "Cmd-Down / document end must reach the last paragraph, got {end}"
        );
        let (to_end, reversed_end) = extend_selection_range(h..h, false, end);
        assert!(!reversed_end);
        assert_eq!(to_end, h..end);
        let (to_home, reversed_home) = extend_selection_range(n..n, false, home);
        assert!(reversed_home);
        assert_eq!(to_home, home..n);
    }

    #[test]
    fn overlay_delete_edits_the_draft_not_the_body() {
        assert!(
            !widget_owns_caret(&WidgetEdit::Idle),
            "body Delete still deletes in the document"
        );
        let mut edit = caption();
        edit.set_caret(1);
        let mut anchor = edit.caret();
        let body = "hello";
        delete_after_in_widget(&mut edit, &mut anchor);
        match &edit {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "ct", "Delete must remove the inner grapheme");
                assert_eq!(*caret, 1);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        assert_eq!(body, "hello", "overlay Delete must not mutate the body");

        delete_after_in_widget(&mut edit, &mut anchor);
        match &edit {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "c");
                assert_eq!(*caret, 1);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        delete_after_in_widget(&mut edit, &mut anchor);
        match &edit {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "c", "Delete at end of overlay is a no-op");
                assert_eq!(*caret, 1);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
    }

    #[test]
    fn overlay_select_all_selects_the_draft_not_the_body() {
        let body = 0..80;
        for (mut edit, label) in [
            (chip(), "language chip"),
            (caption(), "image caption"),
            (frontmatter_title(), "frontmatter field"),
            (yaml(), "frontmatter YAML"),
        ] {
            let len = match &edit {
                WidgetEdit::CodeInfo { draft, .. }
                | WidgetEdit::ImageAlt { draft, .. }
                | WidgetEdit::Frontmatter { draft, .. }
                | WidgetEdit::FrontmatterYaml { draft, .. } => draft.len(),
                WidgetEdit::Idle => panic!("expected overlay"),
            };
            let mut anchor = edit.caret();
            assert!(
                select_all_in_widget(&mut edit, &mut anchor),
                "Cmd+A in {label} must select the draft"
            );
            assert_eq!(anchor, 0, "{label} SelectAll anchor");
            assert_eq!(edit.caret(), len, "{label} SelectAll caret");
            assert_eq!(body, 0..80, "{label} must not SelectAll the document");
        }
        let mut unused = 0;
        assert!(
            !select_all_in_widget(&mut WidgetEdit::Idle, &mut unused),
            "body Cmd+A is not consumed by an overlay (tables select the cell first)"
        );
        assert_eq!(body, 0..80);
    }

    #[test]
    fn overlay_empty_caret_copy_is_the_whole_draft() {
        assert_eq!(
            overlay_copy_text("rust", 4..4).as_deref(),
            Some("rust"),
            "empty caret in the language chip copies the draft"
        );
        assert_eq!(
            overlay_copy_text("cat", 3..3).as_deref(),
            Some("cat"),
            "empty caret in the caption copies the draft"
        );
        assert_eq!(
            overlay_copy_text("Hi", 2..2).as_deref(),
            Some("Hi"),
            "empty caret in frontmatter copies the draft"
        );
        assert_eq!(
            overlay_copy_text("title: Hi", 0..0).as_deref(),
            Some("title: Hi")
        );
        assert_eq!(
            overlay_copy_text("hello", 0..5).as_deref(),
            Some("hello"),
            "non-empty overlay selection still copies the slice"
        );
        assert_eq!(overlay_copy_text("hello", 1..4).as_deref(), Some("ell"));
        assert_eq!(
            overlay_copy_text("", 0..0),
            None,
            "empty overlay draft copies nothing"
        );
    }

    #[test]
    fn body_table_select_all_selects_the_cell_not_the_document() {
        use markrust_core::rich::{table_select_all_range, RichEngine};
        use markrust_core::Document;

        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let doc = Document::new(source);
        let mut engine = RichEngine::new();
        engine.sync(&doc);
        let a = source.find('a').expect("header a");
        let cell = table_select_all_range(&engine, source, &(a..a), None).expect("first Cmd-A");
        assert_eq!(
            cell,
            engine.cell_edit_range(a, source).expect("cell a"),
            "body Cmd+A in a table must select the cell, like overlay Cmd+A selects the draft"
        );
        assert_ne!(cell, 0..source.len(), "must not SelectAll the document");
        assert!(
            !source[cell.clone()].contains('|'),
            "cell SelectAll must not include `|`"
        );
        assert!(
            table_select_all_range(&engine, source, &cell, None).is_none(),
            "second Cmd-A falls through to the document"
        );
    }

    #[test]
    fn overlay_insert_and_backspace_respect_inner_selection() {
        let mut edit = caption();
        edit.set_caret(1);
        let mut anchor = 3;
        let body = "hello";
        insert_into_widget(&mut edit, &mut anchor, "x");
        match &edit {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "cx", "typing must replace the inner selection");
                assert_eq!(*caret, 2);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        assert_eq!(anchor, 2);
        assert_eq!(body, "hello");

        let mut selected = caption();
        selected.set_caret(1);
        let mut sel_anchor = 3;
        delete_before_in_widget(&mut selected, &mut sel_anchor);
        match &selected {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(
                    draft, "c",
                    "Backspace on a non-empty inner selection must delete the range"
                );
                assert_eq!(*caret, 1);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        assert_eq!(sel_anchor, 1);
        assert_eq!(body, "hello");

        let mut del = caption();
        del.set_caret(0);
        let mut del_anchor = 2;
        delete_after_in_widget(&mut del, &mut del_anchor);
        match &del {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(
                    draft, "t",
                    "Delete on a non-empty inner selection must delete the range"
                );
                assert_eq!(*caret, 0);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }

        let mut collapsed = caption();
        collapsed.set_caret(3);
        let mut col_anchor = 3;
        delete_before_in_widget(&mut collapsed, &mut col_anchor);
        match &collapsed {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "ca", "collapsed Backspace still deletes one char");
                assert_eq!(*caret, 2);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
    }

    #[test]
    fn overlay_editing_keeps_extended_graphemes_atomic() {
        for (cluster, label) in [
            ("e\u{301}", "combining accent"),
            ("👩\u{200d}💻", "ZWJ emoji"),
        ] {
            let draft = format!("{cluster}x");
            let mut edit = WidgetEdit::ImageAlt {
                range: 0..0,
                draft: draft.clone(),
                caret: 0,
            };

            // A click/IME offset may be a scalar boundary within a single
            // visible glyph. It must normalize to a real caret boundary.
            edit.set_caret(cluster.chars().next().expect(label).len_utf8());
            assert_eq!(
                edit.caret(),
                0,
                "inner {label} boundary must not become a visible caret stop"
            );

            let mut anchor = edit.caret();
            assert!(move_in_widget(
                &mut edit,
                &mut anchor,
                CaretMove::Right,
                false
            ));
            assert_eq!(edit.caret(), cluster.len(), "Right skips one {label}");
            assert!(move_in_widget(
                &mut edit,
                &mut anchor,
                CaretMove::Left,
                false
            ));
            assert_eq!(edit.caret(), 0, "Left skips one {label}");

            edit.set_caret(cluster.len());
            anchor = edit.caret();
            delete_before_in_widget(&mut edit, &mut anchor);
            match &edit {
                WidgetEdit::ImageAlt { draft, caret, .. } => {
                    assert_eq!(draft, "x", "Backspace removes one {label}");
                    assert_eq!(*caret, 0);
                }
                other => panic!("expected image caption, got {other:?}"),
            }

            let mut delete = WidgetEdit::ImageAlt {
                range: 0..0,
                draft,
                caret: 0,
            };
            let mut delete_anchor = 0;
            delete_after_in_widget(&mut delete, &mut delete_anchor);
            match &delete {
                WidgetEdit::ImageAlt { draft, caret, .. } => {
                    assert_eq!(draft, "x", "Delete removes one {label}");
                    assert_eq!(*caret, 0);
                }
                other => panic!("expected image caption, got {other:?}"),
            }
        }
    }

    #[test]
    fn overlay_undo_rewinds_draft_not_the_body() {
        let mut edit = caption();
        let mut anchor = edit.caret();
        let mut undo = Vec::new();
        let mut redo = Vec::new();
        let body = "hello";
        push_widget_history(&edit, anchor, &mut undo, &mut redo);
        insert_into_widget(&mut edit, &mut anchor, "!");
        match &edit {
            WidgetEdit::ImageAlt { draft, .. } => assert_eq!(draft, "cat!"),
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        assert!(undo_widget_history(
            &mut edit,
            &mut anchor,
            &mut undo,
            &mut redo
        ));
        match &edit {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "cat", "overlay undo must restore the draft");
                assert_eq!(*caret, 3);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        assert_eq!(body, "hello", "overlay undo must not mutate the body");
        assert!(
            !undo_widget_history(&mut edit, &mut anchor, &mut undo, &mut redo),
            "empty overlay undo must not fall through to the document"
        );
        assert!(redo_widget_history(
            &mut edit,
            &mut anchor,
            &mut undo,
            &mut redo
        ));
        match &edit {
            WidgetEdit::ImageAlt { draft, .. } => assert_eq!(draft, "cat!"),
            other => panic!("expected ImageAlt, got {other:?}"),
        }

        let mut idle_anchor = 0;
        let mut idle_undo = Vec::new();
        let mut idle_redo = Vec::new();
        assert!(!undo_widget_history(
            &mut WidgetEdit::Idle,
            &mut idle_anchor,
            &mut idle_undo,
            &mut idle_redo
        ));
    }

    #[test]
    fn overlay_yaml_up_down_moves_between_lines() {
        let mut edit = WidgetEdit::FrontmatterYaml {
            draft: "ab\ncd".into(),
            caret: 4,
        };
        let mut anchor = 4;
        let body = 12..12;
        assert!(move_in_widget(&mut edit, &mut anchor, CaretMove::Up, false));
        assert_eq!(edit.caret(), 1, "YAML Up stays on the same column");
        assert_eq!(anchor, 1);
        assert_eq!(body, 12..12, "overlay Up must not move the body caret");
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Down,
            false
        ));
        assert_eq!(edit.caret(), 4);
        assert_eq!(body, 12..12);

        let mut caption_edit = caption();
        let mut cap_anchor = 3;
        assert!(move_in_widget(
            &mut caption_edit,
            &mut cap_anchor,
            CaretMove::Up,
            false
        ));
        assert_eq!(caption_edit.caret(), 3, "single-line overlay Up is a no-op");
    }

    #[test]
    fn overlay_vertical_navigation_preserves_grapheme_column() {
        let combining = "e\u{301}";
        let draft = format!("{combining}\nab");
        let mut edit = WidgetEdit::FrontmatterYaml {
            draft,
            caret: combining.len(),
        };
        let mut anchor = edit.caret();

        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Down,
            false
        ));
        assert_eq!(
            edit.caret(),
            combining.len() + 2,
            "Down from one visible combining glyph must land after one visible glyph"
        );

        assert!(move_in_widget(&mut edit, &mut anchor, CaretMove::Up, false));
        assert_eq!(
            edit.caret(),
            combining.len(),
            "Up must return to the end of the same visible grapheme"
        );
    }

    #[test]
    fn overlay_click_does_not_match_idle_as_focused() {
        assert!(!WidgetEdit::Idle.matches_overlay(&OverlayTarget::CodeInfo(NodeId(1))));
        assert!(chip().matches_overlay(&OverlayTarget::CodeInfo(NodeId(1))));
        assert!(caption().matches_overlay(&OverlayTarget::ImageAlt {
            range: 0..8,
            stored: "cat".into(),
        }));
        assert!(
            frontmatter_title().matches_overlay(&OverlayTarget::Frontmatter {
                key: "title",
                stored: "Hi".into(),
            })
        );
        assert!(yaml().matches_overlay(&OverlayTarget::FrontmatterYaml {
            stored: "title: Hi".into(),
        }));
    }

    #[test]
    fn widget_draft_with_caret_paints_bar_at_inner_offset() {
        assert_eq!(widget_draft_with_caret("****", 2, ""), "**|**");
        assert_eq!(widget_draft_with_caret("cat", 3, "x"), "catx|");
    }
}
