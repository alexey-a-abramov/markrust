// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-kind block renderers for the WYSIWYG surface.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    canvas, div, img, prelude::*, px, AnyElement, App, Bounds, CursorStyle, Element, ElementId,
    Entity, FontWeight, GlobalElementId, InspectorElementId, LayoutId, MouseButton, ObjectFit,
    Pixels, SharedString, StyledText, TextStyle, Window,
};
use markrust_core::html_visual::{
    definition_list_items, footnote_definition, project_html_block, to_superscript, HtmlBlockVisual,
};
use markrust_core::rich::{
    alert_title_range, blank_caret_gap_after_last, blank_caret_gap_before, blank_caret_gaps,
    link_reference_def_chrome, toc_visible_range, Block, BlockKind, ColumnAlign, Inline, NodeId,
    PrefixBlank, RichTree,
};

use super::block_text::{
    apply_code_fence_reveal, apply_structural_chrome, build_blank_gap_layout_with_source,
    build_code_block_layout, build_code_layout, build_html_block_layout, build_leaf_layout_inlines,
    build_leaf_layout_revealed, build_prefix_blank_layout, chrome_hosts_for, layout_html_block,
    project_leading_horizontal_whitespace, project_trailing_horizontal_whitespace,
    BlockTextElement, ChromeHosts, OverlayTarget, RevealState, WidgetOverlay, WysiwygHost,
};
use super::image::{resolve_image_source, ResolvedImage};
use super::inline_layout::{
    classify_paragraph, image_role, inline_image_height, inline_segments, visual_image, ImageRole,
    InlineSegment, ParagraphFlow, BLOCK_IMAGE_MAX_HEIGHT, BLOCK_IMAGE_MAX_WIDTH,
};
use crate::highlight::highlight_code_block;
use crate::theme::EditorTheme;

const TOP_BLOCK_VERTICAL_PADDING: f32 = 4.;
const LIST_ROW_GAP: f32 = 2.;

/// Immutable per-frame snapshot the virtualized list renders from.
#[derive(Clone)]
pub struct RenderSnapshot {
    pub tree: RichTree,
    pub source: String,
    pub theme: EditorTheme,
    /// Directory of the document, for resolving relative image paths.
    pub base_dir: Option<PathBuf>,
    /// Original local image path → validated, content-addressed cache path
    /// approved for GPUI's decoder. The source path never reaches GPUI.
    pub local_image_paths: HashMap<PathBuf, PathBuf>,
    /// Paths currently being classified off the UI thread. This distinguishes
    /// the brief first-frame placeholder from a failed or missing image.
    pub local_image_pending: HashSet<PathBuf>,
    /// Normalized document `data:` URL → validated, content-addressed cache
    /// path approved for GPUI's decoder. The URI itself never reaches GPUI.
    pub data_image_paths: HashMap<String, PathBuf>,
    /// `data:` URLs currently undergoing bounded background preflight.
    pub data_image_pending: HashSet<String>,
    pub editing_code: Option<(NodeId, String)>,
    pub editing_image: Option<(Range<usize>, String)>,
    pub caret: usize,
    pub selected_range: Range<usize>,
}

impl RenderSnapshot {
    fn reveal_state(&self) -> RevealState {
        // Live WYSIWYG keeps one glyph projection. Context hints belong in
        // paint-only tint and app chrome, never in the document's text flow.
        RevealState::HIDDEN
    }
}

pub fn render_top_block<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    index: usize,
    editor: Entity<H>,
) -> AnyElement {
    if snap.tree.blocks.is_empty() {
        let gap = blank_caret_gap_after_last(&snap.tree).unwrap_or(0..snap.tree.source_len);
        let mut leaf = source_blank_gap_layout(snap, gap.clone());
        leaf.caret_range = Some(gap);
        return div()
            .w_full()
            .min_w_0()
            .px(px(24.))
            .py(px(TOP_BLOCK_VERTICAL_PADDING))
            .child(blank_layout_element(snap, leaf, editor))
            .into_any_element();
    }
    let Some(block) = snap.tree.blocks.get(index) else {
        return div().into_any_element();
    };
    let mut root = div()
        .w_full()
        .min_w_0()
        .px(px(24.))
        .py(px(TOP_BLOCK_VERTICAL_PADDING));
    if let Some(gap) = blank_caret_gap_before(&snap.tree, index) {
        root = root.child(blank_gap_element(snap, gap, editor.clone()));
    }
    let trailing = (index + 1 == snap.tree.blocks.len())
        .then(|| blank_caret_gap_after_last(&snap.tree))
        .flatten();
    if let Some(gap) = trailing {
        root = root.child(render_block(snap, block, editor.clone()));
        let mut leaf = source_blank_gap_layout(snap, gap.clone());
        leaf.caret_range = Some(gap);
        // Reserve the same separator row and two 4px root paddings as the
        // paragraph this EOF draft becomes. Its first letter must not jump
        // downward when comrak finally creates a nonempty paragraph node.
        let sibling_spacing = paragraph_draft_spacing(&snap.theme);
        root = root.child(
            div()
                .w_full()
                .min_w_0()
                .mt(px(sibling_spacing))
                .child(blank_layout_element(snap, leaf, editor)),
        );
    } else {
        root = root.child(render_block(snap, block, editor));
    }
    root.into_any_element()
}

