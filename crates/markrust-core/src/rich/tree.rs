// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The rich document tree: a structural projection of the markdown source.
//!
//! The rope buffer remains the single source of truth; this tree is derived
//! from it and is authoritative only for interpretation and command
//! targeting. Every node carries its `source_range` (byte range in the
//! source) plus enough delimiter fidelity to re-serialize a touched block
//! without churning the author's syntax choices. Unsupported constructs
//! become [`BlockKind::Opaque`] / [`Inline::OpaqueInline`] and round-trip
//! verbatim.

use std::ops::Range;

/// Stable identity of a block across reparses (best effort; fresh imports
/// assign fresh ids, the engine remaps unchanged blocks in a later phase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u64);

/// Monotonic [`NodeId`] source.
#[derive(Debug, Default)]
pub struct IdGen(u64);

impl IdGen {
    pub fn next_id(&mut self) -> NodeId {
        self.0 += 1;
        NodeId(self.0)
    }
}

/// Whole-document projection.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RichTree {
    /// YAML frontmatter is document metadata, not a block (panel edits it).
    pub frontmatter: Option<Frontmatter>,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frontmatter {
    /// Raw text including the `---` delimiters and trailing newline.
    pub raw: String,
    pub source_range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub id: NodeId,
    pub source_range: Range<usize>,
    /// Hash of the source slice at import time; used by the engine to keep
    /// NodeIds stable for unchanged blocks across reparses.
    pub content_hash: u64,
    pub kind: BlockKind,
    /// Child blocks (container kinds only).
    pub children: Vec<Block>,
    /// Inline content (leaf kinds only).
    pub inlines: Vec<Inline>,
}

impl Block {
    pub fn is_container(&self) -> bool {
        matches!(
            self.kind,
            BlockKind::BlockQuote
                | BlockKind::BulletList { .. }
                | BlockKind::OrderedList { .. }
                | BlockKind::ListItem { .. }
                | BlockKind::Table { .. }
                | BlockKind::TableRow { .. }
                | BlockKind::FootnoteDefinition { .. }
                | BlockKind::DefinitionList
                | BlockKind::DefinitionItem { .. }
                | BlockKind::DefinitionTerm
                | BlockKind::DefinitionDetails
        )
    }
}

/// How a heading was written in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadingStyle {
    Atx,
    Setext,
}

