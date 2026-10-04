// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native regressions for the desktop-window and private-draft contracts.
//!
//! Every fixture owns a unique directory below the GUI runner output. It never
//! opens a user document, reads the production recovery location, or relies on
//! a native filesystem watcher. The checks use real GPUI windows so Finder
//! routing, close gating, recovery restore, and editor observers all take the
//! same paths as the desktop application.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{ensure, Context as _, Result};
use gpui::{
    px, size, AppContext, Entity, EntityInputHandler, Focusable, HeadlessAppContext, Keystroke,
    WindowHandle,
};
use serde_json::json;

use crate::app;
use crate::config::{AppConfig, ThemeChoice};
use crate::crash::OpenOrigin;
use crate::recovery::{
    RecoveryEditingPane, RecoveryEditorMode, RecoverySelection, RecoverySnapshot, RecoveryStore,
    RecoveryTab, RecoveryWidgetDraft, RecoveryWidgetKind, RECOVERY_VERSION,
};
use crate::window::MarkRustWindow;
use crate::workspace::{EditingPane, EditorMode, Workspace};
use markrust_core::rich::RichCommand;
use markrust_core::Document;
use markrust_editor::EditorCommand;

const WIDTH: f32 = 960.;
const HEIGHT: f32 = 720.;
const CHECKPOINT_WAIT: Duration = Duration::from_secs(2);

static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Exercise app-global routing and window-local recovery with real native
/// windows. The returned value is a native-state count, not a pixel baseline.
pub(crate) fn check(
    cx: &mut HeadlessAppContext,
    output: &Path,
    geometry_only: bool,
) -> Result<usize> {
    let root = fixture_directory(output, "notepad-window-recovery")?;
    let session_root = root.join("window-sessions");
    std::fs::create_dir(&session_root).with_context(|| {
        format!(
            "create isolated application recovery root {}",
            session_root.display()
        )
    })?;
    initialize_window_registry(cx, config(ThemeChoice::Dark), session_root);

    check_last_active_window_routes_and_queues(cx, &root)?;
    check_private_widget_draft_checkpoint_and_restart(cx, &root, geometry_only)?;
    check_stale_widget_draft_becomes_pathless_scratch(cx, &root)?;
    check_body_ime_preedit_becomes_pathless_scratch(cx, &root)?;
    check_dirty_closed_tab_reopens_pathless(cx, &root)?;
    check_failed_window_close_keeps_live_owner(cx, &root)?;
    check_changed_disk_restart_blocks_autosave(cx, &root)?;

    println!(
        "PASS notepad-window-recovery (last-active routing, queued Finder open, raw widget draft, archive, fail-closed close, changed-disk restart)"
    );
    Ok(7)
}

fn config(theme: ThemeChoice) -> AppConfig {
    AppConfig {
        theme,
        ..AppConfig::default()
    }
}

fn fixture_directory(output: &Path, label: &str) -> Result<PathBuf> {
    let directory = output.join(format!(
        "{label}-{}-{}",
        std::process::id(),
        FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory)
        .with_context(|| format!("create native fixture directory {}", directory.display()))?;
    std::fs::canonicalize(&directory).with_context(|| {
        format!(
            "canonicalize native fixture directory {}",
            directory.display()
        )
    })
}

fn create_window(
    cx: &mut HeadlessAppContext,
    store: RecoveryStore,
    theme: ThemeChoice,
) -> Result<(WindowHandle<MarkRustWindow>, Entity<Workspace>)> {
    let window = cx.update(|cx| {
        app::test_create_application_window(config(theme), store, size(px(WIDTH), px(HEIGHT)), cx)
    })?;
    let root = window.root(cx)?;
    let workspace = cx.read_entity(&root, |root, _| root.workspace.clone());
    draw(cx, window)?;
    Ok((window, workspace))
}

fn initialize_window_registry(
    cx: &mut HeadlessAppContext,
    config: AppConfig,
    recovery_root: PathBuf,
) {
    cx.update(|cx| app::test_initialize_window_registry(config, recovery_root, cx));
}

fn route_external_open(
    cx: &mut HeadlessAppContext,
    path: PathBuf,
    origin: OpenOrigin,
) -> Result<()> {
    cx.update(|cx| app::route_external_open(path, origin, cx))
}

fn last_active_window(cx: &mut HeadlessAppContext) -> Option<WindowHandle<MarkRustWindow>> {
    cx.update(|cx| app::test_last_active_window(cx))
}

fn draw(cx: &mut HeadlessAppContext, window: WindowHandle<MarkRustWindow>) -> Result<()> {
    for _ in 0..3 {
        cx.advance_clock(Duration::from_millis(35));
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| {
            window.simulate_next_frame(cx);
            window.refresh();
            window.draw(cx).clear(cx);
        })?;
    }
    Ok(())
}

