// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use gpui::{
    actions, div, prelude::*, px, uniform_list, App, Context, Entity, ExternalPaths, FocusHandle,
    Focusable, FontWeight, PathPromptOptions, PromptButton, PromptLevel, Render, Role,
    ScrollHandle, SharedString, Subscription, UniformListScrollHandle, Window,
};
use markrust_core::parse_frontmatter;
use markrust_core::rich::{RichCommand, RichOutcome};
use markrust_core::Document;
use markrust_editor::outline_headings;
use markrust_editor::theme::EditorTheme;
use markrust_editor::EditorCommand;
use markrust_editor::{MarkdownEditor, MarkdownEditorView};
use std::collections::VecDeque;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use unicode_segmentation::UnicodeSegmentation;

use crate::config::HighlightStyle;
use crate::crash::{self, OpenOrigin, OpenTarget};
use crate::icons::Icon;
use crate::menus::{self, MenuState};
use crate::panels::{Panel, OUTLINE_WIDTH, SIDEBAR_WIDTH};
use crate::session::{DropTarget, WorkspaceCommand};
use crate::ui::{
    context_hint, document_tab, empty_sidebar_state, muted_hint, outline_row, panel_layer,
    section_header, sidebar_row, toolbar_icon_button, toolbar_scroll_button, ToolbarState,
};
use crate::workspace::{
    fuzzy_match, EditingPane, EditorMode, ExternalResolution, ExternalReview, NormalizationReview,
    SaveStatus, Workspace,
};

actions!(
    markrust_app,
    [
        Save,
        SaveAs,
        NormalizeMarkdown,
        InsertTableRowBelow,
        InsertTableRowAbove,
        DeleteTableRow,
        InsertTableColumnRight,
        InsertTableColumnLeft,
        DeleteTableColumn,
        OpenFile,
        OpenFolder,
        OpenPath,
        NewDocument,
        NewTab,
        NextTab,
        PreviousTab,
        Find,
        FindNext,
        FindPrevious,
        CloseTab,
        ToggleTheme,
        HighlightNative,
        HighlightOcean,
        HighlightForest,
        ToggleSidebar,
        ToggleOutline,
        CommandPalette,
        ExportHtml,
        LoadRemoteImages,
        Undo,
        Redo,
        ToggleEditorMode,
        ToggleMarkupHints,
        ShowWysiwyg,
        ShowSource,
        ShowSplit,
        Paste,
        About,
        Help,
        Minimize,
        Zoom,
        ToggleFullScreen
    ]
);

#[derive(Clone)]
enum ReviewKind {
    Normalization(NormalizationReview),
    External(ExternalReview),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReviewDisplay {
    Changes,
    SideBySide,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReviewTone {
    Context,
    Removed,
    Added,
    Header,
}

#[derive(Clone)]
struct ReviewLine {
    text: SharedString,
    tone: ReviewTone,
}

struct ReviewPane {
    lines: Arc<Vec<ReviewLine>>,
    vertical: UniformListScrollHandle,
    horizontal: ScrollHandle,
    content_width: f32,
}

impl ReviewPane {
    fn new(lines: Vec<ReviewLine>) -> Self {
        let content_width =
            lines.iter().map(|line| line.text.len()).max().unwrap_or(0) as f32 * 9. + 32.;
        Self {
            lines: Arc::new(lines),
            vertical: UniformListScrollHandle::new(),
            horizontal: ScrollHandle::new(),
            content_width,
        }
    }
}

struct DocumentReview {
    kind: ReviewKind,
    display: ReviewDisplay,
    title: String,
    window: gpui::AnyWindowHandle,
    focus: FocusHandle,
    controls: Vec<FocusHandle>,
    shell_bounds: ScrollHandle,
    body_bounds: ScrollHandle,
    footer_bounds: ScrollHandle,
    control_bounds: Vec<ScrollHandle>,
    changes: ReviewPane,
    before: ReviewPane,
    after: ReviewPane,
    error: Option<String>,
    #[cfg(feature = "gui-tests")]
    native_input_sink_registered: bool,
}

impl DocumentReview {
    fn new(
        kind: ReviewKind,
        title: String,
        window: &Window,
        cx: &mut Context<MarkRustWindow>,
    ) -> Self {
        let (before, after) = match &kind {
            ReviewKind::Normalization(review) => (&review.original, &review.normalized),
            ReviewKind::External(review) => (&review.ours, &review.theirs),
        };
        let external = matches!(kind, ReviewKind::External(_));
        Self {
            display: if external {
                ReviewDisplay::SideBySide
            } else {
                ReviewDisplay::Changes
            },
            changes: ReviewPane::new(review_diff_lines(before, after)),
            before: ReviewPane::new(review_source_lines(before)),
            after: ReviewPane::new(review_source_lines(after)),
            kind,
            title,
            window: window.window_handle(),
            focus: cx.focus_handle(),
            controls: (0..if external { 6 } else { 4 })
                .map(|_| cx.focus_handle())
                .collect(),
            shell_bounds: ScrollHandle::new(),
            body_bounds: ScrollHandle::new(),
            footer_bounds: ScrollHandle::new(),
            control_bounds: (0..if external { 6 } else { 4 })
                .map(|_| ScrollHandle::new())
                .collect(),
            error: None,
            #[cfg(feature = "gui-tests")]
            native_input_sink_registered: false,
        }
    }
}

#[cfg(feature = "gui-tests")]
pub(crate) struct ReviewBounds {
    pub shell: gpui::Bounds<gpui::Pixels>,
    pub body: gpui::Bounds<gpui::Pixels>,
    pub footer: gpui::Bounds<gpui::Pixels>,
    pub controls: Vec<gpui::Bounds<gpui::Pixels>>,
    pub panes: [gpui::Bounds<gpui::Pixels>; 3],
}

#[derive(Clone)]
enum PaletteTarget {
    Mode(EditorMode),
    NewTab,
    Save,
    ExportHtml,
    LoadRemoteImages,
    Tab(usize),
}

#[derive(Clone)]
struct PaletteEntry {
    label: String,
    target: PaletteTarget,
}

#[derive(Clone)]
struct PaletteInputGeometry {
    line: gpui::ShapedLine,
    origin: gpui::Point<gpui::Pixels>,
    bounds: gpui::Bounds<gpui::Pixels>,
}

#[cfg(feature = "gui-tests")]
pub(crate) struct PaletteState {
    pub open: bool,
    pub focused: bool,
    pub query: String,
    pub query_selection: Range<usize>,
    pub selection_reversed: bool,
    pub marked_range: Option<Range<usize>>,
    pub selected_result: usize,
    pub results: Vec<String>,
    pub bounds: Option<gpui::Bounds<gpui::Pixels>>,
    pub input_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    pub native_input_registered: bool,
}

struct FindOrigin {
    tab_id: usize,
    pane: EditingPane,
    source_scroll: gpui::Point<gpui::Pixels>,
    rich_scroll: gpui::ListOffset,
}

struct DocumentFind {
    query_document: Entity<Document>,
    query_editor: Entity<MarkdownEditor>,
    query_view: Entity<MarkdownEditorView>,
    origin: FindOrigin,
    revision: u64,
    query: String,
    matches: Vec<Range<usize>>,
    active: Option<usize>,
    paused: bool,
    bar_bounds: ScrollHandle,
    input_bounds: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

struct OpenLocation {
    document: Entity<Document>,
    editor: Entity<MarkdownEditor>,
    view: Entity<MarkdownEditorView>,
    base: PathBuf,
    error: Option<String>,
    bounds: ScrollHandle,
    input_bounds: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

#[cfg(feature = "gui-tests")]
pub(crate) struct OpenPathTestState {
    pub query: String,
    pub query_selection: Range<usize>,
    pub error: Option<String>,
    pub focused: bool,
    pub bar_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    pub input_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    pub native_input_registered: bool,
}

#[cfg(feature = "gui-tests")]
pub(crate) struct FindTestState {
    pub focused: bool,
    pub query: String,
    pub query_selection: Range<usize>,
    pub marked_range: Option<Range<usize>>,
    pub current: Option<Range<usize>>,
    pub matches: Vec<Range<usize>>,
    pub tab_id: usize,
    pub revision: u64,
    pub pane: EditingPane,
    pub input_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    pub bar_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    pub native_input_registered: bool,
}

pub struct MarkRustWindow {
    pub workspace: Entity<Workspace>,
    pub palette_query: String,
    pub palette_selection: usize,
    pub focus_handle: FocusHandle,
    palette_focus: FocusHandle,
    palette_query_selection: Range<usize>,
    palette_query_reversed: bool,
    palette_marked_range: Option<Range<usize>>,
    palette_dragging: bool,
    palette_bounds: ScrollHandle,
    palette_results_scroll: UniformListScrollHandle,
    palette_input_geometry: Option<PaletteInputGeometry>,
    palette_cursor_visible: bool,
    palette_blink_task: gpui::Task<()>,
    #[cfg(feature = "gui-tests")]
    palette_native_input_registered: bool,
    tab_scroll: ScrollHandle,
    format_scroll: ScrollHandle,
    last_tab_reveal: Option<(usize, usize, f32)>,
    welcome_dismissed: bool,
    find: Option<DocumentFind>,
    last_find_query: String,
    open_location: Option<OpenLocation>,
    review: Option<DocumentReview>,
    pending_external_opens: VecDeque<(PathBuf, Option<OpenOrigin>)>,
    pending_application_dialogs: usize,
    application_notice: Option<SharedString>,
    _application_activation_subscription: Option<Subscription>,
    _workspace_subscription: Subscription,
    _editor_input_subscription: Subscription,
    overlay_document_subscription: Option<(usize, Subscription)>,
}

impl MarkRustWindow {
    pub(crate) fn can_restart_for_update(&self, cx: &App) -> bool {
        self.pending_application_dialogs == 0
            && self.pending_external_opens.is_empty()
            && self.review.is_none()
            && self.open_location.is_none()
            && self.palette_marked_range.is_none()
            && self
                .find
                .as_ref()
                .is_none_or(|find| find.query_editor.read(cx).marked_range.is_none())
            && self.workspace.read(cx).tabs.iter().all(|tab| {
                tab.editor.read(cx).marked_range.is_none()
                    && !tab.rich_view.read(cx).has_pending_composition()
                    && !tab.rich_view.read(cx).has_image_editor()
            })
    }

    pub fn new(workspace: Entity<Workspace>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&workspace, |_, _, cx| cx.notify());
        let view = cx.weak_entity();
        let editor_input_subscription = cx.intercept_keystrokes(move |event, window, cx| {
            if crate::update_ui::is_installing(cx) {
                cx.stop_propagation();
                return;
            }
            let handled = view
                .update(cx, |this, cx| {
                    this.review_keystroke(&event.keystroke, window, cx)
                        || this.open_path_keystroke(&event.keystroke, window, cx)
                        || this.palette_keystroke(&event.keystroke, window, cx)
                        || this.find_keystroke(&event.keystroke, window, cx)
                })
                .unwrap_or(false);
            if handled {
                cx.stop_propagation();
                return;
            }
            if !keystroke_dismisses_editor_overlay(&event.keystroke) {
                return;
            }
            let _ = view.update(cx, |this, cx| {
                let editor_focused = this.workspace.read(cx).active_tab().is_some_and(|tab| {
                    tab.editor.read(cx).focus_handle(cx).is_focused(window)
                        || tab.rich_view.read(cx).is_focused(window)
                });
                if editor_focused {
                    this.dismiss_panel_overlay(window, cx);
                }
            });
        });
        Self {
            workspace,
            palette_query: String::new(),
            palette_selection: 0,
            focus_handle: cx.focus_handle(),
            palette_focus: cx.focus_handle(),
            palette_query_selection: 0..0,
            palette_query_reversed: false,
            palette_marked_range: None,
            palette_dragging: false,
            palette_bounds: ScrollHandle::new(),
            palette_results_scroll: UniformListScrollHandle::new(),
            palette_input_geometry: None,
            palette_cursor_visible: true,
            palette_blink_task: gpui::Task::ready(()),
            #[cfg(feature = "gui-tests")]
            palette_native_input_registered: false,
            tab_scroll: ScrollHandle::new(),
            format_scroll: ScrollHandle::new(),
            last_tab_reveal: None,
            welcome_dismissed: false,
            find: None,
            last_find_query: String::new(),
            open_location: None,
            review: None,
            pending_external_opens: VecDeque::new(),
            pending_application_dialogs: 0,
            application_notice: None,
            _application_activation_subscription: None,
            _workspace_subscription: subscription,
            _editor_input_subscription: editor_input_subscription,
            overlay_document_subscription: None,
        }
    }

    pub(crate) fn attach_application_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._application_activation_subscription =
            Some(cx.observe_window_activation(window, |_, window, cx| {
                if window.is_window_active() {
                    crate::app::note_window_activated(window.window_handle(), cx);
                    cx.notify();
                }
            }));
        let view = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            view.update(cx, |view, cx| view.checkpoint_window_close(cx))
                .unwrap_or(false)
        });
    }

