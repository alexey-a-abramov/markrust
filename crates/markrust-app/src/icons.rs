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
    // Markdown formatting toolbar
    Bold,
    Italic,
    Code,
    Link,
    Heading1,
    Heading2,
    Heading3,
    Paragraph,
    Quote,
    UnorderedList,
    OrderedList,
    TaskList,
    HorizontalRule,
    CodeBlock,
    Strikethrough,
    Image,
    Table,
    Indent,
    Outdent,
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
            Self::Bold => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M5.5 4.5h5.5a3.25 3.25 0 0 1 0 6.5H5.5zM5.5 11h6a3.25 3.25 0 0 1 0 6.5H5.5z"/></svg>"#
            }
            Self::Italic => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M8 4.5h7M5 15.5h7M11 4.5l-2 11"/></svg>"#
            }
            Self::Code => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M10 4c-2 0-3 1.5-3 3s1 3 0 4.5-2 3-2 3M7 6.5l-3.5 2L7 11M13 6.5l3.5 2L13 11"/></svg>"#
            }
            Self::Link => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M8 12a3.5 3.5 0 0 0 5 0l3-3a3.5 3.5 0 0 0-5-5l-1 1M12 8a3.5 3.5 0 0 0-5 0l-3 3a3.5 3.5 0 0 0 5 5l1-1"/></svg>"#
            }
            Self::Heading1 => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M4.5 4.5v11M11 4.5v11M4.5 10H11M14 7v8.5h2"/></svg>"#
            }
            Self::Heading2 => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M4.5 4.5v11M11 4.5v11M4.5 10H11M14 5.5h5l-3 3a3 3 0 0 0 3 5h0a3 3 0 0 1-3 3h-2"/></svg>"#
            }
            Self::Heading3 => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M4.5 4.5v11M11 4.5v11M4.5 10H11M14.5 5.5h4a2.5 2.5 0 0 1 0 5h-3a2.5 2.5 0 0 0 0 5h4"/></svg>"#
            }
            Self::Paragraph => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M7 4.5h8M7 10h8M7 15.5h5M5 4.5v11"/></svg>"#
            }
            Self::Quote => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M6 6.5a2.5 2.5 0 1 0-2 4l1 1a2.5 2.5 0 1 1-2 4M14 6.5a2.5 2.5 0 1 0-2 4l1 1a2.5 2.5 0 1 1-2 4"/></svg>"#
            }
            Self::UnorderedList => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M7 5.5h10M7 10h10M7 14.5h10M3.5 5.5h.01M3.5 10h.01M3.5 14.5h.01"/></svg>"#
            }
            Self::OrderedList => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M7 5.5h10M7 10h10M7 14.5h10M3 5v-.5a1 1 0 0 1 1-1h.5M3 10.5h2a1 1 0 0 0-1-1v-1M4 14.5a1 1 0 0 0-1 1v.5h2.5"/></svg>"#
            }
            Self::TaskList => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><rect x="2.5" y="4" width="4" height="4" rx="0.75"/><path d="M3.5 6l1 1 2-2M8.5 6h8.5M8.5 10h8.5M2.5 11.5h4v4h-4zM3.5 13.5l1 1 2-2M8.5 15h8.5"/></svg>"#
            }
            Self::HorizontalRule => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round"><path d="M2.5 10h15"/></svg>"#
            }
            Self::CodeBlock => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><rect x="2.5" y="3.5" width="15" height="13" rx="1.5"/><path d="M7 8l-2 2 2 2M13 8l2 2-2 2"/></svg>"#
            }
            Self::Strikethrough => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M5 6.5a3 3 0 0 1 3-2.5h0a3 3 0 0 1 3 2.5M5 13.5a3 3 0 0 0 3 2.5h0a3 3 0 0 0 3-2.5M3 10h14"/></svg>"#
            }
            Self::Image => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><rect x="2.5" y="3.5" width="15" height="13" rx="1.5"/><path d="m3.5 14 4-4 3 3 3-3 3 3M13 7.5a1 1 0 1 0 0-.01"/></svg>"#
            }
            Self::Table => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><rect x="2.5" y="3.5" width="15" height="13" rx="1.5"/><path d="M2.5 9h15M2.5 14h15M8 3.5v13"/></svg>"#
            }
            Self::Indent => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M11 4.5h6M11 10h6M11 15.5h6M3 10h2m0 0L7 8m-2 2 2 2"/></svg>"#
            }
            Self::Outdent => {
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M3 4.5h6M3 10h6M3 15.5h6M13 10H7m0 0 2-2m-2 2 2 2"/></svg>"#
            }
        }
    }
}
