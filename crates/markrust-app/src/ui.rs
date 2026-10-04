// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::icons::Icon;
use gpui::{
    div, prelude::*, px, App, ClickEvent, Context, FontWeight, InteractiveElement, IntoElement,
    Render, Role, SharedString, Toggled, Window,
};
use markrust_editor::EditorTheme;

/// A visible block-style name, its accessible name, and the command it invokes.
/// Keeping these together prevents a correctly wired heading from displaying
/// an unrelated numeral.
pub struct BlockStyleTool {
    pub icon: Icon,
    pub label: &'static str,
    pub shortcut: &'static str,
    pub id: &'static str,
    pub command: markrust_editor::EditorCommand,
}

pub const BLOCK_STYLE_TOOLS: [BlockStyleTool; 4] = [
    BlockStyleTool {
        icon: Icon::Heading1,
        label: "Heading 1",
        shortcut: "⌘1",
        id: "fmt-h1",
        command: markrust_editor::EditorCommand::SetBlockType(
            markrust_core::rich::BlockType::Heading(1),
        ),
    },
    BlockStyleTool {
        icon: Icon::Heading2,
        label: "Heading 2",
        shortcut: "⌘2",
        id: "fmt-h2",
        command: markrust_editor::EditorCommand::SetBlockType(
            markrust_core::rich::BlockType::Heading(2),
        ),
    },
    BlockStyleTool {
        icon: Icon::Heading3,
        label: "Heading 3",
        shortcut: "⌘3",
        id: "fmt-h3",
        command: markrust_editor::EditorCommand::SetBlockType(
            markrust_core::rich::BlockType::Heading(3),
        ),
    },
    BlockStyleTool {
        icon: Icon::Paragraph,
        label: "Plain paragraph",
        shortcut: "⌘⌥0",
        id: "fmt-paragraph",
        command: markrust_editor::EditorCommand::Paragraph,
    },
];

#[derive(Clone)]
pub enum ToolbarState {
    Action,
    Toggle(bool),
    Mode(bool),
    /// Disabled action button (greyed out + tooltip still visible).
    Disabled,
}

/// Floating compact-window panels paint above the document while keeping
/// their absolute layout out of the document's flex width calculation.
pub fn panel_layer(panel: impl IntoElement, overlay: bool) -> gpui::AnyElement {
    if overlay {
        gpui::deferred(panel).with_priority(1).into_any_element()
    } else {
        panel.into_any_element()
    }
}

/// A compact symbol button with a tooltip and a VoiceOver label.
pub fn toolbar_icon_button(
    icon: Icon,
    label: &'static str,
    shortcut: &'static str,
    theme: &EditorTheme,
    id: &'static str,
    state: ToolbarState,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme.clone();
    let label: SharedString = theme.ui_text(label).into();
    let tooltip_theme = theme.clone();
    let selected = matches!(state, ToolbarState::Toggle(true) | ToolbarState::Mode(true));
    let disabled = matches!(state, ToolbarState::Disabled);
    let role = if matches!(state, ToolbarState::Mode(_)) {
        Role::RadioButton
    } else {
        Role::Button
    };
    let icon_color = if selected {
        theme.text
    } else if disabled {
        theme.secondary_text.opacity(0.45)
    } else {
        theme.sidebar_text
    };
    div()
        .id(id)
        .accessibility_id(id)
        .role(role)
        .aria_label(label.clone())
        .aria_keyshortcuts(shortcut.replace('⌃', "Control+").replace('⌘', "Meta+"))
        .when(!matches!(state, ToolbarState::Action), |button| {
            button.aria_toggled(if selected {
                Toggled::True
            } else {
                Toggled::False
            })
        })
        .flex()
        .items_center()
        .justify_center()
        .w(px(34.))
        .h(px(28.))
        .flex_shrink_0()
        .when(matches!(icon, Icon::Paragraph), |button| {
            button.w_auto().min_w(px(34.)).max_w(px(80.)).px_2()
        })
        .rounded(px(5.))
        .when(selected, |s| s.bg(theme.tab_active).shadow_sm())
        .when(!disabled, |s| {
            s.hover(move |s| s.bg(theme.toolbar_button_hover))
        })
        .tooltip(move |_, cx| {
            cx.new(|_| ToolbarTooltip {
                label: label.clone(),
                shortcut,
                theme: tooltip_theme.clone(),
            })
            .into()
        })
        .child(if matches!(icon, Icon::Paragraph) {
            div()
                .h(px(18.))
                .max_w_full()
                .overflow_hidden()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(icon_color)
                .child(theme.ui_text("Body"))
                .into_any_element()
        } else {
            icon.render(icon_color)
        })
        .when(!disabled, |button| button.on_click(on_click))
}

