// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A bounded, out-of-flow image inspector. Drafts never modify the document
//! until Apply; previews use the same approved cache boundary as body images.

use std::{
    collections::BTreeMap,
    fs,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::{
    canvas, div, img, point, prelude::*, px, size, App, Bounds, Context, Entity,
    EntityInputHandler, FocusHandle, Focusable, MouseButton, ObjectFit, PathPromptOptions, Pixels,
    Render, Role, ScrollHandle, Subscription, Task, WeakEntity, Window,
};
use markrust_core::Document;

use super::image::{
    cache_path_for_url, default_image_cache_dir, fetch_remote_image, materialize_safe_data_image,
    materialize_safe_local_image, resolve_image_source, ResolvedImage,
};
use super::view::RichEditorView;
use crate::{
    headless::{EditorCommand, EditorOutcome},
    source::{editor::MarkdownEditor, element::MarkdownEditorView},
    theme::EditorTheme,
};

#[derive(Clone)]
pub(super) struct ImageEditTarget {
    pub range: Range<usize>,
    pub source: String,
    pub existing: bool,
    pub alt: String,
    pub url: String,
}

pub(super) struct ImageEditor {
    owner: WeakEntity<RichEditorView>,
    target: ImageEditTarget,
    base_dir: Option<PathBuf>,
    theme: EditorTheme,
    url_document: Entity<Document>,
    alt_document: Entity<Document>,
    url_editor: Entity<MarkdownEditor>,
    alt_editor: Entity<MarkdownEditor>,
    url_view: Entity<MarkdownEditorView>,
    alt_view: Entity<MarkdownEditorView>,
    preview: Option<PathBuf>,
    preview_status: String,
    preview_generation: u64,
    remote_authorized: bool,
    nearby_files: Vec<PathBuf>,
    error: Option<String>,
    focused_alt: bool,
    apply_bounds: Option<Bounds<Pixels>>,
    cancel_bounds: Option<Bounds<Pixels>>,
    preview_bounds: Option<Bounds<Pixels>>,
    preview_status_bounds: Option<Bounds<Pixels>>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
    _preview_task: Option<Task<()>>,
    _directory_task: Option<Task<()>>,
}

impl ImageEditor {
    pub(super) fn set_ui_strings(
        &mut self,
        strings: Arc<BTreeMap<String, String>>,
        cx: &mut Context<Self>,
    ) {
        self.theme.ui_strings = strings.clone();
        for editor in [&self.url_editor, &self.alt_editor] {
            editor.update(cx, |editor, cx| {
                editor.theme.ui_strings = strings.clone();
                cx.notify();
            });
        }
        cx.notify();
    }

    pub fn new(
        owner: WeakEntity<RichEditorView>,
        target: ImageEditTarget,
        base_dir: Option<PathBuf>,
        remote_authorized: bool,
        theme: EditorTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let url_document = cx.new(|_| Document::plain_text(&target.url));
        let alt_document = cx.new(|_| Document::plain_text(&target.alt));
        let mut input_theme = theme.clone();
        input_theme.font_size = 14.;
        input_theme.background = theme.editor_bg;
        let url_editor =
            cx.new(|cx| MarkdownEditor::new(url_document.clone(), input_theme.clone(), window, cx));
        let alt_editor =
            cx.new(|cx| MarkdownEditor::new(alt_document.clone(), input_theme.clone(), window, cx));
        let url_view = cx.new(|_| MarkdownEditorView::new(url_editor.clone()));
        let alt_view = cx.new(|_| MarkdownEditorView::new(alt_editor.clone()));
        let subscription = cx.observe(&url_document, |this, _, cx| {
            this.error = None;
            this.remote_authorized = false;
            this.publish_recovery(cx);
            this.refresh_preview(cx);
        });
        let alt_subscription = cx.observe(&alt_document, |this, _, cx| this.publish_recovery(cx));
        let url_focus = cx.on_focus(
            &url_editor.read(cx).focus_handle.clone(),
            window,
            |this, _, _| this.focused_alt = false,
        );
        let alt_focus = cx.on_focus(
            &alt_editor.read(cx).focus_handle.clone(),
            window,
            |this, _, _| this.focused_alt = true,
        );
        url_editor.read(cx).focus_handle.clone().focus(window, cx);
        let mut this = Self {
            owner,
            target,
            base_dir,
            theme,
            url_document,
            alt_document,
            url_editor,
            alt_editor,
            url_view,
            alt_view,
            preview: None,
            preview_status: "Choose an image or enter its location.".into(),
            preview_generation: 0,
            remote_authorized,
            nearby_files: Vec::new(),
            error: None,
            focused_alt: false,
            apply_bounds: None,
            cancel_bounds: None,
            preview_bounds: None,
            preview_status_bounds: None,
            scroll: ScrollHandle::new(),
            _subscriptions: vec![subscription, alt_subscription, url_focus, alt_focus],
            _preview_task: None,
            _directory_task: None,
        };
        this.refresh_preview(cx);
        if let Some(directory) = this.base_dir.clone() {
            this._directory_task = Some(cx.spawn(async move |this, cx| {
                let files = cx
                    .background_executor()
                    .spawn(async move { nearby_image_files(&directory) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.nearby_files = files;
                    cx.notify();
                });
            }));
        }
        this
    }

    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_generation += 1;
        let generation = self.preview_generation;
        let url = self.url_document.read(cx).buffer.content();
        let base_dir = self.base_dir.clone();
        let remote = self.remote_authorized;
        self.preview = None;
        self.preview_status = "Loading preview…".into();
        self._preview_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { safe_preview_path(base_dir.as_deref(), &url, remote) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.preview_generation {
                    return;
                }
                match result {
                    Ok(path) => {
                        this.preview = Some(path);
                        this.preview_status = "Preview".into();
                    }
                    Err(message) => {
                        this.preview = None;
                        this.preview_status = message;
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn publish_recovery(&self, cx: &mut Context<Self>) {
        let url = self.url_document.read(cx).buffer.content();
        let alt = self.alt_document.read(cx).buffer.content();
        if let Some(owner) = self.owner.upgrade() {
            owner.update(cx, |owner, cx| {
                owner.update_image_recovery_fields(url, alt, cx)
            });
        }
    }

    pub(super) fn undo_or_redo(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> EditorOutcome {
        let editor = if self.focused_alt {
            &self.alt_editor
        } else {
            &self.url_editor
        };
        editor.update(cx, |editor, cx| editor.apply_command(command, cx))
    }

    pub(super) fn remember_input_owner(&mut self, window: &Window, cx: &App) {
        if self.alt_editor.read(cx).focus_handle.is_focused(window) {
            self.focused_alt = true;
        } else if self.url_editor.read(cx).focus_handle.is_focused(window) {
            self.focused_alt = false;
        }
    }

    pub(super) fn focus_current_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_input_owner(window, cx);
        let editor = if self.focused_alt {
            &self.alt_editor
        } else {
            &self.url_editor
        };
        editor.read(cx).focus_handle.clone().focus(window, cx);
    }

    pub(super) fn input_is_focused(&self, window: &Window, cx: &App) -> bool {
        self.url_editor.read(cx).focus_handle.is_focused(window)
            || self.alt_editor.read(cx).focus_handle.is_focused(window)
    }

    pub(super) fn paste_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_input_owner(window, cx);
        let editor = if self.focused_alt {
            &self.alt_editor
        } else {
            &self.url_editor
        };
        editor.update(cx, |editor, cx| {
            editor.replace_text_in_range(None, text, window, cx)
        });
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_focused_field(&self, window: &Window, cx: &App) -> Option<&'static str> {
        if self.url_editor.read(cx).focus_handle.is_focused(window) {
            Some("location")
        } else if self.alt_editor.read(cx).focus_handle.is_focused(window) {
            Some("alt")
        } else {
            None
        }
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_action_bounds(&self) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
        Some((self.apply_bounds?, self.cancel_bounds?))
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_preview_status_bounds(&self) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
        Some((self.preview_bounds?, self.preview_status_bounds?))
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_scroll_state(&self) -> super::view::ImageInspectorScrollState {
        (
            self.scroll.bounds(),
            self.scroll.offset(),
            self.preview_bounds,
        )
    }

    fn set_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.focused_alt = false;
        let destination = image_destination_for_file(self.base_dir.as_deref(), &path);
        self.url_editor.update(cx, |editor, cx| {
            editor.apply_command(EditorCommand::SelectAll, cx);
            editor.apply_command(EditorCommand::InsertText(destination), cx);
            editor.focus_handle.focus(window, cx);
        });
    }

    fn choose_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose an image".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update_in(cx, |this, window, cx| this.set_file(path, window, cx));
                }
            }
        })
        .detach();
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = self.url_document.read(cx).buffer.content();
        let alt = self.alt_document.read(cx).buffer.content();
        if let Err(message) = validate_image_fields(&alt, &url) {
            self.error = Some(message);
            cx.notify();
            return;
        }
        if let Some(owner) = self.owner.upgrade() {
            if let Err(message) = owner.update(cx, |owner, cx| {
                owner.apply_image_panel(&self.target, &alt, url.trim(), window, cx)
            }) {
                self.error = Some(message);
                cx.notify();
            }
        }
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(owner) = self.owner.upgrade() {
            owner.update(cx, |owner, cx| owner.close_image_panel(window, cx));
        }
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_state(&self, cx: &App) -> (String, String, bool) {
        (
            self.url_document.read(cx).buffer.content(),
            self.alt_document.read(cx).buffer.content(),
            self.preview.is_some(),
        )
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_set_fields(&mut self, alt: &str, url: &str, cx: &mut Context<Self>) {
        for (editor, text) in [
            (self.alt_editor.clone(), alt),
            (self.url_editor.clone(), url),
        ] {
            editor.update(cx, |editor, cx| {
                editor.apply_command(EditorCommand::SelectAll, cx);
                editor.apply_command(EditorCommand::InsertText(text.to_owned()), cx);
            });
        }
    }
}

