// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Typora-style input rules. Pure functions: given the source, caret, and the
//! just-typed text, either insert the characters as markdown (unescaped) or
//! rewrite a prefix into a block/mark construct. Disabled in code / raw
//! contexts by the caller.

use std::ops::Range;

/// How an input rule should splice the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputRule {
    /// Insert `text` at the caret without markdown escaping (typing coalesces).
    InsertRaw(String),
    /// Replace an existing prefix with `insert`. One Command undo group;
    /// the caller may absorb a preceding Typing transaction that produced
    /// `range`.
    Replace {
        range: Range<usize>,
        insert: String,
        caret: usize,
    },
}

/// Match a rule for `typed` at a collapsed `caret`. Returns `None` when the
/// caller should fall back to context-sensitive escaping.
pub fn match_input_rule(
    source: &str,
    caret: usize,
    typed: &str,
    in_raw: bool,
) -> Option<InputRule> {
    if in_raw || typed.is_empty() || typed.contains('\n') {
        return None;
    }
    let caret = caret.min(source.len());
    if let Some(rule) = match_fence_or_break(source, caret, typed) {
        return Some(rule);
    }
    if let Some(rule) = match_block_space(source, caret, typed) {
        return Some(rule);
    }
    if let Some(rule) = match_auto_close(source, caret, typed) {
        return Some(rule);
    }
    match_pending_opener(source, caret, typed)
}

fn match_block_space(source: &str, caret: usize, typed: &str) -> Option<InputRule> {
    if typed != " " {
        return None;
    }
    let start = line_start(source, caret);
    let prefix = &source[start..caret];
    if prefix.is_empty() {
        return None;
    }
    let marker = unescape_ascii(prefix);
    let heading = marker.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&heading) && marker.len() == heading {
        return Some(unescaped_or_replace(
            start,
            prefix,
            "#".repeat(heading) + " ",
            caret,
        ));
    }
    if matches!(marker.as_str(), "-" | "*" | "+") {
        return Some(unescaped_or_replace(
            start,
            prefix,
            format!("{marker} "),
            caret,
        ));
    }
    if marker == ">" {
        return Some(unescaped_or_replace(start, prefix, "> ".into(), caret));
    }
    if ordered_marker(&marker).is_some() {
        return Some(unescaped_or_replace(
            start,
            prefix,
            format!("{marker} "),
            caret,
        ));
    }
    None
}

fn match_fence_or_break(source: &str, caret: usize, typed: &str) -> Option<InputRule> {
    let start = line_start(source, caret);
    if !at_line_start(source, start) {
        return None;
    }
    if !line_is_only_prefix(source, caret) {
        return None;
    }
    let prefix = &source[start..caret];
    let marker = unescape_ascii(prefix);

    if typed == "`" && marker == "``" {
        return Some(InputRule::Replace {
            range: start..caret,
            insert: "```\n\n```".into(),
            caret: start + 4,
        });
    }
    if typed == "~" && marker == "~~" {
        return Some(InputRule::Replace {
            range: start..caret,
            insert: "~~~\n\n~~~".into(),
            caret: start + 4,
        });
    }
    if typed == "-" && marker == "--" {
        return Some(InputRule::Replace {
            range: start..caret,
            insert: "---\n\n".into(),
            caret: start + 5,
        });
    }
    if typed == "*" && marker == "**" {
        return Some(InputRule::Replace {
            range: start..caret,
            insert: "***\n\n".into(),
            caret: start + 5,
        });
    }
    if typed == "_" && marker == "__" {
        return Some(InputRule::Replace {
            range: start..caret,
            insert: "___\n\n".into(),
            caret: start + 5,
        });
    }
    None
}

fn match_auto_close(source: &str, caret: usize, typed: &str) -> Option<InputRule> {
    let typed_b = typed.as_bytes();
    if typed_b.len() != 1 {
        return None;
    }
    let ch = typed_b[0];
    if !matches!(ch, b'*' | b'_' | b'`' | b'~') {
        return None;
    }
    let line0 = line_start(source, caret);
    let before = &source[line0..caret];
    let prev = before.as_bytes().last().copied();

    // Completing `**` / `~~` takes precedence over a single-char closer.
    if ch == b'*' && prev == Some(b'*') {
        return close_pair(source, line0, caret, "**", "*");
    }
    if ch == b'~' && prev == Some(b'~') {
        return close_pair(source, line0, caret, "~~", "~");
    }
    if ch == b'*' || ch == b'_' || ch == b'`' {
        let delim = typed;
        return close_pair(source, line0, caret, delim, typed);
    }
    if ch == b'~' {
        return close_pair(source, line0, caret, "~~", typed);
    }
    None
}

