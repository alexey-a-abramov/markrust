// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use futures::{channel::mpsc, StreamExt};
use gpui::{
    px, size, App, AppContext, BorrowAppContext, Bounds, Global, KeyBinding, Subscription,
    WeakEntity, WindowBounds, WindowHandle, WindowId, WindowOptions,
};
use gpui_platform::application;

use crate::config::AppConfig;
use crate::crash::{self, OpenOrigin};
use crate::recovery::{RecoveryStore, RecoveryWarning};
use crate::window::MarkRustWindow;
use crate::workspace::Workspace;

/// GPUI revision pinned in `Cargo.toml` for reproducible builds.
pub const GPUI_GIT_REV: &str = "8166e3d7b8b42d8aaf4d4dee7fcd25ab4ec65105";

#[derive(Default)]
struct ActivationOrder<T> {
    recent: Vec<T>,
}

impl<T: Copy + PartialEq> ActivationOrder<T> {
    fn activate(&mut self, id: T) {
        self.recent.retain(|previous| *previous != id);
        self.recent.push(id);
    }

    fn remove(&mut self, id: T) {
        self.recent.retain(|previous| *previous != id);
    }

    fn latest_live(&self, live: &[T]) -> Option<T> {
        self.recent
            .iter()
            .rev()
            .copied()
            .find(|id| live.contains(id))
    }
}

#[derive(Clone)]
struct RegisteredWindow {
    handle: WindowHandle<MarkRustWindow>,
    workspace: WeakEntity<Workspace>,
}

struct DesktopWindows {
    config: AppConfig,
    windows: HashMap<WindowId, RegisteredWindow>,
    activation: ActivationOrder<WindowId>,
    _closed_subscription: Subscription,
    #[cfg(feature = "gui-tests")]
    test_recovery_root: Option<PathBuf>,
    #[cfg(feature = "gui-tests")]
    test_window_sequence: usize,
}

impl Global for DesktopWindows {}

fn initialize_window_registry(config: AppConfig, cx: &mut App) {
    let closed_subscription = cx.on_window_closed(|cx, id| {
        if cx.try_global::<DesktopWindows>().is_some() {
            cx.update_global::<DesktopWindows, _>(|registry, _| {
                registry.windows.remove(&id);
                registry.activation.remove(id);
            });
        }
    });
    cx.set_global(DesktopWindows {
        config,
        windows: HashMap::new(),
        activation: ActivationOrder { recent: Vec::new() },
        _closed_subscription: closed_subscription,
        #[cfg(feature = "gui-tests")]
        test_recovery_root: None,
        #[cfg(feature = "gui-tests")]
        test_window_sequence: 0,
    });
}

pub(crate) fn note_window_activated(handle: gpui::AnyWindowHandle, cx: &mut App) {
    if cx.try_global::<DesktopWindows>().is_some() {
        cx.update_global::<DesktopWindows, _>(|registry, _| {
            if registry.windows.contains_key(&handle.window_id()) {
                registry.activation.activate(handle.window_id());
            }
        });
    }
}

fn register_window(handle: WindowHandle<MarkRustWindow>, cx: &mut App) -> anyhow::Result<()> {
    let workspace = handle.read_with(cx, |root, _| root.workspace.downgrade())?;
    cx.update_global::<DesktopWindows, _>(|registry, _| {
        registry
            .windows
            .insert(handle.window_id(), RegisteredWindow { handle, workspace });
        registry.activation.activate(handle.window_id());
    });
    Ok(())
}

fn last_active_window(cx: &App) -> Option<WindowHandle<MarkRustWindow>> {
    let registry = cx.try_global::<DesktopWindows>()?;
    let live: Vec<_> = cx
        .windows()
        .iter()
        .map(|handle| handle.window_id())
        .collect();
    let current = cx
        .active_window()
        .filter(|handle| registry.windows.contains_key(&handle.window_id()));
    // Activation callbacks are authoritative; the platform's current-window
    // lookup can lag a just-created or programmatically activated window.
    let id = registry
        .activation
        .latest_live(&live)
        .or_else(|| current.map(|handle| handle.window_id()))?;
    registry.windows.get(&id).map(|entry| entry.handle)
}