    fn checkpoint_window_close(&mut self, cx: &mut Context<Self>) -> bool {
        // Keep a Finder request owned by this window until its review/dialog finishes.
        if self.pending_application_dialogs > 0 || !self.pending_external_opens.is_empty() {
            self.application_notice =
                Some("Finish the review or file dialog before closing this window.".into());
            if let Some(review) = &mut self.review {
                review.error = Some("Files are waiting to open in this window. Apply or cancel this review before closing it.".into());
            }
            cx.notify();
            return false;
        }
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.checkpoint_before_window_close(cx)
            })
            .is_ok()
    }

    pub(crate) fn focus_visible_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(review) = &self.review {
            window.focus(&review.controls[2], cx);
        } else if let Some(location) = &self.open_location {
            location
                .editor
                .read(cx)
                .focus_handle
                .clone()
                .focus(window, cx);
        } else if self.workspace.read(cx).palette_open {
            window.focus(&self.palette_focus, cx);
        } else if self.pending_application_dialogs == 0 {
            self.workspace.update(cx, |workspace, cx| {
                workspace.focus_active_editor(window, cx)
            });
        }
        cx.notify();
    }

    pub(crate) fn open_external_path(
        &mut self,
        path: PathBuf,
        origin: OpenOrigin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        if self.review.is_some()
            || self.pending_application_dialogs > 0
            || self.open_location.is_some()
        {
            self.pending_external_opens.push_back((path, Some(origin)));
            self.focus_visible_surface(window, cx);
            return Ok(());
        }
        self.open_owned_path(path, Some(origin), window, cx)
    }

    fn open_owned_path(
        &mut self,
        path: PathBuf,
        origin: Option<OpenOrigin>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let target = if path.is_dir() {
            OpenTarget::Folder
        } else {
            OpenTarget::File
        };
        if let Some(origin) = origin {
            crash::record_open_started(origin, target);
        }
        let result = self.workspace.update(cx, |workspace, cx| {
            workspace.open_launch_path(path, window, cx)
        });
        if let Some(origin) = origin {
            match &result {
                Ok(()) => crash::record_open_succeeded(origin, target),
                Err(error) => crash::record_open_failed(origin, target, error),
            }
        }
        if result.is_ok() {
            self.focus_visible_surface(window, cx);
        }
        result
    }

    fn drain_external_opens(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while self.review.is_none()
            && self.open_location.is_none()
            && self.pending_application_dialogs == 0
        {
            let Some((path, origin)) = self.pending_external_opens.pop_front() else {
                break;
            };
            if let Err(error) = self.open_owned_path(path, origin, window, cx) {
                drop(window.prompt(
                    PromptLevel::Critical,
                    "Could not open document",
                    Some(&error.to_string()),
                    &[PromptButton::Ok("OK".into())],
                    cx,
                ));
            }
        }
        if self.pending_external_opens.is_empty() && self.pending_application_dialogs == 0 {
            self.application_notice = None;
        }
        cx.notify();
    }

    #[cfg(feature = "gui-tests")]
    pub(crate) fn test_pending_external_open_count(&self) -> usize {
        self.pending_external_opens.len()
    }

    #[cfg(feature = "gui-tests")]
    pub(crate) fn test_has_application_dialog(&self) -> bool {
        self.pending_application_dialogs > 0
    }

    #[cfg(feature = "gui-tests")]
    pub(crate) fn test_close_application_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.checkpoint_window_close(cx) {
            return false;
        }
        window.remove_window();
        true
    }

    fn dismiss_panel_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panel = self.workspace.read(cx).panel_overlay;
        if let Some(panel) = panel {
            self.workspace.update(cx, |workspace, cx| {
                workspace.toggle_panel(panel, f32::from(window.viewport_size().width), cx);
            });
        }
    }

    fn save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() {
            return;
        }
        let Some((tab_id, document)) = self
            .workspace
            .read(cx)
            .active_tab()
            .map(|tab| (tab.id, tab.document.clone()))
        else {
            return;
        };
        let result = self.workspace.update(cx, |workspace, cx| {
            workspace.save_document_checked(document, cx)
        });
        match result {
            Ok(SaveStatus::Saved) => {}
            Ok(SaveStatus::Untitled) => self.save_as(&SaveAs, window, cx),
            Ok(SaveStatus::NeedsReview) => self.open_external_review(tab_id, window, cx),
            Err(error) => prompt_save_failure(&error, window, cx),
        }
    }

    fn normalize_markdown(
        &mut self,
        _: &NormalizeMarkdown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.review.is_some() {
            return;
        }
        let Some((title, document)) = self
            .workspace
            .read(cx)
            .active_tab()
            .map(|tab| (tab.title.clone(), tab.document.clone()))
        else {
            return;
        };
        let result = self.workspace.update(cx, |workspace, cx| {
            workspace.prepare_normalization_review(document, cx)
        });
        match result {
            Ok(review) => self.open_review(ReviewKind::Normalization(review), title, window, cx),
            Err(error) => prompt_review_failure(&error, window, cx),
        }
    }

    /// Apply a structural table command only through the live rich-text owner.
    /// A Source pane (including Source-focused Split view) never receives a
    /// fallback text edit from a Table menu action.
    fn apply_active_table_command(
        &mut self,
        command: RichCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.review.is_some() {
            return;
        }
        let changed = self.workspace.update(cx, |workspace, cx| {
            let Some(tab) = workspace.active_tab() else {
                return false;
            };
            if tab.active_editing_pane(window, cx) != EditingPane::Wysiwyg {
                return false;
            }
            tab.rich_view.update(cx, |view, cx| {
                view.apply_rich(command, cx) == RichOutcome::Changed
            })
        });
        if !changed {
            window.play_system_bell();
        }
    }

    fn insert_table_row_below(
        &mut self,
        _: &InsertTableRowBelow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_active_table_command(RichCommand::InsertTableRow { after: true }, window, cx);
    }

    fn insert_table_row_above(
        &mut self,
        _: &InsertTableRowAbove,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_active_table_command(RichCommand::InsertTableRow { after: false }, window, cx);
    }

    fn delete_table_row(
        &mut self,
        _: &DeleteTableRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_active_table_command(RichCommand::DeleteTableRow, window, cx);
    }

    fn insert_table_column_right(
        &mut self,
        _: &InsertTableColumnRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_active_table_command(RichCommand::InsertTableColumn { after: true }, window, cx);
    }

    fn insert_table_column_left(
        &mut self,
        _: &InsertTableColumnLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_active_table_command(
            RichCommand::InsertTableColumn { after: false },
            window,
            cx,
        );
    }

    fn delete_table_column(
        &mut self,
        _: &DeleteTableColumn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_active_table_command(RichCommand::DeleteTableColumn, window, cx);
    }

    fn open_external_review(&mut self, tab_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() {
            return;
        }
        let result = self.workspace.update(cx, |workspace, cx| {
            workspace.prepare_external_review(tab_id, cx)
        });
        match result {
            Ok(Some(review)) => {
                let title = basename_label(&review.path);
                self.open_review(ReviewKind::External(review), title, window, cx);
            }
            Ok(None) => {}
            Err(error) => prompt_review_failure(&error, window, cx),
        }
    }

    fn open_review(
        &mut self,
        kind: ReviewKind,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_palette(false, window, cx);
        let review = DocumentReview::new(kind, title, window, cx);
        window.focus(
            if matches!(review.kind, ReviewKind::External(_)) {
                &review.controls[2]
            } else {
                &review.focus
            },
            cx,
        );
        self.review = Some(review);
        cx.notify();
    }

    fn cancel_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.take().is_some() {
            self.workspace.update(cx, |workspace, cx| {
                workspace.focus_active_editor(window, cx)
            });
            self.drain_external_opens(window, cx);
            cx.notify();
        }
    }

    fn review_control(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = self.review.as_mut() else {
            return;
        };
        if index >= review.controls.len() {
            return;
        }
        if index < 2 {
            review.display = if index == 0 {
                ReviewDisplay::Changes
            } else {
                ReviewDisplay::SideBySide
            };
            window.focus(&review.controls[index], cx);
            cx.notify();
            return;
        }
        if index == 2 {
            self.cancel_review(window, cx);
            return;
        }
        let kind = review.kind.clone();
        let result = match &kind {
            ReviewKind::Normalization(token) => self.workspace.update(cx, |workspace, cx| {
                workspace.apply_normalization_review(token, cx)
            }),
            ReviewKind::External(token) => {
                let resolution = match index {
                    3 => ExternalResolution::KeepLocal,
                    4 => ExternalResolution::UseDisk,
                    5 => ExternalResolution::KeepBoth,
                    _ => return,
                };
                self.workspace.update(cx, |workspace, cx| {
                    workspace.resolve_external_review(token, resolution, window, cx)
                })
            }
        };
        match result {
            Ok(()) => {
                if matches!(kind, ReviewKind::Normalization(_)) {
                    self.cancel_review(window, cx);
                } else {
                    // The workspace focuses the resolved tab; Keep Both may activate a new draft.
                    self.review = None;
                    self.drain_external_opens(window, cx);
                    cx.notify();
                }
            }
            Err(error) => {
                if let Some(review) = self.review.as_mut() {
                    review.error = Some(error.to_string());
                }
                cx.notify();
            }
        }
    }

    fn review_keystroke(
        &mut self,
        key: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(review) = self.review.as_ref() else {
            return false;
        };
        if review.window != window.window_handle() {
            return false;
        }
        // Explicit application Quit keeps the normal recovery/shutdown path.
        if key.modifiers.platform && key.key == "q" {
            return false;
        }
        // A new, isolated window does not mutate the draft owned by this review.
        if key.key == "n"
            && key.modifiers.shift
            && (key.modifiers.platform || key.modifiers.control)
        {
            return false;
        }
        if !review.focus.contains_focused(window, cx) {
            // A late event must not reopen the editor's input route behind the modal.
            window.focus(&review.controls[2], cx);
            cx.notify();
        }
        if key.key == "escape" {
            self.cancel_review(window, cx);
        } else if key.key == "tab" && !key.modifiers.control && !key.modifiers.platform {
            let current = review
                .controls
                .iter()
                .position(|focus| focus.is_focused(window));
            let next = review_focus_index(current, review.controls.len(), key.modifiers.shift);
            window.focus(&review.controls[next], cx);
            cx.notify();
        } else if key.key == "enter" || key.key == "space" {
            if let Some(index) = review
                .controls
                .iter()
                .position(|focus| focus.is_focused(window))
            {
                self.review_control(index, window, cx);
            }
        } else if matches!(
            key.key.as_str(),
            "up" | "down" | "pageup" | "pagedown" | "home" | "end" | "left" | "right"
        ) {
            for pane in if review.display == ReviewDisplay::Changes {
                vec![&review.changes]
            } else {
                vec![&review.before, &review.after]
            } {
                scroll_review_pane(pane, key);
            }
            cx.notify();
        }
        // A review owns keyboard input. Editor shortcuts cannot mutate the pinned draft behind it.
        true
    }

    fn guard_review_action<A: gpui::Action>(
        &mut self,
        _: &A,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if crate::update_ui::is_installing(cx) {
            cx.stop_propagation();
            return;
        }
        if let Some(review) = &self.review {
            window.focus(&review.controls[2], cx);
            cx.stop_propagation();
            cx.notify();
        } else if self.workspace.read(cx).palette_open {
            window.focus(&self.palette_focus, cx);
            cx.stop_propagation();
        } else if (self.find_input_focused(window, cx) || self.open_location.is_some())
            && std::any::TypeId::of::<A>() != std::any::TypeId::of::<markrust_editor::Cut>()
            && std::any::TypeId::of::<A>() != std::any::TypeId::of::<markrust_editor::SelectAll>()
        {
            cx.stop_propagation();
        } else {
            cx.propagate();
        }
    }

    fn guard_update_action<A: gpui::Action>(
        &mut self,
        _: &A,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if crate::update_ui::is_installing(cx) {
            cx.stop_propagation();
        } else {
            cx.propagate();
        }
    }

    fn render_review(
        &self,
        theme: &EditorTheme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(review) = self.review.as_ref() else {
            return div().hidden().into_any_element();
        };
        let external = matches!(review.kind, ReviewKind::External(_));
        let (width, height) = review_dimensions(
            f32::from(window.viewport_size().width),
            f32::from(window.viewport_size().height),
        );
        let title = if external {
            "Review external changes"
        } else {
            "Normalize Markdown"
        };
        let (before_label, after_label) = if external {
            ("Mine", "Disk")
        } else {
            ("Original", "Normalized")
        };
        let mut shell = div()
            .id("document-review")
            .role(Role::Group)
            .aria_label(theme.ui_text(title))
            .track_focus(&review.focus)
            .track_scroll(&review.shell_bounds)
            .key_context("DocumentReview")
            .w(px(width)).h(px(height)).min_w_0().min_h_0()
            .flex().flex_col().overflow_hidden().rounded_lg()
            .shadow_lg().bg(theme.chrome_bg).border_1().border_color(theme.separator)
            .child(div().id("review-header").flex_shrink_0().h(px(74.)).px_4().py_3()
                .child(div().font_weight(FontWeight::SEMIBOLD).child(theme.ui_text(title)))
                .child(div().mt_1().text_sm().text_color(theme.secondary_text).truncate()
                    .child(review.title.clone())))
            .child(div().flex_shrink_0().h(px(38.)).px_4().flex().items_center().gap_2()
                .border_b_1().border_color(theme.separator)
                .children([(0, "Changes", "review-view-changes"), (1, "Side by Side", "review-view-side-by-side")].into_iter().map(|(index, label, id)| {
                    let active = (index == 0) == (review.display == ReviewDisplay::Changes);
                    review_button(id, label, &review.controls[index], &review.control_bounds[index], active, theme, window)
                        .on_click(cx.listener(move |this, _, window, cx| this.review_control(index, window, cx)))
                }))
                .child(div().flex_1())
                .child(div().text_xs().text_color(theme.secondary_text).child("Scroll · ⇧ Scroll horizontally")))
            .child(div().id("review-body").track_scroll(&review.body_bounds).flex_1().min_h_0().min_w_0().overflow_hidden().flex()
                .child(if review.display == ReviewDisplay::Changes {
                    review_pane("review-changes", &review.changes, "Changes", theme).into_any_element()
                } else {
                    div().size_full().min_h_0().min_w_0().flex()
                        .child(review_pane("review-before", &review.before, before_label, theme))
                        .child(div().w(px(1.)).flex_shrink_0().bg(theme.separator))
                        .child(review_pane("review-after", &review.after, after_label, theme))
                        .into_any_element()
                }))
            .children(review.error.as_ref().map(|error| div()
                .id("review-error").flex_shrink_0().max_h(px(76.)).overflow_y_scroll()
                .px_4().py_2().text_sm().text_color(theme.text).bg(theme.accent.opacity(0.12))
                .child(error.clone())))
            .child(div().id("review-footer").track_scroll(&review.footer_bounds).flex_shrink_0().h(px(if external { 112. } else { 92. }))
                .px_4().py_3().border_t_1().border_color(theme.separator).flex().flex_col().justify_between()
                .child(div().text_xs().text_color(theme.secondary_text).child(if external {
                    "The other version is preserved in a new unsaved tab. Keep Both opens Mine as a separate draft. Nothing is written to disk."
                } else { "Applies an undoable change to this buffer only. Use Save separately to write the file." }))
                .child(div().flex().justify_end().items_center().gap_2()
                    .child(review_button("review-cancel", "Cancel", &review.controls[2], &review.control_bounds[2], false, theme, window)
                        .on_click(cx.listener(|this, _, window, cx| this.cancel_review(window, cx))))
                    .children(if external {
                        vec![(3, "Keep mine", "review-keep-mine"), (4, "Use disk", "review-use-disk"), (5, "Keep Both", "review-keep-both")]
                    } else { vec![(3, "Normalize", "review-apply")] }.into_iter().map(|(index, label, id)| {
                        review_button(id, label, &review.controls[index], &review.control_bounds[index], !external, theme, window)
                            .on_click(cx.listener(move |this, _, window, cx| this.review_control(index, window, cx)))
                    }))));
        // Native menu actions must not reach the editor behind a review either.
        for action in review_blocked_actions() {
            shell = shell.on_boxed_action(action.as_ref(), |_, _, _| {});
        }
        let input_view = cx.entity();
        let input_focus = review.focus.clone();
        let input_cancel = review.controls[2].clone();
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.28))
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(shell)
            .child(
                gpui::canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        // Replace GPUI's retained editor input handler even for native IME/emoji events
                        // that arrive without a keyboard event. A review has no editable text surface.
                        if !input_focus.contains_focused(window, cx) {
                            window.focus(&input_cancel, cx);
                        }
                        if let Some(focus) = window.focused(cx) {
                            window.handle_input(
                                &focus,
                                gpui::ElementInputHandler::new(bounds, input_view.clone()),
                                cx,
                            );
                            #[cfg(feature = "gui-tests")]
                            input_view.update(cx, |view, _| {
                                if let Some(review) = &mut view.review {
                                    review.native_input_sink_registered = true;
                                }
                            });
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .into_any_element()
    }

    #[cfg(feature = "gui-tests")]
    pub(crate) fn test_review_state(
        &self,
    ) -> Option<(&'static str, &'static str, usize, Option<&str>)> {
        self.review.as_ref().map(|review| {
            (
                if matches!(review.kind, ReviewKind::External(_)) {
                    "external"
                } else {
                    "normalization"
                },
                if review.display == ReviewDisplay::Changes {
                    "changes"
                } else {
                    "side-by-side"
                },
                review.changes.lines.len(),
                review.error.as_deref(),
            )
        })
    }

    #[cfg(feature = "gui-tests")]
    pub(crate) fn test_review_scroll_offsets(&self) -> Option<[(f32, f32); 3]> {
        self.review.as_ref().map(|review| {
            [&review.changes, &review.before, &review.after].map(|pane| {
                (
                    f32::from(pane.horizontal.offset().x),
                    f32::from(pane.vertical.0.borrow().base_handle.offset().y),
                )
            })
        })
    }

    #[cfg(feature = "gui-tests")]
    pub(crate) fn test_review_visible_ranges(&self) -> Option<[std::ops::Range<usize>; 3]> {
        self.review.as_ref().map(|review| {
            [&review.changes, &review.before, &review.after].map(|pane| {
                let scroll = &pane.vertical.0.borrow().base_handle;
                let start = (f32::from(scroll.offset().y).abs() / 20.).floor() as usize;
                let count = (f32::from(scroll.bounds().size.height) / 20.).ceil() as usize;
                start.min(pane.lines.len())..(start + count).min(pane.lines.len())
            })
        })
    }

    #[cfg(feature = "gui-tests")]
    pub(crate) fn test_review_bounds(&self) -> Option<ReviewBounds> {
        self.review.as_ref().map(|review| ReviewBounds {
            shell: review.shell_bounds.bounds(),
            body: review.body_bounds.bounds(),
            footer: review.footer_bounds.bounds(),
            controls: review
                .control_bounds
                .iter()
                .map(ScrollHandle::bounds)
                .collect(),
            panes: [&review.changes, &review.before, &review.after]
                .map(|pane| pane.vertical.0.borrow().base_handle.bounds()),
        })
    }

    #[cfg(feature = "gui-tests")]
    pub(crate) fn test_review_native_input_sink_registered(&self) -> bool {
        self.review
            .as_ref()
            .is_some_and(|review| review.native_input_sink_registered)
    }

    fn save_as(&mut self, _: &SaveAs, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() || self.pending_application_dialogs > 0 {
            return;
        }
        let workspace = self.workspace.read(cx);
        let Some(tab) = workspace.active_tab() else {
            return;
        };
        let document = tab.document.clone();
        let directory = document
            .read(cx)
            .path
            .as_ref()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .or_else(|| workspace.root.clone())
            .or_else(dirs::document_dir)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let suggested = if tab.title == "Untitled" {
            "Untitled.md"
        } else {
            &tab.title
        };
        let receiver = cx.prompt_for_new_path(&directory, Some(suggested));
        self.pending_application_dialogs += 1;
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |view, cx| {
            let result = receiver.await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.pending_application_dialogs =
                    this.pending_application_dialogs.saturating_sub(1);
                if let Ok(Ok(Some(path))) = result {
                    if this.review.is_some() {
                        return;
                    }
                    let result = workspace.update(cx, |workspace, cx| {
                        workspace.save_document_as(document.clone(), path.clone(), cx)
                    });
                    if let Err(error) = result {
                        if error.kind() == std::io::ErrorKind::WouldBlock {
                            let tab_id = workspace
                                .read(cx)
                                .tabs
                                .iter()
                                .find(|tab| tab.document == document)
                                .map(|tab| tab.id);
                            if let Some(tab_id) = tab_id {
                                this.open_external_review(tab_id, window, cx);
                                return;
                            }
                        }
                        prompt_save_failure(&error, window, cx);
                    }
                }
                this.drain_external_opens(window, cx);
            });
        })
        .detach();
    }

    fn open_file(&mut self, _: &OpenFile, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() || self.pending_application_dialogs > 0 {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(
                self.workspace
                    .read(cx)
                    .config
                    .editor_theme()
                    .ui_text("Open…")
                    .into(),
            ),
        });
        self.pending_application_dialogs += 1;
        cx.spawn_in(window, async move |view, cx| {
            let result = receiver.await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.pending_application_dialogs =
                    this.pending_application_dialogs.saturating_sub(1);
                if let Ok(Ok(Some(paths))) = result {
                    if let Some(path) = paths.into_iter().next() {
                        this.pending_external_opens.push_front((path, None));
                    }
                }
                this.drain_external_opens(window, cx);
            });
        })
        .detach();
    }

    fn open_folder(&mut self, _: &OpenFolder, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() || self.pending_application_dialogs > 0 {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(
                self.workspace
                    .read(cx)
                    .config
                    .editor_theme()
                    .ui_text("Open Folder…")
                    .into(),
            ),
        });
        self.pending_application_dialogs += 1;
        cx.spawn_in(window, async move |view, cx| {
            let result = receiver.await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.pending_application_dialogs =
                    this.pending_application_dialogs.saturating_sub(1);
                if let Ok(Ok(Some(paths))) = result {
                    if let Some(path) = paths.into_iter().next() {
                        this.pending_external_opens.push_front((path, None));
                    }
                }
                this.drain_external_opens(window, cx);
            });
        })
        .detach();
    }

    fn open_path(&mut self, _: &OpenPath, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() || self.pending_application_dialogs > 0 {
            return;
        }
        if let Some(location) = &self.open_location {
            location
                .editor
                .read(cx)
                .focus_handle
                .clone()
                .focus(window, cx);
            return;
        }
        let safe = self.workspace.read(cx).active_tab().is_some_and(|tab| {
            let rich = tab.rich_view.read(cx);
            !rich.has_pending_widget_edit()
                && !rich.has_pending_composition()
                && tab.editor.read(cx).marked_range.is_none()
        });
        if !safe {
            window.play_system_bell();
            return;
        }
        self.dismiss_palette(false, window, cx);
        if self.find.is_some() {
            self.dismiss_find(window, cx);
        }
        self.workspace.update(cx, |workspace, cx| {
            workspace.remember_active_editing_pane(window, cx)
        });
        let base = {
            let workspace = self.workspace.read(cx);
            workspace
                .root
                .clone()
                .or_else(|| {
                    workspace.active_tab().and_then(|tab| {
                        tab.document
                            .read(cx)
                            .path
                            .as_ref()
                            .and_then(|path| path.parent().map(Path::to_path_buf))
                    })
                })
                .or_else(dirs::document_dir)
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."))
        };
        let document = cx.new(|_| Document::plain_text(&base.to_string_lossy()));
        let mut theme = self.workspace.read(cx).config.editor_theme();
        theme.font_size = 14.;
        theme.background = theme.editor_bg;
        let editor = cx.new(|cx| MarkdownEditor::new(document.clone(), theme, window, cx));
        let view = cx.new(|_| MarkdownEditorView::new(editor.clone()));
        let subscription = cx.observe(&document, |this, _, cx| {
            if let Some(location) = &mut this.open_location {
                location.error = None;
            }
            cx.notify();
        });
        let editor_subscription = cx.observe(&editor, |_, _, cx| cx.notify());
        self.open_location = Some(OpenLocation {
            document,
            editor: editor.clone(),
            view,
            base,
            error: None,
            bounds: ScrollHandle::new(),
            input_bounds: ScrollHandle::new(),
            _subscriptions: vec![subscription, editor_subscription],
        });
        editor.update(cx, |editor, cx| {
            editor.apply_command(EditorCommand::SelectAll, cx);
            editor.focus_handle.focus(window, cx);
        });
        cx.notify();
    }

    fn cancel_open_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_location.take().is_none() {
            return;
        }
        let source_view = self
            .workspace
            .read(cx)
            .active_tab()
            .map(|tab| tab.editor_view.clone());
        if let Some(view) = source_view {
            view.update(cx, |view, _| view.preserve_scroll_on_next_focus());
        }
        self.workspace.update(cx, |workspace, cx| {
            workspace.focus_active_editor(window, cx)
        });
        self.drain_external_opens(window, cx);
        cx.notify();
    }

    fn submit_open_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(location) = &self.open_location else {
            return;
        };
        let input = location.document.read(cx).buffer.content();
        let path = resolve_open_location(&input, &location.base, dirs::home_dir().as_deref());
        let result = path.and_then(|path| {
            self.open_owned_path(path, None, window, cx)
                .map_err(|error| error.to_string())
        });
        match result {
            Ok(()) => {
                self.open_location = None;
                self.focus_visible_surface(window, cx);
                self.drain_external_opens(window, cx);
            }
            Err(error) => {
                if let Some(location) = &mut self.open_location {
                    location.error = Some(error);
                    location
                        .editor
                        .read(cx)
                        .focus_handle
                        .clone()
                        .focus(window, cx);
                }
            }
        }
        cx.notify();
    }

    fn open_path_keystroke(
        &mut self,
        key: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(location) = &self.open_location else {
            return false;
        };
        if !location.editor.read(cx).focus_handle.is_focused(window) {
            location
                .editor
                .read(cx)
                .focus_handle
                .clone()
                .focus(window, cx);
        }
        if location.editor.read(cx).marked_range.is_some() {
            return false;
        }
        if key.key == "escape" {
            self.cancel_open_path(window, cx);
            return true;
        }
        if matches!(key.key.as_str(), "enter" | "return") {
            self.submit_open_path(window, cx);
            return true;
        }
        // Keep application commands from replacing a modal's input owner.
        if key.modifiers.platform || key.modifiers.control {
            return !matches!(
                key.key.as_str(),
                "a" | "c"
                    | "v"
                    | "x"
                    | "z"
                    | "y"
                    | "left"
                    | "right"
                    | "up"
                    | "down"
                    | "backspace"
                    | "delete"
            );
        }
        false
    }

    #[cfg(feature = "gui-tests")]
    pub(crate) fn test_open_path_state(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<OpenPathTestState> {
        let location = self.open_location.as_ref()?;
        let editor = location.editor.read(cx);
        Some(OpenPathTestState {
            query: location.document.read(cx).buffer.content(),
            query_selection: editor.selected_range.clone(),
            error: location.error.clone(),
            focused: editor.focus_handle.is_focused(window),
            bar_bounds: Some(location.bounds.bounds()),
            input_bounds: Some(location.input_bounds.bounds()),
            native_input_registered: location.view.read(cx).painted_geometry().revision.is_some(),
        })
    }

    fn render_open_path(
        &self,
        theme: &EditorTheme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(location) = &self.open_location else {
            return div().into_any_element();
        };
        let width = (f32::from(window.viewport_size().width) - 32.).clamp(0., 560.);
        let height = (f32::from(window.viewport_size().height) - 32.).max(0.);
        let mut shell = div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgba(0x00000066))
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("open-location")
                    .w(px(width))
                    .max_h(px(height))
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .gap_3()
                    .flex()
                    .flex_col()
                    .track_scroll(&location.bounds)
                    .rounded_lg()
                    .shadow_lg()
                    .border_1()
                    .border_color(theme.separator)
                    .bg(theme.chrome_bg)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(theme.ui_text("Open path")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.secondary_text)
                            .child(theme.ui_text("Enter a file or folder path.")),
                    )
                    .child(div().text_xs().text_color(theme.secondary_text).child(
                        SharedString::from(format!(
                            "{}: {}",
                            theme.ui_text("Folder"),
                            location.base.display()
                        )),
                    ))
                    .child(
                        div()
                            .id("open-location-input")
                            .role(Role::TextInput)
                            .aria_label(theme.ui_text("File or folder path"))
                            .h(px(34.))
                            .track_scroll(&location.input_bounds)
                            .border_1()
                            .border_color(theme.accent)
                            .rounded_md()
                            .overflow_hidden()
                            .child(location.view.clone()),
                    )
                    .when_some(location.error.clone(), |panel, error| {
                        panel.child(
                            div()
                                .id("open-location-error")
                                .text_sm()
                                .text_color(theme.link)
                                .child(theme.ui_text(&error)),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_3()
                            .child(
                                div()
                                    .id("open-location-cancel")
                                    .role(Role::Button)
                                    .aria_label(theme.ui_text("Cancel"))
                                    .cursor_pointer()
                                    .px_3()
                                    .py_1()
                                    .child(theme.ui_text("Cancel"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cancel_open_path(window, cx)
                                    })),
                            )
                            .child(
                                div()
                                    .id("open-location-open")
                                    .role(Role::Button)
                                    .aria_label(theme.ui_text("Open"))
                                    .cursor_pointer()
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .bg(theme.accent)
                                    .text_color(gpui::rgb(0xffffff))
                                    .child(theme.ui_text("Open"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_open_path(window, cx)
                                    })),
                            ),
                    ),
            );
        for action in review_blocked_actions() {
            if action.name() == <Paste as gpui::Action>::name_for_type()
                || action.name() == <Undo as gpui::Action>::name_for_type()
                || action.name() == <Redo as gpui::Action>::name_for_type()
                || action.name() == <markrust_editor::Cut as gpui::Action>::name_for_type()
                || action.name() == <markrust_editor::SelectAll as gpui::Action>::name_for_type()
            {
                continue;
            }
            shell = shell.on_boxed_action(action.as_ref(), |_, _, _| {});
        }
        shell.into_any_element()
    }

    fn new_document(&mut self, _: &NewDocument, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() {
            return;
        }
        self.welcome_dismissed = true;
        self.workspace.update(cx, |workspace, cx| {
            let _ = workspace.dispatch(WorkspaceCommand::NewDocument, window, cx);
        });
    }

    fn new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.new_document(&NewDocument, window, cx);
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(false, window, cx);
    }

    fn previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(true, window, cx);
    }

    fn cycle_tab(&mut self, previous: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() {
            return;
        }
        let target = {
            let workspace = self.workspace.read(cx);
            adjacent_tab(workspace.active_tab, workspace.tabs.len(), previous)
        };
        if let Some(index) = target {
            self.tab_scroll.scroll_to_item(index);
        }
        self.workspace.update(cx, |workspace, cx| {
            if let Some(index) = target {
                let _ = workspace.dispatch(WorkspaceCommand::SwitchTab(index), window, cx);
            }
        });
    }

    #[cfg(feature = "gui-tests")]
    pub(crate) fn test_tab_strip_state(
        &self,
        active: usize,
    ) -> (
        gpui::Bounds<gpui::Pixels>,
        Option<gpui::Bounds<gpui::Pixels>>,
    ) {
        let offset = self.tab_scroll.offset();
        (
            self.tab_scroll.bounds(),
            self.tab_scroll
                .bounds_for_item(active)
                .map(|bounds| gpui::Bounds::new(bounds.origin + offset, bounds.size)),
        )
    }

    #[cfg(feature = "gui-tests")]
    pub(crate) fn test_format_toolbar_state(
        &self,
    ) -> (
        gpui::Bounds<gpui::Pixels>,
        gpui::Point<gpui::Pixels>,
        gpui::Point<gpui::Pixels>,
    ) {
        (
            self.format_scroll.bounds(),
            self.format_scroll.offset(),
            self.format_scroll.max_offset(),
        )
    }

    fn scroll_format_tools(&mut self, forward: bool, cx: &mut Context<Self>) {
        let current = self.format_scroll.offset();
        let next = formatting_scroll_offset(
            f32::from(current.x),
            f32::from(self.format_scroll.max_offset().x),
            f32::from(self.format_scroll.bounds().size.width),
            forward,
        );
        self.format_scroll
            .set_offset(gpui::point(px(next), current.y));
        cx.notify();
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() {
            return;
        }
        self.workspace.update(cx, |workspace, cx| {
            let _ = workspace.dispatch(WorkspaceCommand::CloseTab, window, cx);
        });
    }

    fn toggle_editor_mode(
        &mut self,
        _: &ToggleEditorMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.review.is_some() {
            return;
        }
        self.workspace.update(cx, |workspace, cx| {
            workspace.toggle_editor_mode(window, cx);
        });
        cx.notify();
    }

    fn toggle_markup_hints(
        &mut self,
        _: &ToggleMarkupHints,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.review.is_some() {
            return;
        }
        self.workspace.update(cx, |workspace, cx| {
            workspace.toggle_markup_hints(cx);
        });
    }

    fn set_editor_mode(&mut self, mode: EditorMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() {
            return;
        }
        self.workspace.update(cx, |workspace, cx| {
            workspace.set_editor_mode(mode, window, cx)
        });
    }

    fn show_wysiwyg(&mut self, _: &ShowWysiwyg, window: &mut Window, cx: &mut Context<Self>) {
        self.set_editor_mode(EditorMode::Wysiwyg, window, cx);
    }

    fn show_source(&mut self, _: &ShowSource, window: &mut Window, cx: &mut Context<Self>) {
        self.set_editor_mode(EditorMode::Source, window, cx);
    }

    fn show_split(&mut self, _: &ShowSplit, window: &mut Window, cx: &mut Context<Self>) {
        self.set_editor_mode(EditorMode::Split, window, cx);
    }

    fn find_origin(&self, window: &Window, cx: &App) -> Option<FindOrigin> {
        let tab = self.workspace.read(cx).active_tab()?;
        Some(FindOrigin {
            tab_id: tab.id,
            pane: tab.active_editing_pane(window, cx),
            source_scroll: tab.editor_view.read(cx).scroll_offset(),
            rich_scroll: tab.rich_view.read(cx).scroll_anchor(),
        })
    }

    fn find_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.find
            .as_ref()
            .is_some_and(|find| find.query_editor.read(cx).focus_handle.is_focused(window))
    }

    fn find_document_available(&self, cx: &App) -> bool {
        self.workspace.read(cx).active_tab().is_some_and(|tab| {
            let rich = tab.rich_view.read(cx);
            !rich.has_pending_widget_edit()
                && !rich.has_pending_composition()
                && tab.editor.read(cx).marked_range.is_none()
        })
    }

    fn find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some()
            || self.pending_application_dialogs > 0
            || self.open_location.is_some()
        {
            return;
        }
        if let Some(find) = &self.find {
            find.query_editor.update(cx, |editor, cx| {
                editor.apply_command(EditorCommand::SelectAll, cx);
                editor.focus_handle.focus(window, cx);
            });
            return;
        }
        let safe = self.find_document_available(cx);
        if !safe {
            window.play_system_bell();
            return;
        }
        self.dismiss_palette(false, window, cx);
        self.workspace.update(cx, |workspace, cx| {
            workspace.remember_active_editing_pane(window, cx);
        });
        let Some(origin) = self.find_origin(window, cx) else {
            return;
        };
        let query = {
            let tab = self
                .workspace
                .read(cx)
                .active_tab()
                .expect("Find origin has a tab");
            let selection = match origin.pane {
                EditingPane::Source => tab.editor.read(cx).selected_range.clone(),
                EditingPane::Wysiwyg => tab.rich_view.read(cx).selected_range.clone(),
            };
            let source = tab.document.read(cx).buffer.content();
            source
                .get(selection)
                .filter(|text| !text.is_empty() && text.len() <= 256 && !text.contains('\n'))
                .map(str::to_owned)
                .unwrap_or_else(|| self.last_find_query.clone())
        };
        let query_document = cx.new(|_| Document::plain_text(&query));
        let mut theme = self.workspace.read(cx).config.editor_theme();
        theme.font_size = 14.;
        theme.background = theme.editor_bg;
        let query_editor =
            cx.new(|cx| MarkdownEditor::new(query_document.clone(), theme, window, cx));
        let query_view = cx.new(|_| MarkdownEditorView::new(query_editor.clone()));
        let subscription = cx.observe(&query_document, |_, _, cx| cx.notify());
        let editor_subscription = cx.observe(&query_editor, |_, _, cx| cx.notify());
        self.find = Some(DocumentFind {
            query_document,
            query_editor: query_editor.clone(),
            query_view,
            origin,
            revision: u64::MAX,
            query: String::new(),
            matches: Vec::new(),
            active: None,
            paused: false,
            bar_bounds: ScrollHandle::new(),
            input_bounds: ScrollHandle::new(),
            _subscriptions: vec![subscription, editor_subscription],
        });
        query_editor.update(cx, |editor, cx| {
            editor.apply_command(EditorCommand::SelectAll, cx);
            editor.focus_handle.focus(window, cx);
        });
        self.sync_find(window, cx);
        cx.notify();
    }

    fn sync_find(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_ref() else {
            return;
        };
        let query = find.query_document.read(cx).buffer.content();
        let paused = !self.find_document_available(cx);
        let Some(origin) = self.find_origin(window, cx) else {
            return;
        };
        let (revision, source, caret) = {
            let tab = self
                .workspace
                .read(cx)
                .active_tab()
                .expect("Find origin has a tab");
            let doc = tab.document.read(cx);
            let caret = match origin.pane {
                EditingPane::Source => tab.editor.read(cx).cursor_offset(),
                EditingPane::Wysiwyg => tab.rich_view.read(cx).cursor_offset(),
            };
            (doc.revision(), doc.buffer.content(), caret)
        };
        let find = self.find.as_mut().expect("Find is open");
        let switched = find.origin.tab_id != origin.tab_id || find.origin.pane != origin.pane;
        if !switched && find.revision == revision && find.query == query && find.paused == paused {
            return;
        }
        let at = if switched || find.query != query {
            caret
        } else {
            find.active
                .and_then(|index| find.matches.get(index))
                .map_or(caret, |range| range.start)
        };
        if switched {
            find.origin = origin;
        }
        find.matches = markrust_editor::search::literal_matches(&source, &query);
        find.active = initial_find_result(&find.matches, at);
        find.revision = revision;
        find.paused = paused;
        find.query = query;
        self.paint_find(cx);
    }

    fn paint_find(&self, cx: &mut Context<Self>) {
        let tabs: Vec<_> = self
            .workspace
            .read(cx)
            .tabs
            .iter()
            .map(|tab| (tab.id, tab.editor.clone(), tab.rich_view.clone()))
            .collect();
        for (id, source, rich) in tabs {
            let search = self
                .find
                .as_ref()
                .filter(|find| find.origin.tab_id == id && !find.paused)
                .map(|find| markrust_editor::search::SearchHighlights {
                    revision: find.revision,
                    ranges: find.matches.clone(),
                    active: find.active,
                });
            source.update(cx, |editor, cx| {
                editor.set_search_highlights(search.clone(), cx)
            });
            rich.update(cx, |editor, cx| editor.set_search_highlights(search, cx));
        }
    }

    fn move_find(&mut self, previous: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.find_document_available(cx) {
            window.play_system_bell();
            return;
        }
        let was_closed = self.find.is_none();
        if was_closed {
            if self.last_find_query.is_empty() {
                self.find(&Find, window, cx);
                return;
            }
            let retained_query = self.last_find_query.clone();
            self.find(&Find, window, cx);
            if let Some(find) = &self.find {
                find.query_editor.update(cx, |editor, cx| {
                    editor.apply_command(EditorCommand::SelectAll, cx);
                    editor.insert_text(&retained_query, window, cx);
                });
            }
        }
        self.sync_find(window, cx);
        let Some(find) = self.find.as_mut() else {
            return;
        };
        if was_closed {
            if previous {
                let caret =
                    self.workspace
                        .read(cx)
                        .active_tab()
                        .map_or(0, |tab| match find.origin.pane {
                            EditingPane::Source => tab.editor.read(cx).cursor_offset(),
                            EditingPane::Wysiwyg => tab.rich_view.read(cx).cursor_offset(),
                        });
                find.active = find
                    .matches
                    .iter()
                    .rposition(|range| range.end <= caret)
                    .or_else(|| find.matches.len().checked_sub(1));
            }
        } else {
            find.active = adjacent_find_result(find.active, find.matches.len(), previous);
        }
        let current = find
            .active
            .and_then(|index| find.matches.get(index))
            .cloned();
        let pane = find.origin.pane;
        if let Some(range) = current {
            let editors = self
                .workspace
                .read(cx)
                .active_tab()
                .map(|tab| (tab.editor.clone(), tab.rich_view.clone()));
            if let Some((source, rich)) = editors {
                match pane {
                    EditingPane::Source => {
                        source.update(cx, |editor, cx| editor.reveal_search_match(range.end, cx))
                    }
                    EditingPane::Wysiwyg => {
                        rich.update(cx, |editor, cx| editor.reveal_search_match(range.end, cx))
                    }
                }
            }
        } else if !find.query.is_empty() {
            window.play_system_bell();
        }
        self.paint_find(cx);
        if was_closed {
            self.dismiss_find(window, cx);
        }
        cx.notify();
    }

    fn find_next(&mut self, _: &FindNext, window: &mut Window, cx: &mut Context<Self>) {
        self.move_find(false, window, cx);
    }

    fn find_previous(&mut self, _: &FindPrevious, window: &mut Window, cx: &mut Context<Self>) {
        self.move_find(true, window, cx);
    }

    fn dismiss_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_find(window, cx);
        let Some(find) = self.find.take() else {
            return;
        };
        self.last_find_query = find.query;
        let current = find
            .active
            .and_then(|index| find.matches.get(index))
            .cloned();
        self.paint_find(cx);
        let editors = self
            .workspace
            .read(cx)
            .active_tab()
            .filter(|tab| tab.id == find.origin.tab_id)
            .map(|tab| {
                (
                    tab.editor.clone(),
                    tab.editor_view.clone(),
                    tab.rich_view.clone(),
                )
            });
        if let Some((source, source_view, rich)) = editors {
            match find.origin.pane {
                EditingPane::Source => {
                    if let Some(range) = current {
                        source.update(cx, |editor, cx| {
                            editor.apply_command(
                                EditorCommand::SetSelection {
                                    start: range.start,
                                    end: range.end,
                                },
                                cx,
                            );
                            editor.reveal_search_match(range.end, cx);
                        });
                        source_view.update(cx, |view, _| view.preserve_scroll_on_next_focus());
                    } else {
                        source_view.update(cx, |view, cx| {
                            view.restore_scroll_offset(find.origin.source_scroll, cx)
                        });
                    }
                    source.read(cx).focus_handle.clone().focus(window, cx);
                }
                EditingPane::Wysiwyg => {
                    rich.update(cx, |editor, cx| {
                        if let Some(range) = current {
                            editor.apply_editor_command(
                                EditorCommand::SetSelection {
                                    start: range.start,
                                    end: range.end,
                                },
                                cx,
                            );
                        } else {
                            editor.restore_scroll_anchor(find.origin.rich_scroll, cx);
                        }
                        editor.focus_current_input(window, cx);
                    });
                }
            }
        }
        cx.notify();
    }

    fn find_keystroke(
        &mut self,
        key: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.find.is_none() || self.review.is_some() || self.workspace.read(cx).palette_open {
            return false;
        }
        if self.find_input_focused(window, cx)
            && self
                .find
                .as_ref()
                .is_some_and(|find| find.query_editor.read(cx).marked_range.is_some())
        {
            return false;
        }
        if key.key == "escape" && !key.modifiers.platform && !key.modifiers.control {
            if !self.find_input_focused(window, cx) && !self.find_document_available(cx) {
                return false;
            }
            self.dismiss_find(window, cx);
            return true;
        }
        if !self.find_input_focused(window, cx) {
            return false;
        }
        if self
            .find
            .as_ref()
            .is_some_and(|find| find.query_editor.read(cx).marked_range.is_some())
        {
            return false;
        }
        if matches!(key.key.as_str(), "enter" | "return")
            && !key.modifiers.platform
            && !key.modifiers.control
        {
            self.move_find(key.modifiers.shift, window, cx);
            return true;
        }
        false
    }

    #[cfg(feature = "gui-tests")]
    pub(crate) fn test_find_state(&self, window: &Window, cx: &App) -> Option<FindTestState> {
        let find = self.find.as_ref()?;
        let query = find.query_editor.read(cx);
        Some(FindTestState {
            focused: query.focus_handle.is_focused(window),
            query: find.query.clone(),
            query_selection: query.selected_range.clone(),
            marked_range: query.marked_range.clone(),
            current: find
                .active
                .and_then(|index| find.matches.get(index))
                .cloned(),
            matches: find.matches.clone(),
            tab_id: find.origin.tab_id,
            revision: find.revision,
            pane: find.origin.pane,
            input_bounds: Some(find.input_bounds.bounds()),
            bar_bounds: Some(find.bar_bounds.bounds()),
            native_input_registered: find
                .query_view
                .read(cx)
                .painted_geometry()
                .revision
                .is_some(),
        })
    }

    fn render_find(
        &self,
        theme: &EditorTheme,
        editor_width: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(find) = &self.find else {
            return div().into_any_element();
        };
        if find.paused {
            return div()
                .id("document-find-paused")
                .absolute()
                .top(px(44.))
                .right(px(12.))
                .p_2()
                .rounded_md()
                .bg(theme.chrome_bg)
                .text_sm()
                .child(theme.ui_text("Finish the active field before searching."))
                .into_any_element();
        }
        let count = if find.query.is_empty() {
            theme.ui_text("Find in document")
        } else if find.matches.is_empty() {
            theme.ui_text("No matches")
        } else {
            theme
                .ui_text("{current} of {total}")
                .replace(
                    "{current}",
                    &find.active.map_or(0, |index| index + 1).to_string(),
                )
                .replace("{total}", &find.matches.len().to_string())
        };
        div()
            .id("document-find")
            .absolute()
            .top(px(44.))
            .right(px(12.))
            .w(px((editor_width - 24.).clamp(0., 480.)))
            .h(px(46.))
            .px_2()
            .gap_2()
            .flex()
            .items_center()
            .track_scroll(&find.bar_bounds)
            .bg(theme.chrome_bg)
            .border_1()
            .border_color(theme.separator)
            .rounded_md()
            .shadow_md()
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("document-find-query")
                    .role(Role::TextInput)
                    .aria_label(theme.ui_text("Find in document"))
                    .track_scroll(&find.input_bounds)
                    .flex_1()
                    .min_w_0()
                    .h(px(30.))
                    .border_1()
                    .border_color(theme.accent)
                    .rounded_sm()
                    .overflow_hidden()
                    .child(find.query_view.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.secondary_text)
                    .flex_shrink_0()
                    .child(SharedString::from(count)),
            )
            .child(
                div()
                    .id("find-previous")
                    .role(Role::Button)
                    .aria_label(theme.ui_text("Previous match"))
                    .cursor_pointer()
                    .px_1()
                    .child("↑")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.find_previous(&FindPrevious, window, cx)
                    })),
            )
            .child(
                div()
                    .id("find-next")
                    .role(Role::Button)
                    .aria_label(theme.ui_text("Next match"))
                    .cursor_pointer()
                    .px_1()
                    .child("↓")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.find_next(&FindNext, window, cx)),
                    ),
            )
            .child(
                div()
                    .id("find-close")
                    .role(Role::Button)
                    .aria_label(theme.ui_text("Close find"))
                    .cursor_pointer()
                    .px_1()
                    .child("×")
                    .on_click(cx.listener(|this, _, window, cx| this.dismiss_find(window, cx))),
            )
            .into_any_element()
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() {
            return;
        }
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            if let Some(location) = &self.open_location {
                location.editor.update(cx, |editor, cx| {
                    editor.insert_text(&palette_single_line(&text), window, cx)
                });
                return;
            }
            if self.find_input_focused(window, cx) {
                if let Some(find) = &self.find {
                    find.query_editor.update(cx, |editor, cx| {
                        editor.insert_text(&palette_single_line(&text), window, cx)
                    });
                }
                return;
            }
            if self.workspace.read(cx).palette_open {
                self.replace_palette_query(self.palette_query_selection.clone(), &text, cx);
                return;
            }
            self.workspace
                .update(cx, |workspace, cx| workspace.paste(&text, window, cx));
        }
    }

    fn about(&mut self, _: &About, window: &mut Window, cx: &mut Context<Self>) {
        let theme = self.workspace.read(cx).config.editor_theme();
        let details = crate::build_info::about_details()
            .replace("Version ", &format!("{} ", theme.ui_text("Version")))
            .replace("Built:", &format!("{}:", theme.ui_text("Built")))
            .replace(
                "A native Markdown writing app.",
                &theme.ui_text("A native Markdown writing app."),
            );
        let _response = window.prompt(
            PromptLevel::Info,
            &theme.ui_text("About MarkRust"),
            Some(&details),
            &[PromptButton::ok("OK")],
            cx,
        );
    }

    fn help(&mut self, _: &Help, _: &mut Window, cx: &mut Context<Self>) {
        cx.open_url("https://github.com/alexey-a-abramov/markrust#readme");
    }

    fn toggle_theme(&mut self, _: &ToggleTheme, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            let _ = workspace.dispatch(WorkspaceCommand::ToggleTheme, window, cx);
        });
    }

    fn highlight_native(&mut self, _: &HighlightNative, _: &mut Window, cx: &mut Context<Self>) {
        self.set_highlight_style(HighlightStyle::Native, cx);
    }

    fn highlight_ocean(&mut self, _: &HighlightOcean, _: &mut Window, cx: &mut Context<Self>) {
        self.set_highlight_style(HighlightStyle::Ocean, cx);
    }

    fn highlight_forest(&mut self, _: &HighlightForest, _: &mut Window, cx: &mut Context<Self>) {
        self.set_highlight_style(HighlightStyle::Forest, cx);
    }

    fn set_highlight_style(&mut self, style: HighlightStyle, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| workspace.set_highlight_style(style, cx));
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() {
            return;
        }
        self.workspace.update(cx, |workspace, cx| {
            workspace.toggle_panel(Panel::Sidebar, f32::from(window.viewport_size().width), cx);
        });
    }

    fn toggle_outline(&mut self, _: &ToggleOutline, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() {
            return;
        }
        self.workspace.update(cx, |workspace, cx| {
            workspace.toggle_panel(Panel::Outline, f32::from(window.viewport_size().width), cx);
        });
    }

    fn command_palette(&mut self, _: &CommandPalette, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() {
            return;
        }
        if self.workspace.read(cx).palette_open {
            self.dismiss_palette(true, window, cx);
            return;
        }
        self.workspace.update(cx, |workspace, cx| {
            if let Some(tab) = workspace.tabs.get_mut(workspace.active_tab) {
                tab.editing_pane = tab.active_editing_pane(window, cx);
            }
            workspace.palette_open = true;
            cx.notify();
        });
        self.palette_query.clear();
        self.palette_query_selection = 0..0;
        self.palette_query_reversed = false;
        self.palette_marked_range = None;
        self.palette_selection = 0;
        self.palette_input_geometry = None;
        self.palette_results_scroll
            .scroll_to_item(0, gpui::ScrollStrategy::Top);
        #[cfg(feature = "gui-tests")]
        {
            self.palette_native_input_registered = false;
        }
        window.focus(&self.palette_focus, cx);
        self.reset_palette_blink(cx);
    }

    fn dismiss_palette(
        &mut self,
        restore_focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.workspace.read(cx).palette_open {
            return;
        }
        self.workspace.update(cx, |workspace, cx| {
            workspace.palette_open = false;
            if restore_focus {
                workspace.focus_active_editor(window, cx);
            }
            cx.notify();
        });
        self.palette_marked_range = None;
        self.palette_dragging = false;
        self.palette_input_geometry = None;
        self.palette_blink_task = gpui::Task::ready(());
        #[cfg(feature = "gui-tests")]
        {
            self.palette_native_input_registered = false;
        }
        cx.notify();
    }

    fn reset_palette_blink(&mut self, cx: &mut Context<Self>) {
        self.palette_cursor_visible = true;
        self.palette_blink_task = cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            if this
                .update(cx, |this, cx| {
                    this.palette_cursor_visible = !this.palette_cursor_visible;
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        });
        cx.notify();
    }

    fn palette_entries(&self, cx: &App) -> Vec<PaletteEntry> {
        let theme = self.workspace.read(cx).config.editor_theme();
        let mut entries = vec![
            PaletteEntry {
                label: theme.ui_text("WYSIWYG"),
                target: PaletteTarget::Mode(EditorMode::Wysiwyg),
            },
            PaletteEntry {
                label: theme.ui_text("Source"),
                target: PaletteTarget::Mode(EditorMode::Source),
            },
            PaletteEntry {
                label: theme.ui_text("Split View"),
                target: PaletteTarget::Mode(EditorMode::Split),
            },
            PaletteEntry {
                label: theme.ui_text("New Tab"),
                target: PaletteTarget::NewTab,
            },
            PaletteEntry {
                label: theme.ui_text("Save"),
                target: PaletteTarget::Save,
            },
            PaletteEntry {
                label: theme.ui_text("Export HTML"),
                target: PaletteTarget::ExportHtml,
            },
            PaletteEntry {
                label: theme.ui_text("Load Remote Images"),
                target: PaletteTarget::LoadRemoteImages,
            },
        ];
        entries.extend(self.workspace.read(cx).tabs.iter().map(|tab| PaletteEntry {
            label: display_tab_title(&tab.title, tab.document.read(cx).path.is_none(), &theme),
            target: PaletteTarget::Tab(tab.id),
        }));
        let query = self.palette_query.to_lowercase();
        entries.retain(|entry| fuzzy_match(&entry.label.to_lowercase(), &query));
        entries
    }

    fn activate_palette_entry(
        &mut self,
        entry: PaletteEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_palette(true, window, cx);
        match entry.target {
            PaletteTarget::Mode(mode) => self.set_editor_mode(mode, window, cx),
            PaletteTarget::NewTab => self.new_tab(&NewTab, window, cx),
            PaletteTarget::Save => self.save(&Save, window, cx),
            PaletteTarget::ExportHtml => self.export_html(&ExportHtml, window, cx),
            PaletteTarget::LoadRemoteImages => {
                self.load_remote_images(&LoadRemoteImages, window, cx)
            }
            PaletteTarget::Tab(id) => {
                let index = self
                    .workspace
                    .read(cx)
                    .tabs
                    .iter()
                    .position(|tab| tab.id == id);
                if let Some(index) = index {
                    self.tab_scroll.scroll_to_item(index);
                    self.workspace.update(cx, |workspace, cx| {
                        let _ = workspace.dispatch(WorkspaceCommand::SwitchTab(index), window, cx);
                    });
                }
            }
        }
        cx.notify();
    }

    fn palette_cursor(&self) -> usize {
        if self.palette_query_reversed {
            self.palette_query_selection.start
        } else {
            self.palette_query_selection.end
        }
    }

    fn select_palette_query_to(&mut self, at: usize, extend: bool, cx: &mut Context<Self>) {
        let at = palette_byte_boundary(&self.palette_query, at);
        let anchor = if extend {
            if self.palette_query_reversed {
                self.palette_query_selection.end
            } else {
                self.palette_query_selection.start
            }
        } else {
            at
        };
        self.palette_query_selection = anchor.min(at)..anchor.max(at);
        self.palette_query_reversed = at < anchor;
        self.palette_marked_range = None;
        self.reset_palette_blink(cx);
    }

    fn replace_palette_query(
        &mut self,
        range: Range<usize>,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Range<usize> {
        let start = palette_byte_boundary(&self.palette_query, range.start);
        let end = palette_byte_boundary(&self.palette_query, range.end).max(start);
        let text = palette_single_line(text);
        self.palette_query.replace_range(start..end, &text);
        let inserted = start..start + text.len();
        self.palette_query_selection = inserted.end..inserted.end;
        self.palette_query_reversed = false;
        self.palette_marked_range = None;
        self.palette_selection = 0;
        self.palette_results_scroll
            .scroll_to_item(0, gpui::ScrollStrategy::Top);
        self.reset_palette_blink(cx);
        inserted
    }

    fn palette_keystroke(
        &mut self,
        key: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.review.is_some() || !self.workspace.read(cx).palette_open {
            return false;
        }
        if key.modifiers.platform && matches!(key.key.as_str(), "p" | "q") {
            return false;
        }
        if key.modifiers.control && key.key == "p" {
            self.dismiss_palette(true, window, cx);
            return true;
        }
        if !self.palette_focus.is_focused(window) {
            window.focus(&self.palette_focus, cx);
        }
        // Composition navigation belongs to the platform IME, not the result list.
        if self.palette_marked_range.is_some() && !key.modifiers.platform && !key.modifiers.control
        {
            return false;
        }
        if (key.modifiers.platform || key.modifiers.control) && key.key == "a" {
            self.palette_query_selection = 0..self.palette_query.len();
            self.palette_query_reversed = false;
            self.reset_palette_blink(cx);
            return true;
        }
        if (key.modifiers.platform || key.modifiers.control)
            && matches!(key.key.as_str(), "c" | "x")
        {
            if !self.palette_query_selection.is_empty() {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                    self.palette_query[self.palette_query_selection.clone()].to_owned(),
                ));
                if key.key == "x" {
                    self.replace_palette_query(self.palette_query_selection.clone(), "", cx);
                }
            }
            return true;
        }
        if (key.modifiers.platform || key.modifiers.control) && key.key == "v" {
            self.paste(&Paste, window, cx);
            return true;
        }
        match key.key.as_str() {
            "escape" => self.dismiss_palette(true, window, cx),
            "enter" => {
                let entries = self.palette_entries(cx);
                if let Some(entry) = entries
                    .get(self.palette_selection.min(entries.len().saturating_sub(1)))
                    .cloned()
                {
                    self.activate_palette_entry(entry, window, cx);
                }
            }
            "up" | "down" | "tab" => {
                let entries = self.palette_entries(cx);
                let previous = key.key == "up" || (key.key == "tab" && key.modifiers.shift);
                self.palette_selection =
                    adjacent_tab(self.palette_selection, entries.len(), previous).unwrap_or(0);
                self.palette_results_scroll
                    .scroll_to_item(self.palette_selection, gpui::ScrollStrategy::Center);
                self.reset_palette_blink(cx);
            }
            "left" | "right" | "home" | "end" => {
                let cursor = self.palette_cursor();
                let at = match key.key.as_str() {
                    "home" => 0,
                    "end" => self.palette_query.len(),
                    "left" if key.modifiers.platform => 0,
                    "right" if key.modifiers.platform => self.palette_query.len(),
                    "left" if !key.modifiers.shift && !self.palette_query_selection.is_empty() => {
                        self.palette_query_selection.start
                    }
                    "right" if !key.modifiers.shift && !self.palette_query_selection.is_empty() => {
                        self.palette_query_selection.end
                    }
                    "left" => palette_grapheme_left(&self.palette_query, cursor),
                    _ => palette_grapheme_right(&self.palette_query, cursor),
                };
                self.select_palette_query_to(at, key.modifiers.shift, cx);
            }
            "backspace" | "delete" => {
                let mut range = self.palette_query_selection.clone();
                if range.is_empty() {
                    if key.key == "backspace" {
                        range.start = if key.modifiers.platform {
                            0
                        } else {
                            palette_grapheme_left(&self.palette_query, range.start)
                        };
                    } else {
                        range.end = palette_grapheme_right(&self.palette_query, range.end);
                    }
                }
                self.replace_palette_query(range, "", cx);
            }
            _ => return key.modifiers.platform || key.modifiers.control,
        }
        true
    }

    #[cfg(feature = "gui-tests")]
    pub(crate) fn test_palette_state(&self, window: &Window, cx: &App) -> PaletteState {
        let open = self.workspace.read(cx).palette_open;
        PaletteState {
            open,
            focused: open && self.palette_focus.is_focused(window),
            query: self.palette_query.clone(),
            query_selection: self.palette_query_selection.clone(),
            selection_reversed: self.palette_query_reversed,
            marked_range: self.palette_marked_range.clone(),
            selected_result: self.palette_selection,
            results: self
                .palette_entries(cx)
                .into_iter()
                .map(|entry| entry.label)
                .collect(),
            bounds: open.then(|| self.palette_bounds.bounds()),
            input_bounds: self
                .palette_input_geometry
                .as_ref()
                .map(|geometry| geometry.bounds),
            native_input_registered: open && self.palette_native_input_registered,
        }
    }

    fn render_palette(
        &mut self,
        theme: &EditorTheme,
        open: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if !open {
            return div().hidden().into_any_element();
        }
        let entries = self.palette_entries(cx);
        self.palette_selection = self.palette_selection.min(entries.len().saturating_sub(1));
        let selected = self.palette_selection;
        let width = (f32::from(window.viewport_size().width) - 32.).clamp(0., 480.);
        let top = (f32::from(window.viewport_size().height) * 0.15).clamp(16., 96.);
        let results_height = (entries.len().max(1) as f32 * 30.)
            .min((f32::from(window.viewport_size().height) - top - 114.).clamp(30., 300.));
        let query = self.palette_query.clone();
        let range = self.palette_query_selection.clone();
        let marked = self.palette_marked_range.clone();
        let cursor = self.palette_cursor();
        let cursor_visible = self.palette_cursor_visible;
        let input_view = cx.entity();
        let input_paint_view = input_view.clone();
        let input_theme = theme.clone();
        let paint_theme = theme.clone();
        let input_focus = self.palette_focus.clone();
        let query_input = gpui::canvas(
            move |bounds, window, _| {
                let font = gpui::Font {
                    family: input_theme.font_family.clone().into(),
                    fallbacks: Some(EditorTheme::system_font_fallbacks()),
                    ..Default::default()
                };
                let line = window.text_system().shape_line(
                    query.clone().into(),
                    px(14.),
                    &[gpui::TextRun {
                        len: query.len(),
                        font: font.clone(),
                        color: input_theme.text,
                        ..Default::default()
                    }],
                    None,
                );
                // Scroll shaped glyphs, not query bytes: drawing, mouse input,
                // and native IME coordinates share this translated origin.
                let scroll = px(palette_query_scroll(
                    f32::from(line.x_for_index(cursor)),
                    f32::from(bounds.size.width),
                ));
                let origin = gpui::point(bounds.left() - scroll, bounds.top() + px(4.));
                let placeholder = query.is_empty().then(|| {
                    let placeholder = input_theme.ui_text("Search commands and open tabs…");
                    window.text_system().shape_line(
                        placeholder.clone().into(),
                        px(14.),
                        &[gpui::TextRun {
                            len: placeholder.len(),
                            font,
                            color: input_theme.secondary_text,
                            ..Default::default()
                        }],
                        None,
                    )
                });
                (line, origin, placeholder)
            },
            move |bounds, (line, origin, placeholder), window, cx| {
                input_paint_view.update(cx, |this, _| {
                    this.palette_input_geometry = Some(PaletteInputGeometry {
                        line: line.clone(),
                        origin,
                        bounds,
                    });
                });
                if input_focus.is_focused(window) {
                    window.handle_input(
                        &input_focus,
                        gpui::ElementInputHandler::new(bounds, input_paint_view.clone()),
                        cx,
                    );
                    #[cfg(feature = "gui-tests")]
                    input_paint_view.update(cx, |this, _| {
                        this.palette_native_input_registered = true;
                    });
                }
                window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                    if !range.is_empty() {
                        let left = line.x_for_index(range.start);
                        let right = line.x_for_index(range.end);
                        window.paint_quad(gpui::fill(
                            gpui::Bounds::new(
                                gpui::point(origin.x + left, origin.y),
                                gpui::size(right - left, px(22.)),
                            ),
                            paint_theme.selection,
                        ));
                    }
                    let _ = placeholder.as_ref().unwrap_or(&line).paint(
                        origin,
                        px(22.),
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                    if let Some(marked) = &marked {
                        let left = line.x_for_index(marked.start);
                        let right = line.x_for_index(marked.end);
                        window.paint_quad(gpui::fill(
                            gpui::Bounds::new(
                                gpui::point(origin.x + left, origin.y + px(21.)),
                                gpui::size(right - left, px(1.)),
                            ),
                            paint_theme.accent,
                        ));
                    }
                    if range.is_empty() && cursor_visible && input_focus.is_focused(window) {
                        window.paint_quad(gpui::fill(
                            gpui::Bounds::new(
                                gpui::point(origin.x + line.x_for_index(cursor), origin.y),
                                gpui::size(px(2.), px(22.)),
                            ),
                            paint_theme.caret,
                        ));
                    }
                });
                let drag_view = input_paint_view.clone();
                window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                    if !phase.bubble()
                        || event.pressed_button != Some(gpui::MouseButton::Left)
                        || !drag_view.read(cx).palette_dragging
                    {
                        return;
                    }
                    drag_view.update(cx, |this, cx| {
                        if let Some(geometry) = &this.palette_input_geometry {
                            let at = geometry
                                .line
                                .closest_index_for_x(event.position.x - geometry.origin.x);
                            this.select_palette_query_to(at, true, cx);
                        }
                    });
                    cx.stop_propagation();
                });
                let release_view = input_paint_view.clone();
                window.on_mouse_event(move |event: &gpui::MouseUpEvent, phase, _, cx| {
                    if phase.bubble() && event.button == gpui::MouseButton::Left {
                        release_view.update(cx, |this, _| {
                            this.palette_dragging = false;
                        });
                    }
                });
            },
        )
        .w_full()
        .h(px(30.));
        let row_theme = theme.clone();
        let empty = entries.is_empty();
        let results = uniform_list(
            "command-palette-results",
            entries.len(),
            cx.processor(move |_, range: Range<usize>, _, cx| {
                range
                    .map(|index| {
                        let entry = entries[index].clone();
                        let label = entry.label.clone();
                        div()
                            .id(("palette-item", index))
                            .role(Role::Button)
                            .aria_label(label.clone())
                            .h(px(30.))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .text_sm()
                            .truncate()
                            .bg(if index == selected {
                                row_theme.sidebar_selected
                            } else {
                                row_theme.tab_active
                            })
                            .text_color(if index == selected {
                                row_theme.sidebar_selected_text
                            } else {
                                row_theme.text
                            })
                            .cursor_pointer()
                            .child(label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.activate_palette_entry(entry.clone(), window, cx)
                            }))
                    })
                    .collect()
            }),
        )
        .h(px(results_height))
        .w_full()
        .track_scroll(&self.palette_results_scroll);
        div()
            .id("command-palette-overlay")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .justify_center()
            .pt(px(top))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.dismiss_palette(true, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, _, window, cx| {
                    this.dismiss_palette(true, window, cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .id("command-palette")
                    .track_focus(&self.palette_focus)
                    .track_scroll(&self.palette_bounds)
                    .w(px(width))
                    .h_auto()
                    .max_h(px(results_height + 100.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .rounded_lg()
                    .shadow_lg()
                    .bg(theme.tab_active)
                    .border_1()
                    .border_color(theme.separator)
                    .p_3()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .id("command-palette-query")
                            .role(Role::TextInput)
                            .aria_label(theme.ui_text("Search commands and open tabs"))
                            .h(px(38.))
                            .flex_shrink_0()
                            .px_2()
                            .border_1()
                            .border_color(theme.accent)
                            .rounded_md()
                            .cursor_text()
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                                    window.focus(&this.palette_focus, cx);
                                    if let Some(geometry) = &this.palette_input_geometry {
                                        let at = geometry.line.closest_index_for_x(
                                            event.position.x - geometry.origin.x,
                                        );
                                        if event.click_count >= 3 {
                                            this.palette_query_selection =
                                                0..this.palette_query.len();
                                            this.palette_query_reversed = false;
                                        } else if event.click_count == 2 {
                                            this.palette_query_selection =
                                                palette_word_range(&this.palette_query, at);
                                            this.palette_query_reversed = false;
                                        } else {
                                            this.select_palette_query_to(
                                                at,
                                                event.modifiers.shift,
                                                cx,
                                            );
                                        }
                                        this.palette_dragging = true;
                                    }
                                    this.reset_palette_blink(cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .child(query_input),
                    )
                    .child(if empty {
                        div()
                            .h(px(30.))
                            .px_2()
                            .text_sm()
                            .text_color(theme.secondary_text)
                            .child(theme.ui_text("No matching commands or tabs"))
                            .into_any_element()
                    } else {
                        results.into_any_element()
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.secondary_text)
                            .child("↑ ↓ Select · Enter Open · Esc Close"),
                    ),
            )
            .into_any_element()
    }

    fn export_html(&mut self, _: &ExportHtml, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() {
            return;
        }
        crate::crash::record_export_started();
        self.workspace.update(cx, |workspace, cx| {
            match workspace.dispatch(WorkspaceCommand::ExportHtml { output: None }, window, cx) {
                Ok(()) => crate::crash::record_export_succeeded(),
                Err(error) => {
                    crate::crash::record_export_failed(&error);
                    eprintln!("MarkRust HTML export failed; see diagnostics log");
                }
            }
        });
    }

    fn load_remote_images(
        &mut self,
        _: &LoadRemoteImages,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.review.is_some() {
            return;
        }
        self.workspace.update(cx, |workspace, cx| {
            workspace.load_remote_images(cx);
        });
    }

    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() || self.workspace.read(cx).palette_open {
            return;
        }
        if let Some(location) = &self.open_location {
            location.editor.update(cx, |editor, cx| {
                editor.apply_command(EditorCommand::Undo, cx);
            });
            return;
        }
        if self.find_input_focused(window, cx) {
            if let Some(find) = &self.find {
                find.query_editor.update(cx, |editor, cx| {
                    editor.apply_command(EditorCommand::Undo, cx);
                });
            }
            return;
        }
        self.workspace.update(cx, |workspace, cx| {
            let _ = workspace.dispatch(
                WorkspaceCommand::Editor(markrust_editor::EditorCommand::Undo),
                window,
                cx,
            );
        });
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_some() || self.workspace.read(cx).palette_open {
            return;
        }
        if let Some(location) = &self.open_location {
            location.editor.update(cx, |editor, cx| {
                editor.apply_command(EditorCommand::Redo, cx);
            });
            return;
        }
        if self.find_input_focused(window, cx) {
            if let Some(find) = &self.find {
                find.query_editor.update(cx, |editor, cx| {
                    editor.apply_command(EditorCommand::Redo, cx);
                });
            }
            return;
        }
        self.workspace.update(cx, |workspace, cx| {
            let _ = workspace.dispatch(
                WorkspaceCommand::Editor(markrust_editor::EditorCommand::Redo),
                window,
                cx,
            );
        });
    }
}

impl Focusable for MarkRustWindow {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl MarkRustWindow {
    /// Compact Markdown formatting row that lives between the document
    /// toolbar and the tab strip. Inline marks are always live; block-level
    /// commands grey out whenever the caret is in a surface that cannot
    /// route them (Source mode, or Split mode with the source pane focused).
    fn format_toolbar(
        theme: &EditorTheme,
        block_enabled: bool,
        workspace_entity: Entity<Workspace>,
        scroll: ScrollHandle,
        available_width: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let separator = || {
            div()
                .w(px(1.))
                .h(px(18.))
                .flex_shrink_0()
                .mx_2()
                .bg(theme.separator)
                .into_any_element()
        };

        let dispatch = |cmd: EditorCommand| {
            let ws = workspace_entity.clone();
            cx.listener(move |this, _, window, cx| {
                let _ = ws; // keep the captured entity alive across the closure
                if this.find_input_focused(window, cx) || this.open_location.is_some() {
                    return;
                }
                this.workspace.update(cx, |workspace, cx| {
                    let _ = workspace.dispatch(WorkspaceCommand::Editor(cmd.clone()), window, cx);
                });
            })
        };

        let block_state = if block_enabled {
            ToolbarState::Action
        } else {
            ToolbarState::Disabled
        };

        let tools = div()
            .w(px(FORMATTING_TOOLS_WIDTH))
            .flex()
            .flex_shrink_0()
            .items_center()
            .h(px(36.))
            .gap_1()
            // --- Inline marks (always live) ---
            .child(toolbar_icon_button(
                Icon::Bold,
                "Bold",
                "⌘B",
                theme,
                "fmt-bold",
                ToolbarState::Action,
                dispatch(EditorCommand::Wrap(markrust_editor::WrapKind::Bold)),
            ))
            .child(toolbar_icon_button(
                Icon::Italic,
                "Italic",
                "⌘I",
                theme,
                "fmt-italic",
                ToolbarState::Action,
                dispatch(EditorCommand::Wrap(markrust_editor::WrapKind::Italic)),
            ))
            .child(toolbar_icon_button(
                Icon::Code,
                "Inline Code",
                "⌘⌥K",
                theme,
                "fmt-code",
                ToolbarState::Action,
                dispatch(EditorCommand::Wrap(markrust_editor::WrapKind::Code)),
            ))
            .child(toolbar_icon_button(
                Icon::Link,
                "Link",
                "⌘K",
                theme,
                "fmt-link",
                ToolbarState::Action,
                dispatch(EditorCommand::Wrap(markrust_editor::WrapKind::Link)),
            ))
            .child(separator())
            // --- Block-level (WYSIWYG only) ---
            .children(crate::ui::BLOCK_STYLE_TOOLS.into_iter().map(|tool| {
                toolbar_icon_button(
                    tool.icon,
                    tool.label,
                    tool.shortcut,
                    theme,
                    tool.id,
                    block_state.clone(),
                    dispatch(tool.command),
                )
            }))
            .child(toolbar_icon_button(
                Icon::Quote,
                "Blockquote",
                "⌘⇧.",
                theme,
                "fmt-quote",
                block_state.clone(),
                dispatch(EditorCommand::ToggleBlockquote),
            ))
            .child(toolbar_icon_button(
                Icon::UnorderedList,
                "Bulleted List",
                "⌘⇧8",
                theme,
                "fmt-ul",
                block_state.clone(),
                dispatch(EditorCommand::ToggleList { ordered: false }),
            ))
            .child(toolbar_icon_button(
                Icon::OrderedList,
                "Numbered List",
                "⌘⇧7",
                theme,
                "fmt-ol",
                block_state.clone(),
                dispatch(EditorCommand::ToggleList { ordered: true }),
            ))
            .child(toolbar_icon_button(
                Icon::TaskList,
                "Task List",
                "⌘⇧9",
                theme,
                "fmt-task",
                block_state.clone(),
                dispatch(EditorCommand::ToggleTaskList),
            ))
            .child(separator())
            .child(toolbar_icon_button(
                Icon::HorizontalRule,
                "Horizontal Rule",
                "⌘⇧-",
                theme,
                "fmt-hr",
                block_state.clone(),
                dispatch(EditorCommand::InsertHorizontalRule),
            ))
            .child(toolbar_icon_button(
                Icon::CodeBlock,
                "Code Block",
                "⌘⌥C",
                theme,
                "fmt-codeblock",
                block_state.clone(),
                dispatch(EditorCommand::InsertCodeBlock),
            ))
            .child(separator())
            .child(toolbar_icon_button(
                Icon::Strikethrough,
                "Strikethrough",
                "⌘⇧X",
                theme,
                "fmt-strike",
                block_state.clone(),
                dispatch(EditorCommand::ToggleStrikethrough),
            ))
            .child(toolbar_icon_button(
                Icon::Image,
                "Image…",
                "⌘⇧I",
                theme,
                "fmt-image",
                block_state.clone(),
                dispatch(EditorCommand::InsertImage),
            ))
            .child(toolbar_icon_button(
                Icon::Table,
                "Table",
                "⌘⌥T",
                theme,
                "fmt-table",
                block_state.clone(),
                dispatch(EditorCommand::InsertTable),
            ))
            .child(separator())
            // --- Indent / Outdent (always live) ---
            .child(toolbar_icon_button(
                Icon::Indent,
                "Indent",
                "Tab",
                theme,
                "fmt-indent",
                ToolbarState::Action,
                dispatch(EditorCommand::Indent),
            ))
            .child(toolbar_icon_button(
                Icon::Outdent,
                "Outdent",
                "⇧Tab",
                theme,
                "fmt-outdent",
                ToolbarState::Action,
                dispatch(EditorCommand::Outdent),
            ))
            .into_any_element();
        let compact = formatting_toolbar_overflows(available_width);
        div()
            .id("format-toolbar")
            .role(Role::Toolbar)
            .aria_label(theme.ui_text("Formatting toolbar"))
            .flex()
            .items_center()
            .px_3()
            .h(px(36.))
            .flex_shrink_0()
            .when(compact, |bar| bar.gap_1())
            .bg(theme.chrome_bg)
            .border_b_1()
            .border_color(theme.separator)
            .when(compact, |bar| {
                bar.child(toolbar_scroll_button(
                    false,
                    theme,
                    cx.listener(|this, _, _, cx| {
                        this.scroll_format_tools(false, cx);
                    }),
                ))
            })
            .child(
                div()
                    .id("format-tools-scroll")
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_x_scroll()
                    .track_scroll(&scroll)
                    .child(tools),
            )
            .when(compact, |bar| {
                bar.child(toolbar_scroll_button(
                    true,
                    theme,
                    cx.listener(|this, _, _, cx| {
                        this.scroll_format_tools(true, cx);
                    }),
                ))
            })
            .into_any_element()
    }
}

impl Render for MarkRustWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut input_theme = self.workspace.read(cx).config.editor_theme();
        input_theme.font_size = 14.;
        input_theme.background = input_theme.editor_bg;
        let inputs = self
            .find
            .as_ref()
            .map(|find| find.query_editor.clone())
            .into_iter()
            .chain(
                self.open_location
                    .as_ref()
                    .map(|location| location.editor.clone()),
            );
        for editor in inputs {
            let previous = &editor.read(cx).theme;
            if previous.ui_strings != input_theme.ui_strings
                || previous.background != input_theme.background
                || previous.syntax_keyword != input_theme.syntax_keyword
                || previous.code_font_family != input_theme.code_font_family
            {
                editor.update(cx, |editor, cx| {
                    editor.theme = input_theme.clone();
                    cx.notify();
                });
            }
        }
        if let Some(location) = &self.open_location {
            location
                .editor
                .read(cx)
                .focus_handle
                .clone()
                .focus(window, cx);
        }
        let old_find_origin = self
            .find
            .as_ref()
            .map(|find| (find.origin.tab_id, find.origin.pane));
        self.sync_find(window, cx);
        if let Some(find) = &self.find {
            if !find.paused && old_find_origin != Some((find.origin.tab_id, find.origin.pane)) {
                find.query_editor
                    .read(cx)
                    .focus_handle
                    .clone()
                    .focus(window, cx);
            }
        }
        if let Some(review) = &self.review {
            if !review.focus.is_focused(window)
                && !review.controls.iter().any(|focus| focus.is_focused(window))
            {
                window.focus(&review.controls[2], cx);
            }
        }
        self.workspace.update(cx, |workspace, cx| {
            workspace.ensure_panel_layout(f32::from(window.viewport_size().width), cx);
            workspace.sync_split_shadow(window, cx);
        });
        let overlay_document = {
            let workspace = self.workspace.read(cx);
            workspace.panel_overlay.and_then(|_| {
                workspace
                    .active_tab()
                    .map(|tab| (tab.id, tab.document.clone()))
            })
        };
        if let Some((id, document)) = overlay_document {
            if self
                .overlay_document_subscription
                .as_ref()
                .map(|(id, _)| *id)
                != Some(id)
            {
                // macOS composition and the emoji picker can commit text
                // without delivering a GPUI keystroke. Observe actual edits
                // too, while ignoring parser-only document notifications.
                let revision = document.read(cx).revision();
                self.overlay_document_subscription = Some((
                    id,
                    cx.observe_in(&document, window, move |this, document, window, cx| {
                        if document.read(cx).revision() != revision {
                            this.dismiss_panel_overlay(window, cx);
                        }
                    }),
                ));
            }
        } else {
            self.overlay_document_subscription = None;
        }
        let menu_state = {
            let workspace = self.workspace.read(cx);
            MenuState {
                language: workspace.config.language,
                automatic_updates: workspace.config.automatic_updates,
                mode: workspace
                    .active_tab()
                    .map(|tab| tab.mode)
                    .unwrap_or_default(),
                sidebar_open: workspace.sidebar_open,
                outline_open: workspace.outline_open,
                markup_hints_enabled: workspace.config.markup_hints_enabled,
                highlight_style: workspace.config.highlight_style,
            }
        };
        if window.is_window_active() {
            menus::sync(menu_state, cx);
        }
        let frontmatter_info = self
            .workspace
            .read(cx)
            .active_tab()
            .and_then(|tab| parse_frontmatter(&tab.document.read(cx).buffer.content()));
        let outline_items = {
            let mut items = Vec::new();
            if let Some(document) = self
                .workspace
                .read(cx)
                .active_tab()
                .map(|tab| tab.document.clone())
            {
                document.update(cx, |doc, _| {
                    doc.apply_pending_parse();
                    items = outline_headings(&doc.syntax_spans, &doc.buffer.content());
                });
            }
            items
        };

        let workspace = self.workspace.read(cx);
        let theme = workspace.config.editor_theme();
        let mode = menu_state.mode;
        let document_title = workspace
            .active_tab()
            .map(|tab| display_tab_title(&tab.title, tab.document.read(cx).path.is_none(), &theme))
            .unwrap_or_else(|| "MarkRust".into());
        window.set_window_title(&format!("{document_title} — MarkRust"));
        let active = workspace.active_tab;
        let tab_count = workspace.tabs.len();
        let files = workspace.list_files();
        let recent = workspace.recent.clone();
        let palette_open = workspace.palette_open;
        let sidebar_open = workspace.sidebar_open;
        let outline_open = workspace.outline_open;
        let sidebar_overlay = workspace.panel_overlay == Some(Panel::Sidebar);
        let outline_overlay = workspace.panel_overlay == Some(Panel::Outline);
        let external_change = workspace.pending_external_tab_id().and_then(|tab_id| {
            workspace
                .tabs
                .iter()
                .find(|tab| tab.id == tab_id)
                .and_then(|tab| {
                    tab.document
                        .read(cx)
                        .path
                        .clone()
                        .map(|path| (tab_id, path))
                })
        });
        let recovery_warning = workspace.recovery_warning().map(|warning| {
            (
                workspace
                    .recovery_warning_summary(cx)
                    .unwrap_or_else(|| warning.message().to_owned()),
                warning.message().to_owned(),
            )
        });
        let source_mode = matches!(
            workspace.active_tab().map(|tab| tab.mode),
            Some(crate::workspace::EditorMode::Source)
        );
        // Block-level Markdown formatting only lives on the rich view, so
        // the toolbar greys those buttons out unless the rich surface owns
        // the caret. Inline marks and indents still work in Source mode
        // because delimiter-masking already shows the marks.
        let rich_active = workspace
            .active_tab()
            .is_some_and(|tab| tab.active_editing_pane(window, cx) == EditingPane::Wysiwyg);
        let block_enabled = match mode {
            EditorMode::Wysiwyg => true,
            EditorMode::Split => rich_active,
            EditorMode::Source => false,
        };
        let fm_for_window = if source_mode {
            frontmatter_info.clone()
        } else {
            None
        };
        let workspace_entity = self.workspace.clone();
        let root = workspace.root.clone();
        let active_doc_path = workspace
            .active_tab()
            .and_then(|tab| tab.document.read(cx).path.clone());
        let document_location = active_doc_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| theme.ui_text("Unsaved document"));
        let pristine = initial_welcome_allowed(
            root.is_some(),
            tab_count,
            active_doc_path.is_some(),
            workspace
                .active_tab()
                .is_some_and(|tab| tab.document.read(cx).dirty),
            workspace
                .active_tab()
                .is_some_and(|tab| tab.document.read(cx).buffer.len_bytes() > 0),
        );
        if !pristine {
            self.welcome_dismissed = true;
        }
        let welcome = pristine && !self.welcome_dismissed;
        let editor_width = f32::from(window.viewport_size().width)
            - if sidebar_open && !sidebar_overlay {
                SIDEBAR_WIDTH
            } else {
                0.
            }
            - if outline_open && !outline_overlay {
                OUTLINE_WIDTH
            } else {
                0.
            };
        let tab_reveal = (
            workspace.active_tab().map_or(0, |tab| tab.id),
            tab_count,
            editor_width,
        );
        if self.last_tab_reveal != Some(tab_reveal) {
            self.tab_scroll.scroll_to_item(active);
            self.last_tab_reveal = Some(tab_reveal);
        }
        // Drop the read borrow before mutating `cx` for the format-toolbar
        // listeners below. The tab-strip closures re-acquire the borrow as
        // needed.
        let _ = workspace;

        let ws_drop = workspace_entity.clone();
        let ws_editor_drop = workspace_entity.clone();
        let tab_strip = div()
            .id("document-tabs")
            .role(Role::TabList)
            .aria_label("Open document tabs")
            .flex()
            .items_end()
            .h(px(36.))
            .flex_shrink_0()
            .min_w_0()
            .gap_px()
            .px_2()
            .overflow_x_scroll()
            .track_scroll(&self.tab_scroll)
            .bg(theme.tab_inactive)
            .border_b_1()
            .border_color(theme.separator)
            .children((0..tab_count).map(|index| {
                let workspace = self.workspace.read(cx);
                let tab = &workspace.tabs[index];
                let doc = tab.document.read(cx);
                let full_path = doc
                    .path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| theme.ui_text("Unsaved document"));
                let ws = workspace_entity.clone();
                let ws_close = workspace_entity.clone();
                document_tab(
                    display_tab_title(&tab.title, doc.path.is_none(), &theme),
                    full_path,
                    doc.dirty,
                    &theme,
                    index == active,
                    SharedString::from(format!("tab-{index}")),
                    cx.listener(move |_, _, window, cx| {
                        ws.update(cx, |workspace, cx| {
                            let _ =
                                workspace.dispatch(WorkspaceCommand::SwitchTab(index), window, cx);
                        });
                    }),
                    cx.listener(move |_, _, window, cx| {
                        ws_close.update(cx, |workspace, cx| workspace.close_tab(index, window, cx));
                    }),
                )
            }));

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.chrome_bg)
            .text_color(theme.text)
            .font_family(theme.font_family.clone())
            .text_size(px(14.))
            .track_focus(&self.focus_handle)
            .key_context("MarkRust")
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::Cut>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::SelectAll>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::ToggleBold>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::ToggleItalic>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::ToggleCode>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::ToggleLink>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::ToggleStrikethrough>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::SetHeading1>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::SetHeading2>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::SetHeading3>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::SetHeading4>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::SetHeading5>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::SetHeading6>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::Paragraph>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::ToggleUnorderedList>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::ToggleOrderedList>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::ToggleTaskList>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::ToggleBlockquote>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::InsertHorizontalRule>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::InsertCodeBlock>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::InsertImage>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::InsertTable>))
            .capture_action(cx.listener(Self::guard_review_action::<InsertTableRowBelow>))
            .capture_action(cx.listener(Self::guard_review_action::<InsertTableRowAbove>))
            .capture_action(cx.listener(Self::guard_review_action::<DeleteTableRow>))
            .capture_action(cx.listener(Self::guard_review_action::<InsertTableColumnRight>))
            .capture_action(cx.listener(Self::guard_review_action::<InsertTableColumnLeft>))
            .capture_action(cx.listener(Self::guard_review_action::<DeleteTableColumn>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::Indent>))
            .capture_action(cx.listener(Self::guard_review_action::<markrust_editor::Outdent>))
            .on_action(cx.listener(Self::save))
            .capture_action(cx.listener(Self::guard_update_action::<Save>))
            .capture_action(cx.listener(Self::guard_update_action::<SaveAs>))
            .capture_action(cx.listener(Self::guard_update_action::<NormalizeMarkdown>))
            .capture_action(cx.listener(Self::guard_update_action::<OpenFile>))
            .capture_action(cx.listener(Self::guard_update_action::<OpenFolder>))
            .capture_action(cx.listener(Self::guard_update_action::<OpenPath>))
            .capture_action(cx.listener(Self::guard_update_action::<NewDocument>))
            .capture_action(cx.listener(Self::guard_update_action::<NewTab>))
            .capture_action(cx.listener(Self::guard_update_action::<CloseTab>))
            .on_action(cx.listener(Self::save_as))
            .on_action(cx.listener(Self::normalize_markdown))
            .on_action(cx.listener(Self::insert_table_row_below))
            .on_action(cx.listener(Self::insert_table_row_above))
            .on_action(cx.listener(Self::delete_table_row))
            .on_action(cx.listener(Self::insert_table_column_right))
            .on_action(cx.listener(Self::insert_table_column_left))
            .on_action(cx.listener(Self::delete_table_column))
            .on_action(cx.listener(Self::open_file))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(Self::open_path))
            .on_action(cx.listener(Self::new_document))
            .on_action(cx.listener(Self::new_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::find))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_previous))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::toggle_theme))
            .on_action(cx.listener(Self::highlight_native))
            .on_action(cx.listener(Self::highlight_ocean))
            .on_action(cx.listener(Self::highlight_forest))
            .on_action(cx.listener(Self::toggle_editor_mode))
            .on_action(cx.listener(Self::toggle_markup_hints))
            .on_action(cx.listener(Self::show_wysiwyg))
            .on_action(cx.listener(Self::show_source))
            .on_action(cx.listener(Self::show_split))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::about))
            .on_action(cx.listener(Self::help))
            .on_action(|_: &Minimize, window, _| window.minimize_window())
            .on_action(|_: &Zoom, window, _| window.zoom_window())
            .on_action(|_: &ToggleFullScreen, window, _| window.toggle_fullscreen())
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_outline))
            .on_action(cx.listener(Self::command_palette))
            .on_action(cx.listener(Self::export_html))
            .on_action(cx.listener(Self::load_remote_images))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_drop(cx.listener({
                let ws = ws_drop.clone();
                move |this, paths: &ExternalPaths, window, cx| {
                    if this.review.is_some() { return; }
                    ws.update(cx, |workspace, cx| {
                        let _ = workspace.dispatch(
                            WorkspaceCommand::DropFiles {
                                paths: paths.paths().to_vec(),
                                target: DropTarget::Window,
                            },
                            window,
                            cx,
                        );
                    });
                }
            }))
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(theme.drop_zone_bg))
            .children(external_change.map(|(tab_id, path)| {
                let banner = format!("Review external changes — {}. Your edits are preserved.", basename_label(&path));
                let tooltip_theme = theme.clone();
                let tooltip_message = SharedString::from(format!("{}\nReview Mine and Disk before choosing. The other version will be preserved as a separate draft.", path.display()));
                div()
                    .id("external-change-banner")
                    .flex_shrink_0()
                    .h(px(36.))
                    .px_4()
                    .py_2()
                    .bg(theme.accent.opacity(0.85))
                    .text_color(theme.sidebar_selected_text)
                    .text_sm()
                    .truncate()
                    .tooltip(move |window, cx| {
                        let width = (f32::from(window.viewport_size().width) - 32.).clamp(120., 520.);
                        cx.new(|_| RecoveryWarningTooltip { message: tooltip_message.clone(), theme: tooltip_theme.clone(), width }).into()
                    })
                    .child(banner)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| this.open_external_review(tab_id, window, cx)))
            }))
            .children(recovery_warning.map(|(summary, message)| {
                let tooltip_theme = theme.clone();
                let full_message = SharedString::from(format!("Session recovery: {message}"));
                let tooltip_message = full_message.clone();
                div()
                    .id("session-recovery-warning")
                    .role(Role::Group)
                    .aria_label(full_message)
                    .flex_shrink_0()
                    .h(px(36.))
                    .px_4()
                    .py_2()
                    .bg(theme.accent.opacity(0.12))
                    .border_b_1()
                    .border_color(theme.separator)
                    .text_sm()
                    .truncate()
                    .tooltip(move |window, cx| {
                        let width = (f32::from(window.viewport_size().width) - 32.)
                            .clamp(120., 520.);
                        cx.new(|_| RecoveryWarningTooltip {
                            message: tooltip_message.clone(),
                            theme: tooltip_theme.clone(),
                            width,
                        })
                        .into()
                    })
                    .child(format!("Session recovery: {summary}"))
            }))
            .child(
                div()
                    .id("document-toolbar")
                    .role(Role::Toolbar)
                    .aria_label(theme.ui_text("Document toolbar"))
                    .flex()
                    .items_center()
                    .px_3()
                    .h(px(46.))
                    .flex_shrink_0()
                    .gap_1()
                    .bg(theme.chrome_bg)
                    .border_b_1()
                    .border_color(theme.separator)
                    .child(toolbar_icon_button(
                        Icon::Sidebar,
                        "Sidebar",
                        "⌃⌘S",
                        &theme,
                        "toolbar-sidebar",
                        ToolbarState::Toggle(sidebar_open),
                        cx.listener(|this, _, window, cx| this.toggle_sidebar(&ToggleSidebar, window, cx)),
                    ))
                    .child(div().w(px(1.)).h(px(18.)).mx_2().bg(theme.separator))
                    .child(toolbar_icon_button(
                        Icon::NewDocument,
                        "New Document",
                        "⌘N",
                        &theme,
                        "toolbar-new",
                        ToolbarState::Action,
                        cx.listener(|this, _, window, cx| {
                            this.new_document(&NewDocument, window, cx)
                        }),
                    ))
                    .child(toolbar_icon_button(
                        Icon::Open,
                        "Open…",
                        "⌘O",
                        &theme,
                        "toolbar-open-file",
                        ToolbarState::Action,
                        cx.listener(|this, _, window, cx| this.open_file(&OpenFile, window, cx)),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_4()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .flex().flex_col()
                            .child(div().truncate().child(document_title))
                            .child(crate::ui::path_caption(document_location, &theme)),
                    )
                    .child(
                        div()
                            .id("editor-mode-picker")
                            .role(Role::RadioGroup)
                            .aria_label(theme.ui_text("Editor modes"))
                            .flex()
                            .flex_shrink_0()
                            .gap_px()
                            .p(px(2.))
                            .rounded(px(7.))
                            .bg(theme.tab_inactive)
                            .border_1()
                            .border_color(theme.separator)
                            .child(toolbar_icon_button(
                                Icon::Wysiwyg,
                                "WYSIWYG",
                                "⌘⌥1",
                                &theme,
                                "toolbar-mode-wysiwyg",
                                ToolbarState::Mode(mode == EditorMode::Wysiwyg),
                                cx.listener(|this, _, window, cx| this.show_wysiwyg(&ShowWysiwyg, window, cx)),
                            ))
                            .child(toolbar_icon_button(
                                Icon::Source,
                                "Source",
                                "⌘⌥2",
                                &theme,
                                "toolbar-mode-source",
                                ToolbarState::Mode(mode == EditorMode::Source),
                                cx.listener(|this, _, window, cx| this.show_source(&ShowSource, window, cx)),
                            ))
                            .child(toolbar_icon_button(
                                Icon::Split,
                                "Split View",
                                "⌘⌥3",
                                &theme,
                                "toolbar-mode-split",
                                ToolbarState::Mode(mode == EditorMode::Split),
                                cx.listener(|this, _, window, cx| this.show_split(&ShowSplit, window, cx)),
                            )),
                    )
                    .child(div().w(px(1.)).h(px(18.)).mx_2().bg(theme.separator))
                    .child(toolbar_icon_button(
                        Icon::Outline,
                        "Outline",
                        "⌃⌘O",
                        &theme,
                        "toolbar-outline",
                        ToolbarState::Toggle(outline_open),
                        cx.listener(|this, _, window, cx| {
                            this.toggle_outline(&ToggleOutline, window, cx)
                        }),
                    )),
            )
            .child(Self::format_toolbar(
                &theme,
                block_enabled,
                workspace_entity.clone(),
                self.format_scroll.clone(),
                f32::from(window.viewport_size().width),
                cx,
            ))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(panel_layer(if sidebar_open {
                        div()
                            .w(px(SIDEBAR_WIDTH))
                            .flex_shrink_0()
                            .h_full()
                            .id("sidebar")
                            .when(sidebar_overlay, |panel| {
                                panel.absolute().left_0().top_0().shadow_lg().occlude()
                                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                    .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
                            })
                            .flex()
                            .flex_col()
                            .overflow_y_scroll()
                            .bg(theme.sidebar_bg)
                            .border_r_1()
                            .border_color(theme.separator)
                            .when(root.is_some(), |panel| {
                                let folder_name = root
                                    .as_ref()
                                    .and_then(|p| p.file_name())
                                    .map(|name| name.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "Workspace".into());
                                panel
                                    .child(section_header(format!("{} · {folder_name}", theme.ui_text("Folder")), &theme))
                                    .child(div().px_3().pb_2().child(crate::ui::path_caption(
                                        root.as_ref().unwrap().display().to_string(), &theme)))
                                    .children({
                                        let root = root.clone().unwrap();
                                        if files.is_empty() {
                                            vec![muted_hint(
                                                "No Markdown files in this folder.",
                                                &theme,
                                            )
                                            .into_any_element()]
                                        } else {
                                            files
                                                .iter()
                                                .enumerate()
                                                .map(|(file_index, path)| {
                                                    let display = path
                                                        .strip_prefix(&root)
                                                        .unwrap_or(path)
                                                        .display()
                                                        .to_string();
                                                    let path = path.clone();
                                                    let selected = active_doc_path
                                                        .as_ref()
                                                        .is_some_and(|active| active == &path);
                                                    let ws = workspace_entity.clone();
                                                    sidebar_row(
                                                        display,
                                                        path.display().to_string(),
                                                        &theme,
                                                        selected,
                                                        SharedString::from(format!(
                                                            "sidebar-file-{file_index}"
                                                        )),
                                                        cx.listener(move |_, _, window, cx| {
                                                            ws.update(cx, |workspace, cx| {
                                                                let _ = workspace.open_document(
                                                                    path.clone(),
                                                                    window,
                                                                    cx,
                                                                );
                                                            });
                                                        }),
                                                    )
                                                    .into_any_element()
                                                })
                                                .collect::<Vec<_>>()
                                        }
                                    })
                            })
                            .when(root.is_none(), |panel| {
                                panel
                                    .child(section_header("Open Files", &theme))
                                    .child(div().px_3().pb_1().child(crate::ui::path_caption(theme.ui_text("No folder open"), &theme)))
                                    .children((0..tab_count).map(|index| {
                                        let workspace = self.workspace.read(cx);
                                        let tab = &workspace.tabs[index];
                                        let doc = tab.document.read(cx);
                                        let full_path = doc.path.as_ref()
                                            .map(|path| path.display().to_string())
                                            .unwrap_or_else(|| theme.ui_text("Unsaved document"));
                                        let title = display_tab_title(&tab.title, doc.path.is_none(), &theme);
                                        let label = if doc.dirty { format!("{title} •") } else { title };
                                        let ws = workspace_entity.clone();
                                        crate::ui::sidebar_row_with_location(label, full_path, &theme, index == active,
                                            SharedString::from(format!("sidebar-tab-{index}")),
                                            cx.listener(move |_, _, window, cx| {
                                                ws.update(cx, |workspace, cx| {
                                                    let _ = workspace.dispatch(WorkspaceCommand::SwitchTab(index), window, cx);
                                                });
                                            }))
                                    }))
                                    .when(!recent.workspaces.is_empty(), |panel| {
                                        panel.child(section_header("Recent Folders", &theme)).children(
                                            recent
                                                .workspaces
                                                .iter()
                                                .enumerate()
                                                .map(|(recent_index, path)| {
                                                    let label = basename_label(path);
                                                    let full_path = path.display().to_string();
                                                    let path = path.clone();
                                                    let ws = workspace_entity.clone();
                                                    sidebar_row(
                                                        label,
                                                        full_path,
                                                        &theme,
                                                        false,
                                                        SharedString::from(format!(
                                                            "recent-workspace-{recent_index}"
                                                        )),
                                                        cx.listener(move |_, _, _, cx| {
                                                            ws.update(cx, |workspace, cx| {
                                                                let _ = workspace.open_workspace(
                                                                    path.clone(),
                                                                    cx,
                                                                );
                                                            });
                                                        }),
                                                    )
                                                })
                                                .collect::<Vec<_>>(),
                                        )
                                    })
                            })
                            .when(root.is_some(), |panel| {
                                let outside: Vec<_> = self.workspace.read(cx).tabs.iter().enumerate()
                                    .filter_map(|(index, tab)| {
                                        let doc = tab.document.read(cx);
                                        if doc.path.as_ref().is_some_and(|path| path.starts_with(root.as_ref().unwrap())) {
                                            return None;
                                        }
                                        let path = doc.path.as_ref().map(|path| path.display().to_string())
                                            .unwrap_or_else(|| theme.ui_text("Unsaved document"));
                                        Some((index, tab.title.clone(), path, doc.dirty))
                                    }).collect();
                                if outside.is_empty() { return panel; }
                                panel.child(section_header("Files outside this folder", &theme))
                                    .children(outside.into_iter().map(|(index, title, path, dirty)| {
                                        let ws = workspace_entity.clone();
                                        let label = if dirty { format!("{title} •") } else { title };
                                        crate::ui::sidebar_row_with_location(label, path, &theme, index == active,
                                            SharedString::from(format!("outside-folder-tab-{index}")),
                                            cx.listener(move |_, _, window, cx| {
                                                ws.update(cx, |workspace, cx| {
                                                    let _ = workspace.dispatch(WorkspaceCommand::SwitchTab(index), window, cx);
                                                });
                                            }))
                                    }))
                            })
                            .when(!recent.files.is_empty(), |panel| {
                                panel.child(section_header("Recent Files", &theme)).children(
                                    recent.files.iter().enumerate().map(|(recent_index, path)| {
                                        let label = basename_label(path);
                                        let full_path = path.display().to_string();
                                        let path = path.clone();
                                        let ws = workspace_entity.clone();
                                        sidebar_row(label, full_path, &theme, false,
                                            SharedString::from(format!("recent-file-{recent_index}")),
                                            cx.listener(move |this, _, window, cx| {
                                                ws.update(cx, |workspace, cx| {
                                                    let _ = workspace.open_document(path.clone(), window, cx);
                                                });
                                                this.last_tab_reveal = None;
                                            }))
                                    }).collect::<Vec<_>>()
                                )
                            })
                    } else {
                        div().w(px(0.)).id("sidebar-closed")
                    }, sidebar_overlay))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .id("editor-area")
                            .relative()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .bg(theme.editor_bg)
                            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                                this.dismiss_panel_overlay(window, cx);
                            }))
                            .child(tab_strip)
                            .when(welcome, |area| {
                                area.child(empty_sidebar_state(
                                    &theme,
                                    cx.listener(|this, _, window, cx| this.open_folder(&OpenFolder, window, cx)),
                                    cx.listener(|this, _, window, cx| this.open_file(&OpenFile, window, cx)),
                                ))
                            })
                            .on_drop(cx.listener({
                                let ws = ws_editor_drop.clone();
                                move |this, paths: &ExternalPaths, window, cx| {
                                    if this.review.is_some() { return; }
                                    ws.update(cx, |workspace, cx| {
                                        let _ = workspace.dispatch(
                                            WorkspaceCommand::DropFiles {
                                                paths: paths.paths().to_vec(),
                                                target: DropTarget::Editor,
                                            },
                                            window,
                                            cx,
                                        );
                                    });
                                }
                            }))
                            .drag_over::<ExternalPaths>(move |style, _, _, _| {
                                style.bg(theme.drop_zone_bg)
                            })
                            .when_some(fm_for_window, |area, info| {
                                let ws = workspace_entity.clone();
                                let title = info
                                    .title
                                    .clone()
                                    .unwrap_or_else(|| "YAML frontmatter".into());
                                let description = info.description.clone().unwrap_or_default();
                                let tags = info.tags.clone().unwrap_or_default();
                                let yaml_preview = {
                                    let body = info.yaml_body.trim();
                                    let mut lines = body.lines();
                                    let first = lines.next().unwrap_or("");
                                    match lines.next() {
                                        Some(_) => format!("{first} …"),
                                        None => first.to_string(),
                                    }
                                };
                                area.child(
                                    div()
                                        .id("frontmatter-panel")
                                        .mx(px(24.))
                                        .mt(px(12.))
                                        .px(px(12.))
                                        .py(px(8.))
                                        .rounded_md()
                                        .border_1()
                                        .border_color(theme.separator)
                                        .bg(theme.sidebar_bg)
                                        .cursor_pointer()
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.secondary_text)
                                                .child("Frontmatter"),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(theme.frontmatter_text)
                                                .child(SharedString::from(title)),
                                        )
                                        .when(!description.is_empty(), |panel| {
                                            panel.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.secondary_text)
                                                    .child(SharedString::from(description)),
                                            )
                                        })
                                        .when(!tags.is_empty(), |panel| {
                                            panel.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.secondary_text)
                                                    .child(SharedString::from(format!("Tags: {tags}"))),
                                            )
                                        })
                                        .when(!yaml_preview.is_empty(), |panel| {
                                            panel.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.secondary_text)
                                                    .child(SharedString::from(yaml_preview)),
                                            )
                                        })
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.secondary_text)
                                                .child("Click to edit in source"),
                                        )
                                        .on_click(cx.listener(move |_, _, window, cx| {
                                            let _ = ws.update(cx, |workspace, cx| {
                                                workspace.dispatch(
                                                    WorkspaceCommand::EditFrontmatter,
                                                    window,
                                                    cx,
                                                )
                                            });
                                        })),
                                )
                            })
                            .child({
                                let workspace = self.workspace.read(cx);
                                let tab =
                                    workspace.active_tab().unwrap_or_else(|| &workspace.tabs[0]);
                                match tab.mode {
                                    crate::workspace::EditorMode::Wysiwyg => div()
                                        .flex_1()
                                        .min_h_0()
                                        .overflow_hidden()
                                        .p(px(24.))
                                        .child(tab.rich_view.clone())
                                        .into_any_element(),
                                    crate::workspace::EditorMode::Source => div()
                                        .flex_1()
                                        .min_h_0()
                                        .p(px(24.))
                                        .overflow_hidden()
                                        .child(tab.editor_view.clone())
                                        .into_any_element(),
                                    crate::workspace::EditorMode::Split => div()
                                        .flex_1()
                                        .min_h_0()
                                        .flex()
                                        .flex_row()
                                        .overflow_hidden()
                                        .child(
                                            div()
                                                .id("split-source")
                                                .flex_1()
                                                .min_w_0()
                                                .min_h_0()
                                                .p(px(12.))
                                                .overflow_hidden()
                                                .child(tab.editor_view.clone()),
                                        )
                                        .child(
                                            div()
                                                .w(px(1.))
                                                .h_full()
                                                .bg(theme.separator),
                                        )
                                        .child(
                                            div()
                                                .id("split-rich")
                                                .flex_1()
                                                .min_w_0()
                                                .overflow_hidden()
                                                .child(tab.rich_view.clone()),
                                        )
                                        .into_any_element(),
                                }
                            })
                            .child(gpui::deferred(self.render_find(&theme, editor_width, cx)).with_priority(1)),
                    )
                    .child(panel_layer(if outline_open {
                        div()
                            .w(px(OUTLINE_WIDTH))
                            .flex_shrink_0()
                            .h_full()
                            .id("outline-panel")
                            .when(outline_overlay, |panel| {
                                panel.absolute().right_0().top_0().shadow_lg().occlude()
                                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                    .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
                            })
                            .overflow_y_scroll()
                            .bg(theme.sidebar_bg)
                            .border_l_1()
                            .border_color(theme.separator)
                            .child(section_header("Outline", &theme))
                            .when(outline_items.is_empty(), |panel| {
                                panel.child(muted_hint("No headings in this document.", &theme))
                            })
                            .children(outline_items.iter().map(|(offset, level, title)| {
                                let ws = workspace_entity.clone();
                                let offset = *offset;
                                let level = *level;
                                outline_row(
                                    title.clone(),
                                    level,
                                    &theme,
                                    SharedString::from(format!("outline-item-{offset}")),
                                    cx.listener(move |_, _, window, cx| {
                                        let _ = ws.update(cx, |workspace, cx| {
                                            let source_focused = workspace.active_tab().is_some_and(|tab| {
                                                tab.active_editing_pane(window, cx) == EditingPane::Source
                                            });
                                            workspace.dispatch(
                                                WorkspaceCommand::JumpToHeading { offset },
                                                window,
                                                cx,
                                            )?;
                                            if workspace.panel_overlay == Some(Panel::Outline) {
                                                workspace.toggle_panel(
                                                    Panel::Outline,
                                                    f32::from(window.viewport_size().width),
                                                    cx,
                                                );
                                            }
                                            if let Some(tab) = workspace.active_tab() {
                                                if source_focused {
                                                    window.focus(&tab.editor.read(cx).focus_handle(cx), cx);
                                                } else {
                                                    window.focus(&tab.rich_view.read(cx).focus_handle(cx), cx);
                                                }
                                            }
                                            Ok::<_, anyhow::Error>(())
                                        });
                                    }),
                                )
                            }))
                    } else {
                        div().w(px(0.)).id("outline-closed")
                    }, outline_overlay)),
            )
            .child({
                let workspace = self.workspace.read(cx);
                let tab = workspace.active_tab();
                let path = tab
                    .map(|t| {
                        let path = t.document.read(cx).path.as_deref();
                        status_document_label(path, &display_tab_title(&t.title, path.is_none(), &theme))
                    })
                    .unwrap_or_else(|| theme.ui_text("Untitled"));
                let dirty = tab.map(|t| t.document.read(cx).dirty).unwrap_or(false);
                let draft_notice = dirty.then(|| workspace.recovery_notice()).flatten();
                let pending_opens = self.pending_external_opens.len();
                let words = tab.map(|t| t.document.read(cx).word_count()).unwrap_or(0);
                let (line, col) = tab
                    .map(|t| {
                        let doc = t.document.read(cx);
                        let rich_active = t.active_editing_pane(window, cx) == EditingPane::Wysiwyg;
                        let offset = if rich_active {
                            t.rich_view.read(cx).cursor_offset()
                        } else {
                            t.editor.read(cx).cursor_offset()
                        };
                        markrust_editor::cursor_line_col(&doc.buffer.content(), offset)
                    })
                    .unwrap_or((0, 0));
                let frontmatter_label = tab
                    .map(|t| {
                        let content = t.document.read(cx).buffer.content();
                        parse_frontmatter(&content)
                            .and_then(|info| info.title)
                            .map(|title| format!("  ·  {title}"))
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                let editing_hint = tab.and_then(|tab| {
                    (tab.active_editing_pane(window, cx) == EditingPane::Wysiwyg)
                        .then(|| tab.rich_view.read(cx).editing_context_hint())
                        .flatten()
                });
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .px_4()
                    .py_1()
                    .h(px(22.))
                    .bg(theme.status_bar_bg)
                    .border_t_1()
                    .border_color(theme.separator)
                    .text_xs()
                    .text_color(theme.status_bar_text)
                    .child(div().flex_1().min_w_0().truncate().child(if dirty { format!("{path} — {}", theme.ui_text("Unsaved changes")) } else { path }))
                    .children(draft_notice.map(|notice| div().id("private-draft-notice").max_w(px(260.)).min_w_0().mx_2().truncate().child(notice)))
                    .children((pending_opens > 0).then(|| div().id("pending-file-opens").flex_shrink_0().mx_2().child(format!("{pending_opens} files waiting"))))
                    .children(self.application_notice.clone().map(|notice| div().id("application-notice").max_w(px(260.)).min_w_0().mx_2().truncate().child(notice)))
                    .children(editing_hint.map(|hint| context_hint(hint, &theme)))
                    .child(div().flex_shrink_0().child(format!(
                        "Ln {}, Col {}  ·  {words} words{frontmatter_label}",
                        line + 1,
                        col + 1
                    )))
            })
            .child(gpui::deferred(self.render_palette(&theme, palette_open, window, cx)).with_priority(2))
            .child(gpui::deferred(self.render_open_path(&theme, window, cx)).with_priority(3))
            .child(gpui::deferred(self.render_review(&theme, window, cx)).with_priority(3))
            .when(self.review.is_none() && self.open_location.is_none() && self.find.is_none() && !self.workspace.read(cx).palette_open, |element| {
                element.child(gpui::deferred(crate::update_ui::render(&theme, f32::from(window.viewport_size().width), cx)).with_priority(4))
            })
    }
}

