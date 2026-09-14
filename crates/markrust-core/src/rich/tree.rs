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

/// One empty quote/list/footnote-def/details line: the source line (no `\n`)
/// and the body offset after `>` / `- ` / `1. ` / task checkbox / `[^1]: ` /
/// `: `.
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
    /// Raw text including the `---` / `...` delimiters and trailing newline.
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

    /// Editable inner range of a code block. Fenced: between the opening and
    /// closing fence lines. Indented: after the opening 4 spaces / tab (dest
    /// chrome like fence ticks). Non-code blocks return the full
    /// `source_range`. Empty fences collapse to the byte after the opening
    /// newline so the caret can sit in the body.
    pub fn code_body_range(&self, source: &str) -> Range<usize> {
        match &self.kind {
            BlockKind::CodeBlock { fence: None, .. } => {
                indented_code_body_range(source, &self.source_range)
            }
            BlockKind::CodeBlock { fence: Some(_), .. } => {
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
            _ => self.source_range.clone(),
        }
    }
}

/// CommonMark indented-code opener at the start of `slice`: one tab or up
/// to four spaces. Additional indent is content.
pub(crate) fn indented_code_indent_len(slice: &str) -> usize {
    let bytes = slice.as_bytes();
    if bytes.first() == Some(&b'\t') {
        return 1;
    }
    bytes.iter().take(4).take_while(|&&b| b == b' ').count()
}

fn indented_code_body_range(source: &str, span: &Range<usize>) -> Range<usize> {
    let start = span.start.min(source.len());
    let end = span.end.min(source.len()).max(start);
    if start >= end {
        return start..start;
    }
    let indent = indented_code_indent_len(&source[start..end]);
    start + indent..end
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
    /// GFM `[label]: dest` / `[label]: dest "title"` link or image reference
    /// definition. Comrak detaches these from the AST; import recovers them
    /// as editable WYSIWYG leaves so Typora-style `[ref]: url` lines stay
    /// visible and `[label][ref]` still resolves.
    LinkReferenceDefinition {
        label: String,
        url: String,
        title: Option<String>,
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
    /// True when the destination was written as `<…>` (CommonMark autolink).
    /// Bare GFM autolinks (`https://…` with no brackets) stay `false`.
    pub angle: bool,
    /// Identity of the originating link node: adjacent links with identical
    /// attrs must not merge into one span on serialization.
    pub group: u64,
}

impl LinkAttrs {
    /// Source span of wrapping `<>` around an inner URL/email run.
    pub fn angle_span(&self, inner: &Range<usize>) -> Option<Range<usize>> {
        if !self.angle || inner.start == 0 {
            return None;
        }
        Some(inner.start - 1..inner.end + 1)
    }

    /// `[…](url)` / `[…][ref]` / autolink `<>` around `inner`.
    pub fn outer_span(&self, source: &str, inner: Range<usize>) -> Range<usize> {
        let auto = expand_around_autolink(source, inner.clone());
        if self.autolink || auto != inner {
            auto
        } else {
            expand_around_markdown_link(source, inner)
        }
    }
}

/// Opening `[` / `![`, closing `]`, and dest `(url)` / `[ref]` around a
/// markdown link or image. Autolink `<>` is not this shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownLinkChrome {
    pub outer: Range<usize>,
    pub open: Range<usize>,
    pub close: Range<usize>,
    pub dest: Range<usize>,
}

fn is_mark_delim(b: u8) -> bool {
    matches!(b, b'*' | b'_' | b'`' | b'~' | b'=' | b'^')
}

/// Grow `inner` across adjacent `*` / `_` / ticks / `~` / `=` / `^` so a
/// marked label `[**bold**](url)` still finds `[` / dest.
pub fn grow_mark_delimiters(source: &str, range: Range<usize>) -> Range<usize> {
    expand_mark_delimiters_bounded(source, range, 0, source.len())
}