impl Focusable for ImageEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.url_editor.read(cx).focus_handle.clone()
    }
}

impl Render for ImageEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        let cancel = cx.entity();
        let apply = cx.entity();
        let enter = cx.entity();
        let escape = cx.entity();
        let choose = cx.entity();
        let load = cx.entity();
        let tab = cx.entity();
        let shift_tab = cx.entity();
        let apply_geometry = cx.entity();
        let cancel_geometry = cx.entity();
        let preview_geometry_owner = cx.entity();
        let preview_status_geometry_owner = cx.entity();
        self.apply_bounds = None;
        self.cancel_bounds = None;
        self.preview_bounds = None;
        self.preview_status_bounds = None;
        let remote_url = super::image::is_http_url(&self.url_document.read(cx).buffer.content());
        div()
            .id("image-editor")
            .accessibility_id("image-editor")
            .role(Role::Group)
            .aria_label(theme.ui_text("Image editor"))
            .size_full()
            .min_h_0()
            .p(px(14.))
            .rounded_lg()
            .border_1()
            .border_color(theme.separator)
            .bg(theme.sidebar_bg)
            .flex()
            .flex_col()
            .gap(px(8.))
            .cursor(gpui::CursorStyle::Arrow)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            // The middle may scroll first, but never bubble its wheel event
            // into the virtualized Markdown document behind this inspector.
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .capture_action(move |_: &crate::editor::Enter, window, cx| {
                enter.update(cx, |this, cx| this.apply(window, cx));
                cx.stop_propagation();
            })
            .capture_action(move |_: &crate::editor::Escape, window, cx| {
                escape.update(cx, |this, cx| this.cancel(window, cx));
                cx.stop_propagation();
            })
            .capture_action(move |_: &crate::editor::Indent, window, cx| {
                tab.update(cx, |this, cx| {
                    this.focused_alt = this.url_editor.read(cx).focus_handle.is_focused(window);
                    let field = if this.focused_alt {
                        &this.alt_editor
                    } else {
                        &this.url_editor
                    };
                    field.read(cx).focus_handle.clone().focus(window, cx);
                });
                cx.stop_propagation();
            })
            .capture_action(move |_: &crate::editor::Outdent, window, cx| {
                shift_tab.update(cx, |this, cx| {
                    this.focused_alt = !this.alt_editor.read(cx).focus_handle.is_focused(window);
                    let field = if this.focused_alt {
                        &this.alt_editor
                    } else {
                        &this.url_editor
                    };
                    field.read(cx).focus_handle.clone().focus(window, cx);
                });
                cx.stop_propagation();
            })
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(theme.text)
                    .child(theme.ui_text(if self.target.existing {
                        "Edit image"
                    } else {
                        "Insert image"
                    })),
            )
            .child(
                div()
                    .id("image-inspector-scroll")
                    .min_h_0()
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .child(field(
                                "image-location-field",
                                "Image location · relative path or HTTPS URL",
                                self.url_view.clone(),
                                &theme,
                            ))
                            .child(field(
                                "image-alt-field",
                                "Alternative text",
                                self.alt_view.clone(),
                                &theme,
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap(px(12.))
                                    .flex_shrink_0()
                                    .child(
                                        button("image-choose-file", "Choose file…", &theme)
                                            .on_click(move |_, window, cx| {
                                                choose.update(cx, |this, cx| {
                                                    this.choose_file(window, cx)
                                                })
                                            }),
                                    )
                                    .when(remote_url && !self.remote_authorized, |row| {
                                        row.child(
                                            button(
                                                "image-load-preview",
                                                "Load remote preview",
                                                &theme,
                                            )
                                            .on_click(
                                                move |_, _, cx| {
                                                    load.update(cx, |this, cx| {
                                                        this.remote_authorized = true;
                                                        this.refresh_preview(cx);
                                                    })
                                                },
                                            ),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .id("image-preview")
                                    .w_full()
                                    .min_w_0()
                                    .h(px(150.))
                                    .flex_shrink_0()
                                    .rounded_md()
                                    .bg(theme.editor_bg)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .relative()
                                    .child(preview_geometry(preview_geometry_owner, false))
                                    .when_some(self.preview.clone(), |el, path| {
                                        let decode_error =
                                            theme.ui_text("Could not decode this image.");
                                        el.child(
                                            img(path)
                                                .w_full()
                                                .min_w_0()
                                                .max_w_full()
                                                .max_h(px(150.))
                                                .object_fit(ObjectFit::Contain)
                                                .with_fallback(move || {
                                                    div()
                                                        .child(decode_error.clone())
                                                        .into_any_element()
                                                }),
                                        )
                                    })
                                    .when(self.preview.is_none(), |el| {
                                        el.child(
                                            div()
                                                .w_full()
                                                .min_w_0()
                                                .px(px(8.))
                                                .whitespace_normal()
                                                .text_center()
                                                .text_sm()
                                                .text_color(theme.secondary_text)
                                                .child(self.preview_status.clone())
                                                .relative()
                                                .child(preview_geometry(
                                                    preview_status_geometry_owner,
                                                    true,
                                                )),
                                        )
                                    }),
                            )
                            .when(!self.nearby_files.is_empty(), |el| {
                                el.child(
                                    div()
                                        .pt(px(8.))
                                        .text_xs()
                                        .text_color(theme.secondary_text)
                                        .child(theme.ui_text("Images in this document’s folder")),
                                )
                            })
                            .children(self.nearby_files.iter().take(16).enumerate().map(
                                |(index, path)| {
                                    let editor = cx.entity();
                                    let path = path.clone();
                                    let label = path
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .into_owned();
                                    div()
                                        .id(("image-nearby", index))
                                        .py(px(3.))
                                        .text_sm()
                                        .text_color(theme.accent)
                                        .cursor(gpui::CursorStyle::PointingHand)
                                        .child(label)
                                        .on_click(move |_, window, cx| {
                                            editor.update(cx, |this, cx| {
                                                this.set_file(path.clone(), window, cx)
                                            })
                                        })
                                },
                            ))
                            .children(self.error.clone().map(|message| {
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(theme.secondary_text)
                                    .child(message)
                            })),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .gap(px(14.))
                    .child(
                        button("image-editor-cancel", "Cancel", &theme)
                            .relative()
                            .child(action_geometry(cancel_geometry, false))
                            .on_click(move |_, window, cx| {
                                cancel.update(cx, |this, cx| this.cancel(window, cx))
                            }),
                    )
                    .child(
                        button("image-editor-apply", "Apply", &theme)
                            .relative()
                            .child(action_geometry(apply_geometry, true))
                            .on_click(move |_, window, cx| {
                                apply.update(cx, |this, cx| this.apply(window, cx))
                            }),
                    ),
            )
    }
}

fn action_geometry(editor: Entity<ImageEditor>, apply: bool) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), _, cx| {
            editor.update(cx, |editor, _| {
                if apply {
                    editor.apply_bounds = Some(bounds);
                } else {
                    editor.cancel_bounds = Some(bounds);
                }
            });
        },
    )
    .absolute()
    .inset_0()
}

