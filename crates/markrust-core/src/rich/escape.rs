// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Context-sensitive markdown escaping for serialized text.
//!
//! Plain backslash escaping is safe here because comrak implements the
//! CommonMark spec, whose ASCII-punctuation class includes `\` — so `\*`
//! round-trips through our own parser. (The `&#42;` NCR trick nimbalyst
//! uses exists only to dodge a Lexical scanner bug; see
//! FORKED_MARKDOWN_IMPORT.md in their repo.)

/// Where the text is being emitted; controls which characters are active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscapeContext {
    /// Escaping `|` is only needed inside table cells.
    pub in_table: bool,
    /// True when the emission point is at the start of a line (block-level
    /// starters like `#`, `-`, `>` are only active there).
    pub at_line_start: bool,
}

/// Escape `text` so it round-trips as literal text through the parser.
pub fn escape_text(text: &str, ctx: EscapeContext) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut at_line_start = ctx.at_line_start;
    let mut i = 0;
    while i < text.len() {
        let ch = text[i..].chars().next().unwrap();
        let ch_len = ch.len_utf8();
        let escaped = match ch {
            '\\' | '*' | '_' | '`' | '[' | ']' | '<' => true,
            '|' if ctx.in_table => true,
            '~' if peek(bytes, i + 1) == Some(b'~') => true,
            '#' | '>' | '+' | '-' if at_line_start => true,
            '=' if at_line_start => true,
            '.' | ')' if follows_line_start_digits(text, i, at_line_start_offset(text, i, ctx)) => {
                true
            }
            '!' if peek(bytes, i + 1) == Some(b'[') => true,
            '&' if looks_like_entity(&text[i..]) => true,
            _ => false,
        };
        if escaped {
            out.push('\\');
        }
        out.push(ch);
        at_line_start = ch == '\n';
        i += ch_len;
    }
    out
}

fn peek(bytes: &[u8], i: usize) -> Option<u8> {
    bytes.get(i).copied()
}

/// Byte offset of the current line's start within `text` (or 0 with the
/// caller's line-start flag deciding activity for offset 0).
fn at_line_start_offset(text: &str, i: usize, ctx: EscapeContext) -> Option<usize> {
    match text[..i].rfind('\n') {
        Some(nl) => Some(nl + 1),
        None => ctx.at_line_start.then_some(0),
    }
}

/// True when `.` or `)` at byte `i` terminates a line-initial digit run
/// (which would otherwise parse as an ordered-list marker).
fn follows_line_start_digits(text: &str, i: usize, line_start: Option<usize>) -> bool {
    let Some(start) = line_start else {
        return false;
    };
    let prefix = &text[start..i];
    !prefix.is_empty() && prefix.len() <= 9 && prefix.bytes().all(|b| b.is_ascii_digit())
}

/// Rough entity detection: `&name;` / `&#123;` / `&#xAB;`.
fn looks_like_entity(rest: &str) -> bool {
    let Some(body) = rest.strip_prefix('&') else {
        return false;
    };
    let Some(end) = body.find(';') else {
        return false;
    };
    let name = &body[..end];
    if name.is_empty() || end > 32 {
        return false;
    }
    if let Some(num) = name.strip_prefix('#') {
        let num = num.strip_prefix(['x', 'X']).unwrap_or(num);
        !num.is_empty() && num.bytes().all(|b| b.is_ascii_hexdigit())
    } else {
        name.bytes().all(|b| b.is_ascii_alphanumeric())
    }
}

/// Wrap inline-code content in enough backticks, extending the run and
/// padding with spaces when the literal itself contains backticks (the
/// `addBackticks` algorithm from tui.editor).
pub fn wrap_inline_code(literal: &str, preferred_backticks: usize) -> String {
    let mut longest_run = 0usize;
    let mut current = 0usize;
    for b in literal.bytes() {
        if b == b'`' {
            current += 1;
            longest_run = longest_run.max(current);
        } else {
            current = 0;
        }
    }
    let ticks = "`".repeat(preferred_backticks.max(longest_run + usize::from(longest_run > 0)));
    let needs_pad = literal.starts_with('`')
        || literal.ends_with('`')
        || (literal.starts_with(' ') && literal.ends_with(' ') && !literal.trim().is_empty());
    if needs_pad {
        format!("{ticks} {literal} {ticks}")
    } else {
        format!("{ticks}{literal}{ticks}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: EscapeContext = EscapeContext {
        in_table: false,
        at_line_start: true,
    };

    #[test]
    fn escapes_emphasis_and_backslash() {
        assert_eq!(escape_text("a*b_c\\d", BODY), "a\\*b\\_c\\\\d");
    }

    #[test]
    fn line_start_only_characters() {
        assert_eq!(escape_text("# not heading", BODY), "\\# not heading");
        let mid = EscapeContext {
            in_table: false,
            at_line_start: false,
        };
        assert_eq!(escape_text("a # b", mid), "a # b");
    }

    #[test]
    fn ordered_marker_dot_escaped_only_after_line_start_digits() {
        assert_eq!(escape_text("1. item", BODY), "1\\. item");
        assert_eq!(escape_text("v1. fine", BODY), "v1. fine");
    }

    #[test]
    fn pipe_only_in_tables() {
        assert_eq!(escape_text("a|b", BODY), "a|b");
        let table = EscapeContext {
            in_table: true,
            at_line_start: false,
        };
        assert_eq!(escape_text("a|b", table), "a\\|b");
    }

    #[test]
    fn inline_code_backtick_extension() {
        assert_eq!(wrap_inline_code("plain", 1), "`plain`");
        assert_eq!(wrap_inline_code("has ` tick", 1), "``has ` tick``");
        assert_eq!(wrap_inline_code("`starts", 1), "`` `starts ``");
    }
}
