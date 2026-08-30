// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Modest GitHub/Typora emoji shortcode table (`:smile:` → 😄).
//!
//! Unmatched `:foo:` stays text. The on-disk form is always `:name:`.

use std::ops::Range;

use super::tree::{Inline, MarkSet};

/// `(alias, glyph)` sorted by alias for binary search. GitHub names only;
/// no custom octocat aliases, no vendored gemoji dump.
const EMOJI: &[(&str, &str)] = &[
    ("+1", "👍"),
    ("-1", "👎"),
    ("100", "💯"),
    ("angry", "😠"),
    ("balloon", "🎈"),
    ("beer", "🍺"),
    ("blush", "😊"),
    ("book", "📖"),
    ("boom", "💥"),
    ("broken_heart", "💔"),
    ("bug", "🐛"),
    ("bulb", "💡"),
    ("cat", "🐱"),
    ("clap", "👏"),
    ("coffee", "☕"),
    ("confused", "😕"),
    ("construction", "🚧"),
    ("cry", "😢"),
    ("disappointed", "😞"),
    ("dog", "🐶"),
    ("exclamation", "❗"),
    ("eyes", "👀"),
    ("fire", "🔥"),
    ("gift", "🎁"),
    ("grinning", "😀"),
    ("hankey", "💩"),
    ("heart", "❤️"),
    ("heart_eyes", "😍"),
    ("hearts", "💕"),
    ("heavy_check_mark", "✔️"),
    ("heavy_minus_sign", "➖"),
    ("heavy_plus_sign", "➕"),
    ("hugging_face", "🤗"),
    ("joy", "😂"),
    ("link", "🔗"),
    ("lock", "🔒"),
    ("memo", "📝"),
    ("muscle", "💪"),
    ("nerd_face", "🤓"),
    ("ok_hand", "👌"),
    ("pizza", "🍕"),
    ("poop", "💩"),
    ("pray", "🙏"),
    ("question", "❓"),
    ("raised_hands", "🙌"),
    ("rocket", "🚀"),
    ("scream", "😱"),
    ("see_no_evil", "🙈"),
    ("shit", "💩"),
    ("sleeping", "😴"),
    ("slightly_smiling_face", "🙂"),
    ("smile", "😄"),
    ("smiley", "😃"),
    ("smirk", "😏"),
    ("sob", "😭"),
    ("sparkles", "✨"),
    ("star", "⭐"),
    ("stuck_out_tongue", "😛"),
    ("sunglasses", "😎"),
    ("sweat_smile", "😅"),
    ("tada", "🎉"),
    ("thinking", "🤔"),
    ("thumbsdown", "👎"),
    ("thumbsup", "👍"),
    ("warning", "⚠️"),
    ("wave", "👋"),
    ("white_check_mark", "✅"),
    ("wink", "😉"),
    ("x", "❌"),
    ("zap", "⚡"),
];

/// Unicode glyph for a GitHub shortcode name (`smile`), if we know it.
pub fn lookup_emoji(name: &str) -> Option<&'static str> {
    EMOJI
        .binary_search_by_key(&name, |&(n, _)| n)
        .ok()
        .map(|i| EMOJI[i].1)
}

/// Glyph for a full `:name:` slice, if the name is in the table.
pub fn lookup_shortcode(raw: &str) -> Option<&'static str> {
    let name = raw.strip_prefix(':')?.strip_suffix(':')?;
    lookup_emoji(name)
}

/// One matched `:name:` in `text`: byte range, alias, glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmojiMatch {
    pub start: usize,
    pub end: usize,
    pub name: &'static str,
    pub glyph: &'static str,
}

/// Scan `text` for known `:alias:` tokens. Unmatched `:foo:` is ignored.
/// A backslash before `:` skips that candidate (escaped colon).
pub fn find_emoji_shortcodes(text: &str) -> Vec<EmojiMatch> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i = i.saturating_add(2).min(bytes.len());
            continue;
        }
        if bytes[i] != b':' {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        let name_start = i;
        while i < bytes.len() && is_shortcode_char(bytes[i]) {
            i += 1;
        }
        if i > name_start && i < bytes.len() && bytes[i] == b':' {
            let name = &text[name_start..i];
            if let Some((alias, glyph)) = lookup_pair(name) {
                out.push(EmojiMatch {
                    start,
                    end: i + 1,
                    name: alias,
                    glyph,
                });
                i += 1;
                continue;
            }
        }
        i = start + 1;
    }
    out
}

fn is_shortcode_char(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'+' | b'-')
}