fn preview_geometry(editor: Entity<ImageEditor>, status: bool) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), _, cx| {
            editor.update(cx, |editor, _| {
                if status {
                    editor.preview_status_bounds = Some(bounds);
                } else {
                    editor.preview_bounds = Some(bounds);
                }
            });
        },
    )
    .absolute()
    .inset_0()
}

fn button(id: &'static str, label: &'static str, theme: &EditorTheme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .accessibility_id(id)
        .role(Role::Button)
        .aria_label(theme.ui_text(label))
        .text_sm()
        .text_color(theme.accent)
        .cursor(gpui::CursorStyle::PointingHand)
        .child(theme.ui_text(label))
}

fn field(
    id: &'static str,
    label: &'static str,
    view: Entity<MarkdownEditorView>,
    theme: &EditorTheme,
) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap(px(3.))
        .child(
            div()
                .text_xs()
                .text_color(theme.secondary_text)
                .child(theme.ui_text(label)),
        )
        .child(
            div()
                .id(id)
                .accessibility_id(id)
                .role(Role::TextInput)
                .aria_label(theme.ui_text(label))
                .w_full()
                .h(px(38.))
                .border_1()
                .border_color(theme.separator)
                .rounded_md()
                .overflow_hidden()
                .child(view),
        )
}