fn blank_gap_element<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    gap: Range<usize>,
    editor: Entity<H>,
) -> AnyElement {
    blank_layout_element(snap, source_blank_gap_layout(snap, gap), editor)
}

fn source_blank_gap_layout(
    snap: &RenderSnapshot,
    gap: Range<usize>,
) -> super::block_text::LeafLayout {
    let style = base_text_style(&snap.theme, snap.theme.font_size, FontWeight::NORMAL);
    build_blank_gap_layout_with_source(gap, &snap.source, &style)
}

fn blank_layout_element<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    layout: super::block_text::LeafLayout,
    editor: Entity<H>,
) -> AnyElement {
    let theme = &snap.theme;
    let font_size = theme.font_size;
    let line_height = theme.line_height_for_font_size(font_size);
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

fn prefix_blanks_for<'a>(block: &Block, tree: &'a RichTree) -> Vec<&'a PrefixBlank> {
    tree.empty_prefix_homes
        .iter()
        .filter(|blank| {
            let h = blank.home;
            h >= block.source_range.start
                && h <= block.source_range.end
                && !block
                    .children
                    .iter()
                    .any(|c| c.source_range.start <= h && h <= c.source_range.end)
        })
        .collect()
}

fn prefix_blank_element<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    blank: &PrefixBlank,
    editor: Entity<H>,
) -> AnyElement {
    let theme = &snap.theme;
    let font_size = theme.font_size;
    let line_height = theme.line_height_for_font_size(font_size);
    let reveal = snap.reveal_state();
    let text_style = base_text_style(theme, font_size, FontWeight::NORMAL);
    BlockTextElement {
        editor,
        layout: Arc::new(build_prefix_blank_layout(
            &snap.source,
            blank,
            &reveal,
            &text_style,
            theme,
        )),
        font_size,
        line_height,
        theme: theme.clone(),
        hug_width: false,
    }
    .into_any_element()
}

fn with_prefix_blank_leaves<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    block: &Block,
    editor: Entity<H>,
) -> Vec<AnyElement> {
    let mut slots: Vec<(usize, AnyElement)> = block
        .children
        .iter()
        .map(|child| {
            (
                child.source_range.start,
                render_block(snap, child, editor.clone()),
            )
        })
        .collect();
    for blank in prefix_blanks_for(block, &snap.tree) {
        slots.push((
            blank.home,
            prefix_blank_element(snap, blank, editor.clone()),
        ));
    }
    slots.sort_by_key(|(k, _)| *k);
    slots.into_iter().map(|(_, el)| el).collect()
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
        BlockKind::Toc { .. } => render_toc(snap, block, editor),
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
            let reveal = snap.reveal_state();
            let mut layout =
                build_code_block_layout(&body, &snap.source, block, &code_style, theme);
            let hl = code_runs(&body, &language, theme);
            if !hl.is_empty() && !body.is_empty() {
                layout.runs = text_runs_from_highlights(&body, &hl, &code_style);
            }
            apply_code_fence_reveal(
                &mut layout,
                block,
                &snap.source,
                &reveal,
                &code_style,
                theme,
            );
            let layout = std::sync::Arc::new(layout);
            let line_height = theme.line_height_for_font_size(theme.font_size * 0.9);
            let editor_away = editor.clone();
            let chip_font = 11.0;
            let chip_lh = theme.line_height_for_font_size(chip_font);
            let chip = div()
                .id(("code-lang", block.id.0))
                .px(px(6.))
                .py(px(2.))
                .mb(px(4.))
                .rounded_md()
                .cursor(CursorStyle::PointingHand)
                .when(editing, |el| {
                    el.bg(theme.code_bg).border_1().border_color(theme.accent)
                })
                .when(!editing, |el| el.bg(theme.code_bg.opacity(0.5)))
                .when(editing, |el| {
                    el.on_mouse_down_out(move |_, _, cx| {
                        editor_away.update(cx, |host, cx| host.finish_widget(cx));
                    })
                })
                .child(WidgetOverlay {
                    editor: editor.clone(),
                    prefix: String::new(),
                    text: chip_text,
                    editing,
                    font_size: chip_font,
                    line_height: chip_lh,
                    theme: theme.clone(),
                    color: theme.secondary_text,
                    italic: false,
                    monospace: true,
                    hug_width: true,
                    single_line: false,
                    target: OverlayTarget::CodeInfo(block.id),
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
            let children = with_prefix_blank_leaves(snap, block, editor);
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
            let children = with_prefix_blank_leaves(snap, block, editor);
            div().children(children).into_any_element()
        }
        BlockKind::Table { alignments } => render_table(snap, block, alignments, editor),
        BlockKind::TableRow { .. } | BlockKind::TableCell => div().into_any_element(),
        BlockKind::ThematicBreak => render_thematic_break(snap, block, editor),
        BlockKind::FootnoteDefinition { label } => {
            render_footnote_def_nested(snap, block, label, editor)
        }
        BlockKind::LinkReferenceDefinition { .. } => {
            paragraph_element(snap, block, theme.font_size, FontWeight::NORMAL, editor)
        }
        BlockKind::DefinitionList => render_definition_list_nested(snap, block, editor),
        BlockKind::DefinitionItem { .. } => render_definition_item(snap, block, editor),
        BlockKind::DefinitionTerm => {
            render_nested_inlines(snap, block, theme.font_size, FontWeight::SEMIBOLD, editor)
        }
        BlockKind::DefinitionDetails => {
            let reveal = snap.reveal_state();
            let hosts = chrome_hosts_for(&snap.tree, block.id);
            let show_colon = reveal.intersects(&block.source_range)
                || revealed_details_prefix(&snap.source, block, &reveal, &hosts);
            let inner = with_prefix_blank_leaves(snap, block, editor);
            let mut wrap = div().text_color(theme.secondary_text);
            if !show_colon {
                wrap = wrap.pl(px(16.));
            }
            wrap.children(inner).into_any_element()
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
    let reveal = snap.reveal_state();
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
        let caret_at = alert_title_range(tag_range, chrome_range)
            .map(|r| r.start)
            .or_else(|| {
                block
                    .children
                    .iter()
                    .find(|child| child.source_range.start >= tag_range.end)
                    .map(|child| child.source_range.start)
            })
            .unwrap_or(tag_range.end);
        let editor_click = editor.clone();
        let label = kind.callout_label(title.as_deref());
        div()
            .id(("alert-label", block.id.0))
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(accent)
            .cursor(CursorStyle::PointingHand)
            .child(SharedString::from(label))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |_, window, cx| {
                editor_click.update(cx, |host, cx| {
                    host.click_source(caret_at, false, window, cx);
                });
            })
            .into_any_element()
    };
    let children = with_prefix_blank_leaves(snap, block, editor);
    div()
        .my(px(4.))
        .pl(px(12.))
        .border_l_3()
        .border_color(accent)
        .child(header)
        .children(children)
        .into_any_element()
}

