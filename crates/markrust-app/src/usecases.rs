// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Use-case matrix: per-step JSON recorder + scenario generator + runner.
//!
//! A `Scenario` pairs a starting document template with a sequence of
//! actions (keystrokes, raw text insertion, caret jumps, selections).
//! For each action, the runner captures a `Snapshot` of the editor's
//! state (caret, selection, mode, source, rendered top-level blocks,
//! viewport) and writes it as a JSON line. Semantic and transition contracts
//! are checked after every action, including generated smoke journeys.
//!
//! The generator is deterministic per `seed` and can produce larger matrices
//! by mixing document templates with action sequences; curated
//! scenarios cover the small handful of behaviours users actually
//! notice (Enter on an empty task item, mode switching, etc.).

use std::fs;
use std::io::Write;
use std::ops::Range;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use gpui::{
    point, px, AppContext, Entity, Focusable, HeadlessAppContext, Keystroke, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, PlatformInput, Point,
    WindowHandle,
};
use markrust_core::rich::BlockKind;
use serde::Serialize;

use crate::observation::{InputOwner, Observation};
use crate::session::WorkspaceCommand;
use crate::window::MarkRustWindow;
use crate::workspace::{EditorMode, Workspace};
use markrust_editor::EditorCommand;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One captured frame of the editor's state after an action runs.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct Snapshot {
    /// 0-based step index. 0 is the starting snapshot (before any action).
    pub step: usize,
    /// Description of the action that produced this snapshot.
    pub action: Action,
    /// Document source as a UTF-8 string.
    pub source: String,
    /// Cursor offset in bytes.
    pub caret: usize,
    /// Selection range; `start == end` for a collapsed caret.
    pub selection: Range<usize>,
    /// Editor mode (`"wysiwyg"`, `"source"`, `"split"`).
    pub mode: String,
    /// Top-level rendered blocks (heading / paragraph / list / fence / etc.).
    pub blocks: Vec<BlockInfo>,
    /// Number of source lines.
    pub line_count: usize,
    /// Window viewport height in logical pixels.
    pub viewport_height: f32,
    /// First rendered block in the rich viewport, when that pane is visible.
    pub viewport_first_block: Option<usize>,
    /// Pixel offset inside the first rendered block.
    pub viewport_offset_px: Option<f32>,
    /// Whether this frame painted the active caret inside the rich viewport.
    pub caret_visible: Option<bool>,
    /// Painted viewport and caret vertical bounds, for scroll diagnostics.
    pub viewport_y: Option<(f32, f32)>,
    pub caret_y: Option<(f32, f32)>,
    /// Whether the visible Source pane's model caret lies inside its viewport.
    pub source_caret_visible: Option<bool>,
    /// Source scroll offset and vertical bounds, for Source/Split traces.
    pub source_viewport_offset_y: Option<f32>,
    pub source_viewport_y: Option<(f32, f32)>,
    pub source_caret_y: Option<(f32, f32)>,
    /// Semantic state and source-mapped observations from the real paint pass.
    pub ui: Option<Observation>,
    /// Response delta from the preceding frame, independent of timestamps.
    pub response: Option<Response>,
    /// Monotonic elapsed milliseconds since this scenario began.
    pub timestamp_ms: u64,
    /// Intermediate production paints, not merely the action's settled state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paint_frames: Vec<PaintFrame>,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct PaintFrame {
    pub step: usize,
    pub action: Action,
    pub pass: usize,
    pub source: String,
    pub caret: usize,
    pub selection: Range<usize>,
    pub rich_first_block: Option<usize>,
    pub rich_offset_px: Option<f32>,
    pub source_offset_y: Option<f32>,
    pub ui: Observation,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct Response {
    pub document_changed: bool,
    pub selection_changed: bool,
    pub focus_changed: bool,
    pub mode_changed: bool,
    pub viewport_changed: bool,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct BlockInfo {
    /// Stable name for the block kind ("paragraph", "heading(2)",
    /// "task-list", "code-fence", "table", etc.).
    pub kind: String,
    /// Heading level (1..=6) when `kind == "heading"`.
    pub level: Option<u8>,
    /// Byte range in the source.
    pub source_range: Range<usize>,
    /// First 80 chars of the block's text content for human inspection.
    pub preview: String,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value")]
pub enum Action {
    /// Typed into the editor via `dispatch_keystroke` (e.g. `"cmd-1"`,
    /// `"shift-right"`, `"enter"`).
    Keystroke(String),
    /// Raw character input (uses the same IME path as real typing).
    InsertText(String),
    /// Advance the deterministic test clock, not wall-clock sleeping.
    WaitMillis(u64),
    /// `JumpTo(offset)` to position the caret without producing undo
    /// history (used to set up the pre-state for follow-up actions).
    JumpTo(usize),
    /// Focus the raw source pane in Split mode.
    FocusSource,
    FocusRich,
    SelectRange {
        start: usize,
        end: usize,
    },
    /// Real pointer input addressed through the last native paint's caret stops.
    MouseClick {
        pane: InputOwner,
        offset: usize,
        count: usize,
    },
    MouseDrag {
        pane: InputOwner,
        start: usize,
        end: usize,
    },
    /// Separate native events each receive their own paint and trace frame.
    MousePress {
        pane: InputOwner,
        offset: usize,
    },
    MousePressPadding {
        pane: InputOwner,
        offset: usize,
    },
    /// OS clicks may deliver Up before any paint of Down's document mutation.
    ClickBelowContentWithoutPaint {
        pane: InputOwner,
    },
    MouseMove {
        pane: InputOwner,
        offset: usize,
    },
    MouseRelease {
        pane: InputOwner,
        offset: usize,
    },
    ScrollPane {
        pane: InputOwner,
        delta_y: f32,
    },
    ScrollCaretToEdge {
        pane: InputOwner,
        bottom: bool,
    },
    /// Click a native glyph row midway through the manually scrolled pane.
    ClickVisibleRow {
        pane: InputOwner,
        vertical_fraction: f32,
    },
    TableClick(usize),
    PaletteOutsideClick,
    NewTab,
    SwitchTab(usize),
    EditFrontmatter,
    HighlightStyle(crate::config::HighlightStyle),
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Action::Keystroke(s) => format!("keystroke: {s}"),
            Action::InsertText(s) => format!("insert-text: {s:?}"),
            Action::WaitMillis(ms) => format!("idle: {ms}ms"),
            Action::JumpTo(o) => format!("jump-to: {o}"),
            Action::FocusSource => "focus-source".into(),
            Action::FocusRich => "focus-wysiwyg".into(),
            Action::SelectRange { start, end } => format!("select-range: {start}..{end}"),
            Action::MouseClick {
                pane,
                offset,
                count,
            } => format!("mouse-click: {pane:?} at {offset} ({count})"),
            Action::MouseDrag { pane, start, end } => {
                format!("mouse-drag: {pane:?} {start}..{end}")
            }
            Action::MousePress { pane, offset } => format!("mouse-press: {pane:?} at {offset}"),
            Action::MousePressPadding { pane, offset } => {
                format!("mouse-press-left-padding: {pane:?} at row {offset}")
            }
            Action::ClickBelowContentWithoutPaint { pane } => {
                format!("mouse-click-below-content-without-paint: {pane:?}")
            }
            Action::MouseMove { pane, offset } => format!("mouse-move: {pane:?} to {offset}"),
            Action::MouseRelease { pane, offset } => format!("mouse-release: {pane:?} at {offset}"),
            Action::ScrollPane { pane, delta_y } => {
                format!("scroll-wheel: {pane:?} by {delta_y}px")
            }
            Action::ScrollCaretToEdge { pane, bottom } => format!(
                "scroll-visible-caret: {pane:?} to {} edge",
                if *bottom { "bottom" } else { "top" }
            ),
            Action::ClickVisibleRow {
                pane,
                vertical_fraction,
            } => format!("mouse-click-visible-row: {pane:?} at {vertical_fraction}"),
            Action::TableClick(index) => format!("table-control-click: {index}"),
            Action::PaletteOutsideClick => "palette-outside-click".into(),
            Action::NewTab => "new-tab".into(),
            Action::SwitchTab(index) => format!("switch-tab: {index}"),
            Action::EditFrontmatter => "edit-frontmatter".into(),
            Action::HighlightStyle(style) => format!("highlight-style: {style:?}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocumentTemplate {
    Empty,
    Plain {
        paragraphs: usize,
    },
    SeparatedParagraphs {
        paragraphs: usize,
    },
    Headings {
        levels: Vec<u8>,
    },
    BulletList {
        items: usize,
        trailing_empty: bool,
    },
    OrderedList {
        items: usize,
        trailing_empty: bool,
    },
    TaskList {
        items: usize,
        trailing_empty: bool,
        all_checked: bool,
    },
    CodeFence,
    Table {
        rows: usize,
    },
    Blockquote {
        paragraphs: usize,
    },
    Mixed,
    Frontmatter,
    Unicode,
    Whitespace,
    StableHints,
    MouseSelection,
    DeepPointer,
    MiddleList,
    FindContent,
    FindDeepContent,
}

const FIND_CONTENT: &str = "## Поиск **слово**\n\nНачало café 👩🏽‍💻. Повтор слово и слово.\n\nДлинная строка: alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega.\n\nКонец 👩🏽‍💻.\n";

impl DocumentTemplate {
    /// Render the template to its starting markdown source.
    pub fn render(&self) -> String {
        match self {
            DocumentTemplate::Empty => String::new(),
            DocumentTemplate::Plain { paragraphs } => (0..*paragraphs)
                .map(|i| format!("Paragraph {}.\n", i + 1))
                .collect(),
            DocumentTemplate::SeparatedParagraphs { paragraphs } => (0..*paragraphs)
                .map(|i| format!("Paragraph {}.\n\n", i + 1))
                .collect(),
            DocumentTemplate::Headings { levels } => levels
                .iter()
                .enumerate()
                .map(|(i, level)| format!("{} Heading {}\n\n", "#".repeat(*level as usize), i + 1))
                .collect(),
            DocumentTemplate::BulletList {
                items,
                trailing_empty,
            } => {
                let mut s = String::new();
                for i in 0..*items {
                    s.push_str(&format!("- item {}\n", i + 1));
                }
                if *trailing_empty {
                    s.push_str("- \n");
                }
                s
            }
            DocumentTemplate::OrderedList {
                items,
                trailing_empty,
            } => {
                let mut s = String::new();
                for i in 0..*items {
                    s.push_str(&format!("{}. item {}\n", i + 1, i + 1));
                }
                if *trailing_empty {
                    s.push_str("1. \n");
                }
                s
            }
            DocumentTemplate::TaskList {
                items,
                trailing_empty,
                all_checked,
            } => {
                let mut s = String::new();
                let marker = if *all_checked { "- [x] " } else { "- [ ] " };
                for i in 0..*items {
                    s.push_str(&format!("{marker}task {}\n", i + 1));
                }
                if *trailing_empty {
                    s.push_str(&format!("{marker}\n"));
                }
                s
            }
            DocumentTemplate::CodeFence => "intro paragraph\n\n```rust\nfn main() {}\n```\n".into(),
            DocumentTemplate::Table { rows } => {
                let mut s = String::from("| Col A | Col B |\n| --- | --- |\n");
                for i in 0..*rows {
                    s.push_str(&format!("| cell {}a | cell {}b |\n", i + 1, i + 1));
                }
                s.push('\n');
                s
            }
            DocumentTemplate::Blockquote { paragraphs } => (0..*paragraphs)
                .map(|i| format!("> quoted paragraph {}\n", i + 1))
                .collect(),
            DocumentTemplate::Mixed => {
                let mut s = String::new();
                s.push_str("# Top heading\n\n");
                s.push_str("intro paragraph with **bold** and _italic_ and `code`.\n\n");
                s.push_str("- bullet 1\n");
                s.push_str("- bullet 2\n");
                s.push_str("- [ ] task 1\n");
                s.push_str("- [x] task 2 done\n\n");
                s.push_str("```rust\nlet x = 1;\n```\n\n");
                s.push_str("## Sub heading\n\n");
                s.push_str("trailing paragraph.\n");
                s
            }
            DocumentTemplate::Frontmatter => "---\ntitle: Example\n---\n\n# Body\n\nText.\n".into(),
            DocumentTemplate::Unicode => "Café 👩🏽‍💻 **bold**\n\nSecond paragraph.\n".into(),
            DocumentTemplate::Whitespace => "café 🌍".into(),
            DocumentTemplate::MouseSelection => "# Notepad smoke C\n\nThis file should open in the last active editor window.\n\nЭто прекрасно! Это прекрасно аффы\n\nваф фыва".into(),
            DocumentTemplate::DeepPointer => (0..120)
                .map(|index| format!("Section {index:03}: Alpha beta gamma delta.\n\n"))
                .collect(),
            DocumentTemplate::MiddleList => "- First\n- \n- Following".into(),
            DocumentTemplate::StableHints => "## Stable heading\n\nA **bold word** and a [link](https://example.com) stay still.\n\n- First item\n- Second item\n\n| Item | Value |\n| --- | --- |\n| One | Two |\n\n```rust\nlet answer = 42;\n```\n\nAfter code.\n".into(),
            DocumentTemplate::FindContent => FIND_CONTENT.into(),
            DocumentTemplate::FindDeepContent => (0..120)
                .map(|index| format!("Section {index:03}: Alpha beta gamma delta.\n\n"))
                .collect(),
        }
    }
}

/// A single user-visible scenario: load the template, run the actions,
/// assert the invariants.
#[derive(Clone, Debug)]
pub struct Scenario {
    pub name: String,
    pub template: DocumentTemplate,
    /// Optional starting caret position (bytes from start of doc).
    pub setup_caret: Option<usize>,
    pub actions: Vec<Action>,
    pub invariants: Vec<Invariant>,
}

#[derive(Clone, Debug)]
pub enum Invariant {
    /// The final source must contain this substring.
    SourceContains(String),
    /// The final source must NOT contain this substring.
    SourceNotContains(String),
    /// The final caret must be at this byte offset.
    CaretAt(usize),
    /// The final caret must lie inside the rendered top-level block at
    /// `index` (0-based).
    CaretInBlock(usize),
    /// The final number of top-level blocks must equal this value.
    BlockCount(usize),
    /// The final mode must be one of these.
    Mode(Vec<EditorMode>),
    /// The final selection must equal this range.
    Selection(Range<usize>),
    /// The caret must stay within a nearby source section after navigation.
    CaretInRange(Range<usize>),
    /// The active rich caret must be painted inside its viewport.
    CaretVisible,
    /// The active rich caret must be visible after every scenario action.
    CaretVisibleAfterEveryAction,
    /// The rich viewport must remain past the first block after every action.
    ViewportFirstBlockAtLeastAfterEveryAction(usize),
    /// The focused source caret must be visible from this 1-based action step.
    SourceCaretVisibleFromStep(usize),
    /// The source viewport must stay away from the document start from this step.
    SourceViewportScrolledFromStep(usize),
    InputOwnedBy(InputOwner),
    SourceEquals(String),
    /// Exact intermediate source, including native character-by-character undo.
    SourceAtSteps(Vec<(usize, String)>),
    /// An ordinary empty paragraph must not retain its former container hint.
    PlainContextAtSteps(Vec<usize>),
    RichDisplayNotContains(String),
    RichDisplayContains(String),
    /// The active tab must be fully visible within its horizontal strip.
    ActiveTabVisible,
    TableControlsHidden,
    TableControlsVisible,
    TableShape {
        rows: usize,
        columns: usize,
    },
    /// Expected idle/input blink phase after each action, including step zero.
    CaretBlinkSequence(Vec<bool>),
    /// Probe selected steps when another step deliberately selects a widget draft.
    CaretBlinkAtSteps(Vec<(usize, bool)>),
    /// Exact source stops at selected journey steps, including atomic entities.
    CaretAtSteps(Vec<(usize, usize)>),
    /// Check source-backed native caret stops immediately after whitespace input.
    WhitespaceAdvancesCaret,
    /// The second Enter in an already empty paragraph must not create history.
    RepeatedEnterIsNoopAtStep(usize),
    EnterMovesCaretDownAtStep(usize),
    /// Filling an already laid-out empty paragraph must not move its baseline.
    CaretStaysOnSameRowAtStep(usize),
    WidgetDraftEquals(String),
    /// Record and validate each paint pass while input is in flight.
    ObserveEveryPaint,
    /// Deleting on an already visible row must preserve the manual viewport.
    VisibleDeletionKeepsViewport,
    /// The linked context must have distinct native cursor/selection quads,
    /// while the peer's real selection and manual viewport stay untouched.
    ShadowVisibleAtSteps(Vec<usize>),
    /// Literal fixture expectations, independent of the production Find model.
    FindAtSteps(Vec<FindExpectation>),
}

#[derive(Clone, Debug)]
pub struct FindExpectation {
    pub step: usize,
    pub query: String,
    pub matches: Vec<Range<usize>>,
    pub current: Option<Range<usize>>,
    pub pane: InputOwner,
}

// ---------------------------------------------------------------------------
// Curated scenarios
// ---------------------------------------------------------------------------

/// Hand-authored scenarios that exercise user-visible behaviour. Each
/// one was either reported as a regression, requested in a review, or
/// is the canonical example for a class of behaviour (e.g. empty list
/// item + Enter exits the list).
pub fn curated_scenarios() -> Vec<Scenario> {
    let mixed_source = DocumentTemplate::Mixed.render();
    let unicode_source = DocumentTemplate::Unicode.render();
    let emoji_start = unicode_source.find('👩').unwrap();
    let emoji_end = emoji_start + "👩🏽‍💻".len();
    let code_body = mixed_source
        .find("let x = 1;")
        .expect("mixed fixture contains a fenced code body");
    let stable_hints = DocumentTemplate::StableHints.render();
    let long_destination = format!("https://example.net/{}", "long-path/".repeat(20));
    let mut scenarios = vec![
        Scenario {
            name: "list_middle_exit_click_repeat_type_separates_following_list".into(),
            template: DocumentTemplate::MiddleList,
            setup_caret: Some(10),
            actions: vec![
                Action::Keystroke("enter".into()),
                Action::MouseClick {
                    pane: InputOwner::Wysiwyg,
                    offset: 8,
                    count: 1,
                },
                Action::Keystroke("enter".into()),
                Action::Keystroke("enter".into()),
                Action::InsertText("New".into()),
                Action::Keystroke("cmd-z".into()),
                Action::Keystroke("cmd-shift-z".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals("- First\n\nNew\n\n- Following".into()),
                Invariant::SourceAtSteps(vec![(6, "- First\n\n- Following".into())]),
                Invariant::PlainContextAtSteps(vec![1, 2, 3, 4, 6]),
                Invariant::BlockCount(3),
                Invariant::CaretVisibleAfterEveryAction,
                Invariant::RepeatedEnterIsNoopAtStep(3),
                Invariant::CaretStaysOnSameRowAtStep(5),
                Invariant::ObserveEveryPaint,
            ],
        },
        Scenario {
            name: "list_literal_dash_never_flashes_an_empty_bullet".into(),
            template: DocumentTemplate::Empty,
            setup_caret: None,
            actions: vec![
                Action::InsertText("-".into()),
                Action::InsertText("вавав".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals("\\-вавав".into()),
                Invariant::BlockCount(1),
                Invariant::RichDisplayContains("-вавав".into()),
                Invariant::CaretVisibleAfterEveryAction,
                Invariant::ObserveEveryPaint,
            ],
        },
        Scenario {
            name: "list_backspace_cancels_autoformat_and_preserves_literal_marker".into(),
            template: DocumentTemplate::Empty,
            setup_caret: None,
            actions: vec![
                Action::InsertText("-".into()),
                Action::InsertText(" ".into()),
                Action::Keystroke("backspace".into()),
                Action::InsertText("plain".into()),
                Action::Keystroke("cmd-z".into()),
                Action::Keystroke("cmd-shift-z".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals("\\- plain".into()),
                Invariant::BlockCount(1),
                Invariant::RichDisplayContains("- plain".into()),
                Invariant::CaretVisibleAfterEveryAction,
                Invariant::ObserveEveryPaint,
            ],
        },
        Scenario {
            name: "list_double_enter_click_blank_repeat_then_type_stays_outside_list".into(),
            template: DocumentTemplate::Empty,
            setup_caret: None,
            actions: vec![
                Action::InsertText("-".into()),
                Action::InsertText(" ".into()),
                Action::InsertText("Item".into()),
                Action::Keystroke("enter".into()),
                Action::Keystroke("enter".into()),
                Action::MouseClick {
                    pane: InputOwner::Wysiwyg,
                    offset: 7,
                    count: 1,
                },
                Action::Keystroke("enter".into()),
                Action::Keystroke("enter".into()),
                Action::InsertText("After".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals("- Item\n\nAfter".into()),
                Invariant::BlockCount(2),
                Invariant::CaretVisibleAfterEveryAction,
                Invariant::RepeatedEnterIsNoopAtStep(7),
                Invariant::CaretStaysOnSameRowAtStep(9),
                Invariant::ObserveEveryPaint,
            ],
        },
        Scenario {
            name: "split_shadow_source_reversed_unicode_drag_is_passive".into(),
            template: DocumentTemplate::Unicode,
            setup_caret: Some(emoji_start),
            actions: vec![
                Action::Keystroke("alt-cmd-3".into()),
                Action::FocusSource,
                Action::MousePress {
                    pane: InputOwner::Source,
                    offset: emoji_end,
                },
                Action::MouseMove {
                    pane: InputOwner::Source,
                    offset: emoji_start,
                },
                Action::MouseRelease {
                    pane: InputOwner::Source,
                    offset: emoji_start,
                },
            ],
            invariants: vec![
                Invariant::SourceEquals(DocumentTemplate::Unicode.render()),
                Invariant::Selection(emoji_start..emoji_end),
                Invariant::ShadowVisibleAtSteps(vec![3, 4, 5]),
                Invariant::ObserveEveryPaint,
            ],
        },
        Scenario {
            name: "split_shadow_source_drag_and_blink_are_passive".into(),
            template: DocumentTemplate::Mixed,
            setup_caret: Some(20),
            actions: vec![
                Action::Keystroke("alt-cmd-3".into()),
                Action::FocusSource,
                Action::MouseDrag {
                    pane: InputOwner::Source,
                    start: 20,
                    end: 33,
                },
                Action::Keystroke("left".into()),
                Action::WaitMillis(400),
                Action::WaitMillis(400),
            ],
            invariants: vec![
                Invariant::SourceEquals(mixed_source.clone()),
                Invariant::ShadowVisibleAtSteps(vec![3, 4, 5, 6]),
                Invariant::ObserveEveryPaint,
            ],
        },
        Scenario {
            name: "split_shadow_rich_reversed_unicode_selection_is_passive".into(),
            template: DocumentTemplate::Unicode,
            setup_caret: Some(emoji_start),
            actions: vec![
                Action::Keystroke("alt-cmd-3".into()),
                Action::MousePress {
                    pane: InputOwner::Wysiwyg,
                    offset: emoji_end,
                },
                Action::MouseMove {
                    pane: InputOwner::Wysiwyg,
                    offset: emoji_start,
                },
                Action::MouseRelease {
                    pane: InputOwner::Wysiwyg,
                    offset: emoji_start,
                },
                Action::WaitMillis(400),
            ],
            invariants: vec![
                Invariant::SourceEquals(DocumentTemplate::Unicode.render()),
                Invariant::Selection(emoji_start..emoji_end),
                Invariant::ShadowVisibleAtSteps(vec![2, 3, 4, 5]),
            ],
        },
        Scenario {
            name: "split_shadow_hidden_heading_prefix_projects_without_reflow".into(),
            template: DocumentTemplate::Mixed,
            setup_caret: Some(20),
            actions: vec![
                Action::Keystroke("alt-cmd-3".into()),
                Action::FocusSource,
                Action::JumpTo(0),
                Action::JumpTo(1),
                Action::JumpTo(3),
            ],
            invariants: vec![
                Invariant::SourceEquals(mixed_source.clone()),
                Invariant::ShadowVisibleAtSteps(vec![3, 4, 5]),
            ],
        },
        Scenario {
            name: "split_shadow_disappears_for_palette_and_standalone_mode".into(),
            template: DocumentTemplate::Mixed,
            setup_caret: Some(20),
            actions: vec![
                Action::Keystroke("alt-cmd-3".into()),
                Action::FocusSource,
                Action::SelectRange { start: 20, end: 33 },
                Action::Keystroke("cmd-shift-p".into()),
                Action::Keystroke("escape".into()),
                Action::Keystroke("alt-cmd-2".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals(mixed_source.clone()),
                Invariant::ShadowVisibleAtSteps(vec![3, 5]),
                Invariant::Mode(vec![EditorMode::Source]),
            ],
        },
        Scenario {
            name: "spaces_in_an_empty_paragraph_keep_the_next_letter_in_place".into(),
            template: DocumentTemplate::Whitespace,
            setup_caret: Some("café 🌍".len()),
            actions: vec![
                Action::Keystroke("enter".into()),
                Action::InsertText(" ".into()),
                Action::InsertText(" ".into()),
                Action::Keystroke("enter".into()),
                Action::InsertText("i".into()),
            ],
            invariants: vec![
                Invariant::SourceContains("café 🌍\n\n  i".into()),
                Invariant::RichDisplayContains("  i".into()),
                Invariant::CaretVisibleAfterEveryAction,
                Invariant::WhitespaceAdvancesCaret,
                Invariant::RepeatedEnterIsNoopAtStep(4),
                Invariant::EnterMovesCaretDownAtStep(1),
                Invariant::CaretStaysOnSameRowAtStep(5),
            ],
        },
        Scenario {
            name: "trailing_spaces_move_the_native_caret_immediately".into(),
            template: DocumentTemplate::Whitespace,
            setup_caret: Some("café 🌍".len()),
            actions: vec![
                Action::InsertText(" ".into()),
                Action::InsertText(" ".into()),
                Action::Keystroke("left".into()),
                Action::Keystroke("right".into()),
                Action::InsertText("次".into()),
                Action::Keystroke("cmd-z".into()),
                Action::Keystroke("cmd-shift-z".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals("café 🌍  次".into()),
                Invariant::CaretVisibleAfterEveryAction,
                Invariant::WhitespaceAdvancesCaret,
                Invariant::CaretAtSteps(vec![(3, "café 🌍 ".len()), (4, "café 🌍  ".len())]),
            ],
        },
        Scenario {
            name: "table_cell_spaces_move_the_caret_without_showing_padding".into(),
            template: DocumentTemplate::Table { rows: 2 },
            setup_caret: Some(
                DocumentTemplate::Table { rows: 2 }
                    .render()
                    .find("cell 1a")
                    .unwrap()
                    + "cell 1a".len(),
            ),
            actions: vec![
                Action::InsertText(" ".into()),
                Action::InsertText(" ".into()),
                Action::Keystroke("left".into()),
                Action::Keystroke("right".into()),
                Action::InsertText("i".into()),
                Action::Keystroke("cmd-z".into()),
                Action::Keystroke("cmd-shift-z".into()),
            ],
            invariants: vec![
                Invariant::SourceContains("cell 1a&#32;&#32;i".into()),
                Invariant::RichDisplayContains("cell 1a  i".into()),
                Invariant::CaretVisibleAfterEveryAction,
                Invariant::WhitespaceAdvancesCaret,
                Invariant::CaretAtSteps(vec![
                    (
                        3,
                        DocumentTemplate::Table { rows: 2 }
                            .render()
                            .find("cell 1a")
                            .unwrap()
                            + "cell 1a&#32;".len(),
                    ),
                    (
                        4,
                        DocumentTemplate::Table { rows: 2 }
                            .render()
                            .find("cell 1a")
                            .unwrap()
                            + "cell 1a&#32;&#32;".len(),
                    ),
                ]),
                Invariant::TableControlsVisible,
                Invariant::RichDisplayNotContains("|".into()),
            ],
        },
        Scenario {
            name: "enter_creates_one_empty_paragraph_and_keeps_undo".into(),
            template: DocumentTemplate::Whitespace,
            setup_caret: Some("café 🌍".len()),
            actions: vec![
                Action::InsertText(" ".into()),
                Action::InsertText(" ".into()),
                Action::Keystroke("enter".into()),
                Action::Keystroke("enter".into()),
                Action::Keystroke("cmd-z".into()),
                Action::Keystroke("cmd-shift-z".into()),
                Action::InsertText("次".into()),
            ],
            invariants: vec![
                Invariant::SourceContains("café 🌍  \n\n次".into()),
                Invariant::BlockCount(2),
                Invariant::CaretVisibleAfterEveryAction,
                Invariant::WhitespaceAdvancesCaret,
                Invariant::RepeatedEnterIsNoopAtStep(4),
                Invariant::EnterMovesCaretDownAtStep(3),
                Invariant::CaretStaysOnSameRowAtStep(7),
            ],
        },
        Scenario {
            name: "rich_caret_blinks_and_boundary_input_wakes_it".into(),
            template: DocumentTemplate::Whitespace,
            setup_caret: Some("café 🌍".len()),
            actions: vec![
                Action::WaitMillis(450),
                Action::InsertText(" ".into()),
                Action::WaitMillis(120),
                Action::WaitMillis(300),
                Action::Keystroke("right".into()),
                Action::WaitMillis(450),
                Action::Keystroke("enter".into()),
                Action::WaitMillis(450),
                Action::Keystroke("enter".into()),
            ],
            invariants: vec![
                Invariant::CaretBlinkSequence(vec![
                    true, false, true, true, false, true, false, true, false, true,
                ]),
                Invariant::RepeatedEnterIsNoopAtStep(9),
            ],
        },
        Scenario {
            name: "source_caret_blinks_and_spaces_remain_literal".into(),
            template: DocumentTemplate::Whitespace,
            setup_caret: Some("café 🌍".len()),
            actions: vec![
                Action::Keystroke("alt-cmd-2".into()),
                Action::WaitMillis(450),
                Action::InsertText(" ".into()),
                Action::WaitMillis(120),
                Action::WaitMillis(300),
                Action::Keystroke("right".into()),
                Action::Keystroke("enter".into()),
                Action::Keystroke("enter".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals("café 🌍 \n\n".into()),
                Invariant::CaretBlinkSequence(vec![
                    true, true, false, true, true, false, true, true, true,
                ]),
            ],
        },
        Scenario {
            name: "rich_undo_and_redo_wake_the_idle_caret".into(),
            template: DocumentTemplate::Whitespace,
            setup_caret: Some("café 🌍".len()),
            actions: vec![
                Action::InsertText(" ".into()),
                Action::WaitMillis(450),
                Action::Keystroke("cmd-z".into()),
                Action::WaitMillis(450),
                Action::Keystroke("cmd-shift-z".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals("café 🌍 ".into()),
                Invariant::CaretBlinkSequence(vec![true, true, false, true, false, true]),
            ],
        },
        Scenario {
            name: "link_draft_typing_and_deletion_wake_the_idle_caret".into(),
            template: DocumentTemplate::StableHints,
            setup_caret: Some(stable_hints.find("link]").unwrap() + 2),
            actions: vec![
                Action::Keystroke("cmd-k".into()),
                Action::Keystroke("end".into()),
                Action::WaitMillis(450),
                Action::InsertText("x".into()),
                Action::WaitMillis(450),
                Action::Keystroke("backspace".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals(stable_hints.clone()),
                Invariant::InputOwnedBy(InputOwner::Widget("link-destination".into())),
                Invariant::CaretBlinkAtSteps(vec![
                    (0, true),
                    (2, true),
                    (3, false),
                    (4, true),
                    (5, false),
                    (6, true),
                ]),
            ],
        },
        Scenario {
            name: "link_destination_is_visible_with_markup_hints_disabled".into(),
            template: DocumentTemplate::StableHints,
            setup_caret: Some(stable_hints.find("link]").unwrap() + 2),
            actions: vec![
                Action::Keystroke("alt-cmd-4".into()),
                Action::Keystroke("cmd-k".into()),
                Action::Keystroke("cmd-a".into()),
                Action::InsertText(long_destination.clone()),
                Action::Keystroke("cmd-z".into()),
                Action::Keystroke("cmd-shift-z".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals(stable_hints.clone()),
                Invariant::InputOwnedBy(InputOwner::Widget("link-destination".into())),
                Invariant::WidgetDraftEquals(long_destination),
                Invariant::CaretVisibleAfterEveryAction,
            ],
        },
        Scenario {
            name: "existing_link_destination_commits_without_reflow".into(),
            template: DocumentTemplate::StableHints,
            setup_caret: Some(stable_hints.find("link]").unwrap() + 2),
            actions: vec![
                Action::Keystroke("cmd-k".into()),
                Action::Keystroke("cmd-a".into()),
                Action::InsertText("https://example.net/new".into()),
                Action::Keystroke("enter".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals(
                    stable_hints.replace("https://example.com", "https://example.net/new"),
                ),
                Invariant::InputOwnedBy(InputOwner::Wysiwyg),
                Invariant::CaretVisibleAfterEveryAction,
            ],
        },
        Scenario {
            name: "link_destination_escape_preserves_document".into(),
            template: DocumentTemplate::StableHints,
            setup_caret: Some(stable_hints.find("link]").unwrap() + 2),
            actions: vec![
                Action::Keystroke("cmd-k".into()),
                Action::Keystroke("cmd-a".into()),
                Action::InsertText("https://cancelled.example".into()),
                Action::Keystroke("escape".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals(stable_hints.clone()),
                Invariant::InputOwnedBy(InputOwner::Wysiwyg),
                Invariant::CaretVisibleAfterEveryAction,
            ],
        },
        Scenario {
            name: "new_link_destination_accepts_native_typing".into(),
            template: DocumentTemplate::Plain { paragraphs: 1 },
            setup_caret: Some(3),
            actions: vec![
                Action::Keystroke("cmd-k".into()),
                Action::InsertText("https://example.net".into()),
                Action::Keystroke("enter".into()),
            ],
            invariants: vec![
                Invariant::SourceContains("[Paragraph](https://example.net)".into()),
                Invariant::InputOwnedBy(InputOwner::Wysiwyg),
                Invariant::CaretVisibleAfterEveryAction,
            ],
        },
        Scenario {
            name: "wysiwyg_hints_do_not_reflow_text".into(),
            template: DocumentTemplate::StableHints,
            setup_caret: Some(4),
            actions: vec![
                Action::JumpTo(stable_hints.find("bold word").unwrap() + 2),
                Action::Keystroke("alt-cmd-4".into()),
                Action::Keystroke("alt-cmd-4".into()),
                Action::JumpTo(stable_hints.find("link]").unwrap() + 2),
                Action::JumpTo(stable_hints.find("First item").unwrap() + 2),
                Action::JumpTo(stable_hints.find("One | Two").unwrap() + 1),
                Action::JumpTo(stable_hints.find("let answer").unwrap() + 2),
                Action::Keystroke("alt-cmd-4".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals(stable_hints.clone()),
                Invariant::CaretVisibleAfterEveryAction,
            ],
        },
        Scenario {
            name: "table_cells_remain_clean_with_context_only_controls".into(),
            template: DocumentTemplate::Table { rows: 2 },
            setup_caret: Some(
                DocumentTemplate::Table { rows: 2 }
                    .render()
                    .find("cell 1a")
                    .unwrap()
                    + 2,
            ),
            actions: vec![Action::Keystroke("right".into())],
            invariants: vec![
                Invariant::CaretVisible,
                Invariant::TableControlsVisible,
                Invariant::RichDisplayNotContains("|".into()),
                Invariant::RichDisplayNotContains("---".into()),
            ],
        },
        Scenario {
            name: "bullet_enter_creates_visible_item_and_empty_enter_exits".into(),
            template: DocumentTemplate::BulletList {
                items: 1,
                trailing_empty: false,
            },
            setup_caret: Some("- item 1".len()),
            actions: vec![
                Action::Keystroke("enter".into()),
                Action::InsertText("Second".into()),
                Action::Keystroke("enter".into()),
                Action::Keystroke("enter".into()),
                Action::InsertText("After list".into()),
            ],
            invariants: vec![
                Invariant::SourceContains("- Second".into()),
                Invariant::SourceContains("After list".into()),
                Invariant::SourceNotContains("- After list".into()),
                Invariant::BlockCount(2),
                Invariant::CaretVisibleAfterEveryAction,
            ],
        },
        Scenario {
            name: "ordered_enter_keeps_caret_visible".into(),
            template: DocumentTemplate::OrderedList {
                items: 1,
                trailing_empty: false,
            },
            setup_caret: Some("1. item 1".len()),
            actions: vec![
                Action::Keystroke("enter".into()),
                Action::InsertText("Next".into()),
            ],
            invariants: vec![
                // Repeated "1." markers preserve the document's source style;
                // visual numbering is derived from the ordered list.
                Invariant::SourceContains("1. Next".into()),
                Invariant::CaretVisibleAfterEveryAction,
            ],
        },
        Scenario {
            name: "heading_enter_exposes_empty_paragraph_caret".into(),
            template: DocumentTemplate::Headings { levels: vec![1] },
            setup_caret: Some("# Heading 1".len()),
            actions: vec![
                Action::Keystroke("enter".into()),
                Action::InsertText("Body text".into()),
            ],
            invariants: vec![
                Invariant::SourceContains("Body text".into()),
                Invariant::BlockCount(2),
                Invariant::CaretVisibleAfterEveryAction,
            ],
        },
        Scenario {
            name: "native_new_tab_and_cycling_shortcuts_keep_buffers".into(),
            template: DocumentTemplate::Empty,
            setup_caret: None,
            actions: vec![
                Action::InsertText("First draft".into()),
                Action::Keystroke("cmd-t".into()),
                Action::InsertText("Second draft".into()),
                Action::Keystroke("ctrl-shift-tab".into()),
                Action::Keystroke("ctrl-tab".into()),
                Action::Keystroke("cmd-shift-[".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals("First draft".into()),
                Invariant::InputOwnedBy(InputOwner::Wysiwyg),
                Invariant::CaretVisible,
            ],
        },
        Scenario {
            name: "many_tabs_keep_active_editor_and_original_buffer".into(),
            template: DocumentTemplate::Unicode,
            setup_caret: Some(0),
            actions: (0..18)
                .map(|_| Action::Keystroke("cmd-t".into()))
                .chain([
                    Action::SwitchTab(0),
                    Action::Keystroke("cmd-shift-]".into()),
                    Action::Keystroke("cmd-shift-[".into()),
                ])
                .collect(),
            invariants: vec![
                Invariant::SourceEquals(DocumentTemplate::Unicode.render()),
                Invariant::InputOwnedBy(InputOwner::Wysiwyg),
                Invariant::CaretVisible,
                Invariant::ActiveTabVisible,
            ],
        },
        Scenario {
            name: "highlight_styles_preserve_geometry_selection_and_source".into(),
            template: DocumentTemplate::StableHints,
            setup_caret: Some(4),
            actions: vec![
                Action::HighlightStyle(crate::config::HighlightStyle::Ocean),
                Action::HighlightStyle(crate::config::HighlightStyle::Forest),
                Action::HighlightStyle(crate::config::HighlightStyle::Native),
            ],
            invariants: vec![
                Invariant::SourceEquals(stable_hints),
                Invariant::CaretVisibleAfterEveryAction,
            ],
        },
        // The example from the user request: pressing Enter on the
        // empty trailing item of an unchecked task list must close the
        // list and drop the caret into a fresh paragraph below.
        Scenario {
            name: "empty_task_item_enter_exits_and_continues_as_paragraph".into(),
            template: DocumentTemplate::TaskList {
                items: 2,
                trailing_empty: true,
                all_checked: false,
            },
            setup_caret: None,
            actions: vec![
                Action::JumpTo(usize::MAX),
                Action::Keystroke("enter".into()),
            ],
            invariants: vec![
                Invariant::SourceContains("- [ ] task 1".into()),
                Invariant::SourceContains("- [ ] task 2".into()),
                // The empty trailing marker must be stripped: the rich
                // engine rewrites `\n- [ ] \n` to `\n\n`, so the
                // marker AND its trailing newline disappear from the
                // source. The empty paragraph zone becomes a real
                // block only once the user types, matching the
                // exit-the-list contract documented in
                // `empty_list_item_enter_exits_the_list` in
                // crates/markrust-core/src/rich/command.rs.
                Invariant::SourceNotContains("- [ ] \n".into()),
                Invariant::Mode(vec![EditorMode::Wysiwyg]),
            ],
        },
        // Same shape but with a checked task list: the empty `[x]`
        // item must also exit and become a paragraph (the checkbox
        // state is not propagated to the new paragraph).
        Scenario {
            name: "empty_checked_task_item_enter_exits".into(),
            template: DocumentTemplate::TaskList {
                items: 1,
                trailing_empty: true,
                all_checked: true,
            },
            setup_caret: None,
            actions: vec![
                Action::JumpTo(usize::MAX),
                Action::Keystroke("enter".into()),
            ],
            invariants: vec![
                Invariant::SourceContains("- [x] task 1".into()),
                Invariant::SourceNotContains("- [x] \n".into()),
            ],
        },
        // Empty bullet item + Enter exits the list.
        Scenario {
            name: "empty_bullet_item_enter_exits".into(),
            template: DocumentTemplate::BulletList {
                items: 3,
                trailing_empty: true,
            },
            setup_caret: None,
            actions: vec![
                Action::JumpTo(usize::MAX),
                Action::Keystroke("enter".into()),
            ],
            invariants: vec![
                Invariant::SourceContains("- item 1".into()),
                Invariant::SourceNotContains("- \n".into()),
            ],
        },
        // Empty ordered item + Enter exits the list.
        Scenario {
            name: "empty_ordered_item_enter_exits".into(),
            template: DocumentTemplate::OrderedList {
                items: 2,
                trailing_empty: true,
            },
            setup_caret: None,
            actions: vec![
                Action::JumpTo(usize::MAX),
                Action::Keystroke("enter".into()),
            ],
            invariants: vec![
                Invariant::SourceContains("1. item 1".into()),
                Invariant::SourceNotContains("2. \n".into()),
            ],
        },
        // Mode switch round-trip: WYSIWYG → Source → Split → WYSIWYG
        // must preserve the source verbatim.
        Scenario {
            name: "mode_round_trip_preserves_source".into(),
            template: DocumentTemplate::Mixed,
            setup_caret: None,
            actions: vec![
                Action::Keystroke("alt-cmd-2".into()),
                Action::Keystroke("alt-cmd-3".into()),
                Action::Keystroke("alt-cmd-1".into()),
            ],
            invariants: vec![
                Invariant::Mode(vec![EditorMode::Wysiwyg]),
                Invariant::SourceEquals(mixed_source.clone()),
            ],
        },
        Scenario {
            name: "reversed_unicode_selection_survives_modes".into(),
            template: DocumentTemplate::Unicode,
            setup_caret: None,
            actions: vec![
                Action::SelectRange {
                    start: emoji_end,
                    end: emoji_start,
                },
                Action::Keystroke("alt-cmd-2".into()),
                Action::Keystroke("alt-cmd-3".into()),
                Action::Keystroke("alt-cmd-1".into()),
            ],
            invariants: vec![
                Invariant::Selection(emoji_start..emoji_end),
                Invariant::SourceEquals(unicode_source),
            ],
        },
        Scenario {
            name: "unicode_selection_replacement_undo_restores_context".into(),
            template: DocumentTemplate::Unicode,
            setup_caret: None,
            actions: vec![
                Action::SelectRange {
                    start: emoji_end,
                    end: emoji_start,
                },
                Action::InsertText("X".into()),
                Action::Keystroke("cmd-z".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals(DocumentTemplate::Unicode.render()),
                Invariant::Selection(emoji_start..emoji_end),
            ],
        },
        Scenario {
            name: "markup_hint_toggle_preserves_selected_content".into(),
            template: DocumentTemplate::Mixed,
            setup_caret: None,
            actions: vec![
                Action::SelectRange { start: 36, end: 40 },
                Action::Keystroke("alt-cmd-4".into()),
            ],
            invariants: vec![
                Invariant::Selection(36..40),
                Invariant::SourceEquals(mixed_source.clone()),
                Invariant::RichDisplayNotContains("**".into()),
            ],
        },
        Scenario {
            name: "split_source_tab_return_restores_input_owner".into(),
            template: DocumentTemplate::Mixed,
            setup_caret: Some(20),
            actions: vec![
                Action::Keystroke("alt-cmd-3".into()),
                Action::FocusSource,
                Action::SelectRange { start: 20, end: 33 },
                Action::Keystroke("alt-cmd-3".into()),
                Action::NewTab,
                Action::SwitchTab(0),
            ],
            invariants: vec![
                Invariant::Selection(20..33),
                Invariant::SourceEquals(mixed_source.clone()),
                Invariant::InputOwnedBy(InputOwner::Source),
            ],
        },
        Scenario {
            name: "frontmatter_edit_from_split_focuses_visible_source".into(),
            template: DocumentTemplate::Frontmatter,
            setup_caret: None,
            actions: vec![
                Action::Keystroke("alt-cmd-3".into()),
                Action::EditFrontmatter,
                Action::Keystroke("down".into()),
                Action::Keystroke("end".into()),
                Action::InsertText(" improved".into()),
            ],
            invariants: vec![
                Invariant::Mode(vec![EditorMode::Source]),
                Invariant::InputOwnedBy(InputOwner::Source),
                Invariant::SourceContains("title: Example improved".into()),
            ],
        },
        Scenario {
            name: "split_delete_in_source_then_type_in_rich".into(),
            template: DocumentTemplate::Mixed,
            setup_caret: None,
            actions: vec![
                Action::JumpTo(usize::MAX),
                Action::Keystroke("alt-cmd-3".into()),
                Action::FocusSource,
                Action::Keystroke("cmd-a".into()),
                Action::Keystroke("backspace".into()),
                Action::FocusRich,
                Action::InsertText("Fresh".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals("Fresh".into()),
                Invariant::CaretAt(5),
            ],
        },
        Scenario {
            name: "split_delete_in_rich_then_type_in_source".into(),
            template: DocumentTemplate::Mixed,
            setup_caret: None,
            actions: vec![
                Action::Keystroke("alt-cmd-2".into()),
                Action::JumpTo(usize::MAX),
                Action::Keystroke("alt-cmd-3".into()),
                Action::FocusRich,
                Action::Keystroke("cmd-a".into()),
                Action::Keystroke("backspace".into()),
                Action::FocusSource,
                Action::InsertText("Fresh".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals("Fresh".into()),
                Invariant::CaretAt(5),
            ],
        },
        Scenario {
            name: "long_document_edit_keeps_caret_visible".into(),
            template: DocumentTemplate::SeparatedParagraphs { paragraphs: 120 },
            setup_caret: Some(0),
            actions: vec![
                Action::JumpTo(usize::MAX),
                Action::InsertText("✓".into()),
                Action::Keystroke("up".into()),
                Action::Keystroke("down".into()),
            ],
            invariants: vec![
                Invariant::SourceContains("✓".into()),
                // EOF is the authored blank after paragraph 120, not its tail.
                Invariant::SourceEquals(format!(
                    "{}✓",
                    DocumentTemplate::SeparatedParagraphs { paragraphs: 120 }.render()
                )),
                Invariant::BlockCount(121),
                Invariant::CaretVisibleAfterEveryAction,
                Invariant::ViewportFirstBlockAtLeastAfterEveryAction(1),
            ],
        },
        Scenario {
            name: "up_from_raw_block_reaches_adjacent_section".into(),
            template: DocumentTemplate::Mixed,
            setup_caret: Some(code_body),
            actions: vec![Action::Keystroke("up".into())],
            invariants: vec![
                Invariant::SourceContains("let x = 1;".into()),
                Invariant::CaretInRange(71..133),
                Invariant::CaretVisible,
            ],
        },
        Scenario {
            name: "source_long_document_edit_keeps_caret_visible".into(),
            template: DocumentTemplate::SeparatedParagraphs { paragraphs: 120 },
            setup_caret: Some(0),
            actions: vec![
                Action::Keystroke("alt-cmd-2".into()),
                Action::JumpTo(usize::MAX),
                Action::InsertText("✓".into()),
                Action::Keystroke("up".into()),
                Action::Keystroke("down".into()),
            ],
            invariants: vec![
                Invariant::SourceContains("✓".into()),
                Invariant::Mode(vec![EditorMode::Source]),
                Invariant::SourceCaretVisibleFromStep(1),
                Invariant::SourceViewportScrolledFromStep(2),
            ],
        },
        Scenario {
            name: "split_source_long_document_edit_keeps_caret_visible".into(),
            template: DocumentTemplate::SeparatedParagraphs { paragraphs: 120 },
            setup_caret: Some(0),
            actions: vec![
                Action::Keystroke("alt-cmd-3".into()),
                Action::FocusSource,
                Action::JumpTo(usize::MAX),
                Action::InsertText("✓".into()),
                Action::Keystroke("up".into()),
                Action::Keystroke("down".into()),
            ],
            invariants: vec![
                Invariant::SourceContains("✓".into()),
                Invariant::Mode(vec![EditorMode::Split]),
                Invariant::SourceCaretVisibleFromStep(2),
                Invariant::SourceViewportScrolledFromStep(3),
            ],
        },
        // Typing into an empty document produces a single paragraph that
        // contains the typed text. This exercises the rich engine's
        // IME / EntityInputHandler path (the one `dispatch_keystroke`
        // uses for printable characters) end-to-end.
        Scenario {
            name: "typing_into_empty_doc_creates_paragraph".into(),
            template: DocumentTemplate::Empty,
            setup_caret: Some(0),
            actions: vec![Action::InsertText("Hello world".into())],
            invariants: vec![Invariant::SourceContains("Hello world".into())],
        },
        // Selection survives a Cmd-Shift-Right followed by typing.
        Scenario {
            name: "select_word_then_typing_replaces_it".into(),
            template: DocumentTemplate::Plain { paragraphs: 1 },
            setup_caret: Some(0),
            actions: vec![
                Action::Keystroke("shift-right".into()),
                Action::Keystroke("shift-right".into()),
                Action::Keystroke("shift-right".into()),
                Action::InsertText("X".into()),
            ],
            invariants: vec![
                Invariant::SourceContains("X".into()),
                Invariant::SourceNotContains("Paragraph".into()),
            ],
        },
        // Backspace at the very start of a non-empty line must not
        // swallow the previous line's content.
        Scenario {
            name: "backspace_at_start_keeps_prior_line".into(),
            template: DocumentTemplate::Plain { paragraphs: 2 },
            setup_caret: Some(0),
            actions: vec![
                Action::Keystroke("down".into()),
                Action::Keystroke("home".into()),
            ],
            invariants: vec![Invariant::SourceContains("Paragraph 1".into())],
        },
    ];
    scenarios.extend(pointer_scenarios());
    scenarios.extend(palette_scenarios());
    scenarios.extend(find_scenarios());
    scenarios
}

fn find_scenarios() -> Vec<Scenario> {
    let words: Vec<_> = FIND_CONTENT
        .match_indices("слово")
        .map(|(start, text)| start..start + text.len())
        .collect();
    let emoji: Vec<_> = FIND_CONTENT
        .match_indices("👩🏽‍💻")
        .map(|(start, text)| start..start + text.len())
        .collect();
    let wrapped_query = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega";
    let wrapped_start = FIND_CONTENT.find(wrapped_query).unwrap();
    let wrapped: Vec<_> =
        std::iter::once(wrapped_start..wrapped_start + wrapped_query.len()).collect();
    let expectation =
        |step, query: &str, matches: &[Range<usize>], current, pane| FindExpectation {
            step,
            query: query.into(),
            matches: matches.to_vec(),
            current,
            pane,
        };
    let mut scenarios = Vec::new();
    for (name, mode, pane, query, matches) in [
        (
            "source_casefold",
            "alt-cmd-2",
            InputOwner::Source,
            "СЛОВО",
            &words,
        ),
        (
            "wysiwyg_hidden_markup",
            "alt-cmd-1",
            InputOwner::Wysiwyg,
            "слово",
            &words,
        ),
        (
            "wysiwyg_grapheme",
            "alt-cmd-1",
            InputOwner::Wysiwyg,
            "👩🏽‍💻",
            &emoji,
        ),
        (
            "split_source_wrapped",
            "alt-cmd-3",
            InputOwner::Source,
            wrapped_query,
            &wrapped,
        ),
        (
            "split_wysiwyg_wrapped",
            "alt-cmd-3",
            InputOwner::Wysiwyg,
            wrapped_query,
            &wrapped,
        ),
    ] {
        scenarios.push(Scenario {
            name: format!("find_{name}_native_query_navigation_escape_preserves_body"),
            template: DocumentTemplate::FindContent,
            setup_caret: Some(3),
            actions: vec![
                Action::Keystroke(mode.into()),
                Action::MouseClick {
                    pane: pane.clone(),
                    offset: 3,
                    count: 1,
                },
                Action::Keystroke("cmd-f".into()),
                Action::Keystroke("cmd-a".into()),
                Action::Keystroke("backspace".into()),
                Action::InsertText(query.into()),
                Action::Keystroke("enter".into()),
                Action::Keystroke("shift-enter".into()),
                Action::Keystroke("escape".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals(FIND_CONTENT.into()),
                Invariant::Selection(matches[0].clone()),
                Invariant::InputOwnedBy(pane.clone()),
                Invariant::ObserveEveryPaint,
                Invariant::FindAtSteps(vec![
                    expectation(5, "", &[], None, pane.clone()),
                    expectation(6, query, matches, Some(matches[0].clone()), pane.clone()),
                    expectation(
                        7,
                        query,
                        matches,
                        Some(matches[1 % matches.len()].clone()),
                        pane.clone(),
                    ),
                    expectation(8, query, matches, Some(matches[0].clone()), pane),
                ]),
            ],
        });
    }
    scenarios.push(Scenario {
        name: "find_no_results_empty_query_never_edits_document".into(),
        template: DocumentTemplate::FindContent,
        setup_caret: Some(3),
        actions: vec![
            Action::Keystroke("cmd-f".into()),
            Action::Keystroke("cmd-a".into()),
            Action::Keystroke("backspace".into()),
            Action::InsertText("несуществующее".into()),
            Action::Keystroke("enter".into()),
            Action::Keystroke("cmd-a".into()),
            Action::Keystroke("backspace".into()),
            Action::Keystroke("escape".into()),
        ],
        invariants: vec![
            Invariant::SourceEquals(FIND_CONTENT.into()),
            Invariant::CaretAt(3),
            Invariant::InputOwnedBy(InputOwner::Wysiwyg),
            Invariant::ObserveEveryPaint,
            Invariant::FindAtSteps(vec![
                expectation(3, "", &[], None, InputOwner::Wysiwyg),
                expectation(4, "несуществующее", &[], None, InputOwner::Wysiwyg),
                expectation(5, "несуществующее", &[], None, InputOwner::Wysiwyg),
                expectation(7, "", &[], None, InputOwner::Wysiwyg),
            ]),
        ],
    });
    scenarios.push(Scenario {
        name: "find_tab_rebind_discards_stale_match_ranges".into(),
        template: DocumentTemplate::FindContent,
        setup_caret: Some(3),
        actions: vec![
            Action::Keystroke("cmd-f".into()),
            Action::Keystroke("cmd-a".into()),
            Action::Keystroke("backspace".into()),
            Action::InsertText("слово".into()),
            Action::NewTab,
            Action::SwitchTab(0),
            Action::Keystroke("escape".into()),
        ],
        invariants: vec![
            Invariant::SourceEquals(FIND_CONTENT.into()),
            Invariant::Selection(words[0].clone()),
            Invariant::InputOwnedBy(InputOwner::Wysiwyg),
            Invariant::ObserveEveryPaint,
            Invariant::FindAtSteps(vec![
                expectation(
                    4,
                    "слово",
                    &words,
                    Some(words[0].clone()),
                    InputOwner::Wysiwyg,
                ),
                expectation(5, "слово", &[], None, InputOwner::Wysiwyg),
                expectation(
                    6,
                    "слово",
                    &words,
                    Some(words[0].clone()),
                    InputOwner::Wysiwyg,
                ),
            ]),
        ],
    });
    scenarios.push(Scenario {
        name: "find_mode_rebind_preserves_query_and_real_selection".into(),
        template: DocumentTemplate::FindContent,
        setup_caret: Some(3),
        actions: vec![
            Action::Keystroke("cmd-f".into()),
            Action::Keystroke("cmd-a".into()),
            Action::Keystroke("backspace".into()),
            Action::InsertText("слово".into()),
            Action::Keystroke("alt-cmd-2".into()),
            Action::Keystroke("enter".into()),
            Action::Keystroke("escape".into()),
        ],
        invariants: vec![
            Invariant::SourceEquals(FIND_CONTENT.into()),
            Invariant::Selection(words[1].clone()),
            Invariant::Mode(vec![EditorMode::Source]),
            Invariant::InputOwnedBy(InputOwner::Source),
            Invariant::ObserveEveryPaint,
            Invariant::FindAtSteps(vec![
                expectation(
                    4,
                    "слово",
                    &words,
                    Some(words[0].clone()),
                    InputOwner::Wysiwyg,
                ),
                expectation(
                    5,
                    "слово",
                    &words,
                    Some(words[0].clone()),
                    InputOwner::Source,
                ),
                expectation(
                    6,
                    "слово",
                    &words,
                    Some(words[1].clone()),
                    InputOwner::Source,
                ),
            ]),
        ],
    });
    let deep_source = DocumentTemplate::FindDeepContent.render();
    let deep_query = "Section 003";
    let deep_start = deep_source.find(deep_query).unwrap();
    let deep_matches: Vec<_> = std::iter::once(deep_start..deep_start + deep_query.len()).collect();
    for (name, mode, pane) in [
        ("source", "alt-cmd-2", InputOwner::Source),
        ("wysiwyg", "alt-cmd-1", InputOwner::Wysiwyg),
    ] {
        let mut invariants = vec![
            Invariant::SourceEquals(deep_source.clone()),
            Invariant::Selection(deep_matches[0].clone()),
            Invariant::InputOwnedBy(pane.clone()),
            Invariant::ObserveEveryPaint,
            Invariant::FindAtSteps(vec![expectation(
                6,
                deep_query,
                &deep_matches,
                Some(deep_matches[0].clone()),
                pane.clone(),
            )]),
        ];
        invariants.push(if pane == InputOwner::Source {
            Invariant::SourceCaretVisibleFromStep(7)
        } else {
            Invariant::CaretVisible
        });
        scenarios.push(Scenario {
            name: format!("find_{name}_escape_commits_visible_result_without_old_caret_jump"),
            template: DocumentTemplate::FindDeepContent,
            setup_caret: Some(0),
            actions: vec![
                Action::Keystroke(mode.into()),
                Action::JumpTo(usize::MAX),
                Action::Keystroke("cmd-f".into()),
                Action::Keystroke("cmd-a".into()),
                Action::Keystroke("backspace".into()),
                Action::InsertText(deep_query.into()),
                Action::Keystroke("escape".into()),
            ],
            invariants,
        });
    }
    scenarios
}

fn palette_scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "palette_native_query_activates_literal_source_and_restores_input".into(),
            template: DocumentTemplate::MouseSelection,
            setup_caret: Some(16),
            actions: vec![
                Action::Keystroke("cmd-p".into()),
                Action::InsertText("Source".into()),
                Action::Keystroke("enter".into()),
                Action::InsertText("!".into()),
            ],
            invariants: vec![
                Invariant::Mode(vec![EditorMode::Source]),
                Invariant::InputOwnedBy(InputOwner::Source),
                Invariant::SourceContains("# Notepad smoke !C".into()),
            ],
        },
        Scenario {
            name: "palette_escape_restores_split_source_owner_without_query_leak".into(),
            template: DocumentTemplate::Empty,
            setup_caret: Some(0),
            actions: vec![
                Action::Keystroke("alt-cmd-3".into()),
                Action::FocusSource,
                Action::Keystroke("cmd-p".into()),
                Action::InsertText("Не документ".into()),
                Action::Keystroke("escape".into()),
                Action::InsertText("Сохранён".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals("Сохранён".into()),
                Invariant::InputOwnedBy(InputOwner::Source),
            ],
        },
        Scenario {
            name: "palette_outside_click_dismisses_and_restores_rich_input".into(),
            template: DocumentTemplate::Empty,
            setup_caret: Some(0),
            actions: vec![
                Action::Keystroke("cmd-p".into()),
                Action::InsertText("not found".into()),
                Action::PaletteOutsideClick,
                Action::InsertText("Text".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals("Text".into()),
                Invariant::InputOwnedBy(InputOwner::Wysiwyg),
            ],
        },
        Scenario {
            name: "palette_duplicate_tab_titles_choose_the_selected_tab_identity".into(),
            template: DocumentTemplate::MouseSelection,
            setup_caret: Some(16),
            actions: vec![
                Action::NewTab,
                Action::InsertText("Second draft".into()),
                Action::SwitchTab(0),
                Action::Keystroke("cmd-p".into()),
                Action::InsertText("Untitled".into()),
                Action::Keystroke("down".into()),
                Action::Keystroke("enter".into()),
                Action::InsertText("!".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals("Second draft!".into()),
                Invariant::InputOwnedBy(InputOwner::Wysiwyg),
            ],
        },
    ]
}

fn pointer_scenarios() -> Vec<Scenario> {
    let source = DocumentTemplate::MouseSelection.render();
    let start = source.find("аффы").unwrap();
    let end = start + "аффы".len();
    let mut scenarios = Vec::new();
    for (name, mode, owner) in [
        ("source", "alt-cmd-2", InputOwner::Source),
        ("wysiwyg", "alt-cmd-1", InputOwner::Wysiwyg),
        ("split_source", "alt-cmd-3", InputOwner::Source),
        ("split_wysiwyg", "alt-cmd-3", InputOwner::Wysiwyg),
    ] {
        scenarios.push(Scenario {
            name: format!("{name}_mouse_click_before_last_heading_glyph_keeps_exact_stop"),
            template: DocumentTemplate::MouseSelection,
            setup_caret: Some(0),
            actions: vec![
                Action::Keystroke(mode.into()),
                Action::MouseClick {
                    pane: owner.clone(),
                    offset: 16,
                    count: 1,
                },
            ],
            invariants: vec![
                Invariant::SourceEquals(source.clone()),
                Invariant::CaretAt(16),
                Invariant::InputOwnedBy(owner.clone()),
            ],
        });
        let empty_paragraph_source = DocumentTemplate::Whitespace.render();
        let below_content_source = if owner == InputOwner::Source {
            empty_paragraph_source
        } else {
            format!("{empty_paragraph_source}\n\n")
        };
        // Rich insertion lives at the beginning of the new blank source line
        // (before its terminator), while Source below-content clicks use EOF.
        let below_content_caret =
            below_content_source.len() - usize::from(owner == InputOwner::Wysiwyg);
        let mut below_content_invariants = vec![
            Invariant::CaretAt(below_content_caret),
            Invariant::SourceEquals(below_content_source),
            Invariant::InputOwnedBy(owner.clone()),
            Invariant::ObserveEveryPaint,
        ];
        if owner == InputOwner::Wysiwyg {
            below_content_invariants.push(Invariant::CaretVisible);
        }
        scenarios.push(Scenario {
            name: format!("{name}_pointer_below_content_release_before_paint_keeps_new_caret"),
            template: DocumentTemplate::Whitespace,
            setup_caret: Some(0),
            actions: vec![
                Action::Keystroke(mode.into()),
                Action::ClickBelowContentWithoutPaint {
                    pane: owner.clone(),
                },
            ],
            invariants: below_content_invariants,
        });
        scenarios.push(Scenario {
            name: format!("{name}_pointer_midviewport_delete_keeps_scroll"),
            template: DocumentTemplate::DeepPointer,
            setup_caret: Some(0),
            actions: vec![
                Action::Keystroke(mode.into()),
                Action::ScrollPane {
                    pane: owner.clone(),
                    delta_y: -1100.,
                },
                Action::ClickVisibleRow {
                    pane: owner.clone(),
                    vertical_fraction: 0.5,
                },
                Action::Keystroke("alt-backspace".into()),
                Action::Keystroke("alt-backspace".into()),
                Action::Keystroke("alt-backspace".into()),
            ],
            invariants: vec![
                Invariant::InputOwnedBy(owner.clone()),
                Invariant::ObserveEveryPaint,
                Invariant::VisibleDeletionKeepsViewport,
            ],
        });
        for bottom in [false, true] {
            scenarios.push(Scenario {
                name: format!(
                    "{name}_pointer_{}_edge_visible_delete_keeps_scroll",
                    if bottom { "bottom" } else { "top" }
                ),
                template: DocumentTemplate::DeepPointer,
                setup_caret: Some(0),
                actions: vec![
                    Action::Keystroke(mode.into()),
                    Action::ScrollPane {
                        pane: owner.clone(),
                        delta_y: -1100.,
                    },
                    Action::ClickVisibleRow {
                        pane: owner.clone(),
                        vertical_fraction: 0.5,
                    },
                    Action::ScrollCaretToEdge {
                        pane: owner.clone(),
                        bottom,
                    },
                    Action::Keystroke("alt-backspace".into()),
                ],
                invariants: vec![
                    Invariant::InputOwnedBy(owner.clone()),
                    Invariant::ObserveEveryPaint,
                    Invariant::VisibleDeletionKeepsViewport,
                ],
            });
        }
        scenarios.push(Scenario {
            name: format!("{name}_pointer_release_uses_final_position_after_last_move"),
            template: DocumentTemplate::MouseSelection,
            setup_caret: Some(16),
            actions: vec![
                Action::Keystroke(mode.into()),
                Action::MousePress {
                    pane: owner.clone(),
                    offset: start,
                },
                Action::MouseMove {
                    pane: owner.clone(),
                    offset: start + 2,
                },
                Action::MouseRelease {
                    pane: owner.clone(),
                    offset: end,
                },
            ],
            invariants: vec![
                Invariant::SourceEquals(source.clone()),
                Invariant::Selection(start..end),
                Invariant::InputOwnedBy(owner.clone()),
                Invariant::ObserveEveryPaint,
            ],
        });
        for reversed in [false, true] {
            let anchor = if reversed { end } else { start };
            let released = if reversed { start } else { end };
            let middle = if reversed { end - 2 } else { start + 2 };
            for without_move in [false, true] {
                if !reversed && !without_move {
                    // Covered by the explicit final-position regression above.
                    continue;
                }
                let mut actions = vec![
                    Action::Keystroke(mode.into()),
                    Action::MousePress {
                        pane: owner.clone(),
                        offset: anchor,
                    },
                ];
                if !without_move {
                    actions.push(Action::MouseMove {
                        pane: owner.clone(),
                        offset: middle,
                    });
                }
                actions.push(Action::MouseRelease {
                    pane: owner.clone(),
                    offset: released,
                });
                scenarios.push(Scenario {
                    name: format!(
                        "{name}_pointer_{}_release_{}_keeps_press_anchor",
                        if reversed { "reverse" } else { "forward" },
                        if without_move {
                            "without_move"
                        } else {
                            "after_move"
                        }
                    ),
                    template: DocumentTemplate::MouseSelection,
                    setup_caret: Some(16),
                    actions,
                    invariants: vec![
                        Invariant::SourceEquals(source.clone()),
                        Invariant::Selection(start..end),
                        Invariant::InputOwnedBy(owner.clone()),
                        Invariant::ObserveEveryPaint,
                    ],
                });
            }
            let (anchor, extent) = if reversed { (end, start) } else { (start, end) };
            scenarios.push(Scenario {
                name: format!(
                    "{name}_mouse_drag_selects_only_cyrillic_word_{}",
                    if reversed { "backwards" } else { "forwards" }
                ),
                template: DocumentTemplate::MouseSelection,
                setup_caret: Some(16),
                actions: vec![
                    Action::Keystroke(mode.into()),
                    Action::MouseDrag {
                        pane: owner.clone(),
                        start: anchor,
                        end: extent,
                    },
                ],
                invariants: vec![
                    Invariant::SourceEquals(source.clone()),
                    Invariant::Selection(start..end),
                    Invariant::InputOwnedBy(owner.clone()),
                ],
            });
        }
        let row_start = source.find("Это прекрасно!").unwrap();
        scenarios.push(Scenario {
            name: format!("{name}_pointer_padding_press_replaces_old_anchor"),
            template: DocumentTemplate::MouseSelection,
            setup_caret: Some(0),
            actions: vec![
                Action::Keystroke(mode.into()),
                Action::MousePressPadding {
                    pane: owner.clone(),
                    offset: row_start,
                },
                Action::MouseMove {
                    pane: owner.clone(),
                    offset: start,
                },
                Action::MouseRelease {
                    pane: owner.clone(),
                    offset: end,
                },
            ],
            invariants: vec![
                Invariant::SourceEquals(source.clone()),
                Invariant::Selection(row_start..end),
                Invariant::InputOwnedBy(owner.clone()),
                Invariant::ObserveEveryPaint,
            ],
        });
        let deleted_start = source.find("file should ").unwrap();
        let deleted_end = deleted_start + "file should ".len();
        let mut after_delete = source.clone();
        after_delete.replace_range(deleted_start..deleted_end, "");
        let new_end = end - (deleted_end - deleted_start);
        for reversed in [false, true] {
            let anchor = if reversed { new_end } else { 16 };
            let released = if reversed { 16 } else { new_end };
            scenarios.push(Scenario {
                name: format!(
                    "{name}_pointer_crossparagraph_{}_after_deletion",
                    if reversed { "reverse" } else { "forward" }
                ),
                template: DocumentTemplate::MouseSelection,
                setup_caret: Some(0),
                actions: vec![
                    Action::Keystroke(mode.into()),
                    Action::MousePress {
                        pane: owner.clone(),
                        offset: deleted_start,
                    },
                    Action::MouseRelease {
                        pane: owner.clone(),
                        offset: deleted_end,
                    },
                    Action::Keystroke("backspace".into()),
                    Action::MousePress {
                        pane: owner.clone(),
                        offset: anchor,
                    },
                    Action::MouseMove {
                        pane: owner.clone(),
                        offset: released,
                    },
                    Action::MouseRelease {
                        pane: owner.clone(),
                        offset: released,
                    },
                ],
                invariants: vec![
                    Invariant::SourceEquals(after_delete.clone()),
                    Invariant::Selection(16..new_end),
                    Invariant::InputOwnedBy(owner.clone()),
                    Invariant::ObserveEveryPaint,
                ],
            });
        }
        let mut expected = source.clone();
        let newline = if owner == InputOwner::Source {
            "\n"
        } else {
            "\n\n"
        };
        expected.insert_str(end, &format!("{newline}Новый"));
        scenarios.push(Scenario {
            name: format!("{name}_mouse_click_then_enter_starts_visible_paragraph"),
            template: DocumentTemplate::MouseSelection,
            setup_caret: Some(16),
            actions: vec![
                Action::Keystroke(mode.into()),
                Action::MouseClick {
                    pane: owner.clone(),
                    offset: end,
                    count: 1,
                },
                Action::Keystroke("enter".into()),
                Action::InsertText("Новый".into()),
            ],
            invariants: vec![
                Invariant::SourceEquals(expected),
                Invariant::InputOwnedBy(owner.clone()),
                Invariant::EnterMovesCaretDownAtStep(3),
            ],
        });
    }
    scenarios.push(Scenario {
        name: "source_double_click_selects_one_cyrillic_word".into(),
        template: DocumentTemplate::MouseSelection,
        setup_caret: Some(16),
        actions: vec![
            Action::Keystroke("alt-cmd-2".into()),
            Action::MouseClick {
                pane: InputOwner::Source,
                offset: start + 2,
                count: 2,
            },
        ],
        invariants: vec![
            Invariant::SourceEquals(source.clone()),
            Invariant::Selection(start..end),
            Invariant::InputOwnedBy(InputOwner::Source),
        ],
    });
    for reversed in [false, true] {
        scenarios.push(Scenario {
            name: format!(
                "rich_mouse_drag_crosses_paragraphs_{}",
                if reversed { "backwards" } else { "forwards" }
            ),
            template: DocumentTemplate::MouseSelection,
            setup_caret: Some(16),
            actions: vec![Action::MouseDrag {
                pane: InputOwner::Wysiwyg,
                start: if reversed { end } else { 16 },
                end: if reversed { 16 } else { end },
            }],
            invariants: vec![
                Invariant::SourceEquals(source.clone()),
                Invariant::Selection(16..end),
            ],
        });
    }
    scenarios.push(Scenario {
        name: "table_context_panel_click_adds_row_and_column_and_keeps_input".into(),
        template: DocumentTemplate::Table { rows: 2 },
        setup_caret: Some(
            DocumentTemplate::Table { rows: 2 }
                .render()
                .find("cell 1a")
                .unwrap()
                + 2,
        ),
        actions: vec![Action::TableClick(1), Action::TableClick(4)],
        invariants: vec![
            Invariant::TableControlsVisible,
            Invariant::InputOwnedBy(InputOwner::Wysiwyg),
            Invariant::SourceContains("cell 2a".into()),
            Invariant::TableShape {
                rows: 4,
                columns: 3,
            },
        ],
    });
    scenarios
}

// ---------------------------------------------------------------------------
// Generator
// ---------------------------------------------------------------------------

/// Deterministically produce `count` scenarios built from a small set of
/// document templates × action sequences. The seed controls which
/// actions and template parameters are picked so reruns are byte-for-byte
/// reproducible.
pub fn generate_scenarios(seed: u64, count: usize) -> Vec<Scenario> {
    let mut rng = SplitMix64::new(seed);
    let mut out: Vec<Scenario> = Vec::with_capacity(count + curated_scenarios().len());
    out.extend(curated_scenarios());

    let templates = [
        DocumentTemplate::Empty,
        DocumentTemplate::Plain { paragraphs: 3 },
        DocumentTemplate::Headings {
            levels: vec![1, 2, 3],
        },
        DocumentTemplate::BulletList {
            items: 4,
            trailing_empty: true,
        },
        DocumentTemplate::TaskList {
            items: 3,
            trailing_empty: true,
            all_checked: false,
        },
        DocumentTemplate::CodeFence,
        DocumentTemplate::Table { rows: 2 },
        DocumentTemplate::Blockquote { paragraphs: 2 },
        DocumentTemplate::Mixed,
    ];
    let action_sets: &[&[&str]] = &[
        &["jump:end", "keystroke:cmd-1"],
        &["jump:end", "keystroke:cmd-shift-."],
        &["keystroke:shift-cmd-8", "keystroke:shift-cmd-8"],
        &["jump:start", "keystroke:shift-end", "insert:X"],
        &["keystroke:home", "keystroke:down", "keystroke:end"],
        &[
            "keystroke:alt-cmd-2",
            "keystroke:cmd-a",
            "keystroke:delete",
            "keystroke:alt-cmd-1",
        ],
        &["jump:start", "keystroke:cmd-shift-c", "keystroke:cmd-v"],
        &[
            "jump:start",
            "keystroke:alt-cmd-0",
            "keystroke:cmd-1",
            "keystroke:cmd-2",
            "keystroke:cmd-0",
        ],
    ];

    while out.len() < count {
        let template = templates[rng.next_usize(templates.len())].clone();
        let set = action_sets[rng.next_usize(action_sets.len())];
        let actions = set
            .iter()
            .map(|spec| parse_action_spec(spec))
            .collect::<Vec<_>>();

        let name = format!("gen_{:04}_{}", out.len(), short_template_name(&template));
        // Every frame checks focus, UTF-8 selection, revisions, source policy,
        // selection paint and non-mutating action contracts. Generated smoke
        // journeys need no template-specific guesses about block counts.
        let invariants: Vec<Invariant> = Vec::new();

        out.push(Scenario {
            name,
            template,
            setup_caret: None,
            actions,
            invariants,
        });
    }

    out
}

fn parse_action_spec(spec: &str) -> Action {
    let (kind, value) = spec.split_once(':').unwrap_or(("keystroke", spec));
    match kind {
        "jump" => match value {
            "end" => Action::JumpTo(usize::MAX),
            "start" => Action::JumpTo(0),
            other => Action::JumpTo(other.parse().expect("parseable jump offset")),
        },
        "keystroke" => Action::Keystroke(value.to_string()),
        "insert" => Action::InsertText(value.to_string()),
        _ => panic!("unknown action spec: {spec}"),
    }
}

fn short_template_name(t: &DocumentTemplate) -> &'static str {
    match t {
        DocumentTemplate::Empty => "empty",
        DocumentTemplate::Plain { .. } => "plain",
        DocumentTemplate::SeparatedParagraphs { .. } => "separated-paragraphs",
        DocumentTemplate::Headings { .. } => "headings",
        DocumentTemplate::BulletList { .. } => "ulist",
        DocumentTemplate::OrderedList { .. } => "olist",
        DocumentTemplate::TaskList { .. } => "task",
        DocumentTemplate::CodeFence => "fence",
        DocumentTemplate::Table { .. } => "table",
        DocumentTemplate::Blockquote { .. } => "quote",
        DocumentTemplate::Mixed => "mixed",
        DocumentTemplate::Frontmatter => "frontmatter",
        DocumentTemplate::Unicode => "unicode",
        DocumentTemplate::Whitespace => "whitespace",
        DocumentTemplate::StableHints => "stable-hints",
        DocumentTemplate::MouseSelection => "mouse-selection",
        DocumentTemplate::DeepPointer => "deep-pointer",
        DocumentTemplate::MiddleList => "middle-list",
        DocumentTemplate::FindContent => "find-content",
        DocumentTemplate::FindDeepContent => "find-deep-content",
    }
}

/// Count top-level blocks in a markdown source by counting lines that
/// either start with `#` or a list marker, or are blank separating
/// blocks. This is a rough heuristic for assertion seeding — the real
/// rendered count comes from the rich engine during the run. Used by
/// the unit tests below; the live runner relies on trace snapshots
/// for real block counts.
#[cfg(test)]
fn count_blocks(source: &str) -> usize {
    let mut count = 0usize;
    let mut in_fence = false;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            count += 1;
            continue;
        }
        if in_fence {
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#')
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("+ ")
            || trimmed.starts_with("> ")
            || (trimmed.starts_with(|c: char| c.is_ascii_digit()) && trimmed.contains(". "))
            || trimmed.starts_with("|")
        {
            count += 1;
        }
    }
    count.max(1)
}

/// SplitMix64: a tiny deterministic PRNG with a clean API.
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn next_usize(&mut self, bound: usize) -> usize {
        (self.next_u64() as usize) % bound
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// Run a single scenario against the open window, capturing snapshots
/// after each action and writing them as JSON lines to
/// `output_dir/usecases/<name>.jsonl`. Returns the final snapshot.
pub fn run_scenario(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    scenario: &Scenario,
    output_dir: &Path,
    record_frames: bool,
) -> Result<Snapshot> {
    crate::evidence::validate_name(&scenario.name)?;
    let result = run_scenario_inner(cx, window, workspace, scenario, output_dir, record_frames);
    let report = match &result {
        Ok(snapshot) => serde_json::json!({
            "scenario": scenario.name, "status": "passed", "checked_steps": snapshot.step + 1,
            "contracts": ["visible-input-owner", "utf8-selection", "current-render-revision", "mode-policy", "action-response"],
        }),
        Err(error) => {
            if let Ok(screenshot) = cx.capture_screenshot(window.into()) {
                let _ = crate::visual_tests::save_screenshot(
                    &screenshot,
                    &output_dir
                        .join("usecases")
                        .join(format!("{}.failure.png", scenario.name)),
                );
            }
            serde_json::json!({"scenario": scenario.name, "status": "failed", "error": format!("{error:#}")})
        }
    };
    fs::write(
        output_dir
            .join("usecases")
            .join(format!("{}.report.json", scenario.name)),
        serde_json::to_vec_pretty(&report)?,
    )?;
    let trace = fs::read_to_string(
        output_dir
            .join("usecases")
            .join(format!("{}.jsonl", scenario.name)),
    )?;
    let snapshots = trace
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<Vec<Snapshot>, _>>()?;
    crate::evidence::write_review(
        &output_dir.join("usecases"),
        &scenario.name,
        &snapshots,
        result
            .as_ref()
            .err()
            .map(|error| format!("{error:#}"))
            .as_deref(),
        record_frames,
    )?;
    result
}

fn run_scenario_inner(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    scenario: &Scenario,
    output_dir: &Path,
    record_frames: bool,
) -> Result<Snapshot> {
    let usecases_dir = output_dir.join("usecases");
    fs::create_dir_all(&usecases_dir)
        .with_context(|| format!("creating usecases output dir {}", usecases_dir.display()))?;
    let trace_path = usecases_dir.join(format!("{}.jsonl", scenario.name));
    let mut file = fs::File::create(&trace_path)?;
    let started = Instant::now();

    // Each journey starts with fresh document/history/views and declared UI
    // policy. A prior Source journey must not turn a WYSIWYG test into Source.
    cx.update_window(window.into(), |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            ws.config.markup_hints_enabled = true;
            ws.set_highlight_style(crate::config::HighlightStyle::Native, cx);
            ws.new_document(window, cx);
            while ws.tabs.len() > 1 {
                ws.close_tab(0, window, cx);
            }
            ws.sidebar_open = false;
            ws.outline_open = false;
            ws.panel_overlay = None;
            ws.palette_open = false;
            cx.notify();
        });
    })?;

    // Reset the document to the template's source and place the caret
    // at the requested starting offset (or 0).
    let source = scenario.template.render();
    let setup_caret = scenario.setup_caret.unwrap_or(0);
    let (len, doc_entity) = cx.read_entity(workspace, |ws, cx| {
        let tab = ws.active_tab().unwrap();
        let len = tab.document.read(cx).buffer.len_bytes();
        (len, tab.document.clone())
    });
    let parsed = doc_entity.update(cx, |doc, cx| {
        doc.replace_range(0, len, &source);
        // Without draining the parse pump, `syntax_spans` keeps claiming
        // positions from the prior buffer revision; the next outline
        // query then OOB-slices into the new (often empty) source. The
        // fixture bootstrap uses the same barrier (`wait_for_parse`).
        // A fresh empty fixture has no syntax to await. Its no-op replacement
        // does not enqueue another parse; waiting for an already-consumed
        // revision-zero update would otherwise stall each empty journey.
        let parsed = source.is_empty() || doc.wait_for_parse(Duration::from_secs(30));
        cx.notify();
        parsed
    });
    ensure!(parsed, "fixture parse did not reach its current revision");
    move_caret(cx, window, workspace, setup_caret.min(source.len()))?;
    // `replace_range` triggers a deferred re-parse + render cycle. Drawing a
    // frame first lets the document settle so the first snapshot mirrors the
    // rich engine's parsed layout, not the pre-replace geometry.
    draw(cx, window)?;

    let mut snapshots = Vec::with_capacity(scenario.actions.len() + 1);
    let mut paint_trace = scenario
        .invariants
        .iter()
        .any(|invariant| matches!(invariant, Invariant::ObserveEveryPaint))
        .then(|| fs::File::create(usecases_dir.join(format!("{}.paints.jsonl", scenario.name))))
        .transpose()?;
    let mut initial = capture_snapshot(cx, window, workspace, 0, &Action::JumpTo(setup_caret))?;
    initial.timestamp_ms = started.elapsed().as_millis() as u64;
    if record_frames {
        crate::visual_tests::save_screenshot(
            &cx.capture_screenshot(window.into())?,
            &usecases_dir.join(format!("{}.step-000.png", scenario.name)),
        )?;
    }
    writeln!(file, "{}", serde_json::to_string(&initial)?)?;
    file.flush()?;
    validate_snapshot(&scenario.name, &initial)?;
    for invariant in &scenario.invariants {
        if matches!(
            invariant,
            Invariant::CaretBlinkSequence(_) | Invariant::CaretBlinkAtSteps(_)
        ) {
            check_invariants(&scenario.name, &initial, std::slice::from_ref(invariant))?;
        }
    }
    snapshots.push(initial);

    let step_invariants: Vec<_> = scenario
        .invariants
        .iter()
        .filter(|inv| {
            matches!(
                inv,
                Invariant::CaretVisibleAfterEveryAction
                    | Invariant::ViewportFirstBlockAtLeastAfterEveryAction(_)
                    | Invariant::SourceCaretVisibleFromStep(_)
                    | Invariant::SourceViewportScrolledFromStep(_)
                    | Invariant::CaretBlinkSequence(_)
                    | Invariant::CaretBlinkAtSteps(_)
                    | Invariant::CaretAtSteps(_)
                    | Invariant::SourceAtSteps(_)
                    | Invariant::PlainContextAtSteps(_)
                    | Invariant::ShadowVisibleAtSteps(_)
                    | Invariant::FindAtSteps(_)
            )
        })
        .cloned()
        .collect();

    for (step, action) in scenario.actions.iter().enumerate() {
        let label = format!(
            "{} step {} ({})",
            scenario.name,
            step + 1,
            action.describe()
        );
        apply_action(cx, window, workspace, action)
            .with_context(|| format!("{label}: dispatch failed"))?;
        // Always draw after a dispatch: the existing `keystroke()` helper in
        // visual_tests uses the same pattern. Without `window.refresh()` /
        // `window.draw()` the input pipeline's on_action handlers may not
        // flush their document mutations into the snapshot reader.
        let previous = snapshots.last().unwrap();
        let observe_paints = scenario
            .invariants
            .iter()
            .any(|invariant| matches!(invariant, Invariant::ObserveEveryPaint));
        let stable_deletion = scenario
            .invariants
            .iter()
            .any(|invariant| matches!(invariant, Invariant::VisibleDeletionKeepsViewport));
        let mut paint_frames = Vec::new();
        if observe_paints {
            for pass in 1..=3 {
                draw_frame(cx, window)?;
                let frame = capture_snapshot(cx, window, workspace, step + 1, action)?;
                let frame_label = format!("{label} paint {pass}");
                let paint_frame = PaintFrame {
                    step: step + 1,
                    action: action.clone(),
                    pass,
                    source: frame.source.clone(),
                    caret: frame.caret,
                    selection: frame.selection.clone(),
                    rich_first_block: frame.viewport_first_block,
                    rich_offset_px: frame.viewport_offset_px,
                    source_offset_y: frame.source_viewport_offset_y,
                    ui: frame.ui.as_ref().unwrap().clone(),
                };
                // Persist the real failing scene before checking it; a later
                // settled frame must not conceal a bad transient render.
                if let Some(file) = paint_trace.as_mut() {
                    writeln!(file, "{}", serde_json::to_string(&paint_frame)?)?;
                    file.flush()?;
                }
                if record_frames {
                    crate::visual_tests::save_screenshot(
                        &cx.capture_screenshot(window.into())?,
                        &usecases_dir.join(format!(
                            "{}.step-{:03}.paint-{pass}.png",
                            scenario.name,
                            step + 1
                        )),
                    )?;
                }
                validate_snapshot(&frame_label, &frame)?;
                crate::observation::validate_paint(cx, window, workspace, &frame_label)?;
                check_response(previous, &frame, action).with_context(|| frame_label.clone())?;
                if stable_deletion {
                    check_visible_deletion(previous, &frame, action)
                        .with_context(|| frame_label.clone())?;
                }
                check_invariants(&frame_label, &frame, &step_invariants)?;
                check_stable_draft_row(previous, &frame, &scenario.invariants)
                    .with_context(|| frame_label.clone())?;
                paint_frames.push(paint_frame);
            }
        } else {
            draw(cx, window).with_context(|| {
                format!("{label}: drawing failed; trace ends at the last completed step")
            })?;
        }
        let mut snapshot =
            capture_snapshot(cx, window, workspace, step + 1, action).with_context(|| {
                format!("{label}: observation failed; trace ends at the last completed step")
            })?;
        snapshot.paint_frames = paint_frames;
        snapshot.timestamp_ms = started.elapsed().as_millis() as u64;
        snapshot.response = Some(response(previous, &snapshot));
        if record_frames {
            crate::visual_tests::save_screenshot(
                &cx.capture_screenshot(window.into())?,
                &usecases_dir.join(format!("{}.step-{:03}.png", scenario.name, snapshot.step)),
            )?;
        }
        writeln!(file, "{}", serde_json::to_string(&snapshot)?)?;
        file.flush()?;
        let label = format!(
            "{} step {} ({})",
            scenario.name,
            snapshot.step,
            action.describe()
        );
        validate_snapshot(&label, &snapshot)?;
        crate::observation::validate_paint(cx, window, workspace, &label)?;
        check_response(previous, &snapshot, action).with_context(|| label.clone())?;
        if stable_deletion {
            check_visible_deletion(previous, &snapshot, action).with_context(|| label.clone())?;
        }
        if scenario
            .invariants
            .iter()
            .any(|invariant| matches!(invariant, Invariant::WhitespaceAdvancesCaret))
            && matches!(action, Action::InsertText(text) if !text.is_empty() && text.bytes().all(|byte| byte == b' ' || byte == b'\t'))
        {
            check_whitespace_caret_advance(previous, &snapshot).with_context(|| label.clone())?;
        }
        if scenario.invariants.iter().any(|invariant| matches!(invariant, Invariant::RepeatedEnterIsNoopAtStep(expected) if *expected == snapshot.step)) {
            ensure!(previous.source == snapshot.source && previous.selection == snapshot.selection
                && previous.ui.as_ref().unwrap().document_revision == snapshot.ui.as_ref().unwrap().document_revision,
                "{label}: repeated Enter changed the empty paragraph or document revision");
        }
        if scenario.invariants.iter().any(|invariant| matches!(invariant, Invariant::EnterMovesCaretDownAtStep(expected) if *expected == snapshot.step)) {
            let before = active_caret_bounds(previous.ui.as_ref().unwrap()).context("missing pre-Enter caret")?;
            let after = active_caret_bounds(snapshot.ui.as_ref().unwrap()).context("missing new paragraph caret")?;
            ensure!(after.y > before.y + before.height * 0.5,
                "{label}: first Enter did not visibly move the caret to a new paragraph");
        }
        check_stable_draft_row(previous, &snapshot, &scenario.invariants)
            .with_context(|| label.clone())?;
        check_invariants(&label, &snapshot, &step_invariants)?;
        snapshots.push(snapshot);
    }
    let final_snapshot = snapshots.last().cloned().unwrap();
    check_invariants(&scenario.name, &final_snapshot, &scenario.invariants)?;
    Ok(final_snapshot)
}

fn check_stable_draft_row(
    previous: &Snapshot,
    snapshot: &Snapshot,
    invariants: &[Invariant],
) -> Result<()> {
    if invariants.iter().any(|invariant| {
        matches!(invariant,
        Invariant::CaretStaysOnSameRowAtStep(expected) if *expected == snapshot.step)
    }) {
        let before =
            active_caret_bounds(previous.ui.as_ref().unwrap()).context("missing draft caret")?;
        let after = active_caret_bounds(snapshot.ui.as_ref().unwrap())
            .context("missing filled paragraph caret")?;
        ensure!(
            (after.y - before.y).abs() <= 1.,
            "filling the empty paragraph moved its baseline: {before:?} -> {after:?}"
        );
    }
    Ok(())
}

fn move_caret(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    target: usize,
) -> Result<()> {
    // Keep the focused editing surface in Split mode; either pane can own
    // keystrokes there. Source and WYSIWYG modes have one active surface.
    let mode = cx.read_entity(workspace, |ws, _| ws.active_tab().unwrap().mode);
    let use_source = cx.update_window(window.into(), |_, window, cx| {
        let tab = workspace.read(cx).active_tab().unwrap();
        let use_source = mode == EditorMode::Source
            || mode == EditorMode::Split && tab.editor.read(cx).focus_handle.is_focused(window);
        if use_source {
            let handle = tab.editor.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
        } else {
            let handle = tab.rich_view.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        use_source
    })?;
    let (rich_entity, editor_entity) = cx.read_entity(workspace, |ws, _| {
        let tab = ws.active_tab().unwrap();
        (tab.rich_view.clone(), tab.editor.clone())
    });
    if use_source {
        editor_entity.update(cx, |ed, cx| ed.jump_to(target, cx));
    } else {
        rich_entity.update(cx, |view, cx| view.jump_to(target, cx));
    }
    Ok(())
}

fn apply_action(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    action: &Action,
) -> Result<()> {
    match action {
        Action::WaitMillis(ms) => {
            cx.advance_clock(Duration::from_millis(*ms));
            cx.run_until_parked();
        }
        Action::Keystroke(key) => {
            let key =
                Keystroke::parse(key).with_context(|| format!("parsing keystroke {key:?}"))?;
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_keystroke(key, cx);
            })?;
        }
        Action::InsertText(text) => {
            for ch in text.chars() {
                let key = Keystroke {
                    modifiers: Modifiers::default(),
                    key: ch.to_string(),
                    key_char: Some(ch.to_string()),
                };
                cx.update_window(window.into(), |_, window, cx| {
                    window.dispatch_keystroke(key, cx);
                })?;
            }
        }
        Action::JumpTo(offset) => {
            // Translate "end of doc" to the actual buffer length, then route
            // through `move_caret` so the richtext view (or source editor in
            // Source mode) actually owns the caret before any follow-up
            // keystroke is dispatched.
            let target = if *offset == usize::MAX {
                cx.read_entity(workspace, |ws, cx| {
                    ws.active_tab()
                        .unwrap()
                        .document
                        .read(cx)
                        .buffer
                        .len_bytes()
                })
            } else {
                *offset
            };
            move_caret(cx, window, workspace, target)?;
        }
        Action::FocusSource => {
            cx.update_window(window.into(), |_, window, cx| {
                let tab = workspace.read(cx).active_tab().unwrap();
                let handle = tab.editor.read(cx).focus_handle.clone();
                window.focus(&handle, cx);
            })?;
        }
        Action::FocusRich => {
            cx.update_window(window.into(), |_, window, cx| {
                let tab = workspace.read(cx).active_tab().unwrap();
                window.focus(&tab.rich_view.read(cx).focus_handle(cx), cx);
            })?;
        }
        Action::SelectRange { start, end } => {
            cx.update_window(window.into(), |_, window, cx| {
                workspace.update(cx, |ws, cx| {
                    ws.dispatch(
                        WorkspaceCommand::Editor(EditorCommand::SetSelection {
                            start: *start,
                            end: *end,
                        }),
                        window,
                        cx,
                    )
                })
            })??;
        }
        Action::MouseClick {
            pane,
            offset,
            count,
        } => {
            let ui = crate::observation::capture(cx, window, workspace)?;
            let position = painted_pointer_position(&ui, pane, *offset)?;
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        position,
                        button: MouseButton::Left,
                        modifiers: Modifiers::default(),
                        click_count: *count,
                        first_mouse: false,
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        position,
                        button: MouseButton::Left,
                        modifiers: Modifiers::default(),
                        click_count: *count,
                    }),
                    cx,
                );
            })?;
        }
        Action::MousePress { pane, offset }
        | Action::MousePressPadding { pane, offset }
        | Action::MouseMove { pane, offset }
        | Action::MouseRelease { pane, offset } => {
            let ui = crate::observation::capture(cx, window, workspace)?;
            let mut position = painted_pointer_position(&ui, pane, *offset)?;
            if matches!(action, Action::MousePressPadding { .. }) {
                position.x = px(document_pane(&ui, pane)?
                    .viewport
                    .as_ref()
                    .context("padding press has no viewport")?
                    .x
                    + 8.);
            }
            cx.update_window(window.into(), |_, window, cx| {
                let event = match action {
                    Action::MousePress { .. } | Action::MousePressPadding { .. } => {
                        PlatformInput::MouseDown(MouseDownEvent {
                            position,
                            button: MouseButton::Left,
                            modifiers: Modifiers::default(),
                            click_count: 1,
                            first_mouse: false,
                        })
                    }
                    Action::MouseMove { .. } => PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Modifiers::default(),
                    }),
                    Action::MouseRelease { .. } => PlatformInput::MouseUp(MouseUpEvent {
                        position,
                        button: MouseButton::Left,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    }),
                    _ => unreachable!(),
                };
                window.dispatch_event(event, cx);
            })?;
        }
        Action::ClickBelowContentWithoutPaint { pane } => {
            let ui = crate::observation::capture(cx, window, workspace)?;
            let viewport = document_pane(&ui, pane)?
                .viewport
                .as_ref()
                .context("below-content click has no viewport")?;
            let rows = painted_rows(&ui, pane)?;
            let last = rows
                .iter()
                .max_by(|a, b| a.bounds.y.total_cmp(&b.bounds.y))
                .context("below-content click has no native row")?;
            let position = point(
                px(last.bounds.x + 8.),
                px(last.bounds.y + last.bounds.height + 40.),
            );
            ensure!(
                f32::from(position.y) < viewport.y + viewport.height,
                "below-content click is outside the pane"
            );
            cx.update_window(window.into(), |_, window, cx| {
                // Deliberately no draw/run_until_parked between these two
                // real native events. Up must not use obsolete pre-Down rows.
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        position,
                        button: MouseButton::Left,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        position,
                        button: MouseButton::Left,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    }),
                    cx,
                );
            })?;
        }
        Action::ScrollPane { pane, delta_y } => {
            let ui = crate::observation::capture(cx, window, workspace)?;
            let viewport = document_pane(&ui, pane)?
                .viewport
                .as_ref()
                .context("scroll pane has no painted viewport")?;
            let position = point(
                px(viewport.x + viewport.width * 0.5),
                px(viewport.y + viewport.height * 0.5),
            );
            cx.update_window(window.into(), |_, window, cx| {
                window.simulate_mouse_move(position, cx);
                window.dispatch_event(
                    PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                        position,
                        delta: gpui::ScrollDelta::Pixels(point(px(0.), px(*delta_y))),
                        modifiers: Modifiers::default(),
                        touch_phase: gpui::TouchPhase::Moved,
                    }),
                    cx,
                );
            })?;
        }
        Action::ScrollCaretToEdge { pane, bottom } => {
            let ui = crate::observation::capture(cx, window, workspace)?;
            let state = document_pane(&ui, pane)?;
            let viewport = state
                .viewport
                .as_ref()
                .context("edge scroll has no viewport")?;
            let caret = state
                .caret_bounds
                .as_ref()
                .context("edge scroll has no native caret")?;
            let target = if *bottom {
                viewport.y + viewport.height - caret.height - 1.
            } else {
                viewport.y + 1.
            };
            apply_action(
                cx,
                window,
                workspace,
                &Action::ScrollPane {
                    pane: pane.clone(),
                    delta_y: target - caret.y,
                },
            )?;
            draw(cx, window)?;
            let after = crate::observation::capture(cx, window, workspace)?;
            let after = document_pane(&after, pane)?
                .caret_bounds
                .as_ref()
                .context("edge scroll lost painted caret")?;
            ensure!(
                (after.y - target).abs() <= 1.1,
                "native scroll did not place caret at visible edge: target{target}, got{}",
                after.y
            );
        }
        Action::ClickVisibleRow {
            pane,
            vertical_fraction,
        } => {
            let ui = crate::observation::capture(cx, window, workspace)?;
            let viewport = document_pane(&ui, pane)?
                .viewport
                .as_ref()
                .context("click pane has no painted viewport")?;
            let target_y = viewport.y + viewport.height * vertical_fraction;
            let rows = painted_rows(&ui, pane)?;
            let row = rows
                .iter()
                .filter(|row| {
                    row.stops.len() > 4
                        && row.bounds.y > viewport.y
                        && row.bounds.y + row.bounds.height < viewport.y + viewport.height
                })
                .min_by(|a, b| {
                    (a.bounds.y - target_y)
                        .abs()
                        .total_cmp(&(b.bounds.y - target_y).abs())
                })
                .context("pane has no fully visible native row")?;
            let (offset, _) = row
                .stops
                .iter()
                .rev()
                .find(|(_, x)| *x > viewport.x + 20. && *x < viewport.x + viewport.width - 3.)
                .context("visible row lacks horizontally visible caret stops")?;
            let position = painted_pointer_position(&ui, pane, *offset)?;
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        position,
                        button: MouseButton::Left,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        position,
                        button: MouseButton::Left,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    }),
                    cx,
                );
            })?;
            draw(cx, window)?;
            let actual = crate::observation::capture(cx, window, workspace)?;
            let actual_pane = document_pane(&actual, pane)?;
            ensure!(
                actual_pane.selection == (*offset..*offset),
                "visible native click chose {:?}, expected exact stop {offset}",
                actual_pane.selection
            );
            let caret = actual_pane
                .caret_bounds
                .as_ref()
                .context("visible click lost its painted caret")?;
            ensure!(
                (caret.y - row.bounds.y).abs() <= 1.,
                "visible native click moved its physical target row: {} -> {}",
                row.bounds.y,
                caret.y
            );
            if *pane == InputOwner::Wysiwyg {
                crate::observation::validate_stationary_rich_layout(&ui, &actual)?;
            }
        }
        Action::MouseDrag { pane, start, end } => {
            let ui = crate::observation::capture(cx, window, workspace)?;
            let anchor = painted_pointer_position(&ui, pane, *start)?;
            let extent = painted_pointer_position(&ui, pane, *end)?;
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        position: anchor,
                        button: MouseButton::Left,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
            })?;
            draw(cx, window)?;
            for step in 1..=4 {
                let fraction = step as f32 / 4.;
                cx.update_window(window.into(), |_, window, cx| {
                    window.dispatch_event(
                        PlatformInput::MouseMove(MouseMoveEvent {
                            position: anchor + (extent - anchor) * fraction,
                            pressed_button: Some(MouseButton::Left),
                            modifiers: Modifiers::default(),
                        }),
                        cx,
                    );
                })?;
                // Keep the physical path fixed across real paints; context
                // changes must not move its target or steal the drag.
                draw(cx, window)?;
            }
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        position: extent,
                        button: MouseButton::Left,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    }),
                    cx,
                );
            })?;
        }
        Action::TableClick(index) => {
            let bounds = cx
                .read_entity(workspace, |ws, cx| {
                    ws.active_tab()
                        .unwrap()
                        .rich_view
                        .read(cx)
                        .painted_table_button_bounds(*index)
                })
                .context("table action has no actual painted button bounds")?;
            let position = bounds.center();
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        position,
                        button: MouseButton::Left,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        position,
                        button: MouseButton::Left,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    }),
                    cx,
                );
            })?;
        }
        Action::PaletteOutsideClick => {
            let ui = crate::observation::capture(cx, window, workspace)?;
            let bounds = ui.palette_bounds.context("palette has no painted bounds")?;
            let position = point(px(bounds.x - 8.), px(bounds.y + 10.));
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        position,
                        button: MouseButton::Left,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        position,
                        button: MouseButton::Left,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    }),
                    cx,
                );
            })?;
        }
        Action::NewTab | Action::SwitchTab(_) | Action::EditFrontmatter => {
            cx.update_window(window.into(), |_, window, cx| {
                workspace.update(cx, |ws, cx| match action {
                    Action::NewTab => {
                        ws.new_document(window, cx);
                        Ok(())
                    }
                    Action::SwitchTab(index) => {
                        ws.dispatch(WorkspaceCommand::SwitchTab(*index), window, cx)
                    }
                    Action::EditFrontmatter => {
                        ws.dispatch(WorkspaceCommand::EditFrontmatter, window, cx)
                    }
                    _ => unreachable!(),
                })
            })??;
        }
        Action::HighlightStyle(style) => {
            cx.update_window(window.into(), |_, _, cx| {
                workspace.update(cx, |ws, cx| ws.set_highlight_style(*style, cx));
            })?;
        }
    }
    Ok(())
}

