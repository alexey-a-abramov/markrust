// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The WYSIWYG editor view: a virtualized list of rendered blocks kept in
//! sync with the document through [`RichEngine`].

use std::sync::Arc;

use gpui::{div, list, prelude::*, px, Context, Entity, ListAlignment, ListState, Render, Window};
use markrust_core::rich::RichEngine;
use markrust_core::Document;

use super::blocks::{render_top_block, RenderSnapshot};
use crate::theme::EditorTheme;

pub struct RichEditorView {
    document: Entity<Document>,
    pub theme: EditorTheme,
    engine: RichEngine,
    list_state: ListState,
    snapshot: Option<Arc<RenderSnapshot>>,
    synced_revision: Option<u64>,
}

impl RichEditorView {
    pub fn new(document: Entity<Document>, theme: EditorTheme, cx: &mut Context<Self>) -> Self {
        cx.observe(&document, |_, _, cx| cx.notify()).detach();
        Self {
            document,
            theme,
            engine: RichEngine::new(),
            list_state: ListState::new(0, ListAlignment::Top, px(512.)),
            snapshot: None,
            synced_revision: None,
        }
    }

    pub fn set_theme(&mut self, theme: EditorTheme, cx: &mut Context<Self>) {
        self.theme = theme;
        self.snapshot = None;
        self.synced_revision = None;
        cx.notify();
    }

    fn sync_snapshot(&mut self, cx: &mut Context<Self>) -> Arc<RenderSnapshot> {
        let doc = self.document.read(cx);
        let revision = doc.revision();
        if self.synced_revision == Some(revision) {
            if let Some(snapshot) = &self.snapshot {
                return snapshot.clone();
            }
        }
        let base_dir = doc
            .path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf());
        let old_count = self.engine.tree().blocks.len();
        self.engine.sync(doc);
        let new_count = self.engine.tree().blocks.len();
        match self.engine.last_splice() {
            Some(splice) if self.synced_revision.is_some() => {
                self.list_state
                    .splice(splice.range.clone(), splice.new_count);
            }
            _ => {
                self.list_state
                    .splice(0..old_count.min(new_count.max(old_count)), new_count);
                // Full reset on first sync.
                self.list_state = ListState::new(new_count, ListAlignment::Top, px(512.));
            }
        }
        let snapshot = Arc::new(RenderSnapshot {
            tree: self.engine.tree().clone(),
            theme: self.theme.clone(),
            base_dir,
        });
        self.snapshot = Some(snapshot.clone());
        self.synced_revision = Some(revision);
        snapshot
    }
}

impl Render for RichEditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.sync_snapshot(cx);
        let theme = self.theme.clone();
        div().size_full().bg(theme.editor_bg).child(
            list(self.list_state.clone(), move |index, _window, _cx| {
                render_top_block(&snapshot, index)
            })
            .size_full()
            .py(px(16.)),
        )
    }
}
