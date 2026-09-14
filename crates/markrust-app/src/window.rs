// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use gpui::{
    actions, div, prelude::*, px, App, Context, Entity, ExternalPaths, FocusHandle, Focusable,
    FontWeight, PathPromptOptions, PromptButton, PromptLevel, Render, SharedString, Window,
};
use markrust_core::parse_frontmatter;
use markrust_editor::outline_headings;
use std::path::Path;

use crate::session::{
    should_offer_normalize_review, DropTarget, NormalizeReviewChoice, WorkspaceCommand,
};
use crate::ui::{
    document_tab, empty_sidebar_state, muted_hint, outline_row, section_header, sidebar_row,
    toolbar_button,
};
use crate::workspace::{fuzzy_match, Workspace};

actions!(
    markrust_app,
    [
        Save,
        OpenFile,
        OpenFolder,
        NewDocument,
        CloseTab,
        ToggleTheme,
        ToggleSidebar,
        ToggleOutline,
        CommandPalette,
        ExportHtml,
        LoadRemoteImages,
        Undo,
        Redo,
        ToggleEditorMode
    ]
);

pub struct MarkRustWindow {
    pub workspace: Entity<Workspace>,
    pub palette_query: String,
    pub palette_selection: usize,
    pub focus_handle: FocusHandle,
}

impl MarkRustWindow {
    pub fn new(workspace: Entity<Workspace>, cx: &mut Context<Self>) -> Self {
        Self {
            workspace,
            palette_query: String::new(),
            palette_selection: 0,
            focus_handle: cx.focus_handle(),
        }
    }

    fn save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        let candidates = self.workspace.read(cx).normalize_candidates(cx);
        let needs_review = candidates
            .as_ref()
            .is_some_and(should_offer_normalize_review);
        if needs_review {
            let preview = candidates
                .as_ref()
                .map(|c| c.hunk_preview(20))
                .unwrap_or_default();
            let message = if preview.is_empty() {
                "Keep original writes the buffer as-is. Normalize rewrites to house style. Cancel aborts the save.".to_string()
            } else {
                format!(
                    "Keep original writes the buffer as-is. Normalize rewrites to house style. Cancel aborts the save.\n\n{preview}"
                )
            };
            let receiver = window.prompt(
                PromptLevel::Info,
                "Normalize markdown on save?",
                Some(message.as_str()),
                &[
                    PromptButton::ok("Keep original"),
                    PromptButton::new("Normalize"),
                    PromptButton::cancel("Cancel"),
                ],
                cx,
            );
            let workspace = self.workspace.clone();
            cx.spawn_in(window, async move |_, cx| {
                let choice = match receiver.await {
                    Ok(0) => NormalizeReviewChoice::KeepOriginal,
                    Ok(1) => NormalizeReviewChoice::Normalize,
                    _ => NormalizeReviewChoice::Cancel,
                };
                let _ = workspace.update_in(cx, |workspace, window, cx| {
                    let _ =
                        workspace.dispatch(WorkspaceCommand::SaveWithReview(choice), window, cx);
                });
            })
            .detach();
            return;
        }
        self.workspace.update(cx, |workspace, cx| {
            let _ = workspace.dispatch(WorkspaceCommand::Save, window, cx);
        });
    }

    fn open_file(&mut self, _: &OpenFile, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open Markdown file".into()),
        });
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.into_iter().next() {
                    let _ = workspace.update_in(cx, |workspace, window, cx| {
                        let _ = workspace.dispatch(WorkspaceCommand::OpenFile(path), window, cx);
                    });
                }
            }
        })
        .detach();
    }

    fn open_folder(&mut self, _: &OpenFolder, _: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open workspace folder".into()),
        });
        let workspace = self.workspace.clone();
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.into_iter().next() {
                    workspace.update(cx, |workspace, cx| {
                        // OpenFolder does not need a Window; dispatch via a dummy path command.
                        let _ = workspace.open_workspace(path, cx);
                    });
                }
            }
        })
        .detach();
    }

    fn new_document(&mut self, _: &NewDocument, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            let _ = workspace.dispatch(WorkspaceCommand::NewDocument, window, cx);
        });
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            let _ = workspace.dispatch(WorkspaceCommand::CloseTab, window, cx);
        });
    }

    fn toggle_editor_mode(
        &mut self,
        _: &ToggleEditorMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.toggle_editor_mode(cx);
        });
        cx.notify();
    }

    fn toggle_theme(&mut self, _: &ToggleTheme, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            let _ = workspace.dispatch(WorkspaceCommand::ToggleTheme, window, cx);
        });
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.sidebar_open = !workspace.sidebar_open;
            cx.notify();
        });
    }

    fn toggle_outline(&mut self, _: &ToggleOutline, _: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.outline_open = !workspace.outline_open;
            cx.notify();
        });
    }

    fn command_palette(&mut self, _: &CommandPalette, _: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.palette_open = !workspace.palette_open;
            cx.notify();
        });
        if self.workspace.read(cx).palette_open {
            self.palette_query.clear();
            self.palette_selection = 0;
        }
    }

    fn export_html(&mut self, _: &ExportHtml, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            match workspace.dispatch(WorkspaceCommand::ExportHtml { output: None }, window, cx) {
                Ok(()) => {}
                Err(error) => eprintln!("Export failed: {error}"),
            }
        });
    }

    fn load_remote_images(
        &mut self,
        _: &LoadRemoteImages,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.load_remote_images(cx);
        });
    }

    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            let _ = workspace.dispatch(
                WorkspaceCommand::Editor(markrust_editor::EditorCommand::Undo),
                window,
                cx,
            );
        });
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
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