/// [`grow_mark_delimiters`] clamped to `lo..hi` (a leaf block's source span).
pub fn expand_mark_delimiters_bounded(
    source: &str,
    mut range: Range<usize>,
    lo: usize,
    hi: usize,
) -> Range<usize> {
    let lo = lo.min(source.len());
    let hi = hi.min(source.len()).max(lo);
    range.start = range.start.clamp(lo, hi);
    range.end = range.end.clamp(lo, hi).max(range.start);
    let bytes = source.as_bytes();
    while range.start > lo && is_mark_delim(bytes[range.start - 1]) {
        range.start -= 1;
    }
    while range.end < hi && is_mark_delim(bytes[range.end]) {
        range.end += 1;
    }
    range
}

/// Grow wrapping HTML phrasing tags (`<b>` / `</b>` / `<a href>` / comments)
/// around `inner` until stable. Nested `<b><i>x</i></b>` needs more than one
/// pass. Autolink `<>` is not a tag (`opaque_inline_is_caret_chrome` is false).
pub fn expand_around_html_phrasing(
    source: &str,
    mut range: Range<usize>,
    lo: usize,
    hi: usize,
) -> Range<usize> {
    let lo = lo.min(source.len());
    let hi = hi.min(source.len()).max(lo);
    range.start = range.start.clamp(lo, hi);
    range.end = range.end.clamp(lo, hi).max(range.start);
    for _ in 0..8 {
        let mut next = range.clone();
        if let Some(start) = html_phrasing_piece_ending_at(source, next.start, lo) {
            next.start = start;
        }
        if let Some(end) = html_phrasing_piece_starting_at(source, next.end, hi) {
            next.end = end;
        }
        if next == range {
            return next;
        }
        range = next;
    }
    range
}

const HTML_PHRASING_SCAN: usize = 2048;

fn html_phrasing_piece_ending_at(source: &str, end: usize, lo: usize) -> Option<usize> {
    if end == 0 || end > source.len() || source.as_bytes().get(end - 1) != Some(&b'>') {
        return None;
    }
    let bytes = source.as_bytes();
    let mut start = end;
    while start > lo {
        start -= 1;
        if end.saturating_sub(start) > HTML_PHRASING_SCAN {
            break;
        }
        if bytes[start] != b'<' {
            continue;
        }
        let slice = source.get(start..end)?;
        if crate::html_visual::opaque_inline_is_caret_chrome(slice) {
            return Some(start);
        }
    }
    None
}

fn html_phrasing_piece_starting_at(source: &str, start: usize, hi: usize) -> Option<usize> {
    if start >= hi || source.as_bytes().get(start) != Some(&b'<') {
        return None;
    }
    let bytes = source.as_bytes();
    let mut end = start + 1;
    while end <= hi {
        if end.saturating_sub(start) > HTML_PHRASING_SCAN {
            break;
        }
        if bytes.get(end - 1) == Some(&b'>') {
            if let Some(slice) = source.get(start..end) {
                if crate::html_visual::opaque_inline_is_caret_chrome(slice) {
                    return Some(end);
                }
            }
        }
        end += 1;
    }
    None
}

/// Grow wrapping `[…](url)` / autolink `<>` and HTML phrasing until stable.
/// Does not include wrap marks — paint finds `*` / `**` adjacent to this span.
pub fn expand_link_and_html_chrome(
    source: &str,
    inner: Range<usize>,
    link: Option<&LinkAttrs>,
    lo: usize,
    hi: usize,
) -> Range<usize> {
    let lo = lo.min(source.len());
    let hi = hi.min(source.len()).max(lo);
    let start = inner.start.clamp(lo, hi);
    let mut outer = start..inner.end.clamp(lo, hi).max(start);
    for _ in 0..8 {
        let mut next = expand_around_html_phrasing(source, outer.clone(), lo, hi);
        if let Some(link) = link {
            next = link.outer_span(source, next);
            next.start = next.start.max(lo);
            next.end = next.end.min(hi);
        }
        if next == outer {
            return outer;
        }
        outer = next;
    }
    outer
}

