// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-kind block renderers for the WYSIWYG surface.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    div, img, prelude::*, px, AnyElement, FontStyle, FontWeight, SharedString, StrikethroughStyle,
    StyledText, TextRun, TextStyle, UnderlineStyle,
};
use markrust_core::rich::{Block, BlockKind, BreakStyle, ColumnAlign, Inline, MarkSet, RichTree};

use crate::highlight::highlight_code_block;
use crate::theme::EditorTheme;

/// Immutable per-frame snapshot the virtualized list renders from.
pub struct RenderSnapshot {
    pub tree: RichTree,
    pub theme: EditorTheme,
    /// Directory of the document, for resolving relative image paths.
    pub base_dir: Option<PathBuf>,
}

pub fn render_top_block(snap: &Arc<RenderSnapshot>, index: usize) -> AnyElement {
    let Some(block) = snap.tree.blocks.get(index) else {
        return div().into_any_element();
    };
    div()
        .px(px(24.))
        .py(px(4.))
        .child(render_block(snap, block))
        .into_any_element()
}

fn render_block(snap: &Arc<RenderSnapshot>, block: &Block) -> AnyElement {
    let theme = &snap.theme;
    match &block.kind {
        BlockKind::Paragraph => paragraph_element(snap, block, theme.font_size, FontWeight::NORMAL),
        BlockKind::Heading { level, .. } => {
            let size = theme.heading_font_size(*level);
            div()
                .pt(px(8.))
                .pb(px(2.))
                .child(paragraph_element(snap, block, size, FontWeight::BOLD))
                .into_any_element()
        }
        BlockKind::CodeBlock { info, literal, .. } => {
            let body = literal.strip_suffix('\n').unwrap_or(literal).to_string();
            let language = info.split_whitespace().next().unwrap_or("").to_string();
            let runs = code_runs(&body, &language, theme);
            let text_style = base_text_style(theme, theme.font_size * 0.9, FontWeight::NORMAL);
            let mut container = div()
                .my(px(4.))
                .p(px(12.))
                .rounded_md()
                .bg(theme.code_block_bg)
                .font_family(theme.code_font_family.clone());
            if !language.is_empty() {
                container = container.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.secondary_text)
                        .mb(px(4.))
                        .child(SharedString::from(language)),
                );
            }
            container
                .child(StyledText::new(body).with_default_highlights(&text_style, runs))
                .into_any_element()
        }
        BlockKind::BlockQuote => {
            let children: Vec<AnyElement> = block
                .children
                .iter()
                .map(|child| render_block(snap, child))
                .collect();
            div()
                .my(px(2.))
                .pl(px(12.))
                .border_l_3()
                .border_color(theme.blockquote_border)
                .text_color(theme.blockquote_text)
                .children(children)
                .into_any_element()
        }
        BlockKind::BulletList { .. } | BlockKind::OrderedList { .. } => {
            render_list(snap, block).into_any_element()
        }
        BlockKind::ListItem { .. } => {
            // Rendered by render_list; standalone fallback:
            let children: Vec<AnyElement> = block
                .children
                .iter()
                .map(|child| render_block(snap, child))
                .collect();
            div().children(children).into_any_element()
        }
        BlockKind::Table { alignments } => render_table(snap, block, alignments),
        BlockKind::TableRow { .. } | BlockKind::TableCell => div().into_any_element(),
        BlockKind::ThematicBreak => div()
            .my(px(12.))
            .h(px(2.))
            .rounded_full()
            .bg(theme.separator)
            .into_any_element(),
        BlockKind::Opaque { raw } => {
            let text_style = base_text_style(theme, theme.font_size * 0.9, FontWeight::NORMAL);
            div()
                .my(px(2.))
                .p(px(8.))
                .rounded_md()
                .bg(theme.code_bg)
                .font_family(theme.code_font_family.clone())
                .text_color(theme.secondary_text)
                .child(StyledText::new(raw.clone()).with_default_highlights(&text_style, vec![]))
                .into_any_element()
        }
    }
}

fn render_list(snap: &Arc<RenderSnapshot>, list: &Block) -> impl IntoElement {
    let theme = snap.theme.clone();
    let (ordered, start) = match &list.kind {
        BlockKind::OrderedList { start, .. } => (true, *start),
        _ => (false, 1),
    };
    let rows: Vec<AnyElement> = list
        .children
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let task = match &item.kind {
                BlockKind::ListItem { task } => *task,
                _ => None,
            };
            let glyph: SharedString = if let Some(checked) = task {
                if checked {
                    "☑".into()
                } else {
                    "☐".into()
                }
            } else if ordered {
                format!("{}.", start + i).into()
            } else {
                "•".into()
            };
            let children: Vec<AnyElement> = item
                .children
                .iter()
                .map(|child| render_block(snap, child))
                .collect();
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(8.))
                .child(
                    div()
                        .min_w(px(20.))
                        .text_color(if task.is_some() {
                            theme.accent
                        } else {
                            theme.secondary_text
                        })
                        .text_size(px(theme.font_size))
                        .child(glyph),
                )
                .child(div().flex_1().children(children))
                .into_any_element()
        })
        .collect();
    div().flex().flex_col().gap(px(2.)).children(rows)
}