pub(super) fn image_editor_placement(viewport: Bounds<Pixels>) -> Bounds<Pixels> {
    let margin = px(12.)
        .min(viewport.size.width / 4.)
        .min(viewport.size.height / 4.);
    let dimensions = size(
        (viewport.size.width - margin * 2.)
            .min(px(540.))
            .max(px(1.)),
        (viewport.size.height - margin * 2.)
            .min(px(500.))
            .max(px(1.)),
    );
    Bounds::new(
        point(
            viewport.origin.x + (viewport.size.width - dimensions.width) / 2.,
            viewport.origin.y + (viewport.size.height - dimensions.height) / 2.,
        ),
        dimensions,
    )
}

pub(super) fn validate_image_fields(alt: &str, url: &str) -> Result<(), String> {
    if alt.chars().any(char::is_control) || url.chars().any(char::is_control) {
        return Err("Image fields must be a single line.".into());
    }
    if url.trim().is_empty() {
        return Err("Choose a file or enter an image location.".into());
    }
    if matches!(resolve_image_source(None, url), ResolvedImage::Blocked) {
        return Err("Use a local file or a public HTTPS image URL.".into());
    }
    if super::image::is_http_url(url) {
        super::image::validate_remote_image_url(url).map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub(super) fn markdown_image_text(alt: &str, url: &str) -> String {
    let alt = markrust_core::rich::escape::escape_text(
        alt,
        markrust_core::rich::escape::EscapeContext {
            in_table: false,
            at_line_start: false,
        },
    );
    let destination = url
        .replace('&', "&amp;")
        .replace('\\', "\\\\")
        .replace('<', "\\<")
        .replace('>', "\\>")
        .replace('(', "\\(")
        .replace(')', "\\)");
    format!("![{alt}](<{destination}>)")
}

pub(super) fn encode_image_draft(target: &ImageEditTarget) -> String {
    format!(
        "Image draft ({})\nLocation bytes: {}\n{}\nAlternative text:\n{}",
        if target.existing { "edit" } else { "insert" },
        target.url.len(),
        target.url,
        target.alt
    )
}

pub(super) fn decode_image_draft(draft: &str) -> Option<(bool, String, String)> {
    let (header, rest) = draft.split_once('\n')?;
    let existing = match header {
        "Image draft (edit)" => true,
        "Image draft (insert)" => false,
        _ => return None,
    };
    let (size, rest) = rest.split_once('\n')?;
    let bytes: usize = size.strip_prefix("Location bytes: ")?.parse().ok()?;
    let url = rest.get(..bytes)?.to_string();
    let alt = rest
        .get(bytes..)?
        .strip_prefix("\nAlternative text:\n")?
        .to_string();
    Some((existing, url, alt))
}

fn image_destination_for_file(base_dir: Option<&Path>, path: &Path) -> String {
    let path = base_dir
        .and_then(|base| path.strip_prefix(base).ok())
        .unwrap_or(path);
    path.to_string_lossy().replace('%', "%25")
}

fn nearby_image_files(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .take(1024)
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
            (matches!(
                extension.as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg"
            ) && entry.file_type().ok()?.is_file())
            .then_some(path)
        })
        .collect();
    paths.sort_by_key(|path| {
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase()
    });
    paths.truncate(16);
    paths
}