/// Grow wrap marks, wrapping `[…](url)` / autolink `<>`, and HTML phrasing
/// until stable.
///
/// One pass is not enough: `[**hello**](url)` needs marks then dest, while
/// `**[hello](url)**` / `**<b>hello</b>**` need dest or tags then marks.
/// Keyboard skip and intersect-reveal share this outer so wrapping `*` is
/// dest chrome, not a caret home.
pub fn expand_marks_and_link_chrome(
    source: &str,
    inner: Range<usize>,
    link: Option<&LinkAttrs>,
    lo: usize,
    hi: usize,
) -> Range<usize> {
    let lo = lo.min(source.len());
    let hi = hi.min(source.len()).max(lo);
    let start = inner.start.clamp(lo, hi);
    let mut outer = start..inner.end.clamp(lo, hi).max(start);
    for _ in 0..8 {
        let mut next = expand_mark_delimiters_bounded(source, outer.clone(), lo, hi);
        next = expand_link_and_html_chrome(source, next, link, lo, hi);
        if next == outer {
            return outer;
        }
        outer = next;
    }
    outer
}

/// `[label](url)` / `[label][ref]` / collapsed `[label][]` around `inner`.
pub fn expand_around_markdown_link(source: &str, mut range: Range<usize>) -> Range<usize> {
    let bytes = source.as_bytes();
    if range.start > 0 && bytes[range.start - 1] == b'[' {
        range.start -= 1;
    }
    if range.start > 0 && bytes[range.start - 1] == b'!' && bytes.get(range.start) == Some(&b'[') {
        range.start -= 1;
    }
    if range.end < bytes.len() && bytes[range.end] == b']' {
        range.end += 1;
        if range.end < bytes.len() && bytes[range.end] == b'(' {
            range.end += 1;
            let mut depth = 1i32;
            while range.end < bytes.len() && depth > 0 {
                match bytes[range.end] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                range.end += 1;
            }
        } else if range.end < bytes.len() && bytes[range.end] == b'[' {
            let open = range.end;
            range.end += 1;
            while range.end < bytes.len() && bytes[range.end] != b']' {
                range.end += 1;
            }
            if range.end < bytes.len() && bytes[range.end] == b']' {
                range.end += 1;
            } else {
                range.end = open;
            }
        }
    }
    range
}

fn expand_around_autolink(source: &str, mut range: Range<usize>) -> Range<usize> {
    let bytes = source.as_bytes();
    if range.start > 0
        && bytes[range.start - 1] == b'<'
        && range.end < bytes.len()
        && bytes[range.end] == b'>'
    {
        range.start -= 1;
        range.end += 1;
    }
    range
}

/// Split `[label](url)` / `![alt](url)` / `[label][ref]` into opener, closer,
/// and dest. `span` must already include those bytes.
pub fn markdown_link_chrome(source: &str, span: Range<usize>) -> Option<MarkdownLinkChrome> {
    let slice = source.get(span.clone())?;
    if slice.starts_with('<') && slice.ends_with('>') {
        return None;
    }
    let open_len = if slice.starts_with("![") {
        2
    } else if slice.starts_with('[') {
        1
    } else {
        0
    };
    if open_len == 0 {
        return None;
    }
    let rest = &slice[open_len..];
    let bytes = rest.as_bytes();
    let mut close_rel = None;
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b']' && matches!(bytes[i + 1], b'(' | b'[') {
            close_rel = Some(i);
        }
        i += 1;
    }
    let close_rel = close_rel.or_else(|| rest.rfind(']'))?;
    let close_at = span.start + open_len + close_rel;
    if close_at >= span.end || source.as_bytes().get(close_at) != Some(&b']') {
        return None;
    }
    let dest = close_at + 1..span.end;
    let open = span.start..span.start + open_len;
    Some(MarkdownLinkChrome {
        outer: span,
        open,
        close: close_at..close_at + 1,
        dest,
    })
}

/// Inner URL and title runs inside markdown dest `(url "title")` / `[ref]`.
/// Wrapping `(` `"` `'` `)` / dest `[` `]` are dest chrome, not caret homes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownLinkDestParts {
    pub outer: Range<usize>,
    pub url: Range<usize>,
    pub title: Option<Range<usize>>,
}

impl MarkdownLinkDestParts {
    pub fn inners(&self) -> impl Iterator<Item = Range<usize>> {
        std::iter::once(self.url.clone()).chain(self.title.clone())
    }

