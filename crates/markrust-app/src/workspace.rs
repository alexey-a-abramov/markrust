// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use gpui::{AppContext, Context, Entity, ExternalPaths, Task, Window};

use crate::config::{is_markdown, AppConfig, RecentWorkspaces};
use crate::drop::{
    classify_editor_drop, classify_window_drop, markdown_image_reference, DropIntent,
};
use crate::session::{
    list_markdown_files, normalize_review_decision, reload_decision, should_offer_normalize_review,
    DropTarget, NormalizeReviewChoice, ReloadDecision, WorkspaceCommand,
};
use markrust_core::Document;
use markrust_editor::{EditorCommand, MarkdownEditor, MarkdownEditorView, RichEditorView};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

/// Which editing surface a tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorMode {
    /// Rendered rich document (editable).
    #[default]
    Wysiwyg,
    /// Raw markdown with delimiter masking.
    Source,
}

#[allow(dead_code)]
pub struct DocumentTab {
    pub id: usize,
    pub document: Entity<Document>,
    pub editor: Entity<MarkdownEditor>,
    pub editor_view: Entity<MarkdownEditorView>,
    pub rich_view: Entity<RichEditorView>,
    pub mode: EditorMode,
    pub title: String,
}

pub struct Workspace {
    pub root: Option<PathBuf>,
    pub tabs: Vec<DocumentTab>,
    pub active_tab: usize,
    pub next_tab_id: usize,
    pub sidebar_open: bool,
    pub outline_open: bool,
    pub palette_open: bool,
    pub config: AppConfig,
    pub pending_external_change: Option<(usize, PathBuf)>,
    pub recent: RecentWorkspaces,
    cached_files: Vec<PathBuf>,
    _watcher: Option<RecommendedWatcher>,
    _watcher_task: Task<()>,
    _file_list_task: Task<()>,
    _autosave_tasks: HashMap<usize, Task<()>>,
}

