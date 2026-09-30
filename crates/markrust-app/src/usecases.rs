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
use gpui::{AppContext, Entity, Focusable, HeadlessAppContext, Keystroke, Modifiers, WindowHandle};
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
    /// Whether the focused source caret is inside its pane's viewport.
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
    NewTab,
    SwitchTab(usize),
    EditFrontmatter,
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Action::Keystroke(s) => format!("keystroke: {s}"),
            Action::InsertText(s) => format!("insert-text: {s:?}"),
            Action::JumpTo(o) => format!("jump-to: {o}"),
            Action::FocusSource => "focus-source".into(),
            Action::FocusRich => "focus-wysiwyg".into(),
            Action::SelectRange { start, end } => format!("select-range: {start}..{end}"),
            Action::NewTab => "new-tab".into(),
            Action::SwitchTab(index) => format!("switch-tab: {index}"),
            Action::EditFrontmatter => "edit-frontmatter".into(),
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
}

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
    RichDisplayNotContains(String),
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
    vec![
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
                Invariant::BlockCount(120),
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
    ]
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
                let _ = screenshot.save(
                    output_dir
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
    doc_entity.update(cx, |doc, cx| {
        doc.replace_range(0, len, &source);
        // Without draining the parse pump, `syntax_spans` keeps claiming
        // positions from the prior buffer revision; the next outline
        // query then OOB-slices into the new (often empty) source. The
        // fixture bootstrap uses the same barrier (`wait_for_parse`).
        let _ = doc.wait_for_parse(Duration::from_secs(30));
        cx.notify();
    });
    move_caret(cx, window, workspace, setup_caret.min(source.len()))?;
    // `replace_range` triggers a deferred re-parse + render cycle. Drawing a
    // frame first lets the document settle so the first snapshot mirrors the
    // rich engine's parsed layout, not the pre-replace geometry.
    draw(cx, window)?;

    let mut snapshots = Vec::with_capacity(scenario.actions.len() + 1);
    let mut initial = capture_snapshot(cx, window, workspace, 0, &Action::JumpTo(setup_caret))?;
    initial.timestamp_ms = started.elapsed().as_millis() as u64;
    if record_frames {
        cx.capture_screenshot(window.into())?
            .save(usecases_dir.join(format!("{}.step-000.png", scenario.name)))?;
    }
    writeln!(file, "{}", serde_json::to_string(&initial)?)?;
    file.flush()?;
    validate_snapshot(&scenario.name, &initial)?;
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
        draw(cx, window).with_context(|| {
            format!("{label}: drawing failed; trace ends at the last completed step")
        })?;
        let mut snapshot =
            capture_snapshot(cx, window, workspace, step + 1, action).with_context(|| {
                format!("{label}: observation failed; trace ends at the last completed step")
            })?;
        let previous = snapshots.last().unwrap();
        snapshot.timestamp_ms = started.elapsed().as_millis() as u64;
        snapshot.response = Some(response(previous, &snapshot));
        if record_frames {
            cx.capture_screenshot(window.into())?.save(
                usecases_dir.join(format!("{}.step-{:03}.png", scenario.name, snapshot.step)),
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
        check_invariants(&label, &snapshot, &step_invariants)?;
        snapshots.push(snapshot);
    }
    let final_snapshot = snapshots.last().cloned().unwrap();
    check_invariants(&scenario.name, &final_snapshot, &scenario.invariants)?;
    Ok(final_snapshot)
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
    }
    Ok(())
}

/// Three-pass frame advance + `window.refresh()` + `window.draw()`, mirroring
/// the baseline `keystroke()` helper in `visual_tests.rs`. Without
/// `simulate_next_frame` / `refresh()` the input pipeline's on_action
/// handlers don't always flush their document mutations into the snapshot
/// reader.
fn draw(cx: &mut HeadlessAppContext, window: WindowHandle<MarkRustWindow>) -> Result<()> {
    for _ in 0..3 {
        cx.advance_clock(Duration::from_millis(35));
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| {
            window.simulate_next_frame(cx);
            window.refresh();
            window.draw(cx).clear(cx);
        })?;
    }
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
            if mode == EditorMode::Source || mode == EditorMode::Split && source_focused {
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

fn check_response(before: &Snapshot, after: &Snapshot, action: &Action) -> Result<()> {
    let before_ui = before.ui.as_ref().context("missing before-state")?;
    let after_ui = after.ui.as_ref().context("missing after-state")?;
    let preserve_source = match action {
        Action::FocusSource
        | Action::FocusRich
        | Action::JumpTo(_)
        | Action::SelectRange { .. }
        | Action::EditFrontmatter => true,
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
                    kinds = snapshot.blocks.iter().map(|b| b.kind.as_str()).collect::<Vec<_>>().join(", "),
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
                snapshot.viewport_first_block.is_some_and(|block| block >= *minimum),
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
                        snapshot.source_viewport_offset_y.is_some_and(|offset| offset < -1.),
                        "[{scenario_name}] source viewport jumped to top (offset {:?})",
                        snapshot.source_viewport_offset_y,
                    );
                }
            }
            Invariant::InputOwnedBy(owner) => ensure!(snapshot.ui.as_ref().is_some_and(|ui| ui.input_owner == *owner),
                "[{scenario_name}] expected input owner {owner:?}"),
            Invariant::SourceEquals(expected) => ensure!(snapshot.source == *expected,
                "[{scenario_name}] document bytes differ from expected result"),
            Invariant::RichDisplayNotContains(needle) => ensure!(snapshot.ui.as_ref().is_some_and(|ui|
                ui.painted_rich.iter().all(|leaf| !leaf.text.contains(needle))),
                "[{scenario_name}] hidden markup {needle:?} is still painted"),
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
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source, snap.source);
        assert_eq!(back.caret, snap.caret);
        assert_eq!(back.mode, snap.mode);
    }
}
