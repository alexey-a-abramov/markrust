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
    match_input_rule_with(source, caret, typed, in_raw, false)
}

/// Like [`match_input_rule`], with `in_table` suppressing block-open / fence
/// rewrites that would smash a GFM row. Mark auto-close still runs unless the
/// splice would insert a newline or `|`.
pub fn match_input_rule_with(
    source: &str,
    caret: usize,
    typed: &str,
    in_raw: bool,
    in_table: bool,
) -> Option<InputRule> {
    if in_raw || typed.is_empty() || typed.contains('\n') {
        return None;
    }
    let caret = caret.min(source.len());
    if !in_table {
        if let Some(rule) = match_fence_or_break(source, caret, typed) {
            return Some(rule);
        }
        if let Some(rule) = match_block_space(source, caret, typed) {
            return Some(rule);
        }
    }
    if let Some(rule) = match_auto_close(source, caret, typed) {
        if !(in_table && input_rule_breaks_table(&rule)) {
            return Some(rule);
        }
    }
    match_pending_opener(source, caret, typed, in_table)
        .filter(|rule| !(in_table && input_rule_breaks_table(rule)))
}

/// True when applying `rule` inside a GFM table cell would rewrite the row
/// (heading / list / quote / fence) or splice a newline or `|`.
pub fn input_rule_breaks_table(rule: &InputRule) -> bool {
    let insert = match rule {
        InputRule::InsertRaw(text) => text.as_str(),
        InputRule::Replace { insert, .. } => insert.as_str(),
    };
    insert.contains('\n') || insert.contains('|') || is_block_marker_space(insert)
}

fn is_block_marker_space(insert: &str) -> bool {
    let Some(marker) = insert.strip_suffix(' ') else {
        return false;
    };
    if marker.is_empty() || marker.bytes().any(|b| b == b' ' || b == b'\t') {
        return false;
    }
    let hashes = marker.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && marker.len() == hashes {
        return true;
    }
    matches!(marker, "-" | "*" | "+" | ">") || ordered_marker(marker).is_some()
}

