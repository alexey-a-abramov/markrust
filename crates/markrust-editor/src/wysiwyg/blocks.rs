// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-kind block renderers for the WYSIWYG surface.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    div, img, prelude::*, px, AnyElement, CursorStyle, Entity, FontWeight, MouseButton, ObjectFit,
    SharedString, StyledText, TextStyle,
};
use markrust_core::html_visual::{
    definition_list_items, footnote_definition, project_html_block, to_superscript, HtmlBlockVisual,
};
use markrust_core::rich::{Block, BlockKind, ColumnAlign, Inline, NodeId, RichTree};

use super::block_text::{
    build_code_layout, build_html_block_layout, build_leaf_layout_inlines,
    build_leaf_layout_revealed, BlockTextElement, RevealState, WidgetImeSink, WysiwygHost,
};
use super::image::{resolve_image_source, ResolvedImage};
use super::inline_layout::{
    classify_paragraph, image_role, inline_image_height, inline_segments, visual_image, ImageRole,
    InlineSegment, ParagraphFlow, BLOCK_IMAGE_MAX_HEIGHT, BLOCK_IMAGE_MAX_WIDTH,
};
use crate::highlight::highlight_code_block;
use crate::theme::EditorTheme;

/// Immutable per-frame snapshot the virtualized list renders from.
#[derive(Clone)]
pub struct RenderSnapshot {
    pub tree: RichTree,
    pub source: String,
    pub theme: EditorTheme,
    /// Directory of the document, for resolving relative image paths.
    pub base_dir: Option<PathBuf>,
    pub editing_code: Option<(NodeId, String)>,
    pub editing_image: Option<(Range<usize>, String)>,
    pub widget_preedit: Option<String>,
    pub caret: usize,
    pub selected_range: Range<usize>,
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
            let chip_label = if editing {
                format!(
                    "{}{}|",
                    chip_text,
                    snap.widget_preedit.as_deref().unwrap_or("")
                )
            } else {
                chip_text
            };
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
            let editor_away = editor.clone();
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
                .child(SharedString::from(chip_label))
                .on_click(move |_, _, cx| {
                    editor_chip.update(cx, |host, cx| host.edit_code_info(chip_id, cx));
                })
                .when(editing, |el| {
                    el.on_mouse_down_out(move |_, _, cx| {
                        editor_away.update(cx, |host, cx| host.finish_widget(cx));
                    })
                });
            let chip = div().relative().child(chip).when(editing, |el| {
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .child(WidgetImeSink {
                            editor: editor.clone(),
                        }),
                )
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
                    hug_width: false,
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
        BlockKind::Alert { .. } => render_alert(snap, block, editor),
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
        BlockKind::ThematicBreak => thematic_rule(theme),
        BlockKind::FootnoteDefinition { label } => {
            render_footnote_def_nested(snap, block, label, editor)
        }
        BlockKind::DefinitionList => render_definition_list_nested(snap, block, editor),
        BlockKind::DefinitionItem { .. } => render_definition_item(snap, block, editor),
        BlockKind::DefinitionTerm => {
            render_nested_inlines(snap, block, theme.font_size, FontWeight::SEMIBOLD, editor)
        }
        BlockKind::DefinitionDetails => {
            let inner =
                render_nested_inlines(snap, block, theme.font_size, FontWeight::NORMAL, editor);
            div()
                .pl(px(16.))
                .text_color(theme.secondary_text)
                .child(inner)
                .into_any_element()
        }
        BlockKind::Opaque { raw } => render_opaque(snap, block, raw, editor),
    }
}

