// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Line-box model for WYSIWYG paragraphs that mix text and images.
//!
//! GPUI cannot put an `Image` inside a shaped `TextRun`, so mixed paragraphs
//! are a horizontal stack of runs (hug-width text + height-capped image) that
//! wrap as units. Standalone image paragraphs stay block-sized.

use std::ops::Range;

use markrust_core::html_visual;
use markrust_core::rich::Inline;

/// Display height of an inline image, in ems of the surrounding font.
pub const INLINE_IMAGE_EM: f32 = 1.5;
/// Max CSS pixels for a standalone (block) image.
pub const BLOCK_IMAGE_MAX_HEIGHT: f32 = 480.0;
/// Max CSS pixels wide for a standalone (block) image.
pub const BLOCK_IMAGE_MAX_WIDTH: f32 = 720.0;

/// How a leaf block's inlines should be painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParagraphFlow {
    /// No images: one fill-width text element (existing wrap).
    TextOnly,
    /// Image(s) and no other visible text: block-sized pixels + caption.
    Standalone,
    /// Visible text plus at least one image: one visual row of runs when they fit.
    Mixed,
}

/// Role that sizes a rendered image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageRole {
    /// Height capped at [`INLINE_IMAGE_EM`].
    Inline,
    /// Natural size, capped at [`BLOCK_IMAGE_MAX_HEIGHT`].
    Block,
}

/// One flex item in a mixed line-box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineSegment {
    /// Inclusive-exclusive slice of `block.inlines` (text / breaks / opaque).
    Text {
        start: usize,
        end: usize,
    },
    Image {
        index: usize,
    },
}

pub fn classify_paragraph(inlines: &[Inline]) -> ParagraphFlow {
    let has_image = inlines.iter().any(is_visual_image);
    let has_text = inlines.iter().any(inline_has_visible_text);
    match (has_image, has_text) {
        (false, _) => ParagraphFlow::TextOnly,
        (true, false) => ParagraphFlow::Standalone,
        (true, true) => ParagraphFlow::Mixed,
    }
}

/// Markdown `![alt](url)` or a safe inline HTML `<img>`.
pub fn visual_image(inline: &Inline) -> Option<(String, String, Range<usize>)> {
    match inline {
        Inline::Image {
            alt,
            url,
            source_range,
            ..
        } => Some((alt.clone(), url.clone(), source_range.clone())),
        Inline::OpaqueInline {
            raw, source_range, ..
        } => html_visual::html_inline_image(raw).map(|(url, alt)| (alt, url, source_range.clone())),
        _ => None,
    }
}

fn is_visual_image(inline: &Inline) -> bool {
    visual_image(inline).is_some()
}

pub fn image_role(flow: ParagraphFlow) -> Option<ImageRole> {
    match flow {
        ParagraphFlow::TextOnly => None,
        ParagraphFlow::Standalone => Some(ImageRole::Block),
        ParagraphFlow::Mixed => Some(ImageRole::Inline),
    }
}

pub fn inline_image_height(font_size: f32) -> f32 {
    font_size * INLINE_IMAGE_EM
}

/// Split inlines on `Image` so each run can be a flex item.
pub fn inline_segments(inlines: &[Inline]) -> Vec<InlineSegment> {
    let mut out = Vec::new();
    let mut text_start = 0usize;
    for (i, inline) in inlines.iter().enumerate() {
        if is_visual_image(inline) {
            if text_start < i {
                out.push(InlineSegment::Text {
                    start: text_start,
                    end: i,
                });
            }
            out.push(InlineSegment::Image { index: i });
            text_start = i + 1;
        }
    }
    if text_start < inlines.len() {
        out.push(InlineSegment::Text {
            start: text_start,
            end: inlines.len(),
        });
    }
    out
}

/// Greedy wrap of atomic run widths. A run that is wider than `container`
/// still occupies its own row (images and text slices wrap as units).
#[cfg(test)]
pub fn pack_runs(widths: &[f32], container: f32) -> Vec<Range<usize>> {
    if widths.is_empty() {
        return Vec::new();
    }
    let container = container.max(0.0);
    let mut rows = Vec::new();
    let mut start = 0usize;
    let mut used = 0.0f32;
    for (i, &w) in widths.iter().enumerate() {
        let w = w.max(0.0);
        if i > start && used + w > container {
            rows.push(start..i);
            start = i;
            used = w;
        } else {
            used += w;
        }
    }
    rows.push(start..widths.len());
    rows
}