fn render_toc<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    block: &Block,
    editor: Entity<H>,
) -> AnyElement {
    let theme = &snap.theme;
    let reveal = snap.reveal_state();
    if reveal.intersects(&block.source_range) {
        return paragraph_element(snap, block, theme.font_size, FontWeight::NORMAL, editor);
    }
    let entries = snap.tree.outline();
    if entries.is_empty() {
        let caret_at = toc_visible_range(&snap.source, block.source_range.clone()).start;
        let editor_click = editor.clone();
        return div()
            .id(("toc-empty", block.id.0))
            .py(px(4.))
            .text_size(px(theme.font_size * 0.9))
            .text_color(theme.secondary_text)
            .cursor(CursorStyle::PointingHand)
            .child(SharedString::from("No headings"))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |_, window, cx| {
                editor_click.update(cx, |host, cx| {
                    host.click_source(caret_at, false, window, cx);
                });
            })
            .into_any_element();
    }
    let rows: Vec<AnyElement> = entries
        .iter()
        .map(|(offset, level, title)| {
            let indent = px(12.0 * (*level as f32 - 1.0).max(0.0));
            let jump = *offset;
            let editor_click = editor.clone();
            let label = if title.trim().is_empty() {
                SharedString::from("(untitled)")
            } else {
                SharedString::from(title.clone())
            };
            div()
                .id(("toc-item", jump as u64))
                .pl(indent)
                .py(px(2.))
                .text_size(px(theme.font_size))
                .text_color(theme.link)
                .cursor(CursorStyle::PointingHand)
                .child(label)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    editor_click.update(cx, |host, cx| {
                        host.click_source(jump, false, window, cx);
                    });
                })
                .into_any_element()
        })
        .collect();
    div()
        .id(("toc", block.id.0))
        .py(px(4.))
        .children(rows)
        .into_any_element()
}

fn render_thematic_break<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    block: &Block,
    editor: Entity<H>,
) -> AnyElement {
    let theme = &snap.theme;
    let reveal = snap.reveal_state();
    if reveal.intersects(&block.source_range) {
        let text_style = base_text_style(theme, theme.font_size, FontWeight::NORMAL);
        let hosts = chrome_hosts_for(&snap.tree, block.id);
        let layout = build_leaf_layout_revealed(
            block,
            &snap.source,
            &text_style,
            theme,
            FontWeight::NORMAL,
            &reveal,
            &hosts,
        );
        let line_height = theme.line_height_for_font_size(theme.font_size);
        return BlockTextElement {
            editor,
            layout: Arc::new(layout),
            font_size: theme.font_size,
            line_height,
            theme: theme.clone(),
            hug_width: false,
        }
        .into_any_element();
    }
    thematic_rule(theme, editor, block.source_range.clone())
}

fn thematic_rule<H: WysiwygHost>(
    theme: &EditorTheme,
    editor: Entity<H>,
    click_range: Range<usize>,
) -> AnyElement {
    let editor_click = editor.clone();
    div()
        .id(("thematic-rule", click_range.start as u64))
        .w_full()
        .my(px(8.))
        .py(px(8.))
        .relative()
        .cursor(CursorStyle::PointingHand)
        .child(painted_bounds_hit(editor))
        .child(div().w_full().h(px(1.)).bg(theme.table_delimiter))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| {
            editor_click.update(cx, |host, cx| {
                host.select_source_range(click_range.clone(), window, cx);
            });
        })
        .into_any_element()
}

