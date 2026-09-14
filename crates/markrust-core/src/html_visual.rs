// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Safe visual projection of raw HTML and Typora extras (footnote refs,
//! leftover opaque blocks). Does not execute script, follow `javascript:`
//! URLs, or run a document stylesheet. Inline `style=` (and HTML `color`)
//! plus simple `.class` rules from a `<style>` in the **same** HTML block are
//! painted. Type-1 `<style>` / `<textarea>` blocks hide unless the caret
//! intersects (then source); Type-1 `<script>` stays hidden and skips as a
//! widget. GFM tagfilter Type-6 `<iframe>` / `<noframes>` (and Type-7
//! `<noembed>` when present) stay hidden widgets the same way; `<title>` /
//! `<xmp>` / `<plaintext>` reveal source on intersect like `<textarea>` but
//! still skip/delete as one widget. Type-6 `<details>` / `<dialog>` / `<form>`
//! and Type-7 `<video>` / `<audio>` / `<canvas>` / `<math>` are dest-chrome
//! the same way (skip inner / atomic delete / intersect-reveal of source —
//! not a player, form, or disclosure widget). Type-7 `<button>` / `<select>` /
//! `<input>` / `<label>` / `<output>` / `<progress>` / `<meter>` and Type-6
//! `<option>` / `<optgroup>` / `<fieldset>` / `<legend>` skip as dest-chrome
//! widgets the same way (reveal source on intersect — not a live form UI).
//! Type-7 `<noscript>` / `<template>` skip the same way (source on intersect —
//! not a nested document). `<datalist>` is not a dest-chrome widget (inner
//! `<option>` is). `<picture>` / `<summary>` / `<search>` / `<slot>` stay flow,
//! not dest-chrome. `<object>` / `<embed>` stay
//! hidden widgets like `<iframe>`. Safe `<svg>` (no script / `javascript:`) paints as an image via a
//! `data:image/svg+xml` URL. Simple phrasing tags become marks; everything
//! else is hidden chrome or inner text. Footnote definitions and definition
//! lists import as nested [`crate::rich::RichTree`] nodes; this module still
//! classifies `[^label]` refs and can project a leftover opaque blob.

use std::ops::Range;

/// sRGB color from inline `style=` / `color=` / a same-block CSS class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CssColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl CssColor {
    pub fn to_u32(self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }
}

/// Paint flags derived from open HTML tags (and layered on Markdown marks).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HtmlPaint {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
    pub code: bool,
    pub mark: bool,
    pub sup: bool,
    pub sub: bool,
    pub href: Option<String>,
    pub color: Option<CssColor>,
    pub background: Option<CssColor>,
}

/// One styled slice of projected HTML-block text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlPaintRun {
    pub len: usize,
    pub paint: HtmlPaint,
}

/// How an HTML block should look once tags are stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlBlockVisual {
    Hidden,
    ThematicBreak,
    Image {
        url: String,
        alt: String,
    },
    Flow {
        text: String,
        source_at: Vec<usize>,
        runs: Vec<HtmlPaintRun>,
    },
}

/// Result of applying one complete HtmlInline / opaque inline blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineHtmlAction {
    /// Tag, comment, or other chrome: do not paint, do not caret-walk.
    Hide,
    /// `<br>` / `<wbr>`.
    Break,
    /// `<img src>` or a safe complete `<svg>` for the image pipeline.
    Image { url: String, alt: String },
    /// `[^label]` footnote reference.
    FootnoteRef { label: String },
    /// Unknown opaque: show the source (leftover junk).
    Raw,
}

/// How WYSIWYG should treat an opaque inline when the caret intersects.
/// Widgets (`<br>` / `<img>` / SVG / `[^n]`) and dangerous tags stay skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlRevealKind {
    Open(String),
    Close(String),
    /// Comment, `<?…?>`, or a self-closing non-widget tag: one span of chrome.
    Solo,
    Skip,
}

/// Classify phrasing / comment chrome that paints when the caret intersects.
pub fn html_reveal_kind(raw: &str) -> HtmlRevealKind {
    if footnote_ref_label(raw).is_some() {
        return HtmlRevealKind::Skip;
    }
    if html_inline_svg(raw).is_some() {
        return HtmlRevealKind::Skip;
    }
    match parse_complete_piece(raw) {
        Some(HtmlPiece::Comment) | Some(HtmlPiece::OtherMarkup) => HtmlRevealKind::Solo,
        Some(HtmlPiece::Tag(tag)) => {
            if is_dangerous_tag(&tag.name) {
                return HtmlRevealKind::Skip;
            }
            if matches!(tag.name.as_str(), "br" | "wbr" | "img" | "hr") {
                return HtmlRevealKind::Skip;
            }
            if tag.close {
                HtmlRevealKind::Close(tag.name)
            } else if tag.self_closing || is_void_tag(&tag.name) {
                HtmlRevealKind::Solo
            } else {
                HtmlRevealKind::Open(tag.name)
            }
        }
        None => HtmlRevealKind::Skip,
    }
}

/// Inner source of an inline `<!-- … -->` / `<?…?>` / `<![CDATA[…]]>` widget.
///
/// `<!--` / `-->` (and PI / CDATA delimiters) are dest chrome. InsertText
/// sits in this inner; empty wrap wraps the whole widget like `<br>`.
pub fn html_solo_markup_inner_range(raw: &str, source_range: Range<usize>) -> Option<Range<usize>> {
    let t = raw.trim();
    let (opener, closer) = if t.starts_with("<!--") && t.ends_with("-->") && t.len() >= 7 {
        (4usize, 3usize)
    } else if t.starts_with("<![CDATA[") && t.ends_with("]]>") && t.len() >= 12 {
        (9, 3)
    } else if t.starts_with("<?") && t.ends_with("?>") && t.len() >= 4 {
        (2, 2)
    } else {
        return None;
    };
    let lead = raw.len() - raw.trim_start().len();
    let trail = raw.len() - raw.trim_end().len();
    let start = source_range.start.saturating_add(lead + opener);
    let end = source_range.end.saturating_sub(trail + closer).max(start);
    Some(start..end)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedTag {
    name: String,
    close: bool,
    self_closing: bool,
    attrs: Vec<(String, String)>,
}

impl ParsedTag {
    fn css_paint(&self, class_styles: &[(String, CssPaint)]) -> CssPaint {
        let mut paint = CssPaint::default();
        if let Some(class) = attr(&self.attrs, "class") {
            for name in class.split_whitespace() {
                let key = name.to_ascii_lowercase();
                if let Some((_, found)) = class_styles.iter().rev().find(|(n, _)| n == &key) {
                    paint.merge(found);
                }
            }
        }
        if let Some(style) = attr(&self.attrs, "style") {
            paint.merge(&parse_style_attr(style));
        }
        if paint.color.is_none() {
            if let Some(c) = attr(&self.attrs, "color").and_then(parse_css_color) {
                paint.color = Some(c);
            }
        }
        paint
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HtmlPiece {
    Tag(ParsedTag),
    Comment,
    OtherMarkup,
}

/// CSS declarations we honor from `style=` / same-block `.class` rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CssPaint {
    bold: bool,
    italic: bool,
    strike: bool,
    underline: bool,
    color: Option<CssColor>,
    background: Option<CssColor>,
}

impl CssPaint {
    fn merge(&mut self, other: &CssPaint) {
        self.bold |= other.bold;
        self.italic |= other.italic;
        self.strike |= other.strike;
        self.underline |= other.underline;
        if other.color.is_some() {
            self.color = other.color;
        }
        if other.background.is_some() {
            self.background = other.background;
        }
    }
}

#[derive(Debug, Default)]
struct StyleDelta {
    color: bool,
    background: bool,
    bold: bool,
    italic: bool,
    strike: bool,
    underline: bool,
}

/// Open-tag counters so `<b><b>x</b></b>` stays bold until the last close.
#[derive(Debug, Default)]
pub struct HtmlStack {
    bold: u8,
    italic: u8,
    strike: u8,
    underline: u8,
    code: u8,
    mark: u8,
    sup: u8,
    sub: u8,
    hide: u8,
    hrefs: Vec<String>,
    colors: Vec<CssColor>,
    backgrounds: Vec<CssColor>,
    style_deltas: Vec<StyleDelta>,
    class_styles: Vec<(String, CssPaint)>,
}

impl HtmlStack {
    fn with_class_styles(class_styles: Vec<(String, CssPaint)>) -> Self {
        Self {
            class_styles,
            ..Self::default()
        }
    }

    pub fn paint(&self) -> HtmlPaint {
        HtmlPaint {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            underline: self.underline > 0,
            code: self.code > 0,
            mark: self.mark > 0,
            sup: self.sup > 0,
            sub: self.sub > 0,
            href: self.hrefs.last().cloned(),
            color: self.colors.last().copied(),
            background: self.backgrounds.last().copied(),
        }
    }

    pub fn hidden(&self) -> bool {
        self.hide > 0
    }

    fn open(&mut self, tag: &ParsedTag) -> InlineHtmlAction {
        let name = tag.name.as_str();
        if is_dangerous_tag(name) {
            if !is_void_tag(name) && !tag.self_closing {
                self.hide = self.hide.saturating_add(1);
            }
            return InlineHtmlAction::Hide;
        }
        if name == "br" || name == "wbr" {
            return InlineHtmlAction::Break;
        }
        if name == "hr" {
            return InlineHtmlAction::Hide;
        }
        if name == "img" {
            if let Some((url, alt)) = img_from_attrs(&tag.attrs) {
                return InlineHtmlAction::Image { url, alt };
            }
            return InlineHtmlAction::Hide;
        }
        if tag.self_closing || is_void_tag(name) {
            return InlineHtmlAction::Hide;
        }
        bump(&mut self.bold, name, &["b", "strong"], true);
        bump(&mut self.italic, name, &["i", "em", "cite", "dfn"], true);
        bump(&mut self.strike, name, &["s", "del", "strike"], true);
        bump(&mut self.underline, name, &["u", "ins"], true);
        bump(
            &mut self.code,
            name,
            &["code", "kbd", "samp", "tt", "var"],
            true,
        );
        bump(&mut self.mark, name, &["mark"], true);
        bump(&mut self.sup, name, &["sup"], true);
        bump(&mut self.sub, name, &["sub", "small"], true);
        if name == "a" {
            if let Some(href) = attr(&tag.attrs, "href").and_then(safe_url) {
                self.hrefs.push(href);
            }
        }
        self.push_css(tag.css_paint(&self.class_styles));
        InlineHtmlAction::Hide
    }

    fn close(&mut self, name: &str) {
        if is_dangerous_tag(name) {
            self.hide = self.hide.saturating_sub(1);
            return;
        }
        bump(&mut self.bold, name, &["b", "strong"], false);
        bump(&mut self.italic, name, &["i", "em", "cite", "dfn"], false);
        bump(&mut self.strike, name, &["s", "del", "strike"], false);
        bump(&mut self.underline, name, &["u", "ins"], false);
        bump(
            &mut self.code,
            name,
            &["code", "kbd", "samp", "tt", "var"],
            false,
        );
        bump(&mut self.mark, name, &["mark"], false);
        bump(&mut self.sup, name, &["sup"], false);
        bump(&mut self.sub, name, &["sub", "small"], false);
        if name == "a" {
            self.hrefs.pop();
        }
        self.pop_css();
    }

    fn push_css(&mut self, css: CssPaint) {
        let mut delta = StyleDelta::default();
        if let Some(c) = css.color {
            self.colors.push(c);
            delta.color = true;
        }
        if let Some(c) = css.background {
            self.backgrounds.push(c);
            delta.background = true;
        }
        if css.bold {
            self.bold = self.bold.saturating_add(1);
            delta.bold = true;
        }
        if css.italic {
            self.italic = self.italic.saturating_add(1);
            delta.italic = true;
        }
        if css.strike {
            self.strike = self.strike.saturating_add(1);
            delta.strike = true;
        }
        if css.underline {
            self.underline = self.underline.saturating_add(1);
            delta.underline = true;
        }
        self.style_deltas.push(delta);
    }

    fn pop_css(&mut self) {
        let Some(delta) = self.style_deltas.pop() else {
            return;
        };
        if delta.color {
            self.colors.pop();
        }
        if delta.background {
            self.backgrounds.pop();
        }
        if delta.bold {
            self.bold = self.bold.saturating_sub(1);
        }
        if delta.italic {
            self.italic = self.italic.saturating_sub(1);
        }
        if delta.strike {
            self.strike = self.strike.saturating_sub(1);
        }
        if delta.underline {
            self.underline = self.underline.saturating_sub(1);
        }
    }
}

fn bump(slot: &mut u8, name: &str, names: &[&str], open: bool) {
    if !names.contains(&name) {
        return;
    }
    if open {
        *slot = slot.saturating_add(1);
    } else {
        *slot = slot.saturating_sub(1);
    }
}

/// Classify one opaque inline blob (a single comrak HtmlInline, or `[^n]`).
pub fn classify_opaque_inline(raw: &str, stack: &mut HtmlStack) -> InlineHtmlAction {
    if stack.hidden() {
        if let Some(HtmlPiece::Tag(tag)) = parse_complete_piece(raw) {
            if tag.close {
                stack.close(&tag.name);
            }
        }
        return InlineHtmlAction::Hide;
    }
    if let Some(label) = footnote_ref_label(raw) {
        return InlineHtmlAction::FootnoteRef {
            label: label.to_string(),
        };
    }
    if let Some((url, alt)) = html_inline_svg(raw) {
        return InlineHtmlAction::Image { url, alt };
    }
    match parse_complete_piece(raw) {
        Some(HtmlPiece::Comment) | Some(HtmlPiece::OtherMarkup) => InlineHtmlAction::Hide,
        Some(HtmlPiece::Tag(tag)) => {
            if tag.close {
                stack.close(&tag.name);
                InlineHtmlAction::Hide
            } else {
                stack.open(&tag)
            }
        }
        None => InlineHtmlAction::Raw,
    }
}

/// True when WYSIWYG caret should skip this opaque inline (hidden HTML chrome).
/// `<img>` / `<svg>` / `<br>` / `<wbr>` and `[^label]` are widgets, not skipped chrome.
pub fn opaque_inline_is_caret_chrome(raw: &str) -> bool {
    if footnote_ref_label(raw).is_some() {
        return false;
    }
    if html_inline_svg(raw).is_some() {
        return false;
    }
    match parse_complete_piece(raw) {
        Some(HtmlPiece::Tag(tag)) if tag.name == "img" || tag.name == "br" || tag.name == "wbr" => {
            false
        }
        Some(_) => true,
        None => false,
    }
}

/// Safe `<img src>` / `alt` from a complete html inline, or a safe `<svg>`.
pub fn html_inline_image(raw: &str) -> Option<(String, String)> {
    match parse_complete_piece(raw) {
        Some(HtmlPiece::Tag(tag)) if tag.name == "img" => return img_from_attrs(&tag.attrs),
        _ => {}
    }
    html_inline_svg(raw)
}

/// Complete safe `<svg>…</svg>` as a `data:image/svg+xml` URL plus alt.
pub fn html_inline_svg(raw: &str) -> Option<(String, String)> {
    let t = raw.trim();
    if !looks_like_svg_open(t) {
        return None;
    }
    let url = svg_to_data_url(t)?;
    Some((url, svg_alt(t)))
}

/// Complete `<br>` / `<wbr>` tag (inline or a line that is only that tag).
pub fn html_inline_break(raw: &str) -> bool {
    matches!(
        parse_complete_piece(raw),
        Some(HtmlPiece::Tag(tag)) if !tag.close && (tag.name == "br" || tag.name == "wbr")
    )
}

/// `[^label]` → `label`.
pub fn footnote_ref_label(raw: &str) -> Option<&str> {
    let t = raw.trim();
    let inner = t.strip_prefix("[^")?.strip_suffix(']')?;
    if inner.is_empty() || inner.contains(['[', ']']) {
        return None;
    }
    Some(inner)
}

/// Byte ranges of unmatched `[^label]` in a text run (comrak only emits a
/// FootnoteReference when a matching definition exists).
pub fn find_unmatched_footnote_refs(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' && bytes.get(i + 1) == Some(&b'^') && !is_escaped_at(bytes, i) {
            if let Some(end) = unmatched_footnote_ref_end(text, i) {
                out.push(i..end);
                i = end;
                continue;
            }
        }
        i += 1;
        while i < bytes.len() && (bytes[i] & 0b1100_0000) == 0b1000_0000 {
            i += 1;
        }
    }
    out
}