fn advance(cx: &mut HeadlessAppContext, duration: Duration) {
    cx.advance_clock(duration);
    cx.run_until_parked();
}

fn activate_window(
    cx: &mut HeadlessAppContext,
    handle: WindowHandle<MarkRustWindow>,
) -> Result<()> {
    cx.update_window(handle.into(), |_, window, cx| {
        window.activate_window();
        // Native activation events call this exact production registry path.
        // Headless AppKit activation timing is not deterministic, so invoke it
        // after requesting activation rather than relying on a stale pointer.
        app::note_window_activated(window.window_handle(), cx);
    })?;
    draw(cx, handle)
}

fn active_workspace_tab_count(cx: &HeadlessAppContext, workspace: &Entity<Workspace>) -> usize {
    cx.read_entity(workspace, |workspace, _| workspace.tabs.len())
}

fn active_document(
    cx: &HeadlessAppContext,
    workspace: &Entity<Workspace>,
) -> Result<Entity<Document>> {
    cx.read_entity(workspace, |workspace, _| {
        workspace.active_tab().map(|tab| tab.document.clone())
    })
    .context("native fixture has no active document")
}

fn active_content(cx: &HeadlessAppContext, workspace: &Entity<Workspace>) -> Result<String> {
    let document = active_document(cx, workspace)?;
    Ok(cx.read_entity(&document, |document, _| document.buffer.content()))
}

fn replace_active_content(
    cx: &mut HeadlessAppContext,
    workspace: &Entity<Workspace>,
    content: &str,
) -> Result<()> {
    let document = active_document(cx, workspace)?;
    let content = content.to_owned();
    document.update(cx, |document, cx| {
        let end = document.buffer.len_bytes();
        document.replace_range(0, end, &content);
        cx.notify();
    });
    Ok(())
}

fn active_path(cx: &HeadlessAppContext, workspace: &Entity<Workspace>) -> Result<Option<PathBuf>> {
    let document = active_document(cx, workspace)?;
    Ok(cx.read_entity(&document, |document, _| document.path.clone()))
}

fn keystroke(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    key: &str,
) -> Result<()> {
    let key = Keystroke::parse(key)?;
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_keystroke(key, cx);
    })?;
    draw(cx, window)
}

fn checked_close(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
) -> Result<bool> {
    window
        .update(cx, |view, window, cx| {
            view.test_close_application_window(window, cx)
        })
        .context("invoke native checked window close")
}

fn teardown(
    cx: &mut HeadlessAppContext,
    windows: &[WindowHandle<MarkRustWindow>],
    workspaces: Vec<Entity<Workspace>>,
) {
    // A checked close may already have removed a handle. Deliberately ignore
    // that expected case, but always remove surviving test windows before
    // dropping their entities so a primary assertion cannot become a leaked
    // entity panic.
    for window in windows {
        let _ = cx.update_window((*window).into(), |_, window, _| window.remove_window());
    }
    drop(workspaces);
    advance(cx, CHECKPOINT_WAIT);
}