/// Resolve a pointer against painted shaping, never the desired logical selection.
fn painted_pointer_position(
    ui: &Observation,
    pane: &InputOwner,
    offset: usize,
) -> Result<Point<Pixels>> {
    let rows = painted_rows(ui, pane)?;
    let viewport = document_pane(ui, pane)?
        .viewport
        .as_ref()
        .context("pointer pane has no painted viewport")?;
    for row in rows {
        let y = row.bounds.y + row.bounds.height / 2.;
        if y <= viewport.y || y >= viewport.y + viewport.height {
            continue;
        }
        if let Some((_, x)) = row.stops.iter().find(|(byte, _)| *byte == offset) {
            // Endpoint clicks must be just inside the leaf's hitbox, while still
            // nearest to its final caret stop.
            let x = if *x >= row.bounds.x + row.bounds.width {
                *x - 0.2
            } else {
                *x + 0.2
            };
            if x >= viewport.x && x < viewport.x + viewport.width {
                return Ok(point(px(x), px(y)));
            }
        }
    }
    anyhow::bail!("{pane:?} has no visible painted caret stop at byte {offset}")
}

fn document_pane<'a>(
    ui: &'a Observation,
    pane: &InputOwner,
) -> Result<&'a crate::observation::PaneState> {
    match pane {
        InputOwner::Source => Ok(&ui.source_pane),
        InputOwner::Wysiwyg => Ok(&ui.rich_pane),
        _ => anyhow::bail!("pointer action requires a document pane"),
    }
}