fn safe_preview_path(
    base_dir: Option<&Path>,
    url: &str,
    authorized: bool,
) -> Result<PathBuf, String> {
    if url.trim().is_empty() {
        return Err("Choose an image or enter its location.".into());
    }
    let cache = default_image_cache_dir();
    match resolve_image_source(base_dir, url) {
        ResolvedImage::Local(path) => materialize_safe_local_image(&path, &cache)
            .map_err(|_| "Image missing, unsupported, or outside the safe size budget.".into()),
        ResolvedImage::Data => materialize_safe_data_image(url, &cache)
            .ok_or_else(|| "Unsupported or unsafe embedded image.".into()),
        ResolvedImage::File(_) => {
            if !authorized {
                return Err(
                    "Remote images stay private until you choose Load remote preview.".into(),
                );
            }
            let dest = cache_path_for_url(&cache, url);
            fetch_remote_image(url, &dest).map_err(|error| error.to_string())?;
            Ok(dest)
        }
        ResolvedImage::Blocked => Err("Use a local file or a public HTTPS image URL.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_local_file_stays_relative_when_it_is_beside_document() {
        assert_eq!(
            image_destination_for_file(
                Some(Path::new("/docs")),
                Path::new("/docs/images/cat photo.png")
            ),
            "images/cat photo.png"
        );
        assert_eq!(
            image_destination_for_file(Some(Path::new("/docs")), Path::new("/other/cat.png")),
            "/other/cat.png"
        );
        assert_eq!(
            image_destination_for_file(Some(Path::new("/docs")), Path::new("/docs/cat%20.png")),
            "cat%2520.png"
        );
    }

    #[test]
    fn panel_fits_small_viewport_without_changing_document_geometry() {
        for (width, height) in [(900., 800.), (280., 320.)] {
            let viewport = Bounds::new(point(px(100.), px(80.)), size(px(width), px(height)));
            let panel = image_editor_placement(viewport);
            assert!(panel.left() >= viewport.left() && panel.right() <= viewport.right());
            assert!(panel.top() >= viewport.top() && panel.bottom() <= viewport.bottom());
        }
    }

    #[test]
    fn preview_does_not_fetch_remote_content_without_explicit_authorization() {
        assert!(
            safe_preview_path(None, "https://example.com/image.png", false)
                .unwrap_err()
                .contains("Remote images stay private")
        );
        assert!(validate_image_fields("alt", "http://example.com/image.png").is_err());
        assert!(validate_image_fields("alt", "https://127.0.0.1/image.png").is_err());
        assert!(validate_image_fields("alt", "https://").is_err());
        assert!(validate_image_fields("alt", "images/cat.png").is_ok());
        assert!(validate_image_fields("alt\ntext", "cat.png").is_err());
    }

    #[test]
    fn inserted_image_escapes_alt_and_path_syntax() {
        let text = markdown_image_text("cat [one]", "images/cat (one).png");
        assert_eq!(text, "![cat \\[one\\]](<images/cat \\(one\\).png>)");
    }

    #[test]
    fn image_recovery_preserves_multiline_and_unicode_fields_losslessly() {
        let target = ImageEditTarget {
            range: 2..8,
            source: "original".into(),
            existing: false,
            url: "фото\nLocation bytes: 3".into(),
            alt: "Alternative text:\nкот 🐈".into(),
        };
        let encoded = encode_image_draft(&target);
        assert_eq!(
            decode_image_draft(&encoded),
            Some((false, target.url, target.alt))
        );
        assert!(decode_image_draft(
            "Image draft (edit)\nLocation bytes: 1\né\nAlternative text:\nalt"
        )
        .is_none());
    }
}
