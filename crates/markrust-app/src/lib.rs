// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! GPUI application shell for MarkRust.

mod app;
pub mod build_info;
mod config;
pub mod crash;
mod drop;
pub mod i18n;
mod icons;
mod menus;
mod panels;
mod recovery;
mod session;
mod ui;
pub mod update_install;
mod update_ui;
pub mod updater;
mod window;
mod workspace;

#[cfg(feature = "gui-tests")]
mod concurrent_visual_tests;
#[cfg(feature = "gui-tests")]
mod evidence;
#[cfg(feature = "gui-tests")]
mod notepad_visual_tests;
#[cfg(feature = "gui-tests")]
pub mod observation;
#[cfg(feature = "gui-tests")]
mod update_visual_tests;
#[cfg(feature = "gui-tests")]
pub mod usecases;
#[cfg(feature = "gui-tests")]
mod visual_contract;
#[cfg(feature = "gui-tests")]
pub mod visual_tests;

pub use app::{run_gui, run_gui_with_open, GPUI_GIT_REV};
pub use config::{AppConfig, RecentWorkspaces, ThemeChoice};
pub use drop::{
    classify_editor_drop, classify_window_drop, is_image, markdown_image_reference, DropIntent,
};
pub use recovery::{RecoveryWarning, RestoreKind};
pub use session::{
    classify_external_change, list_markdown_files, normalize_review_decision, reload_decision,
    should_offer_normalize_review, AutosaveScheduler, DropTarget, ExternalChangeAction,
    HeadlessWorkspace, NormalizeReviewChoice, SessionError, WorkspaceCommand,
};
