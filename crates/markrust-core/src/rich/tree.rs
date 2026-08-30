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
#[derive(Debug, Clone, PartialEq)]
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
    /// Anything we do not model (HTML blocks, footnote definitions, math,
    /// ...): inert, serialized as its raw source slice, byte for byte.
    Opaque,
}

/// Inline marks as a small bitset (avoids a bitflags dependency).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MarkSet(u8);

impl MarkSet {
    pub const BOLD: MarkSet = MarkSet(1 << 0);
    pub const ITALIC: MarkSet = MarkSet(1 << 1);
    pub const STRIKE: MarkSet = MarkSet(1 << 2);
    pub const CODE: MarkSet = MarkSet(1 << 3);

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
}

impl Default for MarkFidelity {
    fn default() -> Self {
        MarkFidelity {
            emph_delim: b'*',
            strong_delim: b'*',
            code_backticks: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinkAttrs {
    pub url: String,
    pub title: Option<String>,
    /// True when the source had no `](...)` form (autolink / bare URL).
    pub autolink: bool,
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
    },
    SoftBreak,
    HardBreak {
        style: BreakStyle,
    },
    /// Inline HTML, footnote refs, math, wikilinks, ...: verbatim.
    OpaqueInline {
        raw: Box<str>,
        source_range: Range<usize>,
    },
}

impl Inline {
    /// Visible text length contribution (for caret math in later phases).
    pub fn text_len(&self) -> usize {
        match self {
            Inline::Run { text, .. } => text.len(),
            Inline::Image { alt, .. } => alt.len(),
            Inline::SoftBreak | Inline::HardBreak { .. } => 1,
            Inline::OpaqueInline { raw, .. } => raw.len(),
        }
    }
}