    /// Snap dest wrapping `(`, `"`, `'`, `)` onto the URL or title inner.
    pub fn snap(&self, byte: usize) -> Option<usize> {
        if byte < self.outer.start || byte >= self.outer.end {
            return None;
        }
        if byte >= self.url.start && byte < self.url.end {
            return Some(byte);
        }
        if let Some(title) = &self.title {
            if byte >= title.start && byte < title.end {
                return Some(byte);
            }
            if byte > self.url.end {
                return Some(title.start);
            }
        }
        Some(self.url.start)
    }
}

/// Parse dest `(url)`, `(url "title")` / `'title'` / `(title)`, `<url>`, or
/// `[ref]` into inner caret homes. `dest` is [`MarkdownLinkChrome::dest`].
pub fn markdown_link_dest_parts(source: &str, dest: Range<usize>) -> Option<MarkdownLinkDestParts> {
    let bytes = source.as_bytes();
    let start = dest.start.min(source.len());
    let end = dest.end.min(source.len()).max(start);
    if start >= end {
        return None;
    }
    match bytes[start] {
        b'[' => {
            let close = source.get(start + 1..end)?.find(']')?;
            let url = start + 1..start + 1 + close;
            if url.start >= url.end {
                return None;
            }
            Some(MarkdownLinkDestParts {
                outer: start..start + 1 + close + 1,
                url,
                title: None,
            })
        }
        b'(' => {
            let limit = end;
            let mut i = start + 1;
            i = skip_dest_space(source, i, limit);
            let (url, after) = scan_inline_dest_url(source, i, limit)?;
            i = skip_dest_space(source, after, limit);
            let title = scan_inline_dest_title(source, i, limit);
            Some(MarkdownLinkDestParts {
                outer: dest,
                url,
                title,
            })
        }
        _ => None,
    }
}

fn skip_dest_space(source: &str, mut i: usize, limit: usize) -> usize {
    let bytes = source.as_bytes();
    while i < limit && matches!(bytes[i], b' ' | b'\t' | b'\n') {
        i += 1;
    }
    i
}

fn scan_inline_dest_url(source: &str, start: usize, limit: usize) -> Option<(Range<usize>, usize)> {
    let bytes = source.as_bytes();
    if start >= limit {
        return None;
    }
    if bytes[start] == b'<' {
        let mut i = start + 1;
        while i < limit {
            match bytes[i] {
                b'>' => {
                    let inner = start + 1..i;
                    if inner.start >= inner.end {
                        return None;
                    }
                    return Some((inner, i + 1));
                }
                b'\n' | b'<' => return None,
                b'\\' if i + 1 < limit => i += 2,
                _ => i += 1,
            }
        }
        return None;
    }
    let mut i = start;
    let mut parens = 0i32;
    while i < limit {
        let b = bytes[i];
        if b == b'\\' && i + 1 < limit {
            i += 2;
            continue;
        }
        if b == b'(' {
            parens += 1;
            i += 1;
            continue;
        }
        if b == b')' {
            if parens == 0 {
                break;
            }
            parens -= 1;
            i += 1;
            continue;
        }
        if b == b' ' || b == b'\t' || b == b'\n' || b.is_ascii_control() {
            break;
        }
        i += 1;
    }
    if i == start || parens != 0 {
        return None;
    }
    Some((start..i, i))
}

fn scan_inline_dest_title(source: &str, start: usize, limit: usize) -> Option<Range<usize>> {
    let bytes = source.as_bytes();
    let closer = match bytes.get(start).copied() {
        Some(b'"') => b'"',
        Some(b'\'') => b'\'',
        Some(b'(') => b')',
        _ => return None,
    };
    let mut i = start + 1;
    while i < limit {
        let b = bytes[i];
        if b == b'\\' && i + 1 < limit {
            i += 2;
            continue;
        }
        if b == closer {
            let inner = start + 1..i;
            if inner.start >= inner.end {
                return None;
            }
            return Some(inner);
        }
        i += 1;
    }
    None
}

