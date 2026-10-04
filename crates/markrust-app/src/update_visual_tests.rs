// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native update-notice and all-window restart barriers. Fixtures use private
//! recovery stores below runner output: no network, installer, user preference,
//! user document, or production recovery directory is touched.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{ensure, Context as _, Result};
use gpui::{
    point, px, size, AppContext, Bounds, Entity, EntityInputHandler, Focusable, HeadlessAppContext,
    Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, PlatformInput, WindowHandle,
};
use markrust_core::Document;
use markrust_editor::EditorCommand;
use serde_json::{json, Value};

use crate::app;
use crate::config::{AppConfig, ThemeChoice};
use crate::i18n::Language;
use crate::recovery::RecoveryStore;
use crate::window::MarkRustWindow;
use crate::workspace::{EditorMode, Workspace};

static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Native state count, not a golden-pixel baseline. Notices are injected into
/// the production render owner, while dismissal uses its actual painted hitbox.
pub(crate) fn run_update_ui_checks(
    cx: &mut HeadlessAppContext,
    output: &Path,
    geometry_only: bool,
) -> Result<usize> {
    let root = fixture_directory(output, "update-ui")?;
    check_restart_barriers(cx, &root)?;
    check_private_checkpoint_barrier(cx, &root)?;
    for theme in [ThemeChoice::Light, ThemeChoice::Dark] {
        check_notice_geometry_and_dismissal(cx, &root, theme, geometry_only)?;
    }
    println!(
        "PASS update-ui (12 native states: all-window composition/image guards, fail-closed private checkpoint, non-reflowing translated notices and native Later dismissal)"
    );
    Ok(12)
}

fn fixture_directory(output: &Path, label: &str) -> Result<PathBuf> {
    let directory = output.join(format!(
        "{label}-{}-{}",
        std::process::id(),
        FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory)
        .with_context(|| format!("create isolated update fixture {}", directory.display()))?;
    std::fs::canonicalize(&directory).context("canonicalize isolated update fixture")
}

fn configuration(theme: ThemeChoice) -> AppConfig {
    AppConfig {
        theme,
        language: Language::Russian,
        ..AppConfig::default()
    }
}

fn initialize_registry(cx: &mut HeadlessAppContext, root: &Path) -> Result<()> {
    let sessions = root.join("registry-sessions");
    std::fs::create_dir(&sessions)?;
    cx.update(|cx| {
        app::test_initialize_window_registry(configuration(ThemeChoice::Dark), sessions, cx)
    });
    Ok(())
}

fn create_window(
    cx: &mut HeadlessAppContext,
    store: RecoveryStore,
    theme: ThemeChoice,
    width: f32,
) -> Result<(WindowHandle<MarkRustWindow>, Entity<Workspace>)> {
    let handle = cx.update(|cx| {
        app::test_create_application_window(
            configuration(theme),
            store,
            size(px(width), px(720.)),
            cx,
        )
    })?;
    let root = handle.root(cx)?;
    let workspace = cx.read_entity(&root, |root, _| root.workspace.clone());
    draw(cx, handle)?;
    Ok((handle, workspace))
}

fn draw(cx: &mut HeadlessAppContext, handle: WindowHandle<MarkRustWindow>) -> Result<()> {
    for _ in 0..3 {
        cx.advance_clock(Duration::from_millis(35));
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.simulate_next_frame(cx);
            window.refresh();
            window.draw(cx).clear(cx);
        })?;
    }
    Ok(())
}

fn teardown(
    cx: &mut HeadlessAppContext,
    windows: &[WindowHandle<MarkRustWindow>],
    workspaces: Vec<Entity<Workspace>>,
) {
    for handle in windows {
        let _ = cx.update_window((*handle).into(), |_, window, _| window.remove_window());
    }
    drop(workspaces);
    cx.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
}

fn active_document(
    cx: &HeadlessAppContext,
    workspace: &Entity<Workspace>,
) -> Result<Entity<Document>> {
    cx.read_entity(workspace, |workspace, _| {
        workspace.active_tab().map(|tab| tab.document.clone())
    })
    .context("update fixture has no active document")
}

fn content(cx: &HeadlessAppContext, workspace: &Entity<Workspace>) -> Result<String> {
    let document = active_document(cx, workspace)?;
    Ok(cx.read_entity(&document, |document, _| document.buffer.content()))
}