fn inline_has_visible_text(inline: &Inline) -> bool {
    match inline {
        Inline::Run { text, .. } => !text.trim().is_empty(),
        Inline::Math { literal, .. } => !literal.trim().is_empty(),
        Inline::OpaqueInline { raw, .. } => {
            if html_visual::html_inline_image(raw).is_some()
                || html_visual::opaque_inline_is_caret_chrome(raw)
            {
                false
            } else {
                !raw.trim().is_empty()
            }
        }
        Inline::Image { .. } | Inline::SoftBreak | Inline::HardBreak { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use markrust_core::rich::{import_markdown, IdGen, RichTree};

    fn import(source: &str) -> RichTree {
        import_markdown(source, &mut IdGen::default())
    }

    /// Average glyph width used only to size mixed runs in layout tests.
    const GLYPH_EM: f32 = 0.5;

    fn estimate_mixed_widths(inlines: &[Inline], font_size: f32) -> Vec<f32> {
        let mut widths = Vec::new();
        for seg in inline_segments(inlines) {
            match seg {
                InlineSegment::Text { start, end } => {
                    let mut n = 0usize;
                    for inline in &inlines[start..end] {
                        match inline {
                            Inline::Run { text, .. } => n += text.chars().count(),
                            Inline::Math { literal, .. } => n += literal.chars().count(),
                            Inline::OpaqueInline { raw, .. } => n += raw.chars().count(),
                            Inline::SoftBreak | Inline::HardBreak { .. } => n += 1,
                            Inline::Image { .. } => {}
                        }
                    }
                    if n > 0 {
                        widths.push(n as f32 * font_size * GLYPH_EM);
                    }
                }
                InlineSegment::Image { .. } => widths.push(inline_image_height(font_size)),
            }
        }
        widths
    }

    fn visual_rows(source: &str, container: f32, font_size: f32) -> usize {
        let tree = import(source);
        let inlines = &tree.blocks[0].inlines;
        match classify_paragraph(inlines) {
            ParagraphFlow::TextOnly => 1,
            ParagraphFlow::Standalone => {
                inlines.iter().filter(|i| is_visual_image(i)).count().max(1)
            }
            ParagraphFlow::Mixed => {
                let widths = estimate_mixed_widths(inlines, font_size);
                pack_runs(&widths, container).len()
            }
        }
    }

    #[test]
    fn mixed_hello_image_world_is_one_visual_row() {
        let font = 16.0;
        let container = 480.0;
        assert_eq!(
            classify_paragraph(&import("hello ![x](a.png) world\n").blocks[0].inlines),
            ParagraphFlow::Mixed
        );
        assert_eq!(
            visual_rows("hello ![x](a.png) world\n", container, font),
            1,
            "small inline image must sit on the same row as adjacent text"
        );
        let segs = inline_segments(&import("hello ![x](a.png) world\n").blocks[0].inlines);
        assert_eq!(segs.len(), 3);
        assert!(matches!(segs[0], InlineSegment::Text { .. }));
        assert!(matches!(segs[1], InlineSegment::Image { .. }));
        assert!(matches!(segs[2], InlineSegment::Text { .. }));
    }

    #[test]
    fn standalone_image_paragraph_is_block_sized() {
        let inlines = &import("![x](a.png)\n").blocks[0].inlines;
        assert_eq!(classify_paragraph(inlines), ParagraphFlow::Standalone);
        assert_eq!(
            image_role(ParagraphFlow::Standalone),
            Some(ImageRole::Block)
        );
        assert_eq!(visual_rows("![x](a.png)\n", 480.0, 16.0), 1);
        assert!(
            BLOCK_IMAGE_MAX_HEIGHT > inline_image_height(16.0) * 4.0,
            "standalone cap ({BLOCK_IMAGE_MAX_HEIGHT}) must dwarf inline 1.5em ({})",
            inline_image_height(16.0)
        );
    }

    #[test]
    fn whitespace_only_around_image_is_standalone() {
        let inlines = &import(" ![x](a.png) \n").blocks[0].inlines;
        assert_eq!(classify_paragraph(inlines), ParagraphFlow::Standalone);
    }

    #[test]
    fn inline_height_is_between_1_2_and_1_5_em() {
        let font = 16.0;
        let h = inline_image_height(font);
        assert!(h >= font * 1.2 - f32::EPSILON);
        assert!(h <= font * 1.5 + f32::EPSILON);
    }

    #[test]
    fn mixed_role_is_inline() {
        assert_eq!(image_role(ParagraphFlow::Mixed), Some(ImageRole::Inline));
        assert_eq!(image_role(ParagraphFlow::TextOnly), None);
    }

    #[test]
    fn small_runs_pack_as_one_row() {
        let rows = pack_runs(&[40.0, 24.0, 40.0], 200.0);
        assert_eq!(rows, vec![0..3]);
    }

    #[test]
    fn runs_wrap_as_units() {
        let rows = pack_runs(&[300.0, 50.0, 300.0], 400.0);
        assert_eq!(rows, vec![0..2, 2..3]);
    }

    #[test]
    fn oversized_run_keeps_its_own_row() {
        let rows = pack_runs(&[500.0, 20.0], 400.0);
        assert_eq!(rows, vec![0..1, 1..2]);
    }

    #[test]
    fn long_mixed_text_wraps_the_image_as_a_unit() {
        let long = format!("{} ![x](a.png) z\n", "aaaaa ".repeat(40));
        let font = 16.0;
        assert!(
            visual_rows(&long, 240.0, font) > 1,
            "a long mixed paragraph must wrap rather than stay one overflowing row"
        );
        assert_eq!(
            classify_paragraph(&import(&long).blocks[0].inlines),
            ParagraphFlow::Mixed
        );
    }
}
