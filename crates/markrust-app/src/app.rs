// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{px, size, App, AppContext, Bounds, KeyBinding, WindowBounds, WindowOptions};
use gpui_platform::application;

use crate::config::AppConfig;
use crate::window::MarkRustWindow;
use crate::workspace::Workspace;

/// GPUI revision pinned in `Cargo.toml` for reproducible builds.
pub const GPUI_GIT_REV: &str = "8166e3d7b8b42d8aaf4d4dee7fcd25ab4ec65105";

fn load_window_icon() -> Option<Arc<image::RgbaImage>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/icon/markrust.png");
    image::open(path).ok().map(|img| Arc::new(img.into_rgba8()))
}

pub fn run_gui() {
    run_gui_with_open(None);
}

fn load_bundled_fonts(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(include_bytes!("../../../assets/fonts/Inter-Regular.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../../../assets/fonts/Inter-Italic.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../../../assets/fonts/Inter-Bold.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../../../assets/fonts/Inter-BoldItalic.ttf").as_slice()),
    ];
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        eprintln!("Failed to load bundled Inter fonts: {error}");
    }
}

/// Launch the desktop editor, optionally opening a file or workspace folder.
pub fn run_gui_with_open(open_path: Option<PathBuf>) {
    crate::crash::install_panic_logger();
    application().run(move |cx: &mut App| {
        load_bundled_fonts(cx);
        let config = AppConfig::load();
        cx.bind_keys(desktop_key_bindings());

        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("MarkRust".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                icon: load_window_icon(),
                ..Default::default()
            },
            move |window, cx| {
                let config = config.clone();
                let open_path = open_path.clone();
                let workspace = cx.new(|cx| {
                    let mut workspace = Workspace::new(config, window, cx);
                    if let Some(path) = open_path {
                        if let Err(error) = workspace.open_launch_path(path, window, cx) {
                            eprintln!("Failed to open path: {error}");
                        }
                    }
                    workspace
                });
                cx.new(|cx| MarkRustWindow::new(workspace, cx))
            },
        )
        .expect("failed to open MarkRust window");
        cx.activate(true);
    });
}

