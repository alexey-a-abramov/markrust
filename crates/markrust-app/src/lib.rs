// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! GPUI application shell for MarkRust.

mod app;
mod config;
mod drop;
mod ui;
mod window;
mod workspace;

pub use app::{run_gui, GPUI_GIT_REV};
pub use config::{AppConfig, RecentWorkspaces, ThemeChoice};