fn is_escaped_at(bytes: &[u8], i: usize) -> bool {
    let mut slashes = 0usize;
    let mut j = i;
    while j > 0 && bytes[j - 1] == b'\\' {
        slashes += 1;
        j -= 1;
    }
    slashes % 2 == 1
}

fn unmatched_footnote_ref_end(text: &str, start: usize) -> Option<usize> {
    let rest = text.get(start + 2..)?;
    let close_rel = rest.find(']')?;
    let label = &rest[..close_rel];
    if label.is_empty()
        || label.contains(['[', ']', '\n', '\r'])
        || label.chars().any(char::is_whitespace)
    {
        return None;
    }
    let end = start + 2 + close_rel + 1;
    // `[^1](url)` / `[^1][ref]` are links, not footnote refs.
    match text.as_bytes().get(end) {
        Some(b'(' | b'[') => None,
        _ => Some(end),
    }
}

/// Source range of the footnote label inside `[^label]` (the painted mark).
pub fn footnote_ref_inner_range(raw: &str, source_range: Range<usize>) -> Option<Range<usize>> {
    let label = footnote_ref_label(raw)?;
    let t = raw.trim();
    let trim_off = raw.find(t)?;
    let start = source_range.start.saturating_add(trim_off + 2);
    let end = start.saturating_add(label.len());
    if end <= source_range.end && start >= source_range.start {
        Some(start..end)
    } else {
        None
    }
}

/// `[^label]: body` → `(label, body)`.
pub fn footnote_definition(raw: &str) -> Option<(&str, &str)> {
    let t = raw.trim();
    let rest = t.strip_prefix("[^")?;
    let close = rest.find("]:")?;
    let label = &rest[..close];
    if label.is_empty() || label.contains(['[', ']']) {
        return None;
    }
    Some((label, rest[close + 2..].trim()))
}

/// Typora/PHP-Extra definition list: terms followed by `: details` lines.
pub fn definition_list_items(raw: &str) -> Option<Vec<(String, String)>> {
    if raw.trim_start().starts_with('<') {
        return None;
    }
    let mut items = Vec::new();
    let mut term = String::new();
    let mut details = String::new();
    let mut saw_details = false;
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(':') {
            if term.is_empty() {
                return None;
            }
            saw_details = true;
            if !details.is_empty() {
                details.push('\n');
            }
            details.push_str(rest.trim_start());
        } else if trimmed.is_empty() {
            continue;
        } else {
            if saw_details {
                items.push((std::mem::take(&mut term), std::mem::take(&mut details)));
                saw_details = false;
            } else if !term.is_empty() {
                term.push('\n');
            }
            term.push_str(line.trim());
        }
    }
    if saw_details && !term.is_empty() {
        items.push((term, details));
    }
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

/// Project an HTML block literal into something Typora would paint (no JS).
pub fn project_html_block(raw: &str) -> HtmlBlockVisual {
    if html_block_is_source_chrome(raw) {
        return HtmlBlockVisual::Hidden;
    }
    let mut stack = HtmlStack::with_class_styles(collect_class_styles(raw));
    let mut text = String::new();
    let mut source_at = Vec::new();
    let mut runs: Vec<HtmlPaintRun> = Vec::new();
    let mut images: Vec<(String, String)> = Vec::new();
    let mut saw_hr = false;
    let mut saw_other = false;
    let mut i = 0;
    let bytes = raw.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some((piece, end)) = parse_piece_at(raw, i) {
                match piece {
                    HtmlPiece::Tag(tag) if !tag.close && tag.name == "svg" && !stack.hidden() => {
                        if let Some(svg_end) = find_matching_close(raw, i, "svg") {
                            let blob = &raw[i..svg_end];
                            if let Some(url) = svg_to_data_url(blob) {
                                images.push((url, svg_alt(blob)));
                            }
                            i = svg_end;
                            continue;
                        }
                        stack.open(&tag);
                        i = end;
                        continue;
                    }
                    HtmlPiece::Tag(tag) => {
                        if tag.close {
                            stack.close(&tag.name);
                        } else {
                            match stack.open(&tag) {
                                InlineHtmlAction::Break => {
                                    if !stack.hidden() {
                                        push_visible(
                                            &mut text,
                                            &mut source_at,
                                            &mut runs,
                                            "\n",
                                            i..end,
                                            stack.paint(),
                                        );
                                        saw_other = true;
                                    }
                                }
                                InlineHtmlAction::Image { url, alt } => {
                                    if !stack.hidden() {
                                        images.push((url, alt));
                                    }
                                }
                                InlineHtmlAction::Hide if tag.name == "hr" && !stack.hidden() => {
                                    saw_hr = true;
                                }
                                _ => {}
                            }
                        }
                    }
                    HtmlPiece::Comment | HtmlPiece::OtherMarkup => {}
                }
                i = end;
                continue;
            }
        }
        let next = raw[i..].find('<').map(|n| i + n).unwrap_or(raw.len());
        if next > i && !stack.hidden() {
            let chunk = &raw[i..next];
            if is_blockish_gap(chunk) {
                if !text.ends_with('\n') && !text.is_empty() {
                    push_visible(
                        &mut text,
                        &mut source_at,
                        &mut runs,
                        "\n",
                        i..i + 1,
                        stack.paint(),
                    );
                }
            } else {
                let decoded = decode_entities(chunk);
                if !decoded.trim().is_empty() {
                    saw_other = true;
                }
                push_visible(
                    &mut text,
                    &mut source_at,
                    &mut runs,
                    &decoded,
                    i..next,
                    stack.paint(),
                );
            }
        }
        i = next.max(i + 1);
    }

    let text_trim = text.trim();
    if !saw_other && images.len() == 1 && !saw_hr {
        let (url, alt) = images.remove(0);
        return HtmlBlockVisual::Image { url, alt };
    }
    if !saw_other && images.is_empty() && saw_hr {
        return HtmlBlockVisual::ThematicBreak;
    }
    if text_trim.is_empty() && images.is_empty() {
        return HtmlBlockVisual::Hidden;
    }
    for (url, alt) in images {
        if !text.ends_with('\n') && !text.is_empty() {
            let n = text.len();
            push_visible(
                &mut text,
                &mut source_at,
                &mut runs,
                "\n",
                n..n + 1,
                HtmlPaint::default(),
            );
        }
        let label = if alt.is_empty() { url } else { alt };
        let n = text.len();
        push_visible(
            &mut text,
            &mut source_at,
            &mut runs,
            &label,
            n..n + label.len(),
            HtmlPaint::default(),
        );
    }
    while source_at.len() < text.len() + 1 {
        source_at.push(*source_at.last().unwrap_or(&0));
    }
    source_at.truncate(text.len() + 1);
    HtmlBlockVisual::Flow {
        text,
        source_at,
        runs,
    }
}