/// Compact-toolbar overflow controls stay outside the scrolling tools so
/// mouse users can reach every command without a horizontal trackpad gesture.
pub fn toolbar_scroll_button(
    forward: bool,
    theme: &EditorTheme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label = if forward {
        "Show later formatting tools"
    } else {
        "Show earlier formatting tools"
    };
    let theme = theme.clone();
    let label: SharedString = theme.ui_text(label).into();
    let tooltip_theme = theme.clone();
    div()
        .id(if forward {
            "format-scroll-next"
        } else {
            "format-scroll-previous"
        })
        .debug_selector(move || {
            if forward {
                "format-scroll-next"
            } else {
                "format-scroll-previous"
            }
            .into()
        })
        .role(Role::Button)
        .aria_label(label.clone())
        .flex()
        .items_center()
        .justify_center()
        .w(px(24.))
        .h(px(28.))
        .flex_shrink_0()
        .rounded(px(5.))
        .text_size(px(20.))
        .text_color(theme.secondary_text)
        .hover(move |style| style.bg(theme.toolbar_button_hover))
        .tooltip(move |_, cx| {
            cx.new(|_| ToolbarTooltip {
                label: label.clone(),
                shortcut: "",
                theme: tooltip_theme.clone(),
            })
            .into()
        })
        .child(if forward { "›" } else { "‹" })
        .on_click(on_click)
}

struct ToolbarTooltip {
    label: SharedString,
    shortcut: &'static str,
    theme: EditorTheme,
}

struct PathTooltip {
    path: SharedString,
    theme: EditorTheme,
}

impl Render for PathTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_2()
            .max_w(px(640.))
            .rounded_md()
            .border_1()
            .border_color(self.theme.separator)
            .bg(self.theme.chrome_bg)
            .text_color(self.theme.text)
            .text_size(px(12.))
            .shadow_md()
            .child(self.path.clone())
    }
}

impl Render for ToolbarTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_3()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(self.theme.separator)
            .bg(self.theme.chrome_bg)
            .text_color(self.theme.text)
            .text_size(px(12.))
            .shadow_md()
            .child(self.label.clone())
            .child(
                div()
                    .text_color(self.theme.secondary_text)
                    .child(self.shortcut),
            )
    }
}

/// Uppercase section header for sidebar panels.
pub fn section_header(title: impl Into<SharedString>, theme: &EditorTheme) -> impl IntoElement {
    let title: SharedString = title.into();
    div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.secondary_text)
        .child(theme.ui_text(title.as_ref()))
}

/// Visible, ellipsized location with the complete path available on hover.
pub fn path_caption(path: impl Into<SharedString>, theme: &EditorTheme) -> impl IntoElement {
    let path: SharedString = path.into();
    let tooltip_path = path.clone();
    let tooltip_theme = theme.clone();
    div()
        .id(SharedString::from(format!("location-{path}")))
        .min_w_0()
        .truncate()
        .text_size(px(11.))
        .text_color(theme.secondary_text)
        .tooltip(move |_, cx| {
            cx.new(|_| PathTooltip {
                path: tooltip_path.clone(),
                theme: tooltip_theme.clone(),
            })
            .into()
        })
        .child(path)
}

/// Sidebar file or recent-workspace row.
pub fn sidebar_row(
    label: impl Into<SharedString>,
    full_path: impl Into<SharedString>,
    theme: &EditorTheme,
    selected: bool,
    id: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    sidebar_entry(label, full_path, theme, selected, id, None, on_click)
}

/// Standalone documents expose their directory without replacing the basename.
pub fn sidebar_row_with_location(
    label: impl Into<SharedString>,
    full_path: impl Into<SharedString>,
    theme: &EditorTheme,
    selected: bool,
    id: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let full_path: SharedString = full_path.into();
    let location = std::path::Path::new(full_path.as_ref())
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| SharedString::from(parent.display().to_string()));
    sidebar_entry(label, full_path, theme, selected, id, location, on_click)
}

