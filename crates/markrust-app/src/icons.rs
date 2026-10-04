// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Compact monochrome toolbar symbols. Embedded SVG and native style labels
//! stay crisp at every display scale without a runtime asset-directory dependency.

use gpui::{div, prelude::*, px, svg, AnyElement, FontWeight, Hsla};

// A single grid and stroke contract keeps small toolbar symbols coherent.
macro_rules! symbol {
    ($geometry:literal) => {
        concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"black\" stroke-width=\"2\" stroke-linecap=\"round\" stroke-linejoin=\"round\">",
            $geometry,
            "</svg>"
        )
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    pub fn render(self, color: Hsla) -> AnyElement {
        if let Some(label) = self.text_label() {
            return div()
                .h(px(18.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.))
                .line_height(px(18.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .child(label)
                .into_any_element();
        }
        svg()
            .data(self.svg().as_bytes())
            .size(px(18.))
            .text_color(color)
            .into_any_element()
    }

    /// Style names use actual type so their numerals cannot be confused with
    /// hand-drawn symbols or change meaning at small display scales.
    pub(crate) fn text_label(self) -> Option<&'static str> {
        match self {
            Self::Heading1 => Some("H1"),
            Self::Heading2 => Some("H2"),
            Self::Heading3 => Some("H3"),
            Self::Paragraph => Some("Body"),
            _ => None,
        }
    }

    fn svg(self) -> &'static str {
        match self {
            Self::Sidebar => {
                symbol!(
                    r#"<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M9 4v16M6 8h.01M6 12h.01M6 16h.01"/>"#
                )
            }
            Self::NewDocument => {
                symbol!(
                    r#"<path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9l-6-6ZM14 3v6h6M8 15h8M12 11v8"/>"#
                )
            }
            Self::Open => {
                symbol!(
                    r#"<path d="M3 9V6a2 2 0 0 1 2-2h5l2 3h7a2 2 0 0 1 2 2M3 9h18l-2 11H5L3 9Z"/>"#
                )
            }
            Self::Wysiwyg => {
                symbol!(
                    r#"<path d="M11 4H5a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2h13a2 2 0 0 0 2-2v-6M7 9h3M7 13h2M7 17h6M13 11l1-4 5-5 3 3-5 5-4 1ZM18 3l3 3"/>"#
                )
            }
            Self::Source => {
                symbol!(r#"<path d="m7 6-5 6 5 6m10-12 5 6-5 6M14 4l-4 16"/>"#)
            }
            Self::Split => {
                symbol!(
                    r#"<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M12 4v16M6 8h3M6 12h3M6 16h3M15 8h3M15 12h3M15 16h3"/>"#
                )
            }
            Self::Outline => {
                symbol!(r#"<path d="M8 5h13M8 12h10M8 19h13M3 5h.01M3 12h.01M3 19h.01"/>"#)
            }
            Self::Close => {
                symbol!(r#"<path d="m6 6 12 12m0-12L6 18"/>"#)
            }
            Self::Bold => {
                symbol!(r#"<path d="M7 4h6a4 4 0 0 1 0 8H7m0 0h7a4 4 0 0 1 0 8H7V4Z"/>"#)
            }
            Self::Italic => {
                symbol!(r#"<path d="M9 4h10M5 20h10M15 4 9 20"/>"#)
            }
            Self::Code => {
                symbol!(r#"<path d="m8 6-6 6 6 6m8-12 6 6-6 6"/>"#)
            }
            Self::Link => {
                symbol!(
                    r#"<path d="M10 14a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-2 2M14 10a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l2-2"/>"#
                )
            }
            Self::Heading1 | Self::Heading2 | Self::Heading3 | Self::Paragraph => {
                unreachable!("block-style buttons use native text labels")
            }
            Self::Quote => {
                symbol!(r#"<path d="M4 10h6v7H4v-7c0-4 2-6 6-6M14 10h6v7h-6v-7c0-4 2-6 6-6"/>"#)
            }
            Self::UnorderedList => {
                symbol!(
                    r#"<path d="M9 6h12M9 12h12M9 18h12"/><circle cx="3" cy="6" r="1" fill="black" stroke="none"/><circle cx="3" cy="12" r="1" fill="black" stroke="none"/><circle cx="3" cy="18" r="1" fill="black" stroke="none"/>"#
                )
            }
            Self::OrderedList => {
                // Two legible numbers are preferable to three tiny scribbles.
                symbol!(
                    r#"<path d="M10 7h11M10 17h11"/><path stroke-width="1.5" d="m3 5 2-1v6M3 10h4M3 16a2 2 0 1 1 4 0c0 1-1 2-4 4h4"/>"#
                )
            }
            Self::TaskList => {
                symbol!(
                    r#"<rect x="3" y="3" width="6" height="6" rx="1"/><rect x="3" y="15" width="6" height="6" rx="1"/><path d="m4.5 6 1 1 2-2M13 6h8M13 12h8M13 18h8"/>"#
                )
            }
            Self::HorizontalRule => {
                symbol!(r#"<path d="M3 12h18"/>"#)
            }
            Self::CodeBlock => {
                symbol!(
                    r#"<rect x="3" y="4" width="18" height="16" rx="2"/><path d="m9 9-3 3 3 3m6-6 3 3-3 3"/>"#
                )
            }
            Self::Strikethrough => {
                symbol!(
                    r#"<path d="M17 6a5 5 0 0 0-5-3C5 3 5 10 12 10m0 4c7 0 7 7 0 7a5 5 0 0 1-5-3M3 12h18"/>"#
                )
            }
            Self::Image => {
                symbol!(
                    r#"<rect x="3" y="4" width="18" height="16" rx="2"/><circle cx="8" cy="9" r="1.5"/><path d="m3 17 5-5 4 4 4-6 5 7"/>"#
                )
            }
            Self::Table => {
                symbol!(
                    r#"<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M3 10h18M3 15h18M9 4v16M15 4v16"/>"#
                )
            }
            Self::Indent => {
                symbol!(r#"<path d="M3 4h18M3 20h18M12 9h9M12 15h9m-17-6 3 3-3 3"/>"#)
            }
            Self::Outdent => {
                symbol!(r#"<path d="M3 4h18M3 20h18M12 9h9M12 15h9M7 9l-3 3 3 3"/>"#)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_styles_keep_literal_native_labels_in_heading_order() {
        assert_eq!(Icon::Heading1.text_label(), Some("H1"));
        assert_eq!(Icon::Heading2.text_label(), Some("H2"));
        assert_eq!(Icon::Heading3.text_label(), Some("H3"));
        assert_eq!(Icon::Paragraph.text_label(), Some("Body"));
    }

    #[test]
    fn vector_symbols_share_a_crisp_grid_without_external_assets() {
        for icon in [
            Icon::Sidebar,
            Icon::NewDocument,
            Icon::Open,
            Icon::Wysiwyg,
            Icon::Source,
            Icon::Split,
            Icon::Outline,
            Icon::Close,
            Icon::Bold,
            Icon::Italic,
            Icon::Code,
            Icon::Link,
            Icon::Quote,
            Icon::UnorderedList,
            Icon::OrderedList,
            Icon::TaskList,
            Icon::HorizontalRule,
            Icon::CodeBlock,
            Icon::Strikethrough,
            Icon::Image,
            Icon::Table,
            Icon::Indent,
            Icon::Outdent,
        ] {
            let source = icon.svg();
            assert!(source.contains("viewBox=\"0 0 24 24\""), "{icon:?}");
            assert!(source.contains("stroke-width=\"2\""), "{icon:?}");
            assert!(source.contains("stroke-linecap=\"round\""), "{icon:?}");
            assert!(source.contains("stroke-linejoin=\"round\""), "{icon:?}");
            assert!(source.ends_with("</svg>"), "{icon:?}");
            assert!(
                !source.contains("<image") && !source.contains("href="),
                "{icon:?}"
            );
            assert!(
                !source.contains("<script") && !source.contains("<text"),
                "{icon:?}"
            );
        }
    }

    #[test]
    fn code_modes_and_list_styles_have_distinct_conventional_shapes() {
        assert_ne!(Icon::Code.svg(), Icon::Source.svg());
        assert_ne!(Icon::Code.svg(), Icon::CodeBlock.svg());
        assert!(Icon::CodeBlock.svg().contains("<rect"));
        assert!(Icon::Quote.svg().contains("M4 10h6v7H4"));
        assert!(Icon::Quote.svg().contains("M14 10h6v7h-6"));
        assert!(Icon::UnorderedList.svg().contains("<circle"));
        assert!(Icon::OrderedList.svg().contains("m3 5 2-1v6"));
        assert!(Icon::OrderedList.svg().contains("M3 16a2 2"));
        assert!(Icon::TaskList.svg().contains("<rect"));
    }
}
