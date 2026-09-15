// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The actual operating-system menu bar, shared by toolbar and keyboard actions.

use gpui::{actions, App, Global, Menu, MenuItem, OsAction, SystemMenuType};

use crate::{window::*, workspace::EditorMode};

actions!(markrust_app, [Quit, Hide, HideOthers, ShowAll]);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct MenuState {
    pub mode: EditorMode,
    pub sidebar_open: bool,
    pub outline_open: bool,
}

impl Default for MenuState {
    fn default() -> Self {
        Self {
            mode: EditorMode::default(),
            sidebar_open: true,
            outline_open: true,
        }
    }
}

impl Global for MenuState {}

pub(crate) fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.set_global(MenuState::default());
    cx.set_menus(application_menus(MenuState::default()));
}

pub(crate) fn sync(state: MenuState, cx: &mut App) {
    if cx.try_global::<MenuState>() == Some(&state) {
        return;
    }
    cx.set_global(state);
    cx.set_menus(application_menus(state));
}

pub(crate) fn application_menus(state: MenuState) -> Vec<Menu> {
    vec![
        Menu::new("MarkRust").items([
            MenuItem::action("About MarkRust", About),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide MarkRust", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit MarkRust", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Document", NewDocument),
            MenuItem::action("Open…", OpenFile),
            MenuItem::action("Open Folder…", OpenFolder),
            MenuItem::separator(),
            MenuItem::action("Close Tab", CloseTab),
            MenuItem::action("Save", Save),
            MenuItem::action("Save As…", SaveAs),
            MenuItem::separator(),
            MenuItem::action("Export HTML", ExportHtml),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", markrust_editor::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", markrust_editor::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action(
                "Select All",
                markrust_editor::SelectAll,
                OsAction::SelectAll,
            ),
        ]),
        Menu::new("Format").items([
            MenuItem::action("Bold", markrust_editor::ToggleBold),
            MenuItem::action("Italic", markrust_editor::ToggleItalic),
            MenuItem::action("Inline Code", markrust_editor::ToggleCode),
            MenuItem::action("Link…", markrust_editor::ToggleLink),
            MenuItem::separator(),
            MenuItem::action("Indent", markrust_editor::Indent),
            MenuItem::action("Outdent", markrust_editor::Outdent),
        ]),
        Menu::new("View").items([
            MenuItem::action("WYSIWYG", ShowWysiwyg).checked(state.mode == EditorMode::Wysiwyg),
            MenuItem::action("Source", ShowSource).checked(state.mode == EditorMode::Source),
            MenuItem::action("Split View", ShowSplit).checked(state.mode == EditorMode::Split),
            MenuItem::action("Next Editor Mode", ToggleEditorMode),
            MenuItem::separator(),
            MenuItem::action("Show Sidebar", ToggleSidebar).checked(state.sidebar_open),
            MenuItem::action("Show Outline", ToggleOutline).checked(state.outline_open),
            MenuItem::separator(),
            MenuItem::action("Command Palette…", CommandPalette),
            MenuItem::action("Load Remote Images", LoadRemoteImages),
            MenuItem::action("Toggle Light / Dark Appearance", ToggleTheme),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
        ]),
        Menu::new("Help").items([MenuItem::action("MarkRust Help", Help)]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_mode_selection_tracks_the_active_tab() {
        for mode in [EditorMode::Wysiwyg, EditorMode::Source, EditorMode::Split] {
            let menus = application_menus(MenuState {
                mode,
                ..MenuState::default()
            });
            let view = menus.iter().find(|menu| menu.name == "View").unwrap();
            let selected: Vec<_> = view.items[..3]
                .iter()
                .filter(|item| item.is_checked())
                .collect();
            assert_eq!(
                selected.len(),
                1,
                "exactly one editing mode must be selected"
            );
            let MenuItem::Action { action, .. } = selected[0] else {
                panic!("mode is an action")
            };
            let expected = match mode {
                EditorMode::Wysiwyg => <ShowWysiwyg as gpui::Action>::name_for_type(),
                EditorMode::Source => <ShowSource as gpui::Action>::name_for_type(),
                EditorMode::Split => <ShowSplit as gpui::Action>::name_for_type(),
            };
            assert_eq!(action.name(), expected);
        }
    }
}