/// Quoted title inner (`"title"` / `'title'` / `(title)`), if `range` is a
/// complete quoted title run (GFM `[ref]: dest "title"`).
pub fn quoted_title_inner(source: &str, range: Range<usize>) -> Option<Range<usize>> {
    let slice = source.get(range.clone())?;
    let closer = match slice.as_bytes().first().copied() {
        Some(b'"') => b'"',
        Some(b'\'') => b'\'',
        Some(b'(') => b')',
        _ => return None,
    };
    if slice.len() < 2 || *slice.as_bytes().last()? != closer {
        return None;
    }
    let inner = range.start + 1..range.end - 1;
    (inner.start < inner.end).then_some(inner)
}

/// Dest URL / title inners for markdown links and images in `block` (not
/// descendants).
pub fn markdown_link_dests(source: &str, block: &Block) -> Vec<MarkdownLinkDestParts> {
    let mut out = Vec::new();
    collect_markdown_link_dests(source, block, &mut out);
    out
}

fn collect_markdown_link_dests(source: &str, block: &Block, out: &mut Vec<MarkdownLinkDestParts>) {
    let lo = block.source_range.start;
    let hi = block.source_range.end.min(source.len());
    for inline in &block.inlines {
        match inline {
            Inline::Run {
                source_range,
                link: Some(link),
                ..
            }
            | Inline::Emoji {
                source_range,
                link: Some(link),
                ..
            } if !link.autolink => {
                let outer =
                    expand_link_and_html_chrome(source, source_range.clone(), Some(link), lo, hi);
                push_dest_parts(source, outer, out);
            }
            Inline::Image {
                source_range, link, ..
            } => {
                push_dest_parts(source, source_range.clone(), out);
                if let Some(link) = link.as_ref() {
                    if !link.autolink {
                        let outer = expand_link_and_html_chrome(
                            source,
                            source_range.clone(),
                            Some(link),
                            lo,
                            hi,
                        );
                        push_dest_parts(source, outer, out);
                    }
                }
            }
            _ => {}
        }
    }
}

fn push_dest_parts(source: &str, span: Range<usize>, out: &mut Vec<MarkdownLinkDestParts>) {
    let Some(chrome) = markdown_link_chrome(source, span) else {
        return;
    };
    let Some(parts) = markdown_link_dest_parts(source, chrome.dest) else {
        return;
    };
    if out.iter().any(|p| p.outer == parts.outer) {
        return;
    }
    out.push(parts);
}

/// `[` / `]` / `: ` around a recovered GFM `[label]: dest` definition line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkReferenceDefChrome {
    pub outer: Range<usize>,
    pub open: Range<usize>,
    pub close: Range<usize>,
    pub colon: Range<usize>,
    pub label: Range<usize>,
    pub dest: Range<usize>,
}