fn close_pair(
    source: &str,
    line0: usize,
    caret: usize,
    opener: &str,
    typed: &str,
) -> Option<InputRule> {
    let search = &source[line0..caret];
    let (abs, opener_len) = last_unescaped_opener(search, opener)?;
    let abs = line0 + abs;
    let content = &source[abs + opener_len..caret];
    if content.is_empty() || content.contains('\n') {
        return None;
    }
    let opener_slice = &source[abs..abs + opener_len];
    if opener_slice == opener {
        return Some(InputRule::InsertRaw(typed.to_string()));
    }
    let mut insert = String::with_capacity(opener.len() + content.len() + typed.len());
    insert.push_str(opener);
    insert.push_str(content);
    insert.push_str(typed);
    Some(InputRule::Replace {
        range: abs..caret,
        insert: insert.clone(),
        caret: abs + insert.len(),
    })
}

fn match_pending_opener(source: &str, caret: usize, typed: &str) -> Option<InputRule> {
    if typed.len() != 1 {
        return None;
    }
    let ch = typed.as_bytes()[0];
    let at_start = at_line_start(source, caret);
    let left_flank = at_start || prev_is_ws(source, caret);

    match ch {
        b'#' | b'>' | b'+' | b'-' if at_start => Some(InputRule::InsertRaw(typed.to_string())),
        b'*' | b'_' | b'`' | b'~' if left_flank => Some(InputRule::InsertRaw(typed.to_string())),
        b'.' | b')' if at_start || follows_line_start_digits(source, caret) => {
            Some(InputRule::InsertRaw(typed.to_string()))
        }
        b if b.is_ascii_digit() && at_start => Some(InputRule::InsertRaw(typed.to_string())),
        _ => None,
    }
}

fn unescaped_or_replace(start: usize, prefix: &str, desired: String, caret: usize) -> InputRule {
    let already = unescape_ascii(prefix);
    let desired_marker = desired.trim_end();
    if prefix == desired_marker || already == desired_marker && !prefix.contains('\\') {
        InputRule::InsertRaw(" ".into())
    } else {
        InputRule::Replace {
            range: start..caret,
            insert: desired.clone(),
            caret: start + desired.len(),
        }
    }
}

fn line_start(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0)
}

fn at_line_start(source: &str, offset: usize) -> bool {
    offset == 0 || source.as_bytes().get(offset.saturating_sub(1)) == Some(&b'\n')
}

fn line_is_only_prefix(source: &str, caret: usize) -> bool {
    let rest_end = source[caret.min(source.len())..]
        .find('\n')
        .map(|i| caret + i)
        .unwrap_or(source.len());
    source[caret.min(source.len())..rest_end]
        .chars()
        .all(|c| c == ' ' || c == '\t')
}

fn prev_is_ws(source: &str, caret: usize) -> bool {
    if caret == 0 {
        return true;
    }
    source[..caret]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_whitespace())
}

fn follows_line_start_digits(source: &str, caret: usize) -> bool {
    let start = line_start(source, caret);
    let prefix = &source[start..caret];
    !prefix.is_empty() && prefix.len() <= 9 && prefix.bytes().all(|b| b.is_ascii_digit())
}

fn ordered_marker(marker: &str) -> Option<()> {
    let end = marker.find(['.', ')'])?;
    if end == 0 || end != marker.len() - 1 {
        return None;
    }
    marker[..end]
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then_some(())
}

