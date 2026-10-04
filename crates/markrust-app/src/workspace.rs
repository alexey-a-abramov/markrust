// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use gpui::{
    AppContext, Context, Entity, EntityInputHandler, ExternalPaths, Focusable, Subscription, Task,
    Window,
};

use crate::config::{is_markdown, AppConfig, HighlightStyle, RecentWorkspaces};
use crate::drop::{
    classify_editor_drop, classify_window_drop, markdown_image_reference, DropIntent,
};
use crate::panels::{Panel, PanelLayout};
use crate::recovery::{
    read_text_for_restore, restore_kind, RecoveryEditingPane, RecoveryEditorMode, RecoveryError,
    RecoverySelection, RecoverySnapshot, RecoveryStore, RecoveryTab, RecoveryWarning,
    RecoveryWriteResult, RestoreKind, MAX_RECOVERY_SNAPSHOT_BYTES, MAX_RECOVERY_TABS,
    MAX_RECOVERY_TAB_BYTES, MAX_RECOVERY_TITLE_BYTES,
};
use crate::session::{
    classify_external_change, list_markdown_files, DropTarget, ExternalChangeAction,
    NormalizeReviewChoice, WorkspaceCommand,
};
use markrust_core::{Document, SelectionSnapshot};
use markrust_editor::{EditorCommand, MarkdownEditor, MarkdownEditorView, RichEditorView};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

const PENDING_WIDGET_SAVE_ERROR: &str =
    "Finish the active text composition or field before saving or reviewing.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaveStatus {
    Saved,
    Untitled,
    NeedsReview,
}

#[derive(Clone)]
pub(crate) struct NormalizationReview {
    pub document: Entity<Document>,
    pub revision: u64,
    pub original: String,
    pub normalized: String,
}

#[derive(Clone)]
pub(crate) struct ExternalReview {
    pub tab_id: usize,
    pub path: PathBuf,
    pub base: String,
    pub ours: String,
    pub theirs: String,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalResolution {
    KeepLocal,
    UseDisk,
    KeepBoth,
}

/// Which editing surface a tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorMode {
    /// Stable visual Markdown: optional context hints are paint-only and do
    /// not insert source punctuation into the shaped document.
    #[default]
    Wysiwyg,
    /// Literal Markdown with uniform monospace metrics and syntax colors.
    Source,
    /// Source on the left, WYSIWYG on the right.
    Split,
}

/// The editing surface that last owned input in a document tab. Unlike window
/// focus, this survives a tab switch or a temporary toolbar/panel interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditingPane {
    Source,
    #[default]
    Wysiwyg,
}

impl EditorMode {
    pub fn editing_pane(self, remembered: EditingPane) -> EditingPane {
        match self {
            Self::Source => EditingPane::Source,
            Self::Wysiwyg => EditingPane::Wysiwyg,
            Self::Split => remembered,
        }
    }
}

#[allow(dead_code)]
pub struct DocumentTab {
    pub id: usize,
    pub document: Entity<Document>,
    pub editor: Entity<MarkdownEditor>,
    pub editor_view: Entity<MarkdownEditorView>,
    pub rich_view: Entity<RichEditorView>,
    pub mode: EditorMode,
    pub editing_pane: EditingPane,
    pub title: String,
    /// Recovery conflict tabs retain their buffer but must never be saved by
    /// the background autosave path. An explicit user save resolves it.
    autosave_blocked: bool,
    /// The disk base captured before a crash. It is kept independently of
    /// current disk bytes when a restored tab detects an external change.
    recovery_saved_base: Option<String>,
    recovery_disk_changed: bool,
    _focus_subscriptions: Vec<Subscription>,
}

struct RestoredRecoveryState {
    autosave_blocked: bool,
    saved_base: Option<String>,
    disk_changed: bool,
    archived: bool,
}

impl DocumentTab {
    pub fn active_editing_pane(&self, window: &Window, cx: &gpui::App) -> EditingPane {
        let rich = self.rich_view.read(cx);
        let remembered = if rich.is_focused(window) || rich.image_input_is_focused(window, cx) {
            EditingPane::Wysiwyg
        } else if self.editor.read(cx).focus_handle(cx).is_focused(window) {
            EditingPane::Source
        } else {
            self.editing_pane
        };
        self.mode.editing_pane(remembered)
    }
}

pub struct Workspace {
    pub root: Option<PathBuf>,
    pub tabs: Vec<DocumentTab>,
    pub active_tab: usize,
    pub next_tab_id: usize,
    pub sidebar_open: bool,
    pub outline_open: bool,
    pub panel_overlay: Option<Panel>,
    last_panel_layout: Option<(f32, EditorMode)>,
    pub palette_open: bool,
    pub config: AppConfig,
    persist_config: bool,
    pub pending_external_change: Option<(usize, PathBuf)>,
    // The UI renders an index/path pair, but an index alone can be rebased by
    // closing another tab before the banner is clicked. Keep the tab identity
    // privately so reload can never apply an old banner to a different tab.
    pending_external_change_tab_id: Option<usize>,
    pub recent: RecentWorkspaces,
    cached_files: Vec<PathBuf>,
    _watcher: Option<RecommendedWatcher>,
    _watcher_task: Task<()>,
    _file_list_task: Task<()>,
    recovery_store: Option<RecoveryStore>,
    recovery_archives: Vec<RecoveryTab>,
    recovery_warning: Option<RecoveryWarning>,
    recovery_generation: u64,
    recovery_scheduled: bool,
    _recovery_task: Task<()>,
    _quit_subscription: Option<Subscription>,
}

#[allow(dead_code)]
impl Workspace {
    /// Synchronize only passive paint state, using the actual focused input
    /// owner. Remembered tab ownership is not permission to paint a ghost
    /// while a palette, modal, or widget is receiving keyboard input.
    pub(crate) fn sync_split_shadow(&mut self, window: &Window, cx: &mut Context<Self>) {
        for (index, tab) in self.tabs.iter().enumerate() {
            let source = tab.editor.read(cx);
            let rich = tab.rich_view.read(cx);
            let enabled = index == self.active_tab
                && tab.mode == EditorMode::Split
                && !self.palette_open
                && !rich.has_pending_widget_edit()
                && !rich.has_image_editor();
            let revision = tab.document.read(cx).revision();
            let source_shadow = (enabled && rich.is_focused(window)).then(|| {
                markrust_editor::shadow::ShadowSelection {
                    revision,
                    range: rich.selected_range.clone(),
                    reversed: rich.selection_reversed,
                }
            });
            let rich_shadow = (enabled && source.focus_handle.is_focused(window)).then(|| {
                markrust_editor::shadow::ShadowSelection {
                    revision,
                    range: source.selected_range.clone(),
                    reversed: source.selection_reversed,
                }
            });
            tab.editor.update(cx, |editor, cx| {
                editor.set_shadow_selection(source_shadow, cx)
            });
            tab.rich_view.update(cx, |editor, cx| {
                editor.set_shadow_selection(rich_shadow, cx)
            });
        }
    }

    pub fn new(config: AppConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_with_recent(
            config,
            RecentWorkspaces::load(),
            RecoveryStore::production(),
            true,
            window,
            cx,
        )
    }

    /// App startup assigns each saved session once; new windows receive a
    /// freshly allocated private store instead of reopening another owner.
    pub fn new_with_recovery_store(
        config: AppConfig,
        recovery_store: Option<RecoveryStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_recent(
            config,
            RecentWorkspaces::load(),
            recovery_store,
            true,
            window,
            cx,
        )
    }

    #[cfg(feature = "gui-tests")]
    pub(crate) fn new_for_gui_tests(
        config: AppConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_recent(config, RecentWorkspaces::default(), None, false, window, cx)
    }

    /// Test-only construction with an explicit temporary recovery location.
    /// This keeps GUI restart checks away from the user's recovery directory.
    #[cfg(feature = "gui-tests")]
    pub(crate) fn new_for_recovery_tests(
        config: AppConfig,
        recovery_store: RecoveryStore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_recent(
            config,
            RecentWorkspaces::default(),
            Some(recovery_store),
            false,
            window,
            cx,
        )
    }

    fn new_with_recent(
        config: AppConfig,
        recent: RecentWorkspaces,
        recovery_store: Option<RecoveryStore>,
        persist_config: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut workspace = Self {
            root: None,
            tabs: Vec::new(),
            active_tab: 0,
            next_tab_id: 1,
            sidebar_open: true,
            outline_open: true,
            panel_overlay: None,
            last_panel_layout: None,
            palette_open: false,
            config,
            persist_config,
            pending_external_change: None,
            pending_external_change_tab_id: None,
            recent,
            cached_files: Vec::new(),
            _watcher: None,
            _watcher_task: Task::ready(()),
            _file_list_task: Task::ready(()),
            recovery_store,
            recovery_archives: Vec::new(),
            recovery_warning: None,
            recovery_generation: 0,
            recovery_scheduled: false,
            _recovery_task: Task::ready(()),
            _quit_subscription: None,
        };
        workspace.restore_session(window, cx);
        if workspace.persist_config && workspace.recovery_store.is_none() {
            workspace.recovery_warning = Some(RecoveryWarning::WriteFailed(
                "Private draft recovery is unavailable. Keep this window open or explicitly Save your edits before closing.".into(),
            ));
        }
        if workspace.tabs.is_empty() {
            workspace.new_document(window, cx);
        }
        if workspace.recovery_store.is_some() {
            workspace._quit_subscription =
                Some(cx.on_app_quit(|workspace, cx| workspace.flush_recovery_on_quit(cx)));
        }
        workspace
    }