impl Render for MarkRustWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        let mode_label = match workspace.active_tab().map(|tab| tab.mode) {
            Some(crate::workspace::EditorMode::Wysiwyg) => "Wysiwyg",
            Some(crate::workspace::EditorMode::Split) => "Split",
            _ => "Source",
        };
        let active = workspace.active_tab;
        let tab_count = workspace.tabs.len();
        let files = workspace.list_files();
        let recent = workspace.recent.clone();
        let palette_open = workspace.palette_open;
        let sidebar_open = workspace.sidebar_open;
        let outline_open = workspace.outline_open;
        let external_change = workspace.pending_external_change.clone();
        let source_mode = matches!(
            workspace.active_tab().map(|tab| tab.mode),
            Some(crate::workspace::EditorMode::Source)
        );
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

        let ws_drop = workspace_entity.clone();
        let ws_editor_drop = workspace_entity.clone();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.chrome_bg)
            .text_color(theme.text)
            .font_family(theme.font_family.clone())
            .text_size(px(14.))
            .track_focus(&self.focus_handle)
            .key_context("MarkRust")
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::open_file))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(Self::new_document))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::toggle_theme))
            .on_action(cx.listener(Self::toggle_editor_mode))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_outline))
            .on_action(cx.listener(Self::command_palette))
            .on_action(cx.listener(Self::export_html))
            .on_action(cx.listener(Self::load_remote_images))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_drop(cx.listener({
                let ws = ws_drop.clone();
                move |_, paths: &ExternalPaths, window, cx| {
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
            .children(external_change.map(|(index, path)| {
                let ws = workspace_entity.clone();
                let dirty = workspace
                    .tabs
                    .get(index)
                    .map(|tab| tab.document.read(cx).dirty)
                    .unwrap_or(false);
                let banner = if dirty {
                    format!(
                        "File changed on disk and this tab has unsaved edits: {}. Click to reload (your edits will be lost).",
                        path.display()
                    )
                } else {
                    format!("File changed on disk: {}. Click to reload.", path.display())
                };
                div()
                    .px_4()
                    .py_2()
                    .bg(theme.accent.opacity(0.85))
                    .text_color(theme.sidebar_selected_text)
                    .text_sm()
                    .child(banner)
                    .cursor_pointer()
                    .id("external-change-banner")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        ws.update(cx, |workspace, cx| {
                            let _ = workspace.reload_tab(index, cx);
                        });
                    }))
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .px_3()
                    .py_2()
                    .gap_1()
                    .bg(theme.chrome_bg)
                    .border_b_1()
                    .border_color(theme.separator)
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .mr_4()
                            .child("MarkRust"),
                    )
                    .child(toolbar_button(
                        "New",
                        &theme,
                        "toolbar-new",
                        cx.listener(|this, _, window, cx| {
                            this.new_document(&NewDocument, window, cx)
                        }),
                    ))
                    .child(toolbar_button(
                        "Open File",
                        &theme,
                        "toolbar-open-file",
                        cx.listener(|this, _, window, cx| this.open_file(&OpenFile, window, cx)),
                    ))
                    .child(toolbar_button(
                        "Open Folder",
                        &theme,
                        "toolbar-open-folder",
                        cx.listener(|this, _, window, cx| {
                            this.open_folder(&OpenFolder, window, cx)
                        }),
                    ))
                    .child(toolbar_button(
                        "Save",
                        &theme,
                        "toolbar-save",
                        cx.listener(|this, _, window, cx| this.save(&Save, window, cx)),
                    ))
                    .child(toolbar_button(
                        "Theme",
                        &theme,
                        "toolbar-theme",
                        cx.listener(|this, _, window, cx| {
                            this.toggle_theme(&ToggleTheme, window, cx)
                        }),
                    ))
                    .child(toolbar_button(
                        mode_label,
                        &theme,
                        "toolbar-editor-mode",
                        cx.listener(|this, _, window, cx| {
                            this.toggle_editor_mode(&ToggleEditorMode, window, cx)
                        }),
                    ))
                    .child(toolbar_button(
                        "Load images",
                        &theme,
                        "toolbar-load-remote-images",
                        cx.listener(|this, _, window, cx| {
                            this.load_remote_images(&LoadRemoteImages, window, cx)
                        }),
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_end()
                    .h(px(36.))
                    .gap_px()
                    .px_2()
                    .bg(theme.tab_inactive)
                    .border_b_1()
                    .border_color(theme.separator)
                    .children((0..tab_count).map(|index| {
                        let tab = &workspace.tabs[index];
                        let dirty = tab.document.read(cx).dirty;
                        let label = if dirty {
                            format!("{} •", tab.title)
                        } else {
                            tab.title.clone()
                        };
                        let ws = workspace_entity.clone();
                        let ws_close = workspace_entity.clone();
                        document_tab(
                            label,
                            &theme,
                            index == active,
                            SharedString::from(format!("tab-{index}")),
                            cx.listener(move |_, _, _, cx| {
                                ws.update(cx, |workspace, cx| {
                                    workspace.active_tab = index;
                                    cx.notify();
                                });
                            }),
                            cx.listener(move |_, _, window, cx| {
                                ws_close.update(cx, |workspace, cx| {
                                    workspace.close_tab(index, window, cx);
                                });
                            }),
                        )
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(if sidebar_open {
                        div()
                            .w(px(260.))
                            .h_full()
                            .id("sidebar")
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
                                    .child(section_header(format!("Files · {folder_name}"), &theme))
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
                                    .child(empty_sidebar_state(
                                        &theme,
                                        cx.listener(|this, _, window, cx| {
                                            this.open_folder(&OpenFolder, window, cx)
                                        }),
                                        cx.listener(|this, _, window, cx| {
                                            this.open_file(&OpenFile, window, cx)
                                        }),
                                    ))
                                    .when(!recent.workspaces.is_empty(), |panel| {
                                        panel.child(section_header("Recent", &theme)).children(
                                            recent
                                                .workspaces
                                                .iter()
                                                .enumerate()
                                                .map(|(recent_index, path)| {
                                                    let label = path.display().to_string();
                                                    let path = path.clone();
                                                    let ws = workspace_entity.clone();
                                                    sidebar_row(
                                                        label,
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
                    } else {
                        div().w(px(0.)).id("sidebar-closed")
                    })
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .id("editor-area")
                            .flex()
                            .flex_col()
                            .overflow_y_scroll()
                            .bg(theme.editor_bg)
                            .on_drop(cx.listener({
                                let ws = ws_editor_drop.clone();
                                move |_, paths: &ExternalPaths, window, cx| {
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
                                let tab =
                                    workspace.active_tab().unwrap_or_else(|| &workspace.tabs[0]);
                                match tab.mode {
                                    crate::workspace::EditorMode::Wysiwyg => div()
                                        .flex_1()
                                        .p(px(24.))
                                        .child(tab.rich_view.clone())
                                        .into_any_element(),
                                    crate::workspace::EditorMode::Source => div()
                                        .flex_1()
                                        .p(px(24.))
                                        .child(tab.editor_view.clone())
                                        .into_any_element(),
                                    crate::workspace::EditorMode::Split => div()
                                        .flex_1()
                                        .flex()
                                        .flex_row()
                                        .overflow_hidden()
                                        .child(
                                            div()
                                                .id("split-source")
                                                .flex_1()
                                                .min_w(px(120.))
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
                                                .min_w(px(120.))
                                                .overflow_hidden()
                                                .child(tab.rich_view.clone()),
                                        )
                                        .into_any_element(),
                                }
                            }),
                    )
                    .child(if outline_open {
                        div()
                            .w(px(240.))
                            .h_full()
                            .id("outline-panel")
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
                                            workspace.dispatch(
                                                WorkspaceCommand::JumpToHeading { offset },
                                                window,
                                                cx,
                                            )
                                        });
                                    }),
                                )
                            }))
                    } else {
                        div().w(px(0.)).id("outline-closed")
                    }),
            )
            .child({
                let tab = workspace.active_tab();
                let path = tab
                    .and_then(|t| t.document.read(cx).path.clone())
                    .map(|p| status_path_label(&p))
                    .unwrap_or_else(|| "Untitled".into());
                let dirty = tab.map(|t| t.document.read(cx).dirty).unwrap_or(false);
                let words = tab.map(|t| t.document.read(cx).word_count()).unwrap_or(0);
                let (line, col) = tab
                    .map(|t| {
                        let doc = t.document.read(cx);
                        let offset = t.editor.read(cx).cursor_offset();
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
                    .child(format!("{}{}", path, if dirty { " — edited" } else { "" }))
                    .child(format!(
                        "Ln {}, Col {}  ·  {words} words{frontmatter_label}",
                        line + 1,
                        col + 1
                    ))
            })
            .child(if palette_open {
                let query = self.palette_query.clone();
                let mut commands = Vec::new();
                if fuzzy_match("Export HTML", &query) {
                    commands.push("Export HTML".to_string());
                }
                if fuzzy_match("Load remote images", &query) {
                    commands.push("Load remote images".to_string());
                }
                commands.extend((0..tab_count).filter_map(|index| {
                    let title = workspace.tabs[index].title.clone();
                    fuzzy_match(&title, &query).then_some(title)
                }));
                let selected = self.palette_selection.min(commands.len().saturating_sub(1));
                let ws = workspace_entity.clone();
                div()
                    .absolute()
                    .top(px(96.))
                    .left(px(240.))
                    .w(px(440.))
                    .rounded_lg()
                    .shadow_lg()
                    .bg(theme.tab_active)
                    .border_1()
                    .border_color(theme.separator)
                    .p_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.secondary_text)
                            .child(SharedString::from(format!("> {query}"))),
                    )
                    .children(commands.iter().enumerate().map(|(index, title)| {
                        let ws = ws.clone();
                        let title = title.clone();
                        div()
                            .text_sm()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(if index == selected {
                                theme.sidebar_selected
                            } else {
                                theme.tab_active
                            })
                            .cursor_pointer()
                            .child(title.clone())
                            .id(("palette-item", index))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                if title == "Export HTML" {
                                    if let Ok(path) = ws.read(cx).export_active_html(cx) {
                                        eprintln!("Exported HTML to {}", path.display());
                                    }
                                } else if title == "Load remote images" {
                                    ws.update(cx, |workspace, cx| {
                                        workspace.load_remote_images(cx);
                                    });
                                }
                                ws.update(cx, |workspace, cx| {
                                    workspace.palette_open = false;
                                    cx.notify();
                                });
                            }))
                    }))
            } else {
                div().hidden()
            })
    }
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