fn painted_rows<'a>(
    ui: &'a Observation,
    pane: &InputOwner,
) -> Result<Vec<&'a crate::observation::PaintedRow>> {
    match pane {
        InputOwner::Source => Ok(ui.painted_source.iter().collect()),
        InputOwner::Wysiwyg => Ok(ui.painted_rich.iter().flat_map(|leaf| &leaf.rows).collect()),
        _ => anyhow::bail!("pointer action requires a document pane"),
    }
}

/// Three-pass frame advance + `window.refresh()` + `window.draw()`, mirroring
/// the baseline `keystroke()` helper in `visual_tests.rs`. Without
/// `simulate_next_frame` / `refresh()` the input pipeline's on_action
/// handlers don't always flush their document mutations into the snapshot
/// reader.
fn draw(cx: &mut HeadlessAppContext, window: WindowHandle<MarkRustWindow>) -> Result<()> {
    for _ in 0..3 {
        draw_frame(cx, window)?;
    }
    Ok(())
}

fn draw_frame(cx: &mut HeadlessAppContext, window: WindowHandle<MarkRustWindow>) -> Result<()> {
    cx.advance_clock(Duration::from_millis(35));
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.simulate_next_frame(cx);
        window.refresh();
        window.draw(cx).clear(cx);
    })?;
    Ok(())
}