/// Opening wrapper tag and optional last close in an HTML-block literal.
///
/// Used to reveal `<div>` / `</div>` (and `<p>` / `</p>`, …) when the caret
/// intersects, without painting `script` / `iframe` / `<details>` / void widgets.
pub fn html_block_wrapper_tag_ranges(raw: &str) -> Option<(Range<usize>, Option<Range<usize>>)> {
    if html_block_is_source_chrome(raw) || html_block_is_hidden_widget(raw) {
        return None;
    }
    let mut stack = HtmlStack::default();
    let mut first_open = None;
    let mut last_close = None;
    let mut i = 0;
    let bytes = raw.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some((piece, end)) = parse_piece_at(raw, i) {
                if let HtmlPiece::Tag(tag) = piece {
                    let visible = !stack.hidden();
                    if tag.close {
                        if visible
                            && first_open.is_some()
                            && !is_dangerous_tag(&tag.name)
                            && !is_void_widget_tag(&tag.name)
                            && !is_type6_source_tag(&tag.name)
                        {
                            last_close = Some(i..end);
                        }
                        stack.close(&tag.name);
                    } else {
                        if visible
                            && first_open.is_none()
                            && !is_dangerous_tag(&tag.name)
                            && !is_void_widget_tag(&tag.name)
                            && !is_type6_source_tag(&tag.name)
                            && !tag.self_closing
                            && !is_void_tag(&tag.name)
                        {
                            first_open = Some(i..end);
                        }
                        let _ = stack.open(&tag);
                    }
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    Some((first_open?, last_close))
}

/// CommonMark HTML-block type 1 `<pre>`: inner text is literal, not Markdown.
pub fn html_block_is_preformatted(raw: &str) -> bool {
    first_open_tag_name(raw).is_some_and(|name| name == "pre")
}

/// HTML-block comment / `<?…?>` / `<![CDATA[…]]>` / `<!DOCTYPE>` chrome:
/// hidden unless the caret intersects, then the source paints (same rule
/// as inline comments). PI and CDATA close at `?>` / `]]>`, not the first `>`.
///
/// CommonMark Type-1 `<style>` / `<textarea>` share that hide-until-intersect
/// rule (CSS / textarea source, not a document stylesheet). `<script>` is
/// not source chrome — it stays hidden and skips as a widget. GFM tagfilter
/// `<title>` / `<xmp>` / `<plaintext>` paint source the same way as
/// `<textarea>` (raw text, not a nested document) but skip as widgets.
/// Type-6 `<details>` / `<dialog>` / `<form>` / `<option>` / `<optgroup>` /
/// `<fieldset>` / `<legend>` and Type-7 `<video>` / `<audio>` / `<canvas>` /
/// `<math>` / `<button>` / `<select>` / `<input>` / `<label>` / `<noscript>` /
/// `<template>` / `<output>` / `<progress>` / `<meter>` are source chrome the
/// same way (not a player, form, nested document, or disclosure UI).
pub fn html_block_is_source_chrome(raw: &str) -> bool {
    html_block_is_markup_chrome(raw)
        || html_block_is_type1_source(raw)
        || html_block_is_tagfilter_source(raw)
        || html_block_is_type6_source(raw)
}

/// Comments / PI / CDATA / DOCTYPE only — one caret/Delete step.
pub fn html_block_is_markup_chrome(raw: &str) -> bool {
    let t = raw.trim_start();
    matches!(
        parse_piece_at(t, 0),
        Some((HtmlPiece::Comment | HtmlPiece::OtherMarkup, end)) if t[end..].trim().is_empty()
    )
}

/// CommonMark HTML-block type 1 `<style>` / `<textarea>`: hide unless the
/// caret intersects, then paint source. Tags skip as dest chrome; inner
/// CSS / textarea text is editable. Not a document stylesheet.
pub fn html_block_is_type1_source(raw: &str) -> bool {
    first_open_tag_name(raw).is_some_and(|name| matches!(name.as_str(), "style" | "textarea"))
}

/// Open tag and optional close tag of a Type-1 `<style>` / `<textarea>` block.
pub fn html_type1_tag_ranges(raw: &str) -> Option<(Range<usize>, Option<Range<usize>>)> {
    if !html_block_is_type1_source(raw) {
        return None;
    }
    let name = first_open_tag_name(raw)?;
    let t = raw.trim_start();
    let lead = raw.len() - t.len();
    let (piece, open_end_rel) = parse_piece_at(t, 0)?;
    let HtmlPiece::Tag(tag) = piece else {
        return None;
    };
    if tag.close || tag.name != name {
        return None;
    }
    let open = lead..lead + open_end_rel;
    let close = find_matching_close(raw, open.start, &name).map(|end| {
        let close_tok = format!("</{name}>");
        let start = end.saturating_sub(close_tok.len());
        start..end
    });
    Some((open, close))
}

/// Inner CSS / textarea text between Type-1 tags (document-relative when
/// `base` is the block start). Empty when the tags are adjacent.
pub fn html_type1_inner_range(raw: &str, base: usize) -> Option<Range<usize>> {
    let (open, close) = html_type1_tag_ranges(raw)?;
    let start = base.saturating_add(open.end);
    let end = close
        .map(|c| base.saturating_add(c.start))
        .unwrap_or_else(|| base.saturating_add(raw.len()));
    Some(start.min(end)..end)
}

/// CommonMark HTML-block type 1 `<script>`: stay hidden (no JS paint /
/// execute). Skip/delete as one widget so the caret does not walk inner JS.
pub fn html_block_is_type1_script(raw: &str) -> bool {
    first_open_tag_name(raw).is_some_and(|name| name == "script")
}

/// Hidden dest-chrome widgets: Type-1 `<script>` and GFM tagfilter
/// `<iframe>` / `<noembed>` / `<noframes>`, plus plugin/frame tags
/// (`<object>` / `<embed>` / `<applet>` / `<frame>` / `<frameset>`).
/// Stay hidden even when the caret intersects; arrows/click skip inner;
/// Backspace/Delete remove the block.
pub fn html_block_is_hidden_widget(raw: &str) -> bool {
    first_open_tag_name(raw).is_some_and(|name| is_hidden_widget_tag(&name))
}

/// GFM tagfilter raw-text tags (`<title>` / `<xmp>` / `<plaintext>`): hide
/// unless the caret intersects, then paint source like Type-1 `<textarea>`.
/// Still one caret/Delete step — do not walk inner raw text as a document.
pub fn html_block_is_tagfilter_source(raw: &str) -> bool {
    first_open_tag_name(raw).is_some_and(|name| is_tagfilter_source_tag(&name))
}

/// CommonMark Type-6 `<details>` / `<dialog>` / `<form>` / `<option>` /
/// `<optgroup>` / `<fieldset>` / `<legend>` and Type-7 `<video>` / `<audio>` /
/// `<canvas>` / `<math>` / `<button>` / `<select>` / `<input>` / `<label>` /
/// `<noscript>` / `<template>` / `<output>` / `<progress>` / `<meter>`:
/// dest-chrome widgets
/// (skip inner HTML; Backspace/Delete the whole block). Hide unless the
/// caret intersects, then paint source like `<title>` / `<xmp>`. Not a
/// player, form, or disclosure UI — inner markdown after a blank line stays
/// a following CommonMark block. Type-1 `<textarea>` is inner-source, not
/// this list. `<datalist>` / `<picture>` / `<summary>` / `<search>` /
/// `<slot>` are not dest-chrome widgets.
pub fn html_block_is_type6_source(raw: &str) -> bool {
    first_open_tag_name(raw).is_some_and(|name| is_type6_source_tag(&name))
}

/// Tagfilter / Type-1 / Type-6 dest-chrome widgets (hidden or raw-text source).
pub fn html_block_is_tagfilter_widget(raw: &str) -> bool {
    html_block_is_hidden_widget(raw)
        || html_block_is_tagfilter_source(raw)
        || html_block_is_type6_source(raw)
}

/// Opening tagfilter / Type-6 dest-chrome tag as a complete inline
/// (`<iframe>`, `<xmp>`, `<details>`, `<video>`, `<button>`, …).
/// Void / self-closing tags (`<input>`, `<embed>`) are complete widgets,
/// not openers that search for a close (that would swallow the rest of a
/// paragraph).
pub fn html_inline_tagfilter_open(raw: &str) -> Option<String> {
    match parse_complete_piece(raw.trim()) {
        Some(HtmlPiece::Tag(tag))
            if !tag.close
                && !tag.self_closing
                && (is_hidden_widget_tag(&tag.name)
                    || is_tagfilter_source_tag(&tag.name)
                    || is_type6_source_tag(&tag.name)) =>
        {
            Some(tag.name)
        }
        _ => None,
    }
}

/// Matching close tag for [`html_inline_tagfilter_open`].
pub fn html_inline_tagfilter_close(raw: &str, name: &str) -> bool {
    matches!(
        parse_complete_piece(raw.trim()),
        Some(HtmlPiece::Tag(tag)) if tag.close && tag.name == name
    )
}

/// `script` / `iframe` / … stay hidden even when the caret intersects.
/// Type-1 `<style>` / `<textarea>` and tagfilter raw-text tags are source
/// chrome, not this list.
pub fn html_block_is_dangerous(raw: &str) -> bool {
    first_open_tag_name(raw).is_some_and(|name| is_dangerous_tag(&name))
        && !html_block_is_type1_source(raw)
        && !html_block_is_tagfilter_source(raw)
        && !html_block_is_type6_source(raw)
}

fn first_open_tag_name(raw: &str) -> Option<String> {
    let t = raw.trim_start();
    match parse_piece_at(t, 0) {
        Some((HtmlPiece::Tag(tag), _)) if !tag.close => Some(tag.name),
        _ => None,
    }
}

fn is_void_widget_tag(name: &str) -> bool {
    matches!(name, "br" | "wbr" | "hr" | "img")
}

/// Map digits (and a few signs) to Unicode superscripts. `None` if any char
/// cannot be raised — callers keep the original glyphs (footnote labels).
pub fn to_superscript(s: &str) -> Option<String> {
    let mut out = String::new();
    for c in s.chars() {
        out.push(super_char(c)?);
    }
    Some(out)
}

/// Subscript counterpart of [`to_superscript`].
pub fn to_subscript(s: &str) -> Option<String> {
    let mut out = String::new();
    for c in s.chars() {
        out.push(sub_char(c)?);
    }
    Some(out)
}

/// Per-character superscript map. Unmapped glyphs stay as-is. GPUI `TextRun`
/// has no per-run font size or baseline offset, so this is the WYSIWYG path.
pub fn map_superscript(s: &str) -> String {
    s.chars().map(|c| super_char(c).unwrap_or(c)).collect()
}

/// Per-character subscript map. Unmapped glyphs stay as-is.
pub fn map_subscript(s: &str) -> String {
    s.chars().map(|c| sub_char(c).unwrap_or(c)).collect()
}

fn super_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'a' | 'A' => 'ᵃ',
        'b' | 'B' => 'ᵇ',
        'c' | 'C' => 'ᶜ',
        'd' | 'D' => 'ᵈ',
        'e' | 'E' => 'ᵉ',
        'f' | 'F' => 'ᶠ',
        'g' | 'G' => 'ᵍ',
        'h' | 'H' => 'ʰ',
        'i' | 'I' => 'ⁱ',
        'j' | 'J' => 'ʲ',
        'k' | 'K' => 'ᵏ',
        'l' | 'L' => 'ˡ',
        'm' | 'M' => 'ᵐ',
        'n' | 'N' => 'ⁿ',
        'o' | 'O' => 'ᵒ',
        'p' | 'P' => 'ᵖ',
        'r' | 'R' => 'ʳ',
        's' | 'S' => 'ˢ',
        't' | 'T' => 'ᵗ',
        'u' | 'U' => 'ᵘ',
        'v' | 'V' => 'ᵛ',
        'w' | 'W' => 'ʷ',
        'x' | 'X' => 'ˣ',
        'y' | 'Y' => 'ʸ',
        'z' | 'Z' => 'ᶻ',
        _ => return None,
    })
}

