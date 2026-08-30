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
        cx.bind_keys([
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
        ]);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpui_rev_is_non_empty() {
        assert_eq!(GPUI_GIT_REV.len(), 40);
    }
}