    /// Apply a workspace command from the GPUI window adapter.
    pub fn dispatch(
        &mut self,
        command: WorkspaceCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        match command {
            WorkspaceCommand::Editor(editor_command) => {
                self.remember_active_editing_pane(window, cx);
                let tab_id = self.active_tab().map(|t| t.id);
                let use_rich = self
                    .active_tab()
                    .is_some_and(|tab| tab.active_editing_pane(window, cx) == EditingPane::Wysiwyg);
                if use_rich {
                    if let Some(tab) = self.tabs.get(self.active_tab) {
                        tab.rich_view.update(cx, |view, cx| {
                            view.apply_editor_command(editor_command.clone(), cx);
                        });
                    }
                } else if let Some(tab) = self.active_tab() {
                    tab.editor.update(cx, |editor, cx| {
                        editor.apply_command(editor_command, cx);
                    });
                }
                if let Some(id) = tab_id {
                    self.schedule_autosave(id, cx);
                }
            }
            WorkspaceCommand::Save => self.save_active(cx),
            WorkspaceCommand::SaveAs(path) => {
                if let Some(document) = self.active_tab().map(|tab| tab.document.clone()) {
                    let _ = self.save_document_as(document, path, cx);
                }
            }
            WorkspaceCommand::OpenFile(path) => {
                self.open_document(path, window, cx)?;
            }
            WorkspaceCommand::OpenFolder(path) => {
                self.open_workspace(path, cx)?;
            }
            WorkspaceCommand::OpenLaunchPath(path) => {
                self.open_launch_path(path, window, cx)?;
            }
            WorkspaceCommand::ExportHtml { output } => {
                let tab = self
                    .active_tab()
                    .ok_or_else(|| anyhow::anyhow!("no active document"))?;
                let doc = tab.document.read(cx);
                let path = doc
                    .path
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("save the document before exporting"))?;
                markrust_core::export_content_to_html(
                    &doc.buffer.content(),
                    Some(&path),
                    output.as_deref(),
                )?;
            }
            WorkspaceCommand::DropFiles { paths, target } => {
                self.handle_drop_paths(paths, target, window, cx);
            }
            WorkspaceCommand::ToggleTheme => self.toggle_theme(window, cx),
            WorkspaceCommand::NewDocument => self.new_document(window, cx),
            WorkspaceCommand::CloseTab => {
                let index = self.active_tab;
                self.close_tab(index, window, cx);
            }
            WorkspaceCommand::SwitchTab(index) => {
                if index < self.tabs.len() {
                    self.remember_active_editing_pane(window, cx);
                    self.active_tab = index;
                    self.focus_active_editor(window, cx);
                    self.schedule_recovery_snapshot(cx);
                    cx.notify();
                }
            }
            WorkspaceCommand::JumpToHeading { offset } => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    match tab.mode {
                        EditorMode::Wysiwyg => {
                            tab.rich_view.update(cx, |view, cx| {
                                view.jump_to(offset, cx);
                            });
                        }
                        EditorMode::Source => {
                            tab.editor.update(cx, |editor, cx| {
                                editor.apply_command(EditorCommand::JumpTo(offset), cx);
                            });
                        }
                        EditorMode::Split => {
                            tab.rich_view.update(cx, |view, cx| {
                                view.jump_to(offset, cx);
                            });
                            tab.editor.update(cx, |editor, cx| {
                                editor.apply_command(EditorCommand::JumpTo(offset), cx);
                            });
                        }
                    }
                }
            }
            WorkspaceCommand::EditFrontmatter => {
                // Use the same transition as the mode picker: a direct mode
                // assignment can leave a hidden rich editor owning keyboard
                // input and retain Split's literal-source layout.
                self.set_editor_mode(EditorMode::Source, window, cx);
                if let Some(tab) = self.active_tab() {
                    tab.editor.update(cx, |editor, cx| {
                        editor.apply_command(EditorCommand::JumpTo(0), cx);
                    });
                    tab.editor.read(cx).focus_handle(cx).focus(window, cx);
                    cx.notify();
                }
            }
            WorkspaceCommand::SetFrontmatterField { key, value } => {
                if let Some((view, id)) =
                    self.active_tab().map(|tab| (tab.rich_view.clone(), tab.id))
                {
                    view.update(cx, |view, cx| {
                        view.apply_rich(
                            markrust_core::rich::RichCommand::SetFrontmatterField { key, value },
                            cx,
                        );
                    });
                    self.schedule_autosave(id, cx);
                }
            }
            WorkspaceCommand::SaveWithReview(choice) => {
                self.save_with_review(choice, cx);
            }
            WorkspaceCommand::AdvanceTime { .. } => {}
            WorkspaceCommand::ExternalFileChange(path) => {
                if let Some(index) = self.tab_index_for_path(&path, cx) {
                    if let Ok(theirs) = std::fs::read_to_string(&path) {
                        self.apply_external_bytes(index, path, theirs, cx);
                    }
                }
            }
            WorkspaceCommand::ReloadTab(index) => {
                self.reload_tab(index, cx)?;
            }
        }
        Ok(())
    }

    /// Cycle the active tab through the same modes offered by View and the toolbar.
    pub fn toggle_editor_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_tab() {
            let mode = match tab.mode {
                EditorMode::Wysiwyg => EditorMode::Source,
                EditorMode::Source => EditorMode::Split,
                EditorMode::Split => EditorMode::Wysiwyg,
            };
            self.set_editor_mode(mode, window, cx);
        }
    }

    pub fn set_editor_mode(
        &mut self,
        mode: EditorMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remember_active_editing_pane(window, cx);
        if self.active_tab().is_some_and(|tab| tab.mode == mode) {
            self.focus_active_editor(window, cx);
            self.schedule_recovery_snapshot(cx);
            return;
        }
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            let from_rich = tab.active_editing_pane(window, cx) == EditingPane::Wysiwyg;
            let (range, reversed) = if from_rich {
                let view = tab.rich_view.read(cx);
                (view.selected_range.clone(), view.selection_reversed)
            } else {
                let editor = tab.editor.read(cx);
                (editor.selected_range.clone(), editor.selection_reversed)
            };
            // Both surfaces address the same Markdown bytes. Carry the caret and
            // anchor across instead of jumping back to each surface's old caret.
            if matches!(mode, EditorMode::Source | EditorMode::Split) {
                tab.editor.update(cx, |editor, cx| {
                    editor.apply_command(
                        EditorCommand::SetSelection {
                            start: range.start,
                            end: range.end,
                        },
                        cx,
                    );
                    editor.selection_reversed = reversed;
                });
            }
            tab.editor.update(cx, |editor, cx| {
                editor.set_raw_source(mode != EditorMode::Wysiwyg, cx);
            });
            if matches!(mode, EditorMode::Wysiwyg | EditorMode::Split) {
                tab.rich_view.update(cx, |view, cx| {
                    view.apply_editor_command(
                        EditorCommand::SetSelection {
                            start: range.start,
                            end: range.end,
                        },
                        cx,
                    );
                    view.selection_reversed = reversed;
                });
            }
            let rich_view = tab.rich_view.clone();
            tab.mode = mode;
            tab.editing_pane = mode.editing_pane(if from_rich {
                EditingPane::Wysiwyg
            } else {
                EditingPane::Source
            });
            self.focus_active_editor(window, cx);
            if matches!(mode, EditorMode::Wysiwyg | EditorMode::Split) {
                rich_view.update(cx, |view, cx| view.request_caret_reveal(cx));
            }
            self.schedule_recovery_snapshot(cx);
            cx.notify();
        }
    }

    pub(crate) fn focus_active_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_tab() {
            match tab.mode.editing_pane(tab.editing_pane) {
                EditingPane::Source => tab.editor.read(cx).focus_handle(cx).focus(window, cx),
                EditingPane::Wysiwyg => {
                    tab.rich_view
                        .update(cx, |rich, cx| rich.focus_current_input(window, cx));
                }
            }
        }
    }

    pub(crate) fn remember_active_editing_pane(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            // Focus subscriptions can arrive after a tab transition (or not
            // fire for an inactive test window). Capture the actual owner
            // before it is replaced, rather than depending on those events.
            tab.editing_pane = tab.active_editing_pane(window, cx);
            tab.rich_view
                .update(cx, |rich, cx| rich.remember_image_input_owner(window, cx));
        }
    }

    /// Paste via the focused editor's input handler so table, link and frontmatter
    /// drafts receive the text instead of accidentally mutating the document body.
    pub fn paste(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_active_editing_pane(window, cx);
        if let Some(tab) = self.active_tab() {
            let id = tab.id;
            let rich_view = tab.rich_view.read(cx);
            if rich_view.image_input_is_focused(window, cx)
                || (rich_view.has_image_editor() && rich_view.is_focused(window))
            {
                tab.rich_view.update(cx, |rich, cx| {
                    rich.paste_image_text(text, window, cx);
                });
                self.schedule_recovery_snapshot(cx);
                return;
            }
            let rich = tab.active_editing_pane(window, cx) == EditingPane::Wysiwyg;
            let selection = |cx: &gpui::App| {
                let (range, reversed) = if rich {
                    let view = tab.rich_view.read(cx);
                    (view.selected_range.clone(), view.selection_reversed)
                } else {
                    let editor = tab.editor.read(cx);
                    (editor.selected_range.clone(), editor.selection_reversed)
                };
                SelectionSnapshot {
                    start: range.start,
                    end: range.end,
                    reversed,
                }
            };
            let before = selection(cx);
            let checkpoint = tab
                .document
                .update(cx, |doc, _| doc.begin_undo_group(before));
            if rich {
                tab.rich_view.update(cx, |view, cx| {
                    view.replace_text_in_range(None, text, window, cx);
                });
            } else {
                tab.editor.update(cx, |editor, cx| {
                    editor.replace_text_in_range(None, text, window, cx);
                });
            }
            let after = selection(cx);
            tab.document
                .update(cx, |doc, _| doc.finish_undo_group(checkpoint, after));
            self.schedule_autosave(id, cx);
        }
    }

    pub fn active_tab(&self) -> Option<&DocumentTab> {
        self.tabs.get(self.active_tab)
    }

    pub fn ensure_panel_layout(&mut self, width: f32, cx: &mut Context<Self>) {
        let mode = self.active_tab().map(|tab| tab.mode).unwrap_or_default();
        if self.last_panel_layout == Some((width, mode)) {
            return;
        }
        self.last_panel_layout = Some((width, mode));
        let layout = PanelLayout::fit(width, mode, self.sidebar_open, self.outline_open);
        self.apply_panel_layout(layout, cx);
    }

    pub fn toggle_panel(&mut self, panel: Panel, width: f32, cx: &mut Context<Self>) {
        let mode = self.active_tab().map(|tab| tab.mode).unwrap_or_default();
        let layout = PanelLayout {
            sidebar: self.sidebar_open,
            outline: self.outline_open,
            overlay: self.panel_overlay,
        }
        .toggle(panel, width, mode);
        self.last_panel_layout = Some((width, mode));
        self.apply_panel_layout(layout, cx);
    }

    fn apply_panel_layout(&mut self, layout: PanelLayout, cx: &mut Context<Self>) {
        if self.sidebar_open != layout.sidebar
            || self.outline_open != layout.outline
            || self.panel_overlay != layout.overlay
        {
            self.sidebar_open = layout.sidebar;
            self.outline_open = layout.outline;
            self.panel_overlay = layout.overlay;
            cx.notify();
        }
    }

    /// The latest recovery I/O or fallback warning, if any. The window layer
    /// can render this without inspecting recovery files or document contents.
    pub fn recovery_warning(&self) -> Option<&RecoveryWarning> {
        self.recovery_warning.as_ref()
    }

    /// Compact chrome keeps the required action visible even when a file's
    /// full path is too long. The original warning remains the detail text.
    pub fn recovery_warning_summary(&self, cx: &gpui::App) -> Option<String> {
        let warning = self.recovery_warning.as_ref()?;
        Some(match warning {
            RecoveryWarning::ReadFailed(_) => {
                "Recovery unavailable — save your work explicitly.".into()
            }
            RecoveryWarning::RestoredBackup(_) => {
                "Recovered from backup — review restored documents.".into()
            }
            RecoveryWarning::WriteFailed(_) => {
                "Recovery save failed — save your work explicitly.".into()
            }
            RecoveryWarning::CapacityReached(_) => {
                "Recovery full — save your work explicitly.".into()
            }
            RecoveryWarning::PendingWidget(_) => {
                "Apply or cancel the unfinished field before continuing.".into()
            }
            RecoveryWarning::DiskChanged(_) => self
                .tabs
                .iter()
                .find(|tab| tab.recovery_disk_changed)
                .map(|tab| {
                    recovery_disk_warning_summary(tab.document.read(cx).path.as_deref(), &tab.title)
                })
                .unwrap_or_else(|| {
                    "Recovered changes need Review External Changes or Save As.".into()
                }),
        })
    }

    /// Explicit persistence boundaries must include a visible rich-widget
    /// draft. Background autosave intentionally never calls this: a user may
    /// still be typing an incomplete field there.
    fn commit_pending_widget_edit_for_tab(&self, index: usize, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.tabs.get(index) else {
            return true;
        };
        if tab.editor.read(cx).marked_range.is_some()
            || tab.rich_view.read(cx).has_pending_composition()
        {
            return false;
        }
        tab.rich_view
            .update(cx, |view, cx| view.commit_pending_widget_edit(cx))
    }

    fn clear_pending_widget_warning(&mut self) {
        if matches!(
            self.recovery_warning.as_ref(),
            Some(RecoveryWarning::PendingWidget(_))
        ) {
            self.recovery_warning = None;
        }
    }

    fn warn_pending_widget(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.recovery_warning = Some(RecoveryWarning::PendingWidget(message.into()));
        cx.notify();
    }

    fn set_pending_external_change(&mut self, index: usize, path: PathBuf, cx: &gpui::App) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.document.read(cx).path.as_deref() != Some(path.as_path()) {
            return;
        }
        self.pending_external_change = Some((index, path));
        self.pending_external_change_tab_id = Some(tab.id);
    }

    fn clear_pending_external_change_for_tab(&mut self, tab_id: usize) {
        if self.pending_external_change_tab_id == Some(tab_id) {
            self.pending_external_change = None;
            self.pending_external_change_tab_id = None;
        }
    }

    /// Keep the public banner index synchronized with its immutable tab
    /// identity after tabs are removed or reordered. A stale banner is simply
    /// dismissed; it must never reload whichever tab inherited its index.
    fn reconcile_pending_external_change(&mut self, cx: &gpui::App) {
        let Some((_, path)) = self.pending_external_change.clone() else {
            self.pending_external_change_tab_id = None;
            return;
        };
        let Some(tab_id) = self.pending_external_change_tab_id else {
            self.pending_external_change = None;
            return;
        };
        let matching_index = self.tabs.iter().position(|tab| {
            tab.id == tab_id && tab.document.read(cx).path.as_deref() == Some(path.as_path())
        });
        if let Some(index) = matching_index {
            self.pending_external_change = Some((index, path));
        } else {
            self.pending_external_change = None;
            self.pending_external_change_tab_id = None;
        }
    }

    fn pending_external_change_matches_tab(&self, index: usize, cx: &gpui::App) -> bool {
        let Some((pending_index, pending_path)) = self.pending_external_change.as_ref() else {
            return true;
        };
        let Some(pending_tab_id) = self.pending_external_change_tab_id else {
            return false;
        };
        *pending_index == index
            && self.tabs.get(index).is_some_and(|tab| {
                tab.id == pending_tab_id
                    && tab.document.read(cx).path.as_deref() == Some(pending_path.as_path())
            })
    }

    /// Disk-change recovery state is tab-scoped. Rebuild the warning after a
    /// tab transition so an explicit save or close cannot leave a banner that
    /// reloads an unrelated path.
    fn refresh_recovery_disk_warning(&mut self, cx: &gpui::App) {
        let conflict = self.tabs.iter().find_map(|tab| {
            tab.recovery_disk_changed
                .then(|| (tab.document.read(cx).path.clone(), tab.title.clone()))
        });
        match conflict {
            Some((Some(path), _))
                if matches!(
                    self.recovery_warning.as_ref(),
                    None | Some(RecoveryWarning::DiskChanged(_))
                ) =>
            {
                self.recovery_warning = Some(RecoveryWarning::DiskChanged(
                    recovery_disk_warning_details(Some(&path), ""),
                ));
            }
            Some((None, title))
                if matches!(
                    self.recovery_warning.as_ref(),
                    None | Some(RecoveryWarning::DiskChanged(_))
                ) =>
            {
                self.recovery_warning = Some(RecoveryWarning::DiskChanged(
                    recovery_disk_warning_details(None, &title),
                ));
            }
            None if matches!(
                self.recovery_warning.as_ref(),
                Some(RecoveryWarning::DiskChanged(_))
            ) =>
            {
                self.recovery_warning = None;
            }
            _ => {}
        }
    }

    /// Request an immediate best-effort snapshot without waiting for the
    /// normal typing debounce. Normal interaction never blocks on the disk;
    /// the quit hook separately performs its final write.
    pub fn flush_recovery(&mut self, cx: &mut Context<Self>) {
        self.schedule_recovery_after(Duration::ZERO, cx);
    }

    pub fn recovery_notice(&self) -> Option<&'static str> {
        self.recovery_store.as_ref().map(|_| {
            if self.recovery_archives.is_empty() {
                "Private drafts · Save writes the file"
            } else {
                "Closed unsaved edits retained privately for next launch"
            }
        })
    }

    pub fn set_recovery_warning(&mut self, warning: RecoveryWarning, cx: &mut Context<Self>) {
        self.recovery_warning = Some(warning);
        cx.notify();
    }

    /// A close/quit gate, not a source-file Save. Persist before releasing the
    /// last live owner of any edits; callers must keep the window open on Err.
    pub fn checkpoint_before_close_or_quit(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<(), RecoveryError> {
        let result = self
            .capture_recovery_snapshot(cx)
            .and_then(|snapshot| self.write_recovery_checkpoint(&snapshot));
        if result.is_err() && !self.requires_private_recovery(cx) {
            // Failure to retain clean-session metadata is not a reason to
            // trap already-saved documents in the app. No draft owner is lost.
            if let Err(error) = &result {
                self.recovery_warning = Some(RecoveryWarning::WriteFailed(format!(
                    "Private session recovery is unavailable, but all current documents are saved and can be closed safely: {error}."
                )));
                eprintln!("MarkRust clean session checkpoint unavailable: {error}");
            }
            cx.notify();
            return Ok(());
        }
        if let Err(error) = &result {
            self.recovery_warning = Some(match error {
                RecoveryError::Limit(message) => RecoveryWarning::CapacityReached(format!(
                    "Kept the window open because private recovery cannot retain its edits: {message}. Explicitly Save a copy before closing."
                )),
                error => RecoveryWarning::WriteFailed(format!(
                    "Kept the window open because its private draft checkpoint failed: {error}. Your live edits remain intact."
                )),
            });
            cx.notify();
        } else if matches!(
            self.recovery_warning,
            Some(RecoveryWarning::WriteFailed(_) | RecoveryWarning::CapacityReached(_))
        ) {
            self.recovery_warning = None;
            cx.notify();
        }
        result
    }

    /// Unlike app Quit, closing a clean window should not accumulate a stale
    /// Welcome session forever. Dirty documents and archived edits are retained.
    pub fn checkpoint_before_window_close(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<(), RecoveryError> {
        self.checkpoint_before_close_or_quit(cx)?;
        let clean = !self.requires_private_recovery(cx);
        if clean {
            if let Some(store) = &self.recovery_store {
                if let Err(error) = store.retire_clean_session() {
                    self.recovery_warning = Some(RecoveryWarning::WriteFailed(format!(
                        "Saved documents can be closed safely, but their old private session could not be retired: {error}."
                    )));
                    cx.notify();
                    // Retire validates the current snapshot before deleting.
                    // Leave unverified old data intact, without trapping an
                    // entirely saved window because storage is unavailable.
                    eprintln!("MarkRust clean session retirement skipped: {error}");
                }
            }
        }
        Ok(())
    }

    fn requires_private_recovery(&self, cx: &gpui::App) -> bool {
        !self.recovery_archives.is_empty()
            || self.tabs.iter().any(|tab| {
                let source = tab.editor.read(cx);
                let rich = tab.rich_view.read(cx);
                tab_requires_private_recovery(
                    tab.document.read(cx),
                    source.marked_range.is_some()
                        || rich.has_pending_composition()
                        || rich.has_pending_widget_edit(),
                )
            })
    }

    fn write_recovery_checkpoint(
        &mut self,
        snapshot: &RecoverySnapshot,
    ) -> Result<(), RecoveryError> {
        self.recovery_generation = self.recovery_generation.wrapping_add(1);
        self.recovery_scheduled = false;
        self._recovery_task = Task::ready(());
        let Some(store) = self.recovery_store.as_ref() else {
            // Deliberately memory-only isolated GUI fixtures keep their prior
            // behavior; production must never pretend a missing store is safe.
            return if self.persist_config {
                Err(RecoveryError::InvalidSnapshot(
                    "private recovery storage is unavailable".into(),
                ))
            } else {
                Ok(())
            };
        };
        let generation = self.recovery_generation;
        store.note_generation(generation);
        match store.write_if_current(snapshot, generation)? {
            RecoveryWriteResult::Written => Ok(()),
            RecoveryWriteResult::SkippedStale => Err(RecoveryError::InvalidSnapshot(
                "private draft checkpoint was superseded".into(),
            )),
        }
    }

    /// Capture and synchronously persist the final UI state before GPUI starts
    /// its short asynchronous shutdown deadline. Quit can take longer on a
    /// slow volume, but this bounded (16 MiB) write is safer than allowing a
    /// graceful quit to race a debounced checkpoint. Power loss or a failed
    /// filesystem operation can still defeat any local recovery mechanism.
    /// Invalid focused widget drafts cannot cancel GPUI shutdown; they remain
    /// an explicit recovery boundary and are reported before the document
    /// snapshot is written.
    fn flush_recovery_on_quit(
        &mut self,
        cx: &mut Context<Self>,
    ) -> impl std::future::Future<Output = ()> + 'static {
        if let Err(error) = self.checkpoint_before_close_or_quit(cx) {
            // GPUI's platform shutdown hook cannot veto the shutdown. App-menu
            // quit and window-close callers perform the cancellable gate first.
            eprintln!("MarkRust final private draft checkpoint failed: {error}");
        }
        async {}
    }

    fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = self.recovery_store.clone() else {
            return;
        };
        let loaded = store.load();
        self.recovery_warning = loaded.warning;
        let Some(snapshot) = loaded.snapshot else {
            return;
        };

        let restored_root = snapshot.root.filter(|root| root.is_dir());
        self.root = restored_root;
        let active_tab = snapshot.active_tab;
        let mut primary_tab_positions = Vec::new();

        for tab in snapshot.tabs {
            primary_tab_positions.push(self.tabs.len());
            self.restore_recovery_tab(tab, false, window, cx);
        }
        // Archives represent dirty tabs closed in the previous session. Once
        // restored as live tabs, they must not be retained as archives in the
        // next snapshot.
        for tab in snapshot.archived_tabs {
            self.restore_recovery_tab(tab, true, window, cx);
        }
        self.recovery_archives.clear();

        if !primary_tab_positions.is_empty() && !self.tabs.is_empty() {
            self.active_tab =
                primary_tab_positions[active_tab.min(primary_tab_positions.len() - 1)];
            self.focus_active_editor(window, cx);
        }
        if self.root.is_some() {
            self.start_watcher(cx);
            self.schedule_file_list_scan(cx);
        }
        if !self.tabs.is_empty() || self.recovery_warning.is_some() {
            cx.notify();
        }
    }

    fn restore_recovery_tab(
        &mut self,
        recovered: RecoveryTab,
        archived: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let disk = recovered
            .path
            .as_deref()
            .and_then(|path| match read_text_for_restore(path) {
                Ok(disk) => disk,
                Err(error) => {
                    self.recovery_warning = Some(RecoveryWarning::ReadFailed(format!(
                        "Could not read {} while restoring recovery: {error}",
                        path.display()
                    )));
                    None
                }
            });
        let kind = restore_kind(&recovered, disk.as_deref());
        // A closed dirty file is a recovery archive, not a second owner of
        // its former path. Restoring it without a path prevents duplicate
        // writable tabs from silently autosaving over the live document.
        let restored_path = if archived {
            None
        } else {
            recovered.path.clone()
        };
        let (document, autosave_blocked, recovery_saved_base, recovery_disk_changed) = match kind {
            RestoreKind::CurrentDisk => {
                let content = disk.as_deref().unwrap_or(&recovered.content);
                let mut document = Document::new(content);
                document.path = restored_path.clone();
                if let Some(path) = document.path.clone() {
                    document.configure_mode_from_path(&path);
                }
                (document, false, None, false)
            }
            RestoreKind::Recovered {
                disk_changed,
                autosave_blocked,
            } => {
                // Start from the recorded disk base, then apply the
                // recovered buffer as a dirty three-way state. In the
                // changed-disk case we intentionally keep that base so an
                // external-change prompt can explain the conflict; the
                // background autosave is separately blocked below.
                let mut document = Document::new(&recovered.saved_content);
                document.path = restored_path.clone();
                if let Some(path) = document.path.clone() {
                    document.configure_mode_from_path(&path);
                }
                document.apply_merged_edit(&recovered.content, &recovered.saved_content, &[]);
                // A previously clean file may have been deleted while
                // MarkRust was down. Keep its buffer dirty and autosave
                // blocked so only an explicit save can recreate a path.
                document.dirty = recovered.dirty || disk_changed;
                (
                    document,
                    autosave_blocked,
                    disk_changed.then(|| recovered.saved_content.clone()),
                    disk_changed,
                )
            }
        };
        let document = cx.new(|_| document);
        self.push_tab(document, restored_path, window, cx);
        let tab_index = self.tabs.len() - 1;
        self.apply_recovery_tab_state(
            tab_index,
            &recovered,
            RestoredRecoveryState {
                autosave_blocked: autosave_blocked || (archived && recovered.path.is_some()),
                saved_base: if archived { None } else { recovery_saved_base },
                disk_changed: recovery_disk_changed || (archived && recovered.path.is_some()),
                archived,
            },
            cx,
        );
        // push_tab initially focused Rich. Establish the restored input owner
        // before a later restored tab/scratch captures the departing focus;
        // otherwise Split/Source memory can be overwritten by that old focus.
        self.focus_active_editor(window, cx);
        if let Some(draft) = &recovered.widget_draft {
            let snapshot = draft.editor_snapshot();
            let restored = self.tabs[tab_index].rich_view.update(cx, |view, cx| {
                view.restore_recovery_widget_draft(&snapshot, cx)
            });
            if let Err(error) = restored {
                // A disk-changed body or stale anchor must not retarget a field.
                // Keep every raw draft byte in an explicitly pathless owner.
                let mut scratch = Document::new("");
                scratch.replace_range(0, 0, &draft.draft);
                scratch.dirty = true;
                let document = cx.new(|_| scratch);
                self.push_tab(document, None, window, cx);
                if let Some(tab) = self.tabs.last_mut() {
                    tab.title =
                        recovered_archive_title(&format!("field draft — {}", recovered.title));
                    tab.mode = EditorMode::Source;
                    tab.editing_pane = EditingPane::Source;
                    tab.autosave_blocked = true;
                    tab.editor.update(cx, |editor, cx| {
                        editor.apply_command(
                            EditorCommand::SetSelection {
                                start: draft.selection.start,
                                end: draft.selection.end,
                            },
                            cx,
                        );
                        editor.selection_reversed = draft.selection_reversed;
                    });
                }
                self.recovery_warning = Some(RecoveryWarning::PendingWidget(format!(
                    "Recovered the uncommitted field as a separate unsaved draft because {error}. Its source file was not changed."
                )));
            }
        }
        self.refresh_recovery_disk_warning(cx);
    }

    fn apply_recovery_tab_state(
        &mut self,
        tab_index: usize,
        recovered: &RecoveryTab,
        state: RestoredRecoveryState,
        cx: &mut Context<Self>,
    ) {
        let mode = match recovered.mode {
            RecoveryEditorMode::Wysiwyg => EditorMode::Wysiwyg,
            RecoveryEditorMode::Source => EditorMode::Source,
            RecoveryEditorMode::Split => EditorMode::Split,
        };
        let editing_pane = match recovered.editing_pane {
            RecoveryEditingPane::Source => EditingPane::Source,
            RecoveryEditingPane::Wysiwyg => EditingPane::Wysiwyg,
        };
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return;
        };
        tab.title = if state.archived {
            recovered_archive_title(&recovered.title)
        } else {
            recovered.title.clone()
        };
        tab.mode = mode;
        tab.editing_pane = mode.editing_pane(editing_pane);
        tab.autosave_blocked = state.autosave_blocked;
        tab.recovery_saved_base = state.saved_base;
        tab.recovery_disk_changed = state.disk_changed;
        tab.editor.update(cx, |editor, cx| {
            editor.set_raw_source(mode != EditorMode::Wysiwyg, cx);
            editor.apply_command(
                EditorCommand::SetSelection {
                    start: recovered.source_selection.start,
                    end: recovered.source_selection.end,
                },
                cx,
            );
            editor.selection_reversed = recovered.source_selection.reversed;
        });
        tab.rich_view.update(cx, |view, cx| {
            view.apply_editor_command(
                EditorCommand::SetSelection {
                    start: recovered.rich_selection.start,
                    end: recovered.rich_selection.end,
                },
                cx,
            );
            view.selection_reversed = recovered.rich_selection.reversed;
        });
    }

    fn schedule_recovery_snapshot(&mut self, cx: &mut Context<Self>) {
        self.schedule_recovery_after(
            Duration::from_millis(self.config.private_checkpoint_delay_ms()),
            cx,
        );
    }

    fn schedule_recovery_after(&mut self, delay: Duration, cx: &mut Context<Self>) {
        if self.recovery_store.is_none() {
            return;
        }
        self.recovery_generation = self.recovery_generation.wrapping_add(1);
        if self.recovery_scheduled && !delay.is_zero() {
            // A sustained stream of input must not postpone the first
            // checkpoint forever. Capture the latest state at its deadline.
            return;
        }
        self.recovery_scheduled = true;
        let workspace = cx.entity();
        self._recovery_task = cx.spawn(async move |_, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let job = workspace.update(cx, |workspace, cx| {
                let generation = workspace.recovery_generation;
                match workspace.capture_recovery_snapshot(cx) {
                    Ok(snapshot) => workspace
                        .recovery_store
                        .clone()
                        .map(|store| (store, snapshot, generation)),
                    Err(error) => {
                        workspace.recovery_warning = Some(RecoveryWarning::WriteFailed(format!(
                            "Could not prepare session recovery: {error}"
                        )));
                        cx.notify();
                        workspace.recovery_scheduled = false;
                        None
                    }
                }
            });
            let Some((store, snapshot, generation)) = job else {
                return;
            };
            store.note_generation(generation);
            let result = cx
                .background_executor()
                .spawn(async move { store.write_if_current(&snapshot, generation) })
                .await;
            workspace.update(cx, |workspace, cx| {
                workspace.recovery_scheduled = false;
                if workspace.recovery_generation != generation {
                    workspace.schedule_recovery_after(Duration::ZERO, cx);
                    return;
                }
                match result {
                    Ok(RecoveryWriteResult::Written) => {
                        if matches!(
                            workspace.recovery_warning,
                            Some(
                                RecoveryWarning::WriteFailed(_)
                                    | RecoveryWarning::CapacityReached(_)
                            )
                        ) {
                            workspace.recovery_warning = None;
                            cx.notify();
                        }
                    }
                    Ok(RecoveryWriteResult::SkippedStale) => {}
                    Err(error) => {
                        workspace.recovery_warning = Some(RecoveryWarning::WriteFailed(format!(
                            "Could not write session recovery: {error}"
                        )));
                        cx.notify();
                    }
                }
            });
        });
    }

    fn capture_recovery_snapshot(
        &self,
        cx: &gpui::App,
    ) -> Result<RecoverySnapshot, crate::recovery::RecoveryError> {
        self.preflight_recovery_snapshot(cx)?;
        let tabs = self
            .tabs
            .iter()
            .map(|tab| self.capture_recovery_tab(tab, cx))
            .collect::<Vec<_>>();
        let snapshot = RecoverySnapshot {
            version: crate::recovery::RECOVERY_VERSION,
            root: self.root.clone(),
            active_tab: self.active_tab.min(tabs.len().saturating_sub(1)),
            tabs,
            archived_tabs: self.recovery_archives.clone(),
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Bound recovery work before cloning Rope contents into a snapshot on the
    /// UI thread. The estimate counts JSON escaping and leaves fixed room for
    /// tab metadata, so an over-limit session fails without a large temporary
    /// allocation or visible editor stall.
    fn preflight_recovery_snapshot(&self, cx: &gpui::App) -> Result<(), RecoveryError> {
        let fallback_count = self
            .tabs
            .iter()
            .filter(|tab| {
                tab.rich_view
                    .read(cx)
                    .recovery_widget_draft_sizes()
                    .is_some()
            })
            .count()
            .saturating_add(
                self.recovery_archives
                    .iter()
                    .filter(|tab| tab.widget_draft.is_some())
                    .count(),
            );
        let entry_count = self
            .tabs
            .len()
            .saturating_add(self.recovery_archives.len())
            .saturating_add(fallback_count);
        if entry_count > MAX_RECOVERY_TABS {
            return Err(RecoveryError::Limit(format!(
                "session recovery has more than {MAX_RECOVERY_TABS} total tab entries"
            )));
        }

        let mut estimated = 8 * 1024usize;
        if let Some(root) = self.root.as_deref() {
            add_recovery_estimate(&mut estimated, estimated_json_path_bytes(root))?;
        }
        for tab in &self.tabs {
            let document = tab.document.read(cx);
            if document.buffer.len_bytes() > MAX_RECOVERY_TAB_BYTES
                || document.saved_content().len() > MAX_RECOVERY_TAB_BYTES
                || tab.title.len() > MAX_RECOVERY_TITLE_BYTES
            {
                return Err(RecoveryError::Limit(format!(
                    "a tab exceeds the {MAX_RECOVERY_TAB_BYTES}-byte recovery limit"
                )));
            }
            let tab_bytes = 2 * 1024usize
                .saturating_add(estimated_json_rope_bytes(document.buffer.text().chunks()))
                .saturating_add(estimated_json_string_bytes(document.saved_content()))
                .saturating_add(estimated_json_string_bytes(&tab.title))
                .saturating_add(
                    document
                        .path
                        .as_deref()
                        .map(estimated_json_path_bytes)
                        .unwrap_or(4),
                );
            add_recovery_estimate(&mut estimated, tab_bytes)?;
            let rich = tab.rich_view.read(cx);
            let draft_sizes = rich.recovery_widget_draft_sizes();
            if rich.has_pending_widget_edit() && draft_sizes.is_none() {
                return Err(RecoveryError::InvalidSnapshot(
                    "the pending rich-field draft has no valid recovery anchor".into(),
                ));
            }
            if let Some((original_bytes, draft_bytes)) = draft_sizes {
                if original_bytes > MAX_RECOVERY_TAB_BYTES || draft_bytes > MAX_RECOVERY_TAB_BYTES {
                    return Err(RecoveryError::Limit(
                        "rich-field text exceeds the per-tab private recovery limit".into(),
                    ));
                }
                // UTF-8 bytes can expand by at most six when JSON escapes a
                // control byte. Bound before cloning any widget source/draft.
                add_recovery_estimate(
                    &mut estimated,
                    original_bytes
                        .saturating_add(draft_bytes)
                        .saturating_mul(6)
                        .saturating_add(1024),
                )?;
            }
        }
        for tab in &self.recovery_archives {
            if tab.content.len() > MAX_RECOVERY_TAB_BYTES
                || tab.saved_content.len() > MAX_RECOVERY_TAB_BYTES
                || tab.title.len() > MAX_RECOVERY_TITLE_BYTES
            {
                return Err(RecoveryError::Limit(format!(
                    "an archived tab exceeds the {MAX_RECOVERY_TAB_BYTES}-byte recovery limit"
                )));
            }
            let tab_bytes = 2 * 1024usize
                .saturating_add(estimated_json_string_bytes(&tab.content))
                .saturating_add(estimated_json_string_bytes(&tab.saved_content))
                .saturating_add(estimated_json_string_bytes(&tab.title))
                .saturating_add(
                    tab.path
                        .as_deref()
                        .map(estimated_json_path_bytes)
                        .unwrap_or(4),
                );
            add_recovery_estimate(&mut estimated, tab_bytes)?;
            if let Some(draft) = &tab.widget_draft {
                add_recovery_estimate(
                    &mut estimated,
                    estimated_json_string_bytes(&draft.original_source)
                        .saturating_add(estimated_json_string_bytes(&draft.draft))
                        .saturating_add(1024),
                )?;
            }
        }
        Ok(())
    }

    fn capture_recovery_tab(&self, tab: &DocumentTab, cx: &gpui::App) -> RecoveryTab {
        let document = tab.document.read(cx);
        let source = tab.editor.read(cx);
        let rich = tab.rich_view.read(cx);
        RecoveryTab {
            path: document.path.clone(),
            title: tab.title.clone(),
            mode: match tab.mode {
                EditorMode::Wysiwyg => RecoveryEditorMode::Wysiwyg,
                EditorMode::Source => RecoveryEditorMode::Source,
                EditorMode::Split => RecoveryEditorMode::Split,
            },
            editing_pane: match tab.editing_pane {
                EditingPane::Source => RecoveryEditingPane::Source,
                EditingPane::Wysiwyg => RecoveryEditingPane::Wysiwyg,
            },
            source_selection: RecoverySelection {
                start: source.selected_range.start,
                end: source.selected_range.end,
                reversed: source.selection_reversed,
            },
            rich_selection: RecoverySelection {
                start: rich.selected_range.start,
                end: rich.selected_range.end,
                reversed: rich.selection_reversed,
            },
            content: document.buffer.content(),
            saved_content: document.saved_content().to_string(),
            dirty: document.dirty,
            autosave_blocked: tab.autosave_blocked,
            widget_draft: rich.recovery_widget_draft().map(Into::into),
        }
    }

    pub fn new_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let doc = cx.new(|_| Document::new(""));
        self.push_tab(doc, None, window, cx);
    }

    pub fn open_document(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if let Some(index) = self.tab_index_for_path(&path, cx) {
            self.remember_active_editing_pane(window, cx);
            self.active_tab = index;
            self.record_recent_file(path.clone());
            self.focus_active_editor(window, cx);
            self.schedule_recovery_snapshot(cx);
            cx.notify();
            return Ok(());
        }
        if self.root.is_none() {
            if let Some(parent) = path.parent() {
                let _ = self.open_workspace(parent.to_path_buf(), cx);
            }
        }
        let document = Document::from_file(path.clone())?;
        let doc = cx.new(|_| document);
        self.push_tab(doc, Some(path), window, cx);
        if let Some(path) = self
            .active_tab()
            .and_then(|tab| tab.document.read(cx).path.clone())
        {
            self.record_recent_file(path);
        }
        self.dismiss_placeholder_untitled(window, cx);
        Ok(())
    }

    /// Open a CLI file or folder: files also set the parent directory as the workspace.
    pub fn open_launch_path(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if path.is_dir() {
            return self.open_workspace(path, cx);
        }
        self.open_document(path, window, cx)
    }

    fn dismiss_placeholder_untitled(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = self.tabs.iter().position(|tab| {
            tab.title == "Untitled"
                && tab.document.read(cx).path.is_none()
                && !tab.document.read(cx).dirty
                && tab.document.read(cx).buffer.content().is_empty()
        });
        if let Some(index) = placeholder {
            if self.tabs.len() > 1 {
                self.close_tab(index, window, cx);
            }
        }
    }

    fn record_recent_file(&mut self, path: PathBuf) {
        if self.recent.files.first() == Some(&path) {
            return;
        }
        self.recent.push_file(path);
        if self.persist_config {
            let _ = self.recent.save();
        }
    }

    fn next_untitled_title(&self) -> String {
        allocate_untitled_title(
            self.tabs
                .iter()
                .map(|tab| tab.title.as_str())
                .chain(self.recovery_archives.iter().map(|tab| tab.title.as_str())),
        )
    }

    fn push_tab(
        &mut self,
        document: Entity<Document>,
        path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remember_active_editing_pane(window, cx);
        let title = path
            .as_ref()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| self.next_untitled_title());
        let theme = self.config.editor_theme();
        let editor = cx.new(|cx| MarkdownEditor::new(document.clone(), theme, window, cx));
        let editor_view = cx.new(|_| MarkdownEditorView::new(editor.clone()));
        let rich_view = cx.new(|cx| {
            let mut view =
                RichEditorView::new(document.clone(), self.config.editor_theme(), window, cx);
            view.set_markup_hints_enabled(self.config.markup_hints_enabled, cx);
            view
        });
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let source_focus = editor.read(cx).focus_handle(cx);
        let rich_focus = rich_view.read(cx).focus_handle(cx);
        let source_subscription =
            cx.on_focus(&source_focus, window, move |workspace, window, cx| {
                if let Some(tab) = workspace.tabs.iter_mut().find(|tab| tab.id == id) {
                    if tab.editor.read(cx).focus_handle(cx).is_focused(window) {
                        tab.editing_pane = EditingPane::Source;
                        cx.notify();
                    }
                }
            });
        let rich_subscription = cx.on_focus(&rich_focus, window, move |workspace, window, cx| {
            if let Some(tab) = workspace.tabs.iter_mut().find(|tab| tab.id == id) {
                if tab.rich_view.read(cx).is_focused(window) {
                    tab.editing_pane = EditingPane::Wysiwyg;
                    cx.notify();
                }
            }
        });
        let source_blur = cx.on_blur(&source_focus, window, |_, _, cx| cx.notify());
        let rich_blur = cx.on_blur(&rich_focus, window, |_, _, cx| cx.notify());
        // Selection changes do not necessarily mutate Document. Observe the
        // real input state, not passive shadow notifications, to avoid a
        // reflection loop or repeatedly scheduling private recovery writes.
        let mut source_selection = (0..0, false);
        let source_selection_subscription = cx.observe(&editor, move |_, editor, cx| {
            let editor = editor.read(cx);
            let selection = (editor.selected_range.clone(), editor.selection_reversed);
            if source_selection != selection {
                source_selection = selection;
                cx.notify();
            }
        });
        let mut rich_selection = (0..0, false, false, false);
        let rich_selection_subscription = cx.observe(&rich_view, move |_, editor, cx| {
            let editor = editor.read(cx);
            let selection = (
                editor.selected_range.clone(),
                editor.selection_reversed,
                editor.has_pending_widget_edit(),
                editor.has_image_editor(),
            );
            if rich_selection != selection {
                rich_selection = selection;
                cx.notify();
            }
        });
        // Editors mutate Document directly for native input events, so this
        // observer is the recovery boundary. Routing only WorkspaceCommand
        // actions would miss ordinary Source and WYSIWYG typing.
        let recovery_subscription = cx.observe(&document, move |workspace, document, cx| {
            let (path, dirty) = {
                let document = document.read(cx);
                (document.path.clone(), document.dirty)
            };
            if let Some(path) = path {
                workspace.record_recent_file(path);
            }
            if dirty {
                // Native text input mutates Document directly, bypassing the
                // WorkspaceCommand route that previously scheduled autosave.
                workspace.schedule_autosave(id, cx);
            }
            workspace.schedule_recovery_snapshot(cx);
        });
        // Overlay drafts are intentionally outside Document. Observe their
        // logical state too, otherwise invalid URLs/YAML would be recoverable
        // on graceful quit only, not by ordinary private autosave.
        let mut previous_widget_draft = None;
        let widget_recovery_subscription = cx.observe(&rich_view, move |workspace, rich, cx| {
            let rich = rich.read(cx);
            let draft_sizes = rich.recovery_widget_draft_sizes();
            if !rich.has_pending_widget_edit() && draft_sizes.is_none() {
                if previous_widget_draft.take().is_some() {
                    workspace.schedule_recovery_snapshot(cx);
                }
                return;
            }
            if !draft_sizes.is_some_and(|(source, draft)| {
                source <= MAX_RECOVERY_TAB_BYTES && draft <= MAX_RECOVERY_TAB_BYTES
            }) {
                workspace.recovery_warning = Some(RecoveryWarning::CapacityReached(
                    "The pending field exceeds private recovery limits or has no valid anchor. Keep it open or copy its text before closing.".into(),
                ));
                cx.notify();
                return;
            }
            let current = rich.recovery_widget_draft();
            if current != previous_widget_draft {
                previous_widget_draft = current;
                workspace.schedule_recovery_snapshot(cx);
            }
        });
        self.tabs.push(DocumentTab {
            id,
            document: document.clone(),
            editor,
            editor_view,
            rich_view,
            mode: EditorMode::default(),
            editing_pane: EditingPane::default(),
            title,
            autosave_blocked: false,
            recovery_saved_base: None,
            recovery_disk_changed: false,
            _focus_subscriptions: vec![
                source_subscription,
                rich_subscription,
                source_blur,
                rich_blur,
                source_selection_subscription,
                rich_selection_subscription,
                recovery_subscription,
                widget_recovery_subscription,
            ],
        });
        self.active_tab = self.tabs.len() - 1;
        self.focus_active_editor(window, cx);
        spawn_parse_pump(document, cx);
        self.schedule_recovery_snapshot(cx);
        cx.notify();
    }

    pub fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.remember_active_editing_pane(window, cx);
        // A valid field can become an ordinary private Markdown edit. Invalid
        // fields and active composition stay raw and are checkpointed as drafts.
        let rich = self.tabs[index].rich_view.clone();
        if !rich.read(cx).has_pending_composition() {
            rich.update(cx, |view, cx| {
                view.commit_pending_widget_edit(cx);
            });
        }
        if self.tabs.len() <= 1 {
            // The UI intentionally keeps one tab alive. Still capture its
            // latest state so an "all tabs closed" attempt cannot shorten the
            // recovery window for an untitled draft.
            let _ = self.checkpoint_before_close_or_quit(cx);
            return;
        }
        if self.persist_config
            && self.recovery_store.is_none()
            && (self.tabs[index].document.read(cx).dirty
                || rich.read(cx).has_pending_widget_edit()
                || rich.read(cx).has_pending_composition())
        {
            self.recovery_warning = Some(RecoveryWarning::WriteFailed(
                "Kept the dirty tab open because session recovery is unavailable. Save or Save As before closing.".into(),
            ));
            cx.notify();
            return;
        }
        let archive = if self.recovery_store.is_some()
            && (self.tabs[index].document.read(cx).dirty
                || rich.read(cx).has_pending_widget_edit()
                || rich.read(cx).has_pending_composition())
        {
            match self.prepare_closed_tab_archive(index, cx) {
                Ok(archive) => Some(archive),
                Err(error) => {
                    self.recovery_warning = Some(match error {
                        RecoveryError::Limit(message) => RecoveryWarning::CapacityReached(format!(
                            "Kept the dirty tab open because session recovery cannot retain it: {message}"
                        )),
                        error => RecoveryWarning::WriteFailed(format!(
                            "Kept the dirty tab open because session recovery could not prepare it: {error}"
                        )),
                    });
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        // Write the projected post-close state while the live tab still owns
        // its bytes. A failed fsync/rename/size check leaves that owner intact.
        let checkpoint = self.capture_recovery_snapshot(cx).and_then(|mut snapshot| {
            snapshot.tabs.remove(index);
            if let Some(archive) = &archive {
                snapshot.archived_tabs.push(archive.clone());
            }
            snapshot.active_tab = if index < self.active_tab {
                self.active_tab - 1
            } else {
                self.active_tab.min(snapshot.tabs.len().saturating_sub(1))
            };
            self.write_recovery_checkpoint(&snapshot)
        });
        if let Err(error) = checkpoint {
            if self.requires_private_recovery(cx) {
                self.recovery_warning = Some(RecoveryWarning::WriteFailed(format!(
                    "Kept the tab open because its private draft checkpoint failed: {error}. Your live edits remain intact."
                )));
                cx.notify();
                return;
            }
            eprintln!("MarkRust clean tab checkpoint unavailable: {error}");
        }
        if let Some(archive) = archive {
            self.recovery_archives.push(archive);
        }
        let removed_tab = self.tabs.remove(index);
        self.clear_pending_external_change_for_tab(removed_tab.id);
        self.reconcile_pending_external_change(cx);
        self.refresh_recovery_disk_warning(cx);
        if index < self.active_tab {
            self.active_tab -= 1;
        } else {
            self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        }
        self.focus_active_editor(window, cx);
        if self.tabs.is_empty() {
            self.new_document(window, cx);
        }
        // The state above is already durable; focus notifications may queue a
        // newer harmless checkpoint but no removed owner is needed for safety.
        cx.notify();
    }

    fn prepare_closed_tab_archive(
        &self,
        index: usize,
        cx: &gpui::App,
    ) -> Result<RecoveryTab, RecoveryError> {
        // Moving one dirty tab from the visible session into the archive keeps
        // the total entry and content budget unchanged. Check that budget
        // before removing the live tab, so close never silently drops a draft.
        self.preflight_recovery_snapshot(cx)?;
        let archive = self.capture_recovery_tab(&self.tabs[index], cx);
        let remaining_tabs = self.tabs.len().saturating_sub(1);
        let projected_entries = remaining_tabs
            .saturating_add(self.recovery_archives.len())
            .saturating_add(1);
        if projected_entries > MAX_RECOVERY_TABS {
            return Err(RecoveryError::Limit(format!(
                "session recovery allows at most {MAX_RECOVERY_TABS} total tab entries"
            )));
        }
        let archive_snapshot = RecoverySnapshot {
            version: crate::recovery::RECOVERY_VERSION,
            root: None,
            active_tab: 0,
            tabs: vec![archive.clone()],
            archived_tabs: Vec::new(),
        };
        archive_snapshot.validate_for_write()?;
        Ok(archive)
    }

    pub fn tab_index_for_path(&self, path: &Path, cx: &gpui::App) -> Option<usize> {
        self.tabs.iter().position(|tab| {
            tab.document
                .read(cx)
                .path
                .as_deref()
                .is_some_and(|p| p == path)
        })
    }

    pub fn save_active(&mut self, cx: &mut Context<Self>) {
        self.save_with_review(NormalizeReviewChoice::KeepOriginal, cx);
    }

    pub(crate) fn pending_external_tab_id(&self) -> Option<usize> {
        self.pending_external_change_tab_id
    }

    /// Ordinary Save is byte-preserving and reconciles fresh disk bytes even
    /// if a watcher notification is delayed or never delivered.
    pub(crate) fn save_document_checked(
        &mut self,
        document: Entity<Document>,
        cx: &mut Context<Self>,
    ) -> std::io::Result<SaveStatus> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.document == document)
            .ok_or_else(|| stale_review_error("The document is no longer open."))?;
        if !self.commit_pending_widget_edit_for_tab(index, cx) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                PENDING_WIDGET_SAVE_ERROR,
            ));
        }
        self.clear_pending_widget_warning();
        let Some(path) = document.read(cx).path.clone() else {
            return Ok(SaveStatus::Untitled);
        };
        let (base, ours) = {
            let doc = document.read(cx);
            (doc.saved_content().to_owned(), doc.buffer.content())
        };
        let disk = match read_disk_for_save(&path, &base) {
            Ok(disk) => disk,
            Err(error) => {
                self.block_external_save(index, path, cx);
                return Err(error);
            }
        };
        let Some(disk) = disk else {
            self.block_external_save(index, path, cx);
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "The file was removed on disk. Use Save As to preserve your buffer.",
            ));
        };
        match markrust_core::three_way_merge(&base, &ours, &disk) {
            markrust_core::MergeOutcome::Conflict => {
                self.block_external_save(index, path, cx);
                return Ok(SaveStatus::NeedsReview);
            }
            markrust_core::MergeOutcome::TakeTheirs => {
                self.apply_reconciled_buffer(index, &disk, &disk, cx);
            }
            markrust_core::MergeOutcome::Merged(merged) => {
                self.apply_reconciled_buffer(index, &merged, &disk, cx);
            }
            markrust_core::MergeOutcome::Unchanged => {
                if ours == disk {
                    self.apply_reconciled_buffer(index, &ours, &disk, cx);
                }
            }
        }
        let result = document.update(cx, |doc, cx| {
            let result = doc.save_checked_and_mark_clean(Some(&disk));
            cx.notify();
            result
        });
        match result {
            Ok(()) => {
                self.finish_document_save(index, path, cx);
                Ok(SaveStatus::Saved)
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                self.block_external_save(index, path, cx);
                Ok(SaveStatus::NeedsReview)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn prepare_normalization_review(
        &mut self,
        document: Entity<Document>,
        cx: &mut Context<Self>,
    ) -> std::io::Result<NormalizationReview> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.document == document)
            .ok_or_else(|| stale_review_error("The document is no longer open."))?;
        if !self.commit_pending_widget_edit_for_tab(index, cx) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                PENDING_WIDGET_SAVE_ERROR,
            ));
        }
        let doc = document.read(cx);
        if doc.buffer.len_bytes() > MAX_RECOVERY_TAB_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "This document is too large for an interactive normalization review.",
            ));
        }
        let mut engine = markrust_core::rich::RichEngine::new();
        let candidates = markrust_core::rich::save_candidates(doc, &mut engine);
        Ok(NormalizationReview {
            document: document.clone(),
            revision: doc.revision(),
            original: candidates.preserved,
            normalized: candidates.normalized,
        })
    }

    pub(crate) fn apply_normalization_review(
        &mut self,
        review: &NormalizationReview,
        cx: &mut Context<Self>,
    ) -> std::io::Result<()> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.document == review.document)
            .ok_or_else(|| stale_review_error("The reviewed document was closed."))?;
        let doc = review.document.read(cx);
        if doc.revision() != review.revision || doc.buffer.content() != review.original {
            return Err(stale_review_error(
                "The document changed while reviewing. Open a fresh review.",
            ));
        }
        if self.tab_has_pending_input(index, cx) {
            return Err(stale_review_error(
                "A field is being edited. Finish it and open a fresh review.",
            ));
        }
        if review.original != review.normalized {
            let base = doc.saved_content().to_owned();
            self.apply_reconciled_buffer(index, &review.normalized, &base, cx);
            // Review applies to the buffer only. Explicit Save is the disk
            // boundary, including when background autosave is configured.
            self.tabs[index].autosave_blocked = true;
        }
        self.schedule_recovery_snapshot(cx);
        cx.notify();
        Ok(())
    }

    pub(crate) fn prepare_external_review(
        &mut self,
        tab_id: usize,
        cx: &mut Context<Self>,
    ) -> std::io::Result<Option<ExternalReview>> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.id == tab_id)
            .ok_or_else(|| stale_review_error("The changed document was closed."))?;
        if !self.commit_pending_widget_edit_for_tab(index, cx) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                PENDING_WIDGET_SAVE_ERROR,
            ));
        }
        let doc = self.tabs[index].document.read(cx);
        if doc.buffer.len_bytes() > MAX_RECOVERY_TAB_BYTES
            || doc.saved_content().len() > MAX_RECOVERY_TAB_BYTES
        {
            return Err(review_capacity_error());
        }
        let path = doc.path.clone().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Use Save As for this draft.",
            )
        })?;
        let theirs = read_review_disk(&path)?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "The file was removed. Your buffer is intact; use Save As.",
            )
        })?;
        let review = ExternalReview {
            tab_id,
            path: path.clone(),
            base: doc.saved_content().to_owned(),
            ours: doc.buffer.content(),
            theirs,
            revision: doc.revision(),
        };
        if review.ours == review.theirs || review.base == review.theirs {
            if review.ours == review.theirs {
                self.apply_reconciled_buffer(index, &review.ours, &review.theirs, cx);
            }
            self.tabs[index].autosave_blocked = false;
            self.tabs[index].recovery_saved_base = None;
            self.tabs[index].recovery_disk_changed = false;
            self.clear_pending_external_change_for_tab(tab_id);
            self.refresh_recovery_disk_warning(cx);
            self.schedule_recovery_snapshot(cx);
            cx.notify();
            return Ok(None);
        }
        self.block_external_save(index, path, cx);
        Ok(Some(review))
    }

    pub(crate) fn resolve_external_review(
        &mut self,
        review: &ExternalReview,
        resolution: ExternalResolution,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> std::io::Result<()> {
        let index = self.validate_external_review(review, cx)?;
        let keep_local_in_file = resolution == ExternalResolution::KeepLocal;
        let mut copy = self.capture_recovery_tab(&self.tabs[index], cx);
        copy.path = None;
        copy.dirty = true;
        copy.title = preserved_version_title(
            if keep_local_in_file { "Disk" } else { "Mine" },
            &self.tabs[index].title,
            self.next_tab_id,
        );
        if keep_local_in_file {
            copy.content = review.theirs.clone();
            copy.source_selection = RecoverySelection::collapsed(0);
            copy.rich_selection = RecoverySelection::collapsed(0);
        }
        copy.saved_content = copy.content.clone();
        self.preserve_version_checkpoint(copy.clone(), cx)?;
        // Recheck after the potentially slow checkpoint, not only before it.
        self.validate_external_review(review, cx)?;
        let selected = if keep_local_in_file {
            &review.ours
        } else {
            &review.theirs
        };
        self.apply_reconciled_buffer(index, selected, &review.theirs, cx);
        self.tabs[index].autosave_blocked = selected != &review.theirs;
        self.tabs[index].recovery_saved_base = None;
        self.tabs[index].recovery_disk_changed = false;
        let copy_document = cx.new(|_| {
            let mut document = Document::new(&copy.saved_content);
            document.dirty = true;
            document
        });
        self.push_tab(copy_document, None, window, cx);
        let copy_index = self.tabs.len() - 1;
        self.apply_recovery_tab_state(
            copy_index,
            &copy,
            RestoredRecoveryState {
                autosave_blocked: true,
                saved_base: None,
                disk_changed: false,
                archived: false,
            },
            cx,
        );
        // The durable archive is now represented by a live pathless tab.
        self.recovery_archives.pop();
        self.active_tab = if resolution == ExternalResolution::KeepBoth {
            copy_index
        } else {
            index
        };
        self.clear_pending_external_change_for_tab(review.tab_id);
        self.refresh_recovery_disk_warning(cx);
        self.focus_active_editor(window, cx);
        self.schedule_recovery_snapshot(cx);
        cx.notify();
        Ok(())
    }

    fn validate_external_review(
        &self,
        review: &ExternalReview,
        cx: &gpui::App,
    ) -> std::io::Result<usize> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.id == review.tab_id)
            .ok_or_else(|| stale_review_error("The reviewed document was closed."))?;
        let doc = self.tabs[index].document.read(cx);
        if doc.path.as_ref() != Some(&review.path)
            || doc.revision() != review.revision
            || doc.buffer.content() != review.ours
            || doc.saved_content() != review.base
            || self.tab_has_pending_input(index, cx)
        {
            return Err(stale_review_error(
                "The document changed. Cancel and open a fresh review.",
            ));
        }
        if read_review_disk(&review.path)?.as_deref() != Some(review.theirs.as_str()) {
            return Err(stale_review_error(
                "The disk file changed again. Cancel and open a fresh review.",
            ));
        }
        Ok(index)
    }

    /// Preserve a losing version durably before modifying its live owner.
    fn preserve_version_checkpoint(
        &mut self,
        copy: RecoveryTab,
        cx: &mut Context<Self>,
    ) -> std::io::Result<()> {
        if self.recovery_store.is_none() && self.persist_config {
            return Err(std::io::Error::other(
                "Recovery is unavailable. Save a separate copy before resolving.",
            ));
        }
        self.recovery_archives.push(copy);
        let result = (|| {
            let snapshot = self
                .capture_recovery_snapshot(cx)
                .map_err(std::io::Error::other)?;
            snapshot
                .validate_for_write()
                .map_err(std::io::Error::other)?;
            self.recovery_generation = self.recovery_generation.wrapping_add(1);
            self._recovery_task = Task::ready(());
            if let Some(store) = self.recovery_store.clone() {
                store.note_generation(self.recovery_generation);
                if store
                    .write_if_current(&snapshot, self.recovery_generation)
                    .map_err(std::io::Error::other)?
                    == RecoveryWriteResult::SkippedStale
                {
                    return Err(stale_review_error(
                        "The recovery checkpoint was superseded. Try again.",
                    ));
                }
            }
            Ok(())
        })();
        if result.is_err() {
            self.recovery_archives.pop();
        }
        result
    }

    fn block_external_save(&mut self, index: usize, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.autosave_blocked = true;
        }
        self.set_pending_external_change(index, path, cx);
        self.schedule_recovery_snapshot(cx);
        cx.notify();
    }

    fn tab_has_pending_input(&self, index: usize, cx: &gpui::App) -> bool {
        self.tabs.get(index).is_some_and(|tab| {
            tab.editor.read(cx).marked_range.is_some()
                || tab.rich_view.read(cx).has_pending_composition()
                || tab.rich_view.read(cx).has_pending_widget_edit()
        })
    }

    fn finish_document_save(&mut self, index: usize, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.autosave_blocked = false;
            tab.recovery_saved_base = None;
            tab.recovery_disk_changed = false;
            let tab_id = tab.id;
            self.clear_pending_external_change_for_tab(tab_id);
        }
        self.refresh_recovery_disk_warning(cx);
        self.record_recent_file(path);
        self.schedule_recovery_snapshot(cx);
        cx.notify();
    }

    fn apply_reconciled_buffer(
        &mut self,
        index: usize,
        content: &str,
        disk: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.document.read(cx).buffer.content() == content {
            tab.document.update(cx, |doc, cx| {
                doc.apply_merged_edit(content, disk, &[]);
                cx.notify();
            });
            return;
        }
        let source = tab.editor.read(cx);
        let rich = tab.rich_view.read(cx);
        let source_selection = SelectionSnapshot {
            start: source.selected_range.start,
            end: source.selected_range.end,
            reversed: source.selection_reversed,
        };
        let rich_selection = SelectionSnapshot {
            start: rich.selected_range.start,
            end: rich.selected_range.end,
            reversed: rich.selection_reversed,
        };
        let source_active = tab.mode.editing_pane(tab.editing_pane) == EditingPane::Source;
        let active_selection = if source_active {
            source_selection
        } else {
            rich_selection
        };
        let offsets = [
            source_selection.start,
            source_selection.end,
            rich_selection.start,
            rich_selection.end,
        ];
        let document = tab.document.clone();
        let mapped = document.update(cx, |doc, cx| {
            let mapped =
                doc.apply_merged_edit_with_selection(content, disk, &offsets, active_selection);
            cx.notify();
            mapped
        });
        self.tabs[index].editor.update(cx, |editor, cx| {
            editor.apply_command(
                EditorCommand::SetSelection {
                    start: mapped[usize::from(source_selection.reversed)],
                    end: mapped[usize::from(!source_selection.reversed)],
                },
                cx,
            );
        });
        self.tabs[index].rich_view.update(cx, |view, cx| {
            view.apply_editor_command(
                EditorCommand::SetSelection {
                    start: mapped[2 + usize::from(rich_selection.reversed)],
                    end: mapped[2 + usize::from(!rich_selection.reversed)],
                },
                cx,
            );
        });
        spawn_parse_pump(document, cx);
    }

    /// Save a specific tab selected by a native file dialog. Keeping this
    /// transition in Workspace means Save As cannot bypass recovery, recent
    /// files, or a restored-tab autosave block.
    pub fn save_document_as(
        &mut self,
        document: Entity<Document>,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> std::io::Result<()> {
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.document == document)
            .ok_or_else(|| {
                stale_review_error("The document was closed while the file dialog was open.")
            })?;
        if document.read(cx).path.as_ref() == Some(&path) {
            return match self.save_document_checked(document, cx)? {
                SaveStatus::Saved => Ok(()),
                SaveStatus::NeedsReview => Err(stale_review_error(
                    "Review concurrent changes before saving this file.",
                )),
                SaveStatus::Untitled => Err(stale_review_error("The document path changed.")),
            };
        }
        if self.tabs.iter().any(|tab| {
            tab.document != document && tab.document.read(cx).path.as_ref() == Some(&path)
        }) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "This file is open in another tab. Switch to that tab or choose another path.",
            ));
        }
        {
            if !self.commit_pending_widget_edit_for_tab(tab_index, cx) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    PENDING_WIDGET_SAVE_ERROR,
                ));
            }
            self.clear_pending_widget_warning();
        }
        let expected = read_review_disk(&path)?;
        if let Some(disk) = expected
            .as_ref()
            .filter(|disk| **disk != document.read(cx).buffer.content())
        {
            let mut copy = self.capture_recovery_tab(&self.tabs[tab_index], cx);
            copy.path = None;
            copy.title = preserved_version_title(
                "Disk",
                &path.file_name().unwrap_or_default().to_string_lossy(),
                self.next_tab_id,
            );
            copy.content = disk.clone();
            copy.saved_content = disk.clone();
            copy.dirty = true;
            copy.source_selection = RecoverySelection::collapsed(0);
            copy.rich_selection = RecoverySelection::collapsed(0);
            self.preserve_version_checkpoint(copy, cx)?;
        }
        document.update(cx, |doc, cx| -> std::io::Result<()> {
            doc.save_as_checked(path.clone(), expected.as_deref())?;
            cx.notify();
            Ok(())
        })?;
        let saved_tab_id =
            if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.document == document) {
                tab.title = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Untitled".into());
                tab.autosave_blocked = false;
                tab.recovery_saved_base = None;
                tab.recovery_disk_changed = false;
                Some(tab.id)
            } else {
                None
            };
        if let Some(tab_id) = saved_tab_id {
            self.clear_pending_external_change_for_tab(tab_id);
        }
        self.refresh_recovery_disk_warning(cx);
        self.record_recent_file(path);
        self.schedule_recovery_snapshot(cx);
        cx.notify();
        Ok(())
    }

    pub fn save_with_review(&mut self, choice: NormalizeReviewChoice, cx: &mut Context<Self>) {
        let _ = self.save_with_review_checked(choice, cx);
    }

    /// Save the active document while preserving an I/O error for the window
    /// layer to present. Cancellation and an untitled document are deliberate
    /// no-ops, not errors. Recovery conflict state is cleared only after the
    /// document write succeeds.
    pub fn save_with_review_checked(
        &mut self,
        choice: NormalizeReviewChoice,
        cx: &mut Context<Self>,
    ) -> std::io::Result<()> {
        if choice == NormalizeReviewChoice::Cancel {
            return Ok(());
        }
        let Some(document) = self.active_tab().map(|tab| tab.document.clone()) else {
            return Ok(());
        };
        if choice == NormalizeReviewChoice::Normalize {
            let review = self.prepare_normalization_review(document.clone(), cx)?;
            self.apply_normalization_review(&review, cx)?;
        }
        match self.save_document_checked(document, cx)? {
            SaveStatus::Saved | SaveStatus::Untitled => Ok(()),
            SaveStatus::NeedsReview => Err(stale_review_error(
                "Review concurrent disk changes before saving.",
            )),
        }
    }

    pub fn normalize_candidates(
        &self,
        cx: &gpui::App,
    ) -> Option<markrust_core::rich::SaveCandidates> {
        let tab = self.active_tab()?;
        let doc = tab.document.read(cx);
        let mut engine = markrust_core::rich::RichEngine::new();
        Some(markrust_core::rich::save_candidates(doc, &mut engine))
    }

    #[allow(dead_code)]
    pub fn on_document_edited(&mut self, tab_id: usize, cx: &mut Context<Self>) {
        self.schedule_autosave(tab_id, cx);
    }

    pub fn schedule_autosave(&mut self, tab_id: usize, cx: &mut Context<Self>) {
        // Autosave is deliberately private. Publishing to a user's path is
        // authorized only by explicit Save/Save As and their checked-write gate.
        if self.tabs.iter().any(|tab| tab.id == tab_id) {
            self.schedule_recovery_snapshot(cx);
        }
    }

    pub fn open_workspace(&mut self, root: PathBuf, cx: &mut Context<Self>) -> anyhow::Result<()> {
        self.root = Some(root.clone());
        self.recent.push(root);
        if self.persist_config {
            let _ = self.recent.save();
        }
        self.start_watcher(cx);
        self.schedule_file_list_scan(cx);
        self.schedule_recovery_snapshot(cx);
        cx.notify();
        Ok(())
    }

    fn start_watcher(&mut self, cx: &mut Context<Self>) {
        // Headless GUI fixtures deliberately do not install a native watcher:
        // their deterministic scheduler cannot service an unbounded blocking
        // channel receive. Production workspaces keep the normal watcher.
        if !self.persist_config {
            return;
        }
        let Some(root) = self.root.clone() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .ok();
        if let Some(watcher) = watcher.as_mut() {
            let _ = watcher.watch(&root, RecursiveMode::Recursive);
        }
        self._watcher = watcher;

        // `cx.spawn` runs on GPUI's foreground executor (the UI thread).
        // Blocking `recv` there freezes the window for as long as the disk is
        // quiet — which is exactly "open a markdown file and the app hangs",
        // because open also starts a watcher on the parent folder.
        let rx = Arc::new(Mutex::new(rx));
        let workspace = cx.entity();
        self._watcher_task = cx.spawn(async move |_, cx| loop {
            let rx = rx.clone();
            let received = cx
                .background_executor()
                .spawn(async move { recv_notify_blocking(&rx) })
                .await;
            match received {
                Ok(Ok(event)) => {
                    workspace.update(cx, |workspace, cx| {
                        workspace.handle_fs_event(event, cx);
                    });
                }
                Ok(Err(_notify_error)) => continue,
                Err(_disconnected) => break,
            }
        });
    }

    fn handle_fs_event(&mut self, event: Event, cx: &mut Context<Self>) {
        let mut refresh_sidebar = false;
        for path in event.paths {
            if is_markdown(&path) || path.is_dir() {
                refresh_sidebar = true;
            }
            if let Some(index) = self.tab_index_for_path(&path, cx) {
                let workspace = cx.entity();
                let watched = path.clone();
                let tab_id = self.tabs[index].id;
                let watched_base = self.tabs[index]
                    .document
                    .read(cx)
                    .saved_content()
                    .to_owned();
                cx.spawn(async move |_, cx| {
                    let read_path = watched.clone();
                    let result = cx
                        .background_executor()
                        .spawn(async move { read_disk_for_save(&read_path, &watched_base) })
                        .await;
                    workspace.update(cx, |workspace, cx| {
                        let Some(index) = workspace.tabs.iter().position(|tab| {
                            tab.id == tab_id
                                && tab.document.read(cx).path.as_ref() == Some(&watched)
                        }) else {
                            return;
                        };
                        match result {
                            Ok(Some(theirs)) => {
                                workspace.apply_external_bytes(index, watched, theirs, cx)
                            }
                            Ok(None) | Err(_) => workspace.block_external_save(index, watched, cx),
                        }
                    });
                })
                .detach();
            }
        }
        if refresh_sidebar {
            self.schedule_file_list_scan(cx);
        }
    }

    fn schedule_file_list_scan(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            self.cached_files.clear();
            return;
        };
        let workspace = cx.entity();
        self._file_list_task = cx.spawn(async move |_, cx| {
            let scanned = root.clone();
            let files = cx
                .background_executor()
                .spawn(async move { list_markdown_files(&scanned) })
                .await;
            workspace.update(cx, |workspace, cx| {
                if workspace.root.as_ref() == Some(&root) {
                    workspace.cached_files = files;
                    cx.notify();
                }
            });
        });
    }

    pub fn reload_tab(&mut self, index: usize, cx: &mut Context<Self>) -> anyhow::Result<()> {
        // The banner carries an index captured at render time. Reject it if a
        // close/save transition has made that index refer to another tab.
        if self.pending_external_change.is_some()
            && !self.pending_external_change_matches_tab(index, cx)
        {
            self.reconcile_pending_external_change(cx);
            cx.notify();
            return Ok(());
        }
        let Some(tab) = self.tabs.get(index) else {
            self.reconcile_pending_external_change(cx);
            cx.notify();
            return Ok(());
        };
        let tab_id = tab.id;
        let path = tab
            .document
            .read(cx)
            .path
            .clone()
            .ok_or_else(|| anyhow::anyhow!("tab has no path"))?;
        let content = read_review_disk(&path)?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "File was removed. Use Save As to preserve your buffer.",
            )
        })?;
        if self.tab_has_pending_input(index, cx) {
            self.block_external_save(index, path, cx);
            return Err(stale_review_error(
                "Finish the current field or composition before reviewing disk changes.",
            )
            .into());
        }
        if tab.document.read(cx).dirty {
            self.apply_external_bytes(index, path, content, cx);
            if self.pending_external_change_tab_id == Some(tab_id) {
                return Err(stale_review_error("Concurrent edits conflict. Review both versions; reload will not discard your edits.").into());
            }
            Ok(())
        } else {
            self.apply_reconciled_buffer(index, &content, &content, cx);
            self.finish_document_save(index, path, cx);
            Ok(())
        }
    }

    fn apply_external_bytes(
        &mut self,
        index: usize,
        path: PathBuf,
        theirs: String,
        cx: &mut Context<Self>,
    ) {
        // An index alone cannot identify a document after tabs are closed.
        if self
            .tabs
            .get(index)
            .is_none_or(|tab| tab.document.read(cx).path.as_deref() != Some(path.as_path()))
        {
            return;
        }
        // An earlier asynchronous read may finish after a later watcher read.
        // Verify a fresh disk snapshot before modifying the current buffer.
        let base = self.tabs[index]
            .document
            .read(cx)
            .saved_content()
            .to_owned();
        let theirs = match read_disk_for_save(&path, &base) {
            Ok(Some(fresh)) => {
                if fresh == theirs {
                    theirs
                } else {
                    fresh
                }
            }
            Ok(None) | Err(_) => {
                self.block_external_save(index, path, cx);
                return;
            }
        };
        // Metadata/duplicate watcher events must not interrupt an unfinished
        // native field or clear a separate explicit-save boundary.
        if self.tabs[index].document.read(cx).saved_content() == theirs {
            return;
        }
        if self.tab_has_pending_input(index, cx) {
            self.block_external_save(index, path, cx);
            return;
        }
        let action = {
            let Some(tab) = self.tabs.get(index) else {
                return;
            };
            let doc = tab.document.read(cx);
            if doc.path.as_deref() != Some(path.as_path()) {
                return;
            }
            classify_external_change(
                doc.path.as_deref(),
                &path,
                doc.saved_content(),
                &doc.buffer.content(),
                &theirs,
            )
        };
        match action {
            ExternalChangeAction::Ignore => {
                let Some(tab) = self.tabs.get(index) else {
                    return;
                };
                if tab.document.read(cx).buffer.content() == theirs {
                    self.apply_reconciled_buffer(index, &theirs, &theirs, cx);
                    self.finish_document_save(index, path, cx);
                }
            }
            ExternalChangeAction::PromptReload => {
                // There are no local changes to lose. Accept the fresh bytes
                // with an undo boundary rather than demanding a reload.
                self.apply_reconciled_buffer(index, &theirs, &theirs, cx);
                self.finish_document_save(index, path, cx);
            }
            ExternalChangeAction::PromptConflict => {
                self.block_external_save(index, path, cx);
            }
            ExternalChangeAction::Apply(merged) => {
                let tab_id = self.tabs[index].id;
                self.apply_reconciled_buffer(index, &merged, &theirs, cx);
                if let Some(tab) = self.tabs.get_mut(index) {
                    // A merged buffer that differs from current disk is a
                    // user-visible conflict result, not permission to write
                    // in the background. Explicit Save remains available.
                    tab.autosave_blocked = merged != theirs;
                    if !tab.autosave_blocked {
                        tab.recovery_saved_base = None;
                        tab.recovery_disk_changed = false;
                    }
                }
                self.clear_pending_external_change_for_tab(tab_id);
                self.refresh_recovery_disk_warning(cx);
                self.schedule_recovery_snapshot(cx);
                cx.notify();
            }
        }
    }

    pub fn list_files(&self) -> Vec<PathBuf> {
        self.cached_files.clone()
    }

    pub fn toggle_theme(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.config.toggle_theme();
        if self.persist_config {
            let _ = self.config.save();
        }
        let theme = self.config.editor_theme();
        for tab in &self.tabs {
            tab.editor.update(cx, |editor, cx| {
                editor.theme = theme.clone();
                cx.notify();
            });
            tab.rich_view.update(cx, |view, cx| {
                view.set_theme(theme.clone(), cx);
            });
        }
        cx.notify();
    }

    /// Change syntax highlighting without changing the user's typography or
    /// color-scheme preference. GUI-test workspaces never persist this choice.
    pub fn set_highlight_style(&mut self, style: HighlightStyle, cx: &mut Context<Self>) {
        if self.config.highlight_style == style {
            return;
        }
        self.config.highlight_style = style;
        if self.persist_config {
            let _ = self.config.save();
        }
        let theme = self.config.editor_theme();
        for tab in &self.tabs {
            tab.editor.update(cx, |editor, cx| {
                editor.theme = theme.clone();
                cx.notify();
            });
            tab.rich_view.update(cx, |view, cx| {
                view.set_theme(theme.clone(), cx);
            });
        }
        cx.notify();
    }

    /// Update chrome language without rewriting content, selection, or history.
    pub fn set_ui_language(&mut self, language: crate::i18n::Language, cx: &mut Context<Self>) {
        if self.config.language == language {
            return;
        }
        self.config.language = language;
        if self.persist_config {
            let _ = self.config.save();
        }
        let strings = crate::i18n::catalog(language);
        for tab in &self.tabs {
            tab.editor.update(cx, |editor, cx| {
                editor.theme.ui_strings = strings.clone();
                cx.notify();
            });
            tab.rich_view
                .update(cx, |view, cx| view.set_ui_strings(strings.clone(), cx));
        }
        cx.notify();
    }

    pub fn toggle_markup_hints(&mut self, cx: &mut Context<Self>) {
        self.config.markup_hints_enabled = !self.config.markup_hints_enabled;
        if self.persist_config {
            let _ = self.config.save();
        }
        for tab in &self.tabs {
            tab.rich_view.update(cx, |view, cx| {
                view.set_markup_hints_enabled(self.config.markup_hints_enabled, cx);
            });
        }
        cx.notify();
    }

    /// Load remote images only after the reader explicitly requests them for
    /// the current document tab. This avoids document-controlled tracking
    /// requests during file open.
    pub fn load_remote_images(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_tab() {
            tab.rich_view
                .update(cx, |view, cx| view.load_remote_images(cx));
        }
        cx.notify();
    }

    pub fn handle_window_drop(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_drop_paths(paths.paths().to_vec(), DropTarget::Window, window, cx);
    }

    pub fn handle_editor_drop(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_drop_paths(paths.paths().to_vec(), DropTarget::Editor, window, cx);
    }

    pub fn handle_drop_paths(
        &mut self,
        paths: Vec<PathBuf>,
        target: DropTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let intent = match target {
            DropTarget::Editor => classify_editor_drop(&paths),
            DropTarget::Window => classify_window_drop(&paths),
        };
        match intent {
            DropIntent::OpenWorkspace(root) => {
                let _ = self.open_workspace(root, cx);
            }
            DropIntent::OpenDocuments(docs) => {
                for path in docs {
                    let _ = self.open_document(path, window, cx);
                }
            }
            DropIntent::InsertImages(images) => {
                self.insert_images(images, window, cx);
            }
            DropIntent::Ignored => {}
        }
    }

    fn insert_images(&mut self, images: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let doc_path = tab.document.read(cx).path.clone();
        let editor = tab.editor.clone();
        let mut snippets = Vec::new();
        for image in images {
            if let Some(reference) = markdown_image_reference(&image, doc_path.as_deref()) {
                snippets.push(reference);
            }
        }
        if snippets.is_empty() {
            return;
        }
        let text = snippets.join("\n\n");
        let prefix = if doc_path.is_some() { "\n\n" } else { "" };
        editor.update(cx, |editor, cx| {
            editor.insert_text(&format!("{prefix}{text}"), window, cx);
        });
        cx.notify();
    }

    pub fn export_active_html(&self, cx: &gpui::App) -> anyhow::Result<PathBuf> {
        let tab = self
            .active_tab()
            .ok_or_else(|| anyhow::anyhow!("no active document"))?;
        let doc = tab.document.read(cx);
        let path = doc
            .path
            .clone()
            .ok_or_else(|| anyhow::anyhow!("save the document before exporting"))?;
        Ok(markrust_core::export_content_to_html(
            &doc.buffer.content(),
            Some(&path),
            None,
        )?)
    }
}