fn desktop_key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-s", crate::window::Save, None),
        KeyBinding::new("cmd-shift-m", crate::window::ToggleEditorMode, None),
        KeyBinding::new("cmd-o", crate::window::OpenFile, None),
        KeyBinding::new("cmd-shift-o", crate::window::OpenFolder, None),
        KeyBinding::new("cmd-n", crate::window::NewDocument, None),
        KeyBinding::new("cmd-w", crate::window::CloseTab, None),
        KeyBinding::new("cmd-p", crate::window::CommandPalette, None),
        KeyBinding::new("cmd-z", crate::window::Undo, None),
        KeyBinding::new("cmd-shift-z", crate::window::Redo, None),
        KeyBinding::new("shift-cmd-t", crate::window::ToggleTheme, None),
        KeyBinding::new("backspace", markrust_editor::Backspace, None),
        KeyBinding::new("delete", markrust_editor::Delete, None),
        KeyBinding::new("left", markrust_editor::Left, None),
        KeyBinding::new("right", markrust_editor::Right, None),
        KeyBinding::new("up", markrust_editor::Up, None),
        KeyBinding::new("down", markrust_editor::Down, None),
        KeyBinding::new("shift-left", markrust_editor::SelectLeft, None),
        KeyBinding::new("shift-right", markrust_editor::SelectRight, None),
        KeyBinding::new("shift-up", markrust_editor::SelectUp, None),
        KeyBinding::new("shift-down", markrust_editor::SelectDown, None),
        KeyBinding::new("home", markrust_editor::Home, None),
        KeyBinding::new("end", markrust_editor::End, None),
        KeyBinding::new("shift-home", markrust_editor::SelectHome, None),
        KeyBinding::new("shift-end", markrust_editor::SelectEnd, None),
        // macOS laptops have no Home/End keys; Cmd-Left/Right are line bounds.
        KeyBinding::new("cmd-left", markrust_editor::Home, None),
        KeyBinding::new("cmd-right", markrust_editor::End, None),
        KeyBinding::new("cmd-shift-left", markrust_editor::SelectHome, None),
        KeyBinding::new("cmd-shift-right", markrust_editor::SelectEnd, None),
        // Option-Left/Right (macOS) and Ctrl-Left/Right (Windows/Linux): word bounds.
        KeyBinding::new("alt-left", markrust_editor::WordLeft, None),
        KeyBinding::new("alt-right", markrust_editor::WordRight, None),
        KeyBinding::new("alt-shift-left", markrust_editor::SelectWordLeft, None),
        KeyBinding::new("alt-shift-right", markrust_editor::SelectWordRight, None),
        KeyBinding::new("ctrl-left", markrust_editor::WordLeft, None),
        KeyBinding::new("ctrl-right", markrust_editor::WordRight, None),
        KeyBinding::new("ctrl-shift-left", markrust_editor::SelectWordLeft, None),
        KeyBinding::new("ctrl-shift-right", markrust_editor::SelectWordRight, None),
        // Cmd-Up/Down (macOS) and Ctrl-Home/End (Windows/Linux): document bounds.
        KeyBinding::new("cmd-up", markrust_editor::DocumentHome, None),
        KeyBinding::new("cmd-down", markrust_editor::DocumentEnd, None),
        KeyBinding::new("cmd-shift-up", markrust_editor::SelectDocumentHome, None),
        KeyBinding::new("cmd-shift-down", markrust_editor::SelectDocumentEnd, None),
        KeyBinding::new("ctrl-home", markrust_editor::DocumentHome, None),
        KeyBinding::new("ctrl-end", markrust_editor::DocumentEnd, None),
        KeyBinding::new("ctrl-shift-home", markrust_editor::SelectDocumentHome, None),
        KeyBinding::new("ctrl-shift-end", markrust_editor::SelectDocumentEnd, None),
        KeyBinding::new("pageup", markrust_editor::PageUp, None),
        KeyBinding::new("pagedown", markrust_editor::PageDown, None),
        KeyBinding::new("shift-pageup", markrust_editor::SelectPageUp, None),
        KeyBinding::new("shift-pagedown", markrust_editor::SelectPageDown, None),
        KeyBinding::new("cmd-a", markrust_editor::SelectAll, None),
        KeyBinding::new("enter", markrust_editor::Enter, None),
        KeyBinding::new("cmd-b", markrust_editor::ToggleBold, None),
        KeyBinding::new("ctrl-b", markrust_editor::ToggleBold, None),
        KeyBinding::new("cmd-i", markrust_editor::ToggleItalic, None),
        KeyBinding::new("ctrl-i", markrust_editor::ToggleItalic, None),
        KeyBinding::new("cmd-e", markrust_editor::ToggleCode, None),
        KeyBinding::new("ctrl-e", markrust_editor::ToggleCode, None),
        KeyBinding::new("cmd-k", markrust_editor::ToggleLink, None),
        KeyBinding::new("ctrl-k", markrust_editor::ToggleLink, None),
        KeyBinding::new("tab", markrust_editor::Indent, Some("RichEditor")),
        KeyBinding::new("tab", markrust_editor::Indent, Some("MarkdownEditor")),
        KeyBinding::new("shift-tab", markrust_editor::Outdent, Some("RichEditor")),
        KeyBinding::new(
            "shift-tab",
            markrust_editor::Outdent,
            Some("MarkdownEditor"),
        ),
        KeyBinding::new("escape", markrust_editor::Escape, Some("RichEditor")),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Action, Keystroke};

    #[test]
    fn gpui_rev_is_non_empty() {
        assert_eq!(GPUI_GIT_REV.len(), 40);
    }

    fn action_for(key: &str) -> String {
        let bindings = desktop_key_bindings();
        let typed = Keystroke::parse(key).unwrap_or_else(|err| panic!("{key} parses: {err}"));
        bindings
            .iter()
            .find(|binding| binding.match_keystrokes(std::slice::from_ref(&typed)) == Some(false))
            .unwrap_or_else(|| panic!("{key} must be bound"))
            .action()
            .name()
            .to_string()
    }

    #[test]
    fn shift_arrows_and_mac_line_keys_bind_selection() {
        assert_eq!(
            action_for("shift-up"),
            markrust_editor::SelectUp::name_for_type(),
            "Shift-Up extends the selection up a line"
        );
        assert_eq!(
            action_for("shift-down"),
            markrust_editor::SelectDown::name_for_type()
        );
        assert_eq!(
            action_for("shift-home"),
            markrust_editor::SelectHome::name_for_type()
        );
        assert_eq!(
            action_for("shift-end"),
            markrust_editor::SelectEnd::name_for_type()
        );
        assert_eq!(
            action_for("cmd-left"),
            markrust_editor::Home::name_for_type(),
            "Cmd-Left is macOS line start (laptops have no Home key)"
        );
        assert_eq!(
            action_for("cmd-right"),
            markrust_editor::End::name_for_type()
        );
        assert_eq!(
            action_for("cmd-shift-left"),
            markrust_editor::SelectHome::name_for_type()
        );
        assert_eq!(
            action_for("cmd-shift-right"),
            markrust_editor::SelectEnd::name_for_type()
        );
        assert_eq!(
            action_for("pageup"),
            markrust_editor::PageUp::name_for_type()
        );
        assert_eq!(
            action_for("pagedown"),
            markrust_editor::PageDown::name_for_type()
        );
        // Existing Shift-Left must still extend, not be replaced.
        assert_eq!(
            action_for("shift-left"),
            markrust_editor::SelectLeft::name_for_type()
        );
        assert_eq!(action_for("home"), markrust_editor::Home::name_for_type());
    }

    #[test]
    fn word_document_and_shift_page_keys_bind() {
        assert_eq!(
            action_for("alt-left"),
            markrust_editor::WordLeft::name_for_type(),
            "Option-Left is word-left on macOS"
        );
        assert_eq!(
            action_for("alt-right"),
            markrust_editor::WordRight::name_for_type()
        );
        assert_eq!(
            action_for("alt-shift-left"),
            markrust_editor::SelectWordLeft::name_for_type()
        );
        assert_eq!(
            action_for("alt-shift-right"),
            markrust_editor::SelectWordRight::name_for_type()
        );
        assert_eq!(
            action_for("ctrl-left"),
            markrust_editor::WordLeft::name_for_type(),
            "Ctrl-Left is word-left on Windows/Linux"
        );
        assert_eq!(
            action_for("ctrl-right"),
            markrust_editor::WordRight::name_for_type()
        );
        assert_eq!(
            action_for("ctrl-shift-left"),
            markrust_editor::SelectWordLeft::name_for_type()
        );
        assert_eq!(
            action_for("ctrl-shift-right"),
            markrust_editor::SelectWordRight::name_for_type()
        );
        assert_eq!(
            action_for("cmd-up"),
            markrust_editor::DocumentHome::name_for_type(),
            "Cmd-Up is document start on macOS"
        );
        assert_eq!(
            action_for("cmd-down"),
            markrust_editor::DocumentEnd::name_for_type()
        );
        assert_eq!(
            action_for("cmd-shift-up"),
            markrust_editor::SelectDocumentHome::name_for_type()
        );
        assert_eq!(
            action_for("cmd-shift-down"),
            markrust_editor::SelectDocumentEnd::name_for_type()
        );
        assert_eq!(
            action_for("ctrl-home"),
            markrust_editor::DocumentHome::name_for_type(),
            "Ctrl-Home is document start on Windows/Linux"
        );
        assert_eq!(
            action_for("ctrl-end"),
            markrust_editor::DocumentEnd::name_for_type()
        );
        assert_eq!(
            action_for("ctrl-shift-home"),
            markrust_editor::SelectDocumentHome::name_for_type()
        );
        assert_eq!(
            action_for("ctrl-shift-end"),
            markrust_editor::SelectDocumentEnd::name_for_type()
        );
        assert_eq!(
            action_for("shift-pageup"),
            markrust_editor::SelectPageUp::name_for_type()
        );
        assert_eq!(
            action_for("shift-pagedown"),
            markrust_editor::SelectPageDown::name_for_type()
        );
        // Last turn's line keys must stay line-local, not word/document.
        assert_eq!(
            action_for("cmd-left"),
            markrust_editor::Home::name_for_type()
        );
        assert_eq!(
            action_for("cmd-shift-left"),
            markrust_editor::SelectHome::name_for_type()
        );
        assert_eq!(
            action_for("shift-up"),
            markrust_editor::SelectUp::name_for_type()
        );
        assert_eq!(
            action_for("pageup"),
            markrust_editor::PageUp::name_for_type()
        );
    }
}