fn match_block_space(source: &str, caret: usize, typed: &str) -> Option<InputRule> {
    if typed != " " {
        return None;
    }
    let start = content_start(source, caret);
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
    let start = content_start(source, caret);
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

    // Completing a double delimiter takes precedence, even on the first
    // closer (`**hello` + `*` should insert a raw `*`).
    if ch == b'*' {
        if let Some(rule) = close_pair(source, line0, caret, "**", "*") {
            return Some(rule);
        }
    }
    if ch == b'_' {
        if let Some(rule) = close_pair(source, line0, caret, "__", "_") {
            return Some(rule);
        }
    }
    if ch == b'~' {
        if let Some(rule) = close_pair(source, line0, caret, "~~", "~") {
            return Some(rule);
        }
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

fn match_pending_opener(
    source: &str,
    caret: usize,
    typed: &str,
    in_table: bool,
) -> Option<InputRule> {
    if typed.len() != 1 {
        return None;
    }
    let ch = typed.as_bytes()[0];
    let at_start = caret == content_start(source, caret);
    let left_flank = at_start || prev_is_ws(source, caret);
    let continues = continues_opener(source, caret, ch);

    match ch {
        // Block openers stay escaped / literal in a cell so `# ` / `- ` / `> `
        // cannot rewrite the GFM row. `*` / `_` / `` ` `` / `~` at a physical
        // line start would start a list / fence / break (italic still
        // InsertRaw when not at `content_start`).
        b'#' | b'>' | b'+' | b'-' if at_start && !in_table => {
            Some(InputRule::InsertRaw(typed.to_string()))
        }
        b'*' | b'_' | b'`' | b'~' if in_table && at_start => None,
        b'*' | b'_' | b'`' | b'~' if left_flank || continues => {
            Some(InputRule::InsertRaw(typed.to_string()))
        }
        b'.' | b')' if in_table && follows_content_start_digits(source, caret) => {
            Some(InputRule::InsertRaw(format!("\\{typed}")))
        }
        b'.' | b')' if !in_table && (at_start || follows_content_start_digits(source, caret)) => {
            Some(InputRule::InsertRaw(typed.to_string()))
        }
        b if b.is_ascii_digit() && at_start && !in_table => {
            Some(InputRule::InsertRaw(typed.to_string()))
        }
        // Brackets and HTML would otherwise be backslash-escaped on every
        // keystroke, so task lists, links, images, and raw HTML could not
        // be typed in WYSIWYG.
        b'[' | b']' | b'<' => Some(InputRule::InsertRaw(typed.to_string())),
        _ => None,
    }
}

fn continues_opener(source: &str, caret: usize, ch: u8) -> bool {
    if caret == 0 || !matches!(ch, b'*' | b'_' | b'~') {
        return false;
    }
    let bytes = source.as_bytes();
    bytes.get(caret - 1) == Some(&ch) && (caret < 2 || bytes[caret - 2] != b'\\')
}

fn unescaped_or_replace(start: usize, _prefix: &str, desired: String, caret: usize) -> InputRule {
    let new_caret = start + desired.len();
    InputRule::Replace {
        range: start..caret,
        insert: desired,
        caret: new_caret,
    }
}

fn line_start(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// Byte offset where list/quote/indent prefixes end, so `# ` and fences work
/// inside a list item the same way they do at a physical line start.
fn content_start(source: &str, caret: usize) -> usize {
    let line0 = line_start(source, caret);
    let limit = caret.min(source.len());
    let bytes = source.as_bytes();
    let mut i = skip_ws(bytes, line0, limit);
    while i < limit && bytes[i] == b'>' {
        i += 1;
        if i < limit && bytes[i] == b' ' {
            i += 1;
        }
        i = skip_ws(bytes, i, limit);
    }
    if i < limit {
        if matches!(bytes[i], b'-' | b'*' | b'+')
            && bytes.get(i + 1) == Some(&b' ')
            && i + 2 <= limit
        {
            i += 2;
            i = skip_task_prefix(source, i, limit);
        } else if let Some(marker_end) = ordered_marker_end(bytes, i, limit) {
            i = marker_end;
            i = skip_task_prefix(source, i, limit);
        }
    }
    i.min(limit)
}

fn skip_ws(bytes: &[u8], mut i: usize, limit: usize) -> usize {
    while i < limit && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    i
}

fn skip_task_prefix(source: &str, i: usize, limit: usize) -> usize {
    if i + 4 <= limit {
        let slot = &source[i..i + 4];
        if slot == "[ ] " || slot == "[x] " || slot == "[X] " {
            return i + 4;
        }
    }
    i
}

fn ordered_marker_end(bytes: &[u8], start: usize, limit: usize) -> Option<usize> {
    let mut i = start;
    if i >= limit || !bytes[i].is_ascii_digit() {
        return None;
    }
    while i < limit && bytes[i].is_ascii_digit() && i - start < 9 {
        i += 1;
    }
    if i < limit
        && matches!(bytes[i], b'.' | b')')
        && bytes.get(i + 1) == Some(&b' ')
        && i + 2 <= limit
    {
        return Some(i + 2);
    }
    None
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

fn follows_content_start_digits(source: &str, caret: usize) -> bool {
    let start = content_start(source, caret);
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
        "__" => "\\_\\_",
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
            let inside_double = matches!(opener, "*" | "_")
                && ((i > 0
                    && bytes[i - 1] == opener.as_bytes()[0]
                    && (i < 2 || bytes[i - 2] != b'\\'))
                    || bytes.get(i + 1) == Some(&opener.as_bytes()[0]));
            if !escaped && !inside_double {
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
        match rule {
            Some(InputRule::Replace { insert, range, .. }) => {
                assert_eq!(insert, "# ");
                assert_eq!(range, 0..1);
            }
            other => panic!("{other:?}"),
        }
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

    #[test]
    fn underscore_italic_and_bold_auto_close() {
        assert!(matches!(
            match_input_rule("_hello", 6, "_", false),
            Some(InputRule::InsertRaw(s)) if s == "_"
        ));
        assert!(matches!(
            match_input_rule("__hello", 7, "_", false),
            Some(InputRule::InsertRaw(s)) if s == "_"
        ));
        let (out, _) = apply("", 0, "_");
        assert_eq!(out, "_");
        let (out, caret) = apply("_", 1, "_");
        assert_eq!(out, "__");
        assert_eq!(caret, 2);
    }

    #[test]
    fn nested_italic_inside_bold_closes_inner() {
        let rule = match_input_rule("**hello *world", 14, "*", false);
        assert!(
            matches!(rule, Some(InputRule::InsertRaw(ref s)) if s == "*"),
            "{rule:?}"
        );
    }

    #[test]
    fn heading_and_fence_inside_list_item() {
        let rule = match_input_rule("- #", 3, " ", false);
        assert!(
            matches!(rule, Some(InputRule::InsertRaw(ref s)) if s == " ")
                || matches!(rule, Some(InputRule::Replace { ref insert, .. }) if insert.ends_with("# ")),
            "{rule:?}"
        );
        let hash = match_input_rule("- ", 2, "#", false);
        assert!(
            matches!(hash, Some(InputRule::InsertRaw(ref s)) if s == "#"),
            "{hash:?}"
        );
        let (out, caret) = apply("- ``", 4, "`");
        assert!(out.contains("```"), "{out:?}");
        assert!(out.starts_with("- "), "{out:?}");
        assert!(out[caret..].contains("```"), "{out:?} caret={caret}");
    }

    #[test]
    fn heading_inside_blockquote() {
        let hash = match_input_rule("> ", 2, "#", false);
        assert!(
            matches!(hash, Some(InputRule::InsertRaw(ref s)) if s == "#"),
            "{hash:?}"
        );
    }

    #[test]
    fn brackets_and_lt_are_typed_raw() {
        assert!(matches!(
            match_input_rule("", 0, "[", false),
            Some(InputRule::InsertRaw(s)) if s == "["
        ));
        assert!(matches!(
            match_input_rule("[text", 5, "]", false),
            Some(InputRule::InsertRaw(s)) if s == "]"
        ));
        assert!(matches!(
            match_input_rule("", 0, "<", false),
            Some(InputRule::InsertRaw(s)) if s == "<"
        ));
        let (out, _) = apply("", 0, "[");
        assert_eq!(out, "[");
        let (out, _) = apply("[text", 5, "]");
        assert_eq!(out, "[text]");
        let (out, _) = apply("", 0, "<");
        assert_eq!(out, "<");
    }

    #[test]
    fn block_rules_do_not_match_inside_a_table() {
        assert!(match_input_rule_with("#", 1, " ", false, true).is_none());
        assert!(match_input_rule_with("-", 1, " ", false, true).is_none());
        assert!(match_input_rule_with("*", 1, " ", false, true).is_none());
        assert!(match_input_rule_with(">", 1, " ", false, true).is_none());
        assert!(match_input_rule_with("1.", 2, " ", false, true).is_none());
        assert!(match_input_rule_with("``", 2, "`", false, true).is_none());
        assert!(match_input_rule_with("--", 2, "-", false, true).is_none());
        assert!(
            matches!(
                match_input_rule_with("1", 1, ".", false, true),
                Some(InputRule::InsertRaw(ref s)) if s == "\\."
            ),
            "ordered-list `.` in a cell at line start must stay escaped"
        );
        assert!(
            matches!(
                match_input_rule_with("*hello", 6, "*", false, true),
                Some(InputRule::InsertRaw(ref s)) if s == "*"
            ),
            "italic auto-close must still run in a cell"
        );
        let across_pipe = match_input_rule_with("| *foo | bar", 12, "*", false, true);
        assert!(
            across_pipe.is_none()
                || match &across_pipe {
                    Some(InputRule::InsertRaw(s)) => !s.contains('|') && !s.contains('\n'),
                    Some(InputRule::Replace { insert, .. }) => {
                        !insert.contains('|') && !insert.contains('\n')
                    }
                    None => true,
                },
            "auto-close must not splice `|` across cells: {across_pipe:?}"
        );
    }

    #[test]
    fn input_rule_breaks_table_detects_block_open_and_newlines() {
        let heading = match_input_rule("#", 1, " ", false).expect("heading");
        assert!(input_rule_breaks_table(&heading));
        let fence = match_input_rule("``", 2, "`", false).expect("fence");
        assert!(input_rule_breaks_table(&fence));
        let italic = match_input_rule("*hello", 6, "*", false).expect("italic");
        assert!(!input_rule_breaks_table(&italic));
    }

    #[test]
    fn task_list_checkbox_can_be_typed() {
        let (out, caret) = apply("- ", 2, "[");
        assert_eq!(&out[..caret], "- [");
        let (out, caret) = apply(&out, caret, " ");
        let (out, caret) = apply(&out, caret, "]");
        let (out, _) = apply(&out, caret, " ");
        assert!(out.starts_with("- [ ] "), "{out:?}");
    }
}
