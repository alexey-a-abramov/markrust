// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-kind block renderers for the WYSIWYG surface.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    div, img, prelude::*, px, AnyElement, CursorStyle, Entity, FontWeight, SharedString,
    StyledText, TextStyle,
};
use markrust_core::rich::{Block, BlockKind, ColumnAlign, NodeId, RichTree};

use super::block_text::{build_code_layout, build_leaf_layout, BlockTextElement, WysiwygHost};
use crate::highlight::highlight_code_block;
use crate::theme::EditorTheme;

/// Immutable per-frame snapshot the virtualized list renders from.
pub struct RenderSnapshot {
    pub tree: RichTree,
    pub source: String,
    pub theme: EditorTheme,
    /// Directory of the document, for resolving relative image paths.
    pub base_dir: Option<PathBuf>,
    pub editing_code: Option<(NodeId, String)>,
    pub editing_image: Option<(Range<usize>, String)>,
}

pub fn render_top_block<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    index: usize,
    editor: Entity<H>,
) -> AnyElement {
    let Some(block) = snap.tree.blocks.get(index) else {
        return div().into_any_element();
    };
    div()
        .px(px(24.))
        .py(px(4.))
        .child(render_block(snap, block, editor))
        .into_any_element()
}

fn render_block<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    block: &Block,
    editor: Entity<H>,
) -> AnyElement {
    let theme = &snap.theme;
    match &block.kind {
        BlockKind::Paragraph => {
            paragraph_element(snap, block, theme.font_size, FontWeight::NORMAL, editor)
        }
        BlockKind::Heading { level, .. } => {
            let size = theme.heading_font_size(*level);
            div()
                .pt(px(8.))
                .pb(px(2.))
                .child(paragraph_element(
                    snap,
                    block,
                    size,
                    FontWeight::BOLD,
                    editor,
                ))
                .into_any_element()
        }
        BlockKind::CodeBlock { info, literal, .. } => {
            let body = literal.strip_suffix('\n').unwrap_or(literal).to_string();
            let language = info.split_whitespace().next().unwrap_or("").to_string();
            let chip_text = snap
                .editing_code
                .as_ref()
                .and_then(|(id, draft)| (*id == block.id).then(|| draft.clone()))
                .unwrap_or_else(|| {
                    if language.is_empty() {
                        "plain".to_string()
                    } else {
                        language.clone()
                    }
                });
            let editing = snap
                .editing_code
                .as_ref()
                .is_some_and(|(id, _)| *id == block.id);
            let mut code_style = base_text_style(theme, theme.font_size * 0.9, FontWeight::NORMAL);
            code_style.font_family = theme.code_font_family.clone().into();
            let body_start = code_body_source_start(&snap.source, block);
            let mut layout = build_code_layout(&body, body_start, &code_style, theme);
            let hl = code_runs(&body, &language, theme);
            if !hl.is_empty() && !body.is_empty() {
                layout.runs = text_runs_from_highlights(&body, &hl, &code_style);
            }
            let layout = std::sync::Arc::new(layout);
            let line_height = theme.line_height_for_font_size(theme.font_size * 0.9);
            let chip_id = block.id;
            let editor_chip = editor.clone();
            let chip = div()
                .id(("code-lang", block.id.0))
                .text_size(px(11.))
                .text_color(theme.secondary_text)
                .px(px(6.))
                .py(px(2.))
                .mb(px(4.))
                .rounded_md()
                .cursor(CursorStyle::PointingHand)
                .when(editing, |el| {
                    el.bg(theme.code_bg).border_1().border_color(theme.accent)
                })
                .when(!editing, |el| el.bg(theme.code_bg.opacity(0.5)))
                .child(SharedString::from(if editing {
                    format!("{chip_text}|")
                } else {
                    chip_text
                }))
                .on_click(move |_, _, cx| {
                    editor_chip.update(cx, |host, cx| host.edit_code_info(chip_id, cx));
                });
            div()
                .my(px(4.))
                .p(px(12.))
                .rounded_md()
                .bg(theme.code_block_bg)
                .font_family(theme.code_font_family.clone())
                .child(chip)
                .child(BlockTextElement {
                    editor,
                    layout,
                    font_size: theme.font_size * 0.9,
                    line_height,
                    theme: theme.clone(),
                })
                .into_any_element()
        }
        BlockKind::BlockQuote => {
            let children: Vec<AnyElement> = block
                .children
                .iter()
                .map(|child| render_block(snap, child, editor.clone()))
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
            render_list(snap, block, editor).into_any_element()
        }
        BlockKind::ListItem { .. } => {
            let children: Vec<AnyElement> = block
                .children
                .iter()
                .map(|child| render_block(snap, child, editor.clone()))
                .collect();
            div().children(children).into_any_element()
        }
        BlockKind::Table { alignments } => render_table(snap, block, alignments, editor),
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

fn render_list<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    list: &Block,
    editor: Entity<H>,
) -> impl IntoElement {
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
            let item_id = item.id;
            let is_task = task.is_some();
            let children: Vec<AnyElement> = item
                .children
                .iter()
                .map(|child| render_block(snap, child, editor.clone()))
                .collect();
            let mut marker = div()
                .id(("task", item_id.0))
                .min_w(px(20.))
                .text_color(if is_task {
                    theme.accent
                } else {
                    theme.secondary_text
                })
                .text_size(px(theme.font_size))
                .child(glyph);
            if is_task {
                let editor = editor.clone();
                marker = marker
                    .cursor(CursorStyle::PointingHand)
                    .on_click(move |_, _, cx| {
                        editor.update(cx, |host, cx| host.toggle_task(item_id, cx));
                    });
            }
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(8.))
                .child(marker)
                .child(div().flex_1().children(children))
                .into_any_element()
        })
        .collect();
    div().flex().flex_col().gap(px(2.)).children(rows)
}