fn render_table(
    snap: &Arc<RenderSnapshot>,
    table: &Block,
    alignments: &[ColumnAlign],
) -> AnyElement {
    let theme = snap.theme.clone();
    let rows: Vec<AnyElement> = table
        .children
        .iter()
        .map(|row| {
            let header = matches!(row.kind, BlockKind::TableRow { header: true });
            let cells: Vec<AnyElement> = row
                .children
                .iter()
                .enumerate()
                .map(|(c, cell)| {
                    let align = alignments.get(c).copied().unwrap_or(ColumnAlign::None);
                    let weight = if header {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::NORMAL
                    };
                    let mut el = div()
                        .flex_1()
                        .px(px(10.))
                        .py(px(6.))
                        .border_1()
                        .border_color(theme.separator)
                        .child(paragraph_element(
                            snap,
                            cell,
                            theme.font_size * 0.95,
                            weight,
                        ));
                    el = match align {
                        ColumnAlign::Center => el.text_center(),
                        ColumnAlign::Right => el.text_right(),
                        _ => el,
                    };
                    el.into_any_element()
                })
                .collect();
            let mut row_el = div().flex().flex_row();
            if header {
                row_el = row_el.bg(theme.table_header_bg);
            }
            row_el.children(cells).into_any_element()
        })
        .collect();
    div()
        .my(px(6.))
        .rounded_md()
        .overflow_hidden()
        .flex()
        .flex_col()
        .children(rows)
        .into_any_element()
}

/// A leaf block's inline content as wrapped rich text (plus trailing image
/// elements for standalone images).
fn paragraph_element(
    snap: &Arc<RenderSnapshot>,
    block: &Block,
    font_size: f32,
    base_weight: FontWeight,
) -> AnyElement {
    let theme = &snap.theme;
    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    let mut images: Vec<(String, String)> = Vec::new(); // (alt, url)
    let text_style = base_text_style(theme, font_size, base_weight);

    let mut push_run = |text: &mut String, runs: &mut Vec<TextRun>, s: &str, run: TextRun| {
        if s.is_empty() {
            return;
        }
        text.push_str(s);
        let mut run = run;
        run.len = s.len();
        runs.push(run);
    };

    for inline in &block.inlines {
        match inline {
            Inline::Run {
                text: t,
                marks,
                link,
                ..
            } => {
                let mut run = text_style.to_run(0);
                if marks.contains(MarkSet::BOLD) {
                    run.font.weight = FontWeight::BOLD;
                }
                if marks.contains(MarkSet::ITALIC) {
                    run.font.style = FontStyle::Italic;
                }
                if marks.contains(MarkSet::STRIKE) {
                    run.strikethrough = Some(StrikethroughStyle {
                        thickness: px(1.),
                        color: Some(theme.secondary_text.into()),
                    });
                }
                if marks.contains(MarkSet::CODE) {
                    run.font.family = theme.code_font_family.clone().into();
                    run.background_color = Some(theme.code_bg.into());
                }
                if link.is_some() {
                    run.color = theme.link.into();
                    run.underline = Some(UnderlineStyle {
                        thickness: px(1.),
                        color: Some(theme.link.into()),
                        wavy: false,
                    });
                }
                push_run(&mut text, &mut runs, t, run);
            }
            Inline::Image { alt, url, .. } => {
                if block.inlines.len() == 1 {
                    images.push((alt.clone(), url.clone()));
                } else {
                    let mut run = text_style.to_run(0);
                    run.color = theme.image_text.into();
                    run.font.style = FontStyle::Italic;
                    let label = format!("🖼 {alt}");
                    push_run(&mut text, &mut runs, &label, run);
                }
            }
            Inline::SoftBreak => {
                push_run(&mut text, &mut runs, " ", text_style.to_run(0));
            }
            Inline::HardBreak {
                style: BreakStyle::TwoSpaces | BreakStyle::Backslash,
            } => {
                push_run(&mut text, &mut runs, "\n", text_style.to_run(0));
            }
            Inline::OpaqueInline { raw, .. } => {
                let mut run = text_style.to_run(0);
                run.color = theme.secondary_text.into();
                run.font.family = theme.code_font_family.clone().into();
                push_run(&mut text, &mut runs, raw, run);
            }
        }
    }

    let mut container = div().flex().flex_col().gap(px(4.));
    if !text.is_empty() {
        container = container.child(StyledText::new(text).with_runs(runs));
    }
    for (alt, url) in images {
        let source: SharedString = resolve_image_source(snap, &url).into();
        container = container.child(
            div()
                .my(px(4.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(img(source).max_w_full().rounded_md())
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(snap.theme.secondary_text)
                        .italic()
                        .child(SharedString::from(alt)),
                ),
        );
    }
    container.into_any_element()
}

fn resolve_image_source(snap: &Arc<RenderSnapshot>, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("data:") {
        return url.to_string();
    }
    match &snap.base_dir {
        Some(dir) => dir.join(url).to_string_lossy().into_owned(),
        None => url.to_string(),
    }
}

fn base_text_style(theme: &EditorTheme, font_size: f32, weight: FontWeight) -> TextStyle {
    let mut style = TextStyle::default();
    style.color = theme.text.into();
    style.font_family = theme.font_family.clone().into();
    style.font_size = px(font_size).into();
    style.font_weight = weight;
    style.line_height = px(theme.line_height_for_font_size(font_size)).into();
    style
}

fn code_runs(
    body: &str,
    language: &str,
    theme: &EditorTheme,
) -> Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> {
    let mut spans = highlight_code_block(language, body, 0);
    // StyledText requires sorted, non-overlapping, in-bounds highlight
    // ranges; tree-sitter captures can nest and overlap.
    spans.sort_by_key(|s| (s.start_byte, s.end_byte));
    let mut out = Vec::new();
    let mut cursor = 0usize;
    for span in spans {
        let start = span.start_byte.max(cursor).min(body.len());
        let end = span.end_byte.min(body.len());
        if start >= end || !body.is_char_boundary(start) || !body.is_char_boundary(end) {
            continue;
        }
        let color = crate::source::element::syntax_color(theme, span.kind);
        out.push((
            start..end,
            gpui::HighlightStyle {
                color: Some(color),
                ..Default::default()
            },
        ));
        cursor = end;
    }
    out
}
