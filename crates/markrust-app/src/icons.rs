// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Original, monochrome 20-point toolbar symbols. Embedded SVG keeps the UI
//! crisp at every display scale and avoids a runtime asset-directory dependency.

use gpui::{prelude::*, px, svg, Hsla, Svg};

#[derive(Clone, Copy)]
pub enum Icon {
    Sidebar,
    NewDocument,
    Open,
    Wysiwyg,
    Source,
    Split,
    Outline,
    Close,
}

impl Icon {
    pub fn render(self, color: Hsla) -> Svg {
        svg()
            .data(self.svg().as_bytes())
            .size(px(18.))
            .text_color(color)
    }

    fn svg(self) -> &'static str {
        match self {
            Self::Sidebar => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><rect x="2.25" y="3.25" width="15.5" height="13.5" rx="2"/><path d="M7 3.5v13M4.5 6h.01M4.5 9h.01M4.5 12h.01"/></svg>"#
            }
            Self::NewDocument => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M10.5 2.5h-6a1 1 0 0 0-1 1v13a1 1 0 0 0 1 1h11a1 1 0 0 0 1-1v-8M11 2.5v5h5M6.5 12h7M10 8.5v7"/></svg>"#
            }
            Self::Open => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M2.5 6.5v-2a1 1 0 0 1 1-1h4l2 2h7a1 1 0 0 1 1 1v1M3.2 16.5l-1-8h15.6l-1 8z"/></svg>"#
            }
            Self::Wysiwyg => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M9 3H4a1 1 0 0 0-1 1v12a1 1 0 0 0 1 1h11a1 1 0 0 0 1-1v-5M6 13h3M6 6h2M6 9h1M10 10l1-3 5-5 2 2-5 5zM14.5 3.5l2 2"/></svg>"#
            }
            Self::Source => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="m6 5-4.5 5L6 15m8-10 4.5 5-4.5 5M12 3 8 17"/></svg>"#
            }
            Self::Split => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><rect x="2.25" y="3.25" width="15.5" height="13.5" rx="2"/><path d="M10 3.5v13M5 7h2M5 10h2M5 13h2M13 7h2M13 10h2"/></svg>"#
            }
            Self::Outline => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M7 4h10M7 8h7M7 12h10M7 16h7M3 4h.01M3 8h.01M3 12h.01M3 16h.01"/></svg>"#
            }
            Self::Close => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round"><path d="m6 6 8 8m0-8-8 8"/></svg>"#
            }
        }
    }
}