// A palette owns its native query input; a review uses the same entity as a
// noneditable shield. Neither may leave a retained editor input handler alive.
impl gpui::EntityInputHandler for MarkRustWindow {
    fn accepts_text_input(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.review.is_none()
            && self.workspace.read(cx).palette_open
            && self.palette_focus.is_focused(window)
    }

    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        *adjusted_range = None;
        if !self.accepts_text_input(window, cx) {
            return None;
        }
        let range = palette_range_from_utf16(&self.palette_query, range);
        *adjusted_range = Some(palette_range_to_utf16(&self.palette_query, range.clone()));
        Some(self.palette_query[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::UTF16Selection> {
        self.accepts_text_input(window, cx)
            .then(|| gpui::UTF16Selection {
                range: palette_range_to_utf16(
                    &self.palette_query,
                    self.palette_query_selection.clone(),
                ),
                reversed: self.palette_query_reversed,
            })
    }

    fn marked_text_range(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        if self.review.is_some()
            || !self.workspace.read(cx).palette_open
            || !self.palette_focus.is_focused(window)
        {
            return None;
        }
        self.palette_marked_range
            .clone()
            .map(|range| palette_range_to_utf16(&self.palette_query, range))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.palette_marked_range = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.accepts_text_input(window, cx) {
            return;
        }
        let range = range
            .map(|range| palette_range_from_utf16(&self.palette_query, range))
            .or_else(|| self.palette_marked_range.clone())
            .unwrap_or_else(|| self.palette_query_selection.clone());
        self.replace_palette_query(range, text, cx);
        window.invalidate_character_coordinates();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.accepts_text_input(window, cx) {
            return;
        }
        let range = range
            .map(|range| palette_range_from_utf16(&self.palette_query, range))
            .or_else(|| self.palette_marked_range.clone())
            .unwrap_or_else(|| self.palette_query_selection.clone());
        let inserted = self.replace_palette_query(range, text, cx);
        if !inserted.is_empty() {
            self.palette_marked_range = Some(inserted.clone());
        }
        if let Some(selected) = selected {
            let relative =
                palette_range_from_utf16(&self.palette_query[inserted.clone()], selected);
            self.palette_query_selection =
                inserted.start + relative.start..inserted.start + relative.end;
        }
        window.invalidate_character_coordinates();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: gpui::Bounds<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        if !self.accepts_text_input(window, cx) {
            return None;
        }
        let geometry = self.palette_input_geometry.as_ref()?;
        let range = palette_range_from_utf16(&self.palette_query, range);
        let left = geometry.line.x_for_index(range.start);
        let right = geometry.line.x_for_index(range.end);
        let visible_left = (geometry.origin.x + left).clamp(
            geometry.bounds.left(),
            (geometry.bounds.right() - px(2.)).max(geometry.bounds.left()),
        );
        let visible_right = (geometry.origin.x + right)
            .max(visible_left + px(2.))
            .min(geometry.bounds.right());
        Some(gpui::Bounds::new(
            gpui::point(visible_left, geometry.origin.y),
            gpui::size((visible_right - visible_left).max(px(0.)), px(22.)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        if !self.accepts_text_input(window, cx) {
            return None;
        }
        let geometry = self.palette_input_geometry.as_ref()?;
        if !geometry.bounds.contains(&point) {
            return None;
        }
        let byte = geometry
            .line
            .closest_index_for_x(point.x - geometry.origin.x);
        Some(
            self.palette_query[..palette_byte_boundary(&self.palette_query, byte)]
                .encode_utf16()
                .count(),
        )
    }
}

fn palette_query_scroll(caret_x: f32, viewport_width: f32) -> f32 {
    (caret_x - viewport_width + 4.).max(0.)
}

fn palette_byte_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn palette_range_from_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    let byte_at = |target: usize| {
        let mut utf16 = 0;
        for (byte, ch) in text.char_indices() {
            if utf16 + ch.len_utf16() > target {
                return byte;
            }
            utf16 += ch.len_utf16();
        }
        text.len()
    };
    let start = byte_at(range.start);
    start..byte_at(range.end).max(start)
}

fn palette_range_to_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    text[..palette_byte_boundary(text, range.start)]
        .encode_utf16()
        .count()
        ..text[..palette_byte_boundary(text, range.end)]
            .encode_utf16()
            .count()
}

fn palette_grapheme_left(text: &str, at: usize) -> usize {
    text[..palette_byte_boundary(text, at)]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(byte, _)| byte)
}

fn palette_grapheme_right(text: &str, at: usize) -> usize {
    let at = palette_byte_boundary(text, at);
    at + text[at..].graphemes(true).next().map_or(0, str::len)
}

fn palette_word_range(text: &str, at: usize) -> Range<usize> {
    let at = palette_byte_boundary(text, at);
    text.unicode_word_indices()
        .find(|(start, word)| at >= *start && at < start + word.len())
        .map(|(start, word)| start..start + word.len())
        .unwrap_or(at..at)
}

fn palette_single_line(text: &str) -> String {
    text.chars()
        .filter_map(|ch| match ch {
            '\r' | '\n' | '\t' => Some(' '),
            ch if ch.is_control() => None,
            ch => Some(ch),
        })
        .collect()
}

fn status_path_label(path: &Path) -> String {
    match (
        path.parent().and_then(|parent| parent.file_name()),
        path.file_name(),
    ) {
        (Some(dir), Some(file)) => {
            format!("{}/{}", dir.to_string_lossy(), file.to_string_lossy())
        }
        (_, Some(file)) => file.to_string_lossy().into_owned(),
        _ => path.display().to_string(),
    }
}

fn display_tab_title(title: &str, pathless: bool, theme: &EditorTheme) -> String {
    if pathless {
        if title == "Untitled" {
            return theme.ui_text("Untitled");
        }
        if let Some(number) = title
            .strip_prefix("Untitled ")
            .filter(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return format!("{} {number}", theme.ui_text("Untitled"));
        }
    }
    title.to_owned()
}

fn status_document_label(path: Option<&Path>, title: &str) -> String {
    path.map(status_path_label)
        .unwrap_or_else(|| title.to_owned())
}

fn basename_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn review_dimensions(viewport_width: f32, viewport_height: f32) -> (f32, f32) {
    (
        (viewport_width - 32.).clamp(0., 980.),
        (viewport_height - 32.).clamp(0., 720.),
    )
}

fn review_focus_index(current: Option<usize>, count: usize, reverse: bool) -> usize {
    match current {
        Some(index) => adjacent_tab(index, count, reverse).unwrap_or(0),
        None if reverse => count.saturating_sub(1),
        None => 0,
    }
}

fn review_source_lines(source: &str) -> Vec<ReviewLine> {
    source
        .split('\n')
        .enumerate()
        .map(|(index, line)| ReviewLine {
            text: format!(
                "{:>5}  {}",
                index + 1,
                line.strip_suffix('\r').unwrap_or(line)
            )
            .into(),
            tone: ReviewTone::Context,
        })
        .collect()
}

fn review_diff_lines(before: &str, after: &str) -> Vec<ReviewLine> {
    use similar::{ChangeTag, TextDiff};
    if before == after {
        return vec![ReviewLine {
            text: "No normalization changes are needed.".into(),
            tone: ReviewTone::Header,
        }];
    }
    // The time budget may choose a coarser diff, but never omits a changed byte.
    let diff = TextDiff::configure()
        .deadline(std::time::Instant::now() + std::time::Duration::from_millis(250))
        .diff_lines(before, after);
    let mut lines = Vec::new();
    for group in diff.grouped_ops(3) {
        let first = group.first().unwrap();
        let last = group.last().unwrap();
        let old = first.old_range().start..last.old_range().end;
        let new = first.new_range().start..last.new_range().end;
        lines.push(ReviewLine {
            text: format!(
                "@@ -{},{} +{},{} @@",
                old.start + 1,
                old.len(),
                new.start + 1,
                new.len()
            )
            .into(),
            tone: ReviewTone::Header,
        });
        for op in group {
            for change in diff.iter_changes(&op) {
                let (prefix, tone) = match change.tag() {
                    ChangeTag::Equal => ("  ", ReviewTone::Context),
                    ChangeTag::Delete => ("- ", ReviewTone::Removed),
                    ChangeTag::Insert => ("+ ", ReviewTone::Added),
                };
                let text = change.value().trim_end_matches('\n').trim_end_matches('\r');
                lines.push(ReviewLine {
                    text: format!("{prefix}{text}").into(),
                    tone,
                });
                if !change.value().ends_with('\n') {
                    lines.push(ReviewLine {
                        text: "\\ No newline at end of file".into(),
                        tone: ReviewTone::Header,
                    });
                }
            }
        }
    }
    lines
}

fn review_button(
    id: &'static str,
    label: &'static str,
    focus: &FocusHandle,
    bounds: &ScrollHandle,
    active: bool,
    theme: &EditorTheme,
    window: &Window,
) -> gpui::Stateful<gpui::Div> {
    let label: SharedString = theme.ui_text(label).into();
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .track_focus(focus)
        .track_scroll(bounds)
        .px_3()
        .h(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_sm()
        .flex_shrink_0()
        .cursor_pointer()
        .border_1()
        .border_color(if focus.is_focused(window) {
            theme.accent
        } else {
            theme.separator
        })
        .bg(if active {
            theme.accent.opacity(0.16)
        } else {
            theme.tab_active
        })
        .hover(|style| style.bg(theme.accent.opacity(0.12)))
        .child(label)
}

fn review_pane(
    id: &'static str,
    pane: &ReviewPane,
    label: &'static str,
    theme: &EditorTheme,
) -> gpui::Stateful<gpui::Div> {
    let lines = pane.lines.clone();
    let row_theme = theme.clone();
    let vertical = pane.vertical.0.borrow().base_handle.clone();
    let max_offset = f32::from(vertical.max_offset().y);
    let offset = f32::from(vertical.offset().y).abs();
    let thumb_fraction = if max_offset > 0. {
        (offset / max_offset).clamp(0., 1.)
    } else {
        0.
    };
    div()
        .id((gpui::ElementId::from(id), "pane"))
        .flex_1()
        .min_w_0()
        .min_h_0()
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(30.))
                .flex_shrink_0()
                .px_3()
                .flex()
                .items_center()
                .justify_between()
                .bg(theme.code_block_bg)
                .text_xs()
                .text_color(theme.secondary_text)
                .child(theme.ui_text(label))
                .child(format!("{} lines", pane.lines.len())),
        )
        .child(
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_hidden()
                .child(
                    div()
                        .id((gpui::ElementId::from(id), "horizontal"))
                        .size_full()
                        .min_h_0()
                        .min_w_0()
                        .overflow_x_scroll()
                        .track_scroll(&pane.horizontal)
                        .child(
                            uniform_list(id, lines.len(), move |range, _, _| {
                                range
                                    .map(|index| {
                                        let line = &lines[index];
                                        div()
                                            .h(px(20.))
                                            .px_3()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .font_family(row_theme.code_font_family.clone())
                                            .text_size(px(12.))
                                            .line_height(px(20.))
                                            .text_color(match line.tone {
                                                ReviewTone::Header => row_theme.secondary_text,
                                                _ => row_theme.text,
                                            })
                                            .bg(match line.tone {
                                                ReviewTone::Removed => {
                                                    gpui::rgb(0xef4444).opacity(0.10)
                                                }
                                                ReviewTone::Added => {
                                                    gpui::rgb(0x22c55e).opacity(0.10)
                                                }
                                                _ => row_theme.editor_bg.into(),
                                            })
                                            .child(line.text.clone())
                                    })
                                    .collect()
                            })
                            .w_full()
                            .min_w(px(pane.content_width))
                            .h_full()
                            .track_scroll(&pane.vertical),
                        ),
                )
                .children((max_offset > 0.).then(|| {
                    div()
                        .absolute()
                        .right(px(2.))
                        .top(px(4.
                            + thumb_fraction
                                * (f32::from(vertical.bounds().size.height) - 36.)
                                    .max(0.)))
                        .w(px(4.))
                        .h(px(28.))
                        .rounded_full()
                        .bg(theme.secondary_text.opacity(0.35))
                })),
        )
}

fn scroll_review_pane(pane: &ReviewPane, key: &gpui::Keystroke) {
    if key.key == "left" || key.key == "right" {
        let scroll = &pane.horizontal;
        let mut offset = scroll.offset();
        offset.x += px(if key.key == "left" { 80. } else { -80. });
        scroll.set_offset(offset);
        return;
    }
    let scroll = &pane.vertical.0.borrow().base_handle;
    let mut offset = scroll.offset();
    let page = f32::from(scroll.bounds().size.height).max(20.) - 20.;
    offset.y = match key.key.as_str() {
        "home" => px(0.),
        "end" => -scroll.max_offset().y,
        "pageup" => offset.y + px(page),
        "pagedown" => offset.y - px(page),
        "up" => offset.y + px(20.),
        "down" => offset.y - px(20.),
        _ => offset.y,
    };
    scroll.set_offset(offset);
}

fn review_blocked_actions() -> Vec<Box<dyn gpui::Action>> {
    vec![
        Box::new(Save),
        Box::new(SaveAs),
        Box::new(NormalizeMarkdown),
        Box::new(OpenFile),
        Box::new(OpenFolder),
        Box::new(NewDocument),
        Box::new(NewTab),
        Box::new(NextTab),
        Box::new(PreviousTab),
        Box::new(CloseTab),
        Box::new(Undo),
        Box::new(Redo),
        Box::new(Paste),
        Box::new(CommandPalette),
        Box::new(ToggleEditorMode),
        Box::new(ToggleMarkupHints),
        Box::new(ShowWysiwyg),
        Box::new(ShowSource),
        Box::new(ShowSplit),
        Box::new(ToggleSidebar),
        Box::new(ToggleOutline),
        Box::new(ExportHtml),
        Box::new(LoadRemoteImages),
        Box::new(markrust_editor::Cut),
        Box::new(markrust_editor::ToggleBold),
        Box::new(markrust_editor::ToggleItalic),
        Box::new(markrust_editor::ToggleCode),
        Box::new(markrust_editor::ToggleLink),
        Box::new(markrust_editor::ToggleStrikethrough),
        Box::new(markrust_editor::SetHeading1),
        Box::new(markrust_editor::SetHeading2),
        Box::new(markrust_editor::SetHeading3),
        Box::new(markrust_editor::SetHeading4),
        Box::new(markrust_editor::SetHeading5),
        Box::new(markrust_editor::SetHeading6),
        Box::new(markrust_editor::Paragraph),
        Box::new(markrust_editor::ToggleUnorderedList),
        Box::new(markrust_editor::ToggleOrderedList),
        Box::new(markrust_editor::ToggleTaskList),
        Box::new(markrust_editor::ToggleBlockquote),
        Box::new(markrust_editor::InsertHorizontalRule),
        Box::new(markrust_editor::InsertCodeBlock),
        Box::new(markrust_editor::InsertImage),
        Box::new(markrust_editor::InsertTable),
        Box::new(InsertTableRowBelow),
        Box::new(InsertTableRowAbove),
        Box::new(DeleteTableRow),
        Box::new(InsertTableColumnRight),
        Box::new(InsertTableColumnLeft),
        Box::new(DeleteTableColumn),
        Box::new(markrust_editor::Indent),
        Box::new(markrust_editor::Outdent),
    ]
}

fn prompt_review_failure(error: &std::io::Error, window: &mut Window, cx: &mut App) {
    let message = format!("Your document is unchanged.\n\n{error}");
    drop(window.prompt(
        PromptLevel::Warning,
        "Could not open review",
        Some(&message),
        &[PromptButton::ok("OK")],
        cx,
    ));
}

struct RecoveryWarningTooltip {
    message: SharedString,
    theme: EditorTheme,
    width: f32,
}

impl Render for RecoveryWarningTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("session-recovery-details")
            .w(px(self.width))
            .max_w(px(self.width))
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(self.theme.separator)
            .bg(self.theme.chrome_bg)
            .text_color(self.theme.text)
            .text_size(px(12.))
            .whitespace_normal()
            .shadow_md()
            .child(self.message.clone())
    }
}