fn render_alert<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    block: &Block,
    editor: Entity<H>,
) -> AnyElement {
    let BlockKind::Alert {
        kind,
        title,
        tag_range,
        chrome_range,
    } = &block.kind
    else {
        return div().into_any_element();
    };
    let theme = &snap.theme;
    let accent = theme.alert_accent(*kind);
    let reveal = RevealState {
        caret: snap.caret,
        selection: snap.selected_range.clone(),
    };
    let show_chrome = !chrome_range.is_empty() && reveal.intersects(chrome_range);
    let header = if show_chrome {
        let slice = snap.source.get(chrome_range.clone()).unwrap_or("");
        let mut text_style = base_text_style(theme, theme.font_size * 0.85, FontWeight::SEMIBOLD);
        text_style.color = accent;
        let layout = std::sync::Arc::new(build_code_layout(
            slice,
            chrome_range.start,
            &text_style,
            theme,
        ));
        let line_height = theme.line_height_for_font_size(theme.font_size * 0.85);
        BlockTextElement {
            editor: editor.clone(),
            layout,
            font_size: theme.font_size * 0.85,
            line_height,
            theme: theme.clone(),
            hug_width: false,
        }
        .into_any_element()
    } else {
        let caret_at = if tag_range.start < tag_range.end {
            tag_range.start
        } else {
            block.source_range.start
        };
        let editor_click = editor.clone();
        let label = kind.callout_label(title.as_deref());
        div()
            .id(("alert-label", block.id.0))
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(accent)
            .cursor(CursorStyle::PointingHand)
            .child(SharedString::from(label))
            .on_click(move |_, window, cx| {
                editor_click.update(cx, |host, cx| {
                    host.click_source(caret_at, false, window, cx);
                });
            })
            .into_any_element()
    };
    let children: Vec<AnyElement> = block
        .children
        .iter()
        .map(|child| render_block(snap, child, editor.clone()))
        .collect();
    div()
        .my(px(4.))
        .pl(px(12.))
        .border_l_3()
        .border_color(accent)
        .child(header)
        .children(children)
        .into_any_element()
}

fn thematic_rule(theme: &EditorTheme) -> AnyElement {
    div()
        .w_full()
        .my(px(16.))
        .h(px(1.))
        .bg(theme.table_delimiter)
        .into_any_element()
}

fn render_opaque<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    block: &Block,
    raw: &str,
    editor: Entity<H>,
) -> AnyElement {
    let theme = &snap.theme;
    if let Some((label, body)) = footnote_definition(raw) {
        return render_footnote_def(theme, label, body);
    }
    if let Some(items) = definition_list_items(raw) {
        return render_definition_list(theme, items);
    }
    match project_html_block(raw) {
        HtmlBlockVisual::Hidden => div().into_any_element(),
        HtmlBlockVisual::ThematicBreak => thematic_rule(theme),
        HtmlBlockVisual::Image { url, alt } => render_image(
            snap,
            &alt,
            &url,
            block.source_range.clone(),
            editor,
            ImageRole::Block,
            theme.font_size,
        ),
        HtmlBlockVisual::Flow {
            text,
            source_at,
            runs,
        } => {
            let text_style = base_text_style(theme, theme.font_size, FontWeight::NORMAL);
            let layout = build_html_block_layout(
                &text,
                &source_at,
                &runs,
                block.source_range.start,
                &text_style,
                theme,
            );
            let line_height = theme.line_height_for_font_size(theme.font_size);
            BlockTextElement {
                editor,
                layout: Arc::new(layout),
                font_size: theme.font_size,
                line_height,
                theme: theme.clone(),
                hug_width: false,
            }
            .into_any_element()
        }
    }
}

fn render_footnote_def_nested<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    block: &Block,
    label: &str,
    editor: Entity<H>,
) -> AnyElement {
    let theme = &snap.theme;
    let mark = to_superscript(label).unwrap_or_else(|| label.to_string());
    let body: Vec<AnyElement> = block
        .children
        .iter()
        .map(|child| {
            render_nested_inlines(
                snap,
                child,
                theme.font_size * 0.95,
                FontWeight::NORMAL,
                editor.clone(),
            )
        })
        .collect();
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(8.))
        .my(px(4.))
        .pt(px(8.))
        .border_t_1()
        .border_color(theme.separator)
        .child(div().text_color(theme.link).child(SharedString::from(mark)))
        .child(
            div()
                .flex_1()
                .text_color(theme.secondary_text)
                .children(body),
        )
        .into_any_element()
}

fn render_definition_list_nested<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    block: &Block,
    editor: Entity<H>,
) -> AnyElement {
    let rows: Vec<AnyElement> = block
        .children
        .iter()
        .map(|item| render_definition_item(snap, item, editor.clone()))
        .collect();
    div().my(px(4.)).children(rows).into_any_element()
}

