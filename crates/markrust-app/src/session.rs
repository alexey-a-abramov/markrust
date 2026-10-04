// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::{Path, PathBuf};

use markrust_core::rich::SaveCandidates;
use markrust_core::Document;
use markrust_editor::{EditorCommand, EditorOutcome, HeadlessEditor};
use thiserror::Error;

use crate::config::{is_markdown, ThemeChoice};
use crate::drop::{
    classify_editor_drop, classify_window_drop, markdown_image_reference, DropIntent,
};
use crate::recovery::{
    RecoveryEditingPane, RecoveryEditorMode, RecoveryError, RecoverySelection, RecoverySnapshot,
    RecoveryStore, RecoveryTab, RECOVERY_VERSION,
};

/// Where a file drop landed in the chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropTarget {
    Window,
    Editor,
}

/// Workspace-level commands. The GPUI window maps keys/clicks onto this enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceCommand {
    Editor(EditorCommand),
    Save,
    SaveAs(PathBuf),
    OpenFile(PathBuf),
    OpenFolder(PathBuf),
    OpenLaunchPath(PathBuf),
    ExportHtml {
        output: Option<PathBuf>,
    },
    DropFiles {
        paths: Vec<PathBuf>,
        target: DropTarget,
    },
    ToggleTheme,
    NewDocument,
    CloseTab,
    SwitchTab(usize),
    JumpToHeading {
        offset: usize,
    },
    /// Reveal YAML frontmatter in source (hook for the frontmatter panel).
    EditFrontmatter,
    /// Set a top-level YAML frontmatter key (empty value removes it).
    SetFrontmatterField {
        key: String,
        value: String,
    },
    /// Save with an explicit Keep / Normalize / Cancel choice.
    SaveWithReview(NormalizeReviewChoice),
    /// Advance the fake clock so autosave debounce can fire in tests.
    AdvanceTime {
        millis: u64,
    },
    ExternalFileChange(PathBuf),
    ReloadTab(usize),
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("no active document")]
    NoActiveDocument,
    #[error("untitled document has no path; save as required")]
    UntitledHasNoPath,
    #[error("invalid editor range")]
    InvalidRange,
    #[error("tab not found")]
    TabNotFound,
    #[error("document changed on disk; local edits were preserved for review")]
    ExternalChange,
    #[error("cannot reload a tab with unsaved edits; resolve or save a copy first")]
    DirtyReloadBlocked,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Recovery(#[from] RecoveryError),
}

/// How to react to an on-disk change for an open tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalChangeAction {
    Ignore,
    /// Clean tab; ask before replacing the buffer with disk bytes.
    PromptReload,
    /// Dirty tab; disjoint edits were combined. Apply `merged` and map carets.
    Apply(String),
    /// Dirty tab; both sides changed the same lines.
    PromptConflict,
}

/// Classify a watcher event using the last known disk snapshot, in-memory
/// buffer, and current disk bytes.
pub fn classify_external_change(
    tab_path: Option<&Path>,
    changed_path: &Path,
    saved: &str,
    ours: &str,
    theirs: &str,
) -> ExternalChangeAction {
    match tab_path {
        Some(path) if path == changed_path => {}
        _ => return ExternalChangeAction::Ignore,
    }
    match markrust_core::three_way_merge(saved, ours, theirs) {
        markrust_core::MergeOutcome::Unchanged => ExternalChangeAction::Ignore,
        markrust_core::MergeOutcome::TakeTheirs => ExternalChangeAction::PromptReload,
        markrust_core::MergeOutcome::Merged(merged) => ExternalChangeAction::Apply(merged),
        markrust_core::MergeOutcome::Conflict => ExternalChangeAction::PromptConflict,
    }
}

/// Path-only helper kept for tests that do not have buffer contents.
pub fn reload_decision(
    dirty: bool,
    tab_path: Option<&Path>,
    changed_path: &Path,
) -> ExternalChangeAction {
    match tab_path {
        Some(path) if path == changed_path => {
            if dirty {
                ExternalChangeAction::PromptConflict
            } else {
                ExternalChangeAction::PromptReload
            }
        }
        _ => ExternalChangeAction::Ignore,
    }
}

/// Choice for the Normalize review dialog (Keep original / Normalize / Cancel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalizeReviewChoice {
    KeepOriginal,
    Normalize,
    Cancel,
}

/// Whether save should offer the house-style review dialog.
pub fn should_offer_normalize_review(candidates: &SaveCandidates) -> bool {
    candidates.differs()
}

/// Apply a Normalize review choice. `None` means the save was cancelled.
pub fn normalize_review_decision(
    candidates: &SaveCandidates,
    choice: NormalizeReviewChoice,
) -> Option<String> {
    match choice {
        NormalizeReviewChoice::KeepOriginal => Some(candidates.preserved.clone()),
        NormalizeReviewChoice::Normalize => Some(candidates.normalized.clone()),
        NormalizeReviewChoice::Cancel => None,
    }
}

/// Bounded-latency private checkpoint using an injected millisecond clock.
#[derive(Debug, Clone, Default)]
pub struct AutosaveScheduler {
    pub delay_ms: u64,
    pending: Option<(usize, u64)>,
}

impl AutosaveScheduler {
    pub fn new(delay_ms: u64) -> Self {
        Self {
            delay_ms,
            pending: None,
        }
    }

    pub fn note_edit(&mut self, tab_id: usize, now_ms: u64) {
        let deadline = now_ms.saturating_add(self.delay_ms);
        self.pending = Some((
            tab_id,
            self.pending
                .map(|(_, due)| due.min(deadline))
                .unwrap_or(deadline),
        ));
    }

