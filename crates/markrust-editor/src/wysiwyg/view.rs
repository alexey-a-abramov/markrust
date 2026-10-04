// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The WYSIWYG editor view: a virtualized list of rendered blocks kept in
//! sync with the document through [`RichEngine`].

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use unicode_segmentation::UnicodeSegmentation;

use gpui::{
    canvas, div, fill, list, point, prelude::*, px, size, App, AvailableSpace, Bounds,
    ClipboardItem, Context, CursorStyle, ElementInputHandler, Entity, EntityInputHandler,
    FocusHandle, Focusable, ListAlignment, ListState, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Render, Role, ScrollHandle, SharedString, Subscription, Task, TextStyle,
    UTF16Selection, Window,
};
use markrust_core::rich::{
    apply_rich_command, caret_for_click_below_content, place_caret_for_click_below,
    table_select_all_range, Bias, BlockType, CaretState, MarkSet, NodeId, RichCommand, RichEngine,
    RichOutcome,
};
use markrust_core::Document;

use super::block_text::{hit_test_leaf, LeafLayout, OverlayTarget, WidgetOverlay, WysiwygHost};
use super::blocks::{render_top_block, RenderSnapshot};
use super::image::{
    cache_path_for_url, collect_data_image_urls, collect_local_image_paths,
    collect_remote_image_urls, default_image_cache_dir, fetch_remote_image,
    materialize_safe_data_image, materialize_safe_local_image,
};
use super::image_editor::{
    decode_image_draft, encode_image_draft, image_editor_placement, markdown_image_text,
    ImageEditTarget, ImageEditor,
};
use super::ime::{ImeLeafHit, ImeOriginState, VisualLine};
use crate::headless::{
    clamp_selection_to_content, next_boundary, next_word_end, prev_word_start, previous_boundary,
    CaretMove, EditorCommand, EditorOutcome,
};
use crate::theme::EditorTheme;
use crate::wrap::{wrap_selection, WrapKind};

/// Stable discriminator for a focused rich-text widget draft.
///
/// This intentionally contains no GPUI entities, parser node IDs, or IME
/// objects. The app layer can mirror it into its private recovery format
/// without coupling the editor crate to that format or to serde.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetDraftKind {
    /// Display-only IME text in the document body. It has no safe Markdown
    /// insertion target after restart and must be recovered as a scratch tab.
    BodyComposition,
    CodeInfo,
    ImageAlt,
    ImageProperties,
    FrontmatterField(FrontmatterField),
    FrontmatterYaml,
    LinkDestination,
}

/// The fixed frontmatter fields exposed by the WYSIWYG frontmatter panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontmatterField {
    Title,
    Description,
    Tags,
}

impl FrontmatterField {
    fn from_key(key: &str) -> Option<Self> {
        match key {
            "title" => Some(Self::Title),
            "description" => Some(Self::Description),
            "tags" => Some(Self::Tags),
            _ => None,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Description => "description",
            Self::Tags => "tags",
        }
    }
}

/// Portable state for a widget edit that has not yet been committed into the
/// document buffer.
///
/// `original_source` is deliberately retained in full. Reattachment is only
/// allowed when the current document exactly matches it; a range that happens
/// to exist after a disk or merge change must never retarget a link, image, or
/// frontmatter field silently. The app owns on-disk serialization and bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WidgetDraftSnapshot {
    pub kind: WidgetDraftKind,
    pub original_source: String,
    pub source_range: Range<usize>,
    pub draft: String,
    /// Ordered selection in `draft`, represented separately from its direction.
    pub selection: Range<usize>,
    pub selection_reversed: bool,
}

impl WidgetDraftSnapshot {
    /// The draft text that must remain recoverable even when its source anchor
    /// can no longer be safely reattached.
    pub fn raw_draft_text(&self) -> &str {
        &self.draft
    }

    fn has_valid_source_anchor(&self) -> bool {
        self.source_range.start <= self.source_range.end
            && self
                .original_source
                .get(self.source_range.clone())
                .is_some()
    }

    /// Restore malformed persisted offsets to complete grapheme boundaries.
    /// The text is retained; only a stale caret/selection endpoint is repaired.
    fn repaired_selection(&self) -> Range<usize> {
        let start = clamp_grapheme_boundary(&self.draft, self.selection.start);
        let end = clamp_grapheme_boundary(&self.draft, self.selection.end);
        start.min(end)..start.max(end)
    }
}

/// Why a stored widget draft was not attached to the live rich editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetDraftRestoreError {
    InvalidSnapshot,
    DocumentChanged,
    TargetUnavailable,
}

impl std::fmt::Display for WidgetDraftRestoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidSnapshot => "the widget recovery snapshot is invalid",
            Self::DocumentChanged => "the document changed since the widget draft was captured",
            Self::TargetUnavailable => "the original widget target is no longer available",
        })
    }
}

impl std::error::Error for WidgetDraftRestoreError {}

#[derive(Debug, Clone)]
struct WidgetRecoveryAnchor {
    kind: WidgetDraftKind,
    original_source: String,
    source_range: Range<usize>,
}

#[derive(Debug, Clone, Default)]
enum WidgetEdit {
    #[default]
    Idle,
    CodeInfo {
        id: NodeId,
        draft: String,
        caret: usize,
    },
    ImageAlt {
        range: Range<usize>,
        draft: String,
        caret: usize,
    },
    Frontmatter {
        key: &'static str,
        draft: String,
        caret: usize,
    },
    FrontmatterYaml {
        draft: String,
        caret: usize,
    },
    LinkDestination {
        range: Range<usize>,
        revision: u64,
        draft: String,
        caret: usize,
    },
}

impl WidgetEdit {
    fn recovery_kind(&self) -> Option<WidgetDraftKind> {
        match self {
            Self::Idle => None,
            Self::CodeInfo { .. } => Some(WidgetDraftKind::CodeInfo),
            Self::ImageAlt { .. } => Some(WidgetDraftKind::ImageAlt),
            Self::Frontmatter { key, .. } => {
                FrontmatterField::from_key(key).map(WidgetDraftKind::FrontmatterField)
            }
            Self::FrontmatterYaml { .. } => Some(WidgetDraftKind::FrontmatterYaml),
            Self::LinkDestination { .. } => Some(WidgetDraftKind::LinkDestination),
        }
    }

    fn caret(&self) -> usize {
        match self {
            Self::Idle => 0,
            Self::CodeInfo { caret, .. }
            | Self::ImageAlt { caret, .. }
            | Self::Frontmatter { caret, .. }
            | Self::LinkDestination { caret, .. }
            | Self::FrontmatterYaml { caret, .. } => *caret,
        }
    }

    fn draft_and_caret(&self) -> Option<(&str, usize)> {
        match self {
            Self::Idle => None,
            Self::CodeInfo { draft, caret, .. }
            | Self::ImageAlt { draft, caret, .. }
            | Self::Frontmatter { draft, caret, .. }
            | Self::LinkDestination { draft, caret, .. }
            | Self::FrontmatterYaml { draft, caret } => Some((draft, *caret)),
        }
    }

    fn draft_caret_mut(&mut self) -> Option<(&mut String, &mut usize)> {
        match self {
            Self::Idle => None,
            Self::CodeInfo { draft, caret, .. }
            | Self::ImageAlt { draft, caret, .. }
            | Self::Frontmatter { draft, caret, .. }
            | Self::LinkDestination { draft, caret, .. }
            | Self::FrontmatterYaml { draft, caret } => Some((draft, caret)),
        }
    }

    fn allows_newline(&self) -> bool {
        matches!(self, Self::FrontmatterYaml { .. })
    }

    fn set_caret(&mut self, offset: usize) {
        if let Some((draft, caret)) = self.draft_caret_mut() {
            *caret = clamp_grapheme_boundary(draft, offset);
        }
    }

    fn matches_overlay(&self, target: &OverlayTarget) -> bool {
        match (self, target) {
            (Self::CodeInfo { id, .. }, OverlayTarget::CodeInfo(tid)) => id == tid,
            (Self::ImageAlt { range, .. }, OverlayTarget::ImageAlt { range: r, .. }) => range == r,
            (Self::Frontmatter { key, .. }, OverlayTarget::Frontmatter { key: k, .. }) => key == k,
            (Self::FrontmatterYaml { .. }, OverlayTarget::FrontmatterYaml { .. }) => true,
            (Self::LinkDestination { range, .. }, OverlayTarget::LinkDestination { range: r }) => {
                range == r
            }
            _ => false,
        }
    }
}

/// Keep widget carets and selections on visible-character boundaries.
///
/// An offset produced by an IME or a stale drag can otherwise land between a
/// base character and a combining mark (or inside a ZWJ emoji), where editing
/// would visibly split one glyph.
fn clamp_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    if offset == text.len() {
        return offset;
    }
    text.grapheme_indices(true)
        .map(|(start, _)| start)
        .take_while(|start| *start <= offset)
        .last()
        .unwrap_or(0)
}

/// Commands describe anchor and caret offsets, but rendering requires an
/// ordered range plus a separate direction. Keep both endpoints on complete
/// visible characters even when a programmatic selection supplies stale bytes.
fn body_selection_for_offsets(source: &str, start: usize, end: usize) -> CaretState {
    let mut caret = CaretState {
        range: start.min(end)..start.max(end),
        reversed: start > end,
    };
    clamp_selection_to_content(source, &mut caret.range, &mut caret.reversed);
    caret
}

fn editing_context_hint_for_tree(
    tree: &markrust_core::rich::RichTree,
    caret: usize,
) -> Option<String> {
    // CommonMark can absorb the blank between same-marker lists into one
    // container, but the editor exposes it as a plain paragraph draft.
    if markrust_core::rich::blank_caret_gap_at(tree, caret).is_some() {
        return None;
    }
    editing_context_hint_at(&tree.blocks, caret)
}

fn editing_context_hint_at(blocks: &[markrust_core::rich::Block], caret: usize) -> Option<String> {
    use markrust_core::rich::{BlockKind, Inline};
    let block = blocks
        .iter()
        .find(|block| block.source_range.start <= caret && caret < block.source_range.end)
        .or_else(|| {
            blocks
                .iter()
                .rev()
                .find(|block| block.source_range.end == caret)
        })?;
    if let Some(hint) = editing_context_hint_at(&block.children, caret) {
        return Some(hint);
    }
    for inline in &block.inlines {
        if let Inline::Run {
            marks,
            link,
            source_range,
            ..
        } = inline
        {
            if caret >= source_range.start && caret <= source_range.end {
                if link.is_some() {
                    return Some("Link · [text](url)".into());
                }
                if marks.contains(MarkSet::CODE) {
                    return Some("Inline code · `text`".into());
                }
                if marks.contains(MarkSet::BOLD) {
                    return Some("Bold · **text**".into());
                }
                if marks.contains(MarkSet::ITALIC) {
                    return Some("Italic · *text*".into());
                }
                if marks.contains(MarkSet::STRIKE) {
                    return Some("Strikethrough · ~~text~~".into());
                }
            }
        }
    }
    match &block.kind {
        BlockKind::Heading { level, .. } => {
            Some(format!("Heading {level} · {}", "#".repeat(*level as usize)))
        }
        BlockKind::CodeBlock { .. } => Some("Code block · ```".into()),
        BlockKind::BulletList { .. } => Some("Bulleted list · -".into()),
        BlockKind::OrderedList { .. } => Some("Numbered list · 1.".into()),
        BlockKind::ListItem { task: Some(_) } => Some("Task list · - [ ]".into()),
        BlockKind::BlockQuote | BlockKind::Alert { .. } => Some("Blockquote · >".into()),
        BlockKind::Table { .. } => Some("Table · | cell |".into()),
        _ => None,
    }
}

/// Inline Markdown links have a source destination but no visible URL glyphs.
/// Return the exact destination range and decoded value, never a reference
/// definition, image URL, or HTML attribute masquerading as a Markdown link.
fn link_destination_at(
    blocks: &[markrust_core::rich::Block],
    source: &str,
    caret: usize,
) -> Option<(Range<usize>, String)> {
    use markrust_core::rich::{
        expand_link_and_html_chrome, grow_mark_delimiters, markdown_link_chrome, Inline,
    };
    for block in blocks {
        if let Some(found) = link_destination_at(&block.children, source, caret) {
            return Some(found);
        }
        for inline in &block.inlines {
            let Inline::Run {
                link: Some(link),
                source_range,
                ..
            } = inline
            else {
                continue;
            };
            if link.autolink || link.angle {
                continue;
            }
            let mut label = source_range.clone();
            if link.group != 0 {
                for other in &block.inlines {
                    if let Inline::Run {
                        link: Some(attrs),
                        source_range,
                        ..
                    }
                    | Inline::Image {
                        link: Some(attrs),
                        source_range,
                        ..
                    }
                    | Inline::Emoji {
                        link: Some(attrs),
                        source_range,
                        ..
                    } = other
                    {
                        if attrs.group == link.group {
                            label.start = label.start.min(source_range.start);
                            label.end = label.end.max(source_range.end);
                        }
                    }
                }
            }
            let outer = expand_link_and_html_chrome(
                source,
                grow_mark_delimiters(source, label),
                Some(link),
                block.source_range.start,
                block.source_range.end,
            );
            if caret < outer.start || caret >= outer.end {
                continue;
            }
            let Some(chrome) = markdown_link_chrome(source, outer) else {
                continue;
            };
            if source.as_bytes().get(chrome.dest.start) == Some(&b'(') {
                return Some((chrome.dest, link.url.clone()));
            }
        }
    }
    None
}

fn code_info_id_at_range(
    blocks: &[markrust_core::rich::Block],
    range: &Range<usize>,
) -> Option<NodeId> {
    for block in blocks {
        if &block.source_range == range
            && matches!(block.kind, markrust_core::rich::BlockKind::CodeBlock { .. })
        {
            return Some(block.id);
        }
        if let Some(id) = code_info_id_at_range(&block.children, range) {
            return Some(id);
        }
    }
    None
}

fn image_exists_at_range(blocks: &[markrust_core::rich::Block], range: &Range<usize>) -> bool {
    use markrust_core::rich::Inline;

    blocks.iter().any(|block| {
        block.inlines.iter().any(
            |inline| matches!(inline, Inline::Image { source_range, .. } if source_range == range),
        ) || image_exists_at_range(&block.children, range)
    })
}

fn frontmatter_range(source: &str) -> Option<Range<usize>> {
    markrust_core::parse_frontmatter(source).map(|info| info.start_byte..info.end_byte)
}

fn composition_is_pending(body_marked: bool, body_preedit: bool, widget_preedit: bool) -> bool {
    body_marked || body_preedit || widget_preedit
}

/// Tab / Shift-Tab while a language chip, image caption, or frontmatter
/// overlay is focused commits that overlay. The document body must not run
/// `IndentList` / `OutdentList`.
fn widget_owns_tab(edit: &WidgetEdit) -> bool {
    !matches!(edit, WidgetEdit::Idle)
}

/// Left/Right/word/Home/End/document/page (and Delete / word-delete / line-delete)
/// stay inside the overlay; they must not move or mutate the document body.
/// Cmd+A / Undo / Redo use the same gate.
fn widget_owns_caret(edit: &WidgetEdit) -> bool {
    !matches!(edit, WidgetEdit::Idle)
}

/// Cmd/Ctrl+B/I/E/K while a widget overlay is focused must not commit and
/// toggle marks on the document body.
fn widget_owns_wrap(edit: &WidgetEdit) -> bool {
    !matches!(edit, WidgetEdit::Idle)
}

/// Captions are Markdown text; frontmatter values are YAML and must never
/// receive Markdown delimiters from formatting shortcuts.
fn widget_wraps_draft(edit: &WidgetEdit) -> bool {
    matches!(edit, WidgetEdit::ImageAlt { .. })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WidgetWrapResult {
    /// No overlay; wrap the body.
    NotFocused,
    /// Overlay consumed the shortcut without changing the draft (language chip).
    Ignored,
    /// Draft was wrapped (or unwrapped).
    Applied,
}

fn apply_wrap_to_widget(edit: &mut WidgetEdit, kind: WrapKind) -> WidgetWrapResult {
    if matches!(edit, WidgetEdit::Idle) {
        return WidgetWrapResult::NotFocused;
    }
    if !widget_wraps_draft(edit) {
        return WidgetWrapResult::Ignored;
    }
    // Overlay click/drag places an inner caret (body-quality hit-test). Wrap
    // still targets the whole draft. Empty Cmd+B becomes `****` with the caret
    // between the marks so the next insert is `**x**`, not `****x`. IME origin
    // is the inner `|` / caret rect, not the overlay's trailing edge.
    if let Some((draft, caret)) = edit.draft_caret_mut() {
        let wrapped = wrap_selection(draft, 0..draft.len(), kind);
        *draft = wrapped.text;
        *caret = wrapped.selection.end.min(draft.len());
    }
    WidgetWrapResult::Applied
}

fn widget_range(caret: usize, anchor: usize, len: usize) -> Range<usize> {
    let a = caret.min(anchor).min(len);
    let b = caret.max(anchor).min(len);
    a..b
}

/// Overlay Copy: a non-empty inner selection copies that slice; an empty
/// caret copies the whole draft (language chip / caption / frontmatter).
fn overlay_copy_text(draft: &str, sel: Range<usize>) -> Option<String> {
    let text = if sel.start < sel.end {
        let end = sel.end.min(draft.len());
        let start = sel.start.min(end);
        draft.get(start..end).filter(|s| !s.is_empty())?
    } else if draft.is_empty() {
        return None;
    } else {
        draft
    };
    Some(text.to_string())
}

fn insert_into_widget(edit: &mut WidgetEdit, anchor: &mut usize, text: &str) {
    let Some((draft, caret)) = edit.draft_caret_mut() else {
        return;
    };
    *caret = clamp_grapheme_boundary(draft, *caret);
    *anchor = clamp_grapheme_boundary(draft, *anchor);
    let range = widget_range(*caret, *anchor, draft.len());
    let start = range.start;
    draft.replace_range(range, text);
    *caret = start + text.len();
    *anchor = *caret;
}

fn delete_before_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) {
    let Some((draft, caret)) = edit.draft_caret_mut() else {
        return;
    };
    *caret = clamp_grapheme_boundary(draft, *caret);
    *anchor = clamp_grapheme_boundary(draft, *anchor);
    let range = widget_range(*caret, *anchor, draft.len());
    if range.start != range.end {
        let start = range.start;
        draft.replace_range(range, "");
        *caret = start;
        *anchor = start;
        return;
    }
    let at = range.start;
    if at == 0 {
        return;
    }
    let start = draft[..at]
        .grapheme_indices(true)
        .next_back()
        .map(|(start, _)| start)
        .unwrap_or(0);
    draft.replace_range(start..at, "");
    *caret = start;
    *anchor = start;
}

fn delete_after_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) {
    let Some((draft, caret)) = edit.draft_caret_mut() else {
        return;
    };
    *caret = clamp_grapheme_boundary(draft, *caret);
    *anchor = clamp_grapheme_boundary(draft, *anchor);
    let range = widget_range(*caret, *anchor, draft.len());
    if range.start != range.end {
        let start = range.start;
        draft.replace_range(range, "");
        *caret = start;
        *anchor = start;
        return;
    }
    let at = range.start;
    if at >= draft.len() {
        return;
    }
    let next = draft[at..]
        .graphemes(true)
        .next()
        .map(str::len)
        .unwrap_or_default();
    draft.replace_range(at..at + next, "");
}

fn delete_toward_in_widget(
    edit: &mut WidgetEdit,
    anchor: &mut usize,
    target_of: impl Fn(&str, usize) -> usize,
) {
    let Some((draft, caret)) = edit.draft_caret_mut() else {
        return;
    };
    *caret = clamp_grapheme_boundary(draft, *caret);
    *anchor = clamp_grapheme_boundary(draft, *anchor);
    let range = widget_range(*caret, *anchor, draft.len());
    let (start, end) = if range.start != range.end {
        (range.start, range.end)
    } else {
        let at = range.start;
        let target = clamp_grapheme_boundary(draft, target_of(draft, at));
        if target <= at {
            (target, at)
        } else {
            (at, target)
        }
    };
    if start == end {
        return;
    }
    draft.replace_range(start..end, "");
    *caret = start;
    *anchor = start;
}

fn delete_word_before_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) {
    delete_toward_in_widget(edit, anchor, prev_word_start);
}

fn delete_word_after_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) {
    delete_toward_in_widget(edit, anchor, next_word_end);
}

fn delete_to_line_start_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) {
    delete_toward_in_widget(edit, anchor, overlay_line_start);
}

fn delete_to_line_end_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) {
    delete_toward_in_widget(edit, anchor, overlay_line_end);
}