fn lookup_pair(name: &str) -> Option<(&'static str, &'static str)> {
    EMOJI
        .binary_search_by_key(&name, |&(n, _)| n)
        .ok()
        .map(|i| EMOJI[i])
}

/// Split text runs so each known `:name:` becomes [`Inline::Emoji`].
/// Inline code is left alone. Unmatched `:foo:` stays in the run.
pub fn apply_emoji_shortcodes(inlines: &mut Vec<Inline>) {
    if !inlines.iter().any(run_may_have_emoji) {
        return;
    }
    let mut out = Vec::with_capacity(inlines.len());
    for inline in inlines.drain(..) {
        match inline {
            Inline::Run {
                text,
                raw,
                source_range,
                marks,
                link,
                fidelity,
            } if !marks.contains(MarkSet::CODE) => {
                let found = find_emoji_shortcodes(&text);
                if found.is_empty() {
                    out.push(Inline::Run {
                        text,
                        raw,
                        source_range,
                        marks,
                        link,
                        fidelity,
                    });
                    continue;
                }
                let mut cur = 0usize;
                for m in found {
                    if m.start > cur {
                        let src_start = mapped_source(&source_range, text.len(), cur);
                        let src_end = mapped_source(&source_range, text.len(), m.start);
                        out.push(Inline::Run {
                            text: text[cur..m.start].to_string(),
                            raw: None,
                            source_range: src_start..src_end.max(src_start),
                            marks,
                            link: link.clone(),
                            fidelity,
                        });
                    }
                    let src_start = mapped_source(&source_range, text.len(), m.start);
                    let src_end = mapped_source(&source_range, text.len(), m.end);
                    out.push(Inline::Emoji {
                        name: m.name.to_string(),
                        glyph: m.glyph.to_string(),
                        raw: Box::from(&text[m.start..m.end]),
                        source_range: src_start..src_end.max(src_start),
                        marks,
                        link: link.clone(),
                        fidelity,
                    });
                    cur = m.end;
                }
                if cur < text.len() {
                    let src_start = mapped_source(&source_range, text.len(), cur);
                    let src_end = mapped_source(&source_range, text.len(), text.len());
                    out.push(Inline::Run {
                        text: text[cur..].to_string(),
                        raw: None,
                        source_range: src_start..src_end.max(src_start),
                        marks,
                        link,
                        fidelity,
                    });
                }
            }
            other => out.push(other),
        }
    }
    *inlines = out;
}

fn run_may_have_emoji(inline: &Inline) -> bool {
    match inline {
        Inline::Run { text, marks, .. } => !marks.contains(MarkSet::CODE) && text.contains(':'),
        _ => false,
    }
}

fn mapped_source(src: &Range<usize>, text_len: usize, off: usize) -> usize {
    if src.len() == text_len {
        src.start + off.min(text_len)
    } else {
        src.len()
            .saturating_mul(off.min(text_len))
            .checked_div(text_len)
            .map(|n| src.start + n)
            .unwrap_or(src.start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_for_binary_search() {
        for w in EMOJI.windows(2) {
            assert!(
                w[0].0 < w[1].0,
                "emoji table not sorted: {:?} then {:?}",
                w[0].0,
                w[1].0
            );
        }
    }

    #[test]
    fn known_names_resolve() {
        assert_eq!(lookup_emoji("smile"), Some("😄"));
        assert_eq!(lookup_emoji("heart"), Some("❤️"));
        assert_eq!(lookup_emoji("+1"), Some("👍"));
        assert_eq!(lookup_emoji("rocket"), Some("🚀"));
        assert_eq!(lookup_shortcode(":tada:"), Some("🎉"));
    }

    #[test]
    fn unknown_and_wrong_case_are_none() {
        assert_eq!(lookup_emoji("not_an_emoji"), None);
        assert_eq!(lookup_emoji("Smile"), None);
        assert_eq!(lookup_shortcode(":foo:"), None);
        assert_eq!(lookup_shortcode("smile"), None);
    }

    #[test]
    fn scan_finds_known_and_skips_unknown() {
        let found = find_emoji_shortcodes("hi :smile: and :foo: and :rocket:");
        assert_eq!(
            found
                .iter()
                .map(|m| (m.name, &"hi :smile: and :foo: and :rocket:"[m.start..m.end]))
                .collect::<Vec<_>>(),
            vec![("smile", ":smile:"), ("rocket", ":rocket:")]
        );
    }

    #[test]
    fn escaped_colon_is_not_a_shortcode() {
        assert!(find_emoji_shortcodes("\\:smile:").is_empty());
    }
}