    pub fn due(&self, now_ms: u64) -> Option<usize> {
        self.pending
            .and_then(|(tab_id, due)| (now_ms >= due).then_some(tab_id))
    }

    pub fn clear(&mut self) {
        self.pending = None;
    }
}

pub struct HeadlessTab {
    pub id: usize,
    pub editor: HeadlessEditor,
    pub title: String,
    /// A merge or unresolved disk change must never become an implicit write
    /// through the deterministic autosave test path.
    autosave_blocked: bool,
}

/// Headless workspace: tabs, file tree, drop routing, export. No GPUI types.
pub struct HeadlessWorkspace {
    pub root: Option<PathBuf>,
    tabs: Vec<HeadlessTab>,
    pub active_tab: usize,
    next_tab_id: usize,
    pub theme: ThemeChoice,
    pub pending_external_change: Option<(usize, PathBuf)>,
    pending_external_change_tab_id: Option<usize>,
    pub last_export: Option<PathBuf>,
    autosave: AutosaveScheduler,
    now_ms: u64,
    recovery_store: Option<RecoveryStore>,
    recovery_archives: Vec<RecoveryTab>,
    private_checkpoint: Option<RecoverySnapshot>,
}

impl Default for HeadlessWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

impl HeadlessWorkspace {
    pub fn new() -> Self {
        Self::with_autosave_ms(1000)
    }

    pub fn with_autosave_ms(delay_ms: u64) -> Self {
        let mut workspace = Self {
            root: None,
            tabs: Vec::new(),
            active_tab: 0,
            next_tab_id: 1,
            theme: ThemeChoice::Dark,
            pending_external_change: None,
            pending_external_change_tab_id: None,
            last_export: None,
            autosave: AutosaveScheduler::new(delay_ms),
            now_ms: 0,
            recovery_store: None,
            recovery_archives: Vec::new(),
            private_checkpoint: None,
        };
        let _ = workspace.apply(WorkspaceCommand::NewDocument);
        workspace
    }

    pub fn tabs(&self) -> &[HeadlessTab] {
        &self.tabs
    }

    /// Explicit isolated storage for deterministic recovery tests. Default
    /// headless sessions never access the user's production data directory.
    pub fn with_private_recovery(delay_ms: u64, store: RecoveryStore) -> Self {
        let mut workspace = Self::with_autosave_ms(delay_ms);
        workspace.recovery_store = Some(store);
        workspace
    }

    pub fn private_checkpoint(&self) -> Option<&RecoverySnapshot> {
        self.private_checkpoint.as_ref()
    }

    pub fn active(&self) -> Option<&HeadlessTab> {
        self.tabs.get(self.active_tab)
    }

    pub fn active_mut(&mut self) -> Option<&mut HeadlessTab> {
        self.tabs.get_mut(self.active_tab)
    }