fn capture_snapshot(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    step: usize,
    action: &Action,
) -> Result<Snapshot> {
    let ui = crate::observation::capture(cx, window, workspace)?;
    let source_focused = cx.update_window(window.into(), |_, window, cx| {
        let tab = workspace.read(cx).active_tab().unwrap();
        tab.editor.read(cx).focus_handle.is_focused(window)
            || (workspace.read(cx).palette_open
                && tab.editing_pane == crate::workspace::EditingPane::Source)
    })?;
    Ok(cx.read_entity(workspace, |ws, cx| {
        let tab = ws.active_tab().unwrap();
        let rich = tab.rich_view.read(cx);
        let editor = tab.editor.read(cx);
        let doc = tab.document.read(cx);
        let source = doc.buffer.content();
        // Read caret / selection from the surface that actually owns
        // input for the current mode. In Source mode the rich view's
        // caret/selection is "stale" from the last Wysiwyg/Split visit
        // and not representative of what the user is interacting with.
        // (The rich view is still rendered for Split mode and stays in
        // sync; for Source/Split fall back to whichever surface is
        // focused, then the source editor.)
        let (caret, selection) = match tab.mode {
            EditorMode::Source => {
                // The source editor is the active surface; its on_action
                // handlers are the ones that just ran (e.g. cmd-a,
                // delete, etc.). Source mode collapses to the editor.
                (editor.cursor_offset(), editor.selected_range.clone())
            }
            EditorMode::Split if source_focused => {
                (editor.cursor_offset(), editor.selected_range.clone())
            }
            EditorMode::Wysiwyg | EditorMode::Split => {
                (rich.cursor_offset(), rich.selected_range.clone())
            }
        };
        let mode = tab.mode;
        let blocks = collect_blocks(rich.engine_ref(), &source);
        let viewport_height = rich
            .painted_viewport_bounds()
            .map(|b| f32::from(b.size.height))
            .unwrap_or(0.);
        let (viewport_first_block, viewport_offset_px, caret_visible, viewport_y, caret_y) =
            if mode == EditorMode::Wysiwyg || mode == EditorMode::Split && !source_focused {
                let (anchor, viewport, caret_rect) = rich.test_viewport_state();
                let caret_rect = if rich.test_widget_kind().is_some() {
                    rich.painted_widget_caret_bounds()
                } else {
                    caret_rect
                };
                let visible = caret_rect.is_some_and(|caret_rect| {
                    caret_rect.top() >= viewport.top() - gpui::px(2.)
                        && caret_rect.bottom() <= viewport.bottom() + gpui::px(2.)
                });
                (
                    Some(anchor.item_ix),
                    Some(f32::from(anchor.offset_in_item)),
                    Some(visible),
                    Some((f32::from(viewport.top()), f32::from(viewport.bottom()))),
                    caret_rect.map(|rect| (f32::from(rect.top()), f32::from(rect.bottom()))),
                )
            } else {
                (None, None, None, None, None)
            };
        let (source_caret_visible, source_viewport_offset_y, source_viewport_y, source_caret_y) =
            if mode != EditorMode::Wysiwyg {
                let (viewport, _, offset) = tab.editor_view.read(cx).horizontal_scroll_state();
                let caret_rect = editor.painted_caret_bounds();
                let visible = caret_rect.is_some_and(|caret_rect| {
                    caret_rect.left() >= viewport.left() - gpui::px(2.)
                        && caret_rect.right() <= viewport.right() + gpui::px(2.)
                        && caret_rect.top() >= viewport.top() - gpui::px(2.)
                        && caret_rect.bottom() <= viewport.bottom() + gpui::px(2.)
                });
                (
                    Some(visible),
                    Some(f32::from(offset.y)),
                    Some((f32::from(viewport.top()), f32::from(viewport.bottom()))),
                    caret_rect.map(|rect| (f32::from(rect.top()), f32::from(rect.bottom()))),
                )
            } else {
                (None, None, None, None)
            };
        let line_count = source.lines().count();
        Snapshot {
            step,
            action: action.clone(),
            source,
            caret,
            selection,
            mode: mode_label(mode),
            blocks,
            line_count,
            viewport_height,
            viewport_first_block,
            viewport_offset_px,
            caret_visible,
            viewport_y,
            caret_y,
            source_caret_visible,
            source_viewport_offset_y,
            source_viewport_y,
            source_caret_y,
            ui: Some(ui),
            response: None,
            timestamp_ms: 0,
            paint_frames: Vec::new(),
        }
    }))
}