fn check_last_active_window_routes_and_queues(
    cx: &mut HeadlessAppContext,
    root: &Path,
) -> Result<()> {
    let directory = fixture_directory(root, "last-active-routing")?;
    let (oldest, oldest_workspace) = create_window(
        cx,
        RecoveryStore::new(directory.join("oldest-session")),
        ThemeChoice::Dark,
    )?;
    let (latest, latest_workspace) = create_window(
        cx,
        RecoveryStore::new(directory.join("latest-session")),
        ThemeChoice::Dark,
    )?;
    let result = (|| {
        let oldest_path = directory.join("opened-in-oldest.md");
        let latest_path = directory.join("opened-in-latest.md");
        let queued_path = directory.join("queued-behind-review.md");
        let after_close_path = directory.join("opened-after-oldest-close.md");
        for path in [&oldest_path, &latest_path, &queued_path, &after_close_path] {
            std::fs::write(path, "# isolated external open\n")?;
        }

        activate_window(cx, oldest)?;
        route_external_open(cx, oldest_path.clone(), OpenOrigin::Finder)?;
        draw(cx, oldest)?;
        let first_route_state = (
            active_path(cx, &oldest_workspace)?,
            active_path(cx, &latest_workspace)?,
            active_workspace_tab_count(cx, &oldest_workspace),
            active_workspace_tab_count(cx, &latest_workspace),
        );
        ensure!(
            first_route_state.0.as_deref() == Some(oldest_path.as_path())
                && first_route_state.1.is_none(),
            "Finder route did not open only in the explicitly active oldest window: {first_route_state:?}"
        );
        ensure!(
            last_active_window(cx).is_some_and(|handle| handle.window_id() == oldest.window_id()),
            "activation registry did not retain the oldest window as last active"
        );

        activate_window(cx, latest)?;
        route_external_open(cx, latest_path.clone(), OpenOrigin::Finder)?;
        draw(cx, latest)?;
        ensure!(
            active_path(cx, &latest_workspace)?.as_deref() == Some(latest_path.as_path())
                && active_path(cx, &oldest_workspace)?.as_deref() == Some(oldest_path.as_path()),
            "Finder route did not open in the newly activated window"
        );

        // Open an actual external-change review, then ensure a Finder open is
        // queued behind it rather than mutating workspace state underneath it.
        replace_active_content(cx, &latest_workspace, "# local change\n")?;
        std::fs::write(&latest_path, "# disk change\n")?;
        keystroke(cx, latest, "cmd-s")?;
        let latest_root = latest.root(cx)?;
        ensure!(
            cx.read_entity(&latest_root, |view, _| {
                view.test_review_state().is_some() && !view.test_has_application_dialog()
            }),
            "conflicting Save did not present an external-change review without an unrelated native dialog"
        );
        let tabs_before_queued_open = active_workspace_tab_count(cx, &latest_workspace);
        route_external_open(cx, queued_path.clone(), OpenOrigin::Finder)?;
        ensure!(
            cx.read_entity(&latest_root, |view, _| view
                .test_pending_external_open_count())
                == 1,
            "Finder open was not queued while a review owned the window"
        );
        ensure!(
            active_workspace_tab_count(cx, &latest_workspace) == tabs_before_queued_open,
            "queued Finder path changed tabs behind a modal review"
        );
        keystroke(cx, latest, "escape")?;
        ensure!(
            cx.read_entity(&latest_root, |view, _| view.test_review_state().is_none()
                && view.test_pending_external_open_count() == 0),
            "cancelling review did not drain the queued Finder path"
        );
        ensure!(
            active_workspace_tab_count(cx, &latest_workspace) == tabs_before_queued_open + 1
                && active_path(cx, &latest_workspace)?.as_deref() == Some(queued_path.as_path()),
            "queued Finder path did not open exactly once after cancelling review"
        );

        ensure!(
            checked_close(cx, oldest)?,
            "a clean oldest window was not permitted to close"
        );
        advance(cx, Duration::from_millis(100));
        ensure!(
            last_active_window(cx).is_some_and(|handle| handle.window_id() == latest.window_id()),
            "closing the oldest window did not leave the live window routable"
        );
        let tabs_before_post_close_route = active_workspace_tab_count(cx, &latest_workspace);
        route_external_open(cx, after_close_path.clone(), OpenOrigin::Finder)?;
        draw(cx, latest)?;
        ensure!(
            active_workspace_tab_count(cx, &latest_workspace) == tabs_before_post_close_route + 1,
            "Finder route was dropped or sent to a closed oldest window"
        );
        ensure!(
            active_path(cx, &latest_workspace)?.as_deref() == Some(after_close_path.as_path()),
            "post-close Finder route did not activate the newly opened document"
        );
        Ok(())
    })();
    if let Err(error) = &result {
        eprintln!("notepad last-active fixture failed before cleanup: {error:#}");
    }
    teardown(
        cx,
        &[oldest, latest],
        vec![oldest_workspace, latest_workspace],
    );
    result
}