#[allow(dead_code)]
impl Workspace {
    pub fn new(config: AppConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut workspace = Self {
            root: None,
            tabs: Vec::new(),
            active_tab: 0,
            next_tab_id: 1,
            sidebar_open: true,
            outline_open: true,
            palette_open: false,
            config,
            pending_external_change: None,
            recent: RecentWorkspaces::load(),
            cached_files: Vec::new(),
            _watcher: None,
            _watcher_task: Task::ready(()),
            _file_list_task: Task::ready(()),
            _autosave_tasks: HashMap::new(),
        };
        workspace.new_document(window, cx);
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
                let mode = self.active_tab().map(|t| t.mode);
                let tab_id = self.active_tab().map(|t| t.id);
                match mode {
                    Some(EditorMode::Wysiwyg) => {
                        if let Some(tab) = self.tabs.get(self.active_tab) {
                            tab.rich_view.update(cx, |view, cx| {
                                view.apply_editor_command(editor_command.clone(), cx);
                            });
                        }
                    }
                    _ => {
                        if let Some(tab) = self.active_tab() {
                            tab.editor.update(cx, |editor, cx| {
                                editor.apply_command(editor_command, cx);
                            });
                        }
                    }
                }
                if let Some(id) = tab_id {
                    self.schedule_autosave(id, cx);
                }
            }
            WorkspaceCommand::Save => self.save_active(cx),
            WorkspaceCommand::SaveAs(path) => {
                if let Some(tab) = self.active_tab() {
                    tab.document.update(cx, |doc, cx| {
                        let _ = doc.save_as(path);
                        cx.notify();
                    });
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
                    self.active_tab = index;
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
                    }
                }
            }
            WorkspaceCommand::EditFrontmatter => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.mode = EditorMode::Source;
                    tab.editor.update(cx, |editor, cx| {
                        editor.apply_command(EditorCommand::JumpTo(0), cx);
                    });
                    cx.notify();
                }
            }
            WorkspaceCommand::SetFrontmatterField { key, value } => {
                if let Some((view, id)) = self
                    .active_tab()
                    .map(|tab| (tab.rich_view.clone(), tab.id))
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
                    let dirty = self.tabs[index].document.read(cx).dirty;
                    if reload_decision(dirty, Some(&path), &path) == ReloadDecision::PromptReload {
                        self.pending_external_change = Some((index, path));
                        cx.notify();
                    }
                }
            }
            WorkspaceCommand::ReloadTab(index) => {
                self.reload_tab(index, cx)?;
            }
        }
        Ok(())
    }

    /// Cycle the active tab between the source and WYSIWYG surfaces.
    pub fn toggle_editor_mode(&mut self, cx: &mut Context<Self>) {
        let index = self.active_tab;
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.mode = match tab.mode {
                EditorMode::Source => EditorMode::Wysiwyg,
                EditorMode::Wysiwyg => EditorMode::Source,
            };
            cx.notify();
        }
    }

    pub fn active_tab(&self) -> Option<&DocumentTab> {
        self.tabs.get(self.active_tab)
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
            self.active_tab = index;
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

    fn push_tab(
        &mut self,
        document: Entity<Document>,
        path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = path
            .as_ref()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "Untitled".into());
        let theme = self.config.editor_theme();
        let editor = cx.new(|cx| MarkdownEditor::new(document.clone(), theme, window, cx));
        let editor_view = cx.new(|_| MarkdownEditorView::new(editor.clone()));
        let rich_view = cx.new(|cx| {
            RichEditorView::new(document.clone(), self.config.editor_theme(), window, cx)
        });
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.tabs.push(DocumentTab {
            id,
            document: document.clone(),
            editor,
            editor_view,
            rich_view,
            mode: EditorMode::default(),
            title,
        });
        self.active_tab = self.tabs.len() - 1;
        spawn_parse_pump(document, cx);
        cx.notify();
    }

    pub fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.tabs.remove(index);
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        if self.tabs.is_empty() {
            self.new_document(window, cx);
        }
        cx.notify();
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

    pub fn save_with_review(&mut self, choice: NormalizeReviewChoice, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        tab.document.update(cx, |doc, cx| {
            if doc.path.is_none() {
                return;
            }
            let mut engine = markrust_core::rich::RichEngine::new();
            let candidates = markrust_core::rich::save_candidates(doc, &mut engine);
            if should_offer_normalize_review(&candidates) {
                let Some(text) = normalize_review_decision(&candidates, choice) else {
                    return;
                };
                if text != doc.buffer.content() {
                    let len = doc.buffer.len_bytes();
                    doc.replace_range(0, len, &text);
                }
            } else if choice == NormalizeReviewChoice::Cancel {
                return;
            }
            if doc.save_and_mark_clean().is_ok() {
                cx.notify();
            }
        });
        cx.notify();
    }

    pub fn normalize_candidates(&self, cx: &gpui::App) -> Option<markrust_core::rich::SaveCandidates> {
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
        let delay = Duration::from_millis(self.config.autosave_ms);
        let workspace = cx.entity();
        self._autosave_tasks.insert(
            tab_id,
            cx.spawn(async move |_, cx| {
                cx.background_executor().timer(delay).await;
                workspace.update(cx, |workspace, cx| {
                    if let Some(tab) = workspace.tabs.iter().find(|tab| tab.id == tab_id) {
                        if tab.document.read(cx).dirty && tab.document.read(cx).path.is_some() {
                            tab.document.update(cx, |doc, cx| {
                                if doc.save_and_mark_clean().is_ok() {
                                    cx.notify();
                                }
                            });
                        }
                    }
                });
            }),
        );
    }

    pub fn open_workspace(&mut self, root: PathBuf, cx: &mut Context<Self>) -> anyhow::Result<()> {
        self.root = Some(root.clone());
        self.recent.push(root);
        let _ = self.recent.save();
        self.start_watcher(cx);
        self.schedule_file_list_scan(cx);
        cx.notify();
        Ok(())
    }

    fn start_watcher(&mut self, cx: &mut Context<Self>) {
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
                let dirty = self.tabs[index].document.read(cx).dirty;
                let tab_path = self.tabs[index].document.read(cx).path.clone();
                match reload_decision(dirty, tab_path.as_deref(), &path) {
                    ReloadDecision::PromptReload => {
                        self.pending_external_change = Some((index, path));
                        cx.notify();
                    }
                    ReloadDecision::SkipBecauseDirty | ReloadDecision::Ignore => {}
                }
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
        let path = self.tabs[index]
            .document
            .read(cx)
            .path
            .clone()
            .ok_or_else(|| anyhow::anyhow!("tab has no path"))?;
        let content = std::fs::read_to_string(&path)?;
        let source_caret = self.tabs[index].editor.read(cx).cursor_offset();
        let rich_caret = self.tabs[index].rich_view.read(cx).cursor_offset();
        let document = self.tabs[index].document.clone();
        let mapped = document.update(cx, |doc, cx| {
            let mapped = doc.apply_external_edit(&content, &[source_caret, rich_caret]);
            cx.notify();
            mapped
        });
        let source_mapped = mapped.first().copied().unwrap_or(source_caret);
        let rich_mapped = mapped.get(1).copied().unwrap_or(rich_caret);
        self.tabs[index].editor.update(cx, |editor, cx| {
            editor.apply_command(EditorCommand::JumpTo(source_mapped), cx);
        });
        self.tabs[index].rich_view.update(cx, |view, cx| {
            view.jump_to(rich_mapped, cx);
        });
        spawn_parse_pump(document, cx);
        self.pending_external_change = None;
        cx.notify();
        Ok(())
    }

    pub fn list_files(&self) -> Vec<PathBuf> {
        self.cached_files.clone()
    }

    pub fn toggle_theme(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.config.toggle_theme();
        let _ = self.config.save();
        let theme = self.config.editor_theme();
        for tab in &self.tabs {
            tab.editor.update(cx, |editor, cx| {
                editor.theme = theme.clone();
                cx.notify();
            });
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

/// Blocking notify receive. Must run on a background worker, never on GPUI's
/// foreground executor — that is the hang that made opening a file freeze the UI.
fn recv_notify_blocking(
    rx: &Mutex<mpsc::Receiver<notify::Result<Event>>>,
) -> Result<notify::Result<Event>, mpsc::RecvError> {
    rx.lock().unwrap_or_else(|e| e.into_inner()).recv()
}

/// Drain background parse results without blocking a GPUI frame.
fn spawn_parse_pump(document: Entity<Document>, cx: &mut Context<Workspace>) {
    cx.spawn(async move |_, cx| loop {
        cx.background_executor()
            .timer(Duration::from_millis(32))
            .await;
        let done = document.update(cx, |doc, cx| {
            if !doc.mode.parses_markdown() {
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
    use std::time::Instant;

    #[test]
    fn fuzzy_match_finds_subsequence() {
        assert!(fuzzy_match("README.md", "readme"));
        assert!(!fuzzy_match("README.md", "xyz"));
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
        let started = Instant::now();
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "try_recv blocked for {:?}",
            started.elapsed()
        );
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
