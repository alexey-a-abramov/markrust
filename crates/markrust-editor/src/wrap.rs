// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Source-mode wrap / indent helpers. Pure byte splices so delimiter masking
//! still sees a real syntax span after Cmd/Ctrl+B/I/K.

use std::ops::Range;

/// Inline wrap applied to a source selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapKind {
    Bold,
    Italic,
    Code,
    Link,
}

/// Result of a wrap/indent rewrite: replace `range` with `text`, then set
/// the selection to `selection` (document offsets after the splice).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapEdit {
    pub range: Range<usize>,
    pub text: String,
    pub selection: Range<usize>,
}

impl WrapKind {
    fn delimiters(self) -> (&'static str, &'static str) {
        match self {
            WrapKind::Bold => ("**", "**"),
            WrapKind::Italic => ("*", "*"),
            WrapKind::Code => ("`", "`"),
            WrapKind::Link => ("[", "]()"),
        }
    }
}

/// Wrap or unwrap `sel` in `source` according to `kind`.
pub fn wrap_selection(source: &str, sel: Range<usize>, kind: WrapKind) -> WrapEdit {
    let sel = clamp_range(source, sel);
    match kind {
        WrapKind::Link => wrap_link(source, sel),
        other => wrap_pair(source, sel, other),
    }
}

fn wrap_pair(source: &str, sel: Range<usize>, kind: WrapKind) -> WrapEdit {
    let (open, close) = kind.delimiters();
    if let Some(full) = existing_wrap(source, &sel, open, close, kind) {
        let inner_start = full.start + open.len();
        let inner_end = full.end.saturating_sub(close.len());
        let inner = source.get(inner_start..inner_end).unwrap_or("").to_string();
        return WrapEdit {
            range: full.clone(),
            text: inner.clone(),
            selection: full.start..full.start + inner.len(),
        };
    }
    if sel.is_empty() {
        let text = format!("{open}{close}");
        let caret = sel.start + open.len();
        return WrapEdit {
            range: sel.start..sel.start,
            text,
            selection: caret..caret,
        };
    }
    let inner = source.get(sel.clone()).unwrap_or("");
    let text = format!("{open}{inner}{close}");
    let inner_start = sel.start + open.len();
    WrapEdit {
        range: sel.clone(),
        text,
        selection: inner_start..inner_start + inner.len(),
    }
}

fn wrap_link(source: &str, sel: Range<usize>) -> WrapEdit {
    if let Some((full, text)) = existing_link(source, &sel) {
        return WrapEdit {
            range: full.clone(),
            text: text.clone(),
            selection: full.start..full.start + text.len(),
        };
    }
    if sel.is_empty() {
        let text = "[]()".to_string();
        let caret = sel.start + 1;
        return WrapEdit {
            range: sel.start..sel.start,
            text,
            selection: caret..caret,
        };
    }
    let inner = source.get(sel.clone()).unwrap_or("");
    let text = format!("[{inner}]()");
    let url_at = sel.start + 1 + inner.len() + 2;
    WrapEdit {
        range: sel,
        text,
        selection: url_at..url_at,
    }
}

fn existing_wrap(
    source: &str,
    sel: &Range<usize>,
    open: &str,
    close: &str,
    kind: WrapKind,
) -> Option<Range<usize>> {
    let inner = source.get(sel.clone()).unwrap_or("");
    if inner.len() >= open.len() + close.len()
        && inner.starts_with(open)
        && inner.ends_with(close)
        && !italic_is_bold_pair(inner, kind)
    {
        return Some(sel.clone());
    }
    if sel.start >= open.len() && sel.end + close.len() <= source.len() {
        let before = &source[sel.start - open.len()..sel.start];
        let after = &source[sel.end..sel.end + close.len()];
        if before == open && after == close && wrapping_is_kind(source, sel, open, close, kind) {
            return Some(sel.start - open.len()..sel.end + close.len());
        }
    }
    None
}

fn italic_is_bold_pair(inner: &str, kind: WrapKind) -> bool {
    kind == WrapKind::Italic && inner.starts_with("**") && inner.ends_with("**")
}