/// UI language is a desktop preference, not a per-document property. Refresh
/// every registered workspace, preserving buffers, input owners and history.
pub(crate) fn set_ui_language(language: crate::i18n::Language, cx: &mut App) {
    let Some(registry) = cx.try_global::<DesktopWindows>() else {
        return;
    };
    let workspaces: Vec<_> = registry
        .windows
        .values()
        .map(|entry| entry.workspace.clone())
        .collect();
    cx.update_global::<DesktopWindows, _>(|registry, _| registry.config.language = language);
    for workspace in workspaces {
        if let Some(workspace) = workspace.upgrade() {
            workspace.update(cx, |workspace, cx| workspace.set_ui_language(language, cx));
        }
    }
    let mut menu = cx
        .try_global::<crate::menus::MenuState>()
        .copied()
        .unwrap_or_default();
    menu.language = language;
    crate::menus::sync(menu, cx);
    cx.refresh_windows();
}

fn create_application_window(
    config: AppConfig,
    store: Option<RecoveryStore>,
    dimensions: gpui::Size<gpui::Pixels>,
    isolated_test: bool,
    cx: &mut App,
) -> anyhow::Result<WindowHandle<MarkRustWindow>> {
    let bounds = Bounds::centered(None, dimensions, cx);
    let handle = cx.open_window(
        WindowOptions {
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("MarkRust".into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(680.), px(420.))),
            icon: load_window_icon(),
            show: !isolated_test,
            focus: !isolated_test,
            ..Default::default()
        },
        move |window, cx| {
            let workspace = cx.new(|cx| {
                #[cfg(feature = "gui-tests")]
                if isolated_test {
                    return match store {
                        Some(store) => Workspace::new_for_recovery_tests(config, store, window, cx),
                        None => Workspace::new_for_gui_tests(config, window, cx),
                    };
                }
                Workspace::new_with_recovery_store(config, store, window, cx)
            });
            cx.new(|cx| {
                let mut view = MarkRustWindow::new(workspace, cx);
                view.attach_application_window(window, cx);
                crash::record_window_ready();
                view
            })
        },
    )?;
    register_window(handle, cx)?;
    Ok(handle)
}

pub(crate) fn new_application_window(cx: &mut App) -> anyhow::Result<WindowHandle<MarkRustWindow>> {
    let registry = cx
        .try_global::<DesktopWindows>()
        .ok_or_else(|| anyhow::anyhow!("the application window registry is unavailable"))?;
    let config = last_active_window(cx)
        .and_then(|handle| registry.windows.get(&handle.window_id()))
        .and_then(|entry| entry.workspace.upgrade())
        .map(|workspace| workspace.read(cx).config.clone())
        .unwrap_or_else(|| registry.config.clone());
    #[cfg(feature = "gui-tests")]
    if let Some(directory) = cx.global::<DesktopWindows>().test_recovery_root.clone() {
        let sequence = cx.update_global::<DesktopWindows, _>(|registry, _| {
            registry.test_window_sequence += 1;
            registry.test_window_sequence
        });
        let directory = directory.join(format!("new-window-{sequence}"));
        std::fs::create_dir(&directory)?;
        return create_application_window(
            config,
            Some(RecoveryStore::new(directory)),
            size(px(1200.), px(800.)),
            true,
            cx,
        );
    }
    create_application_window(
        config,
        Some(RecoveryStore::production_fresh()?),
        size(px(1200.), px(800.)),
        false,
        cx,
    )
}

pub(crate) fn route_external_open(
    path: PathBuf,
    origin: OpenOrigin,
    cx: &mut App,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !crate::update_ui::is_installing(cx),
        "MarkRust is restarting for an update; reopen this path after restart"
    );
    let handle = match last_active_window(cx) {
        Some(handle) => handle,
        None => new_application_window(cx)?,
    };
    handle.update(cx, |view, window, cx| {
        window.activate_window();
        view.open_external_path(path, origin, window, cx)
    })??;
    Ok(())
}

pub(crate) fn reopen_application(cx: &mut App) -> anyhow::Result<()> {
    let handle = match last_active_window(cx) {
        Some(handle) => handle,
        None => new_application_window(cx)?,
    };
    handle.update(cx, |view, window, cx| {
        window.activate_window();
        view.focus_visible_surface(window, cx);
    })?;
    Ok(())
}