fn stale_review_error(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::WouldBlock, message)
}

fn read_review_disk(path: &Path) -> std::io::Result<Option<String>> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take((MAX_RECOVERY_TAB_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RECOVERY_TAB_BYTES {
        return Err(review_capacity_error());
    }
    String::from_utf8(bytes).map(Some).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "The disk file is not valid UTF-8. Your buffer is intact; use Save As to a new file.",
        )
    })
}

fn review_capacity_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "Concurrent changes exceed the 4 MiB review limit. Your buffer is intact; use Save As to a new file.",
    )
}

/// Large unchanged files remain saveable without reading a hostile replacement
/// into an unbounded allocation. Changed files use the interactive review cap.
fn read_disk_for_save(path: &Path, base: &str) -> std::io::Result<Option<String>> {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() == base.len() as u64 {
        let mut chunk = [0u8; 8192];
        let mut cursor = 0;
        loop {
            let count = file.read(&mut chunk)?;
            if count == 0 {
                if cursor == base.len() {
                    return Ok(Some(base.to_owned()));
                }
                break;
            }
            if base.as_bytes().get(cursor..cursor + count) != Some(&chunk[..count]) {
                break;
            }
            cursor += count;
        }
    }
    read_review_disk(path)
}

fn preserved_version_title(side: &str, title: &str, sequence: usize) -> String {
    let prefix = format!("{side} copy — ");
    let suffix = format!(" ({sequence})");
    let mut end = title
        .len()
        .min(MAX_RECOVERY_TITLE_BYTES.saturating_sub(prefix.len() + suffix.len()));
    while !title.is_char_boundary(end) {
        end -= 1;
    }
    format!("{prefix}{}{suffix}", &title[..end])
}