fn collect_blocks(engine: &markrust_core::rich::RichEngine, source: &str) -> Vec<BlockInfo> {
    engine
        .tree()
        .blocks
        .iter()
        .map(|b| {
            let kind = block_kind_label(&b.kind);
            let level = match &b.kind {
                BlockKind::Heading { level, .. } => Some(*level),
                _ => None,
            };
            let preview = source
                .get(b.source_range.clone())
                .map(|s| s.chars().take(80).collect::<String>())
                .unwrap_or_default();
            BlockInfo {
                kind,
                level,
                source_range: b.source_range.clone(),
                preview,
            }
        })
        .collect()
}

pub(crate) fn block_kind_label(kind: &BlockKind) -> String {
    match kind {
        BlockKind::Paragraph => "paragraph".into(),
        BlockKind::Heading { level, .. } => format!("heading({level})"),
        BlockKind::CodeBlock { .. } => "code-fence".into(),
        BlockKind::BlockQuote => "blockquote".into(),
        BlockKind::BulletList { .. } => "bullet-list".into(),
        BlockKind::OrderedList { .. } => "ordered-list".into(),
        BlockKind::ListItem { task, .. } => match task {
            Some(_) => "task-item".into(),
            None => "list-item".into(),
        },
        BlockKind::Table { .. } => "table".into(),
        BlockKind::TableRow { header, .. } => {
            if *header {
                "table-header-row".into()
            } else {
                "table-row".into()
            }
        }
        BlockKind::TableCell => "table-cell".into(),
        BlockKind::ThematicBreak => "thematic-break".into(),
        _ => format!("{kind:?}"),
    }
}