fn begin_link_widget_draft(
    cx: &mut HeadlessAppContext,
    workspace: &Entity<Workspace>,
    replacement: &str,
) -> Result<()> {
    workspace.update(cx, |workspace, cx| {
        let tab = workspace
            .active_tab()
            .context("missing active tab for link widget")?;
        let rich = tab.rich_view.clone();
        rich.update(cx, |view, cx| {
            view.apply_editor_command(EditorCommand::SetSelection { start: 2, end: 2 }, cx);
            view.apply_rich(RichCommand::ToggleLink, cx);
            view.apply_editor_command(EditorCommand::InsertText(replacement.into()), cx);
        });
        ensure!(
            rich.read(cx).has_pending_widget_edit(),
            "ToggleLink did not create the pending destination widget"
        );
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

/// Exercise the same native IME entry point macOS uses for either a focused
/// rich widget or a display-only body composition. The visible preedit
/// intentionally remains outside `Document` until the platform commits it.
fn set_rich_ime_preedit(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    preedit: &str,
) -> Result<()> {
    let rich = cx
        .read_entity(workspace, |workspace, _| {
            workspace.active_tab().map(|tab| tab.rich_view.clone())
        })
        .context("missing rich editor for native IME preedit")?;
    cx.update_window(window.into(), |_, window, cx| {
        rich.update(cx, |view, cx| {
            EntityInputHandler::replace_and_mark_text_in_range(
                view, None, preedit, None, window, cx,
            );
        });
    })?;
    draw(cx, window)
}

fn check_private_widget_draft_checkpoint_and_restart(
    cx: &mut HeadlessAppContext,
    root: &Path,
    geometry_only: bool,
) -> Result<()> {
    let directory = fixture_directory(root, "private-widget-draft")?;
    let path = directory.join("widget.md");
    let source = "[name](old)\n";
    let draft = "https://draft.example/👩‍🚀";
    let ime_preedit = " IME候補";
    let materialized_draft = format!("{draft}{ime_preedit}");
    std::fs::write(&path, source)?;
    let store = RecoveryStore::new(directory.join("session"));
    let (window, workspace) = create_window(cx, store.clone(), ThemeChoice::Light)?;
    let result = (|| {
        route_external_open(cx, path.clone(), OpenOrigin::Finder)?;
        draw(cx, window)?;
        begin_link_widget_draft(cx, &workspace, draft)?;
        set_rich_ime_preedit(cx, window, &workspace, ime_preedit)?;
        ensure!(
            cx.read_entity(&workspace, |workspace, cx| {
                workspace
                    .active_tab()
                    .is_some_and(|tab| tab.rich_view.read(cx).has_pending_composition())
            }),
            "widget IME preedit was not reported as pending composition"
        );
        draw(cx, window)?;
        ensure!(
            active_content(cx, &workspace)? == source && std::fs::read_to_string(&path)? == source,
            "an uncommitted link widget changed document or disk bytes"
        );

        // The rich-view observer, not a document mutation, must schedule this
        // private checkpoint. Waiting past the typing debounce proves it.
        advance(cx, CHECKPOINT_WAIT);
        let snapshot = store
            .load()
            .snapshot
            .context("pending widget checkpoint was not written")?;
        let tab = snapshot
            .tabs
            .iter()
            .find(|tab| tab.path.as_deref() == Some(path.as_path()))
            .context("widget checkpoint omitted the named tab")?;
        let saved_draft = tab
            .widget_draft
            .as_ref()
            .context("widget checkpoint omitted the raw pending field")?;
        ensure!(
            tab.content == source
                && !tab.dirty
                && saved_draft.draft == materialized_draft
                && saved_draft.selection.end == materialized_draft.len(),
            "widget checkpoint wrote body bytes or lost the Unicode field draft"
        );
        ensure!(
            std::fs::read_to_string(&path)? == source,
            "private widget checkpoint wrote an uncommitted field to disk"
        );

        write_private_draft_evidence(
            cx,
            window,
            &directory,
            geometry_only,
            active_workspace_tab_count(cx, &workspace),
            true,
        )?;

        ensure!(
            checked_close(cx, window)?,
            "a recoverable pending widget draft prevented checked window close"
        );
        advance(cx, Duration::from_millis(100));
        let (restored_window, restored_workspace) = create_window(
            cx,
            RecoveryStore::new(directory.join("session")),
            ThemeChoice::Light,
        )?;
        let restored_result = (|| {
            ensure!(
                active_path(cx, &restored_workspace)?.as_deref() == Some(path.as_path())
                    && active_content(cx, &restored_workspace)? == source,
                "restart changed the document body while restoring a raw widget draft"
            );
            let restored_draft = cx.read_entity(&restored_workspace, |workspace, cx| {
                workspace
                    .active_tab()
                    .and_then(|tab| tab.rich_view.read(cx).test_widget_draft())
            });
            ensure!(
                restored_draft.as_deref() == Some(materialized_draft.as_str()),
                "restart did not preserve the exact materialized IME link-field draft"
            );
            ensure!(
                std::fs::read_to_string(&path)? == source,
                "restoring a pending widget draft wrote to disk"
            );
            Ok(())
        })();
        if let Err(error) = &restored_result {
            eprintln!("notepad widget restart fixture failed before cleanup: {error:#}");
        }
        teardown(cx, &[restored_window], vec![restored_workspace]);
        restored_result
    })();
    if let Err(error) = &result {
        eprintln!("notepad widget checkpoint fixture failed before cleanup: {error:#}");
    }
    teardown(cx, &[window], vec![workspace]);
    result
}

fn write_private_draft_evidence(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    directory: &Path,
    geometry_only: bool,
    window_tabs: usize,
    pending_widget: bool,
) -> Result<()> {
    // This is intentionally content- and path-free: the artifact records only
    // the native state that makes the screenshot interpretable.
    let state = json!({
        "scenario": "two-window-private-draft",
        "window_tabs": window_tabs,
        "pending_widget": pending_widget,
        "private_checkpoint": true,
        "disk_write": false,
    });
    std::fs::write(
        directory.join("two-window-private-draft.json"),
        serde_json::to_vec_pretty(&state)?,
    )?;
    if !geometry_only {
        let screenshot = cx
            .capture_screenshot(window.into())
            .context("capture private widget diagnostic screenshot")?;
        crate::visual_tests::save_screenshot(
            &screenshot,
            &directory.join("two-window-private-draft.png"),
        )?;
    }
    Ok(())
}

fn check_stale_widget_draft_becomes_pathless_scratch(
    cx: &mut HeadlessAppContext,
    root: &Path,
) -> Result<()> {
    let directory = fixture_directory(root, "stale-widget-scratch")?;
    let original_source = "[name](url)\n";
    let body = "different body\n";
    let raw_draft = "INVALID visible raw 👩‍🚀";
    let store = RecoveryStore::new(directory.join("session"));
    store.write(&RecoverySnapshot {
        version: RECOVERY_VERSION,
        root: None,
        active_tab: 0,
        archived_tabs: Vec::new(),
        tabs: vec![RecoveryTab {
            path: None,
            title: "stale-widget.md".into(),
            mode: RecoveryEditorMode::Split,
            editing_pane: RecoveryEditingPane::Source,
            source_selection: RecoverySelection::collapsed(0),
            rich_selection: RecoverySelection::collapsed(0),
            content: body.into(),
            saved_content: original_source.into(),
            dirty: true,
            autosave_blocked: true,
            widget_draft: Some(RecoveryWidgetDraft {
                kind: RecoveryWidgetKind::LinkDestination,
                original_source: original_source.into(),
                source_range: 7..10,
                draft: raw_draft.into(),
                selection: 0..raw_draft.len(),
                selection_reversed: false,
            }),
        }],
    })?;
    let (window, workspace) = create_window(cx, store, ThemeChoice::Dark)?;
    let result = (|| {
        let state = cx.read_entity(&workspace, |workspace, cx| {
            let body_preserved = workspace.tabs.iter().any(|tab| {
                tab.document.read(cx).buffer.content() == body
                    && tab.document.read(cx).path.is_none()
            });
            let scratch = workspace.tabs.iter().find(|tab| {
                tab.document.read(cx).buffer.content() == raw_draft
                    && tab.document.read(cx).path.is_none()
                    && tab.mode == EditorMode::Source
            });
            (body_preserved, scratch.is_some(), workspace.tabs.len())
        });
        ensure!(
            state == (true, true, 2),
            "stale widget anchor did not preserve both body and pathless raw scratch: {state:?}"
        );
        let primary = window.update(cx, |_, native_window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.focus_active_editor(native_window, cx)
            });
            workspace.read(cx).tabs.iter().find_map(|tab| {
                (tab.document.read(cx).buffer.content() == body).then(|| {
                    (
                        tab.mode,
                        tab.editing_pane,
                        tab.editor
                            .read(cx)
                            .focus_handle(cx)
                            .is_focused(native_window),
                    )
                })
            })
        })?;
        ensure!(
            primary == Some((EditorMode::Split, EditingPane::Source, true)),
            "later scratch recovery changed the primary Split/Source input owner: {primary:?}"
        );
        Ok(())
    })();
    if let Err(error) = &result {
        eprintln!("notepad stale-widget fixture failed before cleanup: {error:#}");
    }
    teardown(cx, &[window], vec![workspace]);
    result
}