// Nineteen 34px buttons, four 17px separators, and twenty-two 4px gaps.
const FORMATTING_TOOLS_WIDTH: f32 = 802.;

fn formatting_toolbar_overflows(width: f32) -> bool {
    width < FORMATTING_TOOLS_WIDTH + 24.
}

fn formatting_scroll_offset(current: f32, maximum: f32, viewport: f32, forward: bool) -> f32 {
    let step = (viewport * 0.8).max(34.);
    (current + if forward { -step } else { step }).clamp(-maximum.max(0.), 0.)
}

fn prompt_save_failure(error: &std::io::Error, window: &mut Window, cx: &mut App) {
    let _response = window.prompt(
        PromptLevel::Critical,
        "Could not save document",
        Some(&error.to_string()),
        &[PromptButton::ok("OK")],
        cx,
    );
}

fn keystroke_dismisses_editor_overlay(keystroke: &gpui::Keystroke) -> bool {
    // Panel shortcuts must reach their toggle handler with the current panel
    // state intact. Every other editor keystroke dismisses only floating
    // panels, without consuming input or changing the document's flex width.
    !(keystroke.modifiers.control
        && keystroke.modifiers.platform
        && matches!(keystroke.key.as_str(), "s" | "o"))
}

fn resolve_open_location(input: &str, base: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
    let input = input.trim();
    let input = input
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            input
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(input);
    if input.is_empty() {
        return Err("Enter a file or folder path.".into());
    }
    let path = if input == "~" {
        home.ok_or("Could not open this path.")?.to_path_buf()
    } else if let Some(relative) = input
        .strip_prefix("~/")
        .or_else(|| input.strip_prefix("~\\"))
    {
        home.ok_or("Could not open this path.")?.join(relative)
    } else {
        let path = PathBuf::from(input);
        if path.is_absolute() {
            path
        } else {
            base.join(path)
        }
    };
    let path = std::fs::canonicalize(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "Path does not exist.".into()
        } else {
            error.to_string()
        }
    })?;
    if !path.is_file() && !path.is_dir() {
        return Err("Could not open this path.".into());
    }
    Ok(path)
}