fn replace_content(
    cx: &mut HeadlessAppContext,
    workspace: &Entity<Workspace>,
    text: &str,
) -> Result<()> {
    active_document(cx, workspace)?.update(cx, |document, cx| {
        let end = document.buffer.len_bytes();
        document.replace_range(0, end, text);
        ensure!(
            document.wait_for_parse(Duration::from_secs(5)),
            "update fixture parser timed out"
        );
        cx.notify();
        Ok::<_, anyhow::Error>(())
    })
}

fn click_at(
    cx: &mut HeadlessAppContext,
    handle: WindowHandle<MarkRustWindow>,
    position: gpui::Point<Pixels>,
) -> Result<()> {
    cx.update_window(handle.into(), |_, window, cx| {
        window.simulate_mouse_move(position, cx);
        window.dispatch_event(
            PlatformInput::MouseDown(MouseDownEvent {
                position,
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
        window.dispatch_event(
            PlatformInput::MouseUp(MouseUpEvent {
                position,
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count: 1,
            }),
            cx,
        );
    })?;
    draw(cx, handle)
}

fn check_restart_barriers(cx: &mut HeadlessAppContext, root: &Path) -> Result<()> {
    let directory = fixture_directory(root, "restart-guards")?;
    initialize_registry(cx, &directory)?;
    let (first, first_workspace) = create_window(
        cx,
        RecoveryStore::new(directory.join("first-private-session")),
        ThemeChoice::Dark,
        960.,
    )?;
    let (second, second_workspace) = create_window(
        cx,
        RecoveryStore::new(directory.join("second-private-session")),
        ThemeChoice::Light,
        960.,
    )?;
    let result = (|| {
        std::fs::write(
            directory.join("image.png"),
            include_bytes!("../tests/fixtures/assets/icon/icon.png"),
        )?;
        let path = directory.join("synthetic-document.md");
        let original = "# Restart guards\n\n![Guard image](image.png)\n\nKeep every live draft.\n";
        std::fs::write(&path, original)?;
        active_document(cx, &second_workspace)?.update(cx, |document, cx| {
            *document = Document::from_file(path.clone())?;
            ensure!(
                document.wait_for_parse(Duration::from_secs(5)),
                "image restart fixture parser timed out"
            );
            cx.notify();
            Ok::<_, anyhow::Error>(())
        })?;
        replace_content(cx, &first_workspace, "# Another dirty window\n")?;
        draw(cx, first)?;
        draw(cx, second)?;
        ensure!(
            cx.update(app::can_restart_for_update),
            "ordinary two-window editing unexpectedly blocks a restart"
        );

        cx.update_window(second.into(), |_, window, cx| {
            second_workspace.update(cx, |workspace, cx| {
                workspace.set_editor_mode(EditorMode::Source, window, cx);
                let source = workspace.active_tab().unwrap().editor.clone();
                source.update(cx, |source, cx| {
                    source.jump_to(original.len(), cx);
                    EntityInputHandler::replace_and_mark_text_in_range(
                        source, None, "候補", None, window, cx,
                    );
                });
            });
        })?;
        draw(cx, second)?;
        let source_preedit_bytes = content(cx, &second_workspace)?;
        ensure!(
            !cx.update(app::can_restart_for_update)
                && content(cx, &second_workspace)? == source_preedit_bytes,
            "the second registered window's Source composition did not block restart, or changed bytes"
        );
        cx.update_window(second.into(), |_, window, cx| {
            second_workspace.update(cx, |workspace, cx| {
                workspace
                    .active_tab()
                    .unwrap()
                    .editor
                    .update(cx, |source, cx| {
                        EntityInputHandler::unmark_text(source, window, cx);
                    });
                workspace.set_editor_mode(EditorMode::Wysiwyg, window, cx);
                workspace
                    .active_tab()
                    .unwrap()
                    .rich_view
                    .update(cx, |rich, cx| {
                        EntityInputHandler::replace_and_mark_text_in_range(
                            rich,
                            None,
                            "未確定",
                            None,
                            window,
                            cx,
                        );
                    });
            });
        })?;
        draw(cx, second)?;
        let rich_preedit_bytes = content(cx, &second_workspace)?;
        ensure!(
            !cx.update(app::can_restart_for_update)
                && content(cx, &second_workspace)? == rich_preedit_bytes,
            "the second registered window's Rich composition did not block restart, or changed bytes"
        );
        cx.update_window(second.into(), |_, window, cx| {
            second_workspace.update(cx, |workspace, cx| {
                workspace
                    .active_tab()
                    .unwrap()
                    .rich_view
                    .update(cx, |rich, cx| {
                        EntityInputHandler::unmark_text(rich, window, cx);
                    });
            });
        })?;
        draw(cx, second)?;
        let rich = cx.read_entity(&second_workspace, |workspace, _| {
            workspace.active_tab().unwrap().rich_view.clone()
        });
        let image_point = cx
            .read_entity(&rich, |rich, _| rich.test_first_image_hit_point())
            .context("restart fixture did not paint its image hit target")?;
        click_at(cx, second, image_point)?;
        ensure!(
            cx.read_entity(&rich, |rich, _| rich.has_image_editor())
                && !cx.update(app::can_restart_for_update)
                && content(cx, &second_workspace)? == rich_preedit_bytes,
            "the separate image location/alt editor did not block restart or changed Markdown"
        );
        let (_, cancel) = cx
            .read_entity(&rich, |rich, cx| rich.test_image_action_bounds(cx))
            .context("image inspector has no painted Cancel action")?;
        click_at(cx, second, cancel.center())?;
        ensure!(
            cx.update(app::can_restart_for_update)
                && !cx.read_entity(&rich, |rich, _| rich.has_image_editor())
                && std::fs::read_to_string(&path)? == original,
            "cancelled inspector/composition left a stale restart barrier"
        );
        std::fs::write(
            directory.join("guards.json"),
            serde_json::to_vec_pretty(&json!({
                "registered_windows": 2,
                "ordinary_editing_allows_restart": true,
                "source_ime_blocks_restart": true,
                "rich_ime_blocks_restart": true,
                "separate_image_inspector_blocks_restart": true,
                "live_buffers_preserved": true,
                "helper_started": false,
                "network_used": false,
            }))?,
        )?;
        Ok(())
    })();
    teardown(
        cx,
        &[first, second],
        vec![first_workspace, second_workspace],
    );
    result
}

fn check_private_checkpoint_barrier(cx: &mut HeadlessAppContext, root: &Path) -> Result<()> {
    let directory = fixture_directory(root, "checkpoint-guard")?;
    initialize_registry(cx, &directory)?;
    let first_store = RecoveryStore::new(directory.join("first-private-session"));
    let unusable_store = directory.join("regular-file-not-directory");
    std::fs::write(&unusable_store, "deterministic private checkpoint failure")?;
    let (first, first_workspace) =
        create_window(cx, first_store.clone(), ThemeChoice::Light, 960.)?;
    let (second, second_workspace) = create_window(
        cx,
        RecoveryStore::new(unusable_store),
        ThemeChoice::Dark,
        960.,
    )?;
    let result = (|| {
        let first_text = "# First dirty owner\n\nNo installer may erase this draft.\n";
        let second_text = "# Second dirty owner\n\nFailed recovery must retain this draft.\n";
        let first_path = directory.join("first-on-disk.md");
        let second_path = directory.join("second-on-disk.md");
        let disk = "# Saved on disk\n";
        for (workspace, path) in [
            (&first_workspace, &first_path),
            (&second_workspace, &second_path),
        ] {
            std::fs::write(path, disk)?;
            active_document(cx, workspace)?.update(cx, |document, cx| {
                *document = Document::from_file(path.clone())?;
                cx.notify();
                Ok::<_, anyhow::Error>(())
            })?;
        }
        replace_content(cx, &first_workspace, first_text)?;
        replace_content(cx, &second_workspace, second_text)?;
        ensure!(
            cx.update(app::can_restart_for_update),
            "private checkpoint fixture unexpectedly has a pending editor dialog"
        );
        ensure!(
            !cx.update(app::checkpoint_application),
            "all-window private checkpoint succeeded despite the second dirty store being unusable"
        );
        ensure!(
            content(cx, &first_workspace)? == first_text
                && content(cx, &second_workspace)? == second_text
                && first.root(cx).is_ok()
                && second.root(cx).is_ok()
                && std::fs::read_to_string(&first_path)? == disk
                && std::fs::read_to_string(&second_path)? == disk,
            "failed all-window checkpoint released a live window or modified a dirty buffer"
        );
        ensure!(
            first_store.load().snapshot.is_some()
                && cx.read_entity(&second_workspace, |workspace, _| workspace.recovery_warning().is_some()),
            "failure omitted the unsafe-window warning or skipped the other window's private checkpoint"
        );
        std::fs::write(
            directory.join("checkpoint-failure.json"),
            serde_json::to_vec_pretty(&json!({
                "registered_windows": 2,
                "checkpoint_allowed_restart": false,
                "successful_window_checkpointed": true,
                "failed_window_retained": true,
                "all_live_buffers_preserved": true,
                "helper_started": false,
            }))?,
        )?;
        Ok(())
    })();
    teardown(
        cx,
        &[first, second],
        vec![first_workspace, second_workspace],
    );
    result?;

    // A separate success case ensures fail-closed is not a permanent veto.
    let success = fixture_directory(root, "checkpoint-success")?;
    initialize_registry(cx, &success)?;
    let first_store = RecoveryStore::new(success.join("first-private-session"));
    let second_store = RecoveryStore::new(success.join("second-private-session"));
    let (first, first_workspace) =
        create_window(cx, first_store.clone(), ThemeChoice::Light, 960.)?;
    let (second, second_workspace) =
        create_window(cx, second_store.clone(), ThemeChoice::Dark, 960.)?;
    let result = (|| {
        replace_content(cx, &first_workspace, "# Safely checkpointed first draft\n")?;
        replace_content(
            cx,
            &second_workspace,
            "# Safely checkpointed second draft\n",
        )?;
        ensure!(
            cx.update(app::can_restart_for_update)
                && cx.update(app::checkpoint_application)
                && first_store.load().snapshot.is_some()
                && second_store.load().snapshot.is_some(),
            "ordinary two-window dirty editing did not pass both private checkpoint barriers"
        );
        Ok(())
    })();
    teardown(
        cx,
        &[first, second],
        vec![first_workspace, second_workspace],
    );
    result
}

/// Independent body observations deliberately exclude the blink phase: a timed
/// notice may repaint, but must not acquire focus or alter editor geometry.
fn body_state(
    cx: &mut HeadlessAppContext,
    handle: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
) -> Result<Value> {
    let observed = crate::observation::capture(cx, handle, workspace)?;
    let scroll = cx.read_entity(workspace, |workspace, cx| {
        let tab = workspace.active_tab().unwrap();
        let source = tab.editor_view.read(cx).scroll_offset();
        let (rich, _, _) = tab.rich_view.read(cx).test_viewport_state();
        json!({
            "source_x": f32::from(source.x),
            "source_y": f32::from(source.y),
            "rich_item": rich.item_ix,
            "rich_item_offset": f32::from(rich.offset_in_item),
        })
    });
    Ok(json!({
        "revision": observed.document_revision,
        "active_tab_id": observed.active_tab_id,
        "tab_count": observed.tab_count,
        "input_owner": observed.input_owner,
        "source": {
            "selection": observed.source_pane.selection,
            "reversed": observed.source_pane.reversed,
            "focused": observed.source_pane.focused,
            "viewport": observed.source_pane.viewport,
            "caret": observed.source_pane.caret,
            "caret_bounds": observed.source_pane.caret_bounds,
        },
        "rich": {
            "selection": observed.rich_pane.selection,
            "reversed": observed.rich_pane.reversed,
            "focused": observed.rich_pane.focused,
            "viewport": observed.rich_pane.viewport,
            "caret": observed.rich_pane.caret,
            "caret_bounds": observed.rich_pane.caret_bounds,
        },
        "scroll": scroll,
        "painted_rich": observed.painted_rich,
        "source_rows": observed.painted_source,
    }))
}

fn check_notice_geometry_and_dismissal(
    cx: &mut HeadlessAppContext,
    root: &Path,
    theme: ThemeChoice,
    geometry_only: bool,
) -> Result<()> {
    let theme_name = if theme == ThemeChoice::Light {
        "light"
    } else {
        "dark"
    };
    let directory = fixture_directory(root, &format!("notice-{theme_name}"))?;
    initialize_registry(cx, &directory)?;
    let (handle, workspace) = create_window(
        cx,
        RecoveryStore::new(directory.join("private-session")),
        theme,
        720.,
    )?;
    let result = (|| {
        let mut text = "# Passive update notice\n\n".to_owned();
        for index in 0..50 {
            text.push_str(&format!(
                "Paragraph {index}: keep selection, caret and viewport stable.\n\n"
            ));
        }
        replace_content(cx, &workspace, &text)?;
        cx.update_window(handle.into(), |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.set_editor_mode(EditorMode::Split, window, cx);
                let tab = workspace.active_tab().unwrap();
                let caret = text.find("Paragraph 10").unwrap() + 12;
                tab.editor.update(cx, |source, cx| {
                    source.apply_command(
                        EditorCommand::SetSelection {
                            start: caret - 3,
                            end: caret,
                        },
                        cx,
                    );
                });
                tab.rich_view.update(cx, |rich, cx| {
                    rich.apply_editor_command(
                        EditorCommand::SetSelection {
                            start: caret - 3,
                            end: caret,
                        },
                        cx,
                    );
                });
                window.focus(&tab.rich_view.read(cx).focus_handle(cx), cx);
            });
        })?;
        draw(cx, handle)?;
        // Real independent wheel input leaves the caret off-screen in both
        // panes, detecting a notice that accidentally re-reveals an old caret.
        for rich_pane in [false, true] {
            let bounds = cx.read_entity(&workspace, |workspace, cx| {
                let tab = workspace.active_tab().unwrap();
                if rich_pane {
                    tab.rich_view.read(cx).test_viewport_state().1
                } else {
                    tab.editor_view.read(cx).horizontal_scroll_state().0
                }
            });
            cx.update_window(handle.into(), |_, window, cx| {
                window.simulate_mouse_move(bounds.center(), cx);
                window.dispatch_event(
                    PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                        position: bounds.center(),
                        delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-240.))),
                        modifiers: Modifiers::default(),
                        touch_phase: gpui::TouchPhase::Moved,
                    }),
                    cx,
                );
            })?;
            draw(cx, handle)?;
        }
        let before = body_state(cx, handle, &workspace)?;
        let bytes = content(cx, &workspace)?;
        for status in ["checking", "current", "error"] {
            cx.update(|cx| crate::update_ui::test_initialize_notice(cx, status));
            draw(cx, handle)?;
            ensure!(
                cx.update(|cx| crate::update_ui::test_notice_visible(cx)),
                "injected {status} notice did not become visible"
            );
            let (panel, later) = cx
                .update(|cx| crate::update_ui::test_notice_bounds(cx))
                .context("update notice did not paint panel and Later hit targets")?;
            let window_bounds = cx.update_window(handle.into(), |_, window, _| window.bounds())?;
            ensure_inside(panel, window_bounds, "compact update notice")?;
            ensure_inside(later, panel, "translated Later hit target")?;
            let visible = body_state(cx, handle, &workspace)?;
            ensure!(
                visible == before && content(cx, &workspace)? == bytes,
                "{theme_name} {status} notice moved body geometry, viewport, focus, selection, caret or bytes: before={before}, after={visible}"
            );
            let label = format!("update-{theme_name}-{status}");
            std::fs::write(
                directory.join(format!("{label}.json")),
                serde_json::to_vec_pretty(&json!({
                    "scenario": label,
                    "notice_visible": true,
                    "body_before": before,
                    "body_after": visible,
                    "panel": format!("{panel:?}"),
                    "later_hit_target": format!("{later:?}"),
                    "network_used": false,
                    "helper_started": false,
                }))?,
            )?;
            if !geometry_only {
                crate::visual_tests::save_screenshot(
                    &cx.capture_screenshot(handle.into())
                        .context("capture native update notice")?,
                    &directory.join(format!("{label}.png")),
                )?;
            }
            click_at(cx, handle, later.center())?;
            ensure!(
                !cx.update(|cx| crate::update_ui::test_notice_visible(cx))
                    && body_state(cx, handle, &workspace)? == before
                    && content(cx, &workspace)? == bytes,
                "real Later click did not dismiss only the {status} notice"
            );
        }
        Ok(())
    })();
    teardown(cx, &[handle], vec![workspace]);
    result
}

fn ensure_inside(inner: Bounds<Pixels>, outer: Bounds<Pixels>, label: &str) -> Result<()> {
    ensure!(
        inner.size.width > px(0.)
            && inner.size.height > px(0.)
            && inner.left() >= outer.left()
            && inner.right() <= outer.right()
            && inner.top() >= outer.top()
            && inner.bottom() <= outer.bottom(),
        "{label} clipped: {inner:?} outside {outer:?}"
    );
    Ok(())
}