fn render_definition_item<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    item: &Block,
    editor: Entity<H>,
) -> AnyElement {
    let children: Vec<AnyElement> = item
        .children
        .iter()
        .map(|child| render_block(snap, child, editor.clone()))
        .collect();
    div()
        .flex()
        .flex_col()
        .mb(px(8.))
        .children(children)
        .into_any_element()
}

fn render_nested_inlines<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    block: &Block,
    font_size: f32,
    weight: FontWeight,
    editor: Entity<H>,
) -> AnyElement {
    if matches!(block.kind, BlockKind::Paragraph) || !block.inlines.is_empty() {
        return paragraph_element(snap, block, font_size, weight, editor);
    }
    if block.children.is_empty() {
        return div().into_any_element();
    }
    let children: Vec<AnyElement> = block
        .children
        .iter()
        .map(|child| render_nested_inlines(snap, child, font_size, weight, editor.clone()))
        .collect();
    div().children(children).into_any_element()
}

fn render_footnote_def(theme: &EditorTheme, label: &str, body: &str) -> AnyElement {
    let mark = to_superscript(label).unwrap_or_else(|| label.to_string());
    let text_style = base_text_style(theme, theme.font_size * 0.95, FontWeight::NORMAL);
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(8.))
        .my(px(4.))
        .pt(px(8.))
        .border_t_1()
        .border_color(theme.separator)
        .child(div().text_color(theme.link).child(SharedString::from(mark)))
        .child(
            div().flex_1().text_color(theme.secondary_text).child(
                StyledText::new(body.to_string()).with_default_highlights(&text_style, vec![]),
            ),
        )
        .into_any_element()
}

