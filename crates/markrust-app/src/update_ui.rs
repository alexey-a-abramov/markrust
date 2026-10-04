// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One desktop update owner. Network and archive work run outside the UI thread;
//! only an explicit restart button can cross the all-window recovery barrier.

use futures::channel::oneshot;
use gpui::{div, prelude::*, px, AnyElement, App, Global};
use markrust_editor::EditorTheme;
use std::time::Duration;

use crate::{build_info, update_install, updater};

pub(crate) const REPOSITORY_URL: &str = "https://github.com/alexey-a-abramov/markrust";

enum Status {
    Idle,
    Checking,
    Current,
    Available(Box<updater::Release>),
    Downloading,
    Ready(Box<ReadyUpdate>),
    Error(String),
}

struct ReadyUpdate {
    _staged: updater::StagedUpdate,
    prepared: update_install::PreparedInstall,
}

struct Updates {
    automatic: bool,
    visible: bool,
    generation: u64,
    status: Status,
    handoff: Option<update_install::ApplyHandle>,
    #[cfg(feature = "gui-tests")]
    notice_bounds: gpui::ScrollHandle,
    #[cfg(feature = "gui-tests")]
    later_bounds: gpui::ScrollHandle,
}

impl Global for Updates {}

impl Updates {
    fn busy(&self) -> bool {
        matches!(self.status, Status::Checking | Status::Downloading)
    }
}

pub(crate) fn initialize(automatic: bool, cx: &mut App) {
    cx.set_global(Updates {
        automatic,
        visible: false,
        generation: 0,
        status: Status::Idle,
        handoff: None,
        #[cfg(feature = "gui-tests")]
        notice_bounds: gpui::ScrollHandle::new(),
        #[cfg(feature = "gui-tests")]
        later_bounds: gpui::ScrollHandle::new(),
    });
    cx.spawn(async move |cx| {
        let mut delay = Duration::from_secs(10);
        loop {
            let (sender, receiver) = oneshot::channel();
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                let _ = sender.send(());
            });
            if receiver.await.is_err() {
                break;
            }
            cx.update(|cx| {
                if cx.try_global::<Updates>().is_some_and(|state| {
                    state.automatic && !state.busy() && !matches!(state.status, Status::Ready(_))
                }) {
                    check(false, cx);
                }
            });
            delay = Duration::from_secs(24 * 60 * 60);
        }
    })
    .detach();
}

pub(crate) fn toggle_automatic(cx: &mut App) {
    let Some(state) = cx.try_global::<Updates>() else {
        return;
    };
    let enabled = !state.automatic;
    cx.update_global::<Updates, _>(|state, _| state.automatic = enabled);
    crate::app::set_automatic_updates(enabled, cx);
    cx.refresh_windows();
}

pub(crate) fn check(manual: bool, cx: &mut App) {
    let Some(state) = cx.try_global::<Updates>() else {
        return;
    };
    if state.handoff.is_some() {
        return;
    }
    if state.busy() || matches!(state.status, Status::Ready(_)) {
        if manual {
            cx.update_global::<Updates, _>(|state, _| state.visible = true);
            cx.refresh_windows();
        }
        return;
    }
    let generation = cx.update_global::<Updates, _>(|state, _| {
        state.generation = state.generation.wrapping_add(1);
        state.visible = manual;
        state.status = Status::Checking;
        state.generation
    });
    cx.refresh_windows();
    let (sender, receiver) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(updater::check_latest(build_info::VERSION));
    });
    cx.spawn(async move |cx| {
        if let Ok(result) = receiver.await {
            cx.update(|cx| {
                if cx
                    .try_global::<Updates>()
                    .is_none_or(|state| state.generation != generation)
                {
                    return;
                }
                cx.update_global::<Updates, _>(|state, _| match result {
                    _ if !manual && !state.automatic => {
                        state.status = Status::Idle;
                        state.visible = false;
                    }
                    Ok(Some(release)) => {
                        state.status = Status::Available(Box::new(release));
                        state.visible = true;
                    }
                    Ok(None) => {
                        state.status = Status::Current;
                    }
                    Err(error) => {
                        state.status = Status::Error(error.to_string());
                    }
                });
                cx.refresh_windows();
            });
        }
    })
    .detach();
}

