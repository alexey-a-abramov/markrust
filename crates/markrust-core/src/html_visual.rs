// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Safe visual projection of raw HTML and Typora extras that import as opaque.
//!
//! Does not execute script, apply CSS, or follow `javascript:` URLs. Simple
//! phrasing tags become marks; everything else is either hidden chrome or
//! inner text. The rope / [`crate::rich::RichTree`] stay unchanged so Preserve
//! identity is untouched.

use std::ops::Range;

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
    /// `<img src>` that is safe to hand to the image pipeline.
    Image { url: String, alt: String },
    /// `[^label]` footnote reference.
    FootnoteRef { label: String },
    /// Unknown opaque: show the source (math, junk, …).
    Raw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedTag {
    name: String,
    close: bool,
    self_closing: bool,
    attrs: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HtmlPiece {
    Tag(ParsedTag),
    Comment,
    OtherMarkup,
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
}

impl HtmlStack {
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
        bump(&mut self.code, name, &["code", "kbd", "samp", "tt", "var"], true);
        bump(&mut self.mark, name, &["mark"], true);
        bump(&mut self.sup, name, &["sup"], true);
        bump(&mut self.sub, name, &["sub", "small"], true);
        if name == "a" {
            if let Some(href) = attr(&tag.attrs, "href").and_then(safe_url) {
                self.hrefs.push(href);
            }
        }
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
        bump(&mut self.code, name, &["code", "kbd", "samp", "tt", "var"], false);
        bump(&mut self.mark, name, &["mark"], false);
        bump(&mut self.sup, name, &["sup"], false);
        bump(&mut self.sub, name, &["sub", "small"], false);
        if name == "a" {
            self.hrefs.pop();
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
pub fn opaque_inline_is_caret_chrome(raw: &str) -> bool {
    if footnote_ref_label(raw).is_some() {
        return false;
    }
    match parse_complete_piece(raw) {
        Some(HtmlPiece::Tag(tag)) if tag.name == "img" => false,
        Some(_) => true,
        None => false,
    }
}

/// Safe `<img src>` / `alt` from a complete html inline (or none).
pub fn html_inline_image(raw: &str) -> Option<(String, String)> {
    match parse_complete_piece(raw)? {
        HtmlPiece::Tag(tag) if tag.name == "img" => img_from_attrs(&tag.attrs),
        _ => None,
    }
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
    let mut stack = HtmlStack::default();
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

/// Map digits (and a few signs) to Unicode superscripts. `None` if any char
/// cannot be raised — callers keep the original glyphs.
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
        'n' | 'N' => 'ⁿ',
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
    if rest.starts_with("<!") || rest.starts_with("<?") {
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
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' && bytes[i] != b'/'
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
        assert!(opaque_inline_is_caret_chrome("<br>"));
        assert!(!opaque_inline_is_caret_chrome("<img src=\"a.png\" alt=\"x\">"));
        assert!(!opaque_inline_is_caret_chrome("[^1]"));
        assert!(!opaque_inline_is_caret_chrome("not html"));
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
    }

    #[test]
    fn html_block_script_is_hidden() {
        assert_eq!(
            project_html_block("<script>alert(1)</script>"),
            HtmlBlockVisual::Hidden
        );
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
        assert_eq!(to_superscript("1a"), None);
        assert_eq!(to_subscript("2").as_deref(), Some("₂"));
    }
}