pub(crate) fn quit_application_checked(cx: &mut App) {
    if checkpoint_application(cx) {
        cx.quit();
    }
}

/// Checkpoint every window in the same event turn before allowing an update
/// helper's start gate to open. Failure retains all live editors.
pub(crate) fn checkpoint_application(cx: &mut App) -> bool {
    let windows: Vec<_> = cx
        .try_global::<DesktopWindows>()
        .map(|registry| registry.windows.values().cloned().collect())
        .unwrap_or_default();
    let mut safe = true;
    let mut failed_window = None;
    for entry in windows {
        if let Some(workspace) = entry.workspace.upgrade() {
            let written = workspace
                .update(cx, |workspace, cx| {
                    workspace.checkpoint_before_close_or_quit(cx)
                })
                .is_ok();
            safe &= written;
            if !written {
                failed_window = Some(entry.handle);
            }
        }
    }
    if !safe {
        if let Some(handle) = failed_window {
            let _ = handle.update(cx, |_, window, cx| {
                window.activate_window();
                cx.notify();
            });
        }
    }
    safe
}

pub(crate) fn can_restart_for_update(cx: &mut App) -> bool {
    let windows: Vec<_> = cx
        .try_global::<DesktopWindows>()
        .map(|registry| {
            registry
                .windows
                .values()
                .map(|entry| entry.handle)
                .collect()
        })
        .unwrap_or_default();
    windows.into_iter().all(|handle| {
        handle
            .update(cx, |view, _, cx| view.can_restart_for_update(cx))
            .unwrap_or(false)
    })
}

pub(crate) fn set_automatic_updates(enabled: bool, cx: &mut App) {
    let Some(registry) = cx.try_global::<DesktopWindows>() else {
        return;
    };
    let workspaces: Vec<_> = registry
        .windows
        .values()
        .map(|entry| entry.workspace.clone())
        .collect();
    let config = cx.update_global::<DesktopWindows, _>(|registry, _| {
        registry.config.automatic_updates = enabled;
        registry.config.clone()
    });
    if let Err(error) = config.save() {
        eprintln!("MarkRust update preference could not be saved: {error}");
    }
    for workspace in workspaces {
        if let Some(workspace) = workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.config.automatic_updates = enabled;
                cx.notify();
            });
        }
    }
    let mut menu = cx
        .try_global::<crate::menus::MenuState>()
        .copied()
        .unwrap_or_default();
    menu.automatic_updates = enabled;
    crate::menus::sync(menu, cx);
}

#[cfg(feature = "gui-tests")]
pub(crate) fn test_initialize_window_registry(
    config: AppConfig,
    recovery_root: PathBuf,
    cx: &mut App,
) {
    initialize_window_registry(config, cx);
    cx.update_global::<DesktopWindows, _>(|registry, _| {
        registry.test_recovery_root = Some(recovery_root)
    });
}

#[cfg(feature = "gui-tests")]
pub(crate) fn test_create_application_window(
    config: AppConfig,
    store: RecoveryStore,
    dimensions: gpui::Size<gpui::Pixels>,
    cx: &mut App,
) -> anyhow::Result<WindowHandle<MarkRustWindow>> {
    create_application_window(config, Some(store), dimensions, true, cx)
}

#[cfg(feature = "gui-tests")]
pub(crate) fn test_last_active_window(cx: &App) -> Option<WindowHandle<MarkRustWindow>> {
    last_active_window(cx)
}

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
    if cx.text_system().add_fonts(fonts).is_err() {
        crash::record_fonts_load_failed();
        eprintln!("Failed to load bundled Inter fonts");
    }
}