fn painted_bounds_hit<H: WysiwygHost>(editor: Entity<H>) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, _, cx| {
            editor.update(cx, |host, _cx| host.report_painted_bounds(bounds));
        },
    )
    .absolute()
    .inset_0()
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
        HtmlBlockVisual::Hidden => {
            let text_style = base_text_style(theme, theme.font_size, FontWeight::NORMAL);
            let reveal = snap.reveal_state();
            let layout = layout_html_block(&snap.source, block, &text_style, theme, &reveal);
            if layout.text.is_empty() {
                return div().into_any_element();
            }
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
        HtmlBlockVisual::ThematicBreak => render_thematic_break(snap, block, editor),
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
            let reveal = snap.reveal_state();
            let layout = build_html_block_layout(
                &text,
                &source_at,
                &runs,
                &snap.source,
                block,
                &text_style,
                theme,
                &reveal,
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
    let reveal = snap.reveal_state();
    let hosts = chrome_hosts_for(&snap.tree, block.id);
    let show_source = reveal.intersects(&block.source_range)
        || revealed_footnote_prefix(&snap.source, block, &reveal, &hosts);
    let body: Vec<AnyElement> = {
        let mut slots: Vec<(usize, AnyElement)> = block
            .children
            .iter()
            .map(|child| {
                (
                    child.source_range.start,
                    render_nested_inlines(
                        snap,
                        child,
                        theme.font_size * 0.95,
                        FontWeight::NORMAL,
                        editor.clone(),
                    ),
                )
            })
            .collect();
        for blank in prefix_blanks_for(block, &snap.tree) {
            slots.push((
                blank.home,
                prefix_blank_element(snap, blank, editor.clone()),
            ));
        }
        slots.sort_by_key(|(k, _)| *k);
        slots.into_iter().map(|(_, el)| el).collect()
    };
    if show_source {
        return div()
            .flex()
            .flex_col()
            .my(px(4.))
            .children(body)
            .into_any_element();
    }
    let mark = to_superscript(label).unwrap_or_else(|| label.to_string());
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
    let rows = with_prefix_blank_leaves(snap, block, editor);
    div().my(px(4.)).children(rows).into_any_element()
}

fn render_definition_item<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    item: &Block,
    editor: Entity<H>,
) -> AnyElement {
    let children = with_prefix_blank_leaves(snap, item, editor);
    div()
        .flex()
        .flex_col()
        .mb(px(8.))
        .children(children)
        .into_any_element()
}

fn revealed_details_prefix(
    source: &str,
    block: &Block,
    reveal: &RevealState,
    hosts: &ChromeHosts,
) -> bool {
    let at = block.source_range.start.min(source.len());
    super::block_text::prefix_range_shows_details(source, at, reveal, hosts)
}

fn revealed_footnote_prefix(
    source: &str,
    block: &Block,
    reveal: &RevealState,
    hosts: &ChromeHosts,
) -> bool {
    let at = block.source_range.start.min(source.len());
    super::block_text::prefix_range_shows_footnote(source, at, reveal, hosts)
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
    let mut rows: Vec<(usize, AnyElement)> = list
        .children
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let task = match &item.kind {
                BlockKind::ListItem { task } => *task,
                _ => None,
            };
            let reveal = snap.reveal_state();
            let item_revealed = reveal.intersects(&item.source_range);
            let paints_marker_in_leaf = item
                .children
                .iter()
                .any(|c| matches!(c.kind, BlockKind::Paragraph | BlockKind::Heading { .. }));
            let hide_pretty = item_revealed && paints_marker_in_leaf;
            let item_id = item.id;
            let is_task = task.is_some();
            let mut children: Vec<AnyElement> = item
                .children
                .iter()
                .map(|child| render_block(snap, child, editor.clone()))
                .collect();
            if children.is_empty() {
                for blank in prefix_blanks_for(item, &snap.tree) {
                    children.push(prefix_blank_element(snap, blank, editor.clone()));
                }
            }
            let mut row = div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(8.));
            if hide_pretty && !is_task {
                row = row.child(div().flex_1().min_w_0().children(children));
            } else {
                let glyph: SharedString = if hide_pretty {
                    SharedString::from("")
                } else if let Some(checked) = task {
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
                let mut marker = div()
                    .id(("task", item_id.0))
                    .min_w(px(20.))
                    .flex_shrink_0()
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
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(move |_, _, cx| {
                            editor.update(cx, |host, cx| host.toggle_task(item_id, cx));
                        });
                }
                row = row
                    .child(marker)
                    .child(div().flex_1().min_w_0().children(children));
            }
            (item.source_range.start, row.into_any_element())
        })
        .collect();
    // A same-marker list is one CommonMark container even after an empty
    // item is exited. Preserve its ordinary blank paragraph as an unbulleted
    // source-backed leaf, rather than snapping the caret into the next item.
    for gap in list_blank_gaps(list, &snap.tree) {
        // This draft will become a separate top-level paragraph as soon as
        // it receives text. Reserve both neighboring separator rows and root
        // paddings now; the first character must not move its baseline or the
        // following list. The surrounding list already supplies its row gap.
        let spacing = paragraph_draft_spacing(&theme) - LIST_ROW_GAP;
        rows.push((
            gap.start,
            div()
                .w_full()
                .min_w_0()
                .mt(px(spacing))
                .mb(px(spacing))
                .child(blank_gap_element(snap, gap, editor.clone()))
                .into_any_element(),
        ));
    }
    rows.sort_by_key(|(source, _)| *source);
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(LIST_ROW_GAP))
        .children(rows.into_iter().map(|(_, row)| row))
}