/// Fence details for a fenced code block; `None` means indented code block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FenceFidelity {
    pub fence_char: u8,
    pub fence_length: usize,
    pub fence_offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnAlign {
    None,
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockKind {
    Paragraph,
    Heading {
        level: u8,
        style: HeadingStyle,
    },
    CodeBlock {
        /// Full info string of the fence (first word is the language).
        info: String,
        /// `None` for indented code blocks.
        fence: Option<FenceFidelity>,
        /// Literal body text (also mirrored as a single inline run).
        literal: String,
    },
    BlockQuote,
    BulletList {
        tight: bool,
        /// `b'-'`, `b'*'`, or `b'+'`.
        marker: u8,
    },
    OrderedList {
        start: usize,
        tight: bool,
        /// `b'.'` or `b')'`.
        delimiter: u8,
    },
    ListItem {
        /// `Some(checked)` when this is a task-list item.
        task: Option<bool>,
    },
    Table {
        alignments: Vec<ColumnAlign>,
    },
    TableRow {
        header: bool,
    },
    TableCell,
    ThematicBreak,
    /// `[^label]:` footnote definition; children are the body blocks.
    FootnoteDefinition {
        label: String,
    },
    /// PHP-Extra / Typora definition list (`term` / `: details`).
    DefinitionList,
    DefinitionItem {
        tight: bool,
    },
    DefinitionTerm,
    DefinitionDetails,
    /// Anything we do not model (HTML blocks, …): inert, serialized
    /// verbatim from `raw`. Dollar math is [`Inline::Math`], not opaque.
    Opaque {
        raw: String,
    },
}

/// Inline marks as a small bitset (avoids a bitflags dependency).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MarkSet(u8);

impl MarkSet {
    pub const BOLD: MarkSet = MarkSet(1 << 0);
    pub const ITALIC: MarkSet = MarkSet(1 << 1);
    pub const STRIKE: MarkSet = MarkSet(1 << 2);
    pub const CODE: MarkSet = MarkSet(1 << 3);
    /// Typora `==highlight==` (not GFM; comrak has no node, applied on import).
    pub const HIGHLIGHT: MarkSet = MarkSet(1 << 4);
    /// Comrak `^superscript^` / HTML `<sup>`.
    pub const SUP: MarkSet = MarkSet(1 << 5);
    /// Comrak `~subscript~` / HTML `<sub>`.
    pub const SUB: MarkSet = MarkSet(1 << 6);

    pub fn empty() -> MarkSet {
        MarkSet(0)
    }
    pub fn with(self, other: MarkSet) -> MarkSet {
        MarkSet(self.0 | other.0)
    }
    pub fn without(self, other: MarkSet) -> MarkSet {
        MarkSet(self.0 & !other.0)
    }
    pub fn contains(self, other: MarkSet) -> bool {
        self.0 & other.0 == other.0
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// Which delimiter characters the author used for emphasis marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkFidelity {
    /// `b'*'` or `b'_'` for italic.
    pub emph_delim: u8,
    /// `b'*'` or `b'_'` for bold (doubled on output).
    pub strong_delim: u8,
    /// Number of backticks around inline code.
    pub code_backticks: usize,
    /// Identity of the originating emphasis nodes (0 = unassigned): adjacent
    /// distinct nodes with the same mark must not merge on serialization.
    pub emph_group: u64,
    pub strong_group: u64,
    pub strike_group: u64,
    pub highlight_group: u64,
    pub sup_group: u64,
    pub sub_group: u64,
}

impl Default for MarkFidelity {
    fn default() -> Self {
        MarkFidelity {
            emph_delim: b'*',
            strong_delim: b'*',
            code_backticks: 1,
            emph_group: 0,
            strong_group: 0,
            strike_group: 0,
            highlight_group: 0,
            sup_group: 0,
            sub_group: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinkAttrs {
    pub url: String,
    pub title: Option<String>,
    /// True when the source had no `](...)` form (autolink / bare URL).
    pub autolink: bool,
    /// Identity of the originating link node: adjacent links with identical
    /// attrs must not merge into one span on serialization.
    pub group: u64,
}

/// How a hard break was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakStyle {
    TwoSpaces,
    Backslash,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Run {
        /// Visible text with escapes/entities resolved.
        text: String,
        /// Original source slice; kept until the run's text is edited so the
        /// author's exact escaping is re-emitted verbatim.
        raw: Option<Box<str>>,
        source_range: Range<usize>,
        marks: MarkSet,
        link: Option<LinkAttrs>,
        fidelity: MarkFidelity,
    },
    Image {
        alt: String,
        url: String,
        title: Option<String>,
        source_range: Range<usize>,
        /// Emphasis context the image sits inside (kept through transitions).
        marks: MarkSet,
        /// Enclosing link, when the image is the content of one.
        link: Option<LinkAttrs>,
    },
    SoftBreak,
    HardBreak {
        style: BreakStyle,
    },
    /// `$…$` / `$$…$$` TeX (comrak `math_dollars`). Delimiters live in
    /// `source_range` / `raw`; the visible formula is `literal`.
    Math {
        /// Inner TeX (comrak literal; inline math is code-normalized).
        literal: String,
        /// `true` for `$$…$$` display math.
        display: bool,
        /// Full `$…$` / `$$…$$` source slice (byte-exact for dirty serialize).
        raw: Box<str>,
        /// Full span including the dollar delimiters.
        source_range: Range<usize>,
        /// Emphasis context the fragment sits inside.
        marks: MarkSet,
    },
    /// Inline HTML, footnote refs, wikilinks, ...: verbatim.
    OpaqueInline {
        raw: Box<str>,
        source_range: Range<usize>,
        /// Emphasis context the fragment sits inside.
        marks: MarkSet,
    },
}

impl Inline {
    /// Visible text length contribution (for caret math in later phases).
    pub fn text_len(&self) -> usize {
        match self {
            Inline::Run { text, .. } => text.len(),
            Inline::Image { alt, .. } => alt.len(),
            Inline::SoftBreak | Inline::HardBreak { .. } => 1,
            Inline::Math { literal, .. } => literal.len(),
            Inline::OpaqueInline { raw, .. } => raw.len(),
        }
    }
}

/// `$` vs `$$` delimiter width.
pub fn math_delim_width(display: bool) -> usize {
    if display {
        2
    } else {
        1
    }
}
