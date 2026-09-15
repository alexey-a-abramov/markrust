// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Responsive panel policy. Flags always describe a panel that is visible;
//! explicitly opened inspectors float when there is no room to dock them.

use crate::workspace::EditorMode;

pub const SIDEBAR_WIDTH: f32 = 240.;
pub const OUTLINE_WIDTH: f32 = 220.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    Sidebar,
    Outline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelLayout {
    pub sidebar: bool,
    pub outline: bool,
    pub overlay: Option<Panel>,
}

impl PanelLayout {
    pub fn fit(width: f32, mode: EditorMode, sidebar: bool, outline: bool) -> Self {
        let mut layout = Self {
            sidebar,
            outline,
            overlay: None,
        };
        let minimum = minimum_document_width(mode);
        if layout.document_width(width) < minimum {
            layout.outline = false;
        }
        if layout.document_width(width) < minimum {
            layout.sidebar = false;
        }
        layout
    }

    pub fn toggle(mut self, panel: Panel, width: f32, mode: EditorMode) -> Self {
        let visible = match panel {
            Panel::Sidebar => self.sidebar,
            Panel::Outline => self.outline,
        };
        if visible {
            match panel {
                Panel::Sidebar => self.sidebar = false,
                Panel::Outline => self.outline = false,
            }
            if self.overlay == Some(panel) {
                self.overlay = None;
            }
            return self;
        }
        self.overlay = None;
        match panel {
            Panel::Sidebar => self.sidebar = true,
            Panel::Outline => self.outline = true,
        }
        let minimum = minimum_document_width(mode);
        if self.document_width(width) < minimum {
            match panel {
                Panel::Sidebar => self.outline = false,
                Panel::Outline => self.sidebar = false,
            }
        }
        if self.document_width(width) < minimum {
            self.overlay = Some(panel);
        }
        self
    }

    pub fn document_width(self, width: f32) -> f32 {
        width
            - if self.sidebar && self.overlay != Some(Panel::Sidebar) {
                SIDEBAR_WIDTH
            } else {
                0.
            }
            - if self.outline && self.overlay != Some(Panel::Outline) {
                OUTLINE_WIDTH
            } else {
                0.
            }
    }
}

fn minimum_document_width(mode: EditorMode) -> f32 {
    if mode == EditorMode::Split {
        600.
    } else {
        400.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resizing_preserves_readable_document_and_collapses_outline_first() {
        let wide = PanelLayout::fit(1200., EditorMode::Wysiwyg, true, true);
        assert!(wide.sidebar && wide.outline);
        let narrow = PanelLayout::fit(720., EditorMode::Wysiwyg, true, true);
        assert!(narrow.sidebar && !narrow.outline);
        assert!(narrow.document_width(720.) >= 400.);
        let split = PanelLayout::fit(720., EditorMode::Split, true, true);
        assert!(!split.sidebar && !split.outline);
        assert!(split.document_width(720.) >= 600.);
    }

    #[test]
    fn dock_thresholds_reserve_the_entire_document_target() {
        let threshold = 400. + SIDEBAR_WIDTH + OUTLINE_WIDTH;
        assert!(PanelLayout::fit(threshold, EditorMode::Wysiwyg, true, true).outline);
        assert!(!PanelLayout::fit(threshold - 1., EditorMode::Wysiwyg, true, true).outline);
        let split_threshold = 600. + SIDEBAR_WIDTH;
        assert!(PanelLayout::fit(split_threshold, EditorMode::Split, true, false).sidebar);
        assert!(!PanelLayout::fit(split_threshold - 1., EditorMode::Split, true, false).sidebar);
    }

    #[test]
    fn explicit_toggle_shows_the_requested_panel_and_can_close_it() {
        let narrow = PanelLayout::fit(720., EditorMode::Wysiwyg, true, true);
        let outline = narrow.toggle(Panel::Outline, 720., EditorMode::Wysiwyg);
        assert!(outline.outline && !outline.sidebar);
        assert_eq!(outline.overlay, None);
        assert!(outline.document_width(720.) >= 400.);
        let closed = outline.toggle(Panel::Outline, 720., EditorMode::Wysiwyg);
        assert!(!closed.outline && !closed.sidebar);
    }

    #[test]
    fn compact_split_inspector_floats_instead_of_crushing_the_editor() {
        let split = PanelLayout::fit(720., EditorMode::Split, true, true);
        let outline = split.toggle(Panel::Outline, 720., EditorMode::Split);
        assert!(outline.outline);
        assert_eq!(outline.overlay, Some(Panel::Outline));
        assert_eq!(outline.document_width(720.), 720.);
        let sidebar = outline.toggle(Panel::Sidebar, 720., EditorMode::Split);
        assert!(sidebar.sidebar && !sidebar.outline);
        assert_eq!(sidebar.overlay, Some(Panel::Sidebar));
        assert_eq!(sidebar.document_width(720.), 720.);
        let closed = sidebar.toggle(Panel::Sidebar, 720., EditorMode::Split);
        assert!(!closed.sidebar && !closed.outline && closed.overlay.is_none());
    }
}