fn download(cx: &mut App) {
    let Some(state) = cx.try_global::<Updates>() else {
        return;
    };
    let Status::Available(release) = &state.status else {
        return;
    };
    let release = release.clone();
    let generation = cx.update_global::<Updates, _>(|state, _| {
        state.generation = state.generation.wrapping_add(1);
        state.status = Status::Downloading;
        state.visible = true;
        state.generation
    });
    cx.refresh_windows();
    let (sender, receiver) = oneshot::channel();
    std::thread::spawn(move || {
        let result = (|| -> anyhow::Result<_> {
            let staged = updater::stage_update(&release)?;
            let prepared = update_install::prepare_install(staged.bundle_path(), staged.version())?;
            Ok((staged, prepared))
        })();
        let _ = sender.send(result);
    });
    cx.spawn(async move |cx| {
        if let Ok(result) = receiver.await {
            cx.update(|cx| {
                if cx
                    .try_global::<Updates>()
                    .is_none_or(|state| state.generation != generation)
                {
                    return;
                }
                cx.update_global::<Updates, _>(|state, _| {
                    state.status = match result {
                        Ok((staged, prepared)) => Status::Ready(Box::new(ReadyUpdate {
                            _staged: staged,
                            prepared,
                        })),
                        Err(error) => Status::Error(error.to_string()),
                    }
                });
                cx.refresh_windows();
            });
        }
    })
    .detach();
}

fn restart(cx: &mut App) {
    if !crate::app::can_restart_for_update(cx) {
        fail(
            "Finish the open dialog or text composition before updating.",
            cx,
        );
        return;
    }
    let Some(state) = cx.try_global::<Updates>() else {
        return;
    };
    let Status::Ready(ready) = &state.status else {
        return;
    };
    let result = update_install::spawn_apply(&ready.prepared);
    match result {
        Ok(mut handle) => {
            if !crate::app::checkpoint_application(cx) {
                // Only macOS can create the RAII installer handle. Other
                // platforms return an error before reaching this branch.
                #[cfg(target_os = "macos")]
                drop(handle);
                fail(
                    "Private draft checkpoint failed. The application has not been closed.",
                    cx,
                );
                return;
            }
            if let Err(error) = handle.commit() {
                fail(&error.to_string(), cx);
                return;
            }
            cx.update_global::<Updates, _>(|state, _| state.handoff = Some(handle));
            cx.refresh_windows();
            // GPUI's macOS Quit is asynchronous. Freeze input until exit; if
            // termination is vetoed, abort the helper before accepting edits.
            cx.spawn(async move |cx| {
                let (sender, receiver) = oneshot::channel();
                std::thread::spawn(move || { std::thread::sleep(Duration::from_secs(5)); let _ = sender.send(()); });
                if receiver.await.is_err() { return; }
                cx.update(|cx| {
                    let Some(state) = cx.try_global::<Updates>() else { return; };
                    if state.handoff.is_none() { return; }
                    let result = cx.update_global::<Updates, _>(|state, _| state.handoff.as_mut().unwrap().abort());
                    if result.is_ok() {
                        cx.update_global::<Updates, _>(|state, _| {
                            state.handoff = None;
                            state.status = Status::Error("The application did not quit. Update cancelled; your edits remain open.".into());
                            state.visible = true;
                        });
                        cx.refresh_windows();
                    }
                });
            }).detach();
            cx.quit();
        }
        Err(error) => fail(&error.to_string(), cx),
    }
}

fn fail(message: &str, cx: &mut App) {
    cx.update_global::<Updates, _>(|state, _| {
        state.visible = true;
        state.status = Status::Error(message.to_owned());
    });
    cx.refresh_windows();
}

