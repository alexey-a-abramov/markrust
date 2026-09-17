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

pub(crate) fn load_bundled_fonts(cx: &mut App) {
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
        crate::menus::init(cx);

        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("MarkRust".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(680.), px(420.))),
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

pub(crate) fn desktop_key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-q", crate::menus::Quit, None),
        KeyBinding::new("cmd-h", crate::menus::Hide, None),
        KeyBinding::new("alt-cmd-h", crate::menus::HideOthers, None),
        KeyBinding::new("cmd-m", crate::window::Minimize, None),
        KeyBinding::new("ctrl-cmd-f", crate::window::ToggleFullScreen, None),
        KeyBinding::new("cmd-s", crate::window::Save, None),
        KeyBinding::new("cmd-shift-s", crate::window::SaveAs, None),
        KeyBinding::new("cmd-shift-m", crate::window::ToggleEditorMode, None),
        // Mode picker shortcuts moved off Cmd-1/2/3 to free Cmd-1..6 for
        // ATX heading toggles (the standard set in iA Writer, Typora,
        // Obsidian).
        KeyBinding::new("alt-cmd-1", crate::window::ShowWysiwyg, None),
        KeyBinding::new("alt-cmd-2", crate::window::ShowSource, None),
        KeyBinding::new("alt-cmd-3", crate::window::ShowSplit, None),
        KeyBinding::new("ctrl-cmd-s", crate::window::ToggleSidebar, None),
        KeyBinding::new("ctrl-cmd-o", crate::window::ToggleOutline, None),
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
        // Option-Backspace/Delete (macOS) and Ctrl-Backspace/Delete (Windows/Linux).
        KeyBinding::new("alt-backspace", markrust_editor::DeleteWordLeft, None),
        KeyBinding::new("alt-delete", markrust_editor::DeleteWordRight, None),
        KeyBinding::new("ctrl-backspace", markrust_editor::DeleteWordLeft, None),
        KeyBinding::new("ctrl-delete", markrust_editor::DeleteWordRight, None),
        // Cmd-Backspace/Delete: current visual/source line, not the document.
        KeyBinding::new("cmd-backspace", markrust_editor::DeleteToLineStart, None),
        KeyBinding::new("cmd-delete", markrust_editor::DeleteToLineEnd, None),
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
        KeyBinding::new("cmd-c", markrust_editor::Copy, None),
        KeyBinding::new("ctrl-c", markrust_editor::Copy, None),
        KeyBinding::new("cmd-x", markrust_editor::Cut, None),
        KeyBinding::new("ctrl-x", markrust_editor::Cut, None),
        KeyBinding::new("cmd-v", crate::window::Paste, None),
        KeyBinding::new("ctrl-v", crate::window::Paste, None),
        KeyBinding::new("enter", markrust_editor::Enter, None),
        KeyBinding::new("shift-enter", markrust_editor::InsertLineBreak, None),
        KeyBinding::new("cmd-b", markrust_editor::ToggleBold, None),
        KeyBinding::new("ctrl-b", markrust_editor::ToggleBold, None),
        KeyBinding::new("cmd-i", markrust_editor::ToggleItalic, None),
        KeyBinding::new("ctrl-i", markrust_editor::ToggleItalic, None),
        // Inline code moved to Cmd-Option-K / Ctrl-Option-K so Cmd-1..6
        // can stay reserved for ATX headings.
        KeyBinding::new("cmd-k", markrust_editor::ToggleLink, None),
        KeyBinding::new("ctrl-k", markrust_editor::ToggleLink, None),
        // Markdown formatting toolbar. Cmd-1..6 are ATX headings (mode
        // picker moved to Cmd-Option-1/2/3 above). Cmd-Option-C / K are
        // the block code fence and inline code (which used to live on
        // Cmd-E / Ctrl-E).
        KeyBinding::new("cmd-1", markrust_editor::SetHeading1, None),
        KeyBinding::new("cmd-2", markrust_editor::SetHeading2, None),
        KeyBinding::new("cmd-3", markrust_editor::SetHeading3, None),
        KeyBinding::new("cmd-4", markrust_editor::SetHeading4, None),
        KeyBinding::new("cmd-5", markrust_editor::SetHeading5, None),
        KeyBinding::new("cmd-6", markrust_editor::SetHeading6, None),
        KeyBinding::new("alt-cmd-0", markrust_editor::Paragraph, None),
        KeyBinding::new("shift-cmd-7", markrust_editor::ToggleOrderedList, None),
        KeyBinding::new("shift-cmd-8", markrust_editor::ToggleUnorderedList, None),
        KeyBinding::new("shift-cmd-9", markrust_editor::ToggleTaskList, None),
        KeyBinding::new("shift-cmd-.", markrust_editor::ToggleBlockquote, None),
        KeyBinding::new("shift-cmd--", markrust_editor::InsertHorizontalRule, None),
        KeyBinding::new("alt-cmd-c", markrust_editor::InsertCodeBlock, None),
        KeyBinding::new("alt-cmd-k", markrust_editor::ToggleCode, None),
        KeyBinding::new("shift-cmd-x", markrust_editor::ToggleStrikethrough, None),
        KeyBinding::new("shift-cmd-i", markrust_editor::InsertImage, None),
        KeyBinding::new("alt-cmd-t", markrust_editor::InsertTable, None),
        // Same shortcuts on Windows/Linux (Ctrl instead of Cmd).
        KeyBinding::new("ctrl-1", markrust_editor::SetHeading1, None),
        KeyBinding::new("ctrl-2", markrust_editor::SetHeading2, None),
        KeyBinding::new("ctrl-3", markrust_editor::SetHeading3, None),
        KeyBinding::new("ctrl-4", markrust_editor::SetHeading4, None),
        KeyBinding::new("ctrl-5", markrust_editor::SetHeading5, None),
        KeyBinding::new("ctrl-6", markrust_editor::SetHeading6, None),
        KeyBinding::new("alt-ctrl-0", markrust_editor::Paragraph, None),
        KeyBinding::new("shift-ctrl-7", markrust_editor::ToggleOrderedList, None),
        KeyBinding::new("shift-ctrl-8", markrust_editor::ToggleUnorderedList, None),
        KeyBinding::new("shift-ctrl-9", markrust_editor::ToggleTaskList, None),
        KeyBinding::new("shift-ctrl-.", markrust_editor::ToggleBlockquote, None),
        KeyBinding::new("shift-ctrl--", markrust_editor::InsertHorizontalRule, None),
        KeyBinding::new("alt-ctrl-c", markrust_editor::InsertCodeBlock, None),
        KeyBinding::new("alt-ctrl-k", markrust_editor::ToggleCode, None),
        KeyBinding::new("shift-ctrl-x", markrust_editor::ToggleStrikethrough, None),
        KeyBinding::new("shift-ctrl-i", markrust_editor::InsertImage, None),
        KeyBinding::new("alt-ctrl-t", markrust_editor::InsertTable, None),
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

    #[test]
    fn shift_enter_binds_insert_line_break() {
        let bindings = desktop_key_bindings();
        let typed = Keystroke::parse("shift-enter").expect("shift-enter parses");
        let hit = bindings
            .iter()
            .find(|binding| binding.match_keystrokes(std::slice::from_ref(&typed)) == Some(false))
            .expect("shift-enter must be bound");
        assert_eq!(
            hit.action().name(),
            markrust_editor::InsertLineBreak::name_for_type(),
            "Shift-Enter is Typora's hard line break"
        );
        let enter = Keystroke::parse("enter").expect("enter parses");
        let enter_hit = bindings
            .iter()
            .find(|binding| binding.match_keystrokes(std::slice::from_ref(&enter)) == Some(false))
            .expect("enter must stay bound");
        assert_eq!(
            enter_hit.action().name(),
            markrust_editor::Enter::name_for_type()
        );
    }

    #[test]
    fn cmd_c_and_cmd_x_bind_copy_cut() {
        let bindings = desktop_key_bindings();
        let copy = Keystroke::parse("cmd-c").expect("cmd-c parses");
        let copy_hit = bindings
            .iter()
            .find(|binding| binding.match_keystrokes(std::slice::from_ref(&copy)) == Some(false))
            .expect("cmd-c must be bound");
        assert_eq!(
            copy_hit.action().name(),
            markrust_editor::Copy::name_for_type(),
            "Cmd-C copies the WYSIWYG selection as markdown"
        );
        let cut = Keystroke::parse("cmd-x").expect("cmd-x parses");
        let cut_hit = bindings
            .iter()
            .find(|binding| binding.match_keystrokes(std::slice::from_ref(&cut)) == Some(false))
            .expect("cmd-x must be bound");
        assert_eq!(
            cut_hit.action().name(),
            markrust_editor::Cut::name_for_type(),
            "Cmd-X cuts the WYSIWYG selection as markdown"
        );
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

    #[test]
    fn word_and_line_delete_keys_bind() {
        assert_eq!(
            action_for("alt-backspace"),
            markrust_editor::DeleteWordLeft::name_for_type(),
            "Option-Backspace is word-delete-left on macOS"
        );
        assert_eq!(
            action_for("alt-delete"),
            markrust_editor::DeleteWordRight::name_for_type()
        );
        assert_eq!(
            action_for("ctrl-backspace"),
            markrust_editor::DeleteWordLeft::name_for_type(),
            "Ctrl-Backspace is word-delete-left on Windows/Linux"
        );
        assert_eq!(
            action_for("ctrl-delete"),
            markrust_editor::DeleteWordRight::name_for_type()
        );
        assert_eq!(
            action_for("cmd-backspace"),
            markrust_editor::DeleteToLineStart::name_for_type(),
            "Cmd-Backspace deletes to the current line start"
        );
        assert_eq!(
            action_for("cmd-delete"),
            markrust_editor::DeleteToLineEnd::name_for_type()
        );
        // Word-move and line-move keys from last turn must stay move, not delete.
        assert_eq!(
            action_for("alt-left"),
            markrust_editor::WordLeft::name_for_type()
        );
        assert_eq!(
            action_for("cmd-left"),
            markrust_editor::Home::name_for_type()
        );
        assert_eq!(
            action_for("backspace"),
            markrust_editor::Backspace::name_for_type()
        );
        assert_eq!(
            action_for("delete"),
            markrust_editor::Delete::name_for_type()
        );
    }

    #[test]
    fn markdown_toolbar_shortcuts_are_bound() {
        // The Markdown editing toolbar mirrors these shortcuts — see
        // `format_toolbar` in window.rs. If you add a new toolbar button,
        // bind a default shortcut here too.
        assert_eq!(
            action_for("cmd-1"),
            markrust_editor::SetHeading1::name_for_type(),
            "Cmd-1 toggles Heading 1"
        );
        assert_eq!(
            action_for("cmd-2"),
            markrust_editor::SetHeading2::name_for_type()
        );
        assert_eq!(
            action_for("cmd-3"),
            markrust_editor::SetHeading3::name_for_type()
        );
        assert_eq!(
            action_for("cmd-4"),
            markrust_editor::SetHeading4::name_for_type()
        );
        assert_eq!(
            action_for("cmd-5"),
            markrust_editor::SetHeading5::name_for_type()
        );
        assert_eq!(
            action_for("cmd-6"),
            markrust_editor::SetHeading6::name_for_type()
        );
        assert_eq!(
            action_for("shift-cmd-8"),
            markrust_editor::ToggleUnorderedList::name_for_type()
        );
        assert_eq!(
            action_for("shift-cmd-7"),
            markrust_editor::ToggleOrderedList::name_for_type()
        );
        assert_eq!(
            action_for("shift-cmd-9"),
            markrust_editor::ToggleTaskList::name_for_type()
        );
        assert_eq!(
            action_for("shift-cmd-."),
            markrust_editor::ToggleBlockquote::name_for_type()
        );
        assert_eq!(
            action_for("shift-cmd--"),
            markrust_editor::InsertHorizontalRule::name_for_type()
        );
        assert_eq!(
            action_for("alt-cmd-c"),
            markrust_editor::InsertCodeBlock::name_for_type()
        );
        assert_eq!(
            action_for("alt-cmd-k"),
            markrust_editor::ToggleCode::name_for_type(),
            "Inline code moves from Cmd-E to Cmd-Option-K"
        );
        assert_eq!(
            action_for("shift-cmd-x"),
            markrust_editor::ToggleStrikethrough::name_for_type()
        );
        assert_eq!(
            action_for("shift-cmd-i"),
            markrust_editor::InsertImage::name_for_type()
        );
        assert_eq!(
            action_for("alt-cmd-t"),
            markrust_editor::InsertTable::name_for_type()
        );

        // The mode picker was moved off Cmd-1..3 to make room for headings.
        assert_eq!(
            action_for("alt-cmd-1"),
            crate::window::ShowWysiwyg::name_for_type(),
            "Mode picker is now Cmd-Option-1"
        );
        assert_eq!(
            action_for("alt-cmd-2"),
            crate::window::ShowSource::name_for_type()
        );
        assert_eq!(
            action_for("alt-cmd-3"),
            crate::window::ShowSplit::name_for_type()
        );
    }
}