/// A body IME composition is painted by the rich editor but is not yet part
/// of the Markdown model. Closing it must preserve the visible bytes without
/// manufacturing a post-restart insertion point in the original document.
fn check_body_ime_preedit_becomes_pathless_scratch(
    cx: &mut HeadlessAppContext,
    root: &Path,
) -> Result<()> {
    let directory = fixture_directory(root, "body-ime-scratch")?;
    let path = directory.join("body-ime.md");
    let source = "# unchanged body\n";
    let preedit = "候補 👩‍🚀";
    std::fs::write(&path, source)?;
    let store = RecoveryStore::new(directory.join("session"));
    let (window, workspace) = create_window(cx, store.clone(), ThemeChoice::Light)?;
    let result = (|| {
        route_external_open(cx, path.clone(), OpenOrigin::Finder)?;
        draw(cx, window)?;
        set_rich_ime_preedit(cx, window, &workspace, preedit)?;
        let pending = cx.read_entity(&workspace, |workspace, cx| {
            workspace.active_tab().is_some_and(|tab| {
                let rich = tab.rich_view.read(cx);
                rich.has_pending_composition()
                    && rich.recovery_widget_draft().is_some_and(|draft| {
                        draft.kind == markrust_editor::wysiwyg::WidgetDraftKind::BodyComposition
                    })
            })
        });
        ensure!(
            pending,
            "body IME preedit was not exposed as a recoverable pending composition"
        );
        ensure!(
            active_content(cx, &workspace)? == source && std::fs::read_to_string(&path)? == source,
            "display-only body preedit changed the Markdown buffer or disk"
        );

        // This observer-driven checkpoint must retain body preedit even though
        // no Document notification occurs while the platform composes text.
        advance(cx, CHECKPOINT_WAIT);
        let snapshot = store
            .load()
            .snapshot
            .context("body IME checkpoint was not written")?;
        let tab = snapshot
            .tabs
            .iter()
            .find(|tab| tab.path.as_deref() == Some(path.as_path()))
            .context("body IME checkpoint omitted the named document")?;
        let saved_draft = tab
            .widget_draft
            .as_ref()
            .context("body IME checkpoint omitted visible preedit")?;
        ensure!(
            tab.content == source
                && !tab.dirty
                && saved_draft.kind == RecoveryWidgetKind::BodyComposition
                && saved_draft.draft == preedit,
            "body IME checkpoint did not retain a separate uncommitted composition"
        );
        ensure!(
            checked_close(cx, window)?,
            "recoverable body IME preedit prevented a checked window close"
        );
        advance(cx, Duration::from_millis(100));

        let (restored_window, restored_workspace) = create_window(
            cx,
            RecoveryStore::new(directory.join("session")),
            ThemeChoice::Light,
        )?;
        let restored_result = (|| {
            let state = cx.read_entity(&restored_workspace, |workspace, cx| {
                let source_owner = workspace.tabs.iter().any(|tab| {
                    tab.document.read(cx).path.as_deref() == Some(path.as_path())
                        && tab.document.read(cx).buffer.content() == source
                });
                let scratch = workspace.tabs.iter().any(|tab| {
                    tab.document.read(cx).path.is_none()
                        && tab.document.read(cx).buffer.content() == preedit
                        && tab.mode == EditorMode::Source
                        && tab.editing_pane == EditingPane::Source
                });
                (source_owner, scratch)
            });
            ensure!(
                state == (true, true),
                "body IME restart did not preserve original Markdown and a separate Source scratch: {state:?}"
            );
            ensure!(
                std::fs::read_to_string(&path)? == source,
                "body IME restart rewrote the original Markdown file"
            );
            Ok(())
        })();
        if let Err(error) = &restored_result {
            eprintln!("notepad body IME restart fixture failed before cleanup: {error:#}");
        }
        teardown(cx, &[restored_window], vec![restored_workspace]);
        restored_result
    })();
    if let Err(error) = &result {
        eprintln!("notepad body IME fixture failed before cleanup: {error:#}");
    }
    teardown(cx, &[window], vec![workspace]);
    result
}

