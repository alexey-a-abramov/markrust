// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{AppContext, Context, Entity, ExternalPaths, Task, Window};

use crate::config::AppConfig;
use crate::drop::{
    classify_editor_drop, classify_window_drop, markdown_image_reference, DropIntent,
};
use crate::session::{
    list_markdown_files, reload_decision, DropTarget, ReloadDecision, WorkspaceCommand,
};
use markrust_core::Document;
use markrust_editor::{EditorCommand, MarkdownEditor, MarkdownEditorView};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

#[allow(dead_code)]
pub struct DocumentTab {
    pub id: usize,
    pub document: Entity<Document>,
    pub editor: Entity<MarkdownEditor>,
    pub editor_view: Entity<MarkdownEditorView>,
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
    _watcher: Option<RecommendedWatcher>,
    _watcher_task: Task<()>,
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
            _watcher: None,
            _watcher_task: Task::ready(()),
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
                if let Some(tab) = self.active_tab() {
                    tab.editor.update(cx, |editor, cx| {
                        editor.apply_command(editor_command, cx);
                    });
                    if let Some(tab) = self.active_tab() {
                        self.schedule_autosave(tab.id, cx);
                    }
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
                if let Some(tab) = self.active_tab() {
                    tab.editor.update(cx, |editor, cx| {
                        editor.apply_command(EditorCommand::JumpTo(offset), cx);
                    });
                }
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
        let content = std::fs::read_to_string(&path)?;
        let mut document = Document::new(&content);
        document.path = Some(path.clone());
        document.dirty = false;
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
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.tabs.push(DocumentTab {
            id,
            document,
            editor,
            editor_view,
            title,
        });
        self.active_tab = self.tabs.len() - 1;
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
        let Some(tab) = self.active_tab() else {
            return;
        };
        tab.document.update(cx, |doc, cx| {
            if doc.save_and_mark_clean().is_ok() {
                cx.notify();
            }
        });
        cx.notify();
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
        let mut recent = crate::config::RecentWorkspaces::load();
        recent.push(root);
        let _ = recent.save();
        self.start_watcher(cx);
        cx.notify();
        Ok(())
    }

    fn start_watcher(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .ok();
        if let Some(watcher) = watcher.as_mut() {
            let _ = watcher.watch(&root, RecursiveMode::Recursive);
        }
        let workspace = cx.entity();
        self._watcher = watcher;
        self._watcher_task = cx.spawn(async move |_, cx| loop {
            if let Ok(Ok(event)) = rx.recv() {
                workspace.update(cx, |workspace, cx| workspace.handle_fs_event(event, cx));
            }
        });
    }

    fn handle_fs_event(&mut self, event: Event, cx: &mut Context<Self>) {
        for path in event.paths {
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
    }

    pub fn reload_tab(&mut self, index: usize, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let path = self.tabs[index]
            .document
            .read(cx)
            .path
            .clone()
            .ok_or_else(|| anyhow::anyhow!("tab has no path"))?;
        let content = std::fs::read_to_string(&path)?;
        self.tabs[index].document.update(cx, |doc, cx| {
            doc.replace_content(&content);
            cx.notify();
        });
        self.pending_external_change = None;
        cx.notify();
        Ok(())
    }

    pub fn list_files(&self) -> Vec<PathBuf> {
        let Some(root) = &self.root else {
            return Vec::new();
        };
        list_markdown_files(root)
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
}