fn overlay_line_start(draft: &str, at: usize) -> usize {
    let at = clamp_grapheme_boundary(draft, at);
    draft[..at].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

fn overlay_line_end(draft: &str, at: usize) -> usize {
    let at = clamp_grapheme_boundary(draft, at);
    draft[at..]
        .find('\n')
        .map(|i| at + i)
        .unwrap_or(draft.len())
}

/// Cmd+A selects the overlay draft. The document body is not touched.
fn select_all_in_widget(edit: &mut WidgetEdit, anchor: &mut usize) -> bool {
    let Some((draft, caret)) = edit.draft_caret_mut() else {
        return false;
    };
    *anchor = 0;
    *caret = draft.len();
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WidgetDraftSnap {
    draft: String,
    caret: usize,
    anchor: usize,
}

fn widget_snap(edit: &WidgetEdit, anchor: usize) -> Option<WidgetDraftSnap> {
    match edit {
        WidgetEdit::Idle => None,
        WidgetEdit::CodeInfo { draft, caret, .. }
        | WidgetEdit::ImageAlt { draft, caret, .. }
        | WidgetEdit::Frontmatter { draft, caret, .. }
        | WidgetEdit::FrontmatterYaml { draft, caret, .. }
        | WidgetEdit::LinkDestination { draft, caret, .. } => Some(WidgetDraftSnap {
            draft: draft.clone(),
            caret: *caret,
            anchor,
        }),
    }
}

fn apply_widget_snap(edit: &mut WidgetEdit, snap: &WidgetDraftSnap, anchor: &mut usize) {
    if let Some((draft, caret)) = edit.draft_caret_mut() {
        *draft = snap.draft.clone();
        *caret = clamp_grapheme_boundary(draft, snap.caret);
    }
    *anchor = clamp_grapheme_boundary(&snap.draft, snap.anchor);
}

const WIDGET_UNDO_LIMIT: usize = 64;

fn push_widget_history(
    edit: &WidgetEdit,
    anchor: usize,
    undo: &mut Vec<WidgetDraftSnap>,
    redo: &mut Vec<WidgetDraftSnap>,
) {
    let Some(snap) = widget_snap(edit, anchor) else {
        return;
    };
    if undo.len() >= WIDGET_UNDO_LIMIT {
        undo.remove(0);
    }
    undo.push(snap);
    redo.clear();
}

fn undo_widget_history(
    edit: &mut WidgetEdit,
    anchor: &mut usize,
    undo: &mut Vec<WidgetDraftSnap>,
    redo: &mut Vec<WidgetDraftSnap>,
) -> bool {
    let Some(prev) = undo.pop() else {
        return false;
    };
    if let Some(current) = widget_snap(edit, *anchor) {
        redo.push(current);
    }
    apply_widget_snap(edit, &prev, anchor);
    true
}

fn redo_widget_history(
    edit: &mut WidgetEdit,
    anchor: &mut usize,
    undo: &mut Vec<WidgetDraftSnap>,
    redo: &mut Vec<WidgetDraftSnap>,
) -> bool {
    let Some(next) = redo.pop() else {
        return false;
    };
    if let Some(current) = widget_snap(edit, *anchor) {
        if undo.len() >= WIDGET_UNDO_LIMIT {
            undo.remove(0);
        }
        undo.push(current);
    }
    apply_widget_snap(edit, &next, anchor);
    true
}

/// Byte offset on the previous/next `\n` line, same character column.
fn vertical_in_draft(draft: &str, at: usize, down: bool) -> usize {
    let at = clamp_grapheme_boundary(draft, at);
    let line_start = draft[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col = draft[line_start..at].graphemes(true).count();
    let (dest_start, dest_end) = if down {
        let line_end = draft[at..]
            .find('\n')
            .map(|i| at + i)
            .unwrap_or(draft.len());
        if line_end >= draft.len() {
            return at;
        }
        let next_start = line_end + 1;
        let next_end = draft[next_start..]
            .find('\n')
            .map(|i| next_start + i)
            .unwrap_or(draft.len());
        (next_start, next_end)
    } else if line_start == 0 {
        return at;
    } else {
        let prev_end = line_start - 1;
        let prev_start = draft[..prev_end].rfind('\n').map(|i| i + 1).unwrap_or(0);
        (prev_start, prev_end)
    };
    let line = &draft[dest_start..dest_end];
    for (n, (i, _)) in line.grapheme_indices(true).enumerate() {
        if n == col {
            return dest_start + i;
        }
    }
    dest_end
}

/// Move or extend the overlay caret. Returns false when no overlay is
/// focused so the body caret can run. The body selection is never touched.
fn move_in_widget(
    edit: &mut WidgetEdit,
    anchor: &mut usize,
    movement: CaretMove,
    extend: bool,
) -> bool {
    if matches!(edit, WidgetEdit::Idle) {
        return false;
    }
    let Some((draft, caret)) = edit.draft_caret_mut() else {
        return false;
    };
    let at = clamp_grapheme_boundary(draft, *caret);
    let anchored = clamp_grapheme_boundary(draft, *anchor);
    *caret = at;
    *anchor = anchored;
    let has_sel = at != anchored;
    let target = match movement {
        CaretMove::Left => {
            if !extend && has_sel {
                at.min(*anchor)
            } else {
                previous_boundary(draft, at)
            }
        }
        CaretMove::Right => {
            if !extend && has_sel {
                at.max(*anchor)
            } else {
                next_boundary(draft, at)
            }
        }
        CaretMove::Home => draft[..at].rfind('\n').map(|i| i + 1).unwrap_or(0),
        CaretMove::End => draft[at..]
            .find('\n')
            .map(|i| at + i)
            .unwrap_or(draft.len()),
        CaretMove::WordLeft => prev_word_start(draft, at),
        CaretMove::WordRight => next_word_end(draft, at),
        CaretMove::DocumentHome => 0,
        CaretMove::DocumentEnd => draft.len(),
        CaretMove::Up => vertical_in_draft(draft, at, false),
        CaretMove::Down => vertical_in_draft(draft, at, true),
        CaretMove::Vertical { delta_lines } => {
            let mut pos = at;
            if delta_lines > 0 {
                for _ in 0..delta_lines {
                    pos = vertical_in_draft(draft, pos, true);
                }
            } else {
                for _ in 0..delta_lines.unsigned_abs() {
                    pos = vertical_in_draft(draft, pos, false);
                }
            }
            pos
        }
    };
    *caret = target.min(draft.len());
    if !extend {
        *anchor = *caret;
    }
    true
}

/// Keep `anchor` and move the caret to `target` (Shift-arrows / Shift-Home).
fn extend_selection_range(
    selected: Range<usize>,
    reversed: bool,
    target: usize,
) -> (Range<usize>, bool) {
    let anchor = if reversed {
        selected.end
    } else {
        selected.start
    };
    if target < anchor {
        (target..anchor, true)
    } else {
        (anchor..target, false)
    }
}

fn wrap_kind_from_rich(command: &RichCommand) -> Option<WrapKind> {
    match command {
        RichCommand::ToggleMark(mark) if *mark == MarkSet::BOLD => Some(WrapKind::Bold),
        RichCommand::ToggleMark(mark) if *mark == MarkSet::ITALIC => Some(WrapKind::Italic),
        RichCommand::ToggleMark(mark) if *mark == MarkSet::CODE => Some(WrapKind::Code),
        RichCommand::ToggleLink => Some(WrapKind::Link),
        _ => None,
    }
}

/// Inspector viewport, internal scroll offset, and last painted preview box.
#[cfg(feature = "gui-tests")]
pub type ImageInspectorScrollState = (Bounds<Pixels>, gpui::Point<Pixels>, Option<Bounds<Pixels>>);

pub struct RichEditorView {
    document: Entity<Document>,
    pub theme: EditorTheme,
    engine: RichEngine,
    list_state: ListState,
    markup_hints_enabled: bool,
    pending_caret_reveal: bool,
    snapshot: Option<Arc<RenderSnapshot>>,
    synced_revision: Option<u64>,
    pub selected_range: Range<usize>,
    pub selection_reversed: bool,
    shadow_selection: Option<crate::shadow::ShadowSelection>,
    search_highlights: Option<crate::search::SearchHighlights>,
    pending_search_reveal: Option<usize>,
    selection_revision: u64,
    /// Horizontal intent for repeated rendered Up/Down moves. This is kept
    /// separately from the source selection because a short wrapped row must
    /// not permanently change the column restored on the next long row.
    vertical_preferred_x: Option<f32>,
    /// Cell body selected by the last Cmd-A. Empty cells are collapsed, so
    /// a second Cmd-A cannot be detected from the range alone.
    table_select_all_cell: Option<Range<usize>>,
    marked_range: Option<Range<usize>>,
    preedit: Option<String>,
    is_selecting: bool,
    widget_selecting: bool,
    widget_anchor: usize,
    cursor_visible: bool,
    focus_handle: FocusHandle,
    ime: ImeOriginState,
    /// Cleared every render and reported before floating controls prepaint.
    table_toolbar_caret: Option<(Bounds<Pixels>, usize)>,
    /// Actual floating control bounds also exclude chrome from body pointer hits.
    table_toolbar_bounds: Option<Bounds<Pixels>>,
    #[cfg(feature = "gui-tests")]
    table_button_bounds: [Option<Bounds<Pixels>>; 6],
    #[cfg(feature = "gui-tests")]
    markup_hint_bounds: Option<(Bounds<Pixels>, String)>,
    #[cfg(feature = "gui-tests")]
    visual_test_bounds: Option<Bounds<Pixels>>,
    widget_edit: WidgetEdit,
    /// Exact document state and target range captured when a widget overlay
    /// begins editing. It is intentionally independent of the mutable draft so
    /// recovery can refuse a stale reattachment without discarding that draft.
    widget_recovery_anchor: Option<WidgetRecoveryAnchor>,
    widget_preedit: Option<String>,
    link_scroll: ScrollHandle,
    link_editor_bounds: Option<Bounds<Pixels>>,
    image_editor: Option<Entity<ImageEditor>>,
    image_editor_request: Option<ImageEditTarget>,
    image_editor_recovery: Option<ImageEditTarget>,
    image_editor_bounds: Option<Bounds<Pixels>>,
    #[cfg(feature = "gui-tests")]
    image_bounds: Vec<(Range<usize>, Bounds<Pixels>)>,
    /// Kept alongside an invalid frontmatter draft so the user can correct
    /// it instead of losing the edit on Enter, Tab, blur, or click-away.
    frontmatter_error: Option<String>,
    widget_undo: Vec<WidgetDraftSnap>,
    widget_redo: Vec<WidgetDraftSnap>,
    /// Network requests are document-controlled content, so they need an
    /// explicit user gesture for each open editor tab.
    remote_images_authorized: bool,
    remote_pending: HashSet<String>,
    remote_failed: HashSet<String>,
    /// Local document-controlled images that have passed the background
    /// preflight. Every value is a validated content-addressed cache copy.
    local_image_paths: HashMap<std::path::PathBuf, std::path::PathBuf>,
    local_image_pending: HashSet<std::path::PathBuf>,
    local_image_checked_revision: HashMap<std::path::PathBuf, u64>,
    /// `data:` images use the same cache-only GPUI boundary as local files.
    /// Their full decode, validation, and write happen off the UI thread.
    data_image_paths: HashMap<String, std::path::PathBuf>,
    data_image_pending: HashSet<String>,
    data_image_checked_revision: HashMap<String, u64>,
    _blink_task: Task<()>,
    _remote_fetch_tasks: Vec<Task<()>>,
    _local_image_tasks: Vec<Task<()>>,
    _data_image_tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

fn valid_list_splice(
    range: &Range<usize>,
    count: usize,
    old_count: usize,
    new_count: usize,
) -> bool {
    range.start <= range.end
        && range.end <= old_count
        && old_count - range.len() + count == new_count
}

/// Map a new caret item to the prior list slots before their measured bounds
/// are invalidated. Insertions use their immediate painted neighbors; a
/// structural replacement uses the old items it actually replaced.
fn previous_caret_items(
    index: usize,
    old_count: usize,
    new_count: usize,
    splice: Option<&(Range<usize>, usize)>,
) -> Option<Range<usize>> {
    if index >= new_count || old_count == 0 {
        return None;
    }
    if let Some((range, count)) =
        splice.filter(|(range, count)| valid_list_splice(range, *count, old_count, new_count))
    {
        if index < range.start {
            return Some(index..index + 1);
        }
        if index >= range.start + count {
            let previous = index - count + range.len();
            return Some(previous..previous + 1);
        }
        if range.len() == *count {
            return Some(index..index + 1);
        }
        if range.is_empty() {
            return Some(range.start.saturating_sub(1)..(range.start + 1).min(old_count));
        }
        return Some(range.clone());
    }
    (old_count == new_count).then_some(index..index + 1)
}

fn item_bounds_are_visible(bounds: Bounds<Pixels>, viewport: Bounds<Pixels>) -> bool {
    viewport.size.height > px(0.)
        && bounds.bottom() > viewport.top()
        && bounds.top() < viewport.bottom()
}

/// A fully visible caret never moves the viewport. The comfort margin is only
/// applied when a caret genuinely needs to be brought back onto the screen.
fn caret_vertical_reveal_delta(caret: Bounds<Pixels>, viewport: Bounds<Pixels>) -> Option<f32> {
    let height = f32::from(viewport.size.height);
    if height <= 0. {
        return None;
    }
    if caret.top() >= viewport.top() && caret.bottom() <= viewport.bottom() {
        return Some(0.);
    }
    let margin = 12f32.min(height * 0.25);
    if caret.top() < viewport.top() {
        Some(f32::from(caret.top() - viewport.top()) - margin)
    } else {
        Some(f32::from(caret.bottom() - viewport.bottom()) + margin)
    }
}

fn reconcile_list_state(
    list_state: &ListState,
    splice: Option<(Range<usize>, usize)>,
    new_count: usize,
) {
    let old_count = list_state.item_count();
    if let Some((range, count)) = splice {
        if valid_list_splice(&range, count, old_count, new_count) {
            if range.len() == count {
                // Content changed inside the same top-level slots. Retain the
                // pixel offset within the item currently at the viewport top.
                list_state.remeasure_items(range);
            } else {
                let scroll_top = list_state.logical_scroll_top();
                let replaced_top = range.contains(&scroll_top.item_ix);
                let replacement_start = range.start;
                list_state.splice(range, count);
                if replaced_top {
                    list_state.scroll_to(gpui::ListOffset {
                        item_ix: replacement_start.min(new_count.saturating_sub(1)),
                        offset_in_item: scroll_top.offset_in_item,
                    });
                }
            }
            return;
        }
    }
    // The empty-document placeholder has one list item but no tree block,
    // so its splice cannot be applied directly. A same-size fallback still
    // keeps the current viewport anchor.
    if old_count == new_count {
        list_state.remeasure_items(0..new_count);
    } else {
        list_state.splice(0..old_count, new_count);
    }
}

impl RichEditorView {
    /// Inspect what GPUI actually shaped and painted, not an estimated layout.
    #[cfg(feature = "gui-tests")]
    pub fn painted_geometry(&self) -> Vec<super::ime::PaintedLeafGeometry> {
        self.ime.painted_geometry()
    }

    #[cfg(feature = "gui-tests")]
    pub fn painted_viewport_bounds(&self) -> Option<Bounds<Pixels>> {
        self.visual_test_bounds
    }

    /// Contextual table controls are chrome, never part of document layout.
    #[cfg(feature = "gui-tests")]
    pub fn painted_table_toolbar_bounds(&self) -> Option<Bounds<Pixels>> {
        self.table_toolbar_bounds
    }

    /// Actual GPUI-prepainted button bounds in stable action order: row above,
    /// row below, delete row, column left, column right, delete column.
    #[cfg(feature = "gui-tests")]
    pub fn painted_table_button_bounds(&self, index: usize) -> Option<Bounds<Pixels>> {
        self.table_button_bounds.get(index).copied().flatten()
    }

    /// Fixed source-syntax label and its actual overlay bounds. These labels
    /// intentionally never include document text or a link destination.
    #[cfg(feature = "gui-tests")]
    pub fn painted_markup_hint(&self) -> Option<(Bounds<Pixels>, String)> {
        self.markup_hint_bounds.clone()
    }

    /// Revision used by the current immutable render snapshot.
    #[cfg(feature = "gui-tests")]
    pub fn test_render_revision(&self) -> Option<u64> {
        self.synced_revision
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_cursor_blink_on(&self) -> bool {
        self.cursor_visible
    }

    /// Fixed semantic label only; widget drafts are not diagnostic metadata.
    #[cfg(feature = "gui-tests")]
    pub fn test_widget_kind(&self) -> Option<&'static str> {
        match self.widget_edit {
            WidgetEdit::Idle => None,
            WidgetEdit::CodeInfo { .. } => Some("code-info"),
            WidgetEdit::ImageAlt { .. } => Some("image-alt"),
            WidgetEdit::Frontmatter { .. } => Some("frontmatter-field"),
            WidgetEdit::FrontmatterYaml { .. } => Some("frontmatter-yaml"),
            WidgetEdit::LinkDestination { .. } => Some("link-destination"),
        }
    }

    /// Fixture-only draft inspection; production diagnostics must not log URLs.
    #[cfg(feature = "gui-tests")]
    pub fn test_widget_draft(&self) -> Option<String> {
        self.widget_display().map(|(draft, _)| draft)
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_widget_selection(&self) -> Option<Range<usize>> {
        (!matches!(self.widget_edit, WidgetEdit::Idle)).then(|| self.widget_sel())
    }

    #[cfg(feature = "gui-tests")]
    pub fn painted_widget_caret_bounds(&self) -> Option<Bounds<Pixels>> {
        (!matches!(self.widget_edit, WidgetEdit::Idle))
            .then(|| self.ime.painted_caret_rect())
            .flatten()
    }

    #[cfg(feature = "gui-tests")]
    pub fn painted_link_editor_bounds(&self) -> Option<Bounds<Pixels>> {
        self.link_editor_bounds
    }

    /// Logical body anchor while the URL field owns the visible caret. This
    /// fresh prepaint geometry lets the oracle detect an obscured label row.
    #[cfg(feature = "gui-tests")]
    pub fn painted_link_anchor_bounds(&self) -> Option<Bounds<Pixels>> {
        matches!(self.widget_edit, WidgetEdit::LinkDestination { .. })
            .then(|| self.table_toolbar_caret.map(|(caret, _)| caret))
            .flatten()
    }

    /// Current list scroll anchor, viewport, and this frame's painted body caret.
    /// A missing caret means that the target is outside the rendered items.
    #[cfg(feature = "gui-tests")]
    pub fn test_viewport_state(
        &self,
    ) -> (gpui::ListOffset, Bounds<Pixels>, Option<Bounds<Pixels>>) {
        (
            self.list_state.logical_scroll_top(),
            self.list_state.viewport_bounds(),
            self.ime.focused_leaf().and_then(|leaf| leaf.caret_bounds),
        )
    }

    /// Borrow the rich engine for inspection (tests, debug overlays).
    /// Production code should drive the engine through `apply_rich` /
    /// `apply_editor_command` instead of mutating it directly.
    pub fn engine_ref(&self) -> &RichEngine {
        &self.engine
    }

    pub fn new(
        document: Entity<Document>,
        theme: EditorTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let focus_sub = cx.on_focus(&focus_handle, window, |this, _window, cx| {
            this.start_blink(cx);
        });
        let blur_sub = cx.on_blur(&focus_handle, window, |this, _window, cx| {
            this.is_selecting = false;
            this.widget_selecting = false;
            this.commit_widget_edit(cx);
            this.stop_blink(cx);
        });
        let selection_revision = document.read(cx).revision();
        let doc_sub = cx.observe(&document, |this, _, cx| {
            let doc = this.document.read(cx);
            let revision = doc.revision();
            if this.selection_revision != revision {
                // Composition belongs to the previous source context, even
                // when replacement text has the same length and the caret's
                // numerical range remains valid. Own edits skip this branch.
                super::ime::clear_composition(
                    &mut this.preedit,
                    &mut this.widget_preedit,
                    &mut this.marked_range,
                );
                this.clear_body_composition_anchor();
                let source = doc.buffer.content();
                if clamp_selection_to_content(
                    &source,
                    &mut this.selected_range,
                    &mut this.selection_reversed,
                ) {
                    this.table_select_all_cell = None;
                    this.pending_caret_reveal = true;
                }
                this.selection_revision = revision;
            }
            this.vertical_preferred_x = None;
            this.ime.clear_visual_navigation();
            // Whenever the document mutates we have to mirror its parsed
            // source into the rich engine so block-level snapshots stay
            // accurate *regardless* of whether the rich view is currently
            // being rendered. Without this sync, an empty buffer carved
            // out of a template in Source mode (alt-cmd-2 → cmd-a →
            // delete) leaves the block tree pointing at the old ranges
            // until the view next paints (e.g. the alt-cmd-1 round-trip).
            // The use-case recorder's per-step JSONL surfaced this.
            this.engine.sync(this.document.read(cx));
            cx.notify();
        });
        Self {
            document,
            theme,
            engine: RichEngine::new(),
            list_state: ListState::new(0, ListAlignment::Top, px(512.)),
            markup_hints_enabled: true,
            pending_caret_reveal: false,
            snapshot: None,
            synced_revision: None,
            selected_range: 0..0,
            selection_reversed: false,
            shadow_selection: None,
            search_highlights: None,
            pending_search_reveal: None,
            selection_revision,
            vertical_preferred_x: None,
            table_select_all_cell: None,
            marked_range: None,
            preedit: None,
            is_selecting: false,
            widget_selecting: false,
            widget_anchor: 0,
            cursor_visible: true,
            focus_handle,
            ime: ImeOriginState::default(),
            table_toolbar_caret: None,
            table_toolbar_bounds: None,
            #[cfg(feature = "gui-tests")]
            table_button_bounds: [None; 6],
            #[cfg(feature = "gui-tests")]
            markup_hint_bounds: None,
            #[cfg(feature = "gui-tests")]
            visual_test_bounds: None,
            widget_edit: WidgetEdit::Idle,
            widget_recovery_anchor: None,
            widget_preedit: None,
            link_scroll: ScrollHandle::new(),
            link_editor_bounds: None,
            image_editor: None,
            image_editor_request: None,
            image_editor_recovery: None,
            image_editor_bounds: None,
            #[cfg(feature = "gui-tests")]
            image_bounds: Vec::new(),
            frontmatter_error: None,
            widget_undo: Vec::new(),
            widget_redo: Vec::new(),
            remote_images_authorized: false,
            remote_pending: HashSet::new(),
            remote_failed: HashSet::new(),
            local_image_paths: HashMap::new(),
            local_image_pending: HashSet::new(),
            local_image_checked_revision: HashMap::new(),
            data_image_paths: HashMap::new(),
            data_image_pending: HashSet::new(),
            data_image_checked_revision: HashMap::new(),
            _blink_task: Task::ready(()),
            _remote_fetch_tasks: Vec::new(),
            _local_image_tasks: Vec::new(),
            _data_image_tasks: Vec::new(),
            _subscriptions: vec![focus_sub, blur_sub, doc_sub],
        }
    }

    pub fn set_theme(&mut self, theme: EditorTheme, cx: &mut Context<Self>) {
        self.theme = theme;
        self.vertical_preferred_x = None;
        self.ime.clear_visual_navigation();
        self.snapshot = None;
        self.list_state.remeasure();
        self.request_caret_reveal(cx);
        cx.notify();
    }

    /// Chrome translation must not remeasure content or reveal an offscreen caret.
    pub fn set_ui_strings(
        &mut self,
        strings: Arc<BTreeMap<String, String>>,
        cx: &mut Context<Self>,
    ) {
        self.theme.ui_strings = strings.clone();
        if let Some(editor) = self.image_editor.clone() {
            editor.update(cx, |editor, cx| editor.set_ui_strings(strings, cx));
        }
        cx.notify();
    }

    pub fn markup_hints_enabled(&self) -> bool {
        self.markup_hints_enabled
    }

    pub fn shadow_selection(&self) -> Option<&crate::shadow::ShadowSelection> {
        self.shadow_selection.as_ref().filter(|shadow| {
            self.synced_revision == Some(shadow.revision)
                && matches!(self.widget_edit, WidgetEdit::Idle)
        })
    }

    pub fn search_highlights(&self) -> Option<&crate::search::SearchHighlights> {
        self.search_highlights.as_ref().filter(|search| {
            self.synced_revision == Some(search.revision)
                && matches!(self.widget_edit, WidgetEdit::Idle)
                && !self.has_image_editor()
        })
    }

    pub fn set_search_highlights(
        &mut self,
        search: Option<crate::search::SearchHighlights>,
        cx: &mut Context<Self>,
    ) {
        if self.search_highlights != search {
            if search
                .as_ref()
                .is_none_or(|search| search.ranges.is_empty())
            {
                self.pending_search_reveal = None;
            }
            self.search_highlights = search;
            cx.notify();
        }
    }

    pub fn reveal_search_match(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.pending_search_reveal = Some(offset);
        cx.notify();
    }

    pub fn scroll_anchor(&self) -> gpui::ListOffset {
        self.list_state.logical_scroll_top()
    }

    pub fn restore_scroll_anchor(&mut self, anchor: gpui::ListOffset, cx: &mut Context<Self>) {
        self.pending_caret_reveal = false;
        self.pending_search_reveal = None;
        self.list_state.scroll_to(anchor);
        cx.notify();
    }

    /// Paint-only peer context. Never update the input caret, list anchor,
    /// composition, undo state, or pending scroll reveal from a projection.
    pub fn set_shadow_selection(
        &mut self,
        shadow: Option<crate::shadow::ShadowSelection>,
        cx: &mut Context<Self>,
    ) {
        if self.shadow_selection != shadow {
            self.shadow_selection = shadow;
            cx.notify();
        }
    }

    /// Compact source-syntax context for app chrome, never document text flow.
    /// Labels intentionally contain no user text, paths, or link destinations.
    pub fn editing_context_hint(&self) -> Option<String> {
        self.markup_hints_enabled
            .then(|| editing_context_hint_for_tree(self.engine.tree(), self.cursor_offset()))
            .flatten()
    }

    pub fn set_markup_hints_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.markup_hints_enabled == enabled {
            return;
        }
        self.markup_hints_enabled = enabled;
        // Hints only repaint context tint and a floating syntax badge. They do not change
        // glyph projection or scroll anchors, including a manually scrolled
        // viewport whose caret is currently offscreen.
        cx.notify();
    }

    /// Reveal the current caret after a mode switch or an external selection change.
    pub fn request_caret_reveal(&mut self, cx: &mut Context<Self>) {
        self.pending_caret_reveal = true;
        cx.notify();
    }

    /// Authorize loading remote image URLs for this document tab.
    ///
    /// The permission deliberately is not persisted: opening a Markdown file
    /// must not send requests to URLs chosen by that file until the reader has
    /// explicitly asked to load them.
    pub fn load_remote_images(&mut self, cx: &mut Context<Self>) {
        self.remote_images_authorized = true;
        self.remote_failed.clear();
        self.snapshot = None;
        cx.notify();
    }

    pub fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    pub fn jump_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.apply_editor_command(EditorCommand::JumpTo(offset), cx);
    }

    fn caret_state(&self) -> CaretState {
        CaretState {
            range: self.selected_range.clone(),
            reversed: self.selection_reversed,
        }
    }

    fn restore_caret(&mut self, caret: CaretState) {
        self.table_select_all_cell = None;
        self.selected_range = caret.range;
        self.selection_reversed = caret.reversed;
        self.pending_caret_reveal = true;
    }

    pub fn apply_rich(&mut self, command: RichCommand, cx: &mut Context<Self>) -> RichOutcome {
        if self.has_image_editor() && !matches!(command, RichCommand::SetImage { .. }) {
            return RichOutcome::Noop;
        }
        if let RichCommand::InsertText(text) = &command {
            if self.widget_insert(text, cx) {
                return RichOutcome::Changed;
            }
        } else if let Some(outcome) = self.widget_try_wrap(&command, cx) {
            return outcome;
        } else {
            let was_editing = !matches!(self.widget_edit, WidgetEdit::Idle);
            if was_editing && !self.commit_widget_edit(cx) {
                // Invalid YAML stays in its overlay; do not let the command
                // that tried to commit it fall through and mutate the body.
                return RichOutcome::Noop;
            }
        }
        let editing_link = matches!(command, RichCommand::ToggleLink);
        if editing_link && self.open_link_destination(cx) {
            return RichOutcome::Noop;
        }
        let mut caret = self.caret_state();
        let mut outcome = RichOutcome::Noop;
        self.document.update(cx, |doc, cx| {
            if let Ok(result) = apply_rich_command(doc, &mut self.engine, &mut caret, command) {
                outcome = result;
                cx.notify();
            }
        });
        self.restore_caret(caret);
        self.selection_revision = self.document.read(cx).revision();
        if editing_link {
            self.open_link_destination(cx);
        }
        if outcome != RichOutcome::Noop {
            self.vertical_preferred_x = None;
            self.ime.clear_visual_navigation();
            self.snapshot = None;
        }
        // A boundary key still acknowledges input: wake the caret without
        // moving content or inventing an undo step for a deliberate no-op.
        self.reset_blink(cx);
        cx.notify();
        outcome
    }

    fn open_link_destination(&mut self, cx: &mut Context<Self>) -> bool {
        let document = self.document.read(cx);
        self.engine.sync(document);
        let source = document.buffer.content();
        let Some((range, draft)) =
            link_destination_at(&self.engine.tree().blocks, &source, self.cursor_offset())
        else {
            return false;
        };
        let revision = document.revision();
        self.widget_edit = WidgetEdit::LinkDestination {
            range: range.clone(),
            revision,
            caret: draft.len(),
            draft,
        };
        self.remember_widget_recovery_anchor(WidgetDraftKind::LinkDestination, range, source);
        // Cmd-K selects the existing URL for immediate replacement.
        self.widget_anchor = 0;
        self.widget_selecting = false;
        self.widget_preedit = None;
        self.frontmatter_error = None;
        self.clear_widget_history();
        self.link_scroll.set_offset(point(px(0.), px(0.)));
        self.reset_blink(cx);
        cx.notify();
        true
    }

    pub fn apply_editor_command(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> EditorOutcome {
        if let Some(editor) = self.image_editor.clone() {
            return if matches!(command, EditorCommand::Undo | EditorCommand::Redo) {
                editor.update(cx, |editor, cx| editor.undo_or_redo(command, cx))
            } else {
                EditorOutcome::Noop
            };
        }
        match command {
            EditorCommand::InsertText(text) => {
                if self.widget_insert(&text, cx) {
                    EditorOutcome::Changed
                } else {
                    self.apply_rich(RichCommand::InsertText(text), cx);
                    EditorOutcome::Changed
                }
            }
            EditorCommand::Backspace => {
                if self.widget_backspace(cx) {
                    EditorOutcome::Changed
                } else if self.apply_rich(RichCommand::Backspace, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::Delete => {
                if self.widget_delete(cx) {
                    EditorOutcome::Changed
                } else if self.apply_rich(RichCommand::Delete, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::DeleteWordLeft => {
                if self.widget_delete_span(prev_word_start, delete_word_before_in_widget, cx) {
                    EditorOutcome::Changed
                } else if self.apply_rich(RichCommand::DeleteWordLeft, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::DeleteWordRight => {
                if self.widget_delete_span(next_word_end, delete_word_after_in_widget, cx) {
                    EditorOutcome::Changed
                } else if self.apply_rich(RichCommand::DeleteWordRight, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::DeleteToLineStart => {
                if self.widget_delete_span(overlay_line_start, delete_to_line_start_in_widget, cx) {
                    EditorOutcome::Changed
                } else if self.apply_rich(RichCommand::DeleteToLineStart, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::DeleteToLineEnd => {
                if self.widget_delete_span(overlay_line_end, delete_to_line_end_in_widget, cx) {
                    EditorOutcome::Changed
                } else if self.apply_rich(RichCommand::DeleteToLineEnd, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::Undo => {
                if widget_owns_caret(&self.widget_edit) {
                    self.undo_widget(cx)
                } else {
                    self.undo(cx)
                }
            }
            EditorCommand::Redo => {
                if widget_owns_caret(&self.widget_edit) {
                    self.redo_widget(cx)
                } else {
                    self.redo(cx)
                }
            }
            EditorCommand::JumpTo(offset) => {
                self.vertical_preferred_x = None;
                self.move_to(offset, false, cx);
                EditorOutcome::CaretMoved
            }
            EditorCommand::SetSelection { start, end } => {
                self.vertical_preferred_x = None;
                let source = self.document.read(cx).buffer.content();
                let caret = body_selection_for_offsets(&source, start, end);
                self.table_select_all_cell = None;
                self.selected_range = caret.range;
                self.selection_reversed = caret.reversed;
                self.pending_caret_reveal = true;
                self.reset_blink(cx);
                cx.notify();
                EditorOutcome::CaretMoved
            }
            EditorCommand::SelectAll => {
                self.vertical_preferred_x = None;
                self.reset_blink(cx);
                if select_all_in_widget(&mut self.widget_edit, &mut self.widget_anchor) {
                    self.table_select_all_cell = None;
                    self.snapshot = None;
                    cx.notify();
                    EditorOutcome::CaretMoved
                } else {
                    let source = self.document.read(cx).buffer.content();
                    self.engine.sync(self.document.read(cx));
                    // Typora: first Cmd-A in a table selects the cell. A
                    // second Cmd-A (already that cell, including an empty
                    // collapsed body) takes the document.
                    match table_select_all_range(
                        &self.engine,
                        &source,
                        &self.selected_range,
                        self.table_select_all_cell.as_ref(),
                    ) {
                        Some(cell) => {
                            self.table_select_all_cell = Some(cell.clone());
                            self.selected_range = cell;
                        }
                        None => {
                            self.table_select_all_cell = None;
                            self.selected_range = 0..source.len();
                        }
                    }
                    self.selection_reversed = false;
                    cx.notify();
                    EditorOutcome::CaretMoved
                }
            }
            EditorCommand::Move(movement) => {
                if move_in_widget(
                    &mut self.widget_edit,
                    &mut self.widget_anchor,
                    movement,
                    false,
                ) {
                    self.reset_blink(cx);
                    self.snapshot = None;
                    cx.notify();
                    EditorOutcome::CaretMoved
                } else {
                    self.move_caret(movement, false, cx);
                    EditorOutcome::CaretMoved
                }
            }
            EditorCommand::Select(movement) => {
                if move_in_widget(
                    &mut self.widget_edit,
                    &mut self.widget_anchor,
                    movement,
                    true,
                ) {
                    self.reset_blink(cx);
                    self.snapshot = None;
                    cx.notify();
                    EditorOutcome::CaretMoved
                } else {
                    self.move_caret(movement, true, cx);
                    EditorOutcome::CaretMoved
                }
            }
            EditorCommand::Wrap(kind) => {
                if widget_wraps_draft(&self.widget_edit) {
                    self.record_widget_edit();
                }
                match apply_wrap_to_widget(&mut self.widget_edit, kind) {
                    WidgetWrapResult::NotFocused => {
                        let cmd = match kind {
                            WrapKind::Bold => RichCommand::ToggleMark(MarkSet::BOLD),
                            WrapKind::Italic => RichCommand::ToggleMark(MarkSet::ITALIC),
                            WrapKind::Code => RichCommand::ToggleMark(MarkSet::CODE),
                            WrapKind::Link => RichCommand::ToggleLink,
                        };
                        if self.apply_rich(cmd, cx) == RichOutcome::Noop {
                            EditorOutcome::Noop
                        } else {
                            EditorOutcome::Changed
                        }
                    }
                    WidgetWrapResult::Ignored => {
                        self.reset_blink(cx);
                        EditorOutcome::Noop
                    }
                    WidgetWrapResult::Applied => {
                        self.reset_blink(cx);
                        self.widget_preedit = None;
                        self.widget_anchor = self.widget_edit.caret();
                        self.snapshot = None;
                        cx.notify();
                        EditorOutcome::Changed
                    }
                }
            }
            EditorCommand::Indent => {
                // Widget overlays own Tab: commit, do not indent the body.
                // IndentList owns table Tab (cell nav) vs list indent.
                if widget_owns_tab(&self.widget_edit) {
                    if self.commit_widget_edit(cx) {
                        EditorOutcome::Changed
                    } else {
                        EditorOutcome::Noop
                    }
                } else if self.apply_rich(RichCommand::IndentList, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::Outdent => {
                if widget_owns_tab(&self.widget_edit) {
                    if self.commit_widget_edit(cx) {
                        EditorOutcome::Changed
                    } else {
                        EditorOutcome::Noop
                    }
                } else if self.apply_rich(RichCommand::OutdentList, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::SetBlockType(block_type) => {
                if widget_owns_tab(&self.widget_edit) {
                    self.commit_widget_edit(cx);
                }
                let cmd = match block_type {
                    BlockType::Paragraph => RichCommand::SetBlockType(BlockType::Paragraph),
                    BlockType::Heading(level) => {
                        RichCommand::SetBlockType(BlockType::Heading(level))
                    }
                };
                if self.apply_rich(cmd, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::ToggleBlockquote => {
                if self.apply_rich(RichCommand::ToggleBlockquote, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::ToggleList { ordered } => {
                if self.apply_rich(RichCommand::ToggleList { ordered }, cx) == RichOutcome::Noop {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::ToggleTaskList => {
                // Task lists are an extension of the bullet-list surface, so
                // we first toggle a bullet list and then drop the task marker
                // at the caret so the rich engine parses it as a task item.
                if self.apply_rich(RichCommand::ToggleList { ordered: false }, cx)
                    == RichOutcome::Noop
                {
                    EditorOutcome::Noop
                } else {
                    // Always follow the bullet toggle with the task marker;
                    // InsertText on the rich surface cannot fail once the
                    // bullet was applied, so we report a single Changed.
                    let _ =
                        self.apply_editor_command(EditorCommand::InsertText("- [ ] ".into()), cx);
                    EditorOutcome::Changed
                }
            }
            EditorCommand::ToggleStrikethrough => {
                if self.apply_rich(RichCommand::ToggleMark(MarkSet::STRIKE), cx)
                    == RichOutcome::Noop
                {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::InsertHorizontalRule => {
                if self.apply_editor_command(EditorCommand::InsertText("\n\n---\n\n".into()), cx)
                    == EditorOutcome::Noop
                {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::InsertCodeBlock => {
                // `InsertText` lands the caret between the fences; the rich
                // engine picks them up as a fenced code block on the next
                // sync.
                if self.apply_editor_command(EditorCommand::InsertText("\n```\n\n```\n".into()), cx)
                    == EditorOutcome::Noop
                {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::InsertImage => {
                self.image_editor_request = Some(ImageEditTarget {
                    range: self.selected_range.clone(),
                    source: self.document.read(cx).buffer.content(),
                    existing: false,
                    alt: String::new(),
                    url: String::new(),
                });
                cx.notify();
                EditorOutcome::CaretMoved
            }
            EditorCommand::InsertTable => {
                // 2 columns × 3 body rows is enough to feel like a table
                // without being overwhelming; Tab navigation inside the
                // table works as it does for authored tables.
                let table =
                    "\n| Column 1 | Column 2 |\n| --- | --- |\n| Cell | Cell |\n| Cell | Cell |\n";
                if self.apply_editor_command(EditorCommand::InsertText(table.into()), cx)
                    == EditorOutcome::Noop
                {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
            EditorCommand::Paragraph => {
                if self.apply_rich(RichCommand::SetBlockType(BlockType::Paragraph), cx)
                    == RichOutcome::Noop
                {
                    EditorOutcome::Noop
                } else {
                    EditorOutcome::Changed
                }
            }
        }
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.widget_edit, WidgetEdit::Idle) {
            if let Some((draft, _)) = self.widget_display() {
                if let Some(text) = overlay_copy_text(&draft, self.widget_sel()) {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
            return;
        }
        let source = self.document.read(cx).buffer.content();
        self.engine.sync(self.document.read(cx));
        let text = self
            .engine
            .markdown_for_selection(&source, self.selected_range.clone());
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut_selection(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.widget_edit, WidgetEdit::Idle) {
            let sel = self.widget_sel();
            if sel.start < sel.end {
                if let Some((draft, _)) = self.widget_display() {
                    if let Some(text) = overlay_copy_text(&draft, sel) {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                }
                let _ = self.widget_backspace(cx);
            }
            return;
        }
        let source = self.document.read(cx).buffer.content();
        self.engine.sync(self.document.read(cx));
        let expanded = self
            .engine
            .expand_markdown_cut_selection(&source, self.selected_range.clone());
        if expanded.start == expanded.end {
            return;
        }
        if let Some(text) = source.get(expanded.clone()) {
            if !text.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
            }
        }
        self.selected_range = expanded;
        self.selection_reversed = false;
        self.apply_rich(RichCommand::Delete, cx);
    }

    fn undo(&mut self, cx: &mut Context<Self>) -> EditorOutcome {
        self.reset_blink(cx);
        let mut restored = None;
        self.document.update(cx, |doc, cx| {
            if let Some(tx) = doc.undo_tx() {
                restored = Some(tx.selection_after);
                cx.notify();
            }
        });
        if let Some(snap) = restored {
            self.table_select_all_cell = None;
            self.selected_range = snap.range();
            self.selection_reversed = snap.reversed;
            self.selection_revision = self.document.read(cx).revision();
            self.engine.invalidate();
            self.snapshot = None;
            self.pending_caret_reveal = true;
            cx.notify();
            EditorOutcome::Changed
        } else {
            EditorOutcome::Noop
        }
    }

    fn redo(&mut self, cx: &mut Context<Self>) -> EditorOutcome {
        self.reset_blink(cx);
        let mut restored = None;
        self.document.update(cx, |doc, cx| {
            if let Some(tx) = doc.redo_tx() {
                restored = Some(tx.selection_after);
                cx.notify();
            }
        });
        if let Some(snap) = restored {
            self.table_select_all_cell = None;
            self.selected_range = snap.range();
            self.selection_reversed = snap.reversed;
            self.selection_revision = self.document.read(cx).revision();
            self.engine.invalidate();
            self.snapshot = None;
            self.pending_caret_reveal = true;
            cx.notify();
            EditorOutcome::Changed
        } else {
            EditorOutcome::Noop
        }
    }

    fn undo_widget(&mut self, cx: &mut Context<Self>) -> EditorOutcome {
        self.reset_blink(cx);
        if undo_widget_history(
            &mut self.widget_edit,
            &mut self.widget_anchor,
            &mut self.widget_undo,
            &mut self.widget_redo,
        ) {
            self.widget_preedit = None;
            self.frontmatter_error = None;
            self.snapshot = None;
            cx.notify();
            EditorOutcome::Changed
        } else {
            EditorOutcome::Noop
        }
    }

    fn redo_widget(&mut self, cx: &mut Context<Self>) -> EditorOutcome {
        self.reset_blink(cx);
        if redo_widget_history(
            &mut self.widget_edit,
            &mut self.widget_anchor,
            &mut self.widget_undo,
            &mut self.widget_redo,
        ) {
            self.widget_preedit = None;
            self.frontmatter_error = None;
            self.snapshot = None;
            cx.notify();
            EditorOutcome::Changed
        } else {
            EditorOutcome::Noop
        }
    }

    fn record_widget_edit(&mut self) {
        self.frontmatter_error = None;
        push_widget_history(
            &self.widget_edit,
            self.widget_anchor,
            &mut self.widget_undo,
            &mut self.widget_redo,
        );
    }

    fn clear_widget_history(&mut self) {
        self.widget_undo.clear();
        self.widget_redo.clear();
    }

    fn move_to(&mut self, offset: usize, extend: bool, cx: &mut Context<Self>) {
        self.table_select_all_cell = None;
        let len = self.document.read(cx).buffer.len_bytes();
        let source = self.document.read(cx).buffer.content();
        self.engine.sync(self.document.read(cx));
        let offset = self.engine.clamp_raw_prefix(
            &source,
            self.engine.snap_caret(offset.min(len), Bias::Left),
            Bias::Left,
        );
        if extend {
            let anchor = if self.selection_reversed {
                self.selected_range.end
            } else {
                self.selected_range.start
            };
            let offset = if let Some(cell) = self.engine.cell_edit_range(anchor, &source) {
                offset.clamp(cell.start, cell.end)
            } else {
                offset
            };
            let (range, reversed) = extend_selection_range(
                self.selected_range.clone(),
                self.selection_reversed,
                offset,
            );
            self.selected_range = range;
            self.selection_reversed = reversed;
        } else {
            self.selected_range = offset..offset;
            self.selection_reversed = false;
        }
        self.pending_caret_reveal = true;
        self.reset_blink(cx);
        cx.notify();
    }

    /// First bring the caret's top-level block into the virtualized viewport.
    /// A second adjustment after its text paints handles tall blocks whose
    /// caret may still be outside the viewport.
    fn reveal_caret_item(&self, was_visible_before_remeasure: bool, cx: &mut Context<Self>) {
        if (!self.pending_caret_reveal && self.pending_search_reveal.is_none())
            || self.list_state.item_count() == 0
        {
            return;
        }
        let blocks = &self.engine.tree().blocks;
        let caret = self
            .pending_search_reveal
            .unwrap_or_else(|| self.cursor_offset());
        let index = blocks
            .iter()
            .position(|block| caret <= block.source_range.end)
            .unwrap_or_else(|| blocks.len().saturating_sub(1));
        if index >= self.list_state.item_count() {
            return;
        }
        let viewport = self.list_state.viewport_bounds();
        if let Some(bounds) = self.list_state.bounds_for_item(index) {
            if bounds.bottom() <= viewport.top() || bounds.top() >= viewport.bottom() {
                self.list_state.scroll_to_reveal_item(index);
            }
        } else if was_visible_before_remeasure {
            // Cache invalidation is not navigation. Let the list measure the
            // visible edited item before deciding whether the caret moved out
            // of view. A follow-up paint also handles a structural split whose
            // new caret item was not reached by this first layout pass.
            cx.notify();
        } else if index != self.list_state.logical_scroll_top().item_ix {
            // An unmeasured item has zero estimated height. Reveal-by-item
            // advances only as many rows as the list measures each frame;
            // direct anchoring reaches a distant caret in the next paint.
            self.list_state.scroll_to(gpui::ListOffset {
                item_ix: index,
                offset_in_item: px(0.),
            });
        }
    }

    fn adjust_scroll_to_painted_caret(&mut self, caret: Bounds<Pixels>, cx: &mut Context<Self>) {
        if !self.pending_caret_reveal || !matches!(self.widget_edit, WidgetEdit::Idle) {
            return;
        }
        let viewport = self.list_state.viewport_bounds();
        let Some(delta) = caret_vertical_reveal_delta(caret, viewport) else {
            return;
        };
        if delta.abs() > 0.5 {
            self.list_state.scroll_by(px(delta));
            cx.notify();
        } else {
            self.pending_caret_reveal = false;
        }
    }

    /// Prefer the last painted WYSIWYG glyph geometry for a one-row vertical
    /// move. The rich engine remains the fallback when the virtualized list
    /// has not painted the relevant row yet (or during the first frame).
    fn rendered_vertical_caret(&mut self, source: &str, cursor: usize, delta: i32) -> usize {
        if let Some(target) =
            self.ime
                .visual_vertical_target(cursor, delta, self.vertical_preferred_x)
        {
            self.vertical_preferred_x = Some(target.preferred_x);
            if let Some(target_source) = target.source {
                // A revealed fence or list marker is a painted row but not
                // an editable caret stop. If move_to would clamp it back to
                // the current code-body position, use the source-line path
                // to cross that structural row instead of trapping Up/Down.
                let snapped = self.engine.clamp_raw_prefix(
                    source,
                    self.engine.snap_caret(target_source, Bias::Left),
                    Bias::Left,
                );
                if snapped != cursor {
                    return target_source;
                }
            }
        }
        self.engine.vertical_caret(source, cursor, delta)
    }

    fn move_caret(&mut self, movement: CaretMove, extend: bool, cx: &mut Context<Self>) {
        let source = self.document.read(cx).buffer.content();
        self.engine.sync(self.document.read(cx));
        let cursor = self.cursor_offset();
        if !matches!(movement, CaretMove::Up | CaretMove::Down) {
            self.vertical_preferred_x = None;
        }
        let target = match movement {
            CaretMove::Left => {
                if !extend && !self.selected_range.is_empty() {
                    self.selected_range.start
                } else {
                    self.engine.prev_caret(&source, cursor)
                }
            }
            CaretMove::Right => {
                if !extend && !self.selected_range.is_empty() {
                    self.selected_range.end
                } else {
                    self.engine.next_caret(&source, cursor)
                }
            }
            CaretMove::Home => self.engine.line_start_caret(&source, cursor),
            CaretMove::End => self.engine.line_end_caret(&source, cursor),
            CaretMove::WordLeft => self.engine.prev_word_caret(&source, cursor),
            CaretMove::WordRight => self.engine.next_word_caret(&source, cursor),
            CaretMove::DocumentHome => self.engine.clamp_raw_prefix(
                &source,
                self.engine.snap_caret(0, Bias::Right),
                Bias::Right,
            ),
            CaretMove::DocumentEnd => self.engine.document_end_caret(&source, cursor),
            CaretMove::Up => self.rendered_vertical_caret(&source, cursor, -1),
            CaretMove::Down => self.rendered_vertical_caret(&source, cursor, 1),
            CaretMove::Vertical { delta_lines } => {
                self.engine.vertical_caret(&source, cursor, delta_lines)
            }
        };
        self.move_to(target, extend, cx);
    }

    fn start_blink(&mut self, cx: &mut Context<Self>) {
        self.reset_blink(cx);
    }

    fn stop_blink(&mut self, cx: &mut Context<Self>) {
        self.cursor_visible = false;
        self._blink_task = Task::ready(());
        cx.notify();
    }

    fn spawn_blink_task(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            if this
                .update(cx, |editor, cx| {
                    editor.cursor_visible = !editor.cursor_visible;
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        })
    }

    fn reset_blink(&mut self, cx: &mut Context<Self>) {
        self.cursor_visible = true;
        self._blink_task = Self::spawn_blink_task(cx);
        cx.notify();
    }

    fn sync_snapshot(&mut self, cx: &mut Context<Self>) -> (Arc<RenderSnapshot>, bool) {
        let revision = self.document.read(cx).revision();
        let caret = self.cursor_offset();
        let selected_range = self.selected_range.clone();
        if self.synced_revision == Some(revision) {
            if let Some(snapshot) = &self.snapshot {
                if snapshot.caret == caret && snapshot.selected_range == selected_range {
                    return (snapshot.clone(), false);
                }
                let mut next = (**snapshot).clone();
                next.caret = caret;
                next.selected_range = selected_range;
                let snapshot = Arc::new(next);
                self.snapshot = Some(snapshot.clone());
                return (snapshot, false);
            }
        }
        let widget_only = self.synced_revision == Some(revision) && self.snapshot.is_none();
        let (base_dir, source) = {
            let doc = self.document.read(cx);
            let base_dir = doc
                .path
                .as_ref()
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf());
            let source = doc.buffer.content();
            self.engine.sync(doc);
            (base_dir, source)
        };
        let new_real = self.engine.tree().blocks.len();
        let new_count = new_real.max(1);
        let mut caret_item_was_visible = false;
        if !widget_only {
            let splice = self
                .engine
                .last_splice()
                .map(|splice| (splice.range.clone(), splice.new_count));
            let index = self
                .engine
                .tree()
                .blocks
                .iter()
                .position(|block| caret <= block.source_range.end)
                .unwrap_or_else(|| new_real.saturating_sub(1));
            // Read the actual old measurements before remeasure/splice makes
            // bounds_for_item return None. Otherwise a visible middle edit is
            // mistaken for a distant unpainted block and anchored at its top.
            if self.pending_caret_reveal {
                let viewport = self.list_state.viewport_bounds();
                caret_item_was_visible = previous_caret_items(
                    index,
                    self.list_state.item_count(),
                    new_count,
                    splice.as_ref(),
                )
                .is_some_and(|mut items| {
                    items.any(|index| {
                        self.list_state
                            .bounds_for_item(index)
                            .is_some_and(|bounds| item_bounds_are_visible(bounds, viewport))
                    })
                });
            }
            reconcile_list_state(&self.list_state, splice, new_count);
        }
        self.enqueue_data_images(revision, cx);
        self.enqueue_local_images(base_dir.as_deref(), revision, cx);
        self.enqueue_remote_images(cx);
        let snapshot = Arc::new(RenderSnapshot {
            tree: self.engine.tree().clone(),
            source,
            theme: self.theme.clone(),
            base_dir,
            local_image_paths: self.local_image_paths.clone(),
            local_image_pending: self.local_image_pending.clone(),
            data_image_paths: self.data_image_paths.clone(),
            data_image_pending: self.data_image_pending.clone(),
            editing_code: match &self.widget_edit {
                WidgetEdit::CodeInfo { id, draft, .. } => Some((*id, draft.clone())),
                _ => None,
            },
            editing_image: match &self.widget_edit {
                WidgetEdit::ImageAlt { range, draft, .. } => Some((range.clone(), draft.clone())),
                _ => None,
            },
            caret,
            selected_range,
        });
        self.snapshot = Some(snapshot.clone());
        self.synced_revision = Some(revision);
        (snapshot, caret_item_was_visible)
    }

    fn enqueue_remote_images(&mut self, cx: &mut Context<Self>) {
        if !self.remote_images_authorized {
            return;
        }
        let cache_dir = default_image_cache_dir();
        for url in collect_remote_image_urls(self.engine.tree()) {
            if self.remote_pending.contains(&url) || self.remote_failed.contains(&url) {
                continue;
            }
            let dest = cache_path_for_url(&cache_dir, &url);
            if dest.is_file() {
                continue;
            }
            self.remote_pending.insert(url.clone());
            let url_fetch = url.clone();
            let url_status = url;
            let dest_fetch = dest;
            let task = cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { fetch_remote_image(&url_fetch, &dest_fetch) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.remote_pending.remove(&url_status);
                    if result.is_err() {
                        this.remote_failed.insert(url_status);
                    }
                    this.snapshot = None;
                    cx.notify();
                });
            });
            self._remote_fetch_tasks.push(task);
        }
    }

    /// Decode and validate document `data:` images off the UI thread. Even a
    /// syntactically local data URL can be megabytes long, so the render path
    /// only observes this approved cache map and never calls `img(String)`.
    fn enqueue_data_images(&mut self, revision: u64, cx: &mut Context<Self>) {
        let urls = collect_data_image_urls(self.engine.tree());
        let active = urls.iter().cloned().collect::<HashSet<_>>();
        self.data_image_paths.retain(|url, _| active.contains(url));
        self.data_image_checked_revision
            .retain(|url, _| active.contains(url));

        let cache_dir = default_image_cache_dir();
        for url in urls {
            if self.data_image_pending.contains(&url)
                || self.data_image_checked_revision.get(&url) == Some(&revision)
            {
                continue;
            }
            self.data_image_pending.insert(url.clone());
            let url_read = url.clone();
            let url_status = url;
            let cache_dir = cache_dir.clone();
            let task = cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { materialize_safe_data_image(&url_read, &cache_dir) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.data_image_pending.remove(&url_status);
                    this.data_image_checked_revision
                        .insert(url_status.clone(), revision);
                    match result {
                        Some(approved) => {
                            this.data_image_paths.insert(url_status.clone(), approved);
                        }
                        None => {
                            this.data_image_paths.remove(&url_status);
                        }
                    }
                    this.snapshot = None;
                    cx.notify();
                });
            });
            self._data_image_tasks.push(task);
        }
    }

    /// Classify document-controlled local images without ever asking GPUI to
    /// infer their type first. GPUI treats every non-raster byte stream as an
    /// SVG candidate, so the preflight must look at content rather than an
    /// extension. Each document revision re-checks visible paths in the
    /// background; a previous safe SVG cache copy remains visible while that
    /// work is in flight.
    fn enqueue_local_images(
        &mut self,
        base_dir: Option<&std::path::Path>,
        revision: u64,
        cx: &mut Context<Self>,
    ) {
        let paths = collect_local_image_paths(self.engine.tree(), base_dir);
        let active = paths.iter().cloned().collect::<HashSet<_>>();
        self.local_image_paths
            .retain(|source, _| active.contains(source));
        self.local_image_checked_revision
            .retain(|source, _| active.contains(source));

        let cache_dir = default_image_cache_dir();
        for source in paths {
            if self.local_image_pending.contains(&source)
                || self.local_image_checked_revision.get(&source) == Some(&revision)
            {
                continue;
            }
            self.local_image_pending.insert(source.clone());
            let source_read = source.clone();
            let source_status = source;
            let cache_dir = cache_dir.clone();
            let task = cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { materialize_safe_local_image(&source_read, &cache_dir) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.local_image_pending.remove(&source_status);
                    this.local_image_checked_revision
                        .insert(source_status.clone(), revision);
                    match result {
                        Ok(approved) => {
                            this.local_image_paths
                                .insert(source_status.clone(), approved);
                        }
                        Err(_) => {
                            this.local_image_paths.remove(&source_status);
                        }
                    }
                    this.snapshot = None;
                    cx.notify();
                });
            });
            self._local_image_tasks.push(task);
        }
    }

    fn offset_from_utf16(content: &str, offset: usize) -> usize {
        super::ime::offset_from_utf16(content, offset)
    }

    fn offset_to_utf16(content: &str, offset: usize) -> usize {
        super::ime::offset_to_utf16(content, offset)
    }

    fn widget_try_wrap(
        &mut self,
        command: &RichCommand,
        cx: &mut Context<Self>,
    ) -> Option<RichOutcome> {
        if !widget_owns_wrap(&self.widget_edit) {
            return None;
        }
        if !matches!(
            command,
            RichCommand::ToggleMark(_) | RichCommand::ToggleLink
        ) {
            return None;
        }
        self.reset_blink(cx);
        let Some(kind) = wrap_kind_from_rich(command) else {
            // Other marks (strike, …): consume so the body is not rewritten.
            return Some(RichOutcome::Noop);
        };
        if widget_wraps_draft(&self.widget_edit) {
            self.record_widget_edit();
        }
        match apply_wrap_to_widget(&mut self.widget_edit, kind) {
            WidgetWrapResult::NotFocused => None,
            WidgetWrapResult::Ignored => Some(RichOutcome::Noop),
            WidgetWrapResult::Applied => {
                self.widget_preedit = None;
                self.widget_anchor = self.widget_edit.caret();
                self.snapshot = None;
                cx.notify();
                Some(RichOutcome::Changed)
            }
        }
    }

    fn widget_insert(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        if matches!(self.widget_edit, WidgetEdit::Idle) {
            return false;
        }
        self.reset_blink(cx);
        if matches!(self.widget_edit, WidgetEdit::LinkDestination { .. })
            && text.chars().any(char::is_control)
        {
            self.frontmatter_error =
                Some("A link destination cannot contain control characters.".into());
            cx.notify();
            return true;
        }
        if text.contains('\n') && !self.widget_edit.allows_newline() {
            self.commit_widget_edit(cx);
            return true;
        }
        self.record_widget_edit();
        self.widget_preedit = None;
        insert_into_widget(&mut self.widget_edit, &mut self.widget_anchor, text);
        self.snapshot = None;
        cx.notify();
        true
    }

    fn widget_backspace(&mut self, cx: &mut Context<Self>) -> bool {
        if matches!(self.widget_edit, WidgetEdit::Idle) {
            return false;
        }
        self.reset_blink(cx);
        if let Some(snap) = widget_snap(&self.widget_edit, self.widget_anchor) {
            let range = widget_range(snap.caret, snap.anchor, snap.draft.len());
            if range.start == range.end && range.start == 0 {
                return true;
            }
        }
        self.record_widget_edit();
        self.widget_preedit = None;
        delete_before_in_widget(&mut self.widget_edit, &mut self.widget_anchor);
        self.snapshot = None;
        cx.notify();
        true
    }

    fn widget_delete(&mut self, cx: &mut Context<Self>) -> bool {
        if !widget_owns_caret(&self.widget_edit) {
            return false;
        }
        self.reset_blink(cx);
        if let Some(snap) = widget_snap(&self.widget_edit, self.widget_anchor) {
            let range = widget_range(snap.caret, snap.anchor, snap.draft.len());
            if range.start == range.end && range.end == snap.draft.len() {
                return true;
            }
        }
        self.record_widget_edit();
        self.widget_preedit = None;
        delete_after_in_widget(&mut self.widget_edit, &mut self.widget_anchor);
        self.snapshot = None;
        cx.notify();
        true
    }

    fn widget_delete_span(
        &mut self,
        target_of: impl Fn(&str, usize) -> usize,
        apply: fn(&mut WidgetEdit, &mut usize),
        cx: &mut Context<Self>,
    ) -> bool {
        if matches!(self.widget_edit, WidgetEdit::Idle) {
            return false;
        }
        self.reset_blink(cx);
        if let Some(snap) = widget_snap(&self.widget_edit, self.widget_anchor) {
            let range = widget_range(snap.caret, snap.anchor, snap.draft.len());
            let (start, end) = if range.start != range.end {
                (range.start, range.end)
            } else {
                let at = range.start;
                let target = target_of(&snap.draft, at).min(snap.draft.len());
                if target <= at {
                    (target, at)
                } else {
                    (at, target)
                }
            };
            if start == end {
                return true;
            }
        }
        self.record_widget_edit();
        self.widget_preedit = None;
        apply(&mut self.widget_edit, &mut self.widget_anchor);
        self.snapshot = None;
        cx.notify();
        true
    }

    fn widget_display(&self) -> Option<(String, Option<String>)> {
        let draft = match &self.widget_edit {
            WidgetEdit::Idle => return None,
            WidgetEdit::CodeInfo { draft, .. }
            | WidgetEdit::ImageAlt { draft, .. }
            | WidgetEdit::Frontmatter { draft, .. }
            | WidgetEdit::LinkDestination { draft, .. }
            | WidgetEdit::FrontmatterYaml { draft, .. } => draft.clone(),
        };
        Some((draft, self.widget_preedit.clone()))
    }

    fn cancel_widget_edit(&mut self, cx: &mut Context<Self>) -> bool {
        if matches!(self.widget_edit, WidgetEdit::Idle) {
            return false;
        }
        self.reset_blink(cx);
        self.widget_edit = WidgetEdit::Idle;
        self.widget_recovery_anchor = None;
        self.widget_preedit = None;
        self.frontmatter_error = None;
        self.widget_selecting = false;
        self.clear_widget_history();
        self.snapshot = None;
        cx.notify();
        true
    }

    /// Flush focused field edits before save/archive. Invalid or stale drafts
    /// remain visible and return false so persistence cannot silently save an
    /// older document while the user sees a newer focused value.
    pub fn commit_pending_widget_edit(&mut self, cx: &mut Context<Self>) -> bool {
        !self.has_image_editor()
            && (matches!(self.widget_edit, WidgetEdit::Idle) || self.commit_widget_edit(cx))
    }

    /// Capture a focused widget overlay without committing it into Markdown.
    ///
    /// Active IME preedit cannot be resumed as an OS composition after restart.
    /// To avoid losing visible text, it is materialized into the recoverable
    /// draft and selected on restore. The app layer is responsible for bounding
    /// and serializing this value in its private recovery store.
    ///
    /// Call [`Self::recovery_widget_draft_sizes`] first when deciding whether a
    /// checkpoint may allocate a snapshot of a large document.
    pub fn recovery_widget_draft(&self) -> Option<WidgetDraftSnapshot> {
        if let Some(target) = self
            .image_editor_request
            .as_ref()
            .or(self.image_editor_recovery.as_ref())
        {
            let draft = encode_image_draft(target);
            return Some(WidgetDraftSnapshot {
                kind: WidgetDraftKind::ImageProperties,
                original_source: target.source.clone(),
                source_range: target.range.clone(),
                selection: 0..draft.len(),
                selection_reversed: false,
                draft,
            });
        }
        if matches!(self.widget_edit, WidgetEdit::Idle) {
            let anchor = self.widget_recovery_anchor.as_ref()?;
            if anchor.kind != WidgetDraftKind::BodyComposition {
                return None;
            }
            let draft = self.preedit.as_deref().filter(|draft| !draft.is_empty())?;
            return Some(WidgetDraftSnapshot {
                kind: WidgetDraftKind::BodyComposition,
                original_source: anchor.original_source.clone(),
                source_range: anchor.source_range.clone(),
                draft: draft.to_owned(),
                // The scratch fallback presents all materialized preedit text
                // selected, rather than inventing a body insertion target.
                selection: 0..draft.len(),
                selection_reversed: false,
            });
        }
        let anchor = self.widget_recovery_anchor.as_ref()?;
        if self.widget_edit.recovery_kind()? != anchor.kind {
            return None;
        }
        let (draft, selection, selection_reversed) = self.materialized_widget_draft()?;
        Some(WidgetDraftSnapshot {
            kind: anchor.kind,
            original_source: anchor.original_source.clone(),
            source_range: anchor.source_range.clone(),
            draft,
            selection,
            selection_reversed,
        })
    }

    /// Return recovery payload sizes without allocating the payload.
    ///
    /// The first value is the exact source anchor size. The second is the
    /// materialized widget draft size, including visible IME preedit text that
    /// would otherwise be inserted while taking a snapshot. `None` means that
    /// there is no recoverable active widget or the size cannot be represented.
    pub fn recovery_widget_draft_sizes(&self) -> Option<(usize, usize)> {
        if let Some(target) = self
            .image_editor_request
            .as_ref()
            .or(self.image_editor_recovery.as_ref())
        {
            let kind_length = if target.existing {
                "edit".len()
            } else {
                "insert".len()
            };
            let overhead = "Image draft ()\nLocation bytes: \n\nAlternative text:\n".len()
                + kind_length
                + target.url.len().to_string().len();
            return Some((
                target.source.len(),
                target
                    .url
                    .len()
                    .checked_add(target.alt.len())?
                    .checked_add(overhead)?,
            ));
        }
        if matches!(self.widget_edit, WidgetEdit::Idle) {
            let anchor = self.widget_recovery_anchor.as_ref()?;
            if anchor.kind != WidgetDraftKind::BodyComposition {
                return None;
            }
            let draft = self.preedit.as_deref().filter(|draft| !draft.is_empty())?;
            return Some((anchor.original_source.len(), draft.len()));
        }
        let anchor = self.widget_recovery_anchor.as_ref()?;
        if self.widget_edit.recovery_kind()? != anchor.kind {
            return None;
        }
        let (draft, _) = self.widget_edit.draft_and_caret()?;
        let preedit_bytes = self
            .widget_preedit
            .as_deref()
            .filter(|preedit| !preedit.is_empty())
            .map_or(0, str::len);
        Some((
            anchor.original_source.len(),
            draft.len().checked_add(preedit_bytes)?,
        ))
    }

    /// Reattach an uncommitted widget draft only to the identical original
    /// document and target. Callers must preserve `raw_draft_text()` in a
    /// scratch recovery tab when this returns an error.
    pub fn restore_recovery_widget_draft(
        &mut self,
        snapshot: &WidgetDraftSnapshot,
        cx: &mut Context<Self>,
    ) -> Result<(), WidgetDraftRestoreError> {
        if !snapshot.has_valid_source_anchor() {
            return Err(WidgetDraftRestoreError::InvalidSnapshot);
        }
        let (source, revision) = {
            let document = self.document.read(cx);
            (document.buffer.content(), document.revision())
        };
        if source != snapshot.original_source {
            return Err(WidgetDraftRestoreError::DocumentChanged);
        }
        self.engine.sync(self.document.read(cx));
        let selection = snapshot.repaired_selection();
        if snapshot.kind == WidgetDraftKind::ImageProperties {
            let (existing, url, alt) = decode_image_draft(&snapshot.draft)
                .ok_or(WidgetDraftRestoreError::InvalidSnapshot)?;
            if existing
                && !image_exists_at_range(&self.engine.tree().blocks, &snapshot.source_range)
            {
                return Err(WidgetDraftRestoreError::TargetUnavailable);
            }
            self.image_editor_request = Some(ImageEditTarget {
                range: snapshot.source_range.clone(),
                source,
                existing,
                alt,
                url,
            });
            cx.notify();
            return Ok(());
        }
        let caret = if snapshot.selection_reversed {
            selection.start
        } else {
            selection.end
        };
        let widget_anchor = if snapshot.selection_reversed {
            selection.end
        } else {
            selection.start
        };
        let widget_edit = match snapshot.kind {
            // Display-only body IME text has no safe insertion target after a
            // restart. Let the app preserve it in a pathless scratch document
            // instead of silently changing the recovered Markdown body.
            WidgetDraftKind::BodyComposition => {
                return Err(WidgetDraftRestoreError::TargetUnavailable);
            }
            WidgetDraftKind::CodeInfo => {
                let id = code_info_id_at_range(&self.engine.tree().blocks, &snapshot.source_range)
                    .ok_or(WidgetDraftRestoreError::TargetUnavailable)?;
                WidgetEdit::CodeInfo {
                    id,
                    draft: snapshot.draft.clone(),
                    caret,
                }
            }
            WidgetDraftKind::ImageAlt => {
                if !image_exists_at_range(&self.engine.tree().blocks, &snapshot.source_range) {
                    return Err(WidgetDraftRestoreError::TargetUnavailable);
                }
                WidgetEdit::ImageAlt {
                    range: snapshot.source_range.clone(),
                    draft: snapshot.draft.clone(),
                    caret,
                }
            }
            WidgetDraftKind::ImageProperties => {
                return Err(WidgetDraftRestoreError::InvalidSnapshot)
            }
            WidgetDraftKind::FrontmatterField(field) => {
                if frontmatter_range(&source).as_ref() != Some(&snapshot.source_range) {
                    return Err(WidgetDraftRestoreError::TargetUnavailable);
                }
                WidgetEdit::Frontmatter {
                    key: field.key(),
                    draft: snapshot.draft.clone(),
                    caret,
                }
            }
            WidgetDraftKind::FrontmatterYaml => {
                if frontmatter_range(&source).as_ref() != Some(&snapshot.source_range) {
                    return Err(WidgetDraftRestoreError::TargetUnavailable);
                }
                WidgetEdit::FrontmatterYaml {
                    draft: snapshot.draft.clone(),
                    caret,
                }
            }
            WidgetDraftKind::LinkDestination => {
                let found = link_destination_at(
                    &self.engine.tree().blocks,
                    &source,
                    snapshot.source_range.start,
                );
                if !found.is_some_and(|(range, _)| range == snapshot.source_range) {
                    return Err(WidgetDraftRestoreError::TargetUnavailable);
                }
                WidgetEdit::LinkDestination {
                    range: snapshot.source_range.clone(),
                    revision,
                    draft: snapshot.draft.clone(),
                    caret,
                }
            }
        };
        self.widget_edit = widget_edit;
        self.widget_recovery_anchor = Some(WidgetRecoveryAnchor {
            kind: snapshot.kind,
            original_source: snapshot.original_source.clone(),
            source_range: snapshot.source_range.clone(),
        });
        self.widget_anchor = widget_anchor;
        self.widget_selecting = false;
        self.widget_preedit = None;
        self.clear_widget_history();
        self.frontmatter_error = self.frontmatter_widget_error(cx);
        self.snapshot = None;
        self.reset_blink(cx);
        cx.notify();
        Ok(())
    }

    /// External reconciliation must not replace an independent field draft.
    pub fn has_pending_widget_edit(&self) -> bool {
        self.has_image_editor() || !matches!(self.widget_edit, WidgetEdit::Idle)
    }

    /// True while either the document body or a focused widget owns visible
    /// IME preedit. Callers must not commit or discard a widget in this state:
    /// its displayed bytes have not reached the Markdown buffer yet.
    pub fn has_pending_composition(&self) -> bool {
        composition_is_pending(
            self.marked_range.is_some(),
            self.preedit.is_some(),
            self.widget_preedit.is_some(),
        )
    }

    fn commit_widget_edit(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some(error) = self.frontmatter_widget_error(cx) {
            // Do not take `widget_edit`: the draft remains visible and
            // editable, and a body command cannot accidentally follow it.
            self.widget_preedit = None;
            self.widget_selecting = false;
            self.frontmatter_error = Some(error);
            self.reset_blink(cx);
            cx.notify();
            return false;
        }
        self.widget_preedit = None;
        self.widget_selecting = false;
        self.frontmatter_error = None;
        self.clear_widget_history();
        let edit = std::mem::take(&mut self.widget_edit);
        self.widget_recovery_anchor = None;
        match edit {
            WidgetEdit::Idle => false,
            WidgetEdit::CodeInfo { id, draft, .. } => {
                self.apply_rich(RichCommand::SetCodeInfo { id, info: draft }, cx);
                true
            }
            WidgetEdit::ImageAlt { range, draft, .. } => {
                self.apply_rich(
                    RichCommand::SetImageAlt {
                        source_range: range,
                        alt: draft,
                    },
                    cx,
                );
                true
            }
            WidgetEdit::Frontmatter { key, draft, .. } => {
                self.apply_rich(
                    RichCommand::SetFrontmatterField {
                        key: key.to_string(),
                        value: draft,
                    },
                    cx,
                );
                true
            }
            WidgetEdit::FrontmatterYaml { draft, .. } => {
                self.apply_rich(RichCommand::SetFrontmatter { raw: draft }, cx);
                true
            }
            WidgetEdit::LinkDestination { range, draft, .. } => {
                self.apply_rich(
                    RichCommand::SetLinkDestination {
                        destination: range,
                        url: draft,
                    },
                    cx,
                );
                true
            }
        }
    }

    fn remember_widget_recovery_anchor(
        &mut self,
        kind: WidgetDraftKind,
        source_range: Range<usize>,
        original_source: String,
    ) {
        self.widget_recovery_anchor = (source_range.start <= source_range.end
            && original_source.get(source_range.clone()).is_some())
        .then_some(WidgetRecoveryAnchor {
            kind,
            original_source,
            source_range,
        });
    }

    fn remember_body_composition_anchor(&mut self, source: String, caret: usize) {
        let caret = clamp_grapheme_boundary(&source, caret);
        self.widget_recovery_anchor = Some(WidgetRecoveryAnchor {
            kind: WidgetDraftKind::BodyComposition,
            original_source: source,
            source_range: caret..caret,
        });
    }

    fn clear_body_composition_anchor(&mut self) {
        if self
            .widget_recovery_anchor
            .as_ref()
            .is_some_and(|anchor| anchor.kind == WidgetDraftKind::BodyComposition)
        {
            self.widget_recovery_anchor = None;
        }
    }

    fn materialized_widget_draft(&self) -> Option<(String, Range<usize>, bool)> {
        let (raw_draft, caret) = match &self.widget_edit {
            WidgetEdit::Idle => return None,
            WidgetEdit::CodeInfo { draft, caret, .. }
            | WidgetEdit::ImageAlt { draft, caret, .. }
            | WidgetEdit::Frontmatter { draft, caret, .. }
            | WidgetEdit::FrontmatterYaml { draft, caret, .. }
            | WidgetEdit::LinkDestination { draft, caret, .. } => (draft, *caret),
        };
        let caret = clamp_grapheme_boundary(raw_draft, caret);
        if let Some(preedit) = self
            .widget_preedit
            .as_deref()
            .filter(|text| !text.is_empty())
        {
            let mut materialized = raw_draft.clone();
            materialized.insert_str(caret, preedit);
            let end = caret + preedit.len();
            return Some((materialized, caret..end, false));
        }
        let anchor = clamp_grapheme_boundary(raw_draft, self.widget_anchor);
        Some((
            raw_draft.clone(),
            caret.min(anchor)..caret.max(anchor),
            anchor > caret,
        ))
    }

    fn frontmatter_widget_error(&self, cx: &Context<Self>) -> Option<String> {
        let source = self.document.read(cx).buffer.content();
        match &self.widget_edit {
            WidgetEdit::LinkDestination {
                revision, draft, ..
            } => {
                if *revision != self.document.read(cx).revision() {
                    Some("The document changed. Cancel and reopen the link editor.".into())
                } else if draft.chars().any(char::is_control) {
                    Some("A link destination cannot contain control characters.".into())
                } else {
                    None
                }
            }
            WidgetEdit::Frontmatter { key, draft, .. } => {
                let raw = markrust_core::parse_frontmatter(&source)
                    .and_then(|info| source.get(info.start_byte..info.end_byte))
                    .unwrap_or("");
                markrust_core::upsert_yaml_key(raw, key, draft)
                    .err()
                    .map(|error| error.message().to_string())
            }
            WidgetEdit::FrontmatterYaml { draft, .. } => {
                markrust_core::validate_frontmatter_yaml(draft)
                    .err()
                    .map(|error| error.message().to_string())
            }
            WidgetEdit::Idle | WidgetEdit::CodeInfo { .. } | WidgetEdit::ImageAlt { .. } => None,
        }
    }

    /// Enter / Shift-Enter in YAML inserts a draft newline; other overlays commit.
    /// Returns true when the widget consumed the key (do not run a body command).
    fn consume_widget_newline(&mut self, cx: &mut Context<Self>) -> bool {
        if matches!(self.widget_edit, WidgetEdit::FrontmatterYaml { .. }) {
            self.widget_insert("\n", cx)
        } else {
            self.commit_widget_edit(cx)
        }
    }

    /// Click in leftover viewport below the last painted leaf or non-text
    /// widget (or anywhere on an unpainted / newlines-only document). Opens a
    /// trailing blank if the file has none, then places the caret there —
    /// never a no-op and never a hit-test onto the last paragraph.
    fn click_below_painted_content(
        &mut self,
        point: gpui::Point<Pixels>,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.ime.point_is_below_painted_content(point) {
            return false;
        }
        if self.has_image_editor() {
            self.focus_current_input(window, cx);
            return true;
        }
        self.commit_widget_edit(cx);
        self.engine.sync(self.document.read(cx));
        if !extend {
            let mut caret = self.caret_state();
            let mut outcome = RichOutcome::Noop;
            self.document.update(cx, |doc, cx| {
                outcome = place_caret_for_click_below(doc, &mut self.engine, &mut caret);
                if outcome != RichOutcome::Noop {
                    cx.notify();
                }
            });
            self.restore_caret(caret);
            if outcome != RichOutcome::Noop {
                self.reset_blink(cx);
                self.snapshot = None;
                self.synced_revision = None;
            }
        }
        let source = caret_for_click_below_content(self.engine.tree());
        self.click_source(source, extend, window, cx);
        true
    }

    fn finish_widget_before_switch(&mut self, cx: &mut Context<Self>) -> bool {
        matches!(self.widget_edit, WidgetEdit::Idle) || self.commit_widget_edit(cx)
    }

    pub fn has_image_editor(&self) -> bool {
        self.image_editor.is_some() || self.image_editor_request.is_some()
    }

    /// True only when a location/alternative-text field owns native input.
    /// Merely showing the inspector must not claim the neighboring Source pane.
    pub fn image_input_is_focused(&self, window: &Window, cx: &App) -> bool {
        self.image_editor
            .as_ref()
            .is_some_and(|editor| editor.read(cx).input_is_focused(window, cx))
    }

    /// Capture native field ownership before focus moves to another tab or pane.
    pub fn remember_image_input_owner(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.image_editor.clone() {
            editor.update(cx, |editor, cx| editor.remember_input_owner(window, cx));
        }
    }

    /// Restore the actual input owner, including an uncommitted inspector field.
    pub fn focus_current_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.image_editor.clone() {
            editor.update(cx, |editor, cx| editor.focus_current_input(window, cx));
        } else {
            self.focus_handle.focus(window, cx);
        }
    }

    /// Route app-level paste to an inspector field without touching body history.
    pub fn paste_image_text(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(editor) = self.image_editor.clone() {
            if !editor.read(cx).input_is_focused(window, cx) {
                return false;
            }
            editor.update(cx, |editor, cx| editor.paste_text(text, window, cx));
            return true;
        }
        if self.focus_handle.is_focused(window) {
            if let Some(request) = self.image_editor_request.as_mut() {
                request.url.insert_str(0, text);
                cx.notify();
                return true;
            }
        }
        false
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_image_focused_field(&self, window: &Window, cx: &App) -> Option<&'static str> {
        self.image_editor
            .as_ref()
            .and_then(|editor| editor.read(cx).test_focused_field(window, cx))
    }

    pub(super) fn close_image_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.image_editor = None;
        self.image_editor_request = None;
        self.image_editor_recovery = None;
        self.image_editor_bounds = None;
        self.focus_handle.focus(window, cx);
        self.reset_blink(cx);
        cx.notify();
    }

    pub(super) fn update_image_recovery_fields(
        &mut self,
        url: String,
        alt: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(target) = self.image_editor_recovery.as_mut() {
            target.url = url;
            target.alt = alt;
            cx.notify();
        }
    }

    pub(super) fn apply_image_panel(
        &mut self,
        target: &ImageEditTarget,
        alt: &str,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.document.read(cx).buffer.content() != target.source {
            return Err("The document changed while this image editor was open. Cancel and reopen the image to avoid replacing another edit.".into());
        }
        if target.existing {
            self.engine.sync(self.document.read(cx));
            if !image_exists_at_range(&self.engine.tree().blocks, &target.range) {
                return Err(
                    "This image is no longer editable Markdown. Edit its HTML in Source mode."
                        .into(),
                );
            }
            self.apply_rich(
                RichCommand::SetImage {
                    source_range: target.range.clone(),
                    alt: alt.to_owned(),
                    url: url.to_owned(),
                },
                cx,
            );
        } else {
            // The inspector itself must not intercept its atomic body insert.
            self.image_editor = None;
            self.apply_editor_command(
                EditorCommand::SetSelection {
                    start: target.range.start,
                    end: target.range.end,
                },
                cx,
            );
            self.apply_editor_command(EditorCommand::InsertText(markdown_image_text(alt, url)), cx);
        }
        self.close_image_panel(window, cx);
        Ok(())
    }

    #[cfg(feature = "gui-tests")]
    pub fn painted_image_editor_bounds(&self) -> Option<Bounds<Pixels>> {
        self.image_editor_bounds
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_first_image_hit_point(&self) -> Option<gpui::Point<Pixels>> {
        self.image_bounds.first().map(|(_, bounds)| {
            gpui::point(
                bounds.left() + px(8.).min(bounds.size.width / 2.),
                bounds.top() + px(8.).min(bounds.size.height / 2.),
            )
        })
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_image_action_bounds(&self, cx: &App) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
        self.image_editor.as_ref()?.read(cx).test_action_bounds()
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_first_image_bounds(&self) -> Option<Bounds<Pixels>> {
        self.image_bounds.first().map(|(_, bounds)| *bounds)
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_image_preview_status_bounds(
        &self,
        cx: &App,
    ) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
        self.image_editor
            .as_ref()?
            .read(cx)
            .test_preview_status_bounds()
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_image_scroll_state(&self, cx: &App) -> Option<ImageInspectorScrollState> {
        Some(self.image_editor.as_ref()?.read(cx).test_scroll_state())
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_image_editor_state(&self, cx: &App) -> Option<(String, String, bool)> {
        self.image_editor
            .as_ref()
            .map(|editor| editor.read(cx).test_state(cx))
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_local_images_ready(&self, cx: &App) -> bool {
        let base_dir = self
            .document
            .read(cx)
            .path
            .as_ref()
            .and_then(|path| path.parent());
        let paths = collect_local_image_paths(self.engine.tree(), base_dir);
        !paths.is_empty()
            && paths.iter().all(|path| {
                self.local_image_paths
                    .get(path)
                    .is_some_and(|approved| approved.is_file())
            })
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_set_image_fields(&mut self, alt: &str, url: &str, cx: &mut Context<Self>) {
        if let Some(editor) = self.image_editor.clone() {
            editor.update(cx, |editor, cx| editor.test_set_fields(alt, url, cx));
        }
    }

    #[cfg(feature = "gui-tests")]
    pub fn test_open_first_image(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        fn find(blocks: &[markrust_core::rich::Block]) -> Option<(Range<usize>, String, String)> {
            for block in blocks {
                for inline in &block.inlines {
                    if let markrust_core::rich::Inline::Image {
                        source_range,
                        alt,
                        url,
                        ..
                    } = inline
                    {
                        return Some((source_range.clone(), alt.clone(), url.clone()));
                    }
                }
                if let Some(image) = find(&block.children) {
                    return Some(image);
                }
            }
            None
        }
        self.engine.sync(self.document.read(cx));
        if let Some((range, alt, url)) = find(&self.engine.tree().blocks) {
            self.open_image_editor(range, &alt, &url, window, cx);
            true
        } else {
            false
        }
    }
}

impl WysiwygHost for RichEditorView {
    fn click_source(
        &mut self,
        source: usize,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_image_editor() {
            self.focus_current_input(window, cx);
            return;
        }
        self.commit_widget_edit(cx);
        self.vertical_preferred_x = None;
        self.is_selecting = true;
        self.focus_handle.focus(window, cx);
        self.move_to(source, extend, cx);
    }

    fn select_source_range(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_image_editor() {
            self.focus_current_input(window, cx);
            return;
        }
        self.commit_widget_edit(cx);
        self.vertical_preferred_x = None;
        self.is_selecting = false;
        self.focus_handle.focus(window, cx);
        self.apply_editor_command(
            EditorCommand::SetSelection {
                start: range.start,
                end: range.end,
            },
            cx,
        );
    }

    fn drag_source(&mut self, source: usize, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.vertical_preferred_x = None;
            self.move_to(source, true, cx);
        }
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        let was_selecting = self.is_selecting;
        self.is_selecting = false;
        if was_selecting {
            // Contextual controls return immediately on release rather than
            // waiting for the next caret blink to repaint the editor.
            cx.notify();
        }
    }

    fn selected_range(&self) -> Range<usize> {
        self.selected_range.clone()
    }

    fn shadow_selection(&self) -> Option<&crate::shadow::ShadowSelection> {
        RichEditorView::shadow_selection(self)
    }

    fn search_highlights(&self) -> Option<&crate::search::SearchHighlights> {
        RichEditorView::search_highlights(self)
    }

    fn search_reveal_offset(&self) -> Option<usize> {
        self.pending_search_reveal
    }

    fn report_search_match_bounds(
        &mut self,
        offset: usize,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.pending_search_reveal != Some(offset) {
            return;
        }
        if let Some(delta) = caret_vertical_reveal_delta(bounds, self.list_state.viewport_bounds())
        {
            if delta.abs() > 0.5 {
                self.list_state.scroll_by(px(delta));
                cx.notify();
            } else {
                self.pending_search_reveal = None;
            }
        }
    }

    fn caret_offset(&self) -> usize {
        self.cursor_offset()
    }

    fn caret_visible(&self) -> bool {
        self.cursor_visible
    }

    fn is_selecting(&self) -> bool {
        self.is_selecting
    }

    fn focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    fn widget_editing(&self) -> bool {
        !matches!(self.widget_edit, WidgetEdit::Idle)
    }

    fn editing_context_enabled(&self) -> bool {
        self.markup_hints_enabled
    }

    fn input_focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn toggle_task(&mut self, id: NodeId, cx: &mut Context<Self>) {
        let checked = self.engine.block(id).and_then(|block| match block.kind {
            markrust_core::rich::BlockKind::ListItem { task: Some(c) } => Some(!c),
            _ => None,
        });
        if let Some(checked) = checked {
            self.apply_rich(RichCommand::SetTaskChecked { id, checked }, cx);
        }
    }

    fn edit_code_info(&mut self, id: NodeId, cx: &mut Context<Self>) {
        if self.has_image_editor() {
            return;
        }
        if !self.finish_widget_before_switch(cx) {
            return;
        }
        let source = self.document.read(cx).buffer.content();
        self.engine.sync(self.document.read(cx));
        let target = self.engine.block(id).and_then(|b| match &b.kind {
            markrust_core::rich::BlockKind::CodeBlock { info, .. } => {
                Some((b.source_range.clone(), info.clone()))
            }
            _ => None,
        });
        let draft = target
            .as_ref()
            .map(|(_, info)| info.clone())
            .unwrap_or_default();
        let caret = draft.len();
        self.widget_edit = WidgetEdit::CodeInfo { id, draft, caret };
        if let Some((range, _)) = target {
            self.remember_widget_recovery_anchor(WidgetDraftKind::CodeInfo, range, source);
        } else {
            self.widget_recovery_anchor = None;
        }
        self.widget_anchor = caret;
        self.widget_selecting = false;
        self.widget_preedit = None;
        self.snapshot = None;
        cx.notify();
    }

    fn edit_image_alt(&mut self, source_range: Range<usize>, alt: &str, cx: &mut Context<Self>) {
        if !self.finish_widget_before_switch(cx) {
            return;
        }
        let source = self.document.read(cx).buffer.content();
        self.widget_edit = WidgetEdit::ImageAlt {
            range: source_range.clone(),
            draft: alt.to_string(),
            caret: alt.len(),
        };
        self.remember_widget_recovery_anchor(WidgetDraftKind::ImageAlt, source_range, source);
        self.widget_anchor = alt.len();
        self.widget_selecting = false;
        self.widget_preedit = None;
        self.snapshot = None;
        cx.notify();
    }

    fn open_image_editor(
        &mut self,
        source_range: Range<usize>,
        alt: &str,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_image_editor() {
            // Selecting another image must not silently discard this draft.
            self.focus_current_input(window, cx);
            return;
        }
        if !self.finish_widget_before_switch(cx) {
            return;
        }
        self.is_selecting = false;
        self.select_source_range(source_range.clone(), window, cx);
        self.image_editor_request = Some(ImageEditTarget {
            range: source_range,
            source: self.document.read(cx).buffer.content(),
            existing: true,
            alt: alt.to_owned(),
            url: url.to_owned(),
        });
        cx.notify();
    }

    fn edit_frontmatter_field(&mut self, key: &'static str, current: &str, cx: &mut Context<Self>) {
        if !self.finish_widget_before_switch(cx) {
            return;
        }
        let source = self.document.read(cx).buffer.content();
        self.widget_edit = WidgetEdit::Frontmatter {
            key,
            draft: current.to_string(),
            caret: current.len(),
        };
        if let (Some(field), Some(range)) =
            (FrontmatterField::from_key(key), frontmatter_range(&source))
        {
            self.remember_widget_recovery_anchor(
                WidgetDraftKind::FrontmatterField(field),
                range,
                source,
            );
        } else {
            self.widget_recovery_anchor = None;
        }
        self.widget_anchor = current.len();
        self.widget_selecting = false;
        self.widget_preedit = None;
        self.frontmatter_error = None;
        self.snapshot = None;
        cx.notify();
    }

    fn edit_frontmatter_yaml(&mut self, current: &str, cx: &mut Context<Self>) {
        if !self.finish_widget_before_switch(cx) {
            return;
        }
        let source = self.document.read(cx).buffer.content();
        self.widget_edit = WidgetEdit::FrontmatterYaml {
            draft: current.to_string(),
            caret: current.len(),
        };
        if let Some(range) = frontmatter_range(&source) {
            self.remember_widget_recovery_anchor(WidgetDraftKind::FrontmatterYaml, range, source);
        } else {
            self.widget_recovery_anchor = None;
        }
        self.widget_anchor = current.len();
        self.widget_selecting = false;
        self.widget_preedit = None;
        self.frontmatter_error = None;
        self.snapshot = None;
        cx.notify();
    }

    fn open_table_menu(&mut self, source: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_image_editor() {
            self.focus_current_input(window, cx);
            return;
        }
        if !self.finish_widget_before_switch(cx) {
            return;
        }
        self.vertical_preferred_x = None;
        self.focus_handle.focus(window, cx);
        self.move_to(source, false, cx);
        cx.notify();
    }

    fn finish_widget(&mut self, cx: &mut Context<Self>) {
        self.commit_widget_edit(cx);
    }

    fn report_widget_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.ime.report_widget(bounds);
    }

    fn report_widget_caret(&mut self, caret: Bounds<Pixels>) {
        self.ime.report_widget_caret(caret);
    }

    fn ensure_widget_caret_visible(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.widget_edit, WidgetEdit::LinkDestination { .. }) {
            return;
        }
        let Some(caret) = self.ime.painted_caret_rect() else {
            return;
        };
        let viewport = self.link_scroll.bounds();
        let offset = self.link_scroll.offset();
        let next =
            link_caret_scroll_offset(offset.x, viewport, caret, self.link_scroll.max_offset().x);
        if next != offset.x {
            self.link_scroll.set_offset(point(next, offset.y));
            cx.notify();
        }
    }

    fn overlay_preedit(&self) -> Option<&str> {
        self.widget_preedit.as_deref()
    }

    fn widget_caret_offset(&self) -> usize {
        self.widget_edit.caret()
    }

    fn widget_sel(&self) -> Range<usize> {
        let c = self.widget_edit.caret();
        let a = self.widget_anchor.min(c);
        let b = self.widget_anchor.max(c);
        a..b
    }

    fn is_widget_selecting(&self) -> bool {
        self.widget_selecting
    }

    fn click_overlay(
        &mut self,
        target: OverlayTarget,
        offset: usize,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_image_editor() {
            self.focus_current_input(window, cx);
            return;
        }
        self.focus_handle.focus(window, cx);
        if !self.widget_edit.matches_overlay(&target) {
            match &target {
                OverlayTarget::CodeInfo(id) => self.edit_code_info(*id, cx),
                OverlayTarget::ImageAlt { range, stored } => {
                    self.edit_image_alt(range.clone(), stored, cx);
                }
                OverlayTarget::Frontmatter { key, stored } => {
                    self.edit_frontmatter_field(key, stored, cx);
                }
                OverlayTarget::FrontmatterYaml { stored } => {
                    self.edit_frontmatter_yaml(stored, cx);
                }
                OverlayTarget::LinkDestination { .. } => return,
            }
        }
        let at = offset;
        if extend {
            self.widget_edit.set_caret(at);
        } else {
            self.widget_edit.set_caret(at);
            self.widget_anchor = self.widget_edit.caret();
        }
        self.widget_selecting = true;
        self.reset_blink(cx);
        self.snapshot = None;
        cx.notify();
    }

    fn drag_overlay(&mut self, offset: usize, cx: &mut Context<Self>) {
        if !self.widget_selecting {
            return;
        }
        self.widget_edit.set_caret(offset);
        self.reset_blink(cx);
        self.snapshot = None;
        cx.notify();
    }

    fn end_overlay_drag(&mut self, cx: &mut Context<Self>) {
        let was_selecting = self.widget_selecting;
        self.widget_selecting = false;
        if was_selecting {
            cx.notify();
        }
    }

    fn preedit(&self) -> Option<&str> {
        if matches!(self.widget_edit, WidgetEdit::Idle) {
            self.preedit.as_deref()
        } else {
            None
        }
    }

    fn report_leaf(
        &mut self,
        layout: Arc<LeafLayout>,
        element_bounds: Bounds<Pixels>,
        font_size: f32,
        line_height: f32,
        caret_bounds: Option<Bounds<Pixels>>,
        visual_lines: Vec<VisualLine>,
    ) {
        self.ime.report_visual_lines(layout.clone(), visual_lines);
        self.ime.report_leaf(ImeLeafHit {
            layout,
            bounds: element_bounds,
            font_size,
            line_height,
            caret_bounds,
        });
    }

    fn report_body_caret_prepaint(&mut self, caret: Bounds<Pixels>, source_span: usize) {
        // Match IME ownership at shared block boundaries: the narrowest leaf
        // wins, rather than whichever neighboring block prepainted last.
        if self
            .table_toolbar_caret
            .is_none_or(|(_, previous_span)| source_span < previous_span)
        {
            self.table_toolbar_caret = Some((caret, source_span));
        }
    }

    fn ensure_pending_caret_visible(&mut self, cx: &mut Context<Self>) {
        if let Some(caret) = self.ime.focused_leaf().and_then(|leaf| leaf.caret_bounds) {
            self.adjust_scroll_to_painted_caret(caret, cx);
        }
    }

    fn report_painted_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.ime.report_painted_bounds(bounds);
    }

    fn report_image_bounds(&mut self, range: Range<usize>, bounds: Bounds<Pixels>) {
        #[cfg(feature = "gui-tests")]
        self.image_bounds.push((range, bounds));
        #[cfg(not(feature = "gui-tests"))]
        let _ = (range, bounds);
    }

    fn sync_ime_cursor(&mut self, window: &mut Window) {
        // GPUI: invalidate_character_coordinates → next frame selected_bounds
        // → PlatformWindow::update_ime_position. On macOS that call discards
        // the Bounds and invalidates; AppKit then pulls bounds_for_range
        // (firstRectForCharacterRange:). TestWindow swallows the push;
        // take_platform_push is the in-repo record of the request.
        if self.ime.take_platform_push().is_some() {
            window.invalidate_character_coordinates();
        }
    }
}

impl Focusable for RichEditorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for RichEditorView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if let Some((draft, preedit)) = self.widget_display() {
            let content = format!("{}{}", draft, preedit.unwrap_or_default());
            let start = Self::offset_from_utf16(&content, range_utf16.start);
            let end = Self::offset_from_utf16(&content, range_utf16.end);
            actual_range.replace(
                Self::offset_to_utf16(&content, start)..Self::offset_to_utf16(&content, end),
            );
            return Some(content.get(start..end).unwrap_or("").to_string());
        }
        let content = self.document.read(cx).buffer.content();
        let start = Self::offset_from_utf16(&content, range_utf16.start);
        let end = Self::offset_from_utf16(&content, range_utf16.end);
        actual_range
            .replace(Self::offset_to_utf16(&content, start)..Self::offset_to_utf16(&content, end));
        Some(content.get(start..end).unwrap_or("").to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if let Some((draft, preedit)) = self.widget_display() {
            return Some(super::ime::widget_selected_text_range(
                &draft,
                self.widget_edit.caret(),
                self.widget_anchor,
                preedit.as_deref(),
            ));
        }
        let content = self.document.read(cx).buffer.content();
        Some(super::ime::body_selected_text_range(
            &content,
            self.selected_range.clone(),
            self.selection_reversed,
        ))
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range.clone()
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        super::ime::clear_composition(
            &mut self.preedit,
            &mut self.widget_preedit,
            &mut self.marked_range,
        );
        self.clear_body_composition_anchor();
        self.reset_blink(cx);
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.widget_edit, WidgetEdit::Idle) {
            self.reset_blink(cx);
            self.record_widget_edit();
            self.widget_preedit = None;
            let sel = self.widget_sel();
            if let Some((draft, caret)) = self.widget_edit.draft_caret_mut() {
                super::ime::replace_in_widget_draft(draft, caret, range_utf16, new_text, sel);
            }
            self.widget_anchor = self.widget_edit.caret();
            self.snapshot = None;
            cx.notify();
            return;
        }
        let content = self.document.read(cx).buffer.content();
        super::ime::apply_replace_range_to_selection(
            &content,
            range_utf16,
            &mut self.marked_range,
            &mut self.selected_range,
            &mut self.selection_reversed,
        );
        self.preedit = None;
        self.clear_body_composition_anchor();
        self.apply_rich(RichCommand::InsertText(new_text.to_string()), cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reset_blink(cx);
        if !matches!(self.widget_edit, WidgetEdit::Idle) {
            super::ime::set_preedit(&mut self.widget_preedit, new_text);
            self.snapshot = None;
            cx.notify();
            return;
        }
        // Preedit is display-only; the model is untouched until commit.
        let caret = self.cursor_offset();
        if new_text.is_empty() {
            self.clear_body_composition_anchor();
        } else if !self
            .widget_recovery_anchor
            .as_ref()
            .is_some_and(|anchor| anchor.kind == WidgetDraftKind::BodyComposition)
        {
            self.remember_body_composition_anchor(self.document.read(cx).buffer.content(), caret);
        }
        super::ime::begin_body_preedit(&mut self.preedit, &mut self.marked_range, new_text, caret);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(super::ime::ime_origin_bounds(&self.ime, bounds))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        if self.ime.widget_focused() {
            if let Some((draft, preedit)) = self.widget_display() {
                let content = format!("{}{}", draft, preedit.unwrap_or_default());
                return Some(Self::offset_to_utf16(
                    &content,
                    self.widget_edit.caret().min(content.len()),
                ));
            }
        }
        let theme = self.theme.clone();
        let leaf = self.ime.leaf_at_point(point);
        if let Some(leaf) = leaf {
            let layout = leaf.layout.clone();
            let bounds = leaf.bounds;
            let font_size = leaf.font_size;
            let line_height = leaf.line_height;
            let vis = hit_test_leaf(
                &layout,
                bounds,
                point,
                window,
                font_size,
                line_height,
                &theme,
            );
            let src = layout.source_for_visible(vis);
            let content = self.document.read(cx).buffer.content();
            return Some(Self::offset_to_utf16(&content, src));
        }
        if self.ime.point_is_below_painted_content(point) {
            self.engine.sync(self.document.read(cx));
            let src = caret_for_click_below_content(self.engine.tree());
            let content = self.document.read(cx).buffer.content();
            return Some(Self::offset_to_utf16(&content, src));
        }
        None
    }
}

impl Render for RichEditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(target) = self.image_editor_request.take() {
            self.image_editor_recovery = Some(target.clone());
            let owner = cx.entity().downgrade();
            let base_dir = self
                .document
                .read(cx)
                .path
                .as_ref()
                .and_then(|path| path.parent())
                .map(std::path::Path::to_path_buf);
            let remote = self.remote_images_authorized;
            let theme = self.theme.clone();
            self.image_editor =
                Some(cx.new(|cx| {
                    ImageEditor::new(owner, target, base_dir, remote, theme, window, cx)
                }));
        }
        self.image_editor_bounds = None;
        #[cfg(feature = "gui-tests")]
        self.image_bounds.clear();
        self.table_toolbar_caret = None;
        self.table_toolbar_bounds = None;
        #[cfg(feature = "gui-tests")]
        {
            self.table_button_bounds = [None; 6];
            self.markup_hint_bounds = None;
        }
        self.link_editor_bounds = None;
        let composing = if matches!(self.widget_edit, WidgetEdit::Idle) {
            self.preedit.as_deref()
        } else {
            self.widget_preedit.as_deref()
        };
        self.ime.begin_frame(
            !matches!(self.widget_edit, WidgetEdit::Idle),
            if matches!(self.widget_edit, WidgetEdit::Idle) {
                self.cursor_offset()
            } else {
                self.widget_edit.caret()
            },
            composing,
        );
        let (snapshot, caret_item_was_visible) = self.sync_snapshot(cx);
        self.reveal_caret_item(caret_item_was_visible, cx);
        let theme = self.theme.clone();
        let editor = cx.entity();
        let focus = self.focus_handle.clone();
        let fm_info = markrust_core::parse_frontmatter(&snapshot.source);
        let editing_fm = match &self.widget_edit {
            WidgetEdit::Frontmatter { key, draft, .. } => Some((*key, draft.clone())),
            _ => None,
        };
        let editing_yaml = match &self.widget_edit {
            WidgetEdit::FrontmatterYaml { draft, .. } => Some(draft.clone()),
            _ => None,
        };
        let frontmatter_error = self.frontmatter_error.clone();
        let editing_link = matches!(self.widget_edit, WidgetEdit::LinkDestination { .. });
        let editing_image = self.image_editor.is_some();
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme.editor_bg)
            .key_context("RichEditor")
            .track_focus(&focus)
            .cursor(CursorStyle::IBeam)
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Backspace, window, cx| {
                    editor.update(cx, |e, cx| {
                        if e.apply_editor_command(EditorCommand::Backspace, cx)
                            == EditorOutcome::Noop
                        {
                            window.play_system_bell();
                        }
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Delete, window, cx| {
                    editor.update(cx, |e, cx| {
                        if e.apply_editor_command(EditorCommand::Delete, cx) == EditorOutcome::Noop
                        {
                            window.play_system_bell();
                        }
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::DeleteWordLeft, window, cx| {
                    editor.update(cx, |e, cx| {
                        if e.apply_editor_command(EditorCommand::DeleteWordLeft, cx)
                            == EditorOutcome::Noop
                        {
                            window.play_system_bell();
                        }
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::DeleteWordRight, window, cx| {
                    editor.update(cx, |e, cx| {
                        if e.apply_editor_command(EditorCommand::DeleteWordRight, cx)
                            == EditorOutcome::Noop
                        {
                            window.play_system_bell();
                        }
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::DeleteToLineStart, window, cx| {
                    editor.update(cx, |e, cx| {
                        if e.apply_editor_command(EditorCommand::DeleteToLineStart, cx)
                            == EditorOutcome::Noop
                        {
                            window.play_system_bell();
                        }
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::DeleteToLineEnd, window, cx| {
                    editor.update(cx, |e, cx| {
                        if e.apply_editor_command(EditorCommand::DeleteToLineEnd, cx)
                            == EditorOutcome::Noop
                        {
                            window.play_system_bell();
                        }
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Left, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::Left), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Right, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::Right), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Up, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::Up), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Down, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::Down), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectLeft, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::Left), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectRight, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::Right), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectUp, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::Up), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectDown, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::Down), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Home, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::Home), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::End, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::End), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectHome, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::Home), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectEnd, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::End), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::PageUp, window, cx| {
                    editor.update(cx, |e, cx| {
                        let lines =
                            (window.bounds().size.height / window.line_height()).floor() as i32;
                        e.apply_editor_command(
                            EditorCommand::Move(CaretMove::Vertical {
                                delta_lines: -lines.max(1),
                            }),
                            cx,
                        );
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::PageDown, window, cx| {
                    editor.update(cx, |e, cx| {
                        let lines =
                            (window.bounds().size.height / window.line_height()).floor() as i32;
                        e.apply_editor_command(
                            EditorCommand::Move(CaretMove::Vertical {
                                delta_lines: lines.max(1),
                            }),
                            cx,
                        );
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectPageUp, window, cx| {
                    editor.update(cx, |e, cx| {
                        let lines =
                            (window.bounds().size.height / window.line_height()).floor() as i32;
                        e.apply_editor_command(
                            EditorCommand::Select(CaretMove::Vertical {
                                delta_lines: -lines.max(1),
                            }),
                            cx,
                        );
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectPageDown, window, cx| {
                    editor.update(cx, |e, cx| {
                        let lines =
                            (window.bounds().size.height / window.line_height()).floor() as i32;
                        e.apply_editor_command(
                            EditorCommand::Select(CaretMove::Vertical {
                                delta_lines: lines.max(1),
                            }),
                            cx,
                        );
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::WordLeft, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::WordLeft), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::WordRight, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::WordRight), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectWordLeft, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::WordLeft), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectWordRight, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::WordRight), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::DocumentHome, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::DocumentHome), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::DocumentEnd, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Move(CaretMove::DocumentEnd), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectDocumentHome, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::DocumentHome), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectDocumentEnd, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Select(CaretMove::DocumentEnd), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::SelectAll, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::SelectAll, cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Copy, _, cx| {
                    editor.update(cx, |e, cx| e.copy_selection(cx));
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Cut, _, cx| {
                    editor.update(cx, |e, cx| e.cut_selection(cx));
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Enter, _, cx| {
                    editor.update(cx, |e, cx| {
                        if !e.consume_widget_newline(cx) {
                            // SplitBlock owns table Enter (`<br>`) vs paragraph split.
                            e.apply_rich(RichCommand::SplitBlock, cx);
                        }
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::InsertLineBreak, _, cx| {
                    editor.update(cx, |e, cx| {
                        if !e.consume_widget_newline(cx) {
                            e.apply_rich(RichCommand::InsertLineBreak, cx);
                        }
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Escape, _, cx| {
                    editor.update(cx, |e, cx| {
                        let _ = e.cancel_widget_edit(cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::ToggleBold, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_rich(RichCommand::ToggleMark(MarkSet::BOLD), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::ToggleItalic, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_rich(RichCommand::ToggleMark(MarkSet::ITALIC), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::ToggleCode, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_rich(RichCommand::ToggleMark(MarkSet::CODE), cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::ToggleLink, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_rich(RichCommand::ToggleLink, cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Indent, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Indent, cx);
                    });
                }
            })
            .on_action({
                let editor = editor.clone();
                move |_: &crate::editor::Outdent, _, cx| {
                    editor.update(cx, |e, cx| {
                        e.apply_editor_command(EditorCommand::Outdent, cx);
                    });
                }
            })
            .children(fm_info.map(|info| {
                frontmatter_panel(
                    editor.clone(),
                    &theme,
                    &info,
                    editing_fm.as_ref().map(|(k, d)| (*k, d.as_str())),
                    editing_yaml.as_deref(),
                    frontmatter_error.as_deref(),
                )
            }))
            .child({
                let editor = editor.clone();
                let catcher = editor.clone();
                div()
                    .id("wysiwyg-body")
                    .flex_1()
                    .size_full()
                    .relative()
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, cx| {
                                let editor = catcher.clone();
                                // Markdown parsing can omit trailing whitespace
                                // and other source positions from every painted
                                // leaf. Text input must remain registered there
                                // (and while the caret's block is offscreen).
                                // This canvas paints before the body leaves, so
                                // a precise leaf handler takes precedence later.
                                // An active frontmatter widget may have already
                                // painted above the body; preserve its handler.
                                let view = editor.read(cx);
                                if matches!(view.widget_edit, WidgetEdit::Idle) {
                                    window.handle_input(
                                        &view.focus_handle,
                                        ElementInputHandler::new(bounds, editor.clone()),
                                        cx,
                                    );
                                }
                                #[cfg(feature = "gui-tests")]
                                editor.update(cx, |view, _cx| {
                                    view.visual_test_bounds = Some(bounds);
                                });
                                window.on_mouse_event({
                                    let editor = editor.clone();
                                    move |event: &MouseDownEvent, phase, window, cx| {
                                        if !phase.bubble() || event.button != MouseButton::Left {
                                            return;
                                        }
                                        if !bounds.contains(&event.position) {
                                            return;
                                        }
                                        let placed = editor.update(cx, |view, cx| {
                                            // Exact text leaves and interactive
                                            // chrome own their pointer presses.
                                            // Only genuine document padding
                                            // uses the nearest painted row.
                                            if view.ime.leaf_at_point(event.position).is_some()
                                                || view
                                                    .ime
                                                    .point_is_reserved_surface(event.position)
                                                || view.table_toolbar_bounds.is_some_and(|bounds| {
                                                    bounds.contains(&event.position)
                                                })
                                                || view.link_editor_bounds.is_some_and(|bounds| {
                                                    bounds.contains(&event.position)
                                                })
                                                || view.image_editor_bounds.is_some_and(|bounds| {
                                                    bounds.contains(&event.position)
                                                })
                                            {
                                                return false;
                                            }
                                            if view.click_below_painted_content(
                                                event.position,
                                                event.modifiers.shift,
                                                window,
                                                cx,
                                            ) {
                                                return true;
                                            }
                                            if let Some(source) =
                                                view.ime.source_near_point(event.position)
                                            {
                                                view.click_source(
                                                    source,
                                                    event.modifiers.shift,
                                                    window,
                                                    cx,
                                                );
                                                return true;
                                            }
                                            false
                                        });
                                        if placed {
                                            window.prevent_default();
                                            cx.stop_propagation();
                                        }
                                    }
                                });
                                window.on_mouse_event({
                                    let editor = editor.clone();
                                    move |event: &MouseMoveEvent, phase, window, cx| {
                                        if !phase.bubble() {
                                            return;
                                        }
                                        if !bounds.contains(&event.position) {
                                            return;
                                        }
                                        editor.update(cx, |view, cx| {
                                            if !view.is_selecting
                                                || !view.focus_handle.is_focused(window)
                                                || !matches!(view.widget_edit, WidgetEdit::Idle)
                                            {
                                                return;
                                            }
                                            if !event
                                                .pressed_button
                                                .is_some_and(|b| b == MouseButton::Left)
                                            {
                                                return;
                                            }
                                            // Exact leaf hits are owned by the
                                            // leaf's shaped-text handler. Only
                                            // whitespace gaps use this fallback.
                                            if view.ime.leaf_at_point(event.position).is_some() {
                                                return;
                                            }
                                            let source = if view
                                                .ime
                                                .point_is_below_painted_content(event.position)
                                            {
                                                view.engine.sync(view.document.read(cx));
                                                Some(caret_for_click_below_content(
                                                    view.engine.tree(),
                                                ))
                                            } else {
                                                view.ime.source_near_point(event.position)
                                            };
                                            if let Some(source) = source {
                                                view.drag_source(source, cx);
                                            }
                                        });
                                    }
                                });
                                window.on_mouse_event({
                                    let editor = editor.clone();
                                    let had_body_drag = Rc::new(Cell::new(false));
                                    move |event: &MouseUpEvent, phase, window, cx| {
                                        if event.button != MouseButton::Left {
                                            return;
                                        }
                                        if phase.capture() {
                                            let view = editor.read(cx);
                                            // Controls may call click_source
                                            // during their Click callback. Such
                                            // command navigation is not a drag
                                            // initiated by this pointer press.
                                            had_body_drag.set(
                                                view.is_selecting
                                                    && view.focus_handle.is_focused(window)
                                                    && matches!(view.widget_edit, WidgetEdit::Idle),
                                            );
                                            return;
                                        }
                                        if !phase.bubble() {
                                            return;
                                        }
                                        let was_dragging = had_body_drag.replace(false);
                                        let handled = editor.update(cx, |view, cx| {
                                            if view.focus_handle.is_focused(window) {
                                                if was_dragging
                                                    && matches!(view.widget_edit, WidgetEdit::Idle)
                                                {
                                                    let viewport =
                                                        view.list_state.viewport_bounds();
                                                    if viewport.size.width > px(0.)
                                                        && viewport.size.height > px(0.)
                                                    {
                                                        let release = point(
                                                            event.position.x.clamp(
                                                                viewport.left(),
                                                                viewport.right(),
                                                            ),
                                                            event.position.y.clamp(
                                                                viewport.top(),
                                                                viewport.bottom(),
                                                            ),
                                                        );
                                                        // A press below EOF can
                                                        // create a trailing blank
                                                        // before another paint.
                                                        // Its release must use
                                                        // the current document's
                                                        // terminal caret rather
                                                        // than the old last row.
                                                        let source = if view
                                                            .ime
                                                            .point_is_below_painted_content(release)
                                                        {
                                                            view.engine
                                                                .sync(view.document.read(cx));
                                                            Some(caret_for_click_below_content(
                                                                view.engine.tree(),
                                                            ))
                                                        } else {
                                                            view.ime.source_near_point(release)
                                                        };
                                                        if let Some(source) = source {
                                                            view.drag_source(source, cx);
                                                        }
                                                    }
                                                }
                                                view.end_drag(cx);
                                                return was_dragging;
                                            }
                                            false
                                        });
                                        if handled {
                                            window.prevent_default();
                                            cx.stop_propagation();
                                        }
                                    }
                                });
                            },
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .child(
                        list(self.list_state.clone(), move |index, _window, _cx| {
                            render_top_block(&snapshot, index, editor.clone())
                        })
                        .flex_1()
                        .size_full()
                        .py(px(16.)),
                    )
                    .when(editing_link, |body| {
                        body.child(floating_link_editor(cx.entity(), theme.clone()))
                    })
                    .when(!editing_link && !editing_image, |body| {
                        body.child(floating_table_toolbar(cx.entity(), theme.clone()))
                            .child(floating_markup_hint(cx.entity(), theme.clone()))
                    })
                    .when(editing_image, |body| {
                        body.child(floating_image_editor(cx.entity()))
                    })
            })
    }
}

#[cfg(test)]
fn widget_draft_with_caret(draft: &str, caret: usize, preedit: &str) -> String {
    let mut at = caret.min(draft.len());
    if !draft.is_char_boundary(at) {
        at = draft.len();
    }
    format!("{}{}|{}", &draft[..at], preedit, &draft[at..])
}

fn frontmatter_panel(
    editor: gpui::Entity<RichEditorView>,
    theme: &crate::theme::EditorTheme,
    info: &markrust_core::FrontmatterInfo,
    editing_fm: Option<(&'static str, &str)>,
    editing_yaml: Option<&str>,
    error: Option<&str>,
) -> gpui::AnyElement {
    let field = |key: &'static str, placeholder: &str, stored: Option<&str>| -> String {
        match editing_fm {
            Some((k, draft)) if k == key => draft.to_string(),
            _ => stored
                .filter(|s| !s.is_empty())
                .unwrap_or(placeholder)
                .to_string(),
        }
    };
    let title_value = field("title", "Add a title", info.title.as_deref());
    let desc_value = field(
        "description",
        "Add a description",
        info.description.as_deref(),
    );
    let tags_value = field("tags", "Add tags", info.tags.as_deref());
    let yaml_editing = editing_yaml.is_some();
    let yaml_value = match editing_yaml {
        Some(draft) => draft.to_string(),
        None => {
            let body = info.yaml_body.trim();
            if body.is_empty() {
                "Add YAML".to_string()
            } else {
                body.to_string()
            }
        }
    };
    div()
        .id("wysiwyg-frontmatter")
        .px(px(24.))
        .pt(px(12.))
        .child(
            div()
                .px(px(12.))
                .py(px(8.))
                .rounded_md()
                .border_1()
                .border_color(theme.separator)
                .bg(theme.sidebar_bg)
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.secondary_text)
                        .child("Frontmatter"),
                )
                .when_some(error, |panel, message| {
                    panel.child(
                        div()
                            .text_xs()
                            .text_color(theme.accent)
                            .child(message.to_string()),
                    )
                })
                .child(frontmatter_field_row(FmField {
                    editor: editor.clone(),
                    theme,
                    id: "fm-title",
                    key: "title",
                    label: "Title",
                    display: title_value,
                    current: info.title.clone().unwrap_or_default(),
                    editing: matches!(editing_fm, Some(("title", _))),
                    color: theme.frontmatter_text,
                }))
                .child(frontmatter_field_row(FmField {
                    editor: editor.clone(),
                    theme,
                    id: "fm-description",
                    key: "description",
                    label: "Description",
                    display: desc_value,
                    current: info.description.clone().unwrap_or_default(),
                    editing: matches!(editing_fm, Some(("description", _))),
                    color: theme.secondary_text,
                }))
                .child(frontmatter_field_row(FmField {
                    editor: editor.clone(),
                    theme,
                    id: "fm-tags",
                    key: "tags",
                    label: "Tags",
                    display: tags_value,
                    current: info.tags.clone().unwrap_or_default(),
                    editing: matches!(editing_fm, Some(("tags", _))),
                    color: theme.secondary_text,
                }))
                .child(frontmatter_yaml_row(
                    editor,
                    theme,
                    yaml_value,
                    info.yaml_body.clone(),
                    yaml_editing,
                )),
        )
        .into_any_element()
}

struct FmField<'a> {
    editor: gpui::Entity<RichEditorView>,
    theme: &'a crate::theme::EditorTheme,
    id: &'static str,
    key: &'static str,
    label: &'static str,
    display: String,
    current: String,
    editing: bool,
    color: gpui::Hsla,
}

fn frontmatter_field_row(field: FmField<'_>) -> gpui::AnyElement {
    let FmField {
        editor,
        theme,
        id,
        key,
        label,
        display,
        current,
        editing,
        color,
    } = field;
    let editor_away = editor.clone();
    let font_size = theme.font_size * 0.875;
    let line_height = theme.line_height_for_font_size(font_size);
    div()
        .id(id)
        .relative()
        .cursor(CursorStyle::PointingHand)
        .when(editing, |el| {
            el.border_b_1()
                .border_color(theme.accent)
                .on_mouse_down_out(move |_, _, cx| {
                    editor_away.update(cx, |host, cx| host.finish_widget(cx));
                })
        })
        .child(WidgetOverlay {
            editor,
            prefix: format!("{label}: "),
            text: display,
            editing,
            font_size,
            line_height,
            theme: theme.clone(),
            color,
            italic: false,
            monospace: false,
            hug_width: false,
            single_line: false,
            target: OverlayTarget::Frontmatter {
                key,
                stored: current,
            },
        })
        .into_any_element()
}

fn frontmatter_yaml_row(
    editor: gpui::Entity<RichEditorView>,
    theme: &crate::theme::EditorTheme,
    display: String,
    current: String,
    editing: bool,
) -> gpui::AnyElement {
    let editor_away = editor.clone();
    let font_size = theme.font_size * 0.75;
    let line_height = theme.line_height_for_font_size(font_size);
    div()
        .id("fm-yaml")
        .relative()
        .mt(px(4.))
        .cursor(CursorStyle::PointingHand)
        .when(editing, |el| {
            el.border_b_1()
                .border_color(theme.accent)
                .on_mouse_down_out(move |_, _, cx| {
                    editor_away.update(cx, |host, cx| host.finish_widget(cx));
                })
        })
        .child(
            div()
                .text_xs()
                .text_color(theme.secondary_text)
                .font_family(theme.font_family.clone())
                .child("YAML"),
        )
        .child(WidgetOverlay {
            editor,
            prefix: String::new(),
            text: display,
            editing,
            font_size,
            line_height,
            theme: theme.clone(),
            color: theme.frontmatter_text,
            italic: false,
            monospace: true,
            hug_width: false,
            single_line: false,
            target: OverlayTarget::FrontmatterYaml { stored: current },
        })
        .into_any_element()
}

fn context_controls_eligible(
    focused: bool,
    collapsed: bool,
    widget_idle: bool,
    dragging: bool,
) -> bool {
    focused && collapsed && widget_idle && !dragging
}

/// Place paint-only chrome near the edited row without covering that row.
/// A small viewport may have no safe slot; menu commands remain available.
fn context_overlay_placement(
    viewport: Bounds<Pixels>,
    caret: Bounds<Pixels>,
    dimensions: gpui::Size<Pixels>,
) -> Option<Bounds<Pixels>> {
    let margin = px(8.);
    if !viewport.intersects(&caret)
        || dimensions.width + margin * 2. > viewport.size.width
        || dimensions.height + margin * 2. > viewport.size.height
    {
        return None;
    }
    let left = viewport.origin.x + margin;
    let right = viewport.right() - margin - dimensions.width;
    let top = viewport.origin.y + margin;
    let bottom = viewport.bottom() - margin - dimensions.height;
    let x = caret.origin.x.clamp(left, right);
    // The exclusion covers the entire active row, not only the caret quad.
    let editing_row = Bounds::new(
        point(viewport.origin.x, caret.origin.y - px(2.)),
        size(viewport.size.width, caret.size.height + px(4.)),
    );
    [
        point(x, caret.origin.y - margin - dimensions.height),
        point(x, caret.bottom() + margin),
        point(right, top),
        point(right, bottom),
    ]
    .into_iter()
    .map(|origin| Bounds::new(origin, dimensions))
    .find(|candidate| {
        candidate.origin.x >= left
            && candidate.right() <= viewport.right() - margin
            && candidate.origin.y >= top
            && candidate.bottom() <= viewport.bottom() - margin
            && !candidate.intersects(&editing_row)
    })
}

fn table_toolbar_dimensions(viewport: Bounds<Pixels>) -> (gpui::Size<Pixels>, usize) {
    let width = (viewport.size.width - px(16.)).min(px(344.));
    let columns = if width >= px(300.) {
        3
    } else if width >= px(196.) {
        2
    } else {
        1
    };
    let rows = 6 / columns;
    (
        size(width, px(14. + rows as f32 * 24. + (rows - 1) as f32 * 4.)),
        columns,
    )
}

fn floating_table_toolbar(editor: Entity<RichEditorView>, theme: EditorTheme) -> impl IntoElement {
    canvas(
        move |viewport, window, cx| {
            let view = editor.read(cx);
            if !context_controls_eligible(
                view.focus_handle.is_focused(window),
                view.selected_range.is_empty(),
                matches!(view.widget_edit, WidgetEdit::Idle),
                view.is_selecting || view.widget_selecting,
            ) {
                return None;
            }
            let source = view.document.read(cx).buffer.content();
            view.engine.cell_edit_range(view.cursor_offset(), &source)?;
            let caret = view.table_toolbar_caret.map(|(caret, _)| caret)?;
            let (dimensions, columns) = table_toolbar_dimensions(viewport);
            let bounds = context_overlay_placement(viewport, caret, dimensions)?;
            let mut overlay = table_toolbar(editor.clone(), &theme, columns).into_any_element();
            overlay.prepaint_as_root(
                bounds.origin,
                bounds.size.map(AvailableSpace::Definite),
                window,
                cx,
            );
            editor.update(cx, |view, _| view.table_toolbar_bounds = Some(bounds));
            Some(overlay)
        },
        |_, overlay, window, cx| {
            if let Some(mut overlay) = overlay {
                overlay.paint(window, cx);
            }
        },
    )
    .absolute()
    .inset_0()
}

fn table_toolbar(
    editor: Entity<RichEditorView>,
    theme: &EditorTheme,
    columns: usize,
) -> impl IntoElement {
    let items = [
        (
            "Row above",
            "tbl-row-above",
            RichCommand::InsertTableRow { after: false },
        ),
        (
            "Row below",
            "tbl-row-below",
            RichCommand::InsertTableRow { after: true },
        ),
        ("Delete row", "tbl-row-del", RichCommand::DeleteTableRow),
        (
            "Col left",
            "tbl-col-left",
            RichCommand::InsertTableColumn { after: false },
        ),
        (
            "Col right",
            "tbl-col-right",
            RichCommand::InsertTableColumn { after: true },
        ),
        ("Delete col", "tbl-col-del", RichCommand::DeleteTableColumn),
    ];
    div()
        .id("wysiwyg-table-toolbar")
        .accessibility_id("wysiwyg-table-toolbar")
        .role(Role::Group)
        .aria_label(theme.ui_text("Table controls"))
        .size_full()
        .p(px(6.))
        .rounded_md()
        .border_1()
        .border_color(theme.separator)
        .bg(theme.sidebar_bg)
        .cursor(CursorStyle::Arrow)
        .flex()
        .flex_col()
        .gap(px(4.))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .children(items.chunks(columns).enumerate().map(|(row_index, row)| {
            let line = div().w_full().flex().flex_row().gap(px(4.));
            #[cfg(feature = "gui-tests")]
            let line = {
                let observer = editor.clone();
                line.on_children_prepainted(move |bounds, _window, cx| {
                    observer.update(cx, |view, _| {
                        for (column, bounds) in bounds.into_iter().enumerate() {
                            view.table_button_bounds[row_index * columns + column] = Some(bounds);
                        }
                    });
                })
            };
            line.id(("wysiwyg-table-toolbar-row", row_index))
                .children(row.iter().map(|(label, id, command)| {
                    let editor = editor.clone();
                    let theme = theme.clone();
                    let command = command.clone();
                    let label: SharedString = theme.ui_text(label).into();
                    div()
                        .id(*id)
                        .accessibility_id(*id)
                        .role(Role::Button)
                        .aria_label(label.clone())
                        .flex_1()
                        .h(px(24.))
                        .px(px(4.))
                        .rounded_sm()
                        .text_xs()
                        .text_color(theme.text)
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor(CursorStyle::PointingHand)
                        .hover(move |style| style.bg(theme.sidebar_hover))
                        .child(label)
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            editor.update(cx, |view, cx| {
                                // Revalidate the live target instead of applying a
                                // stale overlay action to a different input owner.
                                let source = view.document.read(cx).buffer.content();
                                if !context_controls_eligible(
                                    view.focus_handle.is_focused(window),
                                    view.selected_range.is_empty(),
                                    matches!(view.widget_edit, WidgetEdit::Idle),
                                    view.is_selecting || view.widget_selecting,
                                ) || view
                                    .engine
                                    .cell_edit_range(view.cursor_offset(), &source)
                                    .is_none()
                                {
                                    return;
                                }
                                if view.apply_rich(command.clone(), cx) == RichOutcome::Noop {
                                    window.play_system_bell();
                                }
                                view.focus_handle.focus(window, cx);
                            });
                        })
                }))
        }))
}

/// Source syntax is explained in paint-only chrome rather than inserted into
/// the text projection. The badge has no hitbox, so it cannot block selection.
fn floating_markup_hint(editor: Entity<RichEditorView>, theme: EditorTheme) -> impl IntoElement {
    let paint_theme = theme.clone();
    canvas(
        move |viewport, window, cx| {
            let view = editor.read(cx);
            if !view.markup_hints_enabled
                || !context_controls_eligible(
                    view.focus_handle.is_focused(window),
                    view.selected_range.is_empty(),
                    matches!(view.widget_edit, WidgetEdit::Idle),
                    view.is_selecting || view.widget_selecting,
                )
            {
                return None;
            }
            // Table controls already identify their context. Do not stack an
            // explanatory badge on top of the structural action panel.
            if view.engine.in_table(view.cursor_offset()) {
                return None;
            }
            let label = view.editing_context_hint()?;
            let caret = view.table_toolbar_caret.map(|(caret, _)| caret)?;
            let style = TextStyle {
                color: theme.secondary_text,
                font_family: theme.code_font_family.clone().into(),
                font_size: px(11.).into(),
                ..Default::default()
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(label.clone()),
                px(11.),
                &[style.to_run(label.len())],
                None,
            );
            let dimensions = size(shaped.width + px(12.), px(24.));
            let mut anchor = caret;
            // Align explanations to the pane edge rather than the caret's
            // horizontal column; typing cannot make the label chase the text.
            anchor.origin.x = viewport.right() - px(8.) - dimensions.width;
            let bounds = context_overlay_placement(viewport, anchor, dimensions)?;
            #[cfg(feature = "gui-tests")]
            editor.update(cx, |view, _| {
                view.markup_hint_bounds = Some((bounds, label))
            });
            Some((bounds, shaped))
        },
        move |_, overlay, window, cx| {
            if let Some((bounds, shaped)) = overlay {
                window.paint_quad(fill(bounds, paint_theme.sidebar_bg));
                let _ = shaped.paint(
                    bounds.origin + point(px(6.), px(3.)),
                    px(18.),
                    gpui::TextAlign::Left,
                    Some(bounds.size.width),
                    window,
                    cx,
                );
            }
        },
    )
    .absolute()
    .inset_0()
}

fn link_caret_scroll_offset(
    offset: Pixels,
    viewport: Bounds<Pixels>,
    caret: Bounds<Pixels>,
    max_offset: Pixels,
) -> Pixels {
    let margin = px(4.).min(viewport.size.width / 4.);
    let adjustment = if caret.origin.x < viewport.origin.x + margin {
        viewport.origin.x + margin - caret.origin.x
    } else if caret.right() > viewport.right() - margin {
        viewport.right() - margin - caret.right()
    } else {
        px(0.)
    };
    (offset + adjustment).clamp(-max_offset, px(0.))
}

/// A focused draft is application chrome, not a document leaf. Measuring and
/// painting it independently keeps list height, text wrapping, and scroll
/// anchors unchanged as the destination is edited or the editor opens/closes.
fn link_editor_placement(
    viewport: Bounds<Pixels>,
    anchor: Option<Bounds<Pixels>>,
    error: bool,
) -> Bounds<Pixels> {
    let margin = px(12.)
        .min(viewport.size.width / 4.)
        .min(viewport.size.height / 4.);
    let dimensions = size(
        (viewport.size.width - margin * 2.)
            .min(px(420.))
            .max(px(1.)),
        px(if error { 132. } else { 112. })
            .min(viewport.size.height - margin * 2.)
            .max(px(1.)),
    );
    let left = viewport.origin.x + margin;
    let top = viewport.origin.y + margin;
    let bottom = viewport.bottom() - margin - dimensions.height;
    let right = viewport.right() - margin - dimensions.width;
    if let Some(anchor) = anchor {
        // Prefer below the editing row, then above it. Neither choice covers
        // the label, and both are independent from the document's flow.
        for origin in [
            point(left, anchor.bottom() + margin),
            point(left, anchor.origin.y - margin - dimensions.height),
            point(right, bottom),
            point(right, top),
        ] {
            let candidate = Bounds::new(origin, dimensions);
            if candidate.origin.y >= top
                && candidate.bottom() <= viewport.bottom() - margin
                && !candidate.intersects(&anchor)
            {
                return candidate;
            }
        }
    }
    Bounds::new(point(left, bottom), dimensions)
}

fn floating_image_editor(editor: Entity<RichEditorView>) -> impl IntoElement {
    canvas(
        move |viewport, window, cx| {
            let panel = editor.read(cx).image_editor.clone()?;
            let bounds = image_editor_placement(viewport);
            let mut overlay = panel.into_any_element();
            overlay.prepaint_as_root(
                bounds.origin,
                bounds.size.map(AvailableSpace::Definite),
                window,
                cx,
            );
            editor.update(cx, |view, _| view.image_editor_bounds = Some(bounds));
            Some(overlay)
        },
        |_, overlay, window, cx| {
            if let Some(mut overlay) = overlay {
                overlay.paint(window, cx);
            }
        },
    )
    .absolute()
    .inset_0()
}

fn floating_link_editor(editor: Entity<RichEditorView>, theme: EditorTheme) -> impl IntoElement {
    canvas(
        move |viewport, window, cx| {
            let view = editor.read(cx);
            let WidgetEdit::LinkDestination { range, draft, .. } = &view.widget_edit else {
                return None;
            };
            let range = range.clone();
            let draft = draft.clone();
            let scroll = view.link_scroll.clone();
            let error = view.frontmatter_error.clone();
            let bounds = link_editor_placement(
                viewport,
                view.table_toolbar_caret.map(|(caret, _)| caret),
                error.is_some(),
            );
            let mut overlay =
                link_editor(editor.clone(), &theme, range, draft, scroll, error).into_any_element();
            overlay.prepaint_as_root(
                bounds.origin,
                bounds.size.map(AvailableSpace::Definite),
                window,
                cx,
            );
            editor.update(cx, |view, _| view.link_editor_bounds = Some(bounds));
            Some(overlay)
        },
        |_, overlay, window, cx| {
            if let Some(mut overlay) = overlay {
                overlay.paint(window, cx);
            }
        },
    )
    .absolute()
    .inset_0()
}

fn link_editor(
    editor: Entity<RichEditorView>,
    theme: &EditorTheme,
    range: Range<usize>,
    draft: String,
    scroll: ScrollHandle,
    error: Option<String>,
) -> impl IntoElement {
    let cancel = editor.clone();
    let apply = editor.clone();
    div()
        .id("link-destination-editor")
        .accessibility_id("link-destination-editor")
        .role(Role::Group)
        .aria_label(theme.ui_text("Edit link"))
        .size_full()
        .p(px(10.))
        .rounded_md()
        .border_1()
        .border_color(theme.separator)
        .bg(theme.sidebar_bg)
        .cursor(CursorStyle::Arrow)
        .flex()
        .flex_col()
        .gap(px(6.))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .text_xs()
                .text_color(theme.secondary_text)
                .child(theme.ui_text("Link location")),
        )
        .child(
            div()
                .id("link-destination-field")
                .accessibility_id("link-destination-field")
                .role(Role::TextInput)
                .aria_label("Link URL")
                .w_full()
                .h(px(30.))
                .flex_shrink_0()
                .px(px(4.))
                .py(px(2.))
                .rounded_sm()
                .border_1()
                .border_color(theme.accent)
                .bg(theme.editor_bg)
                .cursor(CursorStyle::IBeam)
                .overflow_x_scroll()
                .track_scroll(&scroll)
                .child(WidgetOverlay {
                    editor: editor.clone(),
                    prefix: String::new(),
                    text: draft,
                    editing: true,
                    font_size: 14.,
                    line_height: 24.,
                    theme: theme.clone(),
                    color: theme.text,
                    italic: false,
                    monospace: false,
                    hug_width: true,
                    single_line: true,
                    target: OverlayTarget::LinkDestination { range },
                }),
        )
        .children(error.map(|message| {
            div()
                .text_xs()
                .text_color(theme.secondary_text)
                .child(message)
        }))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(8.))
                .child(
                    div()
                        .id("link-destination-cancel")
                        .role(Role::Button)
                        .aria_label(theme.ui_text("Cancel"))
                        .text_xs()
                        .text_color(theme.secondary_text)
                        .cursor(CursorStyle::PointingHand)
                        .child(SharedString::from(format!(
                            "{} (Esc)",
                            theme.ui_text("Cancel")
                        )))
                        .on_click(move |_, _, cx| {
                            cancel.update(cx, |view, cx| {
                                view.cancel_widget_edit(cx);
                            });
                        }),
                )
                .child(
                    div()
                        .id("link-destination-apply")
                        .role(Role::Button)
                        .aria_label(theme.ui_text("Apply"))
                        .text_xs()
                        .text_color(theme.accent)
                        .cursor(CursorStyle::PointingHand)
                        .child(SharedString::from(format!(
                            "{} (Return)",
                            theme.ui_text("Apply")
                        )))
                        .on_click(move |_, _, cx| {
                            apply.update(cx, |view, cx| {
                                view.commit_widget_edit(cx);
                            });
                        }),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
    }

    #[test]
    fn contextual_controls_require_a_focused_collapsed_idle_editor() {
        assert!(context_controls_eligible(true, true, true, false));
        assert!(!context_controls_eligible(false, true, true, false));
        assert!(!context_controls_eligible(true, false, true, false));
        assert!(!context_controls_eligible(true, true, false, false));
        assert!(!context_controls_eligible(true, true, true, true));
    }

    #[test]
    fn table_overlay_fits_the_viewport_without_covering_the_editing_row() {
        for viewport in [
            test_bounds(24., 142., 672., 852.),
            test_bounds(400., 200., 280., 600.),
            test_bounds(400., 200., 180., 600.),
        ] {
            let (dimensions, columns) = table_toolbar_dimensions(viewport);
            assert_eq!(
                dimensions.width + px(16.),
                viewport.size.width.min(px(360.))
            );
            assert!([1, 2, 3].contains(&columns));
            for y in [viewport.origin.y + px(20.), viewport.bottom() - px(44.)] {
                let caret = Bounds::new(point(viewport.right() - px(2.), y), size(px(2.), px(24.)));
                let overlay = context_overlay_placement(viewport, caret, dimensions)
                    .expect("a full toolbar fits above or below the active row");
                assert!(overlay.origin.x >= viewport.origin.x + px(8.));
                assert!(overlay.right() <= viewport.right() - px(8.));
                assert!(overlay.origin.y >= viewport.origin.y + px(8.));
                assert!(overlay.bottom() <= viewport.bottom() - px(8.));
                assert!(overlay.bottom() < caret.origin.y || overlay.origin.y > caret.bottom());
            }
        }
    }

    #[test]
    fn context_overlay_is_absent_for_offscreen_carets_and_a_viewport_without_space() {
        let viewport = test_bounds(10., 20., 320., 240.);
        let (dimensions, _) = table_toolbar_dimensions(viewport);
        assert!(
            context_overlay_placement(viewport, test_bounds(48., 0., 2., 16.), dimensions,)
                .is_none()
        );
        assert!(context_overlay_placement(
            test_bounds(10., 20., 160., 120.),
            test_bounds(48., 60., 2., 24.),
            size(px(144.), px(176.)),
        )
        .is_none());
    }

    #[test]
    fn syntax_badge_stays_in_the_pane_and_off_the_current_glyph_row() {
        let viewport = test_bounds(280., 142., 520., 480.);
        let dimensions = size(px(144.), px(24.));
        let caret = test_bounds(760., 190., 2., 42.);
        let badge = context_overlay_placement(viewport, caret, dimensions).unwrap();
        assert_eq!(badge.right(), viewport.right() - px(8.));
        assert!(!badge.intersects(&Bounds::new(
            point(viewport.origin.x, caret.origin.y),
            size(viewport.size.width, caret.size.height),
        )));
        // The same geometry is reused across blink phases; visibility of the
        // caret quad never moves the badge or the document's text.
        assert_eq!(
            context_overlay_placement(viewport, caret, dimensions),
            Some(badge)
        );
    }

    #[test]
    fn widget_ime_preedit_counts_as_pending_composition() {
        assert!(!composition_is_pending(false, false, false));
        assert!(composition_is_pending(true, false, false));
        assert!(composition_is_pending(false, true, false));
        assert!(composition_is_pending(false, false, true));
    }

    #[test]
    fn widget_recovery_snapshot_keeps_unicode_draft_and_repairs_only_stale_selection() {
        let draft = "a👩‍💻b";
        let snapshot = WidgetDraftSnapshot {
            kind: WidgetDraftKind::LinkDestination,
            original_source: "[label](old)\n".into(),
            source_range: 0..13,
            draft: draft.into(),
            // Both values point into the emoji's multi-byte grapheme.
            selection: 2..draft.len(),
            selection_reversed: true,
        };

        assert!(snapshot.has_valid_source_anchor());
        assert_eq!(snapshot.raw_draft_text(), draft);
        assert_eq!(snapshot.repaired_selection(), 1..draft.len());
        assert!(snapshot.selection_reversed);

        let invalid_anchor = WidgetDraftSnapshot {
            original_source: "é".into(),
            source_range: 1..2,
            ..snapshot
        };
        assert!(!invalid_anchor.has_valid_source_anchor());
    }

    #[test]
    fn widget_recovery_kind_covers_every_supported_overlay_and_rejects_unknown_frontmatter() {
        let overlays = [
            (
                WidgetEdit::CodeInfo {
                    id: NodeId(1),
                    draft: "rust".into(),
                    caret: 4,
                },
                Some(WidgetDraftKind::CodeInfo),
            ),
            (
                WidgetEdit::ImageAlt {
                    range: 0..1,
                    draft: "alt".into(),
                    caret: 3,
                },
                Some(WidgetDraftKind::ImageAlt),
            ),
            (
                WidgetEdit::Frontmatter {
                    key: "title",
                    draft: "Title".into(),
                    caret: 5,
                },
                Some(WidgetDraftKind::FrontmatterField(FrontmatterField::Title)),
            ),
            (
                WidgetEdit::FrontmatterYaml {
                    draft: "title: Title".into(),
                    caret: 12,
                },
                Some(WidgetDraftKind::FrontmatterYaml),
            ),
            (
                WidgetEdit::LinkDestination {
                    range: 8..11,
                    revision: 1,
                    draft: "new".into(),
                    caret: 3,
                },
                Some(WidgetDraftKind::LinkDestination),
            ),
        ];
        for (overlay, expected) in overlays {
            assert_eq!(overlay.recovery_kind(), expected);
        }
        assert_eq!(
            WidgetEdit::Frontmatter {
                key: "unknown",
                draft: String::new(),
                caret: 0,
            }
            .recovery_kind(),
            None
        );
    }

    #[test]
    fn link_destination_editor_resolves_label_hidden_url_and_marked_runs() {
        for source in [
            "[hello]()\n",
            "Before [**hello**](old 'title') after\n",
            "[a **bold** and `code`](https://example.test)\n",
        ] {
            let document = Document::new(source);
            let mut engine = RichEngine::new();
            engine.sync(&document);
            let expected = source.find("(").unwrap()..source.rfind(")").unwrap() + 1;
            for caret in [
                source.find('[').unwrap() + 1,
                expected.start + 1,
                expected.end - 1,
            ] {
                let (destination, draft) =
                    link_destination_at(&engine.tree().blocks, source, caret)
                        .expect("inline link has a visible destination editing surface");
                assert_eq!(destination, expected);
                assert_eq!(
                    draft,
                    if source.contains("https:") {
                        "https://example.test"
                    } else if source.contains("old") {
                        "old"
                    } else {
                        ""
                    }
                );
            }
        }
        for source in [
            "[label][ref]\n\n[ref]: old\n",
            "<https://example.test>\n",
            "![image](old)\n",
            "<a href=\"old\">label</a>\n",
        ] {
            let document = Document::new(source);
            let mut engine = RichEngine::new();
            engine.sync(&document);
            for caret in 0..source.len() {
                assert!(
                    link_destination_at(&engine.tree().blocks, source, caret).is_none(),
                    "not an inline text-link destination: {source:?}, caret {caret}"
                );
            }
        }
    }

    #[test]
    fn link_destination_widget_edits_selected_unicode_draft_not_the_body() {
        let mut edit = WidgetEdit::LinkDestination {
            range: 8..10,
            revision: 3,
            draft: "old".into(),
            caret: 3,
        };
        let mut anchor = 0;
        let initial = widget_snap(&edit, anchor).unwrap();
        insert_into_widget(&mut edit, &mut anchor, "https://example.test/🦀");
        assert_eq!(edit.caret(), "https://example.test/🦀".len());
        assert_eq!(
            apply_wrap_to_widget(&mut edit, WrapKind::Link),
            WidgetWrapResult::Ignored
        );
        apply_widget_snap(&mut edit, &initial, &mut anchor);
        assert_eq!(widget_snap(&edit, anchor).unwrap(), initial);
        assert!(widget_owns_tab(&edit) && widget_owns_caret(&edit) && widget_owns_wrap(&edit));
    }

    #[test]
    fn long_link_destination_scroll_keeps_caret_in_the_field() {
        let viewport = test_bounds(24., 42., 200., 30.);
        let caret = test_bounds(524., 44., 1., 24.);
        let offset = link_caret_scroll_offset(px(0.), viewport, caret, px(600.));
        assert_eq!(offset, px(-305.));
        let moved = test_bounds(524. + f32::from(offset), 44., 1., 24.);
        assert!(viewport.contains(&moved.origin));
        assert_eq!(
            link_caret_scroll_offset(offset, viewport, moved, px(600.)),
            offset
        );
        let home = test_bounds(24. + f32::from(offset), 44., 1., 24.);
        assert_eq!(
            link_caret_scroll_offset(offset, viewport, home, px(600.)),
            px(0.)
        );
    }

    #[test]
    fn link_editor_does_not_cover_its_label_and_fits_after_resize() {
        let viewport = test_bounds(24., 142., 672., 852.);
        let label = test_bounds(200., 245., 2., 25.);
        let overlay = link_editor_placement(viewport, Some(label), false);
        assert_eq!(overlay.origin.y, label.bottom() + px(12.));
        assert!(!overlay.intersects(&label));
        let near_bottom = test_bounds(200., 940., 2., 25.);
        let overlay = link_editor_placement(viewport, Some(near_bottom), true);
        assert!(overlay.bottom() < near_bottom.origin.y);
        assert!(overlay.origin.y >= viewport.origin.y && overlay.bottom() <= viewport.bottom());
        let narrow = test_bounds(100., 200., 160., 300.);
        let overlay = link_editor_placement(narrow, Some(test_bounds(120., 250., 2., 25.)), false);
        assert!(overlay.origin.x >= narrow.origin.x && overlay.right() <= narrow.right());
        assert!(overlay.origin.y >= narrow.origin.y && overlay.bottom() <= narrow.bottom());
    }

    #[test]
    fn editing_context_hint_contains_syntax_without_document_content() {
        let source = "## Secret heading\n\n- **private** [label](https://private.example)\n";
        let mut engine = RichEngine::new();
        engine.sync(&Document::new(source));
        assert_eq!(
            editing_context_hint_at(&engine.tree().blocks, 3),
            Some("Heading 2 · ##".into())
        );
        assert_eq!(
            editing_context_hint_at(&engine.tree().blocks, source.find("private").unwrap()),
            Some("Bold · **text**".into())
        );
        assert_eq!(
            editing_context_hint_at(&engine.tree().blocks, source.find("label").unwrap()),
            Some("Link · [text](url)".into())
        );
    }

    #[test]
    fn exited_list_draft_does_not_report_a_list_context() {
        for (source, home, hint) in [
            ("- First\n\n- Following", 8, "Bulleted list · -"),
            ("1. First\n\n1. Following", 9, "Numbered list · 1."),
        ] {
            let mut engine = RichEngine::new();
            engine.sync(&Document::new(source));
            assert_eq!(editing_context_hint_for_tree(engine.tree(), home), None);
            assert_eq!(
                editing_context_hint_for_tree(engine.tree(), source.find("First").unwrap()),
                Some(hint.into())
            );
            assert_eq!(
                editing_context_hint_for_tree(engine.tree(), source.find("Following").unwrap()),
                Some(hint.into())
            );
        }
    }

    #[test]
    fn body_selection_command_orders_a_reversed_unicode_range() {
        let source = "alpha 👩‍💻 beta\nsecond line";
        let caret = body_selection_for_offsets(source, 21, 6);
        assert_eq!(caret.range, 6..21);
        assert!(caret.reversed);
    }

    #[test]
    fn body_selection_command_repairs_grapheme_and_document_boundaries() {
        let source = "a👩‍💻e\u{301}z";
        let caret = body_selection_for_offsets(source, 14, 3);
        assert_eq!(caret.range, 1..12);
        assert!(caret.reversed);
        let caret = body_selection_for_offsets(source, usize::MAX, 99);
        assert_eq!(caret.range, source.len()..source.len());
        assert!(!caret.reversed);
    }
    use markrust_core::rich::NodeId;

    #[test]
    fn same_size_edit_maps_the_exact_previously_measured_caret_item() {
        assert_eq!(
            previous_caret_items(12, 30, 30, Some(&(12..13, 1))),
            Some(12..13)
        );
        // A broad changed window must not let another visible item make a
        // genuinely distant caret appear to have been visible.
        assert_eq!(
            previous_caret_items(24, 30, 30, Some(&(10..26, 16))),
            Some(24..25)
        );
        assert_eq!(previous_caret_items(12, 30, 30, None), Some(12..13));
    }

    #[test]
    fn structural_edit_maps_replaced_items_and_shifts_unchanged_suffix() {
        assert_eq!(
            previous_caret_items(12, 30, 31, Some(&(12..13, 2))),
            Some(12..13)
        );
        assert_eq!(
            previous_caret_items(13, 30, 31, Some(&(12..13, 2))),
            Some(12..13)
        );
        assert_eq!(
            previous_caret_items(14, 30, 31, Some(&(12..13, 2))),
            Some(13..14)
        );
        assert_eq!(
            previous_caret_items(12, 30, 29, Some(&(12..14, 1))),
            Some(12..14)
        );
        assert_eq!(
            previous_caret_items(13, 30, 29, Some(&(12..14, 1))),
            Some(14..15)
        );
        assert_eq!(
            previous_caret_items(8, 30, 29, Some(&(12..14, 1))),
            Some(8..9)
        );
    }

    #[test]
    fn inserted_caret_item_uses_only_its_adjacent_old_slots() {
        assert_eq!(
            previous_caret_items(12, 30, 32, Some(&(12..12, 2))),
            Some(11..13)
        );
        assert_eq!(
            previous_caret_items(13, 30, 32, Some(&(12..12, 2))),
            Some(11..13)
        );
        assert_eq!(
            previous_caret_items(14, 30, 32, Some(&(12..12, 2))),
            Some(12..13)
        );
        assert_eq!(
            previous_caret_items(0, 30, 31, Some(&(0..0, 1))),
            Some(0..1)
        );
        assert_eq!(
            previous_caret_items(30, 30, 31, Some(&(30..30, 1))),
            Some(29..30)
        );
    }

    #[test]
    fn prior_visibility_rejects_unmapped_or_out_of_range_items() {
        assert_eq!(previous_caret_items(2, 0, 3, Some(&(0..0, 3))), None);
        assert_eq!(previous_caret_items(31, 30, 31, Some(&(12..13, 2))), None);
        assert_eq!(previous_caret_items(12, 30, 31, Some(&(12..13, 1))), None);
        assert_eq!(previous_caret_items(12, 30, 31, None), None);
    }

    #[test]
    fn measured_item_visibility_uses_the_actual_viewport_edges() {
        let viewport = test_bounds(10., 100., 400., 200.);
        assert!(item_bounds_are_visible(
            test_bounds(10., 150., 400., 24.),
            viewport
        ));
        assert!(item_bounds_are_visible(
            test_bounds(10., 90., 400., 24.),
            viewport
        ));
        assert!(item_bounds_are_visible(
            test_bounds(10., 299., 400., 24.),
            viewport
        ));
        assert!(!item_bounds_are_visible(
            test_bounds(10., 76., 400., 24.),
            viewport
        ));
        assert!(!item_bounds_are_visible(
            test_bounds(10., 300., 400., 24.),
            viewport
        ));
        assert!(!item_bounds_are_visible(
            test_bounds(10., 100., 400., 24.),
            test_bounds(10., 100., 400., 0.)
        ));
    }

    #[test]
    fn fully_visible_caret_never_scrolls_inside_the_comfort_margin() {
        let viewport = test_bounds(10., 100., 400., 200.);
        for top in [100., 101., 110., 150., 277., 278.] {
            assert_eq!(
                caret_vertical_reveal_delta(test_bounds(20., top, 2., 22.), viewport),
                Some(0.)
            );
        }
    }

    #[test]
    fn genuinely_offscreen_caret_gets_only_the_minimal_reveal_with_margin() {
        let viewport = test_bounds(10., 100., 400., 200.);
        assert_eq!(
            caret_vertical_reveal_delta(test_bounds(20., 90., 2., 22.), viewport),
            Some(-22.)
        );
        assert_eq!(
            caret_vertical_reveal_delta(test_bounds(20., 295., 2., 22.), viewport),
            Some(29.)
        );
        assert_eq!(
            caret_vertical_reveal_delta(
                test_bounds(20., 100., 2., 22.),
                test_bounds(10., 100., 400., 0.)
            ),
            None
        );
    }

    #[test]
    fn content_remeasurement_preserves_scroll_inside_active_block() {
        let list = ListState::new(30, ListAlignment::Top, px(512.));
        list.scroll_to(gpui::ListOffset {
            item_ix: 12,
            offset_in_item: px(84.),
        });

        reconcile_list_state(&list, Some((12..13, 1)), 30);

        let after = list.logical_scroll_top();
        assert_eq!(after.item_ix, 12);
        assert_eq!(after.offset_in_item, px(84.));
    }

    #[test]
    fn changed_block_count_keeps_scroll_anchor_near_caret() {
        let list = ListState::new(30, ListAlignment::Top, px(512.));
        list.scroll_to(gpui::ListOffset {
            item_ix: 12,
            offset_in_item: px(84.),
        });

        reconcile_list_state(&list, Some((12..13, 2)), 31);

        let after = list.logical_scroll_top();
        assert_eq!(list.item_count(), 31);
        assert_eq!(after.item_ix, 12);
        assert_eq!(after.offset_in_item, px(84.));
    }

    #[test]
    fn source_fallback_moves_up_from_fenced_code_body() {
        let source = "# Top heading\n\nintro paragraph.\n\n- bullet 1\n- bullet 2\n\n```rust\nlet x = 1;\n```\n\n## Sub heading\n";
        let document = Document::new(source);
        let mut engine = RichEngine::new();
        engine.sync(&document);
        let code_body = source.find("let x = 1;").expect("code body");
        let prior = engine.vertical_caret(source, code_body, -1);
        assert!(
            prior < code_body && prior >= source.find("- bullet 1").unwrap(),
            "Up from the first code body line should reach the adjacent list, got {prior}"
        );
    }

    fn chip() -> WidgetEdit {
        WidgetEdit::CodeInfo {
            id: NodeId(1),
            draft: "rust".into(),
            caret: 4,
        }
    }

    fn caption() -> WidgetEdit {
        WidgetEdit::ImageAlt {
            range: 0..8,
            draft: "cat".into(),
            caret: 3,
        }
    }

    fn empty_caption() -> WidgetEdit {
        WidgetEdit::ImageAlt {
            range: 0..8,
            draft: String::new(),
            caret: 0,
        }
    }

    fn frontmatter_title() -> WidgetEdit {
        WidgetEdit::Frontmatter {
            key: "title",
            draft: "Hi".into(),
            caret: 2,
        }
    }

    fn yaml() -> WidgetEdit {
        WidgetEdit::FrontmatterYaml {
            draft: "title: Hi".into(),
            caret: 9,
        }
    }

    #[test]
    fn tab_in_chip_caption_frontmatter_commits_instead_of_indenting_body() {
        assert!(
            !widget_owns_tab(&WidgetEdit::Idle),
            "body Tab still runs IndentList"
        );
        for (edit, label) in [
            (chip(), "language chip"),
            (caption(), "image caption"),
            (frontmatter_title(), "frontmatter field"),
            (yaml(), "frontmatter YAML"),
        ] {
            assert!(
                widget_owns_tab(&edit),
                "Tab in {label} must commit the overlay and not IndentList the body"
            );
        }
    }

    #[test]
    fn wrap_shortcuts_target_widget_text_not_the_body() {
        assert!(
            !widget_owns_wrap(&WidgetEdit::Idle),
            "body Cmd/Ctrl+B still toggles the document"
        );
        assert!(
            !widget_wraps_draft(&chip()),
            "language chip is an identifier, not a wrap target"
        );
        assert!(
            widget_owns_wrap(&caption()) && widget_wraps_draft(&caption()),
            "image captions are Markdown text and may be formatted"
        );
        for (edit, label) in [
            (frontmatter_title(), "frontmatter field"),
            (yaml(), "frontmatter YAML"),
        ] {
            assert!(
                widget_owns_wrap(&edit) && !widget_wraps_draft(&edit),
                "{label} must consume wrap without writing Markdown syntax into YAML"
            );
        }
        assert!(
            widget_owns_wrap(&chip()) && !widget_wraps_draft(&chip()),
            "language chip owns wrap as a no-op so the body is not bolded"
        );

        let mut chip_edit = chip();
        let before = match &chip_edit {
            WidgetEdit::CodeInfo { draft, .. } => draft.clone(),
            _ => unreachable!(),
        };
        assert_eq!(
            apply_wrap_to_widget(&mut chip_edit, WrapKind::Bold),
            WidgetWrapResult::Ignored
        );
        match &chip_edit {
            WidgetEdit::CodeInfo { draft, .. } => {
                assert_eq!(draft, &before, "chip draft must not gain **")
            }
            other => panic!("chip edit must stay CodeInfo, got {other:?}"),
        }

        let mut caption_edit = caption();
        assert_eq!(
            apply_wrap_to_widget(&mut caption_edit, WrapKind::Bold),
            WidgetWrapResult::Applied
        );
        match &caption_edit {
            WidgetEdit::ImageAlt { draft, .. } => {
                assert_eq!(draft, "**cat**", "caption must wrap, got {draft:?}")
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        assert_eq!(
            apply_wrap_to_widget(&mut caption_edit, WrapKind::Bold),
            WidgetWrapResult::Applied
        );
        match &caption_edit {
            WidgetEdit::ImageAlt { draft, .. } => {
                assert_eq!(draft, "cat", "second bold unwraps, got {draft:?}")
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }

        let mut title = frontmatter_title();
        assert_eq!(
            apply_wrap_to_widget(&mut title, WrapKind::Italic),
            WidgetWrapResult::Ignored
        );
        match &title {
            WidgetEdit::Frontmatter { draft, .. } => {
                assert_eq!(draft, "Hi", "title must stay a YAML scalar, got {draft:?}")
            }
            other => panic!("expected Frontmatter, got {other:?}"),
        }

        let mut yaml_edit = yaml();
        assert_eq!(
            apply_wrap_to_widget(&mut yaml_edit, WrapKind::Code),
            WidgetWrapResult::Ignored
        );
        match &yaml_edit {
            WidgetEdit::FrontmatterYaml { draft, .. } => {
                assert_eq!(
                    draft, "title: Hi",
                    "YAML must not gain backticks, got {draft:?}"
                )
            }
            other => panic!("expected FrontmatterYaml, got {other:?}"),
        }

        let mut link_caption = caption();
        assert_eq!(
            apply_wrap_to_widget(&mut link_caption, WrapKind::Link),
            WidgetWrapResult::Applied
        );
        match &link_caption {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "[cat]()", "caption link wrap, got {draft:?}");
                assert_eq!(
                    *caret,
                    "[cat](".len(),
                    "Cmd-K on a caption selection must leave the caret in the URL, got {caret}"
                );
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }

        assert_eq!(
            apply_wrap_to_widget(&mut WidgetEdit::Idle, WrapKind::Bold),
            WidgetWrapResult::NotFocused
        );
    }

    #[test]
    fn empty_caption_wrap_types_inside_the_marks() {
        let mut edit = empty_caption();
        assert_eq!(
            apply_wrap_to_widget(&mut edit, WrapKind::Bold),
            WidgetWrapResult::Applied
        );
        match &edit {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "****", "empty bold wrap, got {draft:?}");
                assert_eq!(*caret, 2, "caret must sit between the marks, got {caret}");
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        let mut wrap_anchor = edit.caret();
        insert_into_widget(&mut edit, &mut wrap_anchor, "x");
        match &edit {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(
                    draft, "**x**",
                    "typing after empty wrap must go inside the marks, not after closers; got {draft:?}"
                );
                assert_eq!(*caret, 3);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }

        let mut empty_fm = WidgetEdit::Frontmatter {
            key: "title",
            draft: String::new(),
            caret: 0,
        };
        assert_eq!(
            apply_wrap_to_widget(&mut empty_fm, WrapKind::Bold),
            WidgetWrapResult::Ignored
        );
        let mut fm_anchor = empty_fm.caret();
        insert_into_widget(&mut empty_fm, &mut fm_anchor, "Hi");
        match &empty_fm {
            WidgetEdit::Frontmatter { draft, .. } => {
                assert_eq!(
                    draft, "Hi",
                    "frontmatter typing remains a YAML scalar after a consumed wrap shortcut, got {draft:?}"
                )
            }
            other => panic!("expected Frontmatter, got {other:?}"),
        }

        // Overlay click/drag places this inner offset; wrap of a non-empty
        // draft still covers the whole field (not a body selection).
        let mut wrapped = caption();
        apply_wrap_to_widget(&mut wrapped, WrapKind::Bold);
        match &wrapped {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "**cat**");
                assert_eq!(
                    *caret, 5,
                    "whole-draft wrap leaves the insert offset before the closers"
                );
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
    }

    #[test]
    fn overlay_click_places_inner_caret_without_moving_body() {
        let mut edit = caption();
        let body = 12..12;
        edit.set_caret(1);
        assert_eq!(edit.caret(), 1, "click x must land mid-caption");
        assert_eq!(body, 12..12, "overlay click must not move the body caret");

        let mut chip_edit = chip();
        chip_edit.set_caret(2);
        assert_eq!(chip_edit.caret(), 2);

        let mut title = frontmatter_title();
        title.set_caret(1);
        assert_eq!(title.caret(), 1);

        let mut yaml_edit = yaml();
        yaml_edit.set_caret(3);
        assert_eq!(yaml_edit.caret(), 3);
    }

    #[test]
    fn overlay_drag_extends_inner_caret() {
        let mut edit = caption();
        edit.set_caret(1);
        let anchor = edit.caret();
        edit.set_caret(3);
        let caret = edit.caret();
        assert_eq!(anchor.min(caret)..anchor.max(caret), 1..3);
        assert_eq!(edit.caret(), 3);
    }

    #[test]
    fn overlay_arrows_move_inner_caret_not_body() {
        assert!(
            !widget_owns_caret(&WidgetEdit::Idle),
            "body Left still moves the document caret"
        );
        for (edit, label) in [
            (chip(), "language chip"),
            (caption(), "image caption"),
            (frontmatter_title(), "frontmatter field"),
            (yaml(), "frontmatter YAML"),
        ] {
            assert!(
                widget_owns_caret(&edit),
                "Left/Right in {label} must own the inner caret"
            );
        }

        let mut edit = caption();
        let body = 12..12;
        edit.set_caret(1);
        let mut anchor = edit.caret();
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Left,
            false
        ));
        assert_eq!(edit.caret(), 0, "Left must step the inner offset");
        assert_eq!(anchor, 0);
        assert_eq!(body, 12..12, "overlay Left must not move the body caret");

        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Right,
            false
        ));
        assert_eq!(edit.caret(), 1);
        assert_eq!(body, 12..12);

        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::End,
            false
        ));
        assert_eq!(edit.caret(), 3);
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Home,
            false
        ));
        assert_eq!(edit.caret(), 0);
        assert_eq!(body, 12..12);

        let mut chip_edit = chip();
        chip_edit.set_caret(2);
        let mut chip_anchor = 2;
        assert!(move_in_widget(
            &mut chip_edit,
            &mut chip_anchor,
            CaretMove::Left,
            false
        ));
        assert_eq!(chip_edit.caret(), 1);

        let mut title = frontmatter_title();
        title.set_caret(1);
        let mut title_anchor = 1;
        assert!(move_in_widget(
            &mut title,
            &mut title_anchor,
            CaretMove::Right,
            false
        ));
        assert_eq!(title.caret(), 2);

        let mut yaml_edit = WidgetEdit::FrontmatterYaml {
            draft: "ab\ncd".into(),
            caret: 4,
        };
        let mut yaml_anchor = 4;
        assert!(move_in_widget(
            &mut yaml_edit,
            &mut yaml_anchor,
            CaretMove::Home,
            false
        ));
        assert_eq!(yaml_edit.caret(), 3, "YAML Home is the wrapped line start");
        assert!(move_in_widget(
            &mut yaml_edit,
            &mut yaml_anchor,
            CaretMove::End,
            false
        ));
        assert_eq!(yaml_edit.caret(), 5);

        let mut unused = 0;
        assert!(!move_in_widget(
            &mut WidgetEdit::Idle,
            &mut unused,
            CaretMove::Left,
            false
        ));
    }

    #[test]
    fn overlay_shift_arrows_extend_inner_selection() {
        let mut edit = caption();
        edit.set_caret(3);
        let mut anchor = 3;
        let body = 12..12;
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Left,
            true
        ));
        assert_eq!(edit.caret(), 2);
        assert_eq!(anchor, 3, "Shift-Left must keep the inner anchor");
        assert_eq!(edit.caret().min(anchor)..edit.caret().max(anchor), 2..3);
        assert_eq!(body, 12..12, "Shift-Left must not move the body caret");

        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Right,
            true
        ));
        assert_eq!(edit.caret(), 3);
        assert_eq!(anchor, 3);

        edit.set_caret(1);
        anchor = 3;
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Left,
            false
        ));
        assert_eq!(
            edit.caret(),
            1,
            "Left with an inner selection collapses to the start"
        );
        assert_eq!(anchor, 1);
        assert_eq!(body, 12..12);
    }

    #[test]
    fn overlay_shift_up_down_home_end_extend_yaml_selection() {
        let mut edit = WidgetEdit::FrontmatterYaml {
            draft: "alpha\nbeta".into(),
            caret: 6,
        };
        let mut anchor = 6;
        assert!(move_in_widget(&mut edit, &mut anchor, CaretMove::Up, true));
        assert_eq!(edit.caret(), 0, "Shift-Up from `beta` lands on `alpha`");
        assert_eq!(anchor, 6, "Shift-Up must keep the inner YAML anchor");
        assert_eq!(edit.caret().min(anchor)..edit.caret().max(anchor), 0..6);

        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Down,
            true
        ));
        assert_eq!(edit.caret(), 6);
        assert_eq!(anchor, 6);

        edit.set_caret(8);
        anchor = 8;
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Home,
            true
        ));
        assert_eq!(edit.caret(), 6, "Shift-Home is line-local in YAML");
        assert_eq!(anchor, 8);
        assert!(move_in_widget(&mut edit, &mut anchor, CaretMove::End, true));
        assert_eq!(edit.caret(), 10);
        assert_eq!(anchor, 8);
    }

    #[test]
    fn overlay_word_and_document_keys_move_inner_draft() {
        let mut edit = WidgetEdit::FrontmatterYaml {
            draft: "alpha beta\ngamma".into(),
            caret: "alpha beta\ngamma".len(),
        };
        let mut anchor = edit.caret();
        let body = 12..12;
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::WordLeft,
            false
        ));
        assert_eq!(
            edit.caret(),
            "alpha beta\n".len(),
            "WordLeft is start of `gamma`"
        );
        assert_eq!(anchor, edit.caret());
        assert_eq!(
            body,
            12..12,
            "overlay WordLeft must not move the body caret"
        );

        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::WordLeft,
            false
        ));
        assert_eq!(edit.caret(), "alpha ".len());

        edit.set_caret("alpha ".len());
        anchor = edit.caret();
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::WordRight,
            true
        ));
        assert_eq!(
            edit.caret(),
            "alpha beta".len(),
            "Shift-WordRight extends to the end of `beta`"
        );
        assert_eq!(anchor, "alpha ".len());

        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::DocumentHome,
            false
        ));
        assert_eq!(edit.caret(), 0);
        assert_eq!(anchor, 0);
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::DocumentEnd,
            true
        ));
        assert_eq!(edit.caret(), "alpha beta\ngamma".len());
        assert_eq!(anchor, 0, "Cmd-Shift-Down keeps the overlay draft anchor");
        assert_eq!(body, 12..12);
    }

    #[test]
    fn overlay_word_and_line_delete_stay_in_the_draft() {
        let mut edit = WidgetEdit::FrontmatterYaml {
            draft: "alpha beta".into(),
            caret: "alpha beta".len(),
        };
        let mut anchor = edit.caret();
        let body = 12..12;
        delete_word_before_in_widget(&mut edit, &mut anchor);
        match &edit {
            WidgetEdit::FrontmatterYaml { draft, caret } => {
                assert_eq!(
                    draft, "alpha ",
                    "overlay Option-Backspace must delete the previous word, got {draft:?}"
                );
                assert_eq!(*caret, "alpha ".len());
            }
            other => panic!("expected FrontmatterYaml, got {other:?}"),
        }
        assert_eq!(anchor, "alpha ".len());
        assert_eq!(body, 12..12, "overlay word-delete must not mutate the body");

        let mut yaml = WidgetEdit::FrontmatterYaml {
            draft: "alpha beta\ngamma extra".into(),
            caret: "alpha beta\ngamma extra".len(),
        };
        let mut yaml_anchor = yaml.caret();
        delete_to_line_start_in_widget(&mut yaml, &mut yaml_anchor);
        match &yaml {
            WidgetEdit::FrontmatterYaml { draft, caret } => {
                assert_eq!(
                    draft, "alpha beta\n",
                    "overlay Cmd-Backspace is the current YAML line, got {draft:?}"
                );
                assert_eq!(*caret, "alpha beta\n".len());
            }
            other => panic!("expected FrontmatterYaml, got {other:?}"),
        }

        let mut selected = caption();
        selected.set_caret(1);
        let mut sel_anchor = 3;
        delete_word_before_in_widget(&mut selected, &mut sel_anchor);
        match &selected {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(
                    draft, "c",
                    "word-delete on a non-empty overlay selection deletes the range"
                );
                assert_eq!(*caret, 1);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }

        let mut forward = WidgetEdit::FrontmatterYaml {
            draft: "alpha beta".into(),
            caret: 0,
        };
        let mut fwd_anchor = 0;
        delete_word_after_in_widget(&mut forward, &mut fwd_anchor);
        match &forward {
            WidgetEdit::FrontmatterYaml { draft, .. } => {
                assert_eq!(
                    draft, " beta",
                    "overlay Option-Delete removes the next word, got {draft:?}"
                );
            }
            other => panic!("expected FrontmatterYaml, got {other:?}"),
        }
        assert_eq!(body, 12..12);
    }

    #[test]
    fn shift_up_extends_body_selection_across_paragraphs() {
        let source = "hello\n\nworld";
        let mut engine = RichEngine::new();
        let doc = Document::new(source);
        engine.sync(&doc);
        let w = source.find('w').expect("w");
        let up = engine.vertical_caret(source, w, -1);
        assert!(
            up < w,
            "Up from `world` must leave the second paragraph, got {up}"
        );
        let (sel, reversed) = extend_selection_range(w..w, false, up);
        assert!(reversed, "Shift-Up moves the caret before the anchor");
        assert_eq!(sel, up..w);
        assert!(
            source.get(sel.clone()).is_some_and(|s| s.contains('\n')),
            "Shift-Up must select across the block gap, got {:?}",
            source.get(sel)
        );
        let r = source.find('r').expect("r in world");
        let line_start = source[..r].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let home = engine.clamp_raw_prefix(
            source,
            engine.snap_caret(line_start, markrust_core::rich::Bias::Right),
            markrust_core::rich::Bias::Right,
        );
        assert_eq!(home, w, "Home on `world` is the first painted letter");
        let (to_home, reversed_home) = extend_selection_range(r..r, false, home);
        assert!(reversed_home);
        assert_eq!(
            to_home,
            home..r,
            "Shift-Home from inside `world` must select back to the line start"
        );
    }

    #[test]
    fn word_and_document_keys_extend_body_selection() {
        let source = "**hello** world\n\nnext";
        let mut engine = RichEngine::new();
        let doc = Document::new(source);
        engine.sync(&doc);
        let h = source.find('h').expect("h");
        let w = source.find('w').expect("w");
        let n = source.find("next").expect("next");
        let end_hello = engine.next_word_caret(source, h);
        assert_eq!(
            source.as_bytes().get(end_hello),
            Some(&b' '),
            "WordRight from bold `hello` lands on the painted space"
        );
        let (sel, reversed) = extend_selection_range(h..h, false, end_hello);
        assert!(!reversed);
        assert_eq!(sel, h..end_hello);

        let start_next = engine.prev_word_caret(source, source.len());
        assert_eq!(start_next, n, "WordLeft from EOF is the start of `next`");
        let start_world = engine.prev_word_caret(source, start_next);
        assert_eq!(
            start_world, w,
            "WordLeft from `next` skips the gap onto `world`"
        );
        let (to_world, rev_world) = extend_selection_range(n..n, false, start_world);
        assert!(rev_world);
        assert_eq!(to_world, w..n);

        let home = engine.clamp_raw_prefix(
            source,
            engine.snap_caret(0, markrust_core::rich::Bias::Right),
            markrust_core::rich::Bias::Right,
        );
        assert_eq!(
            home, h,
            "Cmd-Up / document start is the first painted letter"
        );
        let end = engine.clamp_raw_prefix(
            source,
            engine.snap_caret(source.len(), markrust_core::rich::Bias::Left),
            markrust_core::rich::Bias::Left,
        );
        assert!(
            end >= n,
            "Cmd-Down / document end must reach the last paragraph, got {end}"
        );
        let (to_end, reversed_end) = extend_selection_range(h..h, false, end);
        assert!(!reversed_end);
        assert_eq!(to_end, h..end);
        let (to_home, reversed_home) = extend_selection_range(n..n, false, home);
        assert!(reversed_home);
        assert_eq!(to_home, home..n);
    }

    #[test]
    fn overlay_delete_edits_the_draft_not_the_body() {
        assert!(
            !widget_owns_caret(&WidgetEdit::Idle),
            "body Delete still deletes in the document"
        );
        let mut edit = caption();
        edit.set_caret(1);
        let mut anchor = edit.caret();
        let body = "hello";
        delete_after_in_widget(&mut edit, &mut anchor);
        match &edit {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "ct", "Delete must remove the inner grapheme");
                assert_eq!(*caret, 1);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        assert_eq!(body, "hello", "overlay Delete must not mutate the body");

        delete_after_in_widget(&mut edit, &mut anchor);
        match &edit {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "c");
                assert_eq!(*caret, 1);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        delete_after_in_widget(&mut edit, &mut anchor);
        match &edit {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "c", "Delete at end of overlay is a no-op");
                assert_eq!(*caret, 1);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
    }

    #[test]
    fn overlay_select_all_selects_the_draft_not_the_body() {
        let body = 0..80;
        for (mut edit, label) in [
            (chip(), "language chip"),
            (caption(), "image caption"),
            (frontmatter_title(), "frontmatter field"),
            (yaml(), "frontmatter YAML"),
        ] {
            let len = match &edit {
                WidgetEdit::CodeInfo { draft, .. }
                | WidgetEdit::ImageAlt { draft, .. }
                | WidgetEdit::Frontmatter { draft, .. }
                | WidgetEdit::FrontmatterYaml { draft, .. } => draft.len(),
                WidgetEdit::LinkDestination { draft, .. } => draft.len(),
                WidgetEdit::Idle => panic!("expected overlay"),
            };
            let mut anchor = edit.caret();
            assert!(
                select_all_in_widget(&mut edit, &mut anchor),
                "Cmd+A in {label} must select the draft"
            );
            assert_eq!(anchor, 0, "{label} SelectAll anchor");
            assert_eq!(edit.caret(), len, "{label} SelectAll caret");
            assert_eq!(body, 0..80, "{label} must not SelectAll the document");
        }
        let mut unused = 0;
        assert!(
            !select_all_in_widget(&mut WidgetEdit::Idle, &mut unused),
            "body Cmd+A is not consumed by an overlay (tables select the cell first)"
        );
        assert_eq!(body, 0..80);
    }

    #[test]
    fn overlay_empty_caret_copy_is_the_whole_draft() {
        assert_eq!(
            overlay_copy_text("rust", 4..4).as_deref(),
            Some("rust"),
            "empty caret in the language chip copies the draft"
        );
        assert_eq!(
            overlay_copy_text("cat", 3..3).as_deref(),
            Some("cat"),
            "empty caret in the caption copies the draft"
        );
        assert_eq!(
            overlay_copy_text("Hi", 2..2).as_deref(),
            Some("Hi"),
            "empty caret in frontmatter copies the draft"
        );
        assert_eq!(
            overlay_copy_text("title: Hi", 0..0).as_deref(),
            Some("title: Hi")
        );
        assert_eq!(
            overlay_copy_text("hello", 0..5).as_deref(),
            Some("hello"),
            "non-empty overlay selection still copies the slice"
        );
        assert_eq!(overlay_copy_text("hello", 1..4).as_deref(), Some("ell"));
        assert_eq!(
            overlay_copy_text("", 0..0),
            None,
            "empty overlay draft copies nothing"
        );
    }

    #[test]
    fn body_table_select_all_selects_the_cell_not_the_document() {
        use markrust_core::rich::{table_select_all_range, RichEngine};
        use markrust_core::Document;

        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let doc = Document::new(source);
        let mut engine = RichEngine::new();
        engine.sync(&doc);
        let a = source.find('a').expect("header a");
        let cell = table_select_all_range(&engine, source, &(a..a), None).expect("first Cmd-A");
        assert_eq!(
            cell,
            engine.cell_edit_range(a, source).expect("cell a"),
            "body Cmd+A in a table must select the cell, like overlay Cmd+A selects the draft"
        );
        assert_ne!(cell, 0..source.len(), "must not SelectAll the document");
        assert!(
            !source[cell.clone()].contains('|'),
            "cell SelectAll must not include `|`"
        );
        assert!(
            table_select_all_range(&engine, source, &cell, None).is_none(),
            "second Cmd-A falls through to the document"
        );
    }

    #[test]
    fn overlay_insert_and_backspace_respect_inner_selection() {
        let mut edit = caption();
        edit.set_caret(1);
        let mut anchor = 3;
        let body = "hello";
        insert_into_widget(&mut edit, &mut anchor, "x");
        match &edit {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "cx", "typing must replace the inner selection");
                assert_eq!(*caret, 2);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        assert_eq!(anchor, 2);
        assert_eq!(body, "hello");

        let mut selected = caption();
        selected.set_caret(1);
        let mut sel_anchor = 3;
        delete_before_in_widget(&mut selected, &mut sel_anchor);
        match &selected {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(
                    draft, "c",
                    "Backspace on a non-empty inner selection must delete the range"
                );
                assert_eq!(*caret, 1);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        assert_eq!(sel_anchor, 1);
        assert_eq!(body, "hello");

        let mut del = caption();
        del.set_caret(0);
        let mut del_anchor = 2;
        delete_after_in_widget(&mut del, &mut del_anchor);
        match &del {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(
                    draft, "t",
                    "Delete on a non-empty inner selection must delete the range"
                );
                assert_eq!(*caret, 0);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }

        let mut collapsed = caption();
        collapsed.set_caret(3);
        let mut col_anchor = 3;
        delete_before_in_widget(&mut collapsed, &mut col_anchor);
        match &collapsed {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "ca", "collapsed Backspace still deletes one char");
                assert_eq!(*caret, 2);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
    }

    #[test]
    fn overlay_editing_keeps_extended_graphemes_atomic() {
        for (cluster, label) in [
            ("e\u{301}", "combining accent"),
            ("👩\u{200d}💻", "ZWJ emoji"),
        ] {
            let draft = format!("{cluster}x");
            let mut edit = WidgetEdit::ImageAlt {
                range: 0..0,
                draft: draft.clone(),
                caret: 0,
            };

            // A click/IME offset may be a scalar boundary within a single
            // visible glyph. It must normalize to a real caret boundary.
            edit.set_caret(cluster.chars().next().expect(label).len_utf8());
            assert_eq!(
                edit.caret(),
                0,
                "inner {label} boundary must not become a visible caret stop"
            );

            let mut anchor = edit.caret();
            assert!(move_in_widget(
                &mut edit,
                &mut anchor,
                CaretMove::Right,
                false
            ));
            assert_eq!(edit.caret(), cluster.len(), "Right skips one {label}");
            assert!(move_in_widget(
                &mut edit,
                &mut anchor,
                CaretMove::Left,
                false
            ));
            assert_eq!(edit.caret(), 0, "Left skips one {label}");

            edit.set_caret(cluster.len());
            anchor = edit.caret();
            delete_before_in_widget(&mut edit, &mut anchor);
            match &edit {
                WidgetEdit::ImageAlt { draft, caret, .. } => {
                    assert_eq!(draft, "x", "Backspace removes one {label}");
                    assert_eq!(*caret, 0);
                }
                other => panic!("expected image caption, got {other:?}"),
            }

            let mut delete = WidgetEdit::ImageAlt {
                range: 0..0,
                draft,
                caret: 0,
            };
            let mut delete_anchor = 0;
            delete_after_in_widget(&mut delete, &mut delete_anchor);
            match &delete {
                WidgetEdit::ImageAlt { draft, caret, .. } => {
                    assert_eq!(draft, "x", "Delete removes one {label}");
                    assert_eq!(*caret, 0);
                }
                other => panic!("expected image caption, got {other:?}"),
            }
        }
    }

    #[test]
    fn overlay_undo_rewinds_draft_not_the_body() {
        let mut edit = caption();
        let mut anchor = edit.caret();
        let mut undo = Vec::new();
        let mut redo = Vec::new();
        let body = "hello";
        push_widget_history(&edit, anchor, &mut undo, &mut redo);
        insert_into_widget(&mut edit, &mut anchor, "!");
        match &edit {
            WidgetEdit::ImageAlt { draft, .. } => assert_eq!(draft, "cat!"),
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        assert!(undo_widget_history(
            &mut edit,
            &mut anchor,
            &mut undo,
            &mut redo
        ));
        match &edit {
            WidgetEdit::ImageAlt { draft, caret, .. } => {
                assert_eq!(draft, "cat", "overlay undo must restore the draft");
                assert_eq!(*caret, 3);
            }
            other => panic!("expected ImageAlt, got {other:?}"),
        }
        assert_eq!(body, "hello", "overlay undo must not mutate the body");
        assert!(
            !undo_widget_history(&mut edit, &mut anchor, &mut undo, &mut redo),
            "empty overlay undo must not fall through to the document"
        );
        assert!(redo_widget_history(
            &mut edit,
            &mut anchor,
            &mut undo,
            &mut redo
        ));
        match &edit {
            WidgetEdit::ImageAlt { draft, .. } => assert_eq!(draft, "cat!"),
            other => panic!("expected ImageAlt, got {other:?}"),
        }

        let mut idle_anchor = 0;
        let mut idle_undo = Vec::new();
        let mut idle_redo = Vec::new();
        assert!(!undo_widget_history(
            &mut WidgetEdit::Idle,
            &mut idle_anchor,
            &mut idle_undo,
            &mut idle_redo
        ));
    }

    #[test]
    fn overlay_yaml_up_down_moves_between_lines() {
        let mut edit = WidgetEdit::FrontmatterYaml {
            draft: "ab\ncd".into(),
            caret: 4,
        };
        let mut anchor = 4;
        let body = 12..12;
        assert!(move_in_widget(&mut edit, &mut anchor, CaretMove::Up, false));
        assert_eq!(edit.caret(), 1, "YAML Up stays on the same column");
        assert_eq!(anchor, 1);
        assert_eq!(body, 12..12, "overlay Up must not move the body caret");
        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Down,
            false
        ));
        assert_eq!(edit.caret(), 4);
        assert_eq!(body, 12..12);

        let mut caption_edit = caption();
        let mut cap_anchor = 3;
        assert!(move_in_widget(
            &mut caption_edit,
            &mut cap_anchor,
            CaretMove::Up,
            false
        ));
        assert_eq!(caption_edit.caret(), 3, "single-line overlay Up is a no-op");
    }

    #[test]
    fn overlay_vertical_navigation_preserves_grapheme_column() {
        let combining = "e\u{301}";
        let draft = format!("{combining}\nab");
        let mut edit = WidgetEdit::FrontmatterYaml {
            draft,
            caret: combining.len(),
        };
        let mut anchor = edit.caret();

        assert!(move_in_widget(
            &mut edit,
            &mut anchor,
            CaretMove::Down,
            false
        ));
        assert_eq!(
            edit.caret(),
            combining.len() + 2,
            "Down from one visible combining glyph must land after one visible glyph"
        );

        assert!(move_in_widget(&mut edit, &mut anchor, CaretMove::Up, false));
        assert_eq!(
            edit.caret(),
            combining.len(),
            "Up must return to the end of the same visible grapheme"
        );
    }

    #[test]
    fn overlay_click_does_not_match_idle_as_focused() {
        assert!(!WidgetEdit::Idle.matches_overlay(&OverlayTarget::CodeInfo(NodeId(1))));
        assert!(chip().matches_overlay(&OverlayTarget::CodeInfo(NodeId(1))));
        assert!(caption().matches_overlay(&OverlayTarget::ImageAlt {
            range: 0..8,
            stored: "cat".into(),
        }));
        assert!(
            frontmatter_title().matches_overlay(&OverlayTarget::Frontmatter {
                key: "title",
                stored: "Hi".into(),
            })
        );
        assert!(yaml().matches_overlay(&OverlayTarget::FrontmatterYaml {
            stored: "title: Hi".into(),
        }));
    }

    #[test]
    fn widget_draft_with_caret_paints_bar_at_inner_offset() {
        assert_eq!(widget_draft_with_caret("****", 2, ""), "**|**");
        assert_eq!(widget_draft_with_caret("cat", 3, "x"), "catx|");
    }
}