fn mode_label(mode: EditorMode) -> String {
    match mode {
        EditorMode::Wysiwyg => "wysiwyg".into(),
        EditorMode::Source => "source".into(),
        EditorMode::Split => "split".into(),
    }
}

fn response(before: &Snapshot, after: &Snapshot) -> Response {
    Response {
        document_changed: before.source != after.source,
        selection_changed: before.selection != after.selection || before.caret != after.caret,
        focus_changed: before.ui.as_ref().map(|ui| &ui.input_owner)
            != after.ui.as_ref().map(|ui| &ui.input_owner),
        mode_changed: before.mode != after.mode,
        viewport_changed: before.viewport_first_block != after.viewport_first_block
            || before.viewport_offset_px != after.viewport_offset_px
            || before.source_viewport_offset_y != after.source_viewport_offset_y
            || before
                .ui
                .as_ref()
                .map(|ui| (&ui.rich_pane.viewport, &ui.source_pane.viewport))
                != after
                    .ui
                    .as_ref()
                    .map(|ui| (&ui.rich_pane.viewport, &ui.source_pane.viewport)),
    }
}

fn validate_snapshot(label: &str, snapshot: &Snapshot) -> Result<()> {
    let ui = snapshot
        .ui
        .as_ref()
        .context("missing semantic UI observation")?;
    crate::observation::validate(ui, &snapshot.source, &snapshot.mode)
        .with_context(|| format!("{label}: UI state contract"))
}

fn active_caret_bounds(ui: &Observation) -> Option<&crate::observation::Rect> {
    if ui.source_pane.focused {
        ui.source_pane.caret_bounds.as_ref()
    } else {
        ui.rich_pane.caret_bounds.as_ref()
    }
}

fn check_whitespace_caret_advance(before: &Snapshot, after: &Snapshot) -> Result<()> {
    let before = active_caret_bounds(before.ui.as_ref().context("missing before-state")?)
        .context("missing caret before whitespace input")?;
    let after = active_caret_bounds(after.ui.as_ref().context("missing after-state")?)
        .context("missing caret after whitespace input")?;
    check_caret_advance(before, after)
}

fn check_caret_advance(
    before: &crate::observation::Rect,
    after: &crate::observation::Rect,
) -> Result<()> {
    ensure!(after.x > before.x + 1. || after.y > before.y + 1.,
        "typed whitespace did not immediately advance the native caret: before {before:?}, after {after:?}");
    Ok(())
}