fn sub_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' | 'A' => 'ₐ',
        'e' | 'E' => 'ₑ',
        'h' | 'H' => 'ₕ',
        'i' | 'I' => 'ᵢ',
        'j' | 'J' => 'ⱼ',
        'k' | 'K' => 'ₖ',
        'l' | 'L' => 'ₗ',
        'm' | 'M' => 'ₘ',
        'n' | 'N' => 'ₙ',
        'o' | 'O' => 'ₒ',
        'p' | 'P' => 'ₚ',
        'r' | 'R' => 'ᵣ',
        's' | 'S' => 'ₛ',
        't' | 'T' => 'ₜ',
        'u' | 'U' => 'ᵤ',
        'v' | 'V' => 'ᵥ',
        'x' | 'X' => 'ₓ',
        _ => return None,
    })
}

fn push_visible(
    text: &mut String,
    source_at: &mut Vec<usize>,
    runs: &mut Vec<HtmlPaintRun>,
    s: &str,
    src: Range<usize>,
    paint: HtmlPaint,
) {
    if s.is_empty() {
        return;
    }
    if source_at.len() < text.len() + 1 {
        source_at.resize(text.len() + 1, src.start);
    }
    source_at[text.len()] = src.start;
    let nchars = s.chars().count().max(1);
    for (i, (off, _)) in s.char_indices().enumerate() {
        if off == 0 {
            continue;
        }
        let mapped = if src.len() == s.len() {
            src.start + off
        } else {
            src.start + (src.len() * i / nchars)
        };
        source_at.push(mapped);
    }
    text.push_str(s);
    if let Some(last) = runs.last_mut() {
        if last.paint == paint {
            last.len += s.len();
            return;
        }
    }
    runs.push(HtmlPaintRun {
        len: s.len(),
        paint,
    });
}

fn is_blockish_gap(chunk: &str) -> bool {
    !chunk.is_empty() && chunk.chars().all(|c| c.is_whitespace())
}

fn parse_complete_piece(raw: &str) -> Option<HtmlPiece> {
    let t = raw.trim();
    let (piece, end) = parse_piece_at(t, 0)?;
    if t[end..].trim().is_empty() {
        Some(piece)
    } else {
        None
    }
}

fn parse_piece_at(s: &str, start: usize) -> Option<(HtmlPiece, usize)> {
    let bytes = s.as_bytes();
    if start >= bytes.len() || bytes[start] != b'<' {
        return None;
    }
    let rest = &s[start..];
    if rest.starts_with("<!--") {
        let end = rest.find("-->").map(|n| start + n + 3)?;
        return Some((HtmlPiece::Comment, end));
    }
    // CommonMark HTML-block type 5: ends at `]]>`, not the first `>`.
    if rest.starts_with("<![CDATA[") {
        let end = rest.find("]]>").map(|n| start + n + 3).unwrap_or(s.len());
        return Some((HtmlPiece::OtherMarkup, end));
    }
    // Type 3 processing instruction: ends at `?>`, not the first `>`.
    if rest.starts_with("<?") {
        let end = rest.find("?>").map(|n| start + n + 2).unwrap_or(s.len());
        return Some((HtmlPiece::OtherMarkup, end));
    }
    // Type 4 declaration (`<!DOCTYPE`, `<!ELEMENT`, …): first `>`.
    if rest.starts_with("<!") {
        let end = rest.find('>').map(|n| start + n + 1)?;
        return Some((HtmlPiece::OtherMarkup, end));
    }
    let mut i = start + 1;
    let close = bytes.get(i) == Some(&b'/');
    if close {
        i += 1;
    }
    let name_start = i;
    while i < bytes.len() && is_name_char(bytes[i]) {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name = s[name_start..i].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut self_closing = false;
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        if bytes[i] == b'>' {
            i += 1;
            break;
        }
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'>') {
            self_closing = true;
            i += 2;
            break;
        }
        let (attr_name, attr_val, next) = parse_attr(s, i)?;
        attrs.push((attr_name, attr_val));
        i = next;
    }
    Some((
        HtmlPiece::Tag(ParsedTag {
            self_closing: self_closing || (!close && is_void_tag(&name)),
            name,
            close,
            attrs,
        }),
        i,
    ))
}

fn parse_attr(s: &str, start: usize) -> Option<(String, String, usize)> {
    let bytes = s.as_bytes();
    let mut i = start;
    if i >= bytes.len() || !is_name_char(bytes[i]) {
        return None;
    }
    let name_start = i;
    while i < bytes.len() && (is_name_char(bytes[i]) || bytes[i] == b':') {
        i += 1;
    }
    let name = s[name_start..i].to_ascii_lowercase();
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if bytes.get(i) != Some(&b'=') {
        return Some((name, String::new(), i));
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let (val, next) = match bytes[i] {
        b'"' | b'\'' => {
            let q = bytes[i];
            i += 1;
            let vs = i;
            while i < bytes.len() && bytes[i] != q {
                i += 1;
            }
            if i >= bytes.len() {
                return None;
            }
            let val = decode_entities(&s[vs..i]);
            (val, i + 1)
        }
        _ => {
            let vs = i;
            while i < bytes.len()
                && !bytes[i].is_ascii_whitespace()
                && bytes[i] != b'>'
                && bytes[i] != b'/'
            {
                i += 1;
            }
            (decode_entities(&s[vs..i]), i)
        }
    };
    Some((name, val, next))
}

fn is_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-'
}

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// SVG is data, not an active document in the editor. Keep the accepted
/// surface deliberately small: rendering a hand-authored diagram is useful,
/// while executing, embedding, or fetching through an SVG is never needed.
///
/// Maximum SVG source size accepted by the editor's visual-only SVG policy.
///
/// Image loaders use this to bound their preflight reads before handing bytes
/// to [`is_safe_svg_document`].
pub const MAX_SAFE_SVG_BYTES: usize = 128 * 1024;
const MAX_SVG_TAGS: usize = 4096;

fn looks_like_svg_open(s: &str) -> bool {
    let t = s.trim_start().as_bytes();
    if t.len() < 4 || !t[..4].eq_ignore_ascii_case(b"<svg") {
        return false;
    }
    t.len() == 4 || matches!(t[4], b'>' | b'/' | b'\t' | b'\n' | b'\r' | b' ')
}

fn svg_to_data_url(raw: &str) -> Option<String> {
    let t = raw.trim();
    if !is_safe_svg_document(t) {
        return None;
    }
    let complete = find_matching_close(t, 0, "svg") == Some(t.len())
        || matches!(
            parse_complete_piece(t),
            Some(HtmlPiece::Tag(tag)) if tag.name == "svg" && tag.self_closing
        );
    if !complete {
        return None;
    }
    Some(format!(
        "data:image/svg+xml;charset=utf-8,{}",
        percent_encode_data(t.as_bytes())
    ))
}

/// Whether an SVG document is safe to hand to the native image decoder.
///
/// This is intentionally a strict *validator*, rather than a best-effort
/// sanitizer. If a diagram needs a feature outside this visual-only subset,
/// it remains Markdown source instead of becoming an active image resource.
/// The editor also uses this for direct `data:image/svg+xml` destinations, so
/// HTML-originated and Markdown-originated SVGs follow the same policy.
pub fn is_safe_svg_document(raw: &str) -> bool {
    let t = raw.trim();
    if t.len() > MAX_SAFE_SVG_BYTES || !looks_like_svg_open(t) {
        return false;
    }
    let complete = find_matching_close(t, 0, "svg") == Some(t.len())
        || matches!(
            parse_complete_piece(t),
            Some(HtmlPiece::Tag(tag)) if tag.name == "svg" && !tag.close && tag.self_closing
        );
    if !complete {
        return false;
    }

    let mut i = 0usize;
    let mut tags = 0usize;
    while let Some(relative) = t[i..].find('<') {
        let start = i + relative;
        let Some((piece, end)) = parse_piece_at(t, start) else {
            // SVG is XML: a stray `<` is malformed input, not text to pass to
            // a decoder with its own, potentially different parser.
            return false;
        };
        match piece {
            HtmlPiece::Comment => {}
            HtmlPiece::OtherMarkup => {
                // Processing instructions, declarations, and CDATA are not
                // needed for a visual image and can carry parser-specific
                // behavior such as external entities.
                return false;
            }
            HtmlPiece::Tag(tag) => {
                tags += 1;
                if tags > MAX_SVG_TAGS
                    || is_unsafe_svg_tag(&tag.name)
                    || tag
                        .attrs
                        .iter()
                        .any(|(name, value)| !is_safe_svg_attr(name, value))
                {
                    return false;
                }
            }
        }
        i = end;
    }
    tags > 0
}

fn is_unsafe_svg_tag(name: &str) -> bool {
    matches!(
        name,
        "script"
            | "style"
            | "foreignobject"
            | "iframe"
            | "embed"
            | "object"
            | "frame"
            | "frameset"
            | "image"
            | "audio"
            | "video"
            | "canvas"
            | "animate"
            | "animatemotion"
            | "animatetransform"
            | "set"
            | "discard"
            | "mpath"
            | "handler"
            | "listener"
    )
}