fn initial_find_result(matches: &[Range<usize>], caret: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(
        matches
            .iter()
            .position(|range| range.start >= caret)
            .unwrap_or(0),
    )
}

fn adjacent_find_result(active: Option<usize>, count: usize, previous: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let active = active
        .unwrap_or(if previous { 0 } else { count - 1 })
        .min(count - 1);
    Some(if previous {
        (active + count - 1) % count
    } else {
        (active + 1) % count
    })
}

fn adjacent_tab(active: usize, count: usize, previous: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let active = active.min(count - 1);
    Some(if previous {
        if active == 0 {
            count - 1
        } else {
            active - 1
        }
    } else if active + 1 == count {
        0
    } else {
        active + 1
    })
}

fn initial_welcome_allowed(
    folder_open: bool,
    tab_count: usize,
    saved_document: bool,
    dirty: bool,
    has_content: bool,
) -> bool {
    !folder_open && tab_count == 1 && !saved_document && !dirty && !has_content
}

#[cfg(test)]
mod navigation_tests {
    use super::*;

    #[test]
    fn palette_long_query_keeps_the_measured_caret_in_the_viewport() {
        assert_eq!(palette_query_scroll(20., 400.), 0.);
        assert_eq!(palette_query_scroll(396., 400.), 0.);
        let scroll = palette_query_scroll(900., 400.);
        assert_eq!(scroll, 504.);
        assert_eq!(900. - scroll, 396.);
        assert_eq!(palette_query_scroll(100., 400.), 0.);
    }