fn paragraph_draft_spacing(theme: &EditorTheme) -> f32 {
    theme.line_height_for_font_size(theme.font_size) + 2. * TOP_BLOCK_VERTICAL_PADDING
}

fn list_blank_gaps(list: &Block, tree: &RichTree) -> Vec<Range<usize>> {
    blank_caret_gaps(tree)
        .into_iter()
        .filter(|gap| {
            list.children
                .iter()
                .skip(1)
                .any(|item| item.source_range.start == gap.end)
        })
        .collect()
}

fn render_table<H: WysiwygHost>(
    snap: &Arc<RenderSnapshot>,
    table: &Block,
    alignments: &[ColumnAlign],
    editor: Entity<H>,
) -> AnyElement {
    let theme = snap.theme.clone();
    let mut rows: Vec<AnyElement> = Vec::new();
    for row in &table.children {
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
                    .min_w_0()
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
        let mut row_el = div().w_full().min_w_0().flex().flex_row();
        if header {
            row_el = row_el.bg(theme.table_header_bg);
        }
        rows.push(row_el.children(cells).into_any_element());
    }
    div()
        .w_full()
        .min_w_0()
        .my(px(6.))
        .rounded_md()
        .overflow_hidden()
        .flex()
        .flex_col()
        .children(rows)
        .into_any_element()
}