/// Launch the desktop editor, optionally opening a file or workspace folder.
pub fn run_gui_with_open(open_path: Option<PathBuf>) {
    crash::install_panic_logger();
    crash::record_application_started();
    let app = application();
    let (open_tx, mut open_rx) = mpsc::unbounded::<PathBuf>();
    app.on_open_urls(move |urls| {
        for url in urls {
            if let Some(path) = url::Url::parse(&url)
                .ok()
                .and_then(|url| url.to_file_path().ok())
            {
                let _ = open_tx.unbounded_send(path);
            }
        }
    });
    app.on_reopen(|cx| {
        if cx.try_global::<DesktopWindows>().is_some() {
            if let Err(error) = reopen_application(cx) {
                eprintln!("MarkRust could not reopen a window: {error}");
            }
        }
    });
    app.run(move |cx: &mut App| {
        load_bundled_fonts(cx);
        let config = AppConfig::load();
        cx.bind_keys(desktop_key_bindings());
        crate::menus::init(cx);
        initialize_window_registry(config.clone(), cx);
        crate::update_ui::initialize(config.automatic_updates, cx);
        let (sessions, mut discovery_warning) = match RecoveryStore::production_sessions() {
            Ok(sessions) => (sessions, None),
            Err(error) => (Vec::new(), Some(format!("Private recovery could not be discovered: {error}. Previously retained drafts have not been removed. Save current work explicitly and retry recovery before assuming the session is empty."))),
        };
        for store in sessions {
            if let Err(error) = create_application_window(
                config.clone(),
                Some(store),
                size(px(1200.), px(800.)),
                false,
                cx,
            ) {
                eprintln!("MarkRust could not restore a window: {error}");
                discovery_warning = Some(format!("A private recovery window could not be restored: {error}. Its retained drafts have not been removed. Save current work explicitly and retry recovery."));
            }
        }
        if last_active_window(cx).is_none() {
            if let Err(error) = new_application_window(cx) {
                eprintln!("MarkRust recovery is unavailable: {error}");
                create_application_window(config, None, size(px(1200.), px(800.)), false, cx)
                    .expect("failed to open MarkRust window");
            }
        }
        if let Some(message) = discovery_warning {
            if let Some(handle) = last_active_window(cx) {
                let _ = handle.update(cx, |view, _, cx| {
                    view.workspace.update(cx, |workspace, cx| workspace.set_recovery_warning(RecoveryWarning::ReadFailed(message), cx));
                });
            }
        }
        if let Some(path) = open_path {
            if let Err(error) = route_external_open(path, OpenOrigin::Launch, cx) {
                eprintln!("Failed to open launch document: {error}");
            }
        }
        // This receiver belongs to the application, not to its first window.
        // Finder events keep working after that window closes.
        cx.spawn(async move |cx| {
            while let Some(path) = open_rx.next().await {
                cx.update(|cx| {
                    if let Err(error) = route_external_open(path, OpenOrigin::Finder, cx) {
                        eprintln!("Failed to open external document: {error}");
                    }
                });
            }
        })
        .detach();
        cx.activate(true);
    });
    crash::record_application_stopped();
}

