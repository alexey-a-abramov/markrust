// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Read-only semantic observations of the production UI, not a second editor.
//! Only fixture test builds include this module; real documents are not recorded.

use std::ops::Range;

use anyhow::{ensure, Result};
use gpui::{Bounds, Entity, HeadlessAppContext, Pixels, WindowHandle};
use markrust_core::rich::{Block, BlockKind, Inline};
use serde::{Deserialize, Serialize};

use crate::panels::Panel;
use crate::window::MarkRustWindow;
use crate::workspace::{EditorMode, Workspace};

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

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum InputOwner {
    Source,
    Wysiwyg,
    Widget(String),
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Observation {
    pub schema_version: u8,
    pub document_revision: u64,
    pub rich_render_revision: Option<u64>,
    pub source_render_revision: Option<u64>,
    pub active_tab_id: usize,
    pub input_owner: InputOwner,
    pub markup_hints: bool,
    pub raw_source: bool,
    pub sidebar: bool,
    pub outline: bool,
    pub overlay: Option<String>,
    pub palette: bool,
    pub source_pane: PaneState,
    pub rich_pane: PaneState,
    pub context: Vec<ContextNode>,
    pub context_revision: Option<u64>,
    pub painted_rich: Vec<PaintedLeaf>,
    pub painted_source: Vec<PaintedRow>,
    /// Actual selection-colored scene quads, clipped and in logical pixels.
    pub painted_selection: Vec<Rect>,
}

pub(crate) fn capture(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
) -> Result<Observation> {
    cx.update_window(window.into(), |_, window, cx| {
        let ws = workspace.read(cx);
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
        let input_owner = if source_focused {
            InputOwner::Source
        } else if rich_focused {
            rich.test_widget_kind()
                .map(|kind| InputOwner::Widget(kind.into()))
                .unwrap_or(InputOwner::Wysiwyg)
        } else {
            InputOwner::None
        };
        let (caret, selection) = if source_focused {
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
        Observation {
            schema_version: 1,
            document_revision: doc.revision(),
            rich_render_revision: rich.test_render_revision(),
            source_render_revision: source_geometry
                .as_ref()
                .and_then(|geometry| geometry.revision),
            active_tab_id: tab.id,
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
            },
            context: if rich.test_render_revision() == Some(doc.revision()) {
                context_nodes(&rich.engine_ref().tree().blocks, caret, &selection)
            } else {
                Vec::new()
            },
            context_revision: rich.test_render_revision(),
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
    if !observation.palette {
        ensure!(
            rich.focused || raw.focused,
            "no visible editing pane owns keyboard input"
        );
    }
    ensure!(
        observation.raw_source == (mode == "split"),
        "Source display policy disagrees with mode {mode}"
    );
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
        }
        let source = tab.editor.read(cx);
        if tab.mode != EditorMode::Wysiwyg {
            let view = tab.editor_view.read(cx);
            let viewport = Rect::from_bounds(view.horizontal_scroll_state().0);
            let geometry = view.painted_geometry();
            contract::validate_source_text_geometry(&geometry.rows, viewport, label)?;
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
        state.raw_source = true;
        assert!(validate(&state, "Café", "wysiwyg")
            .unwrap_err()
            .to_string()
            .contains("policy"));
    }
}