fn is_safe_svg_attr(name: &str, value: &str) -> bool {
    // SVG event attributes are arbitrary JavaScript, including less-common
    // names such as `onbegin` and `onrepeat`.
    if name.starts_with("on") || matches!(name, "src" | "srcset" | "externalresourcesrequired") {
        return false;
    }
    if matches!(name, "href" | "xlink:href") && !is_local_svg_reference(value) {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    if lower.contains("javascript:") || lower.contains("vbscript:") || lower.contains("data:") {
        return false;
    }
    if name == "style"
        && (lower.contains("@import")
            || lower.contains("expression(")
            || lower.contains("behavior:")
            || lower.contains("-moz-binding"))
    {
        return false;
    }
    !contains_nonlocal_svg_url(&lower)
}

fn is_local_svg_reference(value: &str) -> bool {
    let value = value.trim();
    value.is_empty()
        || value
            .strip_prefix('#')
            .is_some_and(|fragment| !fragment.is_empty())
}

fn contains_nonlocal_svg_url(value: &str) -> bool {
    let mut rest = value;
    while let Some(index) = rest.find("url(") {
        let after_open = &rest[index + 4..];
        let Some(close) = after_open.find(')') else {
            return true;
        };
        let reference = after_open[..close]
            .trim()
            .trim_matches(|c| matches!(c, '\'' | '"'));
        if !is_local_svg_reference(reference) {
            return true;
        }
        rest = &after_open[close + 1..];
    }
    false
}

fn svg_alt(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if let Some(start_rel) = lower.find("<title") {
        if let Some(gt) = raw[start_rel..].find('>') {
            let inner = start_rel + gt + 1;
            if let Some(end_rel) = lower[inner..].find("</title>") {
                let title = raw[inner..inner + end_rel].trim();
                if !title.is_empty() {
                    return title.to_string();
                }
            }
        }
    }
    "SVG".to_string()
}

fn find_matching_close(s: &str, open_start: usize, name: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let gt = s.get(open_start..)?.find('>')? + open_start;
    let mut i = gt + 1;
    let mut depth = 1i32;
    while i < lower.len() && depth > 0 {
        if lower[i..].starts_with(&close) {
            depth -= 1;
            if depth == 0 {
                return Some(i + close.len());
            }
            i += close.len();
            continue;
        }
        if lower[i..].starts_with(&open) {
            let after = i + open.len();
            let next = lower.as_bytes().get(after).copied();
            if next.is_none() || matches!(next, Some(b'>' | b'/' | b'\t' | b'\n' | b'\r' | b' ')) {
                depth += 1;
            }
        }
        i += 1;
    }
    None
}

fn percent_encode_data(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for &b in bytes {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn collect_class_styles(raw: &str) -> Vec<(String, CssPaint)> {
    let lower = raw.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut search = 0usize;
    while search < lower.len() {
        let Some(rel) = lower[search..].find("<style") else {
            break;
        };
        let start = search + rel;
        let after = match lower.as_bytes().get(start + 6) {
            Some(c) if *c == b'>' || c.is_ascii_whitespace() => start + 6,
            _ => {
                search = start + 6;
                continue;
            }
        };
        let Some(gt) = raw[after..].find('>') else {
            break;
        };
        let css_start = after + gt + 1;
        let Some(close_rel) = lower[css_start..].find("</style>") else {
            break;
        };
        parse_css_class_rules(&raw[css_start..css_start + close_rel], &mut out);
        search = css_start + close_rel + 8;
    }
    out
}

fn parse_css_class_rules(css: &str, out: &mut Vec<(String, CssPaint)>) {
    let stripped = strip_css_comments(css);
    for rule in stripped.split('}') {
        let Some((sel, body)) = rule.split_once('{') else {
            continue;
        };
        let sel = sel.trim();
        let Some(name) = sel.strip_prefix('.') else {
            continue;
        };
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            continue;
        }
        out.push((name.to_ascii_lowercase(), parse_style_attr(body)));
    }
}

fn strip_css_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut i = 0usize;
    while i < css.len() {
        if css[i..].starts_with("/*") {
            if let Some(end) = css[i + 2..].find("*/") {
                i += 2 + end + 2;
                continue;
            }
            break;
        }
        let ch = css[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn parse_style_attr(s: &str) -> CssPaint {
    let mut out = CssPaint::default();
    for part in s.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((k, v)) = part.split_once(':') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let val = v.trim().to_ascii_lowercase();
        match key.as_str() {
            "font-weight" => {
                let heavy = matches!(val.as_str(), "bold" | "bolder")
                    || val.parse::<u16>().ok().is_some_and(|n| n >= 600);
                if heavy {
                    out.bold = true;
                }
            }
            "font-style" if matches!(val.as_str(), "italic" | "oblique") => {
                out.italic = true;
            }
            "text-decoration" | "text-decoration-line" => {
                if val.contains("underline") {
                    out.underline = true;
                }
                if val.contains("line-through") {
                    out.strike = true;
                }
            }
            "color" => out.color = parse_css_color(&val),
            "background-color" => out.background = parse_css_color(&val),
            "background" if !val.contains("url(") => {
                if let Some(token) = val.split_whitespace().next() {
                    out.background = parse_css_color(token);
                }
            }
            _ => {}
        }
    }
    out
}

fn parse_css_color(s: &str) -> Option<CssColor> {
    let s = s
        .trim()
        .trim_matches(|c| c == '\'' || c == '"')
        .to_ascii_lowercase();
    if s.is_empty()
        || s.starts_with("url(")
        || matches!(
            s.as_str(),
            "inherit" | "transparent" | "currentcolor" | "none"
        )
    {
        return None;
    }
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    if let Some(inner) = s.strip_prefix("rgb(").and_then(|t| t.strip_suffix(')')) {
        return parse_rgb_func(inner);
    }
    named_css_color(&s)
}

fn parse_hex_color(hex: &str) -> Option<CssColor> {
    let hex = hex.trim();
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?;
            Some(CssColor { r, g, b })
        }
        6 | 8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(CssColor { r, g, b })
        }
        _ => None,
    }
}

fn parse_rgb_func(inner: &str) -> Option<CssColor> {
    let mut parts = inner.split(',').map(|p| p.trim());
    let r = parse_rgb_channel(parts.next()?)?;
    let g = parse_rgb_channel(parts.next()?)?;
    let b = parse_rgb_channel(parts.next()?)?;
    Some(CssColor { r, g, b })
}

fn parse_rgb_channel(s: &str) -> Option<u8> {
    if s.contains('%') {
        return None;
    }
    s.parse::<u16>().ok().map(|n| n.min(255) as u8)
}

fn named_css_color(name: &str) -> Option<CssColor> {
    Some(match name {
        "black" => CssColor { r: 0, g: 0, b: 0 },
        "silver" => CssColor {
            r: 192,
            g: 192,
            b: 192,
        },
        "gray" | "grey" => CssColor {
            r: 128,
            g: 128,
            b: 128,
        },
        "white" => CssColor {
            r: 255,
            g: 255,
            b: 255,
        },
        "maroon" => CssColor { r: 128, g: 0, b: 0 },
        "red" => CssColor { r: 255, g: 0, b: 0 },
        "purple" => CssColor {
            r: 128,
            g: 0,
            b: 128,
        },
        "fuchsia" | "magenta" => CssColor {
            r: 255,
            g: 0,
            b: 255,
        },
        "green" => CssColor { r: 0, g: 128, b: 0 },
        "lime" => CssColor { r: 0, g: 255, b: 0 },
        "olive" => CssColor {
            r: 128,
            g: 128,
            b: 0,
        },
        "yellow" => CssColor {
            r: 255,
            g: 255,
            b: 0,
        },
        "navy" => CssColor { r: 0, g: 0, b: 128 },
        "blue" => CssColor { r: 0, g: 0, b: 255 },
        "teal" => CssColor {
            r: 0,
            g: 128,
            b: 128,
        },
        "aqua" | "cyan" => CssColor {
            r: 0,
            g: 255,
            b: 255,
        },
        "orange" => CssColor {
            r: 255,
            g: 165,
            b: 0,
        },
        "pink" => CssColor {
            r: 255,
            g: 192,
            b: 203,
        },
        "brown" => CssColor {
            r: 165,
            g: 42,
            b: 42,
        },
        _ => return None,
    })
}

fn img_from_attrs(attrs: &[(String, String)]) -> Option<(String, String)> {
    let url = safe_url(attr(attrs, "src")?)?;
    let alt = attr(attrs, "alt").unwrap_or("").to_string();
    Some((url, alt))
}

fn safe_url(url: &str) -> Option<String> {
    let t = url.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("javascript:")
        || lower.starts_with("vbscript:")
        || lower.starts_with("data:text/html")
    {
        return None;
    }
    Some(t.to_string())
}