fn render_definition_list(theme: &EditorTheme, items: Vec<(String, String)>) -> AnyElement {
    let term_style = base_text_style(theme, theme.font_size, FontWeight::SEMIBOLD);
    let detail_style = base_text_style(theme, theme.font_size, FontWeight::NORMAL);
    let rows: Vec<AnyElement> =
        items
            .into_iter()
            .map(|(term, details)| {
                div()
                    .flex()
                    .flex_col()
                    .mb(px(8.))
                    .child(StyledText::new(term).with_default_highlights(&term_style, vec![]))
                    .child(div().pl(px(16.)).text_color(theme.secondary_text).child(
                        StyledText::new(details).with_default_highlights(&detail_style, vec![]),
                    ))
                    .into_any_element()
            })
            .collect();
    div().my(px(4.)).children(rows).into_any_element()
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
                    let cell_start = cell.source_range.start;
                    let editor_menu = editor.clone();
                    let mut el = div()
                        .flex_1()
                        .px(px(10.))
                        .py(px(6.))
                        .border_1()
                        .border_color(theme.separator)
                        .on_mouse_down(MouseButton::Right, move |_, window, cx| {
                            editor_menu.update(cx, |host, cx| {
                                host.open_table_menu(cell_start, window, cx);
                            });
                        })
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

/// A leaf block's inline content as wrapped rich text, with images as GPUI
/// `img()` pixels (filesystem `PathBuf`, decoded on the background executor).
/// Remote `http(s)` images use a URL cache path once fetched; until then (and
/// on failure) the alt placeholder is shown. Mixed text+image paragraphs are
/// a wrapping flex row (GPUI cannot mix Image and glyphs in one `TextRun`);
/// standalone image paragraphs stay block-sized.
fn paragraph_element<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    block: &Block,
    font_size: f32,
    base_weight: FontWeight,
    editor: Entity<H>,
) -> AnyElement {
    let theme = &snap.theme;
    let text_style = base_text_style(theme, font_size, base_weight);
    let line_height = theme.line_height_for_font_size(font_size);
    let reveal = RevealState {
        caret: snap.caret,
        selection: snap.selected_range.clone(),
    };
    let flow = classify_paragraph(&block.inlines);
    let role = image_role(flow).unwrap_or(ImageRole::Inline);
    match flow {
        ParagraphFlow::TextOnly => {
            let layout =
                build_leaf_layout_revealed(block, &text_style, theme, base_weight, &reveal);
            BlockTextElement {
                editor,
                layout: Arc::new(layout),
                font_size,
                line_height,
                theme: theme.clone(),
                hug_width: false,
            }
            .into_any_element()
        }
        ParagraphFlow::Standalone => {
            let mut children: Vec<AnyElement> = Vec::new();
            for inline in &block.inlines {
                if let Some((alt, url, source_range)) = visual_image(inline) {
                    children.push(render_image(
                        snap,
                        &alt,
                        &url,
                        source_range,
                        editor.clone(),
                        ImageRole::Block,
                        font_size,
                    ));
                }
            }
            if children.is_empty() {
                let layout =
                    build_leaf_layout_revealed(block, &text_style, theme, base_weight, &reveal);
                children.push(
                    BlockTextElement {
                        editor,
                        layout: Arc::new(layout),
                        font_size,
                        line_height,
                        theme: theme.clone(),
                        hug_width: false,
                    }
                    .into_any_element(),
                );
            }
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .children(children)
                .into_any_element()
        }
        ParagraphFlow::Mixed => {
            let mut children: Vec<AnyElement> = Vec::new();
            for seg in inline_segments(&block.inlines) {
                match seg {
                    InlineSegment::Text { start, end } => {
                        push_text_child(
                            &mut children,
                            &block.inlines[start..end],
                            block.source_range.clone(),
                            &text_style,
                            theme,
                            base_weight,
                            font_size,
                            line_height,
                            editor.clone(),
                            true,
                            &reveal,
                        );
                    }
                    InlineSegment::Image { index } => {
                        if let Some((alt, url, source_range)) = visual_image(&block.inlines[index])
                        {
                            children.push(render_image(
                                snap,
                                &alt,
                                &url,
                                source_range,
                                editor.clone(),
                                role,
                                font_size,
                            ));
                        }
                    }
                }
            }
            if children.is_empty() {
                let layout =
                    build_leaf_layout_revealed(block, &text_style, theme, base_weight, &reveal);
                children.push(
                    BlockTextElement {
                        editor,
                        layout: Arc::new(layout),
                        font_size,
                        line_height,
                        theme: theme.clone(),
                        hug_width: false,
                    }
                    .into_any_element(),
                );
            }
            div()
                .w_full()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .children(children)
                .into_any_element()
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_text_child<H: WysiwygHost>(
    children: &mut Vec<AnyElement>,
    inlines: &[Inline],
    block_range: Range<usize>,
    text_style: &TextStyle,
    theme: &EditorTheme,
    base_weight: FontWeight,
    font_size: f32,
    line_height: f32,
    editor: Entity<H>,
    hug_width: bool,
    reveal: &RevealState,
) {
    if inlines.is_empty() {
        return;
    }
    let layout =
        build_leaf_layout_inlines(inlines, block_range, text_style, theme, base_weight, reveal);
    if layout.text.is_empty() {
        return;
    }
    children.push(
        BlockTextElement {
            editor,
            layout: Arc::new(layout),
            font_size,
            line_height,
            theme: theme.clone(),
            hug_width,
        }
        .into_any_element(),
    );
}

fn render_image<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    alt: &str,
    url: &str,
    image_range: Range<usize>,
    editor: Entity<H>,
    role: ImageRole,
    font_size: f32,
) -> AnyElement {
    let caption = snap
        .editing_image
        .as_ref()
        .and_then(|(range, draft)| (*range == image_range).then(|| draft.clone()))
        .unwrap_or_else(|| {
            if alt.is_empty() {
                "Add a caption".to_string()
            } else {
                alt.to_string()
            }
        });
    let editing = snap
        .editing_image
        .as_ref()
        .is_some_and(|(range, _)| *range == image_range);
    let editor_cap = editor.clone();
    let editor_away = editor.clone();
    let editor_click = editor.clone();
    let alt_for_edit = alt.to_string();
    let caret_at = image_range.start;
    let fallback_label = if alt.is_empty() {
        "Missing image".to_string()
    } else {
        alt.to_string()
    };
    let secondary = snap.theme.secondary_text;
    let code_bg = snap.theme.code_bg;
    let inline_h = px(inline_image_height(font_size));
    let edit_on_click = role == ImageRole::Inline;
    let click_range = image_range.clone();
    // GPUI: `img(String)` is an *embedded asset*, not a file. Markdown images
    // must be `PathBuf` (local file or a populated URL cache) so decode runs
    // on the background executor. Do not `img()` a missing cache path: GPUI
    // would cache the failure and never retry after the fetch writes the file.
    let resolved = resolve_image_source(snap.base_dir.as_deref(), url);
    let ready_source = match resolved {
        ResolvedImage::File(path) if path.is_file() => Some(img(path)),
        ResolvedImage::Uri(uri) => Some(img(uri)),
        ResolvedImage::File(_) => None,
    };
    let pixels = if let Some(source) = ready_source {
        let source = source
            .id(("md-img", image_range.start as u64))
            .object_fit(ObjectFit::Contain)
            .rounded_md()
            .cursor(CursorStyle::PointingHand);
        let source = match role {
            ImageRole::Inline => {
                let fallback_label = fallback_label.clone();
                source
                    .h(inline_h)
                    .max_h(inline_h)
                    .flex_none()
                    .with_loading(move || {
                        div()
                            .h(inline_h)
                            .w(inline_h)
                            .rounded_md()
                            .bg(code_bg)
                            .into_any_element()
                    })
                    .with_fallback(move || missing_image_fallback(secondary, &fallback_label))
            }
            ImageRole::Block => source
                .max_w(px(BLOCK_IMAGE_MAX_WIDTH))
                .max_h(px(BLOCK_IMAGE_MAX_HEIGHT))
                .with_loading(move || {
                    div()
                        .h(px(72.))
                        .w_full()
                        .rounded_md()
                        .bg(code_bg)
                        .into_any_element()
                })
                .with_fallback(move || missing_image_fallback(secondary, &fallback_label)),
        };
        source
            .on_click(move |_, window, cx| {
                editor_click.update(cx, |host, cx| {
                    host.click_source(caret_at, false, window, cx);
                    if edit_on_click {
                        host.edit_image_alt(click_range.clone(), &alt_for_edit, cx);
                    }
                });
            })
            .into_any_element()
    } else {
        let placeholder = match role {
            ImageRole::Inline => div()
                .h(inline_h)
                .flex_none()
                .child(missing_image_fallback(secondary, &fallback_label)),
            ImageRole::Block => div().child(missing_image_fallback(secondary, &fallback_label)),
        };
        placeholder
            .id(("md-img", image_range.start as u64))
            .cursor(CursorStyle::PointingHand)
            .on_click(move |_, window, cx| {
                editor_click.update(cx, |host, cx| {
                    host.click_source(caret_at, false, window, cx);
                    if edit_on_click {
                        host.edit_image_alt(click_range.clone(), &alt_for_edit, cx);
                    }
                });
            })
            .into_any_element()
    };
    let show_caption = role == ImageRole::Block || editing;
    let caption_el = div()
        .id(("img-alt", image_range.start as u64))
        .text_size(px(12.))
        .text_color(snap.theme.secondary_text)
        .italic()
        .cursor(CursorStyle::PointingHand)
        .when(editing, |el| {
            el.border_b_1().border_color(snap.theme.accent)
        })
        .child(SharedString::from(if editing {
            format!(
                "{}{}|",
                caption,
                snap.widget_preedit.as_deref().unwrap_or("")
            )
        } else {
            caption
        }))
        .on_click({
            let range = image_range.clone();
            let current = alt.to_string();
            move |_, _, cx| {
                editor_cap.update(cx, |host, cx| {
                    host.edit_image_alt(range.clone(), &current, cx);
                });
            }
        })
        .when(editing, |el| {
            el.on_mouse_down_out(move |_, _, cx| {
                editor_away.update(cx, |host, cx| host.finish_widget(cx));
            })
        });
    let caption_row = div().relative().child(caption_el).when(editing, |el| {
        el.child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .child(WidgetImeSink {
                    editor: editor.clone(),
                }),
        )
    });
    match role {
        ImageRole::Inline => {
            let mut el = div().flex_none().flex().flex_col().child(pixels);
            if show_caption {
                el = el.child(caption_row);
            }
            el.into_any_element()
        }
        ImageRole::Block => div()
            .my(px(4.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(pixels)
            .child(caption_row)
            .into_any_element(),
    }
}

fn missing_image_fallback(secondary: gpui::Hsla, label: &str) -> AnyElement {
    div()
        .text_color(secondary)
        .italic()
        .child(SharedString::from(format!("🖼 {label}")))
        .into_any_element()
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