    #[test]
    fn palette_query_utf16_ranges_do_not_split_surrogate_pairs() {
        let text = "а🌍e\u{301}";
        assert_eq!(palette_range_from_utf16(text, 1..3), 2..6);
        assert_eq!(palette_range_from_utf16(text, 2..3), 2..6);
        assert_eq!(palette_range_from_utf16(text, 3..5), 6..9);
        assert_eq!(palette_range_to_utf16(text, 2..6), 1..3);
        assert_eq!(
            palette_range_from_utf16(text, 50..70),
            text.len()..text.len()
        );
        assert_eq!(palette_byte_boundary(text, 4), 2);
    }

    #[test]
    fn palette_query_navigation_and_word_selection_use_unicode_boundaries() {
        let text = "а🌍e\u{301}";
        assert_eq!(palette_grapheme_left(text, text.len()), 6);
        assert_eq!(palette_grapheme_left(text, 6), 2);
        assert_eq!(palette_grapheme_right(text, 2), 6);
        assert_eq!(palette_grapheme_right(text, 6), text.len());
        assert_eq!(
            palette_word_range("Это аффы", "Это ".len()),
            "Это ".len().."Это аффы".len()
        );
    }

    #[test]
    fn palette_query_paste_is_single_line_and_preserves_visible_unicode() {
        assert_eq!(
            palette_single_line("Source\nаффы\t🌍\u{0}"),
            "Source аффы 🌍"
        );
    }