fn unescape_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_punctuation() {
            out.push(bytes[i + 1] as char);
            i += 2;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Last opener in `search`. Returns (byte index, source length of the opener
/// including a leading backslash when escaped).
fn last_unescaped_opener(search: &str, opener: &str) -> Option<(usize, usize)> {
    let escaped = match opener {
        "**" => "\\*\\*",
        "~~" => "\\~\\~",
        "*" => "\\*",
        "_" => "\\_",
        "`" => "\\`",
        "~" => "\\~",
        _ => "",
    };
    if !escaped.is_empty() {
        if let Some(i) = search.rfind(escaped) {
            let unesc = rfind_bare_opener(search, opener);
            if unesc.is_none_or(|u| i >= u) {
                return Some((i, escaped.len()));
            }
        }
    }
    rfind_bare_opener(search, opener).map(|i| (i, opener.len()))
}

fn rfind_bare_opener(search: &str, opener: &str) -> Option<usize> {
    let bytes = search.as_bytes();
    let needle = opener.as_bytes();
    if needle.is_empty() || bytes.len() < needle.len() {
        return None;
    }
    let mut i = bytes.len() - needle.len();
    loop {
        if bytes[i..].starts_with(needle) {
            let escaped = i > 0 && bytes[i - 1] == b'\\';
            let inside_double_star = opener == "*"
                && ((i > 0 && bytes[i - 1] == b'*' && (i < 2 || bytes[i - 2] != b'\\'))
                    || bytes.get(i + 1) == Some(&b'*'));
            if !escaped && !inside_double_star {
                return Some(i);
            }
        }
        if i == 0 {
            return None;
        }
        i -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(source: &str, caret: usize, typed: &str) -> (String, usize) {
        match match_input_rule(source, caret, typed, false) {
            Some(InputRule::InsertRaw(s)) => {
                let mut out = source.to_string();
                out.insert_str(caret, &s);
                (out, caret + s.len())
            }
            Some(InputRule::Replace {
                range,
                insert,
                caret,
            }) => {
                let mut out = source.to_string();
                out.replace_range(range, &insert);
                (out, caret)
            }
            None => {
                let mut out = source.to_string();
                out.insert_str(caret, typed);
                (out, caret + typed.len())
            }
        }
    }

    #[test]
    fn heading_space_completes_atx() {
        let (out, caret) = apply("#", 1, " ");
        assert_eq!(out, "# ");
        assert_eq!(caret, 2);
    }

    #[test]
    fn heading_hashes_are_pending_openers() {
        assert!(matches!(
            match_input_rule("", 0, "#", false),
            Some(InputRule::InsertRaw(s)) if s == "#"
        ));
        let (out, _) = apply("#", 1, "#");
        assert_eq!(out, "##");
    }

    #[test]
    fn bullet_and_quote_and_ordered() {
        assert_eq!(apply("-", 1, " ").0, "- ");
        assert_eq!(apply(">", 1, " ").0, "> ");
        assert_eq!(apply("1.", 2, " ").0, "1. ");
        assert_eq!(apply("1)", 2, " ").0, "1) ");
    }

    #[test]
    fn fence_inserts_closing_fence() {
        let (out, caret) = apply("``", 2, "`");
        assert_eq!(out, "```\n\n```");
        assert_eq!(caret, 4);
        assert_eq!(&out[caret..], "\n```");
    }

    #[test]
    fn thematic_break_on_three_dashes() {
        let (out, caret) = apply("--", 2, "-");
        assert_eq!(out, "---\n\n");
        assert_eq!(caret, 5);
    }

    #[test]
    fn auto_close_italic_inserts_raw_closer() {
        let rule = match_input_rule("*hello", 6, "*", false);
        assert!(
            matches!(rule, Some(InputRule::InsertRaw(ref s)) if s == "*"),
            "{rule:?}"
        );
    }

    #[test]
    fn auto_close_unescapes_opener() {
        let rule = match_input_rule("\\*hello", 7, "*", false);
        match rule {
            Some(InputRule::Replace { insert, .. }) => assert_eq!(insert, "*hello*"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn auto_close_code_and_strike() {
        assert!(matches!(
            match_input_rule("`code", 5, "`", false),
            Some(InputRule::InsertRaw(s)) if s == "`"
        ));
        assert!(matches!(
            match_input_rule("~~hi", 4, "~", false),
            Some(InputRule::InsertRaw(s)) if s == "~"
        ));
    }

    #[test]
    fn disabled_in_raw_context() {
        assert!(match_input_rule("#", 1, " ", true).is_none());
        assert!(match_input_rule("*hello", 6, "*", true).is_none());
    }

    #[test]
    fn mid_word_star_is_not_an_opener() {
        assert!(match_input_rule("hello", 3, "*", false).is_none());
    }

    #[test]
    fn space_after_hash_on_a_paragraph_is_a_heading() {
        let rule = match_input_rule("#hello", 1, " ", false);
        assert!(
            matches!(rule, Some(InputRule::InsertRaw(ref s)) if s == " "),
            "{rule:?}"
        );
        assert!(match_input_rule("hello", 5, " ", false).is_none());
    }

    #[test]
    fn escaped_hash_space_rewrites() {
        let rule = match_input_rule("\\#", 2, " ", false);
        match rule {
            Some(InputRule::Replace { insert, .. }) => assert_eq!(insert, "# "),
            other => panic!("{other:?}"),
        }
    }
}