fn add_recovery_estimate(total: &mut usize, bytes: usize) -> Result<(), RecoveryError> {
    *total = total.checked_add(bytes).ok_or_else(|| {
        RecoveryError::Limit("session recovery size calculation overflowed".into())
    })?;
    if *total > MAX_RECOVERY_SNAPSHOT_BYTES {
        return Err(RecoveryError::Limit(format!(
            "session recovery exceeds the {MAX_RECOVERY_SNAPSHOT_BYTES}-byte limit"
        )));
    }
    Ok(())
}

fn estimated_json_path_bytes(path: &Path) -> usize {
    estimated_json_string_bytes(path.to_string_lossy().as_ref())
}

fn estimated_json_rope_bytes<'a>(chunks: impl IntoIterator<Item = &'a str>) -> usize {
    chunks.into_iter().fold(2usize, |total, chunk| {
        total.saturating_add(estimated_json_escaped_bytes(chunk))
    })
}

fn estimated_json_string_bytes(text: &str) -> usize {
    2usize.saturating_add(estimated_json_escaped_bytes(text))
}

fn estimated_json_escaped_bytes(text: &str) -> usize {
    text.bytes().fold(0usize, |total, byte| {
        let encoded = match byte {
            b'"' | b'\\' => 2,
            b'\x08' | b'\x09' | b'\x0a' | b'\x0c' | b'\x0d' => 2,
            b'\x00'..=b'\x1f' => 6,
            _ => 1,
        };
        total.saturating_add(encoded)
    })
}