fn wrapping_is_kind(
    source: &str,
    sel: &Range<usize>,
    open: &str,
    close: &str,
    kind: WrapKind,
) -> bool {
    if kind != WrapKind::Italic || open != "*" || close != "*" {
        return true;
    }
    let star_before = sel.start >= 2 && source.as_bytes()[sel.start - 2] == b'*';
    let star_after = sel.end + 1 < source.len() && source.as_bytes()[sel.end + 1] == b'*';
    !star_before && !star_after
}

fn existing_link(source: &str, sel: &Range<usize>) -> Option<(Range<usize>, String)> {
    let start = source
        .get(..sel.start)
        .and_then(|prefix| prefix.rfind('['))?;
    if source.as_bytes().get(start.saturating_sub(1)) == Some(&b'!') {
        return None;
    }
    let after_text = source.get(start + 1..)?;
    let bracket = after_text.find(']')?;
    let text_end = start + 1 + bracket;
    if !source[text_end..].starts_with("](") {
        return None;
    }
    let url_start = text_end + 2;
    let rest = source.get(url_start..)?;
    let paren = rest.find(')')?;
    let full_end = url_start + paren + 1;
    if sel.start < start || sel.end > full_end {
        return None;
    }
    let text = source[start + 1..text_end].to_string();
    Some((start..full_end, text))
}

/// Indent every source line that overlaps `sel` by two spaces.
///
/// Quoted lines (`> …`) get the spaces after the `>` markers so Tab never
/// produces a leading space before the quote (`  > item`).
pub fn indent_selection(source: &str, sel: Range<usize>) -> WrapEdit {
    let sel = clamp_range(source, sel);
    let block = line_block(source, &sel);
    let slice = source.get(block.clone()).unwrap_or("");
    let text = prefix_lines(slice, "  ");
    WrapEdit {
        range: block.clone(),
        text,
        selection: sel.start.saturating_add(2)..sel.end.saturating_add(2),
    }
}

/// Remove up to two indent spaces (or one tab) from each line in `sel`.
/// Quoted lines lose spaces after the `>` markers (the reverse of
/// [`indent_selection`]), never the `>` itself or a space before `>`.
/// Top-level list markers with no indent become a paragraph.
pub fn outdent_selection(source: &str, sel: Range<usize>) -> Option<WrapEdit> {
    let sel = clamp_range(source, sel);
    let block = line_block(source, &sel);
    let slice = source.get(block.clone()).unwrap_or("");
    let first = slice.split('\n').next().unwrap_or(slice);
    let indent = leading_indent_width(first);
    let text = if indent >= 2 {
        unprefix_lines(slice, 2)
    } else if indent == 1 {
        unprefix_lines(slice, 1)
    } else {
        strip_list_marker_line(slice)?
    };
    if text == slice {
        return None;
    }
    let shrink = slice.len().saturating_sub(text.len());
    let start_rel = sel.start.saturating_sub(block.start);
    let start_shrink = indent.max(1).min(start_rel);
    let new_start = sel.start.saturating_sub(start_shrink);
    let new_end = if sel.end == sel.start {
        new_start
    } else {
        sel.end.saturating_sub(shrink.min(sel.end - sel.start))
    };
    Some(WrapEdit {
        range: block,
        text,
        selection: new_start.min(new_end)..new_end.max(new_start),
    })
}

fn prefix_lines(slice: &str, prefix: &str) -> String {
    let trailing_nl = slice.ends_with('\n');
    let body = if trailing_nl {
        &slice[..slice.len() - 1]
    } else {
        slice
    };
    let mut out = String::with_capacity(slice.len() + prefix.len() * 4);
    for (i, line) in body.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !line.is_empty() {
            let quote = quote_prefix(line);
            out.push_str(quote);
            out.push_str(prefix);
            out.push_str(&line[quote.len()..]);
        } else {
            out.push_str(line);
        }
    }
    if trailing_nl {
        out.push('\n');
    }
    out
}

