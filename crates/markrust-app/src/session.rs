// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::{Path, PathBuf};

use markrust_core::Document;
use markrust_editor::{EditorCommand, EditorOutcome, HeadlessEditor};
use thiserror::Error;

use crate::config::{is_markdown, ThemeChoice};
use crate::drop::{
    classify_editor_drop, classify_window_drop, markdown_image_reference, DropIntent,
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
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Whether an on-disk change should reload a tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadDecision {
    Ignore,
    PromptReload,
    SkipBecauseDirty,
}

pub fn reload_decision(
    dirty: bool,
    tab_path: Option<&Path>,
    changed_path: &Path,
) -> ReloadDecision {
    match tab_path {
        Some(path) if path == changed_path => {
            if dirty {
                ReloadDecision::SkipBecauseDirty
            } else {
                ReloadDecision::PromptReload
            }
        }
        _ => ReloadDecision::Ignore,
    }
}

/// Debounced autosave using an injected millisecond clock.
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
        self.pending = Some((tab_id, now_ms.saturating_add(self.delay_ms)));
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
}

/// Headless workspace: tabs, file tree, drop routing, export. No GPUI types.
pub struct HeadlessWorkspace {
    pub root: Option<PathBuf>,
    tabs: Vec<HeadlessTab>,
    pub active_tab: usize,
    next_tab_id: usize,
    pub theme: ThemeChoice,
    pub pending_external_change: Option<(usize, PathBuf)>,
    pub last_export: Option<PathBuf>,
    autosave: AutosaveScheduler,
    now_ms: u64,
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
            last_export: None,
            autosave: AutosaveScheduler::new(delay_ms),
            now_ms: 0,
        };
        let _ = workspace.apply(WorkspaceCommand::NewDocument);
        workspace
    }

    pub fn tabs(&self) -> &[HeadlessTab] {
        &self.tabs
    }

    pub fn active(&self) -> Option<&HeadlessTab> {
        self.tabs.get(self.active_tab)
    }

    pub fn active_mut(&mut self) -> Option<&mut HeadlessTab> {
        self.tabs.get_mut(self.active_tab)
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
            WorkspaceCommand::AdvanceTime { millis } => {
                self.now_ms = self.now_ms.saturating_add(millis);
                self.flush_autosave();
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
                | EditorCommand::Undo
                | EditorCommand::Redo
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
        let tab = self.active_mut().ok_or(SessionError::NoActiveDocument)?;
        if tab.editor.document().path.is_none() {
            return Err(SessionError::UntitledHasNoPath);
        }
        tab.editor.document_mut().save_and_mark_clean()?;
        self.autosave.clear();
        Ok(EditorOutcome::Changed)
    }

    fn save_active_as(&mut self, path: PathBuf) -> Result<EditorOutcome, SessionError> {
        let tab = self.active_mut().ok_or(SessionError::NoActiveDocument)?;
        tab.editor.document_mut().save_as(path.clone())?;
        tab.title = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into());
        self.autosave.clear();
        Ok(EditorOutcome::Changed)
    }

    fn open_file(&mut self, path: PathBuf) -> Result<EditorOutcome, SessionError> {
        if let Some(index) = self.tab_index_for_path(&path) {
            self.active_tab = index;
            return Ok(EditorOutcome::Noop);
        }
        let mut document = Document::from_file(path.clone())?;
        document.dirty = false;
        self.push_tab(HeadlessEditor::from_document(document), Some(&path));
        Ok(EditorOutcome::Changed)
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
            return Ok(EditorOutcome::Noop);
        }
        self.tabs.remove(self.active_tab);
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        Ok(EditorOutcome::Changed)
    }

    fn flush_autosave(&mut self) {
        let Some(tab_id) = self.autosave.due(self.now_ms) else {
            return;
        };
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            let doc = tab.editor.document();
            if doc.dirty && doc.path.is_some() {
                let _ = tab.editor.document_mut().save_and_mark_clean();
            }
        }
        self.autosave.clear();
    }

    fn note_external_change(&mut self, path: &Path) {
        for (index, tab) in self.tabs.iter().enumerate() {
            match reload_decision(
                tab.editor.document().dirty,
                tab.editor.document().path.as_deref(),
                path,
            ) {
                ReloadDecision::PromptReload => {
                    self.pending_external_change = Some((index, path.to_path_buf()));
                }
                ReloadDecision::SkipBecauseDirty | ReloadDecision::Ignore => {}
            }
        }
    }

    fn reload_tab(&mut self, index: usize) -> Result<EditorOutcome, SessionError> {
        let tab = self.tabs.get_mut(index).ok_or(SessionError::TabNotFound)?;
        let path = tab
            .editor
            .document()
            .path
            .clone()
            .ok_or(SessionError::UntitledHasNoPath)?;
        let content = std::fs::read_to_string(&path)?;
        tab.editor.document_mut().replace_content(&content);
        self.pending_external_change = None;
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
        self.tabs.push(HeadlessTab { id, editor, title });
        self.active_tab = self.tabs.len() - 1;
    }
}

pub fn list_markdown_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files(root, &mut files);
    files.sort();
    files
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if should_skip_dir(&path) {
                continue;
            }
            collect_files(&path, out);
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

    #[test]
    fn untitled_cannot_save_without_path() {
        let mut workspace = HeadlessWorkspace::new();
        let err = workspace.apply(WorkspaceCommand::Save).unwrap_err();
        assert!(matches!(err, SessionError::UntitledHasNoPath));
    }

    #[test]
    fn reload_skips_dirty_tabs() {
        let path = Path::new("/tmp/note.md");
        assert_eq!(
            reload_decision(true, Some(path), path),
            ReloadDecision::SkipBecauseDirty
        );
        assert_eq!(
            reload_decision(false, Some(path), path),
            ReloadDecision::PromptReload
        );
        assert_eq!(
            reload_decision(false, Some(path), Path::new("/tmp/other.md")),
            ReloadDecision::Ignore
        );
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
}
