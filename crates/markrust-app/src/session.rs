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
    #[error(transparent)]
    Io(#[from] std::io::Error),
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
        let tab = self.active_mut().ok_or(SessionError::NoActiveDocument)?;
        if tab.editor.document().path.is_none() {
            return Err(SessionError::UntitledHasNoPath);
        }
        let mut engine = markrust_core::rich::RichEngine::new();
        let candidates = markrust_core::rich::save_candidates(tab.editor.document(), &mut engine);
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
        tab.editor.document_mut().save_and_mark_clean()?;
        self.autosave.clear();
        Ok(EditorOutcome::Changed)
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
        let Ok(theirs) = std::fs::read_to_string(path) else {
            return;
        };
        let mut pending = None;
        let mut merges = Vec::new();
        for (index, tab) in self.tabs.iter().enumerate() {
            let doc = tab.editor.document();
            match classify_external_change(
                doc.path.as_deref(),
                path,
                doc.saved_content(),
                &doc.buffer.content(),
                &theirs,
            ) {
                ExternalChangeAction::PromptReload | ExternalChangeAction::PromptConflict => {
                    pending = Some((index, path.to_path_buf()));
                }
                ExternalChangeAction::Apply(merged) => merges.push((index, merged)),
                ExternalChangeAction::Ignore => {}
            }
        }
        for (index, merged) in merges {
            let Some(tab) = self.tabs.get_mut(index) else {
                continue;
            };
            let caret = tab.editor.cursor_offset();
            let mapped = tab
                .editor
                .document_mut()
                .apply_merged_edit(&merged, &theirs, &[caret]);
            if let Some(offset) = mapped.first() {
                let _ = tab.editor.apply(EditorCommand::JumpTo(*offset));
            }
        }
        if let Some(pending) = pending {
            self.pending_external_change = Some(pending);
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
        let caret = tab.editor.cursor_offset();
        let mapped = tab
            .editor
            .document_mut()
            .apply_external_edit(&content, &[caret]);
        if let Some(offset) = mapped.first() {
            let _ = tab.editor.apply(EditorCommand::JumpTo(*offset));
        }
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