fn unprefix_lines(slice: &str, n: usize) -> String {
    let trailing_nl = slice.ends_with('\n');
    let mut out = String::with_capacity(slice.len());
    for (i, line) in slice.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&strip_indent(line, n));
    }
    if trailing_nl && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn strip_indent(line: &str, n: usize) -> String {
    let quote = quote_prefix(line);
    if !quote.is_empty() {
        return format!("{quote}{}", strip_spaces(&line[quote.len()..], n));
    }
    if spaces_before_quote(line) {
        return line.to_string();
    }
    strip_spaces(line, n)
}

fn strip_spaces(line: &str, n: usize) -> String {
    if let Some(rest) = line.strip_prefix('\t') {
        return rest.to_string();
    }
    let mut take = 0usize;
    for (i, b) in line.bytes().enumerate() {
        if b == b' ' && i < n {
            take += 1;
        } else {
            break;
        }
    }
    line[take..].to_string()
}

/// `  > item` is not source-mode indent; Tab puts spaces after `>`.
fn spaces_before_quote(line: &str) -> bool {
    let n = line
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    n > 0 && line[n..].starts_with('>')
}

fn strip_list_marker_line(slice: &str) -> Option<String> {
    let trailing_nl = slice.ends_with('\n');
    let first_end = slice.find('\n').unwrap_or(slice.len());
    let first = &slice[..first_end];
    let prefix_len = list_marker_width(first);
    if prefix_len == 0 {
        return None;
    }
    let mut out = first[prefix_len.min(first.len())..].to_string();
    if first_end < slice.len() {
        out.push_str(&slice[first_end..]);
    }
    if trailing_nl && !out.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

fn list_marker_width(line: &str) -> usize {
    let indent = line
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    let rest = &line[indent..];
    let marker = if rest.starts_with(['-', '*', '+']) && rest.as_bytes().get(1) == Some(&b' ') {
        2
    } else if let Some(end) = rest.find(['.', ')']) {
        if !rest[..end].is_empty() && rest[..end].bytes().all(|b| b.is_ascii_digit()) {
            end + 2
        } else {
            0
        }
    } else {
        0
    };
    if marker == 0 {
        return 0;
    }
    let mut take = indent + marker;
    let after = &line.get(take..).unwrap_or("");
    if after.starts_with("[ ] ") || after.starts_with("[x] ") || after.starts_with("[X] ") {
        take += 4;
    }
    take.min(line.len())
}

/// Leading `>` markers (optional space after each), so indent lands inside the quote.
fn quote_prefix(line: &str) -> &str {
    let bytes = line.as_bytes();
    if bytes.first() != Some(&b'>') {
        return "";
    }
    let mut i = 0;
    while bytes.get(i) == Some(&b'>') {
        i += 1;
        if bytes.get(i) == Some(&b' ') || bytes.get(i) == Some(&b'\t') {
            i += 1;
        }
    }
    &line[..i]
}

fn leading_indent_width(line: &str) -> usize {
    let quote = quote_prefix(line);
    let after = &line[quote.len()..];
    if quote.is_empty() && spaces_before_quote(line) {
        return 0;
    }
    if after.starts_with('\t') {
        return 2;
    }
    after.bytes().take_while(|b| *b == b' ').count()
}

fn line_block(source: &str, sel: &Range<usize>) -> Range<usize> {
    let start = source[..sel.start.min(source.len())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let end = match source[sel.end.min(source.len())..].find('\n') {
        Some(i) => sel.end + i + 1,
        None => source.len(),
    };
    start..end
}

fn clamp_range(source: &str, sel: Range<usize>) -> Range<usize> {
    let start = sel.start.min(source.len());
    let end = sel.end.min(source.len());
    if start <= end {
        start..end
    } else {
        end..start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_bold_then_unwrap() {
        let edit = wrap_selection("hello world", 0..5, WrapKind::Bold);
        assert_eq!(edit.text, "**hello**");
        assert_eq!(edit.selection, 2..7);
        let wrapped = "**hello** world";
        let undo = wrap_selection(wrapped, 2..7, WrapKind::Bold);
        assert_eq!(undo.text, "hello");
        assert_eq!(undo.selection, 0..5);
    }

    #[test]
    fn wrap_italic_does_not_steal_bold() {
        let source = "**hello**";
        let edit = wrap_selection(source, 2..7, WrapKind::Italic);
        assert_eq!(edit.text, "*hello*");
        assert!(edit.range.start >= 2);
    }

    #[test]
    fn wrap_link_puts_caret_in_url() {
        let edit = wrap_selection("hello world", 0..5, WrapKind::Link);
        assert_eq!(edit.text, "[hello]()");
        assert_eq!(edit.selection.start, "[hello](".len());
        assert_eq!(edit.selection.end, edit.selection.start);
    }

    #[test]
    fn unwrap_link_restores_text() {
        let source = "[hello](https://e.com) world";
        let edit = wrap_selection(source, 1..6, WrapKind::Link);
        assert_eq!(edit.text, "hello");
    }

    #[test]
    fn collapsed_bold_inserts_pair() {
        let edit = wrap_selection("ab", 1..1, WrapKind::Bold);
        assert_eq!(edit.text, "****");
        assert_eq!(edit.selection, 3..3);
    }

    #[test]
    fn indent_and_outdent_list_item() {
        let indented = indent_selection("- hello", 2..2);
        assert!(
            indented.text.starts_with("  - hello"),
            "{:?}",
            indented.text
        );
        let out = outdent_selection("  - hello", 4..4).unwrap();
        assert!(out.text.starts_with("- hello"), "{:?}", out.text);
        let para = outdent_selection("- hello", 2..2).unwrap();
        assert_eq!(para.text.trim(), "hello");
    }

    #[test]
    fn indent_quoted_line_does_not_prefix_the_marker() {
        let quoted = indent_selection("> hello", 2..2);
        assert_eq!(quoted.text, ">   hello");
        assert!(
            !quoted.text.starts_with("  >"),
            "source indent must not put a space before >, got {:?}",
            quoted.text
        );
        let nested = indent_selection("> > - item", 4..4);
        assert_eq!(nested.text, "> >   - item");
        assert!(
            !nested.text.starts_with(' '),
            "nested quote indent must stay after >, got {:?}",
            nested.text
        );
        let list = indent_selection("- hello", 2..2);
        assert_eq!(list.text, "  - hello");
    }

    #[test]
    fn outdent_quoted_line_strips_indent_after_the_marker() {
        let quoted = indent_selection("> hello", 2..2);
        assert_eq!(quoted.text, ">   hello");
        let out = outdent_selection(&quoted.text, quoted.selection.clone()).unwrap();
        assert_eq!(out.text, "> hello");
        assert!(
            out.text.starts_with('>'),
            "source outdent must not eat >, got {:?}",
            out.text
        );
        assert!(
            !out.text.starts_with("  >") && !out.text.starts_with(" >"),
            "source outdent must not leave a space before >, got {:?}",
            out.text
        );

        let nested = indent_selection("> > - item", 4..4);
        assert_eq!(nested.text, "> >   - item");
        let nested_out = outdent_selection(&nested.text, nested.selection.clone()).unwrap();
        assert_eq!(nested_out.text, "> > - item");
        assert!(
            nested_out.text.starts_with("> >"),
            "nested quote markers must stay, got {:?}",
            nested_out.text
        );

        let list = indent_selection("> - item", 2..2);
        assert_eq!(list.text, ">   - item");
        let list_out = outdent_selection(&list.text, list.selection.clone()).unwrap();
        assert_eq!(list_out.text, "> - item");

        assert!(
            outdent_selection("  > hello", 2..2).is_none(),
            "spaces before > are not source-mode indent"
        );
        let no_indent = outdent_selection("> hello", 2..2);
        assert!(
            no_indent.is_none()
                || no_indent
                    .as_ref()
                    .is_some_and(|e| e.text.starts_with('>') && e.text.contains("hello")),
            "unindented quote must not eat >, got {no_indent:?}"
        );
    }
}