/// Chrome for a [`BlockKind::LinkReferenceDefinition`] leaf, derived from
/// its label/dest runs and the surrounding `[` / `]:` bytes.
pub fn link_reference_def_chrome(source: &str, block: &Block) -> Option<LinkReferenceDefChrome> {
    if !matches!(block.kind, BlockKind::LinkReferenceDefinition { .. }) {
        return None;
    }
    let mut label = None;
    let mut dest_lo = None;
    let mut dest_hi = None;
    for inline in &block.inlines {
        if let Inline::Run {
            source_range, link, ..
        } = inline
        {
            if link.is_some() {
                dest_lo =
                    Some(dest_lo.map_or(source_range.start, |s: usize| s.min(source_range.start)));
                dest_hi =
                    Some(dest_hi.map_or(source_range.end, |e: usize| e.max(source_range.end)));
            } else if label.is_none() {
                label = Some(source_range.clone());
            }
        }
    }
    let label = label?;
    let dest = match (dest_lo, dest_hi) {
        (Some(lo), Some(hi)) => lo..hi,
        _ => block.inlines.iter().rev().find_map(|inline| match inline {
            Inline::Run { source_range, .. } if *source_range != label => {
                Some(source_range.clone())
            }
            _ => None,
        })?,
    };
    if label.start == 0 || source.as_bytes().get(label.start - 1) != Some(&b'[') {
        return None;
    }
    let open = label.start - 1..label.start;
    if source.as_bytes().get(label.end) != Some(&b']') {
        return None;
    }
    let close = label.end..label.end + 1;
    if source.as_bytes().get(close.end) != Some(&b':') {
        return None;
    }
    let mut colon_end = close.end + 1;
    while colon_end < source.len()
        && colon_end < dest.start
        && matches!(source.as_bytes()[colon_end], b' ' | b'\t')
    {
        colon_end += 1;
    }
    Some(LinkReferenceDefChrome {
        outer: block.source_range.clone(),
        open,
        close: close.clone(),
        colon: close.end..colon_end,
        label,
        dest,
    })
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

/// Source range of the visible TOC name (`TOC` / `toc`) inside `[TOC]` / `[[toc]]`.
/// Brackets (and surrounding whitespace) are dest chrome, not caret homes.
pub fn toc_visible_range(source: &str, source_range: Range<usize>) -> Range<usize> {
    let slice = match source.get(source_range.clone()) {
        Some(s) => s,
        None => return source_range,
    };
    let Some(rel) = slice.find(|c: char| !c.is_whitespace()) else {
        return source_range;
    };
    let trimmed = slice[rel..].trim_end();
    let wiki = trimmed.len() >= 4 && trimmed.starts_with("[[") && trimmed.ends_with("]]");
    let w = if wiki { 2 } else { 1 };
    if trimmed.len() < w * 2 {
        return source_range;
    }
    let start = source_range.start + rel + w;
    let end = source_range.start + rel + trimmed.len() - w;
    if start < end {
        start..end
    } else {
        source_range
    }
}

/// Custom title after `[!NOTE]` on the alert chrome line, if any.
/// The tag itself (and the space after `]`) is dest chrome.
pub fn alert_title_range(
    tag_range: &Range<usize>,
    chrome_range: &Range<usize>,
) -> Option<Range<usize>> {
    if chrome_range.end <= tag_range.end {
        return None;
    }
    let start = tag_range.end + 1;
    if start < chrome_range.end {
        Some(start..chrome_range.end)
    } else {
        Some(tag_range.end..chrome_range.end)
    }
}

/// CommonMark code spans strip one leading and trailing space from the
/// contents when both ends are a space and the span is not all spaces.
/// Those spaces stay in source (between the ticks and the painted run) and
/// are dest chrome: Home/click skip onto the painted content.
pub fn code_span_visible_range(
    source: &str,
    source_range: Range<usize>,
    text: &str,
) -> Range<usize> {
    let Some(slice) = source.get(source_range.clone()) else {
        return source_range;
    };
    if slice == text || text.is_empty() || slice.len() < text.len() + 2 {
        return source_range;
    }
    if slice.starts_with(' ')
        && slice.ends_with(' ')
        && !slice.bytes().all(|b| b == b' ')
        && &slice[1..slice.len() - 1] == text
    {
        return source_range.start + 1..source_range.end - 1;
    }
    source_range
}

/// Source range of the visible TeX inside `$…$` / `$$…$$`.
///
/// Display math often wraps the formula in newlines (`$$\nE=mc^2\n$$`).
/// Those wrapping `\n` / `\r`, and quote/list prefixes on those lines
/// (`> $$\n> E=mc^2\n> $$`), are dest chrome like `$` / `$$` — click/Home
/// land on the formula, not the blank after the opener.
pub fn math_visible_range(source: &str, display: bool, source_range: Range<usize>) -> Range<usize> {
    let w = math_delim_width(display);
    let lo = source_range.start.min(source.len());
    let hi = source_range.end.min(source.len()).max(lo);
    let mut start = lo.saturating_add(w).min(hi);
    let mut end = hi.saturating_sub(w).max(start);
    loop {
        let next = skip_math_leading_wrap(source, start, end);
        if next <= start {
            break;
        }
        start = next;
    }
    loop {
        let next = skip_math_trailing_wrap(source, start, end);
        if next >= end {
            break;
        }
        end = next;
    }
    start..end
}

fn skip_math_leading_wrap(source: &str, start: usize, end: usize) -> usize {
    if start >= end {
        return start;
    }
    let bytes = source.as_bytes();
    if matches!(bytes.get(start).copied(), Some(b'\n' | b'\r')) {
        return start + 1;
    }
    skip_math_line_prefix(source, start, end)
}

fn skip_math_trailing_wrap(source: &str, start: usize, end: usize) -> usize {
    if end <= start {
        return end;
    }
    let bytes = source.as_bytes();
    if matches!(bytes.get(end - 1).copied(), Some(b'\n' | b'\r')) {
        return end - 1;
    }
    let line_start = source[..end]
        .rfind(['\n', '\r'])
        .map(|i| i + 1)
        .unwrap_or(start);
    if line_start < start {
        return end;
    }
    let prefix_end = skip_math_line_prefix(source, line_start, end);
    if prefix_end == end && line_start > start {
        return line_start;
    }
    end
}

/// `>` / `> ` quote markers and leading indent at a line start, inside a
/// math span (quoted `> $$` / list-nested display math).
pub(crate) fn skip_math_line_prefix(source: &str, at: usize, end: usize) -> usize {
    if at >= end {
        return at;
    }
    let bytes = source.as_bytes();
    if at > 0 && !matches!(bytes.get(at - 1).copied(), Some(b'\n' | b'\r')) {
        return at;
    }
    let mut i = at;
    while i < end && bytes[i] == b'>' {
        i += 1;
        if i < end && bytes[i] == b' ' {
            i += 1;
        }
    }
    while i < end && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    i
}

/// Source range of the shortcode name (`smile` in `:smile:`).
pub fn emoji_visible_range(raw: &str, source_range: Range<usize>) -> Range<usize> {
    if raw.len() != source_range.len()
        || raw.len() < 2
        || !raw.starts_with(':')
        || !raw.ends_with(':')
    {
        return source_range;
    }
    source_range.start + 1
        ..source_range
            .end
            .saturating_sub(1)
            .max(source_range.start + 1)
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

#[cfg(test)]
mod tests {
    use super::math_visible_range;

    #[test]
    fn code_span_visible_range_skips_stripped_padding_spaces() {
        use super::code_span_visible_range;
        let source = "` foo `\n";
        let inner = source.find(" foo ").expect("inner");
        let vis = code_span_visible_range(source, inner..inner + 5, "foo");
        assert_eq!(&source[vis], "foo");

        let doubled = "``  foo`bar  ``\n";
        let inner = doubled.find("  foo`bar  ").expect("inner");
        let vis = code_span_visible_range(doubled, inner..inner + 11, " foo`bar ");
        assert_eq!(&doubled[vis], " foo`bar ");

        let one_side = "` foo`\n";
        let inner = one_side.find(" foo").expect("inner");
        let vis = code_span_visible_range(one_side, inner..inner + 4, " foo");
        assert_eq!(&one_side[vis], " foo");

        let only_spaces = "`   `\n";
        let inner = only_spaces.find("   ").expect("inner");
        let vis = code_span_visible_range(only_spaces, inner..inner + 3, "   ");
        assert_eq!(&only_spaces[vis], "   ");
    }

    #[test]
    fn math_visible_range_skips_wrapping_newlines_and_quote_prefixes() {
        let source = "$$\nE=mc^2\n$$";
        let vis = math_visible_range(source, true, 0..source.len());
        assert_eq!(&source[vis], "E=mc^2");

        let quoted = "> $$\n> E=mc^2\n> $$\n";
        let start = quoted.find("$$").expect("open");
        let end = quoted.rfind("$$").expect("close") + 2;
        let vis = math_visible_range(quoted, true, start..end);
        assert_eq!(&quoted[vis], "E=mc^2");

        let list = "- $$\n  E=mc^2\n  $$\n";
        let start = list.find("$$").expect("open");
        let end = list.rfind("$$").expect("close") + 2;
        let vis = math_visible_range(list, true, start..end);
        assert_eq!(&list[vis], "E=mc^2");

        let single = "see $$E=mc^2$$ here\n";
        let start = single.find("$$").expect("open");
        let end = start + "$$E=mc^2$$".len();
        let vis = math_visible_range(single, true, start..end);
        assert_eq!(&single[vis], "E=mc^2");

        let inline = "see $x^2$ here\n";
        let start = inline.find('$').expect("open");
        let end = inline.rfind('$').expect("close") + 1;
        let vis = math_visible_range(inline, false, start..end);
        assert_eq!(&inline[vis], "x^2");
    }
}