fn check_dirty_closed_tab_reopens_pathless(cx: &mut HeadlessAppContext, root: &Path) -> Result<()> {
    let directory = fixture_directory(root, "closed-draft-archive")?;
    let path = directory.join("closed.md");
    let base = "# on disk\n";
    let draft = "# unsaved closed draft\n";
    std::fs::write(&path, base)?;
    let session = directory.join("session");
    let (window, workspace) =
        create_window(cx, RecoveryStore::new(session.clone()), ThemeChoice::Dark)?;
    let result = (|| {
        route_external_open(cx, path.clone(), OpenOrigin::Finder)?;
        draw(cx, window)?;
        replace_active_content(cx, &workspace, draft)?;
        // Opening the first file intentionally reuses the clean welcome tab.
        // Keep a separate blank tab alive so this exercises durable archive
        // semantics rather than the deliberate "never remove the last tab"
        // preservation branch.
        window.update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.new_document(window, cx));
        })?;
        let close_index = cx
            .read_entity(&workspace, |workspace, cx| {
                workspace
                    .tabs
                    .iter()
                    .position(|tab| tab.document.read(cx).path.as_deref() == Some(path.as_path()))
            })
            .context("closed draft fixture no longer owns its named tab")?;
        window.update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.close_tab(close_index, window, cx)
            });
        })?;
        ensure!(
            active_workspace_tab_count(cx, &workspace) == 1,
            "dirty named tab remained open after a successful durable close"
        );
        ensure!(
            std::fs::read_to_string(&path)? == base,
            "closing a dirty tab wrote its unsaved buffer to disk"
        );
        ensure!(
            checked_close(cx, window)?,
            "archive-bearing window did not close after its checkpoint"
        );
        advance(cx, Duration::from_millis(100));

        let (restored_window, restored_workspace) =
            create_window(cx, RecoveryStore::new(session), ThemeChoice::Dark)?;
        let restored_result = (|| {
            let restored = cx.read_entity(&restored_workspace, |workspace, cx| {
                workspace.tabs.iter().any(|tab| {
                    tab.document.read(cx).buffer.content() == draft
                        && tab.document.read(cx).path.is_none()
                })
            });
            ensure!(
                restored,
                "closed dirty named tab was not restored as a pathless draft"
            );
            advance(cx, CHECKPOINT_WAIT);
            ensure!(
                std::fs::read_to_string(&path)? == base,
                "restored closed draft acquired a second writable disk owner"
            );
            Ok(())
        })();
        if let Err(error) = &restored_result {
            eprintln!("notepad closed-draft restart fixture failed before cleanup: {error:#}");
        }
        teardown(cx, &[restored_window], vec![restored_workspace]);
        restored_result
    })();
    if let Err(error) = &result {
        eprintln!("notepad closed-draft archive fixture failed before cleanup: {error:#}");
    }
    teardown(cx, &[window], vec![workspace]);
    result
}

