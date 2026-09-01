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
    /// Source byte length at import.
    pub source_len: usize,
    /// Comrak-less blank after the last block when the file ends with a
    /// blank line (`hello\n\n`). `None` when the last line has content or
    /// only a terminator `\n`. Extra unused newlines share this one range.
    pub trailing_blank: Option<Range<usize>>,
    /// Empty quoted / list lines (`> `, `- `, `1. `, `- [ ] `) that have no
    /// descendant inlines. Caret/click sit after the prefix so typing is
    /// `> x` / `- x`, not chrome.
    pub empty_prefix_homes: Vec<PrefixBlank>,
}

/// One empty quote/list line: the source line (no `\n`) and the body offset
/// after `>` / `- ` / `1. ` / task checkbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixBlank {
    pub line: Range<usize>,
    pub home: usize,
}

impl PrefixBlank {
    pub fn contains(&self, byte: usize) -> bool {
        byte >= self.line.start && byte <= self.home.max(self.line.end)
    }
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
                | BlockKind::Alert { .. }
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

    /// Editable inner range of a fenced code block (between the opening and
    /// closing fence lines). Indented code and non-code blocks return the
    /// full `source_range`. Empty fences collapse to the byte after the
    /// opening newline so the caret can sit in the body.
    pub fn code_body_range(&self, source: &str) -> Range<usize> {
        let BlockKind::CodeBlock { fence: Some(_), .. } = &self.kind else {
            return self.source_range.clone();
        };
        let start = self.source_range.start;
        let end = self.source_range.end.min(source.len());
        if start >= end {
            return start..start;
        }
        let slice = &source[start..end];
        let body_start = match slice.find('\n') {
            Some(i) => start + i + 1,
            None => start,
        };
        let body_end = match slice.rfind('\n') {
            Some(i) => start + i,
            None => end,
        };
        if body_start > body_end {
            body_start..body_start
        } else {
            body_start..body_end
        }
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
    /// GitHub `> [!NOTE]` / TIP / IMPORTANT / WARNING / CAUTION.
    /// Children are the body; `[!NOTE]` lives in `tag_range` / `chrome_range`.
    Alert {
        kind: AlertKind,
        /// Custom title after `[!NOTE]`, if any.
        title: Option<String>,
        /// Byte range of `[!NOTE]` (or `[!TIP]`, …) in source.
        tag_range: Range<usize>,
        /// `[!NOTE]` plus an optional custom title on that first line.
        chrome_range: Range<usize>,
    },
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
    /// `[TOC]` / `[[toc]]` placeholder. WYSIWYG paints the heading outline;
    /// source bytes stay the marker.
    Toc {
        /// `true` when written as `[[toc]]`.
        wiki: bool,
    },
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
    SoftBreak {
        /// The source newline (markdown wrap). WYSIWYG paints a space.
        source_range: Range<usize>,
    },
    HardBreak {
        style: BreakStyle,
        /// Two-space / backslash marker plus the newline.
        source_range: Range<usize>,
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
    /// `[[target]]` / `[[target|label]]` (comrak wikilinks, Typora pipe-after).
    /// `[[` / `]]` (and `target|` when a label is present) are chrome.
    WikiLink {
        /// Destination (left of `|` in Typora `[[target|label]]`).
        target: String,
        /// Visible label (right of `|`, or the target when unpiped).
        label: String,
        /// Full `[[…]]` source slice (byte-exact for dirty serialize).
        raw: Box<str>,
        /// Full span including `[[` / `]]`.
        source_range: Range<usize>,
        /// Emphasis context the fragment sits inside.
        marks: MarkSet,
    },
    /// GitHub/Typora `:name:` shortcode. WYSIWYG paints `glyph` unless the
    /// caret intersects; source bytes stay `:name:`. Unknown names stay
    /// [`Inline::Run`].
    Emoji {
        /// Alias without colons (`smile`).
        name: String,
        /// Unicode to paint when the caret is outside the span.
        glyph: String,
        /// Full `:name:` source slice (byte-exact for dirty serialize).
        raw: Box<str>,
        /// Full span including both colons.
        source_range: Range<usize>,
        /// Emphasis context the fragment sits inside.
        marks: MarkSet,
        /// Enclosing link, when the shortcode is link text.
        link: Option<LinkAttrs>,
        fidelity: MarkFidelity,
    },
    /// Inline HTML, footnote refs, leftover unknowns: verbatim.
    OpaqueInline {
        raw: Box<str>,
        source_range: Range<usize>,
        /// Emphasis context the fragment sits inside.
        marks: MarkSet,
    },
}

impl Inline {
    /// Source bytes this inline occupies (advisory for breaks; exclusive end).
    pub fn source_range(&self) -> Range<usize> {
        match self {
            Inline::Run { source_range, .. }
            | Inline::Image { source_range, .. }
            | Inline::Math { source_range, .. }
            | Inline::WikiLink { source_range, .. }
            | Inline::Emoji { source_range, .. }
            | Inline::OpaqueInline { source_range, .. }
            | Inline::SoftBreak { source_range }
            | Inline::HardBreak { source_range, .. } => source_range.clone(),
        }
    }

    /// Visible text length contribution (for caret math in later phases).
    pub fn text_len(&self) -> usize {
        match self {
            Inline::Run { text, .. } => text.len(),
            Inline::Image { alt, .. } => alt.len(),
            Inline::SoftBreak { .. } | Inline::HardBreak { .. } => 1,
            Inline::Math { literal, .. } => literal.len(),
            Inline::WikiLink { label, .. } => label.len(),
            Inline::Emoji { glyph, .. } => glyph.len(),
            Inline::OpaqueInline { raw, .. } => raw.len(),
        }
    }
}

impl RichTree {
    /// Headings as `(source offset, level, plain text)`.
    pub fn outline(&self) -> Vec<(usize, u8, String)> {
        fn walk(blocks: &[Block], out: &mut Vec<(usize, u8, String)>) {
            for b in blocks {
                if let BlockKind::Heading { level, .. } = b.kind {
                    out.push((b.source_range.start, level, heading_plain_text(&b.inlines)));
                }
                walk(&b.children, out);
            }
        }
        let mut out = Vec::new();
        walk(&self.blocks, &mut out);
        out
    }
}

/// Trailing empty line(s) at EOF. A lone terminator `\n` after the last
/// content line is not a blank; `hello\n\n` / extra `\n`s are.
pub(crate) fn trailing_blank_gap(source: &str) -> Option<Range<usize>> {
    let bytes = source.as_bytes();
    if bytes.last().is_none_or(|b| *b != b'\n') {
        return None;
    }
    let last_nl = source.len() - 1;
    let last_line_start = source[..last_nl].rfind('\n').map(|i| i + 1).unwrap_or(0);
    if !source[last_line_start..last_nl].trim().is_empty() {
        return None;
    }
    let mut start = last_line_start;
    while start > 0 && bytes[start - 1] == b'\n' {
        let prev_start = source[..start - 1].rfind('\n').map(|i| i + 1).unwrap_or(0);
        if !source[prev_start..start - 1].trim().is_empty() {
            break;
        }
        start = prev_start;
    }
    Some(start..source.len())
}

fn heading_plain_text(inlines: &[Inline]) -> String {
    let mut text = String::new();
    for inline in inlines {
        match inline {
            Inline::Run { text: t, .. } => text.push_str(t),
            Inline::WikiLink { label, .. } => text.push_str(label),
            Inline::Math { literal, .. } => text.push_str(literal),
            Inline::Emoji { glyph, .. } => text.push_str(glyph),
            _ => {}
        }
    }
    text
}

/// True when a paragraph's source is Typora's TOC marker (`[TOC]` / `[[toc]]`).
pub fn is_toc_marker(source: &str) -> bool {
    let t = source.trim();
    t.eq_ignore_ascii_case("[toc]") || t.eq_ignore_ascii_case("[[toc]]")
}

/// Source range of the visible wiki label (`label` after `|`, else the target).
pub fn wiki_visible_range(raw: &str, source_range: Range<usize>) -> Range<usize> {
    if raw.len() != source_range.len()
        || raw.len() < 4
        || !raw.starts_with("[[")
        || !raw.ends_with("]]")
    {
        return source_range;
    }
    let inner = &raw[2..raw.len() - 2];
    if let Some(pipe) = inner.find('|') {
        let rel = 2 + pipe + 1;
        source_range.start + rel
            ..source_range
                .end
                .saturating_sub(2)
                .max(source_range.start + rel)
    } else {
        source_range.start + 2
            ..source_range
                .end
                .saturating_sub(2)
                .max(source_range.start + 2)
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

/// GitHub Flavored Markdown alert kinds (`> [!NOTE]`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl AlertKind {
    pub const ALL: [AlertKind; 5] = [
        AlertKind::Note,
        AlertKind::Tip,
        AlertKind::Important,
        AlertKind::Warning,
        AlertKind::Caution,
    ];

    /// Uppercase tag written in source (`NOTE`, `TIP`, …).
    pub fn tag(self) -> &'static str {
        match self {
            AlertKind::Note => "NOTE",
            AlertKind::Tip => "TIP",
            AlertKind::Important => "IMPORTANT",
            AlertKind::Warning => "WARNING",
            AlertKind::Caution => "CAUTION",
        }
    }

    /// GitHub callout title (`Note`, `Tip`, …).
    pub fn label(self) -> &'static str {
        match self {
            AlertKind::Note => "Note",
            AlertKind::Tip => "Tip",
            AlertKind::Important => "Important",
            AlertKind::Warning => "Warning",
            AlertKind::Caution => "Caution",
        }
    }

    pub fn from_tag(s: &str) -> Option<Self> {
        if s.eq_ignore_ascii_case("NOTE") {
            Some(AlertKind::Note)
        } else if s.eq_ignore_ascii_case("TIP") {
            Some(AlertKind::Tip)
        } else if s.eq_ignore_ascii_case("IMPORTANT") {
            Some(AlertKind::Important)
        } else if s.eq_ignore_ascii_case("WARNING") {
            Some(AlertKind::Warning)
        } else if s.eq_ignore_ascii_case("CAUTION") {
            Some(AlertKind::Caution)
        } else {
            None
        }
    }

    /// Painted label: custom title if present, otherwise the kind name.
    pub fn callout_label(self, title: Option<&str>) -> String {
        title
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.label().to_string())
    }
}

/// `[!NOTE]` (etc.) inside an alert block, plus the rest of that first-line chrome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertChrome {
    pub kind: AlertKind,
    pub tag_range: Range<usize>,
    pub chrome_range: Range<usize>,
}

/// Locate `[!NOTE]` / `[!TIP]` / … on the first line of `block_range`.
pub fn find_alert_chrome(source: &str, block_range: Range<usize>) -> Option<AlertChrome> {
    let slice = source.get(block_range.clone())?;
    let line = slice.split('\n').next()?;
    let rel = line.find("[!")?;
    let after = &line[rel + 2..];
    let close = after.find(']')?;
    let kind = AlertKind::from_tag(&after[..close])?;
    let tag_start = block_range.start + rel;
    let tag_end = tag_start + 2 + close + 1;
    let trimmed = line.trim_end();
    let line_end = block_range.start + trimmed.len();
    Some(AlertChrome {
        kind,
        tag_range: tag_start..tag_end,
        chrome_range: tag_start..line_end.max(tag_end),
    })
}