/// A leaf block's inline content as wrapped rich text, with images as GPUI
/// `img()` pixels (validated filesystem cache `PathBuf`s, decoded on the
/// background executor). Local and `data:` images are materialized before
/// render; remote `http(s)` images use a URL cache path once fetched. Until
/// then (and on failure) the alt placeholder is shown. Mixed text+image
/// paragraphs are a wrapping flex row (GPUI cannot mix Image and glyphs in one
/// `TextRun`); standalone image paragraphs stay block-sized.
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
    let reveal = paragraph_reveal_state(snap, block);
    let hosts = chrome_hosts_for(&snap.tree, block.id);
    let flow = classify_paragraph(&block.inlines);
    let role = image_role(flow).unwrap_or(ImageRole::Inline);
    match flow {
        ParagraphFlow::TextOnly => {
            let layout = build_leaf_layout_revealed(
                block,
                &snap.source,
                &text_style,
                theme,
                base_weight,
                &reveal,
                &hosts,
            );
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
                let layout = build_leaf_layout_revealed(
                    block,
                    &snap.source,
                    &text_style,
                    theme,
                    base_weight,
                    &reveal,
                    &hosts,
                );
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
                            block,
                            &block.inlines,
                            start,
                            end,
                            block.source_range.clone(),
                            &snap.source,
                            &text_style,
                            theme,
                            base_weight,
                            font_size,
                            line_height,
                            editor.clone(),
                            true,
                            &reveal,
                            &hosts,
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
                let layout = build_leaf_layout_revealed(
                    block,
                    &snap.source,
                    &text_style,
                    theme,
                    base_weight,
                    &reveal,
                    &hosts,
                );
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

fn paragraph_reveal_state(snap: &RenderSnapshot, block: &Block) -> RevealState {
    // Table delimiters remain source structure, never editable glyphs. Even
    // an explicit markup-hint policy must not insert pipes into rich cells.
    if matches!(block.kind, BlockKind::TableCell) {
        RevealState::HIDDEN
    } else {
        snap.reveal_state()
    }
}

#[allow(clippy::too_many_arguments)]
fn push_text_child<H: WysiwygHost>(
    children: &mut Vec<AnyElement>,
    block: &Block,
    inlines: &[Inline],
    paint_start: usize,
    paint_end: usize,
    block_range: Range<usize>,
    source: &str,
    text_style: &TextStyle,
    theme: &EditorTheme,
    base_weight: FontWeight,
    font_size: f32,
    line_height: f32,
    editor: Entity<H>,
    hug_width: bool,
    reveal: &RevealState,
    hosts: &ChromeHosts,
) {
    if paint_start >= paint_end || paint_start >= inlines.len() {
        return;
    }
    let def_chrome = link_reference_def_chrome(source, block);
    let mut layout = build_leaf_layout_inlines(
        inlines,
        paint_start,
        paint_end,
        block_range,
        source,
        text_style,
        theme,
        base_weight,
        reveal,
        hosts,
        def_chrome.as_ref(),
    );
    if paint_end == inlines.len() {
        project_trailing_horizontal_whitespace(&mut layout, block, source, text_style);
    }
    if paint_start == 0 {
        project_leading_horizontal_whitespace(&mut layout, block, source, text_style);
        apply_structural_chrome(&mut layout, block, source, reveal, hosts, text_style, theme);
    }
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
    let editor_away = editor.clone();
    let editor_click = editor.clone();
    let alt_for_edit = alt.to_string();
    let url_for_edit = url.to_string();
    let secondary = snap.theme.secondary_text;
    let code_bg = snap.theme.code_bg;
    let inline_h = px(inline_image_height(font_size));
    let click_range = image_range.clone();
    // GPUI: `img(String)` is an unvalidated URI path. Markdown images must be
    // `PathBuf`s: a preflight-approved local/data cache copy or a populated
    // remote cache. Do not `img()` a missing cache path: GPUI
    // would cache the failure and never retry after the fetch writes the file.
    let resolved = resolve_image_source(snap.base_dir.as_deref(), url);
    let local_preflight_pending = matches!(
        &resolved,
        ResolvedImage::Local(path) if snap.local_image_pending.contains(path)
    );
    let data_preflight_pending =
        matches!(&resolved, ResolvedImage::Data) && snap.data_image_pending.contains(url.trim());
    let fallback_label = if alt.is_empty() {
        if local_preflight_pending || data_preflight_pending {
            "Loading image".to_string()
        } else {
            "Missing image".to_string()
        }
    } else {
        alt.to_string()
    };
    let ready_source = match resolved {
        ResolvedImage::File(path) if path.is_file() => Some(img(path)),
        ResolvedImage::Local(path) => snap
            .local_image_paths
            .get(&path)
            .filter(|approved| approved.is_file())
            .cloned()
            .map(img),
        ResolvedImage::Data => snap
            .data_image_paths
            .get(url.trim())
            .filter(|approved| approved.is_file())
            .cloned()
            .map(img),
        ResolvedImage::File(_) | ResolvedImage::Blocked => None,
    };
    let pixels_available = ready_source.is_some();
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
                .max_w_full()
                .min_w_0()
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
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |_, window, cx| {
                editor_click.update(cx, |host, cx| {
                    host.open_image_editor(
                        click_range.clone(),
                        &alt_for_edit,
                        &url_for_edit,
                        window,
                        cx,
                    );
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
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |_, window, cx| {
                editor_click.update(cx, |host, cx| {
                    host.open_image_editor(
                        click_range.clone(),
                        &alt_for_edit,
                        &url_for_edit,
                        window,
                        cx,
                    );
                });
            })
            .into_any_element()
    };
    let pixels = ImageBoundsElement {
        element: pixels,
        editor: editor.clone(),
        range: image_range.clone(),
    };
    let show_caption = (pixels_available && role == ImageRole::Block && !alt.is_empty()) || editing;
    let cap_font = 12.0;
    let cap_lh = snap.theme.line_height_for_font_size(cap_font);
    let caption_el = div()
        .id(("img-alt", image_range.start as u64))
        .cursor(CursorStyle::PointingHand)
        .when(editing, |el| {
            el.border_b_1().border_color(snap.theme.accent)
        })
        .when(editing, |el| {
            el.on_mouse_down_out(move |_, _, cx| {
                editor_away.update(cx, |host, cx| host.finish_widget(cx));
            })
        })
        .child(WidgetOverlay {
            editor: editor.clone(),
            prefix: String::new(),
            text: caption,
            editing,
            font_size: cap_font,
            line_height: cap_lh,
            theme: snap.theme.clone(),
            color: snap.theme.secondary_text,
            italic: true,
            monospace: false,
            hug_width: false,
            single_line: false,
            target: OverlayTarget::ImageAlt {
                range: image_range.clone(),
                stored: alt.to_string(),
            },
        });
    let caption_row = caption_el;
    match role {
        ImageRole::Inline => {
            let mut el = div()
                .flex_none()
                .flex()
                .flex_col()
                .relative()
                .child(painted_bounds_hit(editor.clone()))
                .child(pixels);
            if show_caption {
                el = el.child(caption_row);
            }
            el.into_any_element()
        }
        ImageRole::Block => div()
            .w_full()
            .min_w_0()
            .max_w(px(BLOCK_IMAGE_MAX_WIDTH))
            .my(px(4.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .relative()
            .child(painted_bounds_hit(editor))
            .child(pixels)
            .when(show_caption, |el| el.child(caption_row))
            .into_any_element(),
    }
}

/// Observe the clickable image's own layout without adding a sizing wrapper.
/// The caption/container can fit while an intrinsic-size child still overflows.
struct ImageBoundsElement<H: WysiwygHost> {
    element: AnyElement,
    editor: Entity<H>,
    range: Range<usize>,
}

impl<H: WysiwygHost> IntoElement for ImageBoundsElement<H> {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl<H: WysiwygHost> Element for ImageBoundsElement<H> {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.element.request_layout(window, cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.element.prepaint(window, cx);
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.editor.update(cx, |host, _| {
            host.report_image_bounds(self.range.clone(), bounds)
        });
        self.element.paint(window, cx);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn table_snapshot(source: &str) -> RenderSnapshot {
        use markrust_core::rich::{import_markdown, IdGen};
        RenderSnapshot {
            tree: import_markdown(source, &mut IdGen::default()),
            source: source.into(),
            theme: EditorTheme::dark(),
            base_dir: None,
            local_image_paths: HashMap::new(),
            local_image_pending: HashSet::new(),
            data_image_paths: HashMap::new(),
            data_image_pending: HashSet::new(),
            editing_code: None,
            editing_image: None,
            caret: 0,
            selected_range: 0..0,
        }
    }

    fn table_cell_layout(
        snapshot: &RenderSnapshot,
        cell: &Block,
    ) -> super::super::block_text::LeafLayout {
        let theme = &snapshot.theme;
        let style = base_text_style(theme, theme.font_size * 0.95, FontWeight::NORMAL);
        build_leaf_layout_revealed(
            cell,
            &snapshot.source,
            &style,
            theme,
            FontWeight::NORMAL,
            &paragraph_reveal_state(snapshot, cell),
            &chrome_hosts_for(&snapshot.tree, cell.id),
        )
    }

    #[test]
    fn rich_table_paints_only_cells_at_every_caret_and_selection() {
        for (source, expected) in [
            (
                "| Name | Description |\n| :--- | ---: |\n| **First** | [Label](https://example.test) |\n| Second | `code` |\n",
                vec!["Name", "Description", "First", "Label", "Second", "code"],
            ),
            ("Name|Value\n---|---\none|two\n", vec!["Name", "Value", "one", "two"]),
            ("> | A | B |\n> | --- | --- |\n> | C | D |\n", vec!["A", "B", "C", "D"]),
        ] {
            let mut snapshot = table_snapshot(source);
            let mut table = &snapshot.tree.blocks[0];
            while !matches!(table.kind, BlockKind::Table { .. }) {
                table = table.children.first().expect("nested table");
            }
            let table = table.clone();
            assert_eq!(table.children.len(), expected.len() / 2);
            for caret in 0..=source.len() {
                snapshot.caret = caret;
                snapshot.selected_range = caret..source.len();
                let layouts: Vec<_> = table
                    .children
                    .iter()
                    .flat_map(|row| {
                        row.children
                            .iter()
                            .map(|cell| table_cell_layout(&snapshot, cell))
                    })
                    .collect();
                assert_eq!(
                    layouts
                        .iter()
                        .map(|layout| layout.text.as_str())
                        .collect::<Vec<_>>(),
                    expected
                );
                assert!(layouts.iter().all(|layout| !layout.text.contains('|')));
                assert_eq!(
                    snapshot.source, source,
                    "rendering must not normalize the Markdown"
                );
            }
        }
    }

    #[test]
    fn rich_table_preserves_literal_pipes_and_maps_text_to_its_source_cell() {
        let source = "| a\\|b | **bold** |\n| --- | --- |\n| A&amp;B |  |\n";
        let snapshot = table_snapshot(source);
        let table = &snapshot.tree.blocks[0];
        let cells: Vec<_> = table
            .children
            .iter()
            .flat_map(|row| &row.children)
            .collect();
        let layouts: Vec<_> = cells
            .iter()
            .map(|cell| table_cell_layout(&snapshot, cell))
            .collect();
        assert_eq!(
            layouts
                .iter()
                .map(|layout| layout.text.as_str())
                .collect::<Vec<_>>(),
            ["a|b", "bold", "A&B", ""]
        );
        assert_eq!(
            layouts[0].source_for_visible(0),
            source.find("a\\|b").unwrap()
        );
        assert_eq!(
            layouts[1].source_for_visible(0),
            source.find("bold").unwrap()
        );
        assert_eq!(
            layouts[2].source_for_visible(1),
            source.find("&amp;").unwrap()
        );
        for (cell, layout) in cells.iter().zip(&layouts) {
            for (_, mapped) in layout
                .source_at
                .iter()
                .enumerate()
                .filter(|(offset, _)| layout.text.is_char_boundary(*offset))
            {
                assert!(*mapped >= cell.source_range.start && *mapped <= cell.source_range.end);
                assert!(layout.contains_source(*mapped));
            }
        }
    }

    #[test]
    fn rich_table_visible_cell_click_can_edit_text_and_undo_without_losing_structure() {
        use markrust_core::rich::{apply_rich_command, CaretState, RichCommand, RichEngine};
        use markrust_core::Document;
        let source = "| Name | Value |\n| --- | --- |\n| first |  |\n";
        let snapshot = table_snapshot(source);
        let row = &snapshot.tree.blocks[0].children[1];
        for (column, cell) in row.children.iter().enumerate() {
            let layout = table_cell_layout(&snapshot, cell);
            let mut document = Document::new(source);
            let mut engine = RichEngine::new();
            let mut caret = CaretState::collapsed(layout.source_for_visible(0));
            apply_rich_command(
                &mut document,
                &mut engine,
                &mut caret,
                RichCommand::InsertText("X".into()),
            )
            .unwrap();
            let edited = table_snapshot(&document.buffer.content());
            let table = &edited.tree.blocks[0];
            assert!(matches!(table.kind, BlockKind::Table { .. }));
            assert_eq!(table.children.len(), 2);
            assert!(table.children.iter().all(|row| row.children.len() == 2));
            assert!(document.buffer.content().contains("| --- | --- |"));
            assert!(
                table_cell_layout(&edited, &table.children[1].children[column])
                    .text
                    .contains('X')
            );
            assert!(document.undo());
            assert_eq!(document.buffer.content(), source);
        }
    }

    #[test]
    fn markup_hints_and_caret_never_change_live_text_projection() {
        let mut snapshot = RenderSnapshot {
            tree: RichTree::default(),
            source: "**bold**".into(),
            theme: EditorTheme::dark(),
            base_dir: None,
            local_image_paths: HashMap::new(),
            local_image_pending: HashSet::new(),
            data_image_paths: HashMap::new(),
            data_image_pending: HashSet::new(),
            editing_code: None,
            editing_image: None,
            caret: 3,
            selected_range: 3..3,
        };
        assert!(!snapshot.reveal_state().intersects(&(0..8)));

        snapshot.caret = 7;
        snapshot.selected_range = 2..7;
        assert!(!snapshot.reveal_state().intersects(&(0..8)));
        assert_eq!(snapshot.caret, 7, "the editable source caret is unchanged");
    }

    #[test]
    fn live_context_does_not_insert_heading_link_list_or_code_chrome() {
        use markrust_core::rich::{import_markdown, IdGen};
        for source in [
            "# Heading with **bold**\n",
            "A [label](https://example.test/very/long/destination) and **bold**.\n",
            "- **First item**\n- Second item\n",
            "```rust\nlet x = 1;\n```\n",
        ] {
            let tree = import_markdown(source, &mut IdGen::default());
            let mut snapshot = RenderSnapshot {
                tree,
                source: source.into(),
                theme: EditorTheme::dark(),
                base_dir: None,
                local_image_paths: HashMap::new(),
                local_image_pending: HashSet::new(),
                data_image_paths: HashMap::new(),
                data_image_pending: HashSet::new(),
                editing_code: None,
                editing_image: None,
                caret: 0,
                selected_range: 0..0,
            };
            let projection = |snapshot: &RenderSnapshot| {
                let mut block = &snapshot.tree.blocks[0];
                while let Some(child) = block.children.first() {
                    block = child;
                }
                let style = base_text_style(
                    &snapshot.theme,
                    snapshot.theme.font_size,
                    FontWeight::NORMAL,
                );
                let hosts = chrome_hosts_for(&snapshot.tree, block.id);
                let layout = build_leaf_layout_revealed(
                    block,
                    &snapshot.source,
                    &style,
                    &snapshot.theme,
                    FontWeight::NORMAL,
                    &snapshot.reveal_state(),
                    &hosts,
                );
                (
                    layout.text,
                    layout.source_at,
                    layout
                        .runs
                        .iter()
                        .map(|run| (run.len, run.font.weight))
                        .collect::<Vec<_>>(),
                )
            };
            let expected = projection(&snapshot);
            for caret in 0..=source.len() {
                snapshot.caret = caret;
                snapshot.selected_range = caret..caret;
                assert_eq!(
                    projection(&snapshot),
                    expected,
                    "caret {caret} changed live projection for {source:?}"
                );
            }
            snapshot.selected_range = 0..source.len();
            assert_eq!(
                projection(&snapshot),
                expected,
                "selection changed live projection"
            );
        }
    }

    #[test]
    fn newly_continued_empty_list_item_has_a_rendered_caret_home() {
        use markrust_core::rich::{import_markdown, IdGen};
        for source in ["- First\n- ", "1. First\n2. ", "- [x] First\n- [ ] "] {
            let tree = import_markdown(source, &mut IdGen::default());
            let list = &tree.blocks[0];
            let item = list.children.last().expect("continued list item");
            let blanks = prefix_blanks_for(item, &tree);
            let theme = EditorTheme::dark();
            let style = base_text_style(&theme, theme.font_size, FontWeight::NORMAL);
            let layout = if let Some(blank) = blanks.iter().find(|blank| blank.home == source.len())
            {
                build_prefix_blank_layout(source, blank, &RevealState::HIDDEN, &style, &theme)
            } else {
                // Empty task items retain a zero-text paragraph in Comrak;
                // ordinary empty items instead use the prefix-blank leaf.
                let child = item.children.first().expect("empty item text leaf");
                let hosts = chrome_hosts_for(&tree, child.id);
                build_leaf_layout_revealed(
                    child,
                    source,
                    &style,
                    &theme,
                    FontWeight::NORMAL,
                    &RevealState::HIDDEN,
                    &hosts,
                )
            };
            assert!(layout.text.is_empty());
            assert!(layout.contains_source(source.len()));
        }
    }

    #[test]
    fn exited_middle_list_gap_is_an_unbulleted_source_backed_caret_leaf() {
        use markrust_core::rich::{import_markdown, IdGen};
        for (source, home) in [
            ("- First\n\n- Following", 8),
            ("1. First\n\n1. Following", 9),
        ] {
            let tree = import_markdown(source, &mut IdGen::default());
            assert_eq!(tree.blocks.len(), 1, "CommonMark keeps one loose list");
            let gaps = list_blank_gaps(&tree.blocks[0], &tree);
            assert_eq!(gaps, vec![home..home + 1]);
            let layout = super::super::block_text::build_blank_gap_layout(gaps[0].clone());
            assert!(layout.text.is_empty());
            assert!(layout.contains_source(home));
            assert_eq!(layout.source_for_visible(0), home);
        }
    }

    #[test]
    fn list_draft_reserves_the_future_paragraph_separator_spacing() {
        for theme in [EditorTheme::light(), EditorTheme::dark()] {
            let reserve = paragraph_draft_spacing(&theme) - LIST_ROW_GAP;
            let row_height = theme.line_height_for_font_size(theme.font_size);
            assert_eq!(
                LIST_ROW_GAP + reserve,
                row_height + 2. * TOP_BLOCK_VERTICAL_PADDING,
                "draft baseline must already include the paragraph's separator and padding"
            );
            assert_eq!(
                LIST_ROW_GAP + reserve,
                paragraph_draft_spacing(&theme),
                "the following list must retain its baseline when the draft becomes text"
            );
        }
    }
}