fn check_failed_window_close_keeps_live_owner(
    cx: &mut HeadlessAppContext,
    root: &Path,
) -> Result<()> {
    let directory = fixture_directory(root, "failed-close")?;
    let path = directory.join("failed-close.md");
    let disk = "# disk stays safe\n";
    let local = "# live unsaved owner\n";
    std::fs::write(&path, disk)?;
    // RecoveryStore rejects a regular file where it needs a private directory.
    // This is a deterministic storage failure with no permission mutation.
    let unusable_store = directory.join("not-a-session-directory");
    std::fs::write(&unusable_store, "regular file")?;
    let (clean_window, clean_workspace) = create_window(
        cx,
        RecoveryStore::new(unusable_store.clone()),
        ThemeChoice::Light,
    )?;
    let clean_result = checked_close(cx, clean_window);
    teardown(cx, &[clean_window], vec![clean_workspace]);
    ensure!(
        clean_result?,
        "a clean window could not close when private recovery storage was unavailable"
    );

    let (window, workspace) =
        create_window(cx, RecoveryStore::new(unusable_store), ThemeChoice::Light)?;
    let result = (|| {
        route_external_open(cx, path.clone(), OpenOrigin::Finder)?;
        draw(cx, window)?;
        replace_active_content(cx, &workspace, local)?;
        let tabs_before_failed_close = active_workspace_tab_count(cx, &workspace);
        ensure!(
            !checked_close(cx, window)?,
            "window close succeeded even though its dirty recovery checkpoint could not be written"
        );
        ensure!(
            active_workspace_tab_count(cx, &workspace) == tabs_before_failed_close
                && active_content(cx, &workspace)? == local
                && std::fs::read_to_string(&path)? == disk,
            "failed close lost the live owner or changed disk bytes"
        );
        let warning = cx.read_entity(&workspace, |workspace, _| {
            workspace
                .recovery_warning()
                .map(|warning| warning.message().to_owned())
        });
        ensure!(
            warning.is_some(),
            "failed close did not expose a recovery warning while retaining the live tab"
        );
        Ok(())
    })();
    if let Err(error) = &result {
        eprintln!("notepad failed-close fixture failed before cleanup: {error:#}");
    }
    teardown(cx, &[window], vec![workspace]);
    result
}