pub(crate) fn desktop_key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-q", crate::menus::Quit, None),
        KeyBinding::new("cmd-h", crate::menus::Hide, None),
        KeyBinding::new("alt-cmd-h", crate::menus::HideOthers, None),
        KeyBinding::new("cmd-m", crate::window::Minimize, None),
        KeyBinding::new("ctrl-cmd-f", crate::window::ToggleFullScreen, None),
        KeyBinding::new("cmd-s", crate::window::Save, None),
        KeyBinding::new("ctrl-s", crate::window::Save, None),
        KeyBinding::new("cmd-shift-s", crate::window::SaveAs, None),
        KeyBinding::new("ctrl-shift-s", crate::window::SaveAs, None),
        KeyBinding::new("cmd-shift-m", crate::window::ToggleEditorMode, None),
        KeyBinding::new("ctrl-shift-m", crate::window::ToggleEditorMode, None),
        // Mode picker shortcuts moved off Cmd-1/2/3 to free Cmd-1..6 for
        // ATX heading toggles (the standard set in iA Writer, Typora,
        // Obsidian).
        KeyBinding::new("alt-cmd-1", crate::window::ShowWysiwyg, None),
        KeyBinding::new("alt-ctrl-1", crate::window::ShowWysiwyg, None),
        KeyBinding::new("alt-cmd-2", crate::window::ShowSource, None),
        KeyBinding::new("alt-ctrl-2", crate::window::ShowSource, None),
        KeyBinding::new("alt-cmd-3", crate::window::ShowSplit, None),
        KeyBinding::new("alt-ctrl-3", crate::window::ShowSplit, None),
        KeyBinding::new("alt-cmd-4", crate::window::ToggleMarkupHints, None),
        KeyBinding::new("alt-ctrl-4", crate::window::ToggleMarkupHints, None),
        KeyBinding::new("ctrl-cmd-s", crate::window::ToggleSidebar, None),
        KeyBinding::new("ctrl-cmd-o", crate::window::ToggleOutline, None),
        KeyBinding::new("cmd-o", crate::window::OpenFile, None),
        KeyBinding::new("ctrl-o", crate::window::OpenFile, None),
        KeyBinding::new("cmd-shift-o", crate::window::OpenFolder, None),
        KeyBinding::new("ctrl-shift-o", crate::window::OpenFolder, None),
        KeyBinding::new("shift-cmd-l", crate::window::OpenPath, None),
        KeyBinding::new("shift-ctrl-l", crate::window::OpenPath, None),
        KeyBinding::new("cmd-n", crate::window::NewDocument, None),
        KeyBinding::new("cmd-shift-n", crate::menus::NewWindow, None),
        KeyBinding::new("ctrl-shift-n", crate::menus::NewWindow, None),
        KeyBinding::new("ctrl-n", crate::window::NewDocument, None),
        KeyBinding::new("cmd-t", crate::window::NewTab, None),
        KeyBinding::new("ctrl-t", crate::window::NewTab, None),
        KeyBinding::new("ctrl-tab", crate::window::NextTab, None),
        KeyBinding::new("ctrl-shift-tab", crate::window::PreviousTab, None),
        KeyBinding::new("cmd-shift-]", crate::window::NextTab, None),
        KeyBinding::new("cmd-shift-[", crate::window::PreviousTab, None),
        KeyBinding::new("cmd-w", crate::window::CloseTab, None),
        KeyBinding::new("ctrl-w", crate::window::CloseTab, None),
        KeyBinding::new("cmd-p", crate::window::CommandPalette, None),
        KeyBinding::new("ctrl-p", crate::window::CommandPalette, None),
        KeyBinding::new("cmd-z", crate::window::Undo, None),
        KeyBinding::new("ctrl-z", crate::window::Undo, None),
        KeyBinding::new("cmd-shift-z", crate::window::Redo, None),
        KeyBinding::new("ctrl-shift-z", crate::window::Redo, None),
        KeyBinding::new("ctrl-y", crate::window::Redo, None),
        KeyBinding::new("cmd-f", crate::window::Find, None),
        KeyBinding::new("ctrl-f", crate::window::Find, None),
        KeyBinding::new("cmd-g", crate::window::FindNext, None),
        KeyBinding::new("ctrl-g", crate::window::FindNext, None),
        KeyBinding::new("shift-cmd-g", crate::window::FindPrevious, None),
        KeyBinding::new("shift-ctrl-g", crate::window::FindPrevious, None),
        KeyBinding::new("f3", crate::window::FindNext, None),
        KeyBinding::new("shift-f3", crate::window::FindPrevious, None),
        // Keep the conventional Reopen Closed Tab chord free for that action.
        KeyBinding::new("alt-shift-cmd-t", crate::window::ToggleTheme, None),
        KeyBinding::new("alt-shift-ctrl-t", crate::window::ToggleTheme, None),
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
        KeyBinding::new("ctrl-a", markrust_editor::SelectAll, None),
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
    fn activation_order_tracks_reactivation_and_closed_windows() {
        let mut order = ActivationOrder::<u32>::default();
        order.activate(1);
        order.activate(2);
        order.activate(1);
        assert_eq!(order.recent, [2, 1]);
        assert_eq!(order.latest_live(&[1, 2]), Some(1));
        assert_eq!(order.latest_live(&[2]), Some(2));
        order.remove(1);
        assert_eq!(order.latest_live(&[1, 2]), Some(2));
        order.remove(2);
        assert_eq!(order.latest_live(&[1, 2]), None);
    }

    #[test]
    fn new_window_shortcut_is_distinct_from_document_and_tab() {
        assert_eq!(
            action_for("cmd-shift-n"),
            crate::menus::NewWindow::name_for_type()
        );
        assert_eq!(
            action_for("ctrl-shift-n"),
            crate::menus::NewWindow::name_for_type()
        );
        assert_eq!(
            action_for("cmd-n"),
            crate::window::NewDocument::name_for_type()
        );
        assert_eq!(action_for("cmd-t"), crate::window::NewTab::name_for_type());
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
    fn standard_file_and_edit_shortcuts_have_cmd_ctrl_parity() {
        for (cmd, ctrl, expected) in [
            ("cmd-s", "ctrl-s", crate::window::Save::name_for_type()),
            (
                "cmd-shift-s",
                "ctrl-shift-s",
                crate::window::SaveAs::name_for_type(),
            ),
            ("cmd-o", "ctrl-o", crate::window::OpenFile::name_for_type()),
            (
                "cmd-shift-o",
                "ctrl-shift-o",
                crate::window::OpenFolder::name_for_type(),
            ),
            (
                "cmd-shift-l",
                "ctrl-shift-l",
                crate::window::OpenPath::name_for_type(),
            ),
            ("cmd-w", "ctrl-w", crate::window::CloseTab::name_for_type()),
            ("cmd-z", "ctrl-z", crate::window::Undo::name_for_type()),
            (
                "cmd-shift-z",
                "ctrl-shift-z",
                crate::window::Redo::name_for_type(),
            ),
            (
                "cmd-a",
                "ctrl-a",
                markrust_editor::SelectAll::name_for_type(),
            ),
            ("cmd-c", "ctrl-c", markrust_editor::Copy::name_for_type()),
            ("cmd-x", "ctrl-x", markrust_editor::Cut::name_for_type()),
            ("cmd-v", "ctrl-v", crate::window::Paste::name_for_type()),
            (
                "cmd-p",
                "ctrl-p",
                crate::window::CommandPalette::name_for_type(),
            ),
            (
                "cmd-shift-m",
                "ctrl-shift-m",
                crate::window::ToggleEditorMode::name_for_type(),
            ),
        ] {
            assert_eq!(action_for(cmd), expected, "{cmd}");
            assert_eq!(action_for(ctrl), expected, "{ctrl}");
        }
        assert_eq!(action_for("ctrl-y"), crate::window::Redo::name_for_type());
    }

    #[test]
    fn document_find_shortcuts_are_conventional() {
        for key in ["cmd-f", "ctrl-f"] {
            assert_eq!(action_for(key), crate::window::Find::name_for_type());
        }
        for key in ["cmd-g", "ctrl-g", "f3"] {
            assert_eq!(action_for(key), crate::window::FindNext::name_for_type());
        }
        for key in ["shift-cmd-g", "shift-ctrl-g", "shift-f3"] {
            assert_eq!(
                action_for(key),
                crate::window::FindPrevious::name_for_type()
            );
        }
        assert_eq!(
            action_for("alt-shift-cmd-t"),
            crate::window::ToggleTheme::name_for_type()
        );
        assert_eq!(
            action_for("alt-shift-ctrl-t"),
            crate::window::ToggleTheme::name_for_type()
        );
        let reopen = Keystroke::parse("shift-cmd-t").unwrap();
        assert!(!desktop_key_bindings().iter().any(|binding| binding
            .match_keystrokes(std::slice::from_ref(&reopen))
            == Some(false)
            && binding.action().name() == crate::window::ToggleTheme::name_for_type()));
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

    #[test]
    fn document_tabs_have_conventional_creation_and_wrapping_navigation_shortcuts() {
        for key in ["cmd-n", "ctrl-n"] {
            assert_eq!(action_for(key), crate::window::NewDocument::name_for_type());
        }
        for key in ["cmd-t", "ctrl-t"] {
            assert_eq!(action_for(key), crate::window::NewTab::name_for_type());
        }
        for key in ["ctrl-tab", "cmd-shift-]"] {
            assert_eq!(action_for(key), crate::window::NextTab::name_for_type());
        }
        for key in ["ctrl-shift-tab", "cmd-shift-["] {
            assert_eq!(action_for(key), crate::window::PreviousTab::name_for_type());
        }
    }
}
