// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use gpui::{
    div, prelude::*, px, App, ClickEvent, FontWeight, InteractiveElement, IntoElement,
    SharedString, Window,
};
use markrust_editor::EditorTheme;

/// macOS-style toolbar text button.
pub fn toolbar_button(
    label: impl Into<SharedString>,
    theme: &EditorTheme,
    id: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id.into())
        .px_2()
        .py_1()
        .mx_1()
        .rounded_md()
        .text_sm()
        .text_color(theme.sidebar_text)
        .cursor_pointer()
        .hover(move |s| s.bg(theme.toolbar_button_hover))
        .child(label.into())
        .on_click(on_click)
}

/// Uppercase section header for sidebar panels.
pub fn section_header(title: impl Into<SharedString>, theme: &EditorTheme) -> impl IntoElement {
    div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.secondary_text)
        .child(title.into())
}

/// Sidebar file or recent-workspace row.
pub fn sidebar_row(
    label: impl Into<SharedString>,
    theme: &EditorTheme,
    selected: bool,
    id: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme.clone();
    div()
        .id(id.into())
        .mx_2()
        .px_3()
        .py_1()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .text_color(if selected {
            theme.sidebar_selected_text
        } else {
            theme.sidebar_text
        })
        .when(selected, |row| row.bg(theme.sidebar_selected))
        .when(!selected, |row| row.hover(move |s| s.bg(theme.sidebar_hover)))
        .child(label.into())
        .on_click(on_click)
}

/// Outline heading row with indentation by level.
pub fn outline_row(
    title: impl Into<SharedString>,
    level: u8,
    theme: &EditorTheme,
    id: SharedString,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme.clone();
    let pad = (level.saturating_sub(1) as f32) * 12.0 + 8.0;
    div()
        .id(id)
        .pl(px(pad))
        .pr_3()
        .py_1()
        .mx_2()
        .rounded_md()
        .text_sm()
        .text_color(theme.sidebar_text)
        .cursor_pointer()
        .hover(move |s| s.bg(theme.sidebar_hover))
        .child(title.into())
        .on_click(on_click)
}

/// Document tab with rounded top corners (macOS style).
pub fn document_tab(
    label: impl Into<SharedString>,
    theme: &EditorTheme,
    active: bool,
    id: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_close: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme.clone();
    let bg = if active {
        theme.tab_active
    } else {
        theme.tab_inactive
    };
    let id_string: SharedString = id.into();
    let close_id = SharedString::from(format!("{id_string}-close"));
    div()
        .id(id_string)
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .mt_1()
        .rounded_tl(px(6.))
        .rounded_tr(px(6.))
        .bg(bg)
        .cursor_pointer()
        .child(
            div()
                .text_sm()
                .text_color(if active {
                    theme.text
                } else {
                    theme.secondary_text
                })
                .child(label.into()),
        )
        .child(
            div()
                .id(close_id)
                .text_xs()
                .text_color(theme.secondary_text)
                .px_1()
                .rounded_sm()
                .hover(move |s| s.bg(theme.toolbar_button_hover))
                .child("×")
                .on_click(on_close),
        )
        .on_click(on_click)
}

/// Welcome panel shown when no workspace folder is open.
pub fn empty_sidebar_state(
    theme: &EditorTheme,
    on_open_folder: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_open_file: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme_clone = theme.clone();
    div()
        .flex()
        .flex_col()
        .flex_1()
        .p_4()
        .gap_3()
        .child(
            div()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child("Welcome to MarkRust"),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.secondary_text)
                .child("Open a folder or file to get started, or drop items onto the window."),
        )
        .child(
            div()
                .flex()
                .gap_2()
                .child(
                    div()
                        .id("empty-open-folder")
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(theme.accent)
                        .text_sm()
                        .text_color(theme.sidebar_selected_text)
                        .cursor_pointer()
                        .child("Open Folder")
                        .on_click(on_open_folder),
                )
                .child(
                    div()
                        .id("empty-open-file")
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.separator)
                        .text_sm()
                        .text_color(theme.sidebar_text)
                        .cursor_pointer()
                        .hover(move |s| s.bg(theme_clone.toolbar_button_hover))
                        .child("Open File")
                        .on_click(on_open_file),
                ),
        )
}