fn is_void_tag(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn is_hidden_widget_tag(name: &str) -> bool {
    matches!(
        name,
        "script"
            | "iframe"
            | "noembed"
            | "noframes"
            | "object"
            | "embed"
            | "applet"
            | "frame"
            | "frameset"
    )
}

fn is_tagfilter_source_tag(name: &str) -> bool {
    matches!(name, "title" | "xmp" | "plaintext")
}

fn is_type6_source_tag(name: &str) -> bool {
    matches!(
        name,
        "details"
            | "dialog"
            | "form"
            | "video"
            | "audio"
            | "canvas"
            | "math"
            | "button"
            | "select"
            | "input"
            | "option"
            | "optgroup"
            | "label"
            | "noscript"
            | "template"
            | "fieldset"
            | "legend"
            | "output"
            | "progress"
            | "meter"
    )
}

fn is_dangerous_tag(name: &str) -> bool {
    matches!(
        name,
        "script"
            | "iframe"
            | "object"
            | "embed"
            | "applet"
            | "form"
            | "input"
            | "textarea"
            | "select"
            | "button"
            | "link"
            | "style"
            | "meta"
            | "base"
            | "frame"
            | "frameset"
            | "video"
            | "audio"
            | "source"
            | "track"
            | "canvas"
            | "svg"
            | "math"
            | "noscript"
            | "template"
            | "dialog"
            | "plaintext"
            | "xmp"
            | "title"
            | "noembed"
            | "noframes"
    )
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some((ch, n)) = entity_at(s, i) {
                out.push(ch);
                i += n;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn entity_at(s: &str, i: usize) -> Option<(char, usize)> {
    let rest = &s[i + 1..];
    let semi = rest.find(';')?;
    let body = &rest[..semi];
    let consumed = 1 + semi + 1;
    if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
        let n = u32::from_str_radix(hex, 16).ok()?;
        return Some((char::from_u32(n)?, consumed));
    }
    if let Some(dec) = body.strip_prefix('#') {
        let n = dec.parse::<u32>().ok()?;
        return Some((char::from_u32(n)?, consumed));
    }
    let ch = match body {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "ndash" => '–',
        "mdash" => '—',
        "hellip" => '…',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        _ => return None,
    };
    Some((ch, consumed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrasing_tags_are_chrome_and_img_is_not() {
        assert!(opaque_inline_is_caret_chrome("<b>"));
        assert!(opaque_inline_is_caret_chrome("</b>"));
        assert!(opaque_inline_is_caret_chrome("<!-- x -->"));
        assert!(!opaque_inline_is_caret_chrome("<br>"));
        assert!(!opaque_inline_is_caret_chrome("<br/>"));
        assert!(!opaque_inline_is_caret_chrome("<wbr>"));
        assert!(!opaque_inline_is_caret_chrome(
            "<img src=\"a.png\" alt=\"x\">"
        ));
        assert!(!opaque_inline_is_caret_chrome("[^1]"));
        assert!(!opaque_inline_is_caret_chrome("not html"));
        assert_eq!(html_reveal_kind("<b>"), HtmlRevealKind::Open("b".into()));
        assert_eq!(html_reveal_kind("</b>"), HtmlRevealKind::Close("b".into()));
        assert_eq!(
            html_reveal_kind("<a href=\"https://e.com\">"),
            HtmlRevealKind::Open("a".into())
        );
        assert_eq!(html_reveal_kind("<!-- x -->"), HtmlRevealKind::Solo);
        let comment = html_solo_markup_inner_range("<!-- x -->", 0..10).expect("comment inner");
        assert_eq!(&"<!-- x -->"[comment.clone()], " x ");
        assert!(html_solo_markup_inner_range("<b>", 0..3).is_none());
        assert!(html_solo_markup_inner_range("<br>", 0..4).is_none());
        let pi = html_solo_markup_inner_range("<?php echo 1; ?>", 0..16).expect("pi inner");
        assert_eq!(&"<?php echo 1; ?>"[pi], "php echo 1; ");
        let cdata = html_solo_markup_inner_range("<![CDATA[a > b]]>", 0..17).expect("cdata inner");
        assert_eq!(&"<![CDATA[a > b]]>"[cdata], "a > b");
        assert_eq!(html_reveal_kind("<br>"), HtmlRevealKind::Skip);
        assert_eq!(
            html_reveal_kind("<img src=\"a.png\" alt=\"x\">"),
            HtmlRevealKind::Skip
        );
        assert_eq!(html_reveal_kind("<script>"), HtmlRevealKind::Skip);
        assert_eq!(html_reveal_kind("[^1]"), HtmlRevealKind::Skip);
        assert!(html_inline_break("<br>"));
        assert!(html_inline_break("<br/>"));
        assert!(html_inline_break(" <br /> "));
        assert!(html_inline_break("<wbr>"));
        assert!(!html_inline_break("<br></br>"));
        assert!(!html_inline_break("<div>"));
        let inner = footnote_ref_inner_range("[^1]", 0..4).expect("inner");
        assert_eq!(&"[^1]"[inner.clone()], "1");
        let padded = footnote_ref_inner_range(" [^ab] ", 10..17).expect("padded");
        assert_eq!(padded, 13..15);
    }

    #[test]
    fn classify_applies_bold_then_hides_tags() {
        let mut stack = HtmlStack::default();
        assert_eq!(
            classify_opaque_inline("<b>", &mut stack),
            InlineHtmlAction::Hide
        );
        assert!(stack.paint().bold);
        assert_eq!(
            classify_opaque_inline("</b>", &mut stack),
            InlineHtmlAction::Hide
        );
        assert!(!stack.paint().bold);
        assert_eq!(
            classify_opaque_inline("<br/>", &mut stack),
            InlineHtmlAction::Break
        );
    }

    #[test]
    fn script_hides_until_close() {
        let mut stack = HtmlStack::default();
        classify_opaque_inline("<script>", &mut stack);
        assert!(stack.hidden());
        assert_eq!(
            classify_opaque_inline("alert(1)", &mut stack),
            InlineHtmlAction::Hide
        );
        classify_opaque_inline("</script>", &mut stack);
        assert!(!stack.hidden());
    }

    #[test]
    fn html_block_div_shows_inner_text_not_tags() {
        match project_html_block("<div class=\"x\">\nraw html\n</div>") {
            HtmlBlockVisual::Flow { text, .. } => {
                assert_eq!(text.trim(), "raw html");
                assert!(!text.contains("<div"));
            }
            other => panic!("expected flow, got {other:?}"),
        }
    }

    #[test]
    fn html_block_wrapper_tag_ranges_div() {
        let raw = "<div class=\"x\">\n**bold**\n</div>";
        let (open, close) = html_block_wrapper_tag_ranges(raw).expect("wrapper");
        assert_eq!(&raw[open], "<div class=\"x\">");
        assert_eq!(&raw[close.expect("close")], "</div>");
    }

    #[test]
    fn html_block_hr_is_a_rule() {
        assert_eq!(project_html_block("<hr>"), HtmlBlockVisual::ThematicBreak);
        assert_eq!(project_html_block("<hr/>"), HtmlBlockVisual::ThematicBreak);
    }

    #[test]
    fn html_block_comment_is_hidden() {
        assert_eq!(
            project_html_block("<!-- secret -->"),
            HtmlBlockVisual::Hidden
        );
        assert!(html_block_is_source_chrome("<!-- secret -->"));
        assert!(html_block_is_source_chrome("<!--\nsecret\n-->"));
        assert!(html_block_is_source_chrome("<!-- a > b -->"));
        assert!(!html_block_is_source_chrome("<div></div>"));
        assert!(!html_block_is_dangerous("<!-- x -->"));
    }

    #[test]
    fn html_block_pi_and_cdata_close_past_inner_gt() {
        for raw in [
            "<?php echo 1; ?>",
            "<?php if ($a > $b) echo 1; ?>",
            "<?xml version=\"1.0\"?>",
            "<![CDATA[hello]]>",
            "<![CDATA[a > b]]>",
            "<!DOCTYPE html>",
        ] {
            assert!(
                html_block_is_source_chrome(raw),
                "must be source chrome, {raw:?}"
            );
            assert_eq!(
                project_html_block(raw),
                HtmlBlockVisual::Hidden,
                "must not leak inner text, {raw:?}"
            );
        }
        assert!(opaque_inline_is_caret_chrome(
            "<?php if ($a > $b) echo 1; ?>"
        ));
        assert!(opaque_inline_is_caret_chrome("<![CDATA[a > b]]>"));
        assert_eq!(
            html_reveal_kind("<?php if ($a > $b) echo 1; ?>"),
            HtmlRevealKind::Solo
        );
        assert!(!html_block_is_source_chrome("<div>a > b</div>"));
        // Type 4 declarations still end at the first `>`.
        assert!(html_block_is_source_chrome("<!DOCTYPE html>"));
        assert!(!html_block_is_source_chrome(
            "<!DOCTYPE html SYSTEM \"a>b\"> leftover"
        ));
    }

    #[test]
    fn html_block_pre_is_preformatted_not_dangerous() {
        assert!(html_block_is_preformatted("<pre>**bold**</pre>"));
        assert!(html_block_is_preformatted("<PRE class=\"x\">y</PRE>"));
        assert!(!html_block_is_preformatted("<div>**bold**</div>"));
        assert!(!html_block_is_dangerous("<pre>x</pre>"));
    }

    #[test]
    fn html_block_script_is_hidden() {
        assert_eq!(
            project_html_block("<script>alert(1)</script>"),
            HtmlBlockVisual::Hidden
        );
        assert!(html_block_is_type1_script("<script>alert(1)</script>"));
        assert!(html_block_is_dangerous("<script>alert(1)</script>"));
        assert!(!html_block_is_source_chrome("<script>alert(1)</script>"));
        assert!(html_block_is_hidden_widget("<script>alert(1)</script>"));
        assert!(html_block_is_tagfilter_widget("<script>alert(1)</script>"));
    }

    #[test]
    fn html_block_tagfilter_iframe_is_a_hidden_widget() {
        for raw in [
            "<iframe src=\"https://e.com\"></iframe>",
            "<iframe src=\"https://e.com\"><p>nested</p></iframe>",
            "<IFRAME src=\"https://e.com\"></IFRAME>",
            "<noembed>fallback</noembed>",
            "<noframes>fallback</noframes>",
        ] {
            assert!(html_block_is_hidden_widget(raw), "hidden widget, {raw:?}");
            assert!(
                html_block_is_tagfilter_widget(raw),
                "tagfilter widget, {raw:?}"
            );
            assert!(html_block_is_dangerous(raw), "must stay dropped, {raw:?}");
            assert!(
                !html_block_is_source_chrome(raw),
                "iframe/noembed/noframes must not reveal source, {raw:?}"
            );
            assert!(
                !html_block_is_type1_script(raw),
                "not Type-1 script, {raw:?}"
            );
            assert_eq!(
                project_html_block(raw),
                HtmlBlockVisual::Hidden,
                "must not paint inner HTML as a nested document, {raw:?}"
            );
        }
    }

    #[test]
    fn html_block_tagfilter_title_xmp_are_source_chrome_widgets() {
        for raw in [
            "<title>Doc title</title>",
            "<TITLE>Doc title</TITLE>",
            "<xmp>raw <b>html</b></xmp>",
            "<plaintext>raw text",
        ] {
            assert!(
                html_block_is_tagfilter_source(raw),
                "tagfilter source, {raw:?}"
            );
            assert!(
                html_block_is_tagfilter_widget(raw),
                "tagfilter widget, {raw:?}"
            );
            assert!(html_block_is_source_chrome(raw), "source chrome, {raw:?}");
            assert!(
                !html_block_is_dangerous(raw),
                "title/xmp/plaintext reveal source, not stay dropped, {raw:?}"
            );
            assert!(
                !html_block_is_hidden_widget(raw),
                "not a stay-hidden iframe/script widget, {raw:?}"
            );
            assert!(
                !html_block_is_type1_source(raw),
                "not Type-1 style/textarea, {raw:?}"
            );
            assert_eq!(
                project_html_block(raw),
                HtmlBlockVisual::Hidden,
                "must not paint inner as flow/nested HTML, {raw:?}"
            );
        }
        assert!(!html_block_is_tagfilter_source(
            "<div><title>x</title></div>"
        ));
        assert!(!html_block_is_tagfilter_widget("<style>x</style>"));
        assert!(!html_block_is_tagfilter_widget("<textarea>x</textarea>"));
        assert_eq!(
            html_inline_tagfilter_open("<iframe>").as_deref(),
            Some("iframe")
        );
        assert_eq!(html_inline_tagfilter_open("<xmp>").as_deref(), Some("xmp"));
        assert!(html_inline_tagfilter_close("</xmp>", "xmp"));
        assert!(html_inline_tagfilter_close("</iframe>", "iframe"));
        assert!(html_inline_tagfilter_open("<xmp>raw</xmp>").is_none());
    }

    #[test]
    fn html_block_details_is_type6_source_chrome_widget() {
        for raw in [
            "<details><summary>Title</summary>body</details>",
            "<DETAILS open><summary>Title</summary>hidden</DETAILS>",
            "<details>\n<summary>Title</summary>\nhidden\n</details>",
        ] {
            assert!(html_block_is_type6_source(raw), "Type-6 source, {raw:?}");
            assert!(
                html_block_is_tagfilter_widget(raw),
                "dest-chrome widget, {raw:?}"
            );
            assert!(html_block_is_source_chrome(raw), "source chrome, {raw:?}");
            assert!(
                !html_block_is_tagfilter_source(raw),
                "details is Type-6, not a GFM tagfilter tag, {raw:?}"
            );
            assert!(
                !html_block_is_hidden_widget(raw),
                "details reveals source on intersect, {raw:?}"
            );
            assert!(
                !html_block_is_dangerous(raw),
                "details must reveal source, not stay dropped, {raw:?}"
            );
            assert_eq!(
                project_html_block(raw),
                HtmlBlockVisual::Hidden,
                "must not paint inner HTML as a nested document, {raw:?}"
            );
        }
        assert!(!html_block_is_type6_source("<div>hello</div>"));
        assert!(!html_block_is_type6_source("<title>Doc title</title>"));
        assert_eq!(
            html_inline_tagfilter_open("<details>").as_deref(),
            Some("details")
        );
        assert!(html_inline_tagfilter_close("</details>", "details"));
        assert!(html_inline_tagfilter_open("<details>body</details>").is_none());
        assert!(
            html_block_wrapper_tag_ranges("<details><summary>Title</summary>body</details>")
                .is_none()
        );
    }

    #[test]
    fn html_block_dangerous_html_is_dest_chrome_widget() {
        for raw in [
            "<dialog>hello</dialog>",
            "<DIALOG open>hello</DIALOG>",
            "<form action=\"/x\">ok</form>",
            "<video src=\"x.mp4\"></video>",
            "<video>\nhello\n</video>",
            "<audio src=\"x.mp3\"></audio>",
            "<canvas>fallback</canvas>",
            "<math>x^2</math>",
        ] {
            assert!(html_block_is_type6_source(raw), "source widget, {raw:?}");
            assert!(
                html_block_is_tagfilter_widget(raw),
                "dest-chrome widget, {raw:?}"
            );
            assert!(html_block_is_source_chrome(raw), "source chrome, {raw:?}");
            assert!(
                !html_block_is_hidden_widget(raw),
                "reveals source on intersect, {raw:?}"
            );
            assert!(
                !html_block_is_dangerous(raw),
                "must reveal source, not stay dropped, {raw:?}"
            );
            assert_eq!(
                project_html_block(raw),
                HtmlBlockVisual::Hidden,
                "must not paint inner HTML as a nested document, {raw:?}"
            );
        }
        for raw in [
            "<object data=\"x\"></object>",
            "<embed src=\"x\">",
            "<applet></applet>",
        ] {
            assert!(html_block_is_hidden_widget(raw), "hidden widget, {raw:?}");
            assert!(
                html_block_is_tagfilter_widget(raw),
                "dest-chrome widget, {raw:?}"
            );
            assert!(html_block_is_dangerous(raw), "must stay dropped, {raw:?}");
            assert!(
                !html_block_is_source_chrome(raw),
                "object/embed stay hidden like iframe, {raw:?}"
            );
            assert_eq!(
                project_html_block(raw),
                HtmlBlockVisual::Hidden,
                "must not paint plugin HTML, {raw:?}"
            );
        }
        assert!(!html_block_is_type6_source("<div>hello</div>"));
        assert!(!html_block_is_type6_source("<object data=\"x\"></object>"));
        assert_eq!(
            html_inline_tagfilter_open("<video>").as_deref(),
            Some("video")
        );
        assert_eq!(
            html_inline_tagfilter_open("<dialog>").as_deref(),
            Some("dialog")
        );
        assert!(html_inline_tagfilter_close("</video>", "video"));
        assert!(html_inline_tagfilter_open("<video></video>").is_none());
    }

    #[test]
    fn html_block_form_controls_are_dest_chrome_widgets() {
        for raw in [
            "<button>click</button>",
            "<BUTTON type=\"submit\">click</BUTTON>",
            "<button>\nclick\n</button>",
            "<select><option>a</option></select>",
            "<select>\n<option>a</option>\n</select>",
            "<input type=\"text\">",
            "<input type=\"text\"/>",
            "<label>Name</label>",
            "<option>a</option>",
            "<optgroup label=\"g\"><option>a</option></optgroup>",
        ] {
            assert!(html_block_is_type6_source(raw), "source widget, {raw:?}");
            assert!(
                html_block_is_tagfilter_widget(raw),
                "dest-chrome widget, {raw:?}"
            );
            assert!(html_block_is_source_chrome(raw), "source chrome, {raw:?}");
            assert!(
                !html_block_is_hidden_widget(raw),
                "reveals source on intersect, {raw:?}"
            );
            assert!(
                !html_block_is_dangerous(raw),
                "must reveal source, not stay dropped, {raw:?}"
            );
            assert!(
                !html_block_is_type1_source(raw),
                "not Type-1 textarea, {raw:?}"
            );
            assert_eq!(
                project_html_block(raw),
                HtmlBlockVisual::Hidden,
                "must not paint a live form UI, {raw:?}"
            );
        }
        assert!(!html_block_is_type6_source("<textarea>hello</textarea>"));
        assert!(!html_block_is_tagfilter_widget(
            "<textarea>hello</textarea>"
        ));
        assert!(!html_block_is_type6_source("<div>hello</div>"));
        assert_eq!(
            html_inline_tagfilter_open("<button>").as_deref(),
            Some("button")
        );
        assert_eq!(
            html_inline_tagfilter_open("<select>").as_deref(),
            Some("select")
        );
        assert_eq!(
            html_inline_tagfilter_open("<label>").as_deref(),
            Some("label")
        );
        assert!(
            html_inline_tagfilter_open("<input>").is_none(),
            "void <input> is a complete widget, not an opener that swallows the paragraph"
        );
        assert!(html_block_is_tagfilter_widget("<input type=\"text\">"));
        assert!(html_inline_tagfilter_close("</button>", "button"));
        assert!(html_inline_tagfilter_open("<button>click</button>").is_none());
        assert!(html_block_wrapper_tag_ranges("<button>click</button>").is_none());
    }

    #[test]
    fn html_block_noscript_and_template_are_dest_chrome_widgets() {
        for raw in [
            "<noscript>fallback</noscript>",
            "<NOSCRIPT>fallback</NOSCRIPT>",
            "<noscript>\nfallback\n</noscript>",
            "<template><p>slot</p></template>",
            "<template>\n<p>slot</p>\n</template>",
        ] {
            assert!(html_block_is_type6_source(raw), "source widget, {raw:?}");
            assert!(
                html_block_is_tagfilter_widget(raw),
                "dest-chrome widget, {raw:?}"
            );
            assert!(html_block_is_source_chrome(raw), "source chrome, {raw:?}");
            assert!(
                !html_block_is_hidden_widget(raw),
                "reveals source on intersect, {raw:?}"
            );
            assert!(
                !html_block_is_dangerous(raw),
                "must reveal source, not stay dropped, {raw:?}"
            );
            assert_eq!(
                project_html_block(raw),
                HtmlBlockVisual::Hidden,
                "must not paint inner as a nested document, {raw:?}"
            );
        }
        assert_eq!(
            html_inline_tagfilter_open("<noscript>").as_deref(),
            Some("noscript")
        );
        assert_eq!(
            html_inline_tagfilter_open("<template>").as_deref(),
            Some("template")
        );
        assert!(html_inline_tagfilter_close("</template>", "template"));
        assert!(html_inline_tagfilter_open("<noscript>fallback</noscript>").is_none());
    }

    #[test]
    fn html_block_fieldset_legend_are_dest_chrome_widgets() {
        for raw in [
            "<fieldset><legend>Title</legend>body</fieldset>",
            "<FIELDSET><legend>Title</legend>body</FIELDSET>",
            "<fieldset>\n<legend>Title</legend>\nhidden\n</fieldset>",
            "<legend>Title</legend>",
        ] {
            assert!(html_block_is_type6_source(raw), "source widget, {raw:?}");
            assert!(
                html_block_is_tagfilter_widget(raw),
                "dest-chrome widget, {raw:?}"
            );
            assert!(html_block_is_source_chrome(raw), "source chrome, {raw:?}");
            assert!(
                !html_block_is_hidden_widget(raw),
                "reveals source on intersect, {raw:?}"
            );
            assert!(
                !html_block_is_dangerous(raw),
                "must reveal source, not stay dropped, {raw:?}"
            );
            assert_eq!(
                project_html_block(raw),
                HtmlBlockVisual::Hidden,
                "must not paint fieldset/legend inner as flow, {raw:?}"
            );
        }
        assert_eq!(
            html_inline_tagfilter_open("<fieldset>").as_deref(),
            Some("fieldset")
        );
        assert_eq!(
            html_inline_tagfilter_open("<legend>").as_deref(),
            Some("legend")
        );
        assert!(html_inline_tagfilter_close("</fieldset>", "fieldset"));
        assert!(html_inline_tagfilter_open("<fieldset>body</fieldset>").is_none());
        assert!(
            html_block_wrapper_tag_ranges("<fieldset><legend>Title</legend>body</fieldset>")
                .is_none()
        );
    }

    #[test]
    fn html_block_output_progress_meter_are_dest_chrome_widgets() {
        for raw in [
            "<output>42</output>",
            "<OUTPUT>42</OUTPUT>",
            "<output>\n42\n</output>",
            "<progress value=\"70\" max=\"100\">70%</progress>",
            "<progress>\n70%\n</progress>",
            "<meter value=\"0.6\">60%</meter>",
            "<meter>\nhalf\n</meter>",
        ] {
            assert!(html_block_is_type6_source(raw), "source widget, {raw:?}");
            assert!(
                html_block_is_tagfilter_widget(raw),
                "dest-chrome widget, {raw:?}"
            );
            assert!(html_block_is_source_chrome(raw), "source chrome, {raw:?}");
            assert!(
                !html_block_is_hidden_widget(raw),
                "reveals source on intersect, {raw:?}"
            );
            assert!(
                !html_block_is_dangerous(raw),
                "must reveal source, not stay dropped, {raw:?}"
            );
            assert!(
                !html_block_is_type1_source(raw),
                "not Type-1 textarea, {raw:?}"
            );
            assert_eq!(
                project_html_block(raw),
                HtmlBlockVisual::Hidden,
                "must not paint a live output/progress/meter UI, {raw:?}"
            );
        }
        assert_eq!(
            html_inline_tagfilter_open("<output>").as_deref(),
            Some("output")
        );
        assert_eq!(
            html_inline_tagfilter_open("<progress>").as_deref(),
            Some("progress")
        );
        assert_eq!(
            html_inline_tagfilter_open("<meter>").as_deref(),
            Some("meter")
        );
        assert!(html_inline_tagfilter_close("</output>", "output"));
        assert!(html_inline_tagfilter_open("<output>42</output>").is_none());
        assert!(html_block_wrapper_tag_ranges("<output>42</output>").is_none());
    }

    #[test]
    fn html_block_datalist_picture_summary_search_slot_are_not_dest_chrome() {
        for raw in [
            "<datalist><option>a</option></datalist>",
            "<datalist>\n<option>a</option>\n</datalist>",
            "<picture><img src=\"x.png\"></picture>",
            "<summary>Title</summary>",
            "<search>query</search>",
            "<slot>fallback</slot>",
        ] {
            assert!(
                !html_block_is_type6_source(raw),
                "must not be dest-chrome, {raw:?}"
            );
            assert!(
                !html_block_is_tagfilter_widget(raw),
                "outer must not skip as a widget, {raw:?}"
            );
        }
        assert!(html_inline_tagfilter_open("<datalist>").is_none());
        assert!(html_inline_tagfilter_open("<picture>").is_none());
        assert!(html_inline_tagfilter_open("<summary>").is_none());
        assert!(html_inline_tagfilter_open("<search>").is_none());
        assert!(html_inline_tagfilter_open("<slot>").is_none());
        assert_eq!(
            html_inline_tagfilter_open("<option>").as_deref(),
            Some("option"),
            "inner option stays a dest-chrome widget"
        );
        assert!(
            html_block_wrapper_tag_ranges("<summary>Title</summary>").is_some(),
            "summary wrapper tags still paint as flow, not dest-chrome"
        );
        assert!(
            html_block_wrapper_tag_ranges("<search>query</search>").is_some(),
            "search wrapper tags still paint as flow, not dest-chrome"
        );
    }

    #[test]
    fn html_block_style_and_textarea_are_type1_source_chrome() {
        for raw in [
            "<style>body { color: red }</style>",
            "<style>body > p { color: red }</style>",
            "<STYLE type=\"text/css\">x</STYLE>",
            "<textarea>hello</textarea>",
            "<textarea>a > b</textarea>",
        ] {
            assert!(html_block_is_type1_source(raw), "Type-1 source, {raw:?}");
            assert!(html_block_is_source_chrome(raw), "source chrome, {raw:?}");
            assert!(
                !html_block_is_dangerous(raw),
                "style/textarea must reveal, not stay dangerous, {raw:?}"
            );
            assert!(
                !html_block_is_markup_chrome(raw),
                "not atomic comment chrome, {raw:?}"
            );
            assert_eq!(
                project_html_block(raw),
                HtmlBlockVisual::Hidden,
                "must not paint CSS/textarea as flow, {raw:?}"
            );
        }
        assert!(!html_block_is_type1_source(
            "<div><style>.x { color: red }</style></div>"
        ));
        assert!(!html_block_is_type1_source("<pre>x</pre>"));
        assert!(!html_block_is_type1_script("<style>x</style>"));
        let inner = html_type1_inner_range("<style>body > p { color: red }</style>", 0)
            .expect("style inner");
        assert_eq!(
            &"<style>body > p { color: red }</style>"[inner],
            "body > p { color: red }"
        );
        let inner =
            html_type1_inner_range("<textarea>a > b</textarea>", 0).expect("textarea inner");
        assert_eq!(&"<textarea>a > b</textarea>"[inner], "a > b");
    }

    #[test]
    fn html_block_img_is_an_image() {
        match project_html_block("<img src=\"pic.png\" alt=\"cat\">") {
            HtmlBlockVisual::Image { url, alt } => {
                assert_eq!(url, "pic.png");
                assert_eq!(alt, "cat");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn javascript_urls_are_dropped() {
        assert!(html_inline_image("<img src=\"javascript:alert(1)\">").is_none());
        let mut stack = HtmlStack::default();
        classify_opaque_inline("<a href=\"javascript:alert(1)\">", &mut stack);
        assert!(stack.paint().href.is_none());
    }

    #[test]
    fn footnote_and_deflist_helpers() {
        assert_eq!(footnote_ref_label("[^1]"), Some("1"));
        assert_eq!(
            footnote_definition("[^1]: the note"),
            Some(("1", "the note"))
        );
        let items = definition_list_items("Term\n\n: Definition\n").unwrap();
        assert_eq!(items, vec![("Term".into(), "Definition".into())]);
        assert!(definition_list_items("plain paragraph\n").is_none());
    }

    #[test]
    fn find_unmatched_footnote_refs_skips_links_and_escapes() {
        let hits = find_unmatched_footnote_refs("Hello[^1] and [^note] world");
        assert_eq!(
            hits.iter()
                .map(|r| &"Hello[^1] and [^note] world"[r.clone()])
                .collect::<Vec<_>>(),
            vec!["[^1]", "[^note]"]
        );
        assert!(find_unmatched_footnote_refs("Hello[^1](https://e.com)").is_empty());
        assert!(find_unmatched_footnote_refs("Hello[^1][ref]").is_empty());
        assert!(find_unmatched_footnote_refs("Hello\\[^1]").is_empty());
        assert!(find_unmatched_footnote_refs("Hello[^]").is_empty());
        assert!(find_unmatched_footnote_refs("Hello[^a b]").is_empty());
    }

    #[test]
    fn entities_decode_in_html_blocks() {
        match project_html_block("<p>A&amp;B&nbsp;C</p>") {
            HtmlBlockVisual::Flow { text, .. } => {
                assert!(text.contains("A&B"), "{text:?}");
                assert!(text.contains('\u{a0}'), "{text:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn superscript_digits() {
        assert_eq!(to_superscript("12").as_deref(), Some("¹²"));
        assert_eq!(to_superscript("1q"), None);
        assert_eq!(to_subscript("2").as_deref(), Some("₂"));
        assert_eq!(map_superscript("2x"), "²ˣ");
        assert_eq!(map_subscript("2"), "₂");
        assert!(map_superscript("q").contains('q'));
    }

    #[test]
    fn html_block_mark_sub_sup_flags() {
        match project_html_block("<p><mark>hi</mark> H<sub>2</sub><sup>n</sup></p>") {
            HtmlBlockVisual::Flow { text, runs, .. } => {
                assert!(text.contains("hi"), "{text:?}");
                assert!(text.contains('2') || text.contains('H'), "{text:?}");
                assert!(
                    runs.iter().any(|r| r.paint.mark),
                    "expected mark paint, {runs:?}"
                );
                assert!(
                    runs.iter().any(|r| r.paint.sub),
                    "expected sub paint, {runs:?}"
                );
                assert!(
                    runs.iter().any(|r| r.paint.sup),
                    "expected sup paint, {runs:?}"
                );
                assert!(!text.contains("<mark"));
                assert!(!text.contains("<sub"));
            }
            other => panic!("{other:?}"),
        }
    }

    const TINY_SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\"><title>dot</title><rect width=\"8\" height=\"8\" fill=\"#f00\"/></svg>";

    fn flow_paints(raw: &str) -> (String, Vec<HtmlPaintRun>) {
        match project_html_block(raw) {
            HtmlBlockVisual::Flow { text, runs, .. } => (text, runs),
            other => panic!("expected flow, got {other:?} for {raw:?}"),
        }
    }

    #[test]
    fn style_color_and_weight_paint_inner_text() {
        let (text, runs) =
            flow_paints("<p><span style=\"color: #ff0000; font-weight: bold\">red</span></p>");
        assert!(text.contains("red"), "{text:?}");
        assert!(!text.contains("<span"));
        let painted = runs
            .iter()
            .find(|r| r.paint.color.is_some())
            .expect("color run");
        assert_eq!(painted.paint.color, Some(CssColor { r: 255, g: 0, b: 0 }));
        assert!(painted.paint.bold, "{runs:?}");
    }

    #[test]
    fn style_strike_underline_and_background() {
        let (text, runs) = flow_paints(
            "<p style=\"text-decoration: underline line-through; background-color: #ff0\">x</p>",
        );
        assert!(text.contains('x'), "{text:?}");
        assert!(
            runs.iter().any(|r| r.paint.underline && r.paint.strike),
            "{runs:?}"
        );
        assert!(
            runs.iter().any(|r| r.paint.background
                == Some(CssColor {
                    r: 255,
                    g: 255,
                    b: 0
                })),
            "{runs:?}"
        );
    }

    #[test]
    fn font_color_attribute_paints() {
        let (_text, runs) = flow_paints("<p><font color=\"navy\">hi</font></p>");
        assert!(
            runs.iter()
                .any(|r| r.paint.color == Some(CssColor { r: 0, g: 0, b: 128 })),
            "{runs:?}"
        );
    }

    #[test]
    fn same_block_style_tag_class_paints() {
        let (text, runs) = flow_paints(
            "<div><style>.hi { color: #00ff00; font-style: italic; }</style><span class=\"hi\">go</span></div>",
        );
        assert!(text.contains("go"), "{text:?}");
        assert!(
            !text.contains("font-style"),
            "CSS source must not paint, {text:?}"
        );
        assert!(
            runs.iter()
                .any(|r| r.paint.italic && r.paint.color == Some(CssColor { r: 0, g: 255, b: 0 })),
            "{runs:?}"
        );
    }

    #[test]
    fn style_url_background_is_ignored() {
        let (_text, runs) = flow_paints("<p style=\"background: url(javascript:alert(1))\">x</p>");
        assert!(
            runs.iter().all(|r| r.paint.background.is_none()),
            "{runs:?}"
        );
    }

    #[test]
    fn svg_block_paints_as_image_data_url() {
        match project_html_block(TINY_SVG) {
            HtmlBlockVisual::Image { url, alt } => {
                assert!(
                    url.starts_with("data:image/svg+xml"),
                    "svg must paint as a data URL, got {url}"
                );
                assert_eq!(alt, "dot");
                assert!(html_inline_image(TINY_SVG).is_some());
                assert!(!opaque_inline_is_caret_chrome(TINY_SVG));
            }
            other => panic!("expected svg image, got {other:?}"),
        }
    }

    #[test]
    fn svg_with_script_is_not_an_image() {
        let raw = "<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>";
        match project_html_block(raw) {
            HtmlBlockVisual::Image { .. } => panic!("scripted svg must not paint as an image"),
            HtmlBlockVisual::Hidden | HtmlBlockVisual::Flow { .. } => {}
            other => panic!("{other:?}"),
        }
        assert!(html_inline_svg(raw).is_none());
    }

    #[test]
    fn svg_validator_rejects_event_handlers_and_external_resources() {
        for raw in [
            "<svg onload = \"alert(1)\"></svg>",
            "<svg><image href=\"https://tracker.example/pixel.png\"/></svg>",
            "<svg><use href=\"https://tracker.example/icon.svg#x\"/></svg>",
            "<svg><rect fill=\"url(https://tracker.example/pattern.svg)\"/></svg>",
            "<svg><foreignObject><p>HTML</p></foreignObject></svg>",
            "<svg><style>@import url(https://tracker.example/style.css)</style></svg>",
        ] {
            assert!(
                !is_safe_svg_document(raw),
                "unsafe SVG must not reach the decoder: {raw}"
            );
        }
    }

    #[test]
    fn svg_validator_allows_local_paint_references() {
        let raw = concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">",
            "<defs><linearGradient id=\"g\"><stop offset=\"0\"/></linearGradient></defs>",
            "<rect fill=\"url(#g)\"/></svg>"
        );
        assert!(is_safe_svg_document(raw));
        assert!(html_inline_svg(raw).is_some());
    }

    #[test]
    fn classify_inline_svg_is_an_image() {
        let mut stack = HtmlStack::default();
        match classify_opaque_inline(TINY_SVG, &mut stack) {
            InlineHtmlAction::Image { url, alt } => {
                assert!(url.starts_with("data:image/svg+xml"));
                assert_eq!(alt, "dot");
            }
            other => panic!("{other:?}"),
        }
    }
}