pub(crate) fn render(theme: &EditorTheme, window_width: f32, cx: &mut App) -> AnyElement {
    if is_installing(cx) {
        return div()
            .id("update-restart-shield")
            .absolute()
            .inset_0()
            .bg(theme.background.opacity(0.9))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(theme.ui_text("Restart and Update"))
            .into_any_element();
    }
    let Some(state) = cx.try_global::<Updates>().filter(|state| state.visible) else {
        return div().into_any_element();
    };
    let (heading, details, action) = match &state.status {
        Status::Idle => return div().into_any_element(),
        Status::Checking => ("Checking for updates…", String::new(), 0),
        Status::Current => ("No newer stable release is available.", String::new(), 0),
        Status::Available(release) => ("New version available", release.version().to_owned(), 1),
        Status::Downloading => ("Downloading and verifying update…", String::new(), 0),
        Status::Ready(ready) => ("Update is ready", ready.prepared.version().to_owned(), 2),
        Status::Error(message) => ("Update failed", theme.ui_text(message), 3),
    };
    let button_theme = theme.clone();
    let notice = div().id("release-update-panel");
    #[cfg(feature = "gui-tests")]
    let notice = notice.track_scroll(&state.notice_bounds);
    let later = div().id("release-update-later");
    #[cfg(feature = "gui-tests")]
    let later = later.track_scroll(&state.later_bounds);
    notice
        .absolute()
        .right(px(16.))
        .bottom(px(38.))
        .w(px((window_width - 32.).clamp(160., 400.)))
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
        .bg(theme.chrome_bg)
        .border_1()
        .border_color(theme.separator)
        .rounded_lg()
        .shadow_lg()
        .text_color(theme.text)
        .text_sm()
        .occlude()
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(theme.ui_text(heading))
        .when(!details.is_empty(), |element| {
            element.child(
                div()
                    .id("release-update-details")
                    .max_h(px(160.))
                    .overflow_y_scroll()
                    .child(details),
            )
        })
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .items_center()
                .when(action > 0, |element| {
                    element.child(
                        div()
                            .id("release-update-primary")
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(button_theme.accent)
                            .text_color(gpui::rgb(0xffffff))
                            .child(theme.ui_text(match action {
                                1 => "Download Update",
                                2 => "Restart and Update",
                                _ => "Check for Updates…",
                            }))
                            .on_click(move |_, _, cx| match action {
                                1 => download(cx),
                                2 => restart(cx),
                                _ => check(true, cx),
                            }),
                    )
                })
                .child(
                    div()
                        .id("release-update-repository")
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .cursor_pointer()
                        .child(theme.ui_text("GitHub Repository"))
                        .on_click(|_, _, cx| cx.open_url(REPOSITORY_URL)),
                )
                .child(
                    later
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .cursor_pointer()
                        .child(theme.ui_text("Later"))
                        .on_click(|_, _, cx| {
                            cx.update_global::<Updates, _>(|state, _| state.visible = false);
                            cx.refresh_windows();
                        }),
                ),
        )
        .into_any_element()
}

pub(crate) fn is_installing(cx: &App) -> bool {
    cx.try_global::<Updates>()
        .is_some_and(|state| state.handoff.is_some())
}

/// Fixture-only state injection: no checker, download, installer or timer is
/// started. It still paints and dismisses through the production notice owner.
#[cfg(feature = "gui-tests")]
pub(crate) fn test_initialize_notice(cx: &mut App, notice: &'static str) {
    cx.set_global(Updates {
        automatic: false,
        visible: true,
        generation: 0,
        status: match notice {
            "checking" => Status::Checking,
            "current" => Status::Current,
            "error" => {
                Status::Error("Finish the open dialog or text composition before updating.".into())
            }
            _ => panic!("unknown update notice fixture {notice}"),
        },
        handoff: None,
        notice_bounds: gpui::ScrollHandle::new(),
        later_bounds: gpui::ScrollHandle::new(),
    });
    cx.refresh_windows();
}

#[cfg(feature = "gui-tests")]
pub(crate) fn test_notice_visible(cx: &App) -> bool {
    cx.try_global::<Updates>()
        .is_some_and(|state| state.visible)
}

/// Bounds come from the actual rendered Divs, never a second layout model.
#[cfg(feature = "gui-tests")]
pub(crate) fn test_notice_bounds(
    cx: &App,
) -> Option<(gpui::Bounds<gpui::Pixels>, gpui::Bounds<gpui::Pixels>)> {
    let state = cx.try_global::<Updates>().filter(|state| state.visible)?;
    Some((state.notice_bounds.bounds(), state.later_bounds.bounds()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_keeps_large_platform_payloads_indirect() {
        assert!(std::mem::size_of::<Status>() <= 2 * std::mem::size_of::<String>());
    }

    #[test]
    fn update_owner_never_starts_a_second_network_job_while_busy() {
        let mut state = Updates {
            automatic: true,
            visible: false,
            generation: 0,
            status: Status::Checking,
            handoff: None,
            #[cfg(feature = "gui-tests")]
            notice_bounds: gpui::ScrollHandle::new(),
            #[cfg(feature = "gui-tests")]
            later_bounds: gpui::ScrollHandle::new(),
        };
        assert!(state.busy());
        state.status = Status::Downloading;
        assert!(state.busy());
        state.status = Status::Current;
        assert!(!state.busy());
    }
}