    #[test]
    fn review_dimensions_never_escape_the_window_even_on_a_small_viewport() {
        assert_eq!(review_dimensions(1440., 1000.), (980., 720.));
        assert_eq!(review_dimensions(720., 600.), (688., 568.));
        assert_eq!(review_dimensions(300., 220.), (268., 188.));
        assert_eq!(review_dimensions(20., 20.), (0., 0.));
    }

    #[test]
    fn review_keyboard_focus_wraps_with_an_explicit_cancel_control() {
        assert_eq!(review_focus_index(None, 6, false), 0);
        assert_eq!(review_focus_index(None, 6, true), 5);
        assert_eq!(review_focus_index(Some(5), 6, false), 0);
        assert_eq!(review_focus_index(Some(0), 6, true), 5);
        assert_eq!(review_focus_index(Some(2), 6, false), 3);
    }

    #[test]
    fn review_diff_retains_changes_beyond_the_old_twenty_line_preview_limit() {
        let before: String = (0..500)
            .map(|index| format!("original {index}\n"))
            .collect();
        let after: String = (0..500)
            .map(|index| format!("normalized {index}\n"))
            .collect();
        let lines = review_diff_lines(&before, &after);
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.tone == ReviewTone::Removed)
                .count(),
            500
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.tone == ReviewTone::Added)
                .count(),
            500
        );
        assert!(lines.iter().any(|line| line.text == "+ normalized 499"));
        assert!(lines.iter().any(|line| line.text == "- original 499"));
    }

    #[test]
    fn review_diff_has_context_and_an_explicit_missing_final_newline_marker() {
        let lines = review_diff_lines("first\nold\nlast", "first\nnew\nlast\n");
        assert!(lines.iter().any(|line| line.text == "  first"));
        assert!(lines.iter().any(|line| line.text == "- old"));
        assert!(lines.iter().any(|line| line.text == "+ new"));
        assert!(lines
            .iter()
            .any(|line| line.text == "\\ No newline at end of file"));
    }

    #[test]
    fn review_raw_panes_preserve_blank_rows_unicode_and_a_trailing_empty_line() {
        let lines = review_source_lines("# Привет 👩‍🚀\n\nlast\n");
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].text, "    1  # Привет 👩‍🚀");
        assert_eq!(lines[1].text, "    2  ");
        assert_eq!(lines[3].text, "    4  ");
    }

    #[test]
    fn review_blocks_every_native_format_action_and_mutating_window_command() {
        let blocked = review_blocked_actions();
        let names: Vec<_> = blocked.iter().map(|action| action.name()).collect();
        let menus = menus::application_menus(MenuState::default());
        let format = menus.iter().find(|menu| menu.name == "Format").unwrap();
        for item in &format.items {
            if let gpui::MenuItem::Action { action, .. } = item {
                assert!(
                    names.contains(&action.name()),
                    "{} must not edit behind the review",
                    action.name()
                );
            }
        }
        for action in [
            Box::new(Save) as Box<dyn gpui::Action>,
            Box::new(SaveAs),
            Box::new(NewDocument),
            Box::new(NewTab),
            Box::new(CloseTab),
            Box::new(NextTab),
            Box::new(PreviousTab),
            Box::new(InsertTableRowBelow),
            Box::new(InsertTableRowAbove),
            Box::new(DeleteTableRow),
            Box::new(InsertTableColumnRight),
            Box::new(InsertTableColumnLeft),
            Box::new(DeleteTableColumn),
        ] {
            assert!(names.contains(&action.name()));
        }
        assert!(!names.contains(&<crate::menus::Quit as gpui::Action>::name_for_type()));
    }

    #[test]
    fn pathless_status_preserves_the_active_tab_title() {
        assert_eq!(status_document_label(None, "Untitled 2"), "Untitled 2");
        assert_eq!(
            status_document_label(None, "Recovered — notes.md"),
            "Recovered — notes.md"
        );
        assert_eq!(
            status_document_label(Some(Path::new("/Projects/Notes/notes.md")), "notes.md"),
            "Notes/notes.md"
        );
    }

    #[test]
    fn untitled_presentation_never_translates_real_filenames_or_custom_titles() {
        let mut theme = EditorTheme::light();
        theme.ui_strings = Arc::new(std::collections::BTreeMap::from([(
            "Untitled".to_owned(),
            "Localized title".to_owned(),
        )]));
        assert_eq!(
            display_tab_title("Untitled", true, &theme),
            "Localized title"
        );
        assert_eq!(
            display_tab_title("Untitled 12", true, &theme),
            "Localized title 12"
        );
        assert_eq!(display_tab_title("Untitled", false, &theme), "Untitled");
        assert_eq!(
            display_tab_title("Untitled.md", true, &theme),
            "Untitled.md"
        );
        assert_eq!(
            display_tab_title("Untitled draft", true, &theme),
            "Untitled draft"
        );
        assert_eq!(
            display_tab_title("Recovered — Untitled", true, &theme),
            "Recovered — Untitled"
        );
    }

    #[test]
    fn document_tab_cycles_wrap_without_leaving_the_tab_range() {
        assert_eq!(adjacent_tab(0, 3, true), Some(2));
        assert_eq!(adjacent_tab(2, 3, false), Some(0));
        assert_eq!(adjacent_tab(1, 3, false), Some(2));
        assert_eq!(adjacent_tab(1, 3, true), Some(0));
        assert_eq!(adjacent_tab(0, 1, false), Some(0));
        assert_eq!(adjacent_tab(0, 1, true), Some(0));
        assert_eq!(adjacent_tab(0, 0, false), None);
        assert_eq!(adjacent_tab(usize::MAX, 3, false), Some(0));
    }

    #[test]
    fn find_results_start_at_caret_and_cycle_safely() {
        let matches = vec![2..6, 12..16, 22..26];
        assert_eq!(initial_find_result(&matches, 0), Some(0));
        assert_eq!(initial_find_result(&matches, 8), Some(1));
        assert_eq!(initial_find_result(&matches, 100), Some(0));
        assert_eq!(initial_find_result(&[], 0), None);
        assert_eq!(adjacent_find_result(Some(2), 3, false), Some(0));
        assert_eq!(adjacent_find_result(Some(0), 3, true), Some(2));
        assert_eq!(adjacent_find_result(Some(usize::MAX), 3, false), Some(0));
        assert_eq!(adjacent_find_result(None, 0, false), None);
    }

    #[test]
    fn open_location_resolves_relative_absolute_home_and_quoted_paths() {
        struct TestDirectory(PathBuf);
        impl TestDirectory {
            fn new() -> Self {
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let path = std::env::temp_dir().join(format!(
                    "markrust-open-location-{}-{nonce}",
                    std::process::id()
                ));
                std::fs::create_dir(&path).unwrap();
                Self(path)
            }
            fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for TestDirectory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let temp = TestDirectory::new();
        let file = temp.path().join("with spaces.md");
        std::fs::write(&file, "# Note\n").unwrap();
        let expected = std::fs::canonicalize(&file).unwrap();
        assert_eq!(
            resolve_open_location("with spaces.md", temp.path(), None).unwrap(),
            expected
        );
        assert_eq!(
            resolve_open_location(&format!("\"{}\"", file.display()), temp.path(), None).unwrap(),
            expected
        );
        assert_eq!(
            resolve_open_location("~/with spaces.md", Path::new("/unused"), Some(temp.path()))
                .unwrap(),
            expected
        );
        assert_eq!(
            resolve_open_location("~", Path::new("/unused"), Some(temp.path())).unwrap(),
            std::fs::canonicalize(temp.path()).unwrap()
        );
        assert_eq!(
            resolve_open_location("", temp.path(), None).unwrap_err(),
            "Enter a file or folder path."
        );
        assert_eq!(
            resolve_open_location("absent.md", temp.path(), None).unwrap_err(),
            "Path does not exist."
        );
        assert!(resolve_open_location("~/with spaces.md", temp.path(), None).is_err());
    }

    #[test]
    fn welcome_is_exclusive_to_a_single_pristine_untitled_document() {
        assert!(initial_welcome_allowed(false, 1, false, false, false));
        assert!(!initial_welcome_allowed(true, 1, false, false, false));
        assert!(!initial_welcome_allowed(false, 2, false, false, false));
        assert!(!initial_welcome_allowed(false, 1, true, false, false));
        assert!(!initial_welcome_allowed(false, 1, false, true, false));
        assert!(!initial_welcome_allowed(false, 1, false, false, true));
    }

    #[test]
    fn recent_document_labels_are_compact_without_losing_the_full_tooltip_path() {
        let path = Path::new("/Users/writer/Projects/Notes/deep/meeting-notes.md");
        assert_eq!(basename_label(path), "meeting-notes.md");
        assert_eq!(basename_label(Path::new("/")), "/");
    }

    #[test]
    fn editor_input_dismisses_floating_panels_but_panel_toggles_do_not() {
        for key in ["a", "shift-a", "right", "cmd-left", "cmd-b", "escape"] {
            assert!(keystroke_dismisses_editor_overlay(
                &gpui::Keystroke::parse(key).unwrap()
            ));
        }
        for key in ["ctrl-cmd-s", "ctrl-cmd-o"] {
            assert!(!keystroke_dismisses_editor_overlay(
                &gpui::Keystroke::parse(key).unwrap()
            ));
        }
    }

    #[test]
    fn compact_formatting_toolbar_offers_overflow_before_tools_are_clipped() {
        assert!(formatting_toolbar_overflows(720.));
        assert!(formatting_toolbar_overflows(825.));
        assert!(!formatting_toolbar_overflows(826.));
        assert!(!formatting_toolbar_overflows(1200.));
    }

    #[test]
    fn formatting_overflow_navigation_reaches_both_ends_without_overscrolling() {
        assert_eq!(formatting_scroll_offset(0., 162., 640., true), -162.);
        assert_eq!(formatting_scroll_offset(-162., 162., 640., false), 0.);
        assert_eq!(formatting_scroll_offset(-162., 162., 640., true), -162.);
        assert_eq!(formatting_scroll_offset(0., 0., 1000., true), 0.);
        assert_eq!(formatting_scroll_offset(0., 100., 0., true), -34.);
    }
}