    fn set_pending_external_change(&mut self, index: usize, path: PathBuf) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.editor.document().path.as_deref() != Some(path.as_path()) {
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

    fn reconcile_pending_external_change(&mut self) {
        let Some((_, path)) = self.pending_external_change.clone() else {
            self.pending_external_change_tab_id = None;
            return;
        };
        let Some(tab_id) = self.pending_external_change_tab_id else {
            self.pending_external_change = None;
            return;
        };
        if let Some(index) = self.tabs.iter().position(|tab| {
            tab.id == tab_id && tab.editor.document().path.as_deref() == Some(path.as_path())
        }) {
            self.pending_external_change = Some((index, path));
        } else {
            self.pending_external_change = None;
            self.pending_external_change_tab_id = None;
        }
    }

    fn pending_external_change_matches_tab(&self, index: usize) -> bool {
        let Some((pending_index, pending_path)) = self.pending_external_change.as_ref() else {
            return true;
        };
        let Some(pending_tab_id) = self.pending_external_change_tab_id else {
            return false;
        };
        *pending_index == index
            && self.tabs.get(index).is_some_and(|tab| {
                tab.id == pending_tab_id
                    && tab.editor.document().path.as_deref() == Some(pending_path.as_path())
            })
    }

    pub fn list_files(&self) -> Vec<PathBuf> {
        self.root
            .as_deref()
            .map(list_markdown_files)
            .unwrap_or_default()
    }

    pub fn apply(&mut self, command: WorkspaceCommand) -> Result<EditorOutcome, SessionError> {
        match command {
            WorkspaceCommand::Editor(editor_command) => self.apply_editor(editor_command),
            WorkspaceCommand::Save => self.save_active(),
            WorkspaceCommand::SaveAs(path) => self.save_active_as(path),
            WorkspaceCommand::OpenFile(path) => self.open_file(path),
            WorkspaceCommand::OpenFolder(path) => {
                self.root = Some(path);
                Ok(EditorOutcome::Noop)
            }
            WorkspaceCommand::OpenLaunchPath(path) => self.open_launch_path(path),
            WorkspaceCommand::ExportHtml { output } => self.export_html(output),
            WorkspaceCommand::DropFiles { paths, target } => self.drop_files(paths, target),
            WorkspaceCommand::ToggleTheme => {
                self.theme = match self.theme {
                    ThemeChoice::Dark => ThemeChoice::Light,
                    ThemeChoice::Light => ThemeChoice::Dark,
                };
                Ok(EditorOutcome::Noop)
            }
            WorkspaceCommand::NewDocument => {
                self.push_tab(HeadlessEditor::new(""), None);
                Ok(EditorOutcome::Changed)
            }
            WorkspaceCommand::CloseTab => self.close_active_tab(),
            WorkspaceCommand::SwitchTab(index) => {
                if index >= self.tabs.len() {
                    return Err(SessionError::TabNotFound);
                }
                self.active_tab = index;
                Ok(EditorOutcome::Noop)
            }
            WorkspaceCommand::JumpToHeading { offset } => {
                self.apply_editor(EditorCommand::JumpTo(offset))
            }
            WorkspaceCommand::EditFrontmatter => self.apply_editor(EditorCommand::JumpTo(0)),
            WorkspaceCommand::SetFrontmatterField { key, value } => {
                self.set_frontmatter_field(&key, &value)
            }
            WorkspaceCommand::SaveWithReview(choice) => self.save_with_review(choice),
            WorkspaceCommand::AdvanceTime { millis } => {
                self.now_ms = self.now_ms.saturating_add(millis);
                self.flush_autosave()?;
                Ok(EditorOutcome::Noop)
            }
            WorkspaceCommand::ExternalFileChange(path) => {
                self.note_external_change(&path);
                Ok(EditorOutcome::Noop)
            }
            WorkspaceCommand::ReloadTab(index) => self.reload_tab(index),
        }
    }

    fn apply_editor(&mut self, command: EditorCommand) -> Result<EditorOutcome, SessionError> {
        let edits = matches!(
            command,
            EditorCommand::InsertText(_)
                | EditorCommand::Backspace
                | EditorCommand::Delete
                | EditorCommand::DeleteWordLeft
                | EditorCommand::DeleteWordRight
                | EditorCommand::DeleteToLineStart
                | EditorCommand::DeleteToLineEnd
                | EditorCommand::Undo
                | EditorCommand::Redo
                | EditorCommand::Wrap(_)
                | EditorCommand::Indent
                | EditorCommand::Outdent
        );
        let tab_id = self.active().ok_or(SessionError::NoActiveDocument)?.id;
        let outcome = self
            .active_mut()
            .ok_or(SessionError::NoActiveDocument)?
            .editor
            .apply(command)
            .map_err(|err| match err {
                markrust_editor::EditorError::InvalidRange => SessionError::InvalidRange,
            })?;
        if edits && outcome == EditorOutcome::Changed {
            self.autosave.note_edit(tab_id, self.now_ms);
        }
        Ok(outcome)
    }

    fn save_active(&mut self) -> Result<EditorOutcome, SessionError> {
        self.save_with_review(NormalizeReviewChoice::KeepOriginal)
    }

    fn save_with_review(
        &mut self,
        choice: NormalizeReviewChoice,
    ) -> Result<EditorOutcome, SessionError> {
        let active_tab = self.active_tab;
        let (tab_id, path, result) = {
            let tab = self.active_mut().ok_or(SessionError::NoActiveDocument)?;
            let path = tab
                .editor
                .document()
                .path
                .clone()
                .ok_or(SessionError::UntitledHasNoPath)?;
            let mut engine = markrust_core::rich::RichEngine::new();
            let candidates =
                markrust_core::rich::save_candidates(tab.editor.document(), &mut engine);
            if should_offer_normalize_review(&candidates) {
                let Some(text) = normalize_review_decision(&candidates, choice) else {
                    return Ok(EditorOutcome::Noop);
                };
                if text != tab.editor.document().buffer.content() {
                    let len = tab.editor.document().buffer.len_bytes();
                    tab.editor.document_mut().replace_range(0, len, &text);
                }
            } else if choice == NormalizeReviewChoice::Cancel {
                return Ok(EditorOutcome::Noop);
            }
            let expected_disk = tab.editor.document().saved_content().to_string();
            let result = tab
                .editor
                .document_mut()
                .save_checked_and_mark_clean(Some(&expected_disk));
            (tab.id, path, result)
        };
        match result {
            Ok(()) => {
                if let Some(tab) = self.tabs.get_mut(active_tab) {
                    tab.autosave_blocked = false;
                }
                self.clear_pending_external_change_for_tab(tab_id);
                self.autosave.clear();
                Ok(EditorOutcome::Changed)
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                self.note_external_change(&path);
                Err(SessionError::ExternalChange)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn set_frontmatter_field(
        &mut self,
        key: &str,
        value: &str,
    ) -> Result<EditorOutcome, SessionError> {
        let tab = self.active_mut().ok_or(SessionError::NoActiveDocument)?;
        let mut engine = markrust_core::rich::RichEngine::new();
        let mut caret = markrust_core::rich::CaretState::collapsed(tab.editor.cursor_offset());
        markrust_core::rich::apply_rich_command(
            tab.editor.document_mut(),
            &mut engine,
            &mut caret,
            markrust_core::rich::RichCommand::SetFrontmatterField {
                key: key.to_string(),
                value: value.to_string(),
            },
        )
        .map_err(|_| SessionError::InvalidRange)?;
        tab.editor.apply(EditorCommand::JumpTo(caret.cursor())).ok();
        Ok(EditorOutcome::Changed)
    }

    fn save_active_as(&mut self, path: PathBuf) -> Result<EditorOutcome, SessionError> {
        let active_tab = self.active_tab;
        let (tab_id, result) = {
            let tab = self.active_mut().ok_or(SessionError::NoActiveDocument)?;
            let expected_disk = (tab.editor.document().path.as_deref() == Some(path.as_path()))
                .then(|| tab.editor.document().saved_content().to_string());
            let result = tab
                .editor
                .document_mut()
                .save_as_checked(path.clone(), expected_disk.as_deref());
            (tab.id, result)
        };
        match result {
            Ok(()) => {
                if let Some(tab) = self.tabs.get_mut(active_tab) {
                    tab.title = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Untitled".into());
                    tab.autosave_blocked = false;
                }
                self.clear_pending_external_change_for_tab(tab_id);
                self.autosave.clear();
                Ok(EditorOutcome::Changed)
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Err(SessionError::ExternalChange)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn open_file(&mut self, path: PathBuf) -> Result<EditorOutcome, SessionError> {
        if let Some(index) = self.tab_index_for_path(&path) {
            self.active_tab = index;
            return Ok(EditorOutcome::Noop);
        }
        let mut document = Document::from_file(path.clone())?;
        document.dirty = false;
        self.push_tab(HeadlessEditor::from_document(document), Some(&path));
        self.dismiss_placeholder_untitled();
        Ok(EditorOutcome::Changed)
    }

    fn open_launch_path(&mut self, path: PathBuf) -> Result<EditorOutcome, SessionError> {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if path.is_dir() {
            self.root = Some(path);
            return Ok(EditorOutcome::Changed);
        }
        if self.root.is_none() {
            if let Some(parent) = path.parent() {
                self.root = Some(parent.to_path_buf());
            }
        }
        self.open_file(path)
    }

    fn dismiss_placeholder_untitled(&mut self) {
        let placeholder = self.tabs.iter().position(|tab| {
            tab.title == "Untitled"
                && tab.editor.document().path.is_none()
                && !tab.editor.document().dirty
                && tab.editor.content().is_empty()
        });
        if let Some(index) = placeholder {
            if self.tabs.len() > 1 {
                let active_was = self.active_tab;
                self.tabs.remove(index);
                if active_was > index {
                    self.active_tab = active_was - 1;
                } else {
                    self.active_tab = active_was.min(self.tabs.len() - 1);
                }
            }
        }
    }

    fn export_html(&mut self, output: Option<PathBuf>) -> Result<EditorOutcome, SessionError> {
        let tab = self.active().ok_or(SessionError::NoActiveDocument)?;
        let path = tab
            .editor
            .document()
            .path
            .clone()
            .ok_or(SessionError::UntitledHasNoPath)?;
        let exported = markrust_core::export_content_to_html(
            &tab.editor.content(),
            Some(&path),
            output.as_deref(),
        )?;
        self.last_export = Some(exported);
        Ok(EditorOutcome::Changed)
    }

    fn drop_files(
        &mut self,
        paths: Vec<PathBuf>,
        target: DropTarget,
    ) -> Result<EditorOutcome, SessionError> {
        let intent = match target {
            DropTarget::Editor => classify_editor_drop(&paths),
            DropTarget::Window => classify_window_drop(&paths),
        };
        match intent {
            DropIntent::OpenWorkspace(root) => {
                self.root = Some(root);
                Ok(EditorOutcome::Changed)
            }
            DropIntent::OpenDocuments(docs) => {
                for path in docs {
                    self.open_file(path)?;
                }
                Ok(EditorOutcome::Changed)
            }
            DropIntent::InsertImages(images) => {
                self.insert_images(images)?;
                Ok(EditorOutcome::Changed)
            }
            DropIntent::Ignored => Ok(EditorOutcome::Noop),
        }
    }

    fn insert_images(&mut self, images: Vec<PathBuf>) -> Result<EditorOutcome, SessionError> {
        let doc_path = self
            .active()
            .ok_or(SessionError::NoActiveDocument)?
            .editor
            .document()
            .path
            .clone();
        let mut snippets = Vec::new();
        for image in images {
            if let Some(reference) = markdown_image_reference(&image, doc_path.as_deref()) {
                snippets.push(reference);
            }
        }
        if snippets.is_empty() {
            return Ok(EditorOutcome::Noop);
        }
        let text = snippets.join("\n\n");
        let prefix = if doc_path.is_some() { "\n\n" } else { "" };
        self.apply_editor(EditorCommand::InsertText(format!("{prefix}{text}")))
    }

    fn close_active_tab(&mut self) -> Result<EditorOutcome, SessionError> {
        if self.tabs.len() <= 1 {
            self.checkpoint_private_session()?;
            return Ok(EditorOutcome::Noop);
        }
        let mut projected = self.capture_private_session();
        let closed = projected.tabs.remove(self.active_tab);
        if closed.dirty {
            projected.archived_tabs.push(closed.clone());
        }
        projected.active_tab = self.active_tab.min(projected.tabs.len() - 1);
        self.persist_private_session(&projected)?;
        if closed.dirty {
            self.recovery_archives.push(closed);
        }
        let removed = self.tabs.remove(self.active_tab);
        self.clear_pending_external_change_for_tab(removed.id);
        self.reconcile_pending_external_change();
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        Ok(EditorOutcome::Changed)
    }

    fn flush_autosave(&mut self) -> Result<(), SessionError> {
        if self.autosave.due(self.now_ms).is_none() {
            return Ok(());
        }
        self.checkpoint_private_session()?;
        self.autosave.clear();
        Ok(())
    }

    fn capture_private_session(&self) -> RecoverySnapshot {
        RecoverySnapshot {
            version: RECOVERY_VERSION,
            root: self.root.clone(),
            active_tab: self.active_tab,
            tabs: self
                .tabs
                .iter()
                .map(|tab| {
                    let document = tab.editor.document();
                    let state = tab.editor.state();
                    let selection = RecoverySelection {
                        start: state.selected_range.start,
                        end: state.selected_range.end,
                        reversed: state.selection_reversed,
                    };
                    RecoveryTab {
                        path: document.path.clone(),
                        title: tab.title.clone(),
                        mode: RecoveryEditorMode::Source,
                        editing_pane: RecoveryEditingPane::Source,
                        source_selection: selection.clone(),
                        rich_selection: selection,
                        content: document.buffer.content(),
                        saved_content: document.saved_content().to_string(),
                        dirty: document.dirty,
                        autosave_blocked: tab.autosave_blocked,
                        widget_draft: None,
                    }
                })
                .collect(),
            archived_tabs: self.recovery_archives.clone(),
        }
    }

    fn persist_private_session(&mut self, snapshot: &RecoverySnapshot) -> Result<(), SessionError> {
        snapshot.validate_for_write()?;
        if let Some(store) = &self.recovery_store {
            store.write(snapshot)?;
        }
        self.private_checkpoint = Some(snapshot.clone());
        Ok(())
    }

    pub fn checkpoint_private_session(&mut self) -> Result<(), SessionError> {
        let snapshot = self.capture_private_session();
        match self.persist_private_session(&snapshot) {
            Err(_)
                if self.recovery_archives.is_empty()
                    && self.tabs.iter().all(|tab| {
                        !crate::workspace::tab_requires_private_recovery(
                            tab.editor.document(),
                            false,
                        )
                    }) =>
            {
                Ok(())
            }
            result => result,
        }
    }

    fn note_external_change(&mut self, path: &Path) {
        let Ok(theirs) = std::fs::read_to_string(path) else {
            self.block_autosave_for_unreadable_path(path);
            return;
        };
        let actions = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let doc = tab.editor.document();
                (
                    index,
                    classify_external_change(
                        doc.path.as_deref(),
                        path,
                        doc.saved_content(),
                        &doc.buffer.content(),
                        &theirs,
                    ),
                )
            })
            .collect::<Vec<_>>();
        for (index, action) in actions {
            match action {
                ExternalChangeAction::Ignore => {}
                ExternalChangeAction::PromptReload | ExternalChangeAction::PromptConflict => {
                    if let Some(tab) = self.tabs.get_mut(index) {
                        tab.autosave_blocked = tab.editor.document().dirty;
                    }
                    self.set_pending_external_change(index, path.to_path_buf());
                }
                ExternalChangeAction::Apply(merged) => {
                    let Some((tab_id, clear_pending)) = self.tabs.get_mut(index).map(|tab| {
                        let caret = tab.editor.cursor_offset();
                        let mapped =
                            tab.editor
                                .document_mut()
                                .apply_merged_edit(&merged, &theirs, &[caret]);
                        if let Some(offset) = mapped.first() {
                            let _ = tab.editor.apply(EditorCommand::JumpTo(*offset));
                        }
                        tab.autosave_blocked = merged != theirs;
                        (tab.id, !tab.autosave_blocked)
                    }) else {
                        continue;
                    };
                    if clear_pending {
                        self.clear_pending_external_change_for_tab(tab_id);
                    }
                }
            }
        }
    }

    fn block_autosave_for_unreadable_path(&mut self, path: &Path) {
        for index in 0..self.tabs.len() {
            let matching_dirty_tab = self.tabs.get(index).is_some_and(|tab| {
                tab.editor.document().path.as_deref() == Some(path) && tab.editor.document().dirty
            });
            if matching_dirty_tab {
                if let Some(tab) = self.tabs.get_mut(index) {
                    tab.autosave_blocked = true;
                }
                self.set_pending_external_change(index, path.to_path_buf());
            }
        }
    }

    fn reload_tab(&mut self, index: usize) -> Result<EditorOutcome, SessionError> {
        if self.pending_external_change.is_some()
            && !self.pending_external_change_matches_tab(index)
        {
            self.reconcile_pending_external_change();
            return Ok(EditorOutcome::Noop);
        }
        let (tab_id, path, dirty) = {
            let tab = self.tabs.get(index).ok_or(SessionError::TabNotFound)?;
            (
                tab.id,
                tab.editor
                    .document()
                    .path
                    .clone()
                    .ok_or(SessionError::UntitledHasNoPath)?,
                tab.editor.document().dirty,
            )
        };
        if dirty {
            if let Some(tab) = self.tabs.get_mut(index) {
                tab.autosave_blocked = true;
            }
            self.set_pending_external_change(index, path);
            return Err(SessionError::DirtyReloadBlocked);
        }
        let content = std::fs::read_to_string(&path)?;
        {
            let tab = self.tabs.get_mut(index).ok_or(SessionError::TabNotFound)?;
            let caret = tab.editor.cursor_offset();
            let mapped = tab
                .editor
                .document_mut()
                .apply_external_edit(&content, &[caret]);
            if let Some(offset) = mapped.first() {
                let _ = tab.editor.apply(EditorCommand::JumpTo(*offset));
            }
            tab.autosave_blocked = false;
        }
        self.clear_pending_external_change_for_tab(tab_id);
        Ok(EditorOutcome::Changed)
    }

    fn tab_index_for_path(&self, path: &Path) -> Option<usize> {
        self.tabs.iter().position(|tab| {
            tab.editor
                .document()
                .path
                .as_deref()
                .is_some_and(|existing| existing == path)
        })
    }

    fn push_tab(&mut self, editor: HeadlessEditor, path: Option<&Path>) {
        let title = path
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "Untitled".into());
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.tabs.push(HeadlessTab {
            id,
            editor,
            title,
            autosave_blocked: false,
        });
        self.active_tab = self.tabs.len() - 1;
    }
}

const MAX_LISTED_MARKDOWN_FILES: usize = 2_000;
const MAX_LIST_DEPTH: usize = 8;

pub fn list_markdown_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files(root, &mut files, 0);
    files.sort();
    files
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > MAX_LIST_DEPTH || out.len() >= MAX_LISTED_MARKDOWN_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_LISTED_MARKDOWN_FILES {
            return;
        }
        let path = entry.path();
        if path.is_dir() {
            if should_skip_dir(&path) {
                continue;
            }
            collect_files(&path, out, depth + 1);
        } else if is_markdown(&path) {
            out.push(path);
        }
    }
}

pub fn should_skip_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with('.')
                || matches!(
                    name,
                    "node_modules" | "target" | "dist" | "build" | "vendor" | "coverage"
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Test-local directory with a unique name and guaranteed cleanup. Keeping
    /// this local avoids sharing state between parallel unit tests.
    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(prefix: &str) -> Self {
            let sequence = TEST_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "markrust-app-{prefix}-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create test directory");
            Self { path }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn untitled_cannot_save_without_path() {
        let mut workspace = HeadlessWorkspace::new();
        let err = workspace.apply(WorkspaceCommand::Save).unwrap_err();
        assert!(matches!(err, SessionError::UntitledHasNoPath));
    }

    #[test]
    fn reload_path_only_flags_conflict_when_dirty() {
        let path = Path::new("/tmp/note.md");
        assert_eq!(
            reload_decision(true, Some(path), path),
            ExternalChangeAction::PromptConflict
        );
        assert_eq!(
            reload_decision(false, Some(path), path),
            ExternalChangeAction::PromptReload
        );
        assert_eq!(
            reload_decision(false, Some(path), Path::new("/tmp/other.md")),
            ExternalChangeAction::Ignore
        );
    }

    #[test]
    fn classify_merges_disjoint_dirty_edits() {
        let path = Path::new("/tmp/note.md");
        let action = classify_external_change(
            Some(path),
            path,
            "aaa\nbbb\nccc\n",
            "aaa\nBBB\nccc\n",
            "aaa\nbbb\nCCC\n",
        );
        match action {
            ExternalChangeAction::Apply(merged) => assert_eq!(merged, "aaa\nBBB\nCCC\n"),
            other => panic!("expected apply, got {other:?}"),
        }
    }

    #[test]
    fn ordinary_save_merges_a_stale_disk_version_without_overwriting_it() {
        let dir = TestDir::new("checked-save-merge");
        let path = dir.join("note.md");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let mut workspace = HeadlessWorkspace::new();
        workspace
            .apply(WorkspaceCommand::OpenFile(path.clone()))
            .unwrap();
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::JumpTo(0)))
            .unwrap();
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
                "ours\n".into(),
            )))
            .unwrap();

        std::fs::write(&path, "one\ntwo\ntheirs\n").unwrap();
        let error = workspace.apply(WorkspaceCommand::Save).unwrap_err();

        assert!(matches!(error, SessionError::ExternalChange));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "one\ntwo\ntheirs\n"
        );
        let tab = workspace.active().unwrap();
        assert_eq!(tab.editor.content(), "ours\none\ntwo\ntheirs\n");
        assert!(tab.editor.document().dirty);
        assert!(tab.autosave_blocked);
    }

    #[test]
    fn private_autosave_preserves_the_buffer_and_base_without_touching_changed_disk() {
        let dir = TestDir::new("checked-autosave");
        let path = dir.join("note.md");
        std::fs::write(&path, "base\n").unwrap();
        let mut workspace = HeadlessWorkspace::with_autosave_ms(1);
        workspace
            .apply(WorkspaceCommand::OpenFile(path.clone()))
            .unwrap();
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::JumpTo(0)))
            .unwrap();
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
                "ours ".into(),
            )))
            .unwrap();

        std::fs::write(&path, "theirs\n").unwrap();
        workspace
            .apply(WorkspaceCommand::AdvanceTime { millis: 1 })
            .unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs\n");
        let tab = workspace.active().unwrap();
        assert_eq!(tab.editor.content(), "ours base\n");
        assert!(tab.editor.document().dirty);
        assert!(!tab.autosave_blocked);
        assert_eq!(workspace.pending_external_change, None);
        let checkpoint = workspace.private_checkpoint().unwrap();
        assert_eq!(checkpoint.tabs[0].content, "ours base\n");
        assert_eq!(checkpoint.tabs[0].saved_content, "base\n");
        assert!(checkpoint.tabs[0].dirty);
        workspace
            .apply(WorkspaceCommand::ExternalFileChange(path.clone()))
            .unwrap();
        assert!(workspace.active().unwrap().autosave_blocked);
        assert_eq!(workspace.pending_external_change, Some((0, path)));
    }

    #[test]
    fn private_autosave_is_durable_but_only_explicit_save_publishes() {
        let dir = TestDir::new("private-only-autosave");
        let path = dir.join("note.md");
        std::fs::write(&path, "base\n").unwrap();
        let store = RecoveryStore::new(dir.join("private"));
        let mut workspace = HeadlessWorkspace::with_private_recovery(150, store.clone());
        workspace
            .apply(WorkspaceCommand::OpenFile(path.clone()))
            .unwrap();
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::JumpTo(0)))
            .unwrap();
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
                "draft ".into(),
            )))
            .unwrap();
        workspace
            .apply(WorkspaceCommand::AdvanceTime { millis: 150 })
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "base\n");
        assert!(workspace.active().unwrap().editor.document().dirty);
        assert_eq!(
            store.load().snapshot.unwrap().tabs[0].content,
            "draft base\n"
        );
        workspace.apply(WorkspaceCommand::Save).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "draft base\n");
        assert!(!workspace.active().unwrap().editor.document().dirty);
    }

    #[test]
    fn sustained_typing_does_not_postpone_the_first_private_checkpoint() {
        let mut workspace = HeadlessWorkspace::with_autosave_ms(150);
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
                "a".into(),
            )))
            .unwrap();
        workspace
            .apply(WorkspaceCommand::AdvanceTime { millis: 100 })
            .unwrap();
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
                "b".into(),
            )))
            .unwrap();
        workspace
            .apply(WorkspaceCommand::AdvanceTime { millis: 50 })
            .unwrap();
        assert_eq!(
            workspace.private_checkpoint().unwrap().tabs[0].content,
            "ab"
        );
    }

    #[test]
    fn dirty_tab_close_checkpoints_the_archive_before_removing_the_owner() {
        let dir = TestDir::new("durable-close");
        let store = RecoveryStore::new(dir.join("private"));
        let mut workspace = HeadlessWorkspace::with_private_recovery(150, store.clone());
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
                "never saved 👩‍🚀".into(),
            )))
            .unwrap();
        workspace.apply(WorkspaceCommand::NewDocument).unwrap();
        workspace.apply(WorkspaceCommand::SwitchTab(0)).unwrap();
        workspace.apply(WorkspaceCommand::CloseTab).unwrap();
        assert_eq!(workspace.tabs().len(), 1);
        let snapshot = store.load().snapshot.unwrap();
        assert_eq!(snapshot.archived_tabs[0].content, "never saved 👩‍🚀");
        assert!(snapshot.archived_tabs[0].dirty);
    }

    #[test]
    fn failed_private_checkpoint_keeps_the_dirty_tab_open() {
        let dir = TestDir::new("failed-private-close");
        let invalid_directory = dir.join("not-a-directory");
        std::fs::write(&invalid_directory, "sentinel").unwrap();
        let mut workspace = HeadlessWorkspace::with_private_recovery(
            150,
            RecoveryStore::new(invalid_directory.clone()),
        );
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
                "live bytes".into(),
            )))
            .unwrap();
        workspace.apply(WorkspaceCommand::NewDocument).unwrap();
        workspace.apply(WorkspaceCommand::SwitchTab(0)).unwrap();
        assert!(matches!(
            workspace.apply(WorkspaceCommand::CloseTab),
            Err(SessionError::Recovery(_))
        ));
        assert_eq!(workspace.tabs().len(), 2);
        assert_eq!(workspace.active().unwrap().editor.content(), "live bytes");
        assert_eq!(
            std::fs::read_to_string(&invalid_directory).unwrap(),
            "sentinel"
        );
    }

    #[test]
    fn unavailable_private_storage_does_not_trap_a_clean_saved_session() {
        let dir = TestDir::new("clean-unavailable-recovery");
        let invalid_directory = dir.join("not-a-directory");
        std::fs::write(&invalid_directory, "sentinel").unwrap();
        let mut workspace = HeadlessWorkspace::with_private_recovery(
            150,
            RecoveryStore::new(invalid_directory.clone()),
        );
        assert!(workspace.checkpoint_private_session().is_ok());
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
                "unsaved".into(),
            )))
            .unwrap();
        assert!(matches!(
            workspace.checkpoint_private_session(),
            Err(SessionError::Recovery(_))
        ));
        assert_eq!(workspace.active().unwrap().editor.content(), "unsaved");
        assert_eq!(
            std::fs::read_to_string(invalid_directory).unwrap(),
            "sentinel"
        );
    }

    #[test]
    fn reload_never_discards_a_dirty_headless_buffer() {
        let dir = TestDir::new("dirty-reload");
        let path = dir.join("note.md");
        std::fs::write(&path, "base\n").unwrap();
        let mut workspace = HeadlessWorkspace::new();
        workspace
            .apply(WorkspaceCommand::OpenFile(path.clone()))
            .unwrap();
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::JumpTo(0)))
            .unwrap();
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
                "ours ".into(),
            )))
            .unwrap();
        std::fs::write(&path, "theirs\n").unwrap();
        workspace
            .apply(WorkspaceCommand::ExternalFileChange(path.clone()))
            .unwrap();

        let error = workspace.apply(WorkspaceCommand::ReloadTab(0)).unwrap_err();

        assert!(matches!(error, SessionError::DirtyReloadBlocked));
        assert_eq!(workspace.active().unwrap().editor.content(), "ours base\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs\n");
    }

    #[test]
    fn stale_pending_reload_index_cannot_target_the_tab_that_replaced_it() {
        let dir = TestDir::new("stale-reload-index");
        let path = dir.join("note.md");
        std::fs::write(&path, "base\n").unwrap();
        let mut workspace = HeadlessWorkspace::new();
        workspace.apply(WorkspaceCommand::NewDocument).unwrap();
        workspace
            .apply(WorkspaceCommand::OpenFile(path.clone()))
            .unwrap();
        std::fs::write(&path, "theirs\n").unwrap();
        workspace
            .apply(WorkspaceCommand::ExternalFileChange(path))
            .unwrap();
        assert_eq!(
            workspace
                .pending_external_change
                .as_ref()
                .map(|(index, _)| *index),
            Some(1)
        );
        workspace.apply(WorkspaceCommand::SwitchTab(0)).unwrap();
        workspace.apply(WorkspaceCommand::CloseTab).unwrap();

        let outcome = workspace.apply(WorkspaceCommand::ReloadTab(1)).unwrap();
        assert_eq!(outcome, EditorOutcome::Noop);
        assert_eq!(
            workspace
                .pending_external_change
                .as_ref()
                .map(|(index, _)| *index),
            Some(0)
        );
        assert_eq!(workspace.active().unwrap().editor.content(), "base\n");
    }

    #[test]
    fn normalize_review_decision_table() {
        let doc = Document::new("Title\n=====\n\npara\n");
        let mut engine = markrust_core::rich::RichEngine::new();
        let candidates = markrust_core::rich::save_candidates(&doc, &mut engine);
        assert!(should_offer_normalize_review(&candidates));
        assert_eq!(
            normalize_review_decision(&candidates, NormalizeReviewChoice::KeepOriginal).as_deref(),
            Some(candidates.preserved.as_str())
        );
        let normalized =
            normalize_review_decision(&candidates, NormalizeReviewChoice::Normalize).unwrap();
        assert!(normalized.contains("# Title"), "{normalized}");
        assert!(normalize_review_decision(&candidates, NormalizeReviewChoice::Cancel).is_none());
    }

    #[test]
    fn edit_frontmatter_jumps_to_start() {
        let mut workspace = HeadlessWorkspace::new();
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
                "---\ntitle: T\n---\n\n# Body\n".into(),
            )))
            .unwrap();
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::JumpTo(10)))
            .unwrap();
        workspace.apply(WorkspaceCommand::EditFrontmatter).unwrap();
        assert_eq!(workspace.active().unwrap().editor.cursor_offset(), 0);
    }

    #[test]
    fn save_with_review_normalize_rewrites_then_saves() {
        let dir = TestDir::new("normalize");
        let path = dir.join("note.md");
        std::fs::write(&path, "Title\n=====\n\npara\n").unwrap();
        let mut workspace = HeadlessWorkspace::new();
        workspace
            .apply(WorkspaceCommand::OpenFile(path.clone()))
            .unwrap();
        workspace
            .apply(WorkspaceCommand::SaveWithReview(
                NormalizeReviewChoice::Normalize,
            ))
            .unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# Title"), "{saved}");
    }

    #[test]
    fn set_frontmatter_field_from_workspace() {
        let mut workspace = HeadlessWorkspace::new();
        workspace
            .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
                "# Body\n".into(),
            )))
            .unwrap();
        workspace
            .apply(WorkspaceCommand::SetFrontmatterField {
                key: "title".into(),
                value: "Hello".into(),
            })
            .unwrap();
        workspace
            .apply(WorkspaceCommand::SetFrontmatterField {
                key: "description".into(),
                value: "A note".into(),
            })
            .unwrap();
        let content = workspace.active().unwrap().editor.content();
        assert!(content.contains("title: \"Hello\""), "{content}");
        assert!(content.contains("description: \"A note\""), "{content}");
        assert!(content.contains("# Body"), "{content}");
    }

    #[test]
    fn autosave_scheduler_respects_delay() {
        let mut scheduler = AutosaveScheduler::new(1000);
        scheduler.note_edit(3, 0);
        assert_eq!(scheduler.due(999), None);
        assert_eq!(scheduler.due(1000), Some(3));
    }

    #[test]
    fn toggle_theme_flips_in_memory() {
        let mut workspace = HeadlessWorkspace::new();
        assert_eq!(workspace.theme, ThemeChoice::Dark);
        workspace.apply(WorkspaceCommand::ToggleTheme).unwrap();
        assert_eq!(workspace.theme, ThemeChoice::Light);
    }

    #[test]
    fn open_file_with_remote_image_does_not_block_on_network() {
        let dir = TestDir::new("open-remote");
        let path = dir.join("note.md");
        // Opening only parses Markdown; it must not resolve or contact a
        // document-controlled URL. A private HTTPS target makes that explicit
        // without opening a listener (which sandboxed test runners forbid).
        std::fs::write(&path, "![alt](https://127.0.0.1/hang.png)\n").unwrap();
        let started = std::time::Instant::now();
        let mut workspace = HeadlessWorkspace::new();
        workspace.apply(WorkspaceCommand::OpenFile(path)).unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "OpenFile blocked on network for {elapsed:?}"
        );
        assert!(workspace
            .active()
            .unwrap()
            .editor
            .content()
            .contains("hang.png"));
    }

    #[test]
    fn open_launch_path_opens_file_and_parent_folder() {
        let dir = TestDir::new("open-launch");
        let path = dir.join("note.md");
        std::fs::write(&path, "# Hello\n").unwrap();
        let mut workspace = HeadlessWorkspace::new();
        workspace
            .apply(WorkspaceCommand::OpenLaunchPath(path.clone()))
            .unwrap();
        let canonical = std::fs::canonicalize(&path).unwrap();
        assert_eq!(
            workspace
                .active()
                .and_then(|tab| tab.editor.document().path.clone()),
            Some(canonical.clone())
        );
        assert_eq!(workspace.root.as_deref(), canonical.parent());
        assert_eq!(workspace.tabs().len(), 1);
        assert_eq!(workspace.active().unwrap().editor.content(), "# Hello\n");
    }

    #[test]
    fn list_markdown_files_is_bounded_and_timely() {
        let dir = TestDir::new("list-bound");
        let mut cur = dir.path().to_path_buf();
        for i in 0..20 {
            cur = cur.join(format!("d{i}"));
            std::fs::create_dir_all(&cur).unwrap();
            std::fs::write(cur.join("n.md"), "x").unwrap();
        }
        let started = std::time::Instant::now();
        let files = list_markdown_files(dir.path());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "sidebar walk hung: {:?}",
            started.elapsed()
        );
        assert!(!files.is_empty());
        assert!(
            files.len() <= MAX_LIST_DEPTH + 1,
            "depth cap failed: {} files",
            files.len()
        );
    }
}
