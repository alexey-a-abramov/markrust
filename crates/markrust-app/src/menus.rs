// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The actual operating-system menu bar, shared by toolbar and keyboard actions.

use gpui::{actions, App, Global, Menu, MenuItem, OsAction, SystemMenuType};

use crate::config::HighlightStyle;
use crate::{window::*, workspace::EditorMode};

actions!(
    markrust_app,
    [
        Quit,
        Hide,
        HideOthers,
        ShowAll,
        NewWindow,
        CheckForUpdates,
        ToggleAutomaticUpdates,
        OpenRepository
    ]
);
actions!(
    markrust_app,
    [
        LanguageEnglish,
        LanguageRussian,
        LanguageSpanish,
        LanguageFrench,
        LanguageGerman,
        LanguagePortuguese,
        LanguageItalian,
        LanguageDutch,
        LanguagePolish,
        LanguageUkrainian,
        LanguageTurkish,
        LanguageArabic,
        LanguageHebrew,
        LanguageHindi,
        LanguageBengali,
        LanguageChinese,
        LanguageJapanese,
        LanguageKorean,
        LanguageIndonesian,
        LanguageVietnamese
    ]
);

fn language_menu(language: crate::i18n::Language) -> Menu {
    use crate::i18n::{text, Language};
    Menu::new(text(language, "Language")).items([
        MenuItem::action("English", LanguageEnglish).checked(language == Language::English),
        MenuItem::action("Русский", LanguageRussian).checked(language == Language::Russian),
        MenuItem::action("Español", LanguageSpanish).checked(language == Language::Spanish),
        MenuItem::action("Français", LanguageFrench).checked(language == Language::French),
        MenuItem::action("Deutsch", LanguageGerman).checked(language == Language::German),
        MenuItem::action("Português", LanguagePortuguese).checked(language == Language::Portuguese),
        MenuItem::action("Italiano", LanguageItalian).checked(language == Language::Italian),
        MenuItem::action("Nederlands", LanguageDutch).checked(language == Language::Dutch),
        MenuItem::action("Polski", LanguagePolish).checked(language == Language::Polish),
        MenuItem::action("Українська", LanguageUkrainian).checked(language == Language::Ukrainian),
        MenuItem::action("Türkçe", LanguageTurkish).checked(language == Language::Turkish),
        MenuItem::action("العربية", LanguageArabic).checked(language == Language::Arabic),
        MenuItem::action("עברית", LanguageHebrew).checked(language == Language::Hebrew),
        MenuItem::action("हिन्दी", LanguageHindi).checked(language == Language::Hindi),
        MenuItem::action("বাংলা", LanguageBengali).checked(language == Language::Bengali),
        MenuItem::action("简体中文", LanguageChinese).checked(language == Language::Chinese),
        MenuItem::action("日本語", LanguageJapanese).checked(language == Language::Japanese),
        MenuItem::action("한국어", LanguageKorean).checked(language == Language::Korean),
        MenuItem::action("Bahasa Indonesia", LanguageIndonesian)
            .checked(language == Language::Indonesian),
        MenuItem::action("Tiếng Việt", LanguageVietnamese)
            .checked(language == Language::Vietnamese),
    ])
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct MenuState {
    pub language: crate::i18n::Language,
    pub automatic_updates: bool,
    pub mode: EditorMode,
    pub sidebar_open: bool,
    pub outline_open: bool,
    pub markup_hints_enabled: bool,
    pub highlight_style: HighlightStyle,
}

impl Default for MenuState {
    fn default() -> Self {
        Self {
            language: crate::i18n::Language::default(),
            automatic_updates: true,
            mode: EditorMode::default(),
            sidebar_open: true,
            outline_open: true,
            markup_hints_enabled: true,
            highlight_style: HighlightStyle::default(),
        }
    }
}

impl Global for MenuState {}

pub(crate) fn init(cx: &mut App) {
    cx.on_action(|_: &CheckForUpdates, cx| crate::update_ui::check(true, cx));
    cx.on_action(|_: &ToggleAutomaticUpdates, cx| crate::update_ui::toggle_automatic(cx));
    cx.on_action(|_: &OpenRepository, cx| cx.open_url(crate::update_ui::REPOSITORY_URL));
    cx.on_action(|_: &LanguageEnglish, cx| {
        crate::app::set_ui_language(crate::i18n::Language::English, cx)
    });
    cx.on_action(|_: &LanguageRussian, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Russian, cx)
    });
    cx.on_action(|_: &LanguageSpanish, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Spanish, cx)
    });
    cx.on_action(|_: &LanguageFrench, cx| {
        crate::app::set_ui_language(crate::i18n::Language::French, cx)
    });
    cx.on_action(|_: &LanguageGerman, cx| {
        crate::app::set_ui_language(crate::i18n::Language::German, cx)
    });
    cx.on_action(|_: &LanguagePortuguese, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Portuguese, cx)
    });
    cx.on_action(|_: &LanguageItalian, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Italian, cx)
    });
    cx.on_action(|_: &LanguageDutch, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Dutch, cx)
    });
    cx.on_action(|_: &LanguagePolish, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Polish, cx)
    });
    cx.on_action(|_: &LanguageUkrainian, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Ukrainian, cx)
    });
    cx.on_action(|_: &LanguageTurkish, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Turkish, cx)
    });
    cx.on_action(|_: &LanguageArabic, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Arabic, cx)
    });
    cx.on_action(|_: &LanguageHebrew, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Hebrew, cx)
    });
    cx.on_action(|_: &LanguageHindi, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Hindi, cx)
    });
    cx.on_action(|_: &LanguageBengali, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Bengali, cx)
    });
    cx.on_action(|_: &LanguageChinese, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Chinese, cx)
    });
    cx.on_action(|_: &LanguageJapanese, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Japanese, cx)
    });
    cx.on_action(|_: &LanguageKorean, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Korean, cx)
    });
    cx.on_action(|_: &LanguageIndonesian, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Indonesian, cx)
    });
    cx.on_action(|_: &LanguageVietnamese, cx| {
        crate::app::set_ui_language(crate::i18n::Language::Vietnamese, cx)
    });
    cx.on_action(|_: &Quit, cx| crate::app::quit_application_checked(cx));
    cx.on_action(|_: &NewWindow, cx| {
        if let Err(error) = crate::app::new_application_window(cx) {
            eprintln!("MarkRust could not create a new window: {error}");
        }
    });
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
    let text = |key: &str| crate::i18n::text(state.language, key);
    vec![
        Menu::new("MarkRust").items([
            MenuItem::action(text("About MarkRust"), About),
            MenuItem::action(text("Check for Updates…"), CheckForUpdates),
            MenuItem::action(
                text("Automatically Check for Updates"),
                ToggleAutomaticUpdates,
            )
            .checked(state.automatic_updates),
            MenuItem::action(text("GitHub Repository"), OpenRepository),
            MenuItem::separator(),
            MenuItem::os_submenu(text("Services"), SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action(text("Hide MarkRust"), Hide),
            MenuItem::action(text("Hide Others"), HideOthers),
            MenuItem::action(text("Show All"), ShowAll),
            MenuItem::separator(),
            MenuItem::action(text("Quit MarkRust"), Quit),
        ]),
        Menu::new(text("File")).items([
            MenuItem::action(text("New Document"), NewDocument),
            MenuItem::action(text("New Tab"), NewTab),
            MenuItem::action(text("New Window"), NewWindow),
            MenuItem::action(text("Open…"), OpenFile),
            MenuItem::action(text("Open Folder…"), OpenFolder),
            MenuItem::action(text("Open Location…"), OpenPath),
            MenuItem::separator(),
            MenuItem::action(text("Close Tab"), CloseTab),
            MenuItem::action(text("Save"), Save),
            MenuItem::action(text("Save As…"), SaveAs),
            MenuItem::separator(),
            MenuItem::action(text("Export HTML"), ExportHtml),
        ]),
        Menu::new(text("Edit")).items([
            MenuItem::os_action(text("Undo"), Undo, OsAction::Undo),
            MenuItem::os_action(text("Redo"), Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action(text("Cut"), markrust_editor::Cut, OsAction::Cut),
            MenuItem::os_action(text("Copy"), markrust_editor::Copy, OsAction::Copy),
            MenuItem::os_action(text("Paste"), Paste, OsAction::Paste),
            MenuItem::os_action(
                text("Select All"),
                markrust_editor::SelectAll,
                OsAction::SelectAll,
            ),
            MenuItem::separator(),
            MenuItem::submenu(Menu::new(text("Find")).items([
                MenuItem::action(text("Find…"), Find),
                MenuItem::action(text("Find Next"), FindNext),
                MenuItem::action(text("Find Previous"), FindPrevious),
            ])),
        ]),
        Menu::new(text("Format")).items([
            MenuItem::action(text("Bold"), markrust_editor::ToggleBold),
            MenuItem::action(text("Italic"), markrust_editor::ToggleItalic),
            MenuItem::action(text("Inline Code"), markrust_editor::ToggleCode),
            MenuItem::action(text("Link…"), markrust_editor::ToggleLink),
            MenuItem::action(text("Strikethrough"), markrust_editor::ToggleStrikethrough),
            MenuItem::separator(),
            MenuItem::action(text("Heading 1"), markrust_editor::SetHeading1),
            MenuItem::action(text("Heading 2"), markrust_editor::SetHeading2),
            MenuItem::action(text("Heading 3"), markrust_editor::SetHeading3),
            MenuItem::action(text("Heading 4"), markrust_editor::SetHeading4),
            MenuItem::action(text("Heading 5"), markrust_editor::SetHeading5),
            MenuItem::action(text("Heading 6"), markrust_editor::SetHeading6),
            MenuItem::action(text("Paragraph"), markrust_editor::Paragraph),
            MenuItem::separator(),
            MenuItem::action(text("Bulleted List"), markrust_editor::ToggleUnorderedList),
            MenuItem::action(text("Numbered List"), markrust_editor::ToggleOrderedList),
            MenuItem::action(text("Task List"), markrust_editor::ToggleTaskList),
            MenuItem::action(text("Blockquote"), markrust_editor::ToggleBlockquote),
            MenuItem::action(
                text("Horizontal Rule"),
                markrust_editor::InsertHorizontalRule,
            ),
            MenuItem::separator(),
            MenuItem::action(text("Code Block"), markrust_editor::InsertCodeBlock),
            MenuItem::action(text("Image…"), markrust_editor::InsertImage),
            MenuItem::submenu(Menu::new(text("Table")).items([
                MenuItem::action(text("Insert Table"), markrust_editor::InsertTable),
                MenuItem::separator(),
                MenuItem::action(text("Insert Row Below"), InsertTableRowBelow),
                MenuItem::action(text("Insert Row Above"), InsertTableRowAbove),
                MenuItem::action(text("Delete Row"), DeleteTableRow),
                MenuItem::separator(),
                MenuItem::action(text("Insert Column Right"), InsertTableColumnRight),
                MenuItem::action(text("Insert Column Left"), InsertTableColumnLeft),
                MenuItem::action(text("Delete Column"), DeleteTableColumn),
            ])),
            MenuItem::separator(),
            MenuItem::action(text("Indent"), markrust_editor::Indent),
            MenuItem::action(text("Outdent"), markrust_editor::Outdent),
        ]),
        Menu::new(text("View")).items([
            MenuItem::action(text("WYSIWYG"), ShowWysiwyg)
                .checked(state.mode == EditorMode::Wysiwyg),
            MenuItem::action(text("Source"), ShowSource).checked(state.mode == EditorMode::Source),
            MenuItem::action(text("Split View"), ShowSplit)
                .checked(state.mode == EditorMode::Split),
            MenuItem::action(text("Next Editor Mode"), ToggleEditorMode),
            MenuItem::submenu(language_menu(state.language)),
            MenuItem::separator(),
            MenuItem::action(text("Show Markup Hints"), ToggleMarkupHints)
                .checked(state.markup_hints_enabled),
            MenuItem::separator(),
            MenuItem::action(text("Show Sidebar"), ToggleSidebar).checked(state.sidebar_open),
            MenuItem::action(text("Show Outline"), ToggleOutline).checked(state.outline_open),
            MenuItem::separator(),
            MenuItem::action(text("Command Palette…"), CommandPalette),
            MenuItem::action(text("Load Remote Images"), LoadRemoteImages),
            MenuItem::action(text("Toggle Light / Dark Appearance"), ToggleTheme),
            MenuItem::submenu(
                Menu::new(text("Highlight Colors")).items([
                    MenuItem::action(text("Native"), HighlightNative)
                        .checked(state.highlight_style == HighlightStyle::Native),
                    MenuItem::action(text("Ocean"), HighlightOcean)
                        .checked(state.highlight_style == HighlightStyle::Ocean),
                    MenuItem::action(text("Forest"), HighlightForest)
                        .checked(state.highlight_style == HighlightStyle::Forest),
                ]),
            ),
        ]),
        Menu::new(text("Window")).items([
            MenuItem::action(text("Next Tab"), NextTab),
            MenuItem::action(text("Previous Tab"), PreviousTab),
            MenuItem::separator(),
            MenuItem::action(text("Minimize"), Minimize),
            MenuItem::action(text("Zoom"), Zoom),
        ]),
        Menu::new(text("Help")).items([MenuItem::action(text("MarkRust Help"), Help)]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_menu_exposes_document_find_actions() {
        let menus = application_menus(MenuState::default());
        let edit = menus.iter().find(|menu| menu.name == "Edit").unwrap();
        let find = edit
            .items
            .iter()
            .find_map(|item| match item {
                MenuItem::Submenu(menu) if menu.name == "Find" => Some(menu),
                _ => None,
            })
            .expect("Edit contains the standard Find submenu");
        for (label, expected) in [
            ("Find…", <Find as gpui::Action>::name_for_type()),
            ("Find Next", <FindNext as gpui::Action>::name_for_type()),
            (
                "Find Previous",
                <FindPrevious as gpui::Action>::name_for_type(),
            ),
        ] {
            assert!(find.items.iter().any(|item| matches!(item,
                MenuItem::Action { name, action, .. } if name == label && action.name() == expected
            )), "missing {label}");
        }
    }

    #[test]
    fn new_window_is_an_application_level_file_action() {
        let menus = application_menus(MenuState::default());
        let file = menus.iter().find(|menu| menu.name == "File").unwrap();
        assert!(file.items.iter().any(|item| matches!(item, MenuItem::Action { name, action, .. }
            if name == "New Window" && action.name() == <NewWindow as gpui::Action>::name_for_type())));
    }

    #[test]
    fn file_menu_hides_normalization_from_ordinary_save_actions() {
        let menus = application_menus(MenuState::default());
        let file = menus.iter().find(|menu| menu.name == "File").unwrap();
        let actions: Vec<_> = file
            .items
            .iter()
            .filter_map(|item| {
                if let MenuItem::Action { name, action, .. } = item {
                    Some((name.as_str(), action.name()))
                } else {
                    None
                }
            })
            .collect();
        assert!(actions.contains(&("Save", <Save as gpui::Action>::name_for_type())));
        assert!(actions.contains(&("Save As…", <SaveAs as gpui::Action>::name_for_type())));
        assert!(
            !actions.iter().any(|(_, action)| {
                *action == <NormalizeMarkdown as gpui::Action>::name_for_type()
            }),
            "normalization remains an opt-in internal action, not a File menu save path"
        );
    }

    #[test]
    fn highlight_color_menu_tracks_the_current_palette() {
        for style in [
            HighlightStyle::Native,
            HighlightStyle::Ocean,
            HighlightStyle::Forest,
        ] {
            let menus = application_menus(MenuState {
                highlight_style: style,
                ..MenuState::default()
            });
            let view = menus.iter().find(|menu| menu.name == "View").unwrap();
            let colors = view
                .items
                .iter()
                .find_map(|item| match item {
                    MenuItem::Submenu(menu) if menu.name == "Highlight Colors" => Some(menu),
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                colors.items.iter().filter(|item| item.is_checked()).count(),
                1
            );
            let expected = match style {
                HighlightStyle::Native => "Native",
                HighlightStyle::Ocean => "Ocean",
                HighlightStyle::Forest => "Forest",
            };
            assert!(colors.items.iter().any(
                |item| matches!(item, MenuItem::Action { name, .. } if name == expected)
                    && item.is_checked()
            ));
        }
    }

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

    #[test]
    fn language_menu_has_twenty_distinct_actions_and_one_live_selection() {
        for language in crate::i18n::Language::ALL {
            let menu = language_menu(language);
            assert_eq!(menu.items.len(), 20);
            let selected: Vec<_> = menu.items.iter().filter(|item| item.is_checked()).collect();
            assert_eq!(selected.len(), 1);
            assert!(
                matches!(selected[0], MenuItem::Action { name, .. } if name == language.native_name())
            );
            let actions: std::collections::BTreeSet<_> = menu
                .items
                .iter()
                .map(|item| {
                    let MenuItem::Action { action, .. } = item else {
                        panic!("language must be an action")
                    };
                    action.name().to_string()
                })
                .collect();
            assert_eq!(actions.len(), 20);
            let menus = application_menus(MenuState {
                language,
                ..MenuState::default()
            });
            assert!(menus
                .iter()
                .any(|menu| menu.name == crate::i18n::text(language, "View")));
        }
    }

    #[test]
    fn markup_hints_menu_tracks_the_preference() {
        for enabled in [true, false] {
            let menus = application_menus(MenuState {
                markup_hints_enabled: enabled,
                ..MenuState::default()
            });
            let view = menus.iter().find(|menu| menu.name == "View").unwrap();
            let item = view.items.iter().find(
                |item| matches!(item, MenuItem::Action { name, .. } if name == "Show Markup Hints"),
            );
            assert_eq!(item.unwrap().is_checked(), enabled);
        }
    }

    #[test]
    fn format_menu_exposes_standard_format_commands() {
        // Table operations live in their own submenu because they are
        // structural commands, not persistent editor controls.
        use gpui::Action;
        let menus = application_menus(MenuState::default());
        let format = menus.iter().find(|menu| menu.name == "Format").unwrap();
        let expected: &[(&str, std::borrow::Cow<'static, str>)] = &[
            (
                "Bold",
                std::borrow::Cow::Borrowed(<markrust_editor::ToggleBold as Action>::name_for_type()),
            ),
            (
                "Italic",
                std::borrow::Cow::Borrowed(
                    <markrust_editor::ToggleItalic as Action>::name_for_type(),
                ),
            ),
            (
                "Inline Code",
                std::borrow::Cow::Borrowed(<markrust_editor::ToggleCode as Action>::name_for_type()),
            ),
            (
                "Link…",
                std::borrow::Cow::Borrowed(<markrust_editor::ToggleLink as Action>::name_for_type()),
            ),
            (
                "Strikethrough",
                std::borrow::Cow::Borrowed(
                    <markrust_editor::ToggleStrikethrough as Action>::name_for_type(),
                ),
            ),
            (
                "Heading 1",
                std::borrow::Cow::Borrowed(
                    <markrust_editor::SetHeading1 as Action>::name_for_type(),
                ),
            ),
            (
                "Heading 2",
                std::borrow::Cow::Borrowed(
                    <markrust_editor::SetHeading2 as Action>::name_for_type(),
                ),
            ),
            (
                "Heading 3",
                std::borrow::Cow::Borrowed(
                    <markrust_editor::SetHeading3 as Action>::name_for_type(),
                ),
            ),
            (
                "Bulleted List",
                std::borrow::Cow::Borrowed(
                    <markrust_editor::ToggleUnorderedList as Action>::name_for_type(),
                ),
            ),
            (
                "Numbered List",
                std::borrow::Cow::Borrowed(
                    <markrust_editor::ToggleOrderedList as Action>::name_for_type(),
                ),
            ),
            (
                "Task List",
                std::borrow::Cow::Borrowed(
                    <markrust_editor::ToggleTaskList as Action>::name_for_type(),
                ),
            ),
            (
                "Blockquote",
                std::borrow::Cow::Borrowed(
                    <markrust_editor::ToggleBlockquote as Action>::name_for_type(),
                ),
            ),
            (
                "Horizontal Rule",
                std::borrow::Cow::Borrowed(
                    <markrust_editor::InsertHorizontalRule as Action>::name_for_type(),
                ),
            ),
            (
                "Code Block",
                std::borrow::Cow::Borrowed(
                    <markrust_editor::InsertCodeBlock as Action>::name_for_type(),
                ),
            ),
            (
                "Image…",
                std::borrow::Cow::Borrowed(
                    <markrust_editor::InsertImage as Action>::name_for_type(),
                ),
            ),
            (
                "Indent",
                std::borrow::Cow::Borrowed(<markrust_editor::Indent as Action>::name_for_type()),
            ),
            (
                "Outdent",
                std::borrow::Cow::Borrowed(<markrust_editor::Outdent as Action>::name_for_type()),
            ),
        ];
        for (label, action_name) in expected {
            let found = format.items.iter().any(|item| match item {
                MenuItem::Action {
                    name: n, action: a, ..
                } => n.as_ref() == *label && a.name() == *action_name,
                _ => false,
            });
            assert!(
                found,
                "Format menu is missing `{label}` (action {action_name})"
            );
        }
    }

    #[test]
    fn format_menu_exposes_table_structure_commands_in_a_submenu() {
        use gpui::Action;

        let menus = application_menus(MenuState::default());
        let format = menus.iter().find(|menu| menu.name == "Format").unwrap();
        let table = format
            .items
            .iter()
            .find_map(|item| match item {
                MenuItem::Submenu(menu) if menu.name == "Table" => Some(menu),
                _ => None,
            })
            .expect("Format menu must expose a Table submenu");
        let expected: &[(&str, std::borrow::Cow<'static, str>)] = &[
            (
                "Insert Table",
                std::borrow::Cow::Borrowed(
                    <markrust_editor::InsertTable as Action>::name_for_type(),
                ),
            ),
            (
                "Insert Row Below",
                std::borrow::Cow::Borrowed(<InsertTableRowBelow as Action>::name_for_type()),
            ),
            (
                "Insert Row Above",
                std::borrow::Cow::Borrowed(<InsertTableRowAbove as Action>::name_for_type()),
            ),
            (
                "Delete Row",
                std::borrow::Cow::Borrowed(<DeleteTableRow as Action>::name_for_type()),
            ),
            (
                "Insert Column Right",
                std::borrow::Cow::Borrowed(<InsertTableColumnRight as Action>::name_for_type()),
            ),
            (
                "Insert Column Left",
                std::borrow::Cow::Borrowed(<InsertTableColumnLeft as Action>::name_for_type()),
            ),
            (
                "Delete Column",
                std::borrow::Cow::Borrowed(<DeleteTableColumn as Action>::name_for_type()),
            ),
        ];

        for (label, action_name) in expected {
            assert!(
                table.items.iter().any(|item| matches!(
                    item,
                    MenuItem::Action { name, action, .. }
                        if name.as_ref() == *label && action.name() == *action_name
                )),
                "Table submenu is missing `{label}` (action {action_name})"
            );
        }
    }
}