fn check_caret_blink(ui: &Observation, expected: bool) -> Result<()> {
    let pane = if ui.source_pane.focused {
        &ui.source_pane
    } else {
        &ui.rich_pane
    };
    let widget = matches!(ui.input_owner, InputOwner::Widget(_));
    let selection = if widget {
        ui.widget_selection
            .as_ref()
            .context("missing widget selection")?
    } else {
        &pane.selection
    };
    ensure!(
        pane.visible && pane.focused && selection.is_empty(),
        "blink probe does not own a visible collapsed caret"
    );
    ensure!(
        pane.caret_blink_on == expected,
        "expected blink {expected}, got {}",
        pane.caret_blink_on
    );
    let bounds = if widget {
        &ui.widget_caret_bounds
    } else {
        &pane.caret_bounds
    }
    .as_ref()
    .context("idle blink lost caret geometry")?;
    let painted = ui
        .painted_carets
        .iter()
        .any(|caret| (caret.x - bounds.x).abs() < 2. && (caret.y - bounds.y).abs() < 2.);
    ensure!(
        painted == expected,
        "caret paint {painted} does not match blink {expected}"
    );
    Ok(())
}

fn check_response(before: &Snapshot, after: &Snapshot, action: &Action) -> Result<()> {
    let before_ui = before.ui.as_ref().context("missing before-state")?;
    let after_ui = after.ui.as_ref().context("missing after-state")?;
    if before_ui.find.as_ref().is_some_and(|find| find.focused)
        && after_ui.find.as_ref().is_some_and(|find| find.focused)
        && before_ui.active_tab_id == after_ui.active_tab_id
        && before.mode == after.mode
    {
        ensure!(
            before.source == after.source,
            "Find query changed Markdown bytes"
        );
        ensure!(
            before_ui.source_pane.selection == after_ui.source_pane.selection
                && before_ui.rich_pane.selection == after_ui.rich_pane.selection,
            "passive Find query/navigation replaced a real document selection"
        );
        if before.mode == "split" {
            let pane = &after_ui.find.as_ref().unwrap().pane;
            if *pane == InputOwner::Source {
                ensure!(
                    before.viewport_first_block == after.viewport_first_block
                        && before.viewport_offset_px == after.viewport_offset_px,
                    "Source Find navigation scrolled the passive WYSIWYG peer"
                );
            } else {
                ensure!(
                    before.source_viewport_offset_y == after.source_viewport_offset_y,
                    "WYSIWYG Find navigation scrolled the passive Source peer"
                );
            }
        }
    }
    if before.mode == "split"
        && after.mode == "split"
        && before.source == after.source
        && before_ui.input_owner == after_ui.input_owner
        && matches!(
            action,
            Action::JumpTo(_)
                | Action::SelectRange { .. }
                | Action::MousePress { .. }
                | Action::MouseMove { .. }
                | Action::MouseRelease { .. }
                | Action::MouseDrag { .. }
                | Action::Keystroke(_)
                | Action::WaitMillis(_)
        )
        && matches!(
            after_ui.input_owner,
            InputOwner::Source | InputOwner::Wysiwyg
        )
        && !matches!(action, Action::Keystroke(key) if matches!(key.as_str(), "alt-cmd-1" | "alt-cmd-2" | "alt-cmd-3"))
    {
        let (old_peer, new_peer) = if after_ui.input_owner == InputOwner::Source {
            (&before_ui.rich_pane, &after_ui.rich_pane)
        } else {
            (&before_ui.source_pane, &after_ui.source_pane)
        };
        ensure!(
            old_peer.selection == new_peer.selection && old_peer.reversed == new_peer.reversed,
            "linked context changed the peer's real input selection"
        );
        ensure!(
            before.source_viewport_offset_y == after.source_viewport_offset_y
                || after_ui.input_owner == InputOwner::Source,
            "linked context scrolled the passive source pane"
        );
        ensure!(
            (before.viewport_first_block == after.viewport_first_block
                && before.viewport_offset_px == after.viewport_offset_px)
                || after_ui.input_owner == InputOwner::Wysiwyg,
            "linked context scrolled the passive rich pane"
        );
    }
    if let Action::MousePress { pane, .. }
    | Action::MousePressPadding { pane, .. }
    | Action::MouseMove { pane, .. }
    | Action::MouseRelease { pane, .. } = action
    {
        // These targets are all inside the painted viewport. No reveal is
        // necessary, and re-resolving stops on a later paint must not hide a
        // physical drag target that moved while the pointer was held down.
        ensure!(
            document_pane(before_ui, pane)?.viewport == document_pane(after_ui, pane)?.viewport,
            "visible pointer input resized its document pane"
        );
        if *pane == InputOwner::Source {
            ensure!(
                before.source_viewport_offset_y == after.source_viewport_offset_y,
                "visible pointer input moved the Source scroll offset"
            );
        } else {
            ensure!(
                before.viewport_first_block == after.viewport_first_block
                    && before.viewport_offset_px == after.viewport_offset_px,
                "visible pointer input moved the rich scroll anchor"
            );
            crate::observation::validate_stationary_rich_layout(before_ui, after_ui)?;
        }
    }
    if before_ui.palette && after_ui.palette {
        ensure!(
            before.source == after.source,
            "palette query input changed document bytes"
        );
    }
    if before.selection.is_empty()
        && before_ui.input_owner == after_ui.input_owner
        && matches!(
            before_ui.input_owner,
            InputOwner::Source | InputOwner::Wysiwyg
        )
    {
        match action {
            Action::Keystroke(key) if key == "right" => ensure!(
                after.caret >= before.caret,
                "Right moved the caret backwards: {} -> {}",
                before.caret,
                after.caret
            ),
            Action::Keystroke(key) if key == "left" => ensure!(
                after.caret <= before.caret,
                "Left moved the caret forwards: {} -> {}",
                before.caret,
                after.caret
            ),
            _ => {}
        }
    }
    let link_editor_active = |ui: &Observation| {
        matches!(
            &ui.input_owner,
            InputOwner::Widget(kind) if kind == "link-destination"
        )
    };
    if (link_editor_active(before_ui) || link_editor_active(after_ui))
        && before.mode == after.mode
        && before_ui.active_tab_id == after_ui.active_tab_id
    {
        ensure!(
            before_ui.rich_pane.viewport == after_ui.rich_pane.viewport
                && before.viewport_first_block == after.viewport_first_block
                && before.viewport_offset_px == after.viewport_offset_px,
            "link destination editing moved the document viewport"
        );
        crate::observation::validate_stationary_rich_placement(before_ui, after_ui)?;
    }
    let preserve_source = match action {
        Action::FocusSource
        | Action::FocusRich
        | Action::JumpTo(_)
        | Action::SelectRange { .. }
        | Action::MouseClick { .. }
        | Action::MouseDrag { .. }
        | Action::MousePress { .. }
        | Action::MousePressPadding { .. }
        | Action::MouseMove { .. }
        | Action::MouseRelease { .. }
        | Action::ScrollPane { .. }
        | Action::ScrollCaretToEdge { .. }
        | Action::ClickVisibleRow { .. }
        | Action::PaletteOutsideClick
        | Action::EditFrontmatter
        | Action::HighlightStyle(_)
        | Action::WaitMillis(_) => true,
        Action::Keystroke(key) => {
            key.starts_with("alt-cmd-")
                && matches!(
                    key.as_str(),
                    "alt-cmd-1" | "alt-cmd-2" | "alt-cmd-3" | "alt-cmd-4"
                )
                || matches!(
                    key.rsplit('-').next(),
                    Some("left" | "right" | "up" | "down" | "home" | "end")
                )
        }
        _ => false,
    };
    ensure!(
        !preserve_source || before.source == after.source,
        "non-editing action changed Markdown bytes"
    );
    if preserve_source && before.mode == after.mode && after.mode != "source" {
        ensure!(
            before_ui.rich_pane.viewport == after_ui.rich_pane.viewport,
            "caret/context-only action changed the rich viewport bounds"
        );
    }
    if matches!(action, Action::HighlightStyle(_))
        || matches!(action, Action::Keystroke(key) if key == "alt-cmd-4")
    {
        ensure!(
            before.selection == after.selection && before.caret == after.caret,
            "display preference moved selection or caret"
        );
        ensure!(
            before.viewport_first_block == after.viewport_first_block
                && before.viewport_offset_px == after.viewport_offset_px,
            "display preference moved the viewport anchor"
        );
        if after.mode != "source" {
            crate::observation::validate_stationary_rich_layout(before_ui, after_ui)?;
        }
    }
    if preserve_source
        && before.source == after.source
        && before.mode == after.mode
        && after.mode != "source"
        && before.viewport_first_block == after.viewport_first_block
        && before.viewport_offset_px == after.viewport_offset_px
        && before_ui.rich_pane.viewport == after_ui.rich_pane.viewport
    {
        crate::observation::validate_stationary_rich_layout(before_ui, after_ui)?;
    }
    match action {
        Action::FocusSource => ensure!(
            after_ui.input_owner == InputOwner::Source,
            "Source did not receive input focus"
        ),
        Action::FocusRich => ensure!(
            after_ui.input_owner == InputOwner::Wysiwyg,
            "WYSIWYG did not receive body input focus"
        ),
        Action::SelectRange { start, end } => {
            ensure!(
                after.selection == (start.min(end).to_owned()..start.max(end).to_owned()),
                "selection action did not select the requested range"
            );
            let pane = if after_ui.source_pane.focused {
                &after_ui.source_pane
            } else {
                &after_ui.rich_pane
            };
            ensure!(
                pane.reversed == (start > end),
                "selection direction was lost"
            );
        }
        Action::MouseDrag { pane, start, end } => {
            ensure!(
                after_ui.input_owner == *pane,
                "mouse drag did not focus {pane:?}"
            );
            ensure!(
                after.selection == (*start.min(end)..*start.max(end)),
                "mouse drag selected {:?}, expected {}..{}",
                after.selection,
                start.min(end),
                start.max(end)
            );
            let state = if *pane == InputOwner::Source {
                &after_ui.source_pane
            } else {
                &after_ui.rich_pane
            };
            ensure!(
                state.reversed == (start > end),
                "mouse drag lost its anchor direction"
            );
        }
        Action::MousePress { pane, offset } | Action::MousePressPadding { pane, offset } => {
            ensure!(
                after_ui.input_owner == *pane,
                "mouse press did not focus {pane:?}"
            );
            ensure!(
                after.selection == (*offset..*offset),
                "mouse press kept a stale selection {:?}, expected caret {offset}",
                after.selection
            );
        }
        Action::MouseMove { pane, offset } | Action::MouseRelease { pane, offset } => {
            let before_pane = if *pane == InputOwner::Source {
                &before_ui.source_pane
            } else {
                &before_ui.rich_pane
            };
            let anchor = if before_pane.reversed {
                before.selection.end
            } else {
                before.selection.start
            };
            ensure!(
                after_ui.input_owner == *pane,
                "pointer event lost {pane:?} input ownership"
            );
            ensure!(
                after.selection == (anchor.min(*offset)..anchor.max(*offset)),
                "{} selected {:?}, expected anchor {anchor} to extent {offset}",
                action.describe(),
                after.selection
            );
            let after_pane = if *pane == InputOwner::Source {
                &after_ui.source_pane
            } else {
                &after_ui.rich_pane
            };
            ensure!(
                after_pane.reversed == (anchor > *offset),
                "pointer event reversed its press anchor"
            );
        }
        Action::MouseClick {
            pane,
            offset,
            count: 1,
        } => {
            ensure!(
                after_ui.input_owner == *pane,
                "mouse click did not focus {pane:?}"
            );
            ensure!(
                after.selection == (*offset..*offset),
                "mouse click selected {:?}, expected caret {offset}",
                after.selection
            );
        }
        Action::Keystroke(key)
            if matches!(
                key.as_str(),
                "alt-cmd-1" | "alt-cmd-2" | "alt-cmd-3" | "alt-cmd-4"
            ) =>
        {
            let expected_mode = match key.as_str() {
                "alt-cmd-1" => Some("wysiwyg"),
                "alt-cmd-2" => Some("source"),
                "alt-cmd-3" => Some("split"),
                _ => None,
            };
            if let Some(mode) = expected_mode {
                ensure!(
                    after.mode == mode,
                    "mode command expected {mode}, got {}",
                    after.mode
                );
            }
            ensure!(
                before.selection == after.selection && before.caret == after.caret,
                "mode/hint change moved selection {:?}/{} to {:?}/{}",
                before.selection,
                before.caret,
                after.selection,
                after.caret
            );
            if key == "alt-cmd-4" {
                ensure!(
                    before_ui.markup_hints != after_ui.markup_hints,
                    "markup hint command did not change display policy"
                );
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_visible_deletion(before: &Snapshot, after: &Snapshot, action: &Action) -> Result<()> {
    if !matches!(action, Action::Keystroke(key) if key == "alt-backspace") {
        return Ok(());
    }
    let before_ui = before
        .ui
        .as_ref()
        .context("missing before-deletion observation")?;
    let after_ui = after
        .ui
        .as_ref()
        .context("missing after-deletion observation")?;
    let pane = &before_ui.input_owner;
    let before_pane = document_pane(before_ui, pane)?;
    let after_pane = document_pane(after_ui, pane)?;
    let caret = before_pane
        .caret_bounds
        .as_ref()
        .context("deletion has no painted caret")?;
    let viewport = before_pane
        .viewport
        .as_ref()
        .context("deletion has no painted viewport")?;
    ensure!(
        caret.y >= viewport.y && caret.y + caret.height <= viewport.y + viewport.height,
        "deletion fixture caret was not already fully visible: caret {caret:?}, pane {viewport:?}"
    );
    ensure!(
        after.source.len() < before.source.len(),
        "word deletion did not remove text"
    );
    ensure!(
        before_pane.viewport == after_pane.viewport,
        "word deletion changed pane bounds"
    );
    if *pane == InputOwner::Source {
        ensure!(
            before.source_viewport_offset_y == after.source_viewport_offset_y,
            "visible word deletion scrolled Source: {:?} -> {:?}",
            before.source_viewport_offset_y,
            after.source_viewport_offset_y
        );
    } else {
        ensure!(
            before.viewport_first_block.unwrap_or(0) > 0,
            "manual scroll fixture did not leave document start"
        );
        let caret_block = before
            .blocks
            .iter()
            .position(|block| {
                block.source_range.start <= before.caret && before.caret <= block.source_range.end
            })
            .context("caret is not inside a paragraph")?;
        // Midpane and bottom-edge fixtures exercise invalidated bounds for a
        // later visible block; top-edge fixtures intentionally edit the anchor.
        ensure!(
            caret_block >= before.viewport_first_block.unwrap(),
            "deletion fixture caret block precedes the manual viewport anchor"
        );
        ensure!(before.viewport_first_block == after.viewport_first_block && before.viewport_offset_px == after.viewport_offset_px,
            "visible word deletion jumped rich viewport: block {:?}/offset {:?} -> block {:?}/offset {:?}",
            before.viewport_first_block, before.viewport_offset_px, after.viewport_first_block, after.viewport_offset_px);
    }
    let after_caret = after_pane
        .caret_bounds
        .as_ref()
        .context("word deletion lost the caret")?;
    ensure!(
        (caret.y - after_caret.y).abs() <= 1.,
        "visible word deletion moved caret row: {} -> {}",
        caret.y,
        after_caret.y
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Invariants
// ---------------------------------------------------------------------------

/// Property assertions on the final snapshot. Failures include the
/// scenario name and a one-line diagnostic so a regression can be
/// pinpointed without grepping the JSONL trace.
pub fn check_invariants(
    scenario_name: &str,
    snapshot: &Snapshot,
    invariants: &[Invariant],
) -> Result<()> {
    for inv in invariants {
        match inv {
            Invariant::ObserveEveryPaint | Invariant::VisibleDeletionKeepsViewport => {}
            Invariant::SourceContains(needle) => {
                ensure!(
                    snapshot.source.contains(needle),
                    "[{scenario_name}] final source must contain {needle:?}; got: {source}",
                    scenario_name = scenario_name,
                    needle = needle,
                    source = snapshot.source,
                );
            }
            Invariant::SourceNotContains(needle) => {
                ensure!(
                    !snapshot.source.contains(needle),
                    "[{scenario_name}] final source must NOT contain {needle:?}; got: {source}",
                    scenario_name = scenario_name,
                    needle = needle,
                    source = snapshot.source,
                );
            }
            Invariant::CaretAt(offset) => {
                ensure!(
                    snapshot.caret == *offset,
                    "[{scenario_name}] caret must be at {offset}, got {got}",
                    scenario_name = scenario_name,
                    offset = offset,
                    got = snapshot.caret,
                );
            }
            Invariant::CaretInBlock(idx) => {
                let block = snapshot.blocks.get(*idx).unwrap_or_else(|| {
                    panic!(
                        "[{scenario_name}] invariant CaretInBlock({idx}) but only {} blocks rendered",
                        snapshot.blocks.len(),
                    )
                });
                ensure!(
                    block.source_range.contains(&snapshot.caret),
                    "[{scenario_name}] caret {caret} not in block {idx} ({start}..{end})",
                    scenario_name = scenario_name,
                    caret = snapshot.caret,
                    idx = *idx,
                    start = block.source_range.start,
                    end = block.source_range.end,
                );
            }
            Invariant::BlockCount(n) => {
                ensure!(
                    snapshot.blocks.len() == *n,
                    "[{scenario_name}] expected {want} top-level blocks, got {got} ({kinds}); source: {source}",
                    scenario_name = scenario_name,
                    want = *n,
                    got = snapshot.blocks.len(),
                    kinds = snapshot
                        .blocks
                        .iter()
                        .map(|b| b.kind.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    source = snapshot.source,
                );
            }
            Invariant::Mode(modes) => {
                ensure!(
                    modes.iter().any(|m| mode_label(*m) == snapshot.mode),
                    "[{scenario_name}] mode must be one of {want:?}, got {got}",
                    scenario_name = scenario_name,
                    want = modes.iter().map(|m| mode_label(*m)).collect::<Vec<_>>(),
                    got = snapshot.mode,
                );
            }
            Invariant::Selection(sel) => {
                ensure!(
                    snapshot.selection == *sel,
                    "[{scenario_name}] selection must be {sel:?}, got {got:?}",
                    scenario_name = scenario_name,
                    sel = sel,
                    got = snapshot.selection,
                );
            }
            Invariant::ShadowVisibleAtSteps(steps) => {
                if steps.contains(&snapshot.step) {
                    let ui = snapshot.ui.as_ref().expect("native observation");
                    let peer = if ui.input_owner == InputOwner::Source { &ui.rich_pane } else { &ui.source_pane };
                    let shadow = peer.shadow.as_ref().context("Split peer has no shadow")?;
                    ensure!(!shadow.cursor_quads.is_empty(), "[{scenario_name}] linked cursor did not paint");
                    let rows: Vec<_> = if ui.input_owner == InputOwner::Source {
                        ui.painted_rich.iter().flat_map(|leaf| &leaf.rows).collect()
                    } else { ui.painted_source.iter().collect() };
                    if let Some((row, (_, x))) = rows.iter().find_map(|row| {
                        row.stops.iter().find(|(source, _)| *source == shadow.caret).map(|stop| (*row, stop))
                    }) {
                        ensure!(shadow.cursor_quads.iter().any(|quad|
                            (quad.x + 2. - x).abs() < 2. && (quad.y - row.bounds.y).abs() < 2.
                                && quad.width >= 4.),
                            "[{scenario_name}] linked marker disagrees with native glyph caret stop");
                    }
                    if !shadow.selection.is_empty() {
                        ensure!(!shadow.selection_quads.is_empty(), "[{scenario_name}] linked selection did not paint");
                    }
                }
            }
            Invariant::FindAtSteps(expected) => {
                if let Some(expected) = expected.iter().find(|expected| expected.step == snapshot.step) {
                    let ui = snapshot.ui.as_ref().context("Find probe has no observation")?;
                    let find = ui.find.as_ref().context("Find query was not opened by its native shortcut")?;
                    ensure!(
                        ui.input_owner == InputOwner::Find && find.focused && find.native_input_registered,
                        "Find query does not own native keyboard input"
                    );
                    ensure!(find.query == expected.query, "Find query mismatch: {:?}, expected {:?}", find.query, expected.query);
                    ensure!(find.matches == expected.matches, "Find result ranges differ from independent literal fixture positions: {:?}, expected {:?}", find.matches, expected.matches);
                    ensure!(find.current == expected.current, "Find navigation selected {:?}, expected {:?}", find.current, expected.current);
                    ensure!(find.pane == expected.pane, "Find results target {:?}, expected {:?}", find.pane, expected.pane);
                }
            }
            Invariant::CaretInRange(range) => ensure!(
                range.contains(&snapshot.caret),
                "[{scenario_name}] caret {} escaped expected neighboring range {range:?}",
                snapshot.caret,
            ),
            Invariant::CaretVisible | Invariant::CaretVisibleAfterEveryAction => ensure!(
                snapshot.caret_visible == Some(true),
                "[{scenario_name}] rich caret is not visible at byte {} (viewport block {:?}, offset {:?})",
                snapshot.caret,
                snapshot.viewport_first_block,
                snapshot.viewport_offset_px,
            ),
            Invariant::ViewportFirstBlockAtLeastAfterEveryAction(minimum) => ensure!(
                snapshot
                    .viewport_first_block
                    .is_some_and(|block| block >= *minimum),
                "[{scenario_name}] rich viewport jumped toward block 0 (first block {:?}, offset {:?})",
                snapshot.viewport_first_block,
                snapshot.viewport_offset_px,
            ),
            Invariant::SourceCaretVisibleFromStep(first_step) => {
                if snapshot.step >= *first_step {
                    ensure!(
                        snapshot.source_caret_visible == Some(true),
                        "[{scenario_name}] source caret is not visible at byte {} (viewport {:?}, caret {:?}, offset {:?})",
                        snapshot.caret,
                        snapshot.source_viewport_y,
                        snapshot.source_caret_y,
                        snapshot.source_viewport_offset_y,
                    );
                }
            }
            Invariant::SourceViewportScrolledFromStep(first_step) => {
                if snapshot.step >= *first_step {
                    ensure!(
                        snapshot
                            .source_viewport_offset_y
                            .is_some_and(|offset| offset < -1.),
                        "[{scenario_name}] source viewport jumped to top (offset {:?})",
                        snapshot.source_viewport_offset_y,
                    );
                }
            }
            Invariant::InputOwnedBy(owner) => ensure!(
                snapshot
                    .ui
                    .as_ref()
                    .is_some_and(|ui| ui.input_owner == *owner),
                "[{scenario_name}] expected input owner {owner:?}"
            ),
            Invariant::SourceEquals(expected) => ensure!(
                snapshot.source == *expected,
                "[{scenario_name}] document bytes differ from expected result"
            ),
            Invariant::SourceAtSteps(sources) => {
                if let Some((_, expected)) = sources.iter().find(|(step, _)| *step == snapshot.step) {
                    ensure!(snapshot.source == *expected,
                        "[{scenario_name}] step {} document bytes differ: {:?} != {expected:?}",
                        snapshot.step, snapshot.source);
                }
            }
            Invariant::PlainContextAtSteps(steps) => {
                if steps.contains(&snapshot.step) {
                    let ui = snapshot.ui.as_ref().context("missing context observation")?;
                    ensure!(ui.editing_context_hint.is_none() && ui.markup_hint_label.is_none()
                        && ui.markup_hint_bounds.is_none(),
                        "[{scenario_name}] plain paragraph retained container hints: {:?}, {:?}",
                        ui.editing_context_hint, ui.markup_hint_label);
                }
            }
            Invariant::RichDisplayNotContains(needle) => ensure!(
                snapshot.ui.as_ref().is_some_and(|ui| ui
                    .painted_rich
                    .iter()
                    .all(|leaf| !leaf.text.contains(needle))),
                "[{scenario_name}] hidden markup {needle:?} is still painted"
            ),
            Invariant::RichDisplayContains(needle) => ensure!(snapshot.ui.as_ref().is_some_and(|ui|
                ui.painted_rich.iter().any(|leaf| leaf.text.contains(needle))),
                "[{scenario_name}] expected text {needle:?} is not painted"),
            Invariant::ActiveTabVisible => {
                let ui = snapshot.ui.as_ref().expect("native UI observation");
                let strip = ui.tab_strip_bounds.as_ref().expect("tab strip bounds");
                let active = ui.active_tab_bounds.as_ref().expect("active tab bounds");
                ensure!(
                    active.width > 0.
                        && active.x >= strip.x - 1.
                        && active.x + active.width <= strip.x + strip.width + 1.
                        && active.y >= strip.y - 1.
                        && active.y + active.height <= strip.y + strip.height + 1.,
                    "[{scenario_name}] active tab escaped strip: active {active:?}, strip {strip:?}"
                );
            }
            Invariant::TableControlsHidden => ensure!(snapshot.ui.as_ref().is_some_and(|ui|
                ui.table_toolbar_bounds.is_none()),
                "[{scenario_name}] table context painted unsolicited controls"),
            Invariant::TableControlsVisible => ensure!(snapshot.ui.as_ref().is_some_and(|ui|
                ui.table_toolbar_bounds.is_some()),
                "[{scenario_name}] active table cell has no contextual controls"),
            Invariant::TableShape { rows, columns } => {
                let table = snapshot.ui.as_ref().and_then(|ui| ui.table_shapes.first())
                    .context("missing painted table context")?;
                ensure!(table.columns_per_row.len() == *rows && table.columns_per_row.iter().all(|count| *count == *columns),
                    "[{scenario_name}] table shape does not match {rows} rows and {columns} columns");
            }
            Invariant::CaretBlinkSequence(phases) => {
                let ui = snapshot.ui.as_ref().context("missing blink observation")?;
                let expected = *phases.get(snapshot.step).context("missing expected blink phase")?;
                check_caret_blink(ui, expected).with_context(|| format!("{scenario_name} step {}", snapshot.step))?;
            }
            Invariant::CaretBlinkAtSteps(phases) => {
                if let Some((_, expected)) = phases.iter().find(|(step, _)| *step == snapshot.step) {
                    let ui = snapshot.ui.as_ref().context("missing blink observation")?;
                    check_caret_blink(ui, *expected).with_context(|| format!("{scenario_name} step {}", snapshot.step))?;
                }
            }
            Invariant::CaretAtSteps(stops) => {
                if let Some((_, expected)) = stops.iter().find(|(step, _)| *step == snapshot.step) {
                    ensure!(snapshot.caret == *expected,
                        "[{scenario_name}] step {} expected caret {expected}, got {}",
                        snapshot.step, snapshot.caret);
                }
            }
            Invariant::WhitespaceAdvancesCaret | Invariant::RepeatedEnterIsNoopAtStep(_) | Invariant::EnterMovesCaretDownAtStep(_) | Invariant::CaretStaysOnSameRowAtStep(_) => {},
            Invariant::WidgetDraftEquals(expected) => ensure!(
                snapshot.ui.as_ref().and_then(|ui| ui.widget_draft.as_ref()) == Some(expected),
                "[{scenario_name}] widget draft did not match the visible input"
            ),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Test entry point (called from visual_tests.rs)
// ---------------------------------------------------------------------------

/// Run the full use-case matrix (curated + generated) against the
/// open window, writing traces to `output_dir/usecases/`. Returns the
/// number of scenarios run.
pub fn run_all(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    seed: u64,
    count: usize,
    output_dir: &Path,
    record_frames: bool,
) -> Result<usize> {
    let scenarios = generate_scenarios(seed, count);
    let curated_len = curated_scenarios().len();
    let mut passed = 0usize;
    for scenario in &scenarios {
        run_scenario(cx, window, workspace, scenario, output_dir, record_frames)?;
        passed += 1;
    }
    println!(
        "PASS usecases: {} scenarios ({} curated, {} generated, seed {seed})",
        passed,
        curated_len,
        count.saturating_sub(curated_len),
    );
    Ok(passed)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use markrust_core::rich::RichEngine;
    use markrust_core::Document;

    #[test]
    fn generator_is_deterministic() {
        let a = generate_scenarios(0xC0FFEE, 50);
        let b = generate_scenarios(0xC0FFEE, 50);
        assert_eq!(a.len(), b.len());
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            assert_eq!(x.name, y.name, "scenario {i} name mismatch");
            assert_eq!(x.actions.len(), y.actions.len());
        }
    }

    #[test]
    fn generator_produces_at_least_500_scenarios() {
        let scenarios = generate_scenarios(42, 500);
        assert!(
            scenarios.len() >= 500,
            "expected >=500 scenarios, got {}",
            scenarios.len()
        );
    }

    #[test]
    fn curated_includes_empty_task_enter_exits() {
        let scenarios = curated_scenarios();
        let names: Vec<_> = scenarios.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"empty_task_item_enter_exits_and_continues_as_paragraph"),
            "curated scenarios missing the user-requested example; got: {names:?}"
        );
    }

    #[test]
    fn find_journeys_use_native_query_navigation_and_deep_caret_contracts() {
        let scenarios: Vec<_> = find_scenarios();
        assert_eq!(scenarios.len(), 10);
        for scenario in &scenarios {
            assert!(scenario.actions.iter().any(|action| matches!(action,
                Action::Keystroke(key) if key == "cmd-f")));
            assert!(scenario.actions.iter().any(|action| matches!(action,
                Action::Keystroke(key) if key == "escape")));
            assert!(scenario
                .invariants
                .iter()
                .any(|invariant| matches!(invariant, Invariant::ObserveEveryPaint)));
            assert!(scenario
                .invariants
                .iter()
                .any(|invariant| matches!(invariant, Invariant::FindAtSteps(_))));
        }
        for owner in ["source", "wysiwyg"] {
            assert!(scenarios.iter().any(|scenario| scenario.name
                == format!("find_{owner}_escape_commits_visible_result_without_old_caret_jump")));
        }
        let matches: Vec<_> = FIND_CONTENT.match_indices("слово").collect();
        assert_eq!(matches.len(), 3);
        assert!(FIND_CONTENT.contains("**слово**"));
        assert_eq!(FIND_CONTENT.match_indices("👩🏽‍💻").count(), 2);
    }

    #[test]
    fn block_count_for_templates() {
        assert!(count_blocks("- one\n- two\n- three\n") >= 3);
        assert!(count_blocks("# h1\n## h2\n") >= 2);
        assert!(count_blocks("") >= 1);
    }

    #[test]
    fn block_kind_label_is_stable() {
        let source = "# heading\n\nparagraph\n\n- bullet\n";
        let mut tree = RichEngine::new();
        let doc = Document::new(source);
        tree.sync(&doc);
        let labels: Vec<_> = tree
            .tree()
            .blocks
            .iter()
            .map(|b| block_kind_label(&b.kind))
            .collect();
        assert!(labels.iter().any(|l| l.starts_with("heading")));
        assert!(labels.contains(&"paragraph".to_string()));
        assert!(labels.contains(&"bullet-list".to_string()));
    }

    #[test]
    fn action_describe_is_human_readable() {
        let action = Action::Keystroke("cmd-1".into());
        assert_eq!(action.describe(), "keystroke: cmd-1");
        let action = Action::JumpTo(42);
        assert_eq!(action.describe(), "jump-to: 42");
        assert_eq!(Action::WaitMillis(500).describe(), "idle: 500ms");
    }

    #[test]
    fn pointer_journeys_capture_press_move_and_release_separately() {
        let scenarios = curated_scenarios();
        for owner in ["source", "wysiwyg", "split_source", "split_wysiwyg"] {
            let scenario = scenarios
                .iter()
                .find(|scenario| {
                    scenario.name
                        == format!("{owner}_pointer_release_uses_final_position_after_last_move")
                })
                .unwrap();
            assert!(scenario
                .invariants
                .iter()
                .any(|invariant| matches!(invariant, Invariant::ObserveEveryPaint)));
            assert!(matches!(scenario.actions[1], Action::MousePress { .. }));
            assert!(matches!(scenario.actions[2], Action::MouseMove { .. }));
            assert!(matches!(scenario.actions[3], Action::MouseRelease { .. }));
            assert!(scenarios.iter().any(|scenario| scenario.name
                == format!("{owner}_pointer_forward_release_without_move_keeps_press_anchor")));
            assert!(scenarios.iter().any(|scenario| scenario.name
                == format!("{owner}_pointer_reverse_release_without_move_keeps_press_anchor")));
            assert!(scenarios.iter().any(|scenario| scenario.name
                == format!("{owner}_pointer_padding_press_replaces_old_anchor")));
        }
    }

    #[test]
    fn pointer_journey_names_are_unique() {
        let scenarios = curated_scenarios();
        let mut names = std::collections::HashSet::new();
        for scenario in scenarios {
            assert!(
                names.insert(scenario.name.clone()),
                "duplicate journey {}",
                scenario.name
            );
        }
    }

    #[test]
    fn visible_deletion_oracle_rejects_transient_scroll_or_caret_jump() {
        let before = source_deletion_fixture();
        let mut after = before.clone();
        after.source = "alpha beta".into();
        let action = Action::Keystroke("alt-backspace".into());
        assert!(check_visible_deletion(&before, &after, &action).is_ok());
        after.source_viewport_offset_y = Some(0.);
        assert!(check_visible_deletion(&before, &after, &action).is_err());
        after.source_viewport_offset_y = before.source_viewport_offset_y;
        after
            .ui
            .as_mut()
            .unwrap()
            .source_pane
            .caret_bounds
            .as_mut()
            .unwrap()
            .y = 10.;
        assert!(check_visible_deletion(&before, &after, &action).is_err());
    }

    fn source_deletion_fixture() -> Snapshot {
        let viewport = crate::observation::Rect {
            x: 0.,
            y: 0.,
            width: 100.,
            height: 100.,
        };
        let caret = crate::observation::Rect {
            x: 70.,
            y: 50.,
            width: 2.,
            height: 20.,
        };
        let mut ui = Observation {
            input_owner: InputOwner::Source,
            ..Observation::default()
        };
        ui.source_pane.visible = true;
        ui.source_pane.focused = true;
        ui.source_pane.viewport = Some(viewport);
        ui.source_pane.caret_bounds = Some(caret);
        Snapshot {
            step: 3,
            action: Action::JumpTo(16),
            source: "alpha beta gamma".into(),
            caret: 16,
            selection: 16..16,
            mode: "source".into(),
            blocks: Vec::new(),
            line_count: 1,
            viewport_height: 100.,
            viewport_first_block: None,
            viewport_offset_px: None,
            caret_visible: None,
            viewport_y: None,
            caret_y: None,
            source_caret_visible: Some(true),
            source_viewport_offset_y: Some(-1100.),
            source_viewport_y: Some((0., 100.)),
            source_caret_y: Some((50., 70.)),
            ui: Some(ui),
            response: None,
            timestamp_ms: 0,
            paint_frames: Vec::new(),
        }
    }

    #[test]
    fn whitespace_oracle_rejects_a_stationary_caret() {
        let before = crate::observation::Rect {
            x: 10.,
            y: 20.,
            width: 2.,
            height: 24.,
        };
        assert!(check_caret_advance(&before, &before).is_err());
        let mut after = before.clone();
        after.x += 6.;
        assert!(check_caret_advance(&before, &after).is_ok());
        after.x = 0.;
        after.y += 24.;
        assert!(check_caret_advance(&before, &after).is_ok());
    }

    #[test]
    fn blink_oracle_requires_paint_and_keeps_off_frame_geometry() {
        let bounds = crate::observation::Rect {
            x: 10.,
            y: 20.,
            width: 2.,
            height: 24.,
        };
        let mut ui = Observation::default();
        ui.rich_pane.visible = true;
        ui.rich_pane.focused = true;
        ui.rich_pane.caret_blink_on = true;
        ui.rich_pane.caret_bounds = Some(bounds.clone());
        assert!(check_caret_blink(&ui, true).is_err());
        ui.painted_carets.push(bounds);
        assert!(check_caret_blink(&ui, true).is_ok());
        ui.rich_pane.caret_blink_on = false;
        assert!(check_caret_blink(&ui, false).is_err());
        ui.painted_carets.clear();
        assert!(check_caret_blink(&ui, false).is_ok());
        ui.rich_pane.caret_bounds = None;
        assert!(check_caret_blink(&ui, false).is_err());
    }

    #[test]
    fn widget_blink_oracle_uses_the_draft_not_the_body_anchor() {
        let widget = crate::observation::Rect {
            x: 80.,
            y: 50.,
            width: 2.,
            height: 24.,
        };
        let body = crate::observation::Rect {
            x: 10.,
            y: 20.,
            width: 2.,
            height: 24.,
        };
        let mut ui = Observation {
            input_owner: InputOwner::Widget("link-destination".into()),
            ..Observation::default()
        };
        ui.rich_pane.visible = true;
        ui.rich_pane.focused = true;
        ui.rich_pane.caret_blink_on = true;
        ui.rich_pane.selection = 3..7;
        ui.rich_pane.caret_bounds = Some(body.clone());
        ui.widget_selection = Some(2..2);
        ui.widget_caret_bounds = Some(widget.clone());
        ui.painted_carets.push(body);
        assert!(check_caret_blink(&ui, true).is_err());
        ui.painted_carets = vec![widget];
        assert!(check_caret_blink(&ui, true).is_ok());
        ui.widget_selection = Some(1..2);
        assert!(check_caret_blink(&ui, true).is_err());
        ui.widget_selection = Some(2..2);
        ui.widget_caret_bounds = None;
        assert!(check_caret_blink(&ui, true).is_err());
    }

    #[test]
    fn snapshot_roundtrips_through_serde() {
        let snap = Snapshot {
            step: 0,
            action: Action::JumpTo(0),
            source: "hello".into(),
            caret: 5,
            selection: 5..5,
            mode: "wysiwyg".into(),
            blocks: vec![],
            line_count: 1,
            viewport_height: 100.,
            viewport_first_block: Some(0),
            viewport_offset_px: Some(0.),
            caret_visible: Some(true),
            viewport_y: Some((0., 100.)),
            caret_y: Some((10., 30.)),
            source_caret_visible: None,
            source_viewport_offset_y: None,
            source_viewport_y: None,
            source_caret_y: None,
            ui: None,
            response: None,
            timestamp_ms: 0,
            paint_frames: Vec::new(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source, snap.source);
        assert_eq!(back.caret, snap.caret);
        assert_eq!(back.mode, snap.mode);
    }
}