/// Allocate a human-readable untitled label without renaming existing tabs.
/// Recovery archives participate in the caller's input so a dirty closed
/// draft cannot be confused with a new document after a later restore.
fn allocate_untitled_title<'a>(used_titles: impl IntoIterator<Item = &'a str>) -> String {
    let used_titles = used_titles.into_iter().collect::<Vec<_>>();
    if !used_titles.contains(&"Untitled") {
        return "Untitled".into();
    }
    for suffix in 2.. {
        let candidate = format!("Untitled {suffix}");
        if !used_titles.contains(&candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!("the unbounded untitled suffix loop always finds a free title")
}

/// Do not trust only the dirty flag at an exit boundary: independently verify
/// that the complete buffer matches its last saved base without cloning it.
pub(crate) fn tab_requires_private_recovery(document: &Document, pending_input: bool) -> bool {
    if document.dirty || pending_input {
        return true;
    }
    let saved = document.saved_content();
    if document.buffer.len_bytes() != saved.len() {
        return true;
    }
    let mut offset = 0usize;
    for chunk in document.buffer.text().chunks() {
        let end = offset + chunk.len();
        if saved.get(offset..end) != Some(chunk) {
            return true;
        }
        offset = end;
    }
    false
}

fn recovered_archive_title(title: &str) -> String {
    const PREFIX: &str = "Recovered — ";
    let available = MAX_RECOVERY_TITLE_BYTES.saturating_sub(PREFIX.len());
    let mut end = title.len().min(available);
    while end > 0 && !title.is_char_boundary(end) {
        end -= 1;
    }
    format!("{PREFIX}{}", &title[..end])
}

fn recovery_disk_warning_summary(path: Option<&Path>, title: &str) -> String {
    match path {
        Some(path) => {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| title.to_owned());
            format!("External changes — review {name} before Save.")
        }
        None => format!("Save As required — recovered draft: {title}."),
    }
}

fn recovery_disk_warning_details(path: Option<&Path>, title: &str) -> String {
    match path {
        Some(path) => format!(
            "Review External Changes before explicitly saving. Private drafts retain both the recovered edits and their original disk base.\nFile: {}",
            path.display()
        ),
        None => format!(
            "Save As required — recovered unsaved changes in {title} cannot be written to disk until you choose a file."
        ),
    }
}

/// Blocking notify receive. Must run on a background worker, never on GPUI's
/// foreground executor — that is the hang that made opening a file freeze the UI.
fn recv_notify_blocking(
    rx: &Mutex<mpsc::Receiver<notify::Result<Event>>>,
) -> Result<notify::Result<Event>, mpsc::RecvError> {
    rx.lock().unwrap_or_else(|e| e.into_inner()).recv()
}

/// Drain background parse results without blocking a GPUI frame.
fn spawn_parse_pump(document: Entity<Document>, cx: &mut Context<Workspace>) {
    cx.spawn(async move |_, cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(32))
                .await;
            let done = document.update(cx, |doc, cx| {
                // A render or input handler may already have drained this revision.
                // Stop then too, rather than retaining the tab's document forever.
                if !doc.mode.parses_markdown() || doc.parsed_revision >= doc.revision() {
                    cx.notify();
                    return true;
                }
                let updated = doc.apply_pending_parse();
                let done = updated
                    .as_ref()
                    .is_some_and(|update| update.revision >= doc.revision());
                if updated.is_some() || done {
                    cx.notify();
                }
                done
            });
            if done {
                break;
            }
        }
    })
    .detach();
}

