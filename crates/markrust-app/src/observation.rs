// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Read-only semantic observations of the production UI, not a second editor.
//! Only fixture test builds include this module; real documents are not recorded.

use std::ops::Range;

use anyhow::{ensure, Context, Result};
use gpui::{Bounds, Entity, HeadlessAppContext, Pixels, WindowHandle};
use markrust_core::rich::{Block, BlockKind, Inline};
use serde::{Deserialize, Serialize};

use crate::panels::Panel;
use crate::window::MarkRustWindow;
use crate::workspace::{EditingPane, EditorMode, Workspace};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl From<Bounds<Pixels>> for Rect {
    fn from(bounds: Bounds<Pixels>) -> Self {
        Self {
            x: bounds.left().into(),
            y: bounds.top().into(),
            width: bounds.size.width.into(),
            height: bounds.size.height.into(),
        }
    }
}

impl From<crate::visual_contract::Rect> for Rect {
    fn from(rect: crate::visual_contract::Rect) -> Self {
        Self {
            x: rect.left,
            y: rect.top,
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum InputOwner {
    Source,
    Wysiwyg,
    Widget(String),
    Find,
    OpenPath,
    Palette,
    #[default]
    None,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PaneState {
    pub visible: bool,
    pub focused: bool,
    pub selection: Range<usize>,
    pub reversed: bool,
    pub caret: usize,
    pub viewport: Option<Rect>,
    pub caret_bounds: Option<Rect>,
    /// Blink phase; caret geometry remains available during off frames for IME.
    #[serde(default)]
    pub caret_blink_on: bool,
    /// Passive peer context, never the pane's real input selection.
    #[serde(default)]
    pub shadow: Option<ShadowState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShadowState {
    pub revision: u64,
    pub selection: Range<usize>,
    pub reversed: bool,
    pub caret: usize,
    /// Independently observed native scene quads, clipped to the peer viewport.
    pub cursor_quads: Vec<Rect>,
    pub selection_quads: Vec<Rect>,
}

fn painted_shadow(
    shadow: Option<&markrust_editor::shadow::ShadowSelection>,
    viewport: Option<Bounds<Pixels>>,
    window: &gpui::Window,
) -> Option<ShadowState> {
    let shadow = shadow?;
    let viewport = viewport?;
    let viewport = crate::visual_contract::Rect::from_bounds(viewport);
    let cursor_quads = crate::visual_contract::painted_selection_rectangles(
        window,
        markrust_editor::shadow::cursor_color().opacity(0.72),
        viewport,
    )
    .into_iter()
    .map(Into::into)
    .collect();
    let selection_quads = crate::visual_contract::painted_selection_rectangles(
        window,
        markrust_editor::shadow::selection_color(),
        viewport,
    )
    .into_iter()
    .map(Into::into)
    .collect();
    Some(ShadowState {
        revision: shadow.revision,
        selection: shadow.range.clone(),
        reversed: shadow.reversed,
        caret: shadow.caret(),
        cursor_quads,
        selection_quads,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextNode {
    pub kind: String,
    pub source_range: Range<usize>,
    pub caret_inside: bool,
    pub selection_intersects: bool,
    pub children: Vec<ContextNode>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaintedRow {
    pub bounds: Rect,
    /// Source-backed caret stops from the actual native text shaping pass.
    pub stops: Vec<(usize, f32)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaintedLeaf {
    pub text: String,
    pub source_range: Range<usize>,
    pub rows: Vec<PaintedRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TableShape {
    pub source_range: Range<usize>,
    /// Full model dimensions, unlike the selection-filtered context tree.
    pub columns_per_row: Vec<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageInspectorState {
    pub location: String,
    pub alt: String,
    pub preview_ready: bool,
    pub focused_field: Option<String>,
    pub bounds: Option<Rect>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FindState {
    pub focused: bool,
    pub query: String,
    pub query_selection: Range<usize>,
    pub marked_range: Option<Range<usize>>,
    pub matches: Vec<Range<usize>>,
    pub current: Option<Range<usize>>,
    pub pane: InputOwner,
    pub tab_id: usize,
    pub revision: u64,
    pub native_input_registered: bool,
    pub input_bounds: Option<Rect>,
    pub bounds: Option<Rect>,
    pub current_quads: Vec<Rect>,
    pub other_quads: Vec<Rect>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenPathState {
    pub query: String,
    pub query_selection: Range<usize>,
    pub error: Option<String>,
    pub focused: bool,
    pub native_input_registered: bool,
    pub input_bounds: Option<Rect>,
    pub bounds: Option<Rect>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Observation {
    pub schema_version: u8,
    pub document_revision: u64,
    pub rich_render_revision: Option<u64>,
    pub source_render_revision: Option<u64>,
    pub active_tab_id: usize,
    #[serde(default)]
    pub tab_count: usize,
    #[serde(default)]
    pub tab_strip_bounds: Option<Rect>,
    #[serde(default)]
    pub active_tab_bounds: Option<Rect>,
    #[serde(default)]
    pub editing_context_hint: Option<String>,
    #[serde(default)]
    pub table_toolbar_bounds: Option<Rect>,
    #[serde(default)]
    pub markup_hint_bounds: Option<Rect>,
    #[serde(default)]
    pub markup_hint_label: Option<String>,
    /// Test-fixture drafts only; the production application does not record input.
    #[serde(default)]
    pub widget_draft: Option<String>,
    #[serde(default)]
    pub widget_selection: Option<Range<usize>>,
    #[serde(default)]
    pub widget_caret_bounds: Option<Rect>,
    #[serde(default)]
    pub link_editor_bounds: Option<Rect>,
    #[serde(default)]
    pub link_anchor_bounds: Option<Rect>,
    #[serde(default)]
    pub image_inspector: Option<ImageInspectorState>,
    #[serde(default)]
    pub find: Option<FindState>,
    #[serde(default)]
    pub open_path: Option<OpenPathState>,
    pub input_owner: InputOwner,
    pub markup_hints: bool,
    pub raw_source: bool,
    pub sidebar: bool,
    pub outline: bool,
    pub overlay: Option<String>,
    pub palette: bool,
    #[serde(default)]
    pub palette_focused: bool,
    #[serde(default)]
    pub palette_native_input: bool,
    #[serde(default)]
    pub palette_query: String,
    #[serde(default)]
    pub palette_query_selection: Range<usize>,
    #[serde(default)]
    pub palette_query_reversed: bool,
    #[serde(default)]
    pub palette_marked_range: Option<Range<usize>>,
    #[serde(default)]
    pub palette_input_bounds: Option<Rect>,
    #[serde(default)]
    pub palette_results: Vec<String>,
    #[serde(default)]
    pub palette_selected_result: usize,
    #[serde(default)]
    pub palette_bounds: Option<Rect>,
    pub source_pane: PaneState,
    pub rich_pane: PaneState,
    pub context: Vec<ContextNode>,
    #[serde(default)]
    pub table_shapes: Vec<TableShape>,
    pub context_revision: Option<u64>,
    pub painted_rich: Vec<PaintedLeaf>,
    pub painted_source: Vec<PaintedRow>,
    /// Actual selection-colored scene quads, clipped and in logical pixels.
    pub painted_selection: Vec<Rect>,
    #[serde(default)]
    pub painted_carets: Vec<Rect>,
}

pub(crate) fn capture(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
) -> Result<Observation> {
    cx.update_window(window.into(), |root, window, cx| {
        let ws = workspace.read(cx);
        let root = root.downcast::<MarkRustWindow>().ok();
        let tab_geometry = root
            .as_ref()
            .map(|root| root.read(cx).test_tab_strip_state(ws.active_tab));
        let palette = root
            .as_ref()
            .map(|root| root.read(cx).test_palette_state(window, cx));
        let find = root
            .as_ref()
            .and_then(|root| root.read(cx).test_find_state(window, cx));
        let open_path = root
            .as_ref()
            .and_then(|root| root.read(cx).test_open_path_state(window, cx))
            .map(|state| OpenPathState {
                query: state.query,
                query_selection: state.query_selection,
                error: state.error,
                focused: state.focused,
                native_input_registered: state.native_input_registered,
                input_bounds: state.input_bounds.map(Into::into),
                bounds: state.bar_bounds.map(Into::into),
            });
        let tab = ws.active_tab().expect("fixture has an active tab");
        let rich = tab.rich_view.read(cx);
        let source = tab.editor.read(cx);
        let doc = tab.document.read(cx);
        let source_focused = source.focus_handle.is_focused(window);
        let rich_focused = rich.is_focused(window);
        let source_visible = tab.mode != EditorMode::Wysiwyg;
        let rich_visible = tab.mode != EditorMode::Source;
        let source_viewport =
            source_visible.then(|| tab.editor_view.read(cx).horizontal_scroll_state().0.into());
        let source_geometry = source_visible.then(|| tab.editor_view.read(cx).painted_geometry());
        let find = find.map(|state| {
            let viewport = match state.pane {
                EditingPane::Source => Some(tab.editor_view.read(cx).horizontal_scroll_state().0),
                EditingPane::Wysiwyg => rich.painted_viewport_bounds(),
            };
            let painted = |current| {
                viewport.map_or_else(Vec::new, |viewport| {
                    crate::visual_contract::painted_selection_rectangles(
                        window,
                        markrust_editor::search::match_color(current),
                        crate::visual_contract::Rect::from_bounds(viewport),
                    )
                    .into_iter()
                    .map(Into::into)
                    .collect()
                })
            };
            FindState {
                focused: state.focused,
                query: state.query,
                query_selection: state.query_selection,
                marked_range: state.marked_range,
                matches: state.matches,
                current: state.current,
                pane: match state.pane {
                    EditingPane::Source => InputOwner::Source,
                    EditingPane::Wysiwyg => InputOwner::Wysiwyg,
                },
                tab_id: state.tab_id,
                revision: state.revision,
                native_input_registered: state.native_input_registered,
                input_bounds: state.input_bounds.map(Into::into),
                bounds: state.bar_bounds.map(Into::into),
                current_quads: painted(true),
                other_quads: painted(false),
            }
        });
        let image_inspector = rich_visible
            .then(|| rich.test_image_editor_state(cx))
            .flatten()
            .map(|(location, alt, preview_ready)| ImageInspectorState {
                location,
                alt,
                preview_ready,
                focused_field: rich.test_image_focused_field(window, cx).map(str::to_owned),
                bounds: rich.painted_image_editor_bounds().map(Into::into),
            });
        let input_owner = if open_path.as_ref().is_some_and(|state| state.focused) {
            InputOwner::OpenPath
        } else if find.as_ref().is_some_and(|state| state.focused) {
            InputOwner::Find
        } else if palette.as_ref().is_some_and(|state| state.focused) {
            InputOwner::Palette
        } else if let Some(field) = image_inspector
            .as_ref()
            .and_then(|image| image.focused_field.as_ref())
        {
            InputOwner::Widget(format!("image-{field}"))
        } else if source_focused {
            InputOwner::Source
        } else if rich_focused {
            rich.test_widget_kind()
                .map(|kind| InputOwner::Widget(kind.into()))
                .unwrap_or(InputOwner::Wysiwyg)
        } else {
            InputOwner::None
        };
        let (caret, selection) =
            if source_focused || (!rich_focused && tab.editing_pane == EditingPane::Source) {
                (source.cursor_offset(), source.selected_range.clone())
            } else {
                (rich.cursor_offset(), rich.selected_range.clone())
            };
        let scale = window.scale_factor();
        let selection_color = rich.theme.selection;
        let painted_selection = window
            .painted_quads()
            .into_iter()
            .filter(|quad| quad.background == selection_color.into())
            .filter_map(|quad| {
                let left = quad.bounds.left().0.max(quad.content_mask.bounds.left().0) / scale;
                let right = quad
                    .bounds
                    .right()
                    .0
                    .min(quad.content_mask.bounds.right().0)
                    / scale;
                let top = quad.bounds.top().0.max(quad.content_mask.bounds.top().0) / scale;
                let bottom = quad
                    .bounds
                    .bottom()
                    .0
                    .min(quad.content_mask.bounds.bottom().0)
                    / scale;
                (right > left && bottom > top).then_some(Rect {
                    x: left,
                    y: top,
                    width: right - left,
                    height: bottom - top,
                })
            })
            .collect();
        let painted_carets = window
            .painted_quads()
            .into_iter()
            .filter(|quad| quad.background == rich.theme.caret.into())
            .filter_map(|quad| {
                let left = quad.bounds.left().0.max(quad.content_mask.bounds.left().0) / scale;
                let right = quad
                    .bounds
                    .right()
                    .0
                    .min(quad.content_mask.bounds.right().0)
                    / scale;
                let top = quad.bounds.top().0.max(quad.content_mask.bounds.top().0) / scale;
                let bottom = quad
                    .bounds
                    .bottom()
                    .0
                    .min(quad.content_mask.bounds.bottom().0)
                    / scale;
                (right > left && right - left <= 3. && bottom - top >= 10.).then_some(Rect {
                    x: left,
                    y: top,
                    width: right - left,
                    height: bottom - top,
                })
            })
            .collect();
        Observation {
            schema_version: 1,
            document_revision: doc.revision(),
            rich_render_revision: rich.test_render_revision(),
            source_render_revision: source_geometry
                .as_ref()
                .and_then(|geometry| geometry.revision),
            active_tab_id: tab.id,
            tab_count: ws.tabs.len(),
            tab_strip_bounds: tab_geometry.as_ref().map(|(strip, _)| (*strip).into()),
            active_tab_bounds: tab_geometry.and_then(|(_, active)| active.map(Into::into)),
            editing_context_hint: rich.editing_context_hint(),
            table_toolbar_bounds: rich_visible
                .then(|| rich.painted_table_toolbar_bounds().map(Into::into))
                .flatten(),
            markup_hint_bounds: rich_visible
                .then(|| rich.painted_markup_hint().map(|(bounds, _)| bounds.into()))
                .flatten(),
            markup_hint_label: rich_visible
                .then(|| rich.painted_markup_hint().map(|(_, label)| label))
                .flatten(),
            widget_draft: rich.test_widget_draft(),
            image_inspector,
            find,
            open_path,
            widget_selection: rich.test_widget_selection(),
            widget_caret_bounds: rich.painted_widget_caret_bounds().map(Into::into),
            link_editor_bounds: rich.painted_link_editor_bounds().map(Into::into),
            link_anchor_bounds: rich.painted_link_anchor_bounds().map(Into::into),
            input_owner,
            markup_hints: rich.markup_hints_enabled(),
            raw_source: source.raw_source(),
            sidebar: ws.sidebar_open,
            outline: ws.outline_open,
            overlay: ws.panel_overlay.map(|panel| match panel {
                Panel::Sidebar => "sidebar".into(),
                Panel::Outline => "outline".into(),
            }),
            palette: ws.palette_open,
            palette_focused: palette
                .as_ref()
                .is_some_and(|state| state.open && state.focused),
            palette_native_input: palette
                .as_ref()
                .is_some_and(|state| state.native_input_registered),
            palette_query: palette
                .as_ref()
                .map(|state| state.query.clone())
                .unwrap_or_default(),
            palette_query_selection: palette
                .as_ref()
                .map(|state| state.query_selection.clone())
                .unwrap_or_default(),
            palette_query_reversed: palette
                .as_ref()
                .is_some_and(|state| state.selection_reversed),
            palette_marked_range: palette
                .as_ref()
                .and_then(|state| state.marked_range.clone()),
            palette_input_bounds: palette
                .as_ref()
                .and_then(|state| state.input_bounds.map(Into::into)),
            palette_results: palette
                .as_ref()
                .map(|state| state.results.clone())
                .unwrap_or_default(),
            palette_selected_result: palette
                .as_ref()
                .map(|state| state.selected_result)
                .unwrap_or_default(),
            palette_bounds: palette.and_then(|state| state.bounds.map(Into::into)),
            source_pane: PaneState {
                visible: source_visible,
                focused: source_focused,
                selection: source.selected_range.clone(),
                reversed: source.selection_reversed,
                caret: source.cursor_offset(),
                viewport: source_viewport,
                caret_bounds: source_visible
                    .then(|| source.painted_caret_bounds().map(Into::into))
                    .flatten(),
                caret_blink_on: source.cursor_visible,
                shadow: source_visible
                    .then(|| {
                        painted_shadow(
                            source.shadow_selection(),
                            Some(tab.editor_view.read(cx).horizontal_scroll_state().0),
                            window,
                        )
                    })
                    .flatten(),
            },
            rich_pane: PaneState {
                visible: rich_visible,
                focused: rich_focused,
                selection: rich.selected_range.clone(),
                reversed: rich.selection_reversed,
                caret: rich.cursor_offset(),
                viewport: rich_visible
                    .then(|| rich.painted_viewport_bounds().map(Into::into))
                    .flatten(),
                caret_bounds: rich_visible
                    .then(|| rich.test_viewport_state().2.map(Into::into))
                    .flatten(),
                caret_blink_on: rich.test_cursor_blink_on(),
                shadow: rich_visible
                    .then(|| {
                        painted_shadow(
                            rich.shadow_selection(),
                            rich.painted_viewport_bounds(),
                            window,
                        )
                    })
                    .flatten(),
            },
            context: if rich.test_render_revision() == Some(doc.revision()) {
                context_nodes(&rich.engine_ref().tree().blocks, caret, &selection)
            } else {
                Vec::new()
            },
            context_revision: rich.test_render_revision(),
            table_shapes: if rich_visible {
                table_shapes(&rich.engine_ref().tree().blocks)
            } else {
                Vec::new()
            },
            painted_rich: if rich_visible {
                rich.painted_geometry()
                    .into_iter()
                    .map(|leaf| PaintedLeaf {
                        text: leaf.text,
                        source_range: leaf.source_range,
                        rows: leaf
                            .lines
                            .into_iter()
                            .map(|row| PaintedRow {
                                bounds: Rect {
                                    x: row.left,
                                    y: row.top,
                                    width: row.right - row.left,
                                    height: row.height,
                                },
                                stops: row
                                    .stops
                                    .into_iter()
                                    .map(|stop| (stop.source, stop.x))
                                    .collect(),
                            })
                            .collect(),
                    })
                    .collect()
            } else {
                Vec::new()
            },
            painted_source: source_geometry
                .map(|geometry| {
                    geometry
                        .rows
                        .into_iter()
                        .map(|row| PaintedRow {
                            bounds: row.bounds.into(),
                            stops: row.caret_stops,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            painted_selection,
            painted_carets,
        }
    })
}

fn context_nodes(blocks: &[Block], caret: usize, selection: &Range<usize>) -> Vec<ContextNode> {
    blocks
        .iter()
        .filter_map(|block| {
            let caret_inside = block.source_range.start <= caret && caret <= block.source_range.end;
            let selection_intersects = selection.start < block.source_range.end
                && selection.end > block.source_range.start
                && !selection.is_empty();
            (caret_inside || selection_intersects).then(|| ContextNode {
                kind: crate::usecases::block_kind_label(&block.kind),
                source_range: block.source_range.clone(),
                caret_inside,
                selection_intersects,
                children: context_nodes(&block.children, caret, selection),
            })
        })
        .collect()
}

fn table_shapes(blocks: &[Block]) -> Vec<TableShape> {
    let mut shapes = Vec::new();
    for block in blocks {
        if matches!(block.kind, BlockKind::Table { .. }) {
            shapes.push(TableShape {
                source_range: block.source_range.clone(),
                columns_per_row: block
                    .children
                    .iter()
                    .map(|row| row.children.len())
                    .collect(),
            });
        }
        shapes.extend(table_shapes(&block.children));
    }
    shapes
}

/// Cheap contracts for every generated frame, independent of scenario-specific intent.
pub fn validate(observation: &Observation, source: &str, mode: &str) -> Result<()> {
    let rich = &observation.rich_pane;
    let raw = &observation.source_pane;
    ensure!(
        !rich.focused || rich.visible,
        "input focus is on a hidden WYSIWYG pane"
    );
    ensure!(
        !raw.focused || raw.visible,
        "input focus is on a hidden Source pane"
    );
    ensure!(
        !(raw.focused && rich.focused),
        "two panes claim keyboard focus"
    );
    for (pane, owner) in [(raw, rich), (rich, raw)] {
        if let Some(shadow) = &pane.shadow {
            ensure!(
                mode == "split" && !pane.focused && pane.visible && owner.focused,
                "passive cursor has no active Split peer"
            );
            ensure!(
                shadow.selection == owner.selection
                    && shadow.reversed == owner.reversed
                    && shadow.caret == owner.caret,
                "passive cursor disagrees with the actual input owner"
            );
            ensure!(
                matches!(
                    observation.input_owner,
                    InputOwner::Source | InputOwner::Wysiwyg
                ),
                "passive cursor painted while a non-body input owns focus"
            );
        }
    }
    if let Some(path) = &observation.open_path {
        ensure!(
            path.focused
                && path.native_input_registered
                && observation.input_owner == InputOwner::OpenPath
                && !rich.focused
                && !raw.focused
                && rich.shadow.is_none()
                && raw.shadow.is_none(),
            "Open Location modal does not exclusively own its native input"
        );
        let range = &path.query_selection;
        ensure!(
            range.start <= range.end
                && range.end <= path.query.len()
                && path.query.is_char_boundary(range.start)
                && path.query.is_char_boundary(range.end),
            "Open Location query splits UTF-8 or exceeds its input"
        );
        let panel = path
            .bounds
            .as_ref()
            .context("Open Location modal was not painted")?;
        let input = path
            .input_bounds
            .as_ref()
            .context("Open Location field was not painted")?;
        ensure!(
            panel.width > 0.
                && panel.height > 0.
                && input.width > 0.
                && input.height > 0.
                && input.x >= panel.x - 1.
                && input.y >= panel.y - 1.
                && input.x + input.width <= panel.x + panel.width + 1.
                && input.y + input.height <= panel.y + panel.height + 1.,
            "Open Location field escaped its visible modal"
        );
    } else if let Some(find) = observation.find.as_ref().filter(|find| find.focused) {
        ensure!(
            observation.input_owner == InputOwner::Find
                && find.native_input_registered
                && !rich.focused
                && !raw.focused
                && rich.shadow.is_none()
                && raw.shadow.is_none(),
            "Find query does not exclusively own native text input"
        );
    } else if !observation.palette {
        if let Some(field) = observation
            .image_inspector
            .as_ref()
            .and_then(|image| image.focused_field.as_ref())
        {
            ensure!(
                observation.input_owner == InputOwner::Widget(format!("image-{field}"))
                    && !rich.focused
                    && !raw.focused
                    && rich.shadow.is_none()
                    && raw.shadow.is_none(),
                "image inspector does not exclusively own its native input field"
            );
        } else {
            ensure!(
                rich.focused || raw.focused,
                "no visible editing pane owns keyboard input"
            );
        }
    } else {
        ensure!(
            observation.input_owner == InputOwner::Palette
                && observation.palette_focused
                && observation.palette_native_input
                && !rich.focused
                && !raw.focused,
            "command palette does not exclusively own native text input"
        );
        let range = &observation.palette_query_selection;
        let query = &observation.palette_query;
        ensure!(
            range.start <= range.end
                && range.end <= query.len()
                && query.is_char_boundary(range.start)
                && query.is_char_boundary(range.end),
            "palette query selection is not a valid UTF-8 range"
        );
    }
    if let Some(find) = &observation.find {
        ensure!(
            find.tab_id == observation.active_tab_id
                && find.revision == observation.document_revision,
            "Find results belong to a stale tab or document revision"
        );
        let query_range = &find.query_selection;
        ensure!(
            query_range.start <= query_range.end
                && query_range.end <= find.query.len()
                && find.query.is_char_boundary(query_range.start)
                && find.query.is_char_boundary(query_range.end),
            "Find query selection is not a valid UTF-8 range"
        );
        for range in &find.matches {
            ensure!(
                range.start < range.end
                    && range.end <= source.len()
                    && source.is_char_boundary(range.start)
                    && source.is_char_boundary(range.end),
                "Find result splits UTF-8 or exceeds the current document: {range:?}"
            );
        }
        ensure!(
            find.current
                .as_ref()
                .is_none_or(|current| find.matches.contains(current)),
            "Find current result is not among its current matches"
        );
        ensure!(
            !find.query.is_empty() || find.matches.is_empty(),
            "an empty Find query highlighted document positions"
        );
        let pane = match &find.pane {
            InputOwner::Source => raw,
            InputOwner::Wysiwyg => rich,
            _ => anyhow::bail!("Find results have no document-pane owner"),
        };
        ensure!(pane.visible, "Find results target a hidden pane");
        let bounds = find
            .bounds
            .as_ref()
            .context("Find bar has no painted bounds")?;
        let input = find
            .input_bounds
            .as_ref()
            .context("Find query has no painted bounds")?;
        ensure!(
            bounds.width > 0.
                && bounds.height > 0.
                && input.width > 0.
                && input.height > 0.
                && input.x >= bounds.x - 1.
                && input.y >= bounds.y - 1.
                && input.x + input.width <= bounds.x + bounds.width + 1.
                && input.y + input.height <= bounds.y + bounds.height + 1.,
            "Find query field escaped its visible bar"
        );
    }
    if let Some(image) = &observation.image_inspector {
        let bounds = image
            .bounds
            .as_ref()
            .context("image inspector has no painted bounds")?;
        let viewport = rich
            .viewport
            .as_ref()
            .context("image inspector has no rich viewport")?;
        ensure!(
            bounds.width > 0.
                && bounds.height > 0.
                && bounds.x >= viewport.x
                && bounds.y >= viewport.y
                && bounds.x + bounds.width <= viewport.x + viewport.width + 1.
                && bounds.y + bounds.height <= viewport.y + viewport.height + 1.,
            "image inspector escaped its visible viewport"
        );
    }
    if raw.visible {
        ensure!(
            observation.raw_source,
            "visible Source pane must display literal Markdown in mode {mode}"
        );
    }
    if rich.visible {
        ensure!(
            observation.rich_render_revision == Some(observation.document_revision),
            "painted rich revision {:?} is stale; document revision {}",
            observation.rich_render_revision,
            observation.document_revision
        );
    }
    if raw.visible {
        ensure!(
            observation.source_render_revision == Some(observation.document_revision),
            "painted source revision {:?} is stale; document revision {}",
            observation.source_render_revision,
            observation.document_revision
        );
    }
    if matches!(&observation.input_owner, InputOwner::Widget(kind) if kind == "link-destination") {
        let bounds = observation
            .link_editor_bounds
            .as_ref()
            .context("active link editor was not painted")?;
        let caret = observation
            .widget_caret_bounds
            .as_ref()
            .context("active link editor lost its caret")?;
        ensure!(
            caret.x >= bounds.x - 2.
                && caret.x + caret.width <= bounds.x + bounds.width + 2.
                && caret.y >= bounds.y - 2.
                && caret.y + caret.height <= bounds.y + bounds.height + 2.,
            "link destination caret escaped its visible editor"
        );
        let viewport = rich
            .viewport
            .as_ref()
            .context("active link editor has no rich viewport")?;
        ensure!(
            bounds.x >= viewport.x - 2.
                && bounds.x + bounds.width <= viewport.x + viewport.width + 2.
                && bounds.y >= viewport.y - 2.
                && bounds.y + bounds.height <= viewport.y + viewport.height + 2.,
            "link destination editor escaped the rich viewport"
        );
        ensure!(
            observation.widget_draft.is_some(),
            "active link editor has no observed draft"
        );
        if let Some(anchor) = observation.link_anchor_bounds.as_ref() {
            ensure!(
                bounds.x + bounds.width <= anchor.x - 4.
                    || bounds.x >= anchor.x + anchor.width + 4.
                    || bounds.y + bounds.height <= anchor.y - 4.
                    || bounds.y >= anchor.y + anchor.height + 4.,
                "link destination editor occluded the edited label"
            );
        }
    }
    for pane in [raw, rich].into_iter().filter(|pane| pane.focused) {
        ensure!(
            pane.selection.start <= pane.selection.end && pane.selection.end <= source.len(),
            "focused pane selection {:?} exceeds {} source bytes",
            pane.selection,
            source.len()
        );
        ensure!(
            source.is_char_boundary(pane.selection.start)
                && source.is_char_boundary(pane.selection.end),
            "selection splits a UTF-8 character: {:?}",
            pane.selection
        );
        ensure!(
            pane.caret
                == if pane.reversed {
                    pane.selection.start
                } else {
                    pane.selection.end
                },
            "caret does not match the active selection edge"
        );
    }
    Ok(())
}

/// Join the semantic selection to independently observed native scene quads.
pub(crate) fn validate_paint(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    label: &str,
) -> Result<()> {
    use crate::visual_contract::{self as contract, Rect};
    cx.update_window(window.into(), |_, window, cx| {
        let ws = workspace.read(cx);
        let tab = ws.active_tab().unwrap();
        let rich = tab.rich_view.read(cx);
        if tab.mode != EditorMode::Source {
            let bounds = rich.painted_viewport_bounds().ok_or_else(|| {
                anyhow::anyhow!("{label}: visible rich pane has no painted viewport")
            })?;
            ensure!(
                f32::from(bounds.size.width) > 0. && f32::from(bounds.size.height) > 0.,
                "{label}: visible rich pane has an empty viewport"
            );
            let leaves = rich.painted_geometry();
            if let Some(toolbar) = rich.painted_table_toolbar_bounds() {
                ensure!(
                    toolbar.left() >= bounds.left()
                        && toolbar.right() <= bounds.right()
                        && toolbar.top() >= bounds.top()
                        && toolbar.bottom() <= bounds.bottom(),
                    "{label}: floating table controls escaped their viewport"
                );
                if let Some(caret) = rich.test_viewport_state().2 {
                    let margin = gpui::px(8.);
                    ensure!(
                        toolbar.right() <= caret.left() - margin
                            || toolbar.left() >= caret.right() + margin
                            || toolbar.bottom() <= caret.top() - margin
                            || toolbar.top() >= caret.bottom() + margin,
                        "{label}: floating table controls occluded the insertion caret"
                    );
                }
            }
            if text_at_caret(&rich.engine_ref().tree().blocks, rich.cursor_offset()) {
                ensure!(
                    !leaves.is_empty(),
                    "{label}: text at the editing context produced no native glyph leaves"
                );
            }
            if !leaves.is_empty() {
                contract::validate_text_geometry(&leaves, label)?;
            }
            if rich.is_focused(window) && rich.test_widget_kind().is_none() {
                let viewport = Rect::from_bounds(bounds);
                let actual =
                    contract::painted_selection_rectangles(window, rich.theme.selection, viewport);
                contract::validate_selection_geometry(
                    &leaves,
                    &rich.selected_range,
                    viewport,
                    &actual,
                    label,
                )?;
            }
            if let Some(shadow) = rich.shadow_selection() {
                let viewport = Rect::from_bounds(bounds);
                let actual = contract::painted_selection_rectangles(
                    window,
                    markrust_editor::shadow::selection_color(),
                    viewport,
                );
                contract::validate_selection_geometry(
                    &leaves,
                    &shadow.range,
                    viewport,
                    &actual,
                    label,
                )?;
            }
            if let Some(search) = rich.search_highlights() {
                validate_search_paint(
                    SearchGlyphs::Rich(&leaves),
                    search,
                    Rect::from_bounds(bounds),
                    window,
                    label,
                )?;
            }
        }
        let source = tab.editor.read(cx);
        if tab.mode != EditorMode::Wysiwyg {
            let view = tab.editor_view.read(cx);
            let viewport = Rect::from_bounds(view.horizontal_scroll_state().0);
            let geometry = view.painted_geometry();
            contract::validate_source_text_geometry(&geometry.rows, viewport, label)?;
            if let Some(search) = source.search_highlights(cx) {
                validate_search_paint(
                    SearchGlyphs::Source(&geometry.rows),
                    search,
                    viewport,
                    window,
                    label,
                )?;
            }
            if let Some(shadow) = source.shadow_selection() {
                let actual = contract::painted_selection_rectangles(
                    window,
                    markrust_editor::shadow::selection_color(),
                    viewport,
                );
                contract::validate_source_selection_geometry(
                    &geometry.rows,
                    &shadow.range,
                    viewport,
                    &actual,
                    label,
                )?;
            }
            if source.focus_handle.is_focused(window) {
                let actual = contract::painted_selection_rectangles(
                    window,
                    source.theme.selection,
                    viewport,
                );
                contract::validate_source_selection_geometry(
                    &geometry.rows,
                    &source.selected_range,
                    viewport,
                    &actual,
                    label,
                )?;
                contract::validate_selection_layering(
                    window,
                    source.theme.selection,
                    source.theme.code_block_bg,
                    viewport,
                    label,
                )?;
            }
        }
        Ok(())
    })?
}

enum SearchGlyphs<'a> {
    Source(&'a [markrust_editor::element::SourcePaintRow]),
    Rich(&'a [markrust_editor::wysiwyg::PaintedLeafGeometry]),
}

/// Expected spans come from the native text shaper, not the Find painter's
/// intermediate highlight rectangles. Compare both color groups as multisets.
fn validate_search_paint(
    glyphs: SearchGlyphs<'_>,
    search: &markrust_editor::search::SearchHighlights,
    viewport: crate::visual_contract::Rect,
    window: &gpui::Window,
    label: &str,
) -> Result<()> {
    use crate::visual_contract::{self as contract, Rect};
    for current in [false, true] {
        let mut expected = Vec::new();
        for (index, selection) in search.ranges.iter().enumerate() {
            if (search.active == Some(index)) != current {
                continue;
            }
            match &glyphs {
                SearchGlyphs::Source(rows) => {
                    for row in *rows {
                        if selection.start >= row.source_range.end
                            || selection.end <= row.source_range.start
                        {
                            continue;
                        }
                        let first = row
                            .caret_stops
                            .first()
                            .context("Find row has no native stops")?;
                        let last = row
                            .caret_stops
                            .last()
                            .context("Find row has no native stops")?;
                        let x_at = |source| {
                            row.caret_stops
                                .iter()
                                .rev()
                                .find(|(byte, _)| *byte <= source)
                                .unwrap_or(first)
                                .1
                        };
                        let newline = last.0 < row.source_range.end
                            && selection.start <= last.0
                            && selection.end > last.0;
                        let rectangle = Rect {
                            left: x_at(selection.start.max(row.source_range.start)),
                            right: x_at(selection.end.min(last.0)) + if newline { 2. } else { 0. },
                            ..Rect::from_bounds(row.bounds)
                        };
                        if let Some(rectangle) = rectangle.intersection(viewport) {
                            expected.push(rectangle);
                        }
                    }
                }
                SearchGlyphs::Rich(leaves) => {
                    for leaf in *leaves {
                        if selection.start >= leaf.source_range.end
                            || selection.end <= leaf.source_range.start
                        {
                            continue;
                        }
                        let projected = |source| {
                            leaf.lines
                                .iter()
                                .flat_map(|row| &row.stops)
                                .filter(|stop| stop.source <= source)
                                .map(|stop| stop.visible)
                                .max()
                                .unwrap_or(0)
                        };
                        let selected = projected(selection.start)..projected(selection.end);
                        for row in &leaf.lines {
                            let start = selected.start.max(row.visible_start);
                            let end = selected.end.min(row.visible_end);
                            if start >= end {
                                continue;
                            }
                            let x_at = |visible| {
                                row.stops
                                    .iter()
                                    .find(|stop| stop.visible == visible)
                                    .map(|stop| stop.x)
                                    .context("Find boundary has no native glyph stop")
                            };
                            let rectangle = Rect {
                                left: x_at(start)?,
                                right: x_at(end)?,
                                top: row.top,
                                bottom: row.top + row.height,
                            };
                            if let Some(rectangle) = rectangle.intersection(viewport) {
                                expected.push(rectangle);
                            }
                        }
                    }
                }
            }
        }
        let actual = contract::painted_selection_rectangles(
            window,
            markrust_editor::search::match_color(current),
            viewport,
        );
        validate_search_rectangles(&expected, &actual, label)?;
    }
    Ok(())
}

fn validate_search_rectangles(
    expected: &[crate::visual_contract::Rect],
    actual: &[crate::visual_contract::Rect],
    label: &str,
) -> Result<()> {
    let mut consumed = vec![false; expected.len()];
    for actual in actual {
        let position = expected
            .iter()
            .enumerate()
            .position(|(index, expected)| {
                !consumed[index]
                    && (expected.left - actual.left).abs() <= 1.
                    && (expected.right - actual.right).abs() <= 1.
                    && (expected.top - actual.top).abs() <= 1.
                    && (expected.bottom - actual.bottom).abs() <= 1.
            })
            .with_context(|| {
                format!(
                    "{label}: Find highlighted unrelated glyphs: {actual:?}; expected {expected:?}"
                )
            })?;
        consumed[position] = true;
    }
    ensure!(consumed.iter().all(|consumed| *consumed),
        "{label}: Find matches have missing native highlight quads: expected {expected:?}, actual {actual:?}");
    Ok(())
}

fn text_at_caret(blocks: &[Block], caret: usize) -> bool {
    blocks.iter().any(|block| {
        block.source_range.start <= caret
            && caret <= block.source_range.end
            && (matches!(block.kind, BlockKind::CodeBlock { .. })
                || block.inlines.iter().any(|inline| match inline {
                    Inline::Run { text, .. } => !text.is_empty(),
                    Inline::Math { literal, .. } => !literal.is_empty(),
                    Inline::WikiLink { label, .. } => !label.is_empty(),
                    Inline::Emoji { glyph, .. } => !glyph.is_empty(),
                    _ => false,
                })
                || text_at_caret(&block.children, caret))
    })
}

/// A caret or hint-policy change must not reflow an otherwise stationary view.
/// Colors, caret and selection quads are deliberately excluded from this check.
pub(crate) fn validate_stationary_rich_layout(
    before: &Observation,
    after: &Observation,
) -> Result<()> {
    validate_stationary_rich_placement(before, after)?;
    for (a, b) in before.painted_rich.iter().zip(&after.painted_rich) {
        ensure!(
            a.source_range == b.source_range,
            "caret/hint change altered the WYSIWYG source projection"
        );
        for (a, b) in a.rows.iter().zip(&b.rows) {
            ensure!(
                a.stops == b.stops,
                "caret/hint change moved native glyph caret stops"
            );
        }
    }
    Ok(())
}

/// Link URL edits may change source offsets without changing visible text.
pub(crate) fn validate_stationary_rich_placement(
    before: &Observation,
    after: &Observation,
) -> Result<()> {
    ensure!(
        before.painted_rich.len() == after.painted_rich.len(),
        "caret/hint change added or removed painted text leaves"
    );
    for (a, b) in before.painted_rich.iter().zip(&after.painted_rich) {
        ensure!(
            a.text == b.text,
            "caret/hint change altered the WYSIWYG text projection"
        );
        ensure!(
            a.rows.len() == b.rows.len(),
            "caret/hint change rewrapped WYSIWYG text"
        );
        for (a, b) in a.rows.iter().zip(&b.rows) {
            ensure!(
                (a.bounds.x - b.bounds.x).abs() < 0.5
                    && (a.bounds.y - b.bounds.y).abs() < 0.5
                    && (a.bounds.width - b.bounds.width).abs() < 0.5
                    && (a.bounds.height - b.bounds.height).abs() < 0.5,
                "caret/hint change moved text from {:?} to {:?}",
                a.bounds,
                b.bounds
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use markrust_core::{rich::RichEngine, Document};

    #[test]
    fn context_descends_to_selected_table_cell() {
        let source = "| A | B |\n| - | - |\n| Café | value |\n";
        let mut engine = RichEngine::new();
        engine.sync(&Document::new(source));
        let start = source.find("Café").unwrap();
        let context = context_nodes(&engine.tree().blocks, start + 5, &(start..start + 5));
        assert_eq!(context[0].kind, "table");
        let row = &context[0].children[0];
        assert!(row
            .children
            .iter()
            .any(|cell| cell.kind == "table-cell" && cell.selection_intersects));
    }

    #[test]
    fn standalone_text_constructs_require_native_glyphs() {
        for source in ["$x+y$", "[[target|label]]", ":smile:"] {
            let mut engine = RichEngine::new();
            engine.sync(&Document::new(source));
            assert!(text_at_caret(&engine.tree().blocks, 1), "{source}");
        }
    }

    #[test]
    fn rejects_stationary_hint_projection_reflow() {
        let mut state = valid_state();
        state.painted_rich.push(PaintedLeaf {
            text: "word".into(),
            source_range: 2..6,
            rows: vec![PaintedRow {
                bounds: Rect {
                    x: 20.,
                    y: 30.,
                    width: 40.,
                    height: 24.,
                },
                stops: vec![(2, 20.), (6, 60.)],
            }],
        });
        let mut after = state.clone();
        after.markup_hints = !after.markup_hints;
        validate_stationary_rich_layout(&state, &after).unwrap();
        after.painted_rich[0].rows[0].bounds.y += 24.;
        assert!(validate_stationary_rich_layout(&state, &after).is_err());
        after = state.clone();
        after.painted_rich[0].text = "**word**".into();
        assert!(validate_stationary_rich_layout(&state, &after).is_err());
    }

    #[test]
    fn find_paint_oracle_rejects_missing_extra_and_shifted_highlights() {
        let rectangle = crate::visual_contract::Rect {
            left: 20.,
            top: 30.,
            right: 80.,
            bottom: 54.,
        };
        validate_search_rectangles(&[rectangle], &[rectangle], "find").unwrap();
        assert!(validate_search_rectangles(&[rectangle], &[], "find").is_err());
        assert!(validate_search_rectangles(&[rectangle], &[rectangle, rectangle], "find").is_err());
        let shifted = crate::visual_contract::Rect {
            left: 24.,
            right: 84.,
            ..rectangle
        };
        assert!(validate_search_rectangles(&[rectangle], &[shifted], "find").is_err());
    }

    #[test]
    fn find_rejects_stale_ranges_and_nonexclusive_query_focus() {
        let mut state = valid_state();
        state.rich_pane.focused = false;
        state.input_owner = InputOwner::Find;
        state.find = Some(FindState {
            focused: true,
            query: "café".into(),
            query_selection: 5..5,
            marked_range: None,
            matches: std::iter::once(0..5).collect(),
            current: Some(0..5),
            pane: InputOwner::Wysiwyg,
            tab_id: state.active_tab_id,
            revision: state.document_revision,
            native_input_registered: true,
            bounds: Some(Rect {
                x: 0.,
                y: 0.,
                width: 400.,
                height: 40.,
            }),
            input_bounds: Some(Rect {
                x: 8.,
                y: 4.,
                width: 250.,
                height: 32.,
            }),
            current_quads: Vec::new(),
            other_quads: Vec::new(),
        });
        validate(&state, "Café", "wysiwyg").unwrap();
        state.rich_pane.focused = true;
        assert!(validate(&state, "Café", "wysiwyg").is_err());
        state.rich_pane.focused = false;
        state.find.as_mut().unwrap().revision -= 1;
        assert!(validate(&state, "Café", "wysiwyg").is_err());
        state.find.as_mut().unwrap().revision += 1;
        state.find.as_mut().unwrap().matches[0] = 0..4;
        assert!(validate(&state, "Café", "wysiwyg").is_err());
    }

    #[test]
    fn open_location_rejects_lost_focus_clipped_field_and_split_utf8() {
        let mut state = valid_state();
        state.rich_pane.focused = false;
        state.input_owner = InputOwner::OpenPath;
        state.open_path = Some(OpenPathState {
            query: "/tmp/café".into(),
            query_selection: 10..10,
            error: Some("Path does not exist.".into()),
            focused: true,
            native_input_registered: true,
            bounds: Some(Rect {
                x: 100.,
                y: 100.,
                width: 300.,
                height: 180.,
            }),
            input_bounds: Some(Rect {
                x: 116.,
                y: 136.,
                width: 268.,
                height: 34.,
            }),
        });
        validate(&state, "Café", "wysiwyg").unwrap();
        state.open_path.as_mut().unwrap().query_selection = 9..9;
        assert!(validate(&state, "Café", "wysiwyg").is_err());
        state.open_path.as_mut().unwrap().query_selection = 10..10;
        state
            .open_path
            .as_mut()
            .unwrap()
            .input_bounds
            .as_mut()
            .unwrap()
            .x = 450.;
        assert!(validate(&state, "Café", "wysiwyg").is_err());
        state
            .open_path
            .as_mut()
            .unwrap()
            .input_bounds
            .as_mut()
            .unwrap()
            .x = 116.;
        state.rich_pane.focused = true;
        assert!(validate(&state, "Café", "wysiwyg").is_err());
    }

    #[test]
    fn rejects_link_draft_with_missing_or_clipped_caret() {
        let mut state = valid_state();
        state.input_owner = InputOwner::Widget("link-destination".into());
        state.widget_draft = Some("https://example.com".into());
        state.rich_pane.viewport = Some(Rect {
            x: 10.,
            y: 20.,
            width: 500.,
            height: 800.,
        });
        state.link_editor_bounds = Some(Rect {
            x: 30.,
            y: 50.,
            width: 200.,
            height: 40.,
        });
        state.widget_caret_bounds = Some(Rect {
            x: 90.,
            y: 58.,
            width: 2.,
            height: 24.,
        });
        validate(&state, "Café", "wysiwyg").unwrap();
        state.widget_caret_bounds = None;
        assert!(validate(&state, "Café", "wysiwyg").is_err());
        state.widget_caret_bounds = Some(Rect {
            x: 400.,
            y: 58.,
            width: 2.,
            height: 24.,
        });
        assert!(validate(&state, "Café", "wysiwyg").is_err());
    }

    fn valid_state() -> Observation {
        Observation {
            schema_version: 1,
            document_revision: 7,
            rich_render_revision: Some(7),
            rich_pane: PaneState {
                visible: true,
                focused: true,
                selection: 0..5,
                caret: 5,
                ..Default::default()
            },
            input_owner: InputOwner::Wysiwyg,
            ..Default::default()
        }
    }

    #[test]
    fn rejects_focus_on_hidden_editor() {
        let mut state = valid_state();
        assert!(validate(&state, "Café", "wysiwyg").is_ok());
        state.rich_pane.visible = false;
        assert!(validate(&state, "Café", "wysiwyg")
            .unwrap_err()
            .to_string()
            .contains("hidden"));
    }

    #[test]
    fn rejects_stale_render_and_invalid_selection_edges() {
        let mut state = valid_state();
        state.rich_render_revision = Some(6);
        assert!(validate(&state, "Café", "wysiwyg")
            .unwrap_err()
            .to_string()
            .contains("stale"));
        state.rich_render_revision = Some(7);
        state.rich_pane.selection = 0..4;
        assert!(validate(&state, "Café", "wysiwyg")
            .unwrap_err()
            .to_string()
            .contains("UTF-8"));
        state.rich_pane.selection = 0..5;
        state.rich_pane.reversed = true;
        assert!(validate(&state, "Café", "wysiwyg").is_err());
        state.rich_pane.caret = 0;
        assert!(validate(&state, "Café", "wysiwyg").is_ok());
    }

    #[test]
    fn rejects_missing_focus_and_wrong_mode_policy() {
        let mut state = valid_state();
        state.rich_pane.focused = false;
        assert!(validate(&state, "Café", "wysiwyg").is_err());
        state.rich_pane.focused = true;
        state.rich_pane.visible = false;
        state.rich_pane.focused = false;
        state.source_pane.visible = true;
        state.source_pane.focused = true;
        state.input_owner = InputOwner::Source;
        state.raw_source = false;
        assert!(validate(&state, "Café", "source")
            .unwrap_err()
            .to_string()
            .contains("literal Markdown"));
    }
}
