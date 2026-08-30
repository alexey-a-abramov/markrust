// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! GPUI application shell for MarkRust.

mod app;
mod config;
pub mod crash;
mod drop;
mod session;
mod ui;
mod window;
mod workspace;

pub use app::{run_gui, run_gui_with_open, GPUI_GIT_REV};
pub use config::{AppConfig, RecentWorkspaces, ThemeChoice};
pub use drop::{
    classify_editor_drop, classify_window_drop, is_image, markdown_image_reference, DropIntent,
};
pub use session::{
    classify_external_change, list_markdown_files, normalize_review_decision, reload_decision,
    should_offer_normalize_review, AutosaveScheduler, DropTarget, ExternalChangeAction,
    HeadlessWorkspace, NormalizeReviewChoice, SessionError, WorkspaceCommand,
};