fn check_changed_disk_restart_blocks_autosave(
    cx: &mut HeadlessAppContext,
    root: &Path,
) -> Result<()> {
    let directory = fixture_directory(root, "changed-disk-restart")?;
    let path = directory.join("changed.md");
    let base = "# base\n";
    let local = "# local before restart\n";
    let external = "# external after shutdown\n";
    std::fs::write(&path, base)?;
    let session = directory.join("session");
    let (window, workspace) =
        create_window(cx, RecoveryStore::new(session.clone()), ThemeChoice::Dark)?;
    let result = (|| {
        route_external_open(cx, path.clone(), OpenOrigin::Finder)?;
        draw(cx, window)?;
        replace_active_content(cx, &workspace, local)?;
        ensure!(
            checked_close(cx, window)?,
            "dirty named window did not write its private checkpoint before close"
        );
        advance(cx, Duration::from_millis(100));
        std::fs::write(&path, external)?;

        let (restored_window, restored_workspace) =
            create_window(cx, RecoveryStore::new(session), ThemeChoice::Dark)?;
        let restored_result = (|| {
            ensure!(
                active_content(cx, &restored_workspace)? == local,
                "restart discarded the dirty local buffer after a disk change"
            );
            // Real editor input goes through the observed document path. A
            // scheduled autosave must still leave the newer disk file alone.
            keystroke(cx, restored_window, "x")?;
            advance(cx, CHECKPOINT_WAIT);
            ensure!(
                std::fs::read_to_string(&path)? == external,
                "post-restart typing autosaved a recovered dirty buffer over newer disk"
            );
            Ok(())
        })();
        if let Err(error) = &restored_result {
            eprintln!("notepad changed-disk restart fixture failed before cleanup: {error:#}");
        }
        teardown(cx, &[restored_window], vec![restored_workspace]);
        restored_result
    })();
    if let Err(error) = &result {
        eprintln!("notepad changed-disk fixture failed before cleanup: {error:#}");
    }
    teardown(cx, &[window], vec![workspace]);
    result
}