fn sidebar_entry(
    label: impl Into<SharedString>,
    full_path: impl Into<SharedString>,
    theme: &EditorTheme,
    selected: bool,
    id: impl Into<SharedString>,
    location: Option<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme.clone();
    let tooltip_theme = theme.clone();
    let full_path = full_path.into();
    div()
        .id(id.into())
        .mx_2()
        .px_3()
        .py_1()
        .rounded_md()
        .text_sm()
        .truncate()
        .cursor_pointer()
        .text_color(if selected {
            theme.sidebar_selected_text
        } else {
            theme.sidebar_text
        })
        .when(selected, |row| row.bg(theme.sidebar_selected))
        .when(!selected, |row| {
            row.hover(move |s| s.bg(theme.sidebar_hover))
        })
        .tooltip(move |_, cx| {
            cx.new(|_| PathTooltip {
                path: full_path.clone(),
                theme: tooltip_theme.clone(),
            })
            .into()
        })
        .child(div().truncate().child(label.into()))
        .children(location.map(|location| {
            div()
                .truncate()
                .text_size(px(11.))
                .text_color(theme.secondary_text)
                .child(location)
        }))
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
#[allow(clippy::too_many_arguments)]
pub fn document_tab(
    label: impl Into<SharedString>,
    full_path: impl Into<SharedString>,
    dirty: bool,
    theme: &EditorTheme,
    active: bool,
    id: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_close: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme.clone();
    let tooltip_theme = theme.clone();
    let full_path = full_path.into();
    let bg = if active {
        theme.tab_active
    } else {
        theme.tab_inactive
    };
    let id_string: SharedString = id.into();
    let debug_id = id_string.to_string();
    let label: SharedString = label.into();
    let close_id = SharedString::from(format!("{id_string}-close"));
    div()
        .id(id_string)
        .debug_selector(move || debug_id)
        .role(Role::Tab)
        .aria_label(if dirty {
            SharedString::from(format!("{label} — unsaved changes"))
        } else {
            label.clone()
        })
        .aria_selected(active)
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .mt_1()
        .w(px(180.))
        .flex_shrink_0()
        .min_w_0()
        .rounded_tl(px(6.))
        .rounded_tr(px(6.))
        .border_t_2()
        .border_color(if active {
            theme.accent
        } else {
            gpui::transparent_black()
        })
        .bg(bg)
        .cursor_pointer()
        .tooltip(move |_, cx| {
            cx.new(|_| PathTooltip {
                path: full_path.clone(),
                theme: tooltip_theme.clone(),
            })
            .into()
        })
        .when(dirty, |tab| {
            tab.child(
                div()
                    .w(px(6.))
                    .h(px(6.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(theme.accent),
            )
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .when(active, |title| title.font_weight(FontWeight::SEMIBOLD))
                .text_color(if active {
                    theme.text
                } else {
                    theme.secondary_text
                })
                .child(label),
        )
        .child(
            div()
                .id(close_id)
                .role(Role::Button)
                .aria_label("Close Tab")
                .flex_shrink_0()
                .text_xs()
                .text_color(theme.secondary_text)
                .px_1()
                .rounded_sm()
                .hover(move |s| s.bg(theme.toolbar_button_hover))
                .child(Icon::Close.render(theme.secondary_text))
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    on_close(event, window, cx);
                }),
        )
        .on_click(on_click)
}

/// Secondary hint used in empty sidebar/outline panels.
pub fn muted_hint(text: impl Into<SharedString>, theme: &EditorTheme) -> impl IntoElement {
    let text: SharedString = text.into();
    div()
        .px_3()
        .py_2()
        .text_sm()
        .text_color(theme.secondary_text)
        .child(theme.ui_text(text.as_ref()))
}

/// Read-only contextual syntax belongs in chrome, never in the text flow.
pub fn context_hint(text: String, theme: &EditorTheme) -> impl IntoElement {
    let tooltip_theme = theme.clone();
    let full_text = SharedString::from(text.clone());
    div()
        .id("editing-context-hint")
        .min_w_0()
        .max_w(px(280.))
        .mx_3()
        .truncate()
        .text_xs()
        .text_color(theme.secondary_text)
        .tooltip(move |_, cx| {
            cx.new(|_| PathTooltip {
                path: full_text.clone(),
                theme: tooltip_theme.clone(),
            })
            .into()
        })
        .child(text)
}

/// Welcome content shown only in the editor of a pristine initial session.
pub fn empty_sidebar_state(
    theme: &EditorTheme,
    on_open_folder: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_open_file: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme_clone = theme.clone();
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .p_4()
        .gap_3()
        .child(
            div()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(theme.ui_text("Open a file or folder")),
        )
        .child(div().text_sm().text_color(theme.secondary_text).child(
            theme.ui_text("Automatic recovery protects drafts; saving writes the document file."),
        ))
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
                        .child(theme.ui_text("Open Folder…"))
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
                        .child(theme.ui_text("Open…"))
                        .on_click(on_open_file),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use markrust_core::rich::BlockType;
    use markrust_editor::EditorCommand;

    #[test]
    fn heading_labels_follow_their_heading_commands_in_level_order() {
        for (index, tool) in BLOCK_STYLE_TOOLS.into_iter().take(3).enumerate() {
            let level = index as u8 + 1;
            assert_eq!(tool.icon.text_label(), Some(format!("H{level}").as_str()));
            assert_eq!(tool.label, format!("Heading {level}"));
            assert_eq!(tool.shortcut, format!("⌘{level}"));
            assert_eq!(tool.id, format!("fmt-h{level}"));
            assert_eq!(
                tool.command,
                EditorCommand::SetBlockType(BlockType::Heading(level))
            );
        }
    }

    #[test]
    fn body_label_clearly_converts_to_plain_paragraph() {
        let tool = BLOCK_STYLE_TOOLS.into_iter().last().unwrap();
        assert_eq!(tool.icon.text_label(), Some("Body"));
        assert_eq!(tool.label, "Plain paragraph");
        assert_eq!(tool.shortcut, "⌘⌥0");
        assert_eq!(tool.id, "fmt-paragraph");
        assert_eq!(tool.command, EditorCommand::Paragraph);
        assert_eq!(Icon::Bold.text_label(), None);
    }
}