fn render_table<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    table: &Block,
    alignments: &[ColumnAlign],
    editor: Entity<H>,
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
                            editor.clone(),
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
fn paragraph_element<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    block: &Block,
    font_size: f32,
    base_weight: FontWeight,
    editor: Entity<H>,
) -> AnyElement {
    let theme = &snap.theme;
    let text_style = base_text_style(theme, font_size, base_weight);
    let images: Vec<(String, String, std::ops::Range<usize>)> = block
        .inlines
        .iter()
        .filter_map(|inline| match inline {
            markrust_core::rich::Inline::Image {
                alt,
                url,
                source_range,
                ..
            } => Some((alt.clone(), url.clone(), source_range.clone())),
            _ => None,
        })
        .collect();
    let layout = std::sync::Arc::new(build_leaf_layout(block, &text_style, theme, base_weight));
    let line_height = theme.line_height_for_font_size(font_size);
    let mut container = div().flex().flex_col().gap(px(4.)).child(BlockTextElement {
        editor: editor.clone(),
        layout,
        font_size,
        line_height,
        theme: theme.clone(),
    });
    for (alt, url, image_range) in images {
        let source: SharedString = resolve_image_source(snap, &url).into();
        let caption = snap
            .editing_image
            .as_ref()
            .and_then(|(range, draft)| (*range == image_range).then(|| draft.clone()))
            .unwrap_or_else(|| {
                if alt.is_empty() {
                    "caption".to_string()
                } else {
                    alt.clone()
                }
            });
        let editing = snap
            .editing_image
            .as_ref()
            .is_some_and(|(range, _)| *range == image_range);
        let editor_cap = editor.clone();
        let alt_for_edit = alt.clone();
        container = container.child(
            div()
                .my(px(4.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(img(source).max_w_full().rounded_md())
                .child(
                    div()
                        .id(("img-alt", image_range.start as u64))
                        .text_size(px(12.))
                        .text_color(snap.theme.secondary_text)
                        .italic()
                        .cursor(CursorStyle::PointingHand)
                        .when(editing, |el| {
                            el.border_b_1().border_color(snap.theme.accent)
                        })
                        .child(SharedString::from(if editing {
                            format!("{caption}|")
                        } else {
                            caption
                        }))
                        .on_click(move |_, _, cx| {
                            let range = image_range.clone();
                            let current = alt_for_edit.clone();
                            editor_cap.update(cx, |host, cx| {
                                host.edit_image_alt(range, &current, cx);
                            });
                        }),
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
    TextStyle {
        color: theme.text,
        font_family: theme.font_family.clone().into(),
        font_size: px(font_size).into(),
        font_weight: weight,
        line_height: px(theme.line_height_for_font_size(font_size)).into(),
        ..Default::default()
    }
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

fn code_body_source_start(source: &str, block: &Block) -> usize {
    let slice = source.get(block.source_range.clone()).unwrap_or("");
    match slice.find('\n') {
        Some(i) => block.source_range.start + i + 1,
        None => block.source_range.start,
    }
}

fn text_runs_from_highlights(
    body: &str,
    highlights: &[(std::ops::Range<usize>, gpui::HighlightStyle)],
    style: &TextStyle,
) -> Vec<gpui::TextRun> {
    let mut runs = Vec::new();
    let mut cursor = 0usize;
    for (range, hl) in highlights {
        let start = range.start.max(cursor).min(body.len());
        let end = range.end.min(body.len());
        if start > cursor {
            let mut run = style.to_run(start - cursor);
            run.color = style.color;
            runs.push(run);
        }
        if start < end {
            let mut run = style.to_run(end - start);
            if let Some(color) = hl.color {
                run.color = color;
            }
            runs.push(run);
            cursor = end;
        }
    }
    if cursor < body.len() {
        runs.push(style.to_run(body.len() - cursor));
    }
    if runs.is_empty() {
        runs.push(style.to_run(body.len()));
    }
    runs
}