pub fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut needle_chars = needle.chars();
    let mut current = needle_chars.next();
    for ch in haystack.chars() {
        if current == Some(ch.to_ascii_lowercase()) || current == Some(ch) {
            current = needle_chars.next();
            if current.is_none() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_saved_documents_do_not_require_private_recovery_to_exit() {
        let document = Document::new("saved é 👩‍🚀\r\n");
        assert!(!tab_requires_private_recovery(&document, false));
        assert!(!tab_requires_private_recovery(&Document::new(""), false));
    }

    #[test]
    fn unsaved_bytes_or_pending_input_always_require_private_recovery() {
        let mut document = Document::new("base");
        assert!(tab_requires_private_recovery(&document, true));
        document.replace_range(0, 4, "ours");
        assert!(tab_requires_private_recovery(&document, false));
        // A stale dirty bit must not turn changed bytes into a clean exit.
        document.dirty = false;
        assert!(tab_requires_private_recovery(&document, false));
        let mut returned_to_base = Document::new("base");
        returned_to_base.dirty = true;
        assert!(tab_requires_private_recovery(&returned_to_base, false));
    }

    struct SaveReadTestDirectory(PathBuf);

    impl SaveReadTestDirectory {
        fn new(label: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "markrust-save-read-{label}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for SaveReadTestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn save_disk_reader_accepts_large_unchanged_bytes_without_interactive_review() {
        let directory = SaveReadTestDirectory::new("unchanged");
        let path = directory.0.join("large.md");
        let base = format!("{}\r\n", "é".repeat(MAX_RECOVERY_TAB_BYTES / 2 + 1));
        assert!(base.len() > MAX_RECOVERY_TAB_BYTES);
        std::fs::write(&path, &base).unwrap();

        assert_eq!(
            read_disk_for_save(&path, &base).unwrap().as_deref(),
            Some(base.as_str())
        );
        assert!(
            read_review_disk(&path).is_err(),
            "interactive review remains bounded"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), base);
        assert_eq!(
            read_disk_for_save(&directory.0.join("missing.md"), &base).unwrap(),
            None
        );
    }

    #[test]
    fn save_disk_reader_rejects_oversized_changed_files_without_touching_them() {
        let directory = SaveReadTestDirectory::new("changed");
        let path = directory.0.join("concurrent.md");
        let base = "x".repeat(MAX_RECOVERY_TAB_BYTES + 1);
        for offset in [0, base.len() - 1, base.len()] {
            let mut replacement = base.clone();
            if offset == base.len() {
                replacement.push('Y');
            } else {
                replacement.replace_range(offset..offset + 1, "Y");
            }
            std::fs::write(&path, &replacement).unwrap();
            assert!(
                read_disk_for_save(&path, &base).is_err(),
                "changed large disk must not masquerade as base"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), replacement);
        }
        std::fs::write(&path, "small fresh disk\r\n").unwrap();
        assert_eq!(
            read_disk_for_save(&path, &base).unwrap().as_deref(),
            Some("small fresh disk\r\n")
        );
    }

    #[test]
    fn recovery_disk_warning_keeps_action_before_long_path_and_full_details() {
        let path = PathBuf::from(format!(
            "/Users/writer/{}/notes.md",
            "very-long-project-directory/".repeat(40)
        ));
        let summary = recovery_disk_warning_summary(Some(&path), "notes.md");
        assert_eq!(summary, "External changes — review notes.md before Save.");
        assert!(!summary.contains("/Users/"));
        assert!(summary.len() < 80);

        let details = recovery_disk_warning_details(Some(&path), "notes.md");
        assert!(details.starts_with("Review External Changes before explicitly saving"));
        assert!(details.contains("both the recovered edits and their original disk base"));
        assert!(details.contains(&path.display().to_string()));
    }

    #[test]
    fn recovered_pathless_draft_warning_starts_with_save_as_action() {
        let title = "Recovered — notes.md";
        assert_eq!(
            recovery_disk_warning_summary(None, title),
            "Save As required — recovered draft: Recovered — notes.md."
        );
        assert!(recovery_disk_warning_details(None, title).starts_with("Save As required"));
        assert!(recovery_disk_warning_details(None, title).contains(title));
    }

    #[test]
    fn default_editor_mode_is_wysiwyg_not_split() {
        assert_eq!(EditorMode::default(), EditorMode::Wysiwyg);
        assert_ne!(EditorMode::default(), EditorMode::Split);
        assert_ne!(EditorMode::default(), EditorMode::Source);
        let label = match EditorMode::default() {
            EditorMode::Wysiwyg => "Wysiwyg",
            EditorMode::Split => "Split",
            EditorMode::Source => "Source",
        };
        assert_eq!(label, "Wysiwyg");
        assert_ne!(label, "Rich");
        assert_ne!(label, "Split");
    }

    #[test]
    fn split_restores_the_last_focused_pane() {
        for pane in [EditingPane::Source, EditingPane::Wysiwyg] {
            assert_eq!(EditorMode::Split.editing_pane(pane), pane);
        }
    }

    #[test]
    fn single_surface_modes_only_focus_the_visible_editor() {
        for pane in [EditingPane::Source, EditingPane::Wysiwyg] {
            assert_eq!(EditorMode::Source.editing_pane(pane), EditingPane::Source);
            assert_eq!(EditorMode::Wysiwyg.editing_pane(pane), EditingPane::Wysiwyg);
        }
    }

    #[test]
    fn fuzzy_match_finds_subsequence() {
        assert!(fuzzy_match("README.md", "readme"));
        assert!(!fuzzy_match("README.md", "xyz"));
    }

    #[test]
    fn untitled_title_allocation_is_stable_and_reserves_archived_drafts() {
        assert_eq!(allocate_untitled_title([]), "Untitled");
        assert_eq!(allocate_untitled_title(["Untitled"]), "Untitled 2");
        assert_eq!(
            allocate_untitled_title(["Untitled", "Untitled 2", "Untitled 4"]),
            "Untitled 3"
        );

        let visible = ["Untitled"];
        let archived = ["Untitled 2"];
        assert_eq!(
            allocate_untitled_title(visible.iter().copied().chain(archived.iter().copied())),
            "Untitled 3"
        );
    }

    #[test]
    fn skips_build_and_hidden_directories() {
        assert!(crate::session::should_skip_dir(Path::new("node_modules")));
        assert!(crate::session::should_skip_dir(Path::new(".git")));
        assert!(crate::session::should_skip_dir(Path::new("target")));
        assert!(!crate::session::should_skip_dir(Path::new("docs")));
    }

    #[test]
    fn idle_notify_channel_try_recv_does_not_block() {
        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        drop(tx);
    }

    #[test]
    fn notify_disconnect_unblocks_background_recv() {
        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
        let rx = Mutex::new(rx);
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = recv_notify_blocking(&rx);
            let _ = done_tx.send(result.is_err());
        });
        drop(tx);
        let disconnected = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("watcher recv deadlocked after sender drop");
        assert!(disconnected);
    }
}
