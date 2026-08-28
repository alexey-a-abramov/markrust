// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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
    application().run(|cx: &mut App| {
        let config = AppConfig::load();
        cx.bind_keys([
            KeyBinding::new("cmd-s", crate::window::Save, None),
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
            |window, cx| {
                let workspace = cx.new(|cx| Workspace::new(config, window, cx));
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
