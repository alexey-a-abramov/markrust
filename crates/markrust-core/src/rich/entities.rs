// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CommonMark character references (`&amp;` / `&#123;` / `&#x7B;`) and
//! backslash escapes (`\*` / `\[` / `\\`).
//!
//! Comrak decodes them in `Text` nodes while the source still has the
//! literal. WYSIWYG paints the decoded glyph and treats the extra source
//! bytes as dest chrome (one Left/Right/Backspace/Delete step).

use std::ops::Range;

use super::escape::looks_like_entity;
use super::tree::{Inline, MarkSet};

/// Exclusive end of a `&name;` / `&#123;` / `&#xAB;` literal at `at`.
pub fn character_reference_end(source: &str, at: usize) -> Option<usize> {
    if !source.is_char_boundary(at) || source.as_bytes().get(at) != Some(&b'&') {
        return None;
    }
    if !looks_like_entity(&source[at..]) {
        return None;
    }
    let semi = source.get(at + 1..)?.find(';')?;
    Some(at + 1 + semi + 1)
}

/// True when `slice` is a character-reference literal that decoded to `text`.
pub fn is_decoded_character_reference(slice: &str, text: &str) -> bool {
    if slice == text || text.is_empty() || slice.len() < 3 {
        return false;
    }
    if !slice.starts_with('&') || !slice.ends_with(';') {
        return false;
    }
    if !looks_like_entity(slice) || text.starts_with(slice) {
        return false;
    }
    // Comrak emits one decoded codepoint (rarely a 2-codepoint cluster).
    text.chars().count() <= 2
}

/// Inner caret home is the first source byte (`&` / `\`); the rest is dest chrome.
pub fn character_reference_visible_range(source_range: Range<usize>) -> Range<usize> {
    if source_range.end <= source_range.start {
        return source_range;
    }
    source_range.start..source_range.start + 1
}

/// True when `slice` is a CommonMark backslash escape that decoded to `text`.
pub fn is_decoded_backslash_escape(slice: &str, text: &str) -> bool {
    let Some(rest) = slice.strip_prefix('\\') else {
        return false;
    };
    if rest != text || text.is_empty() {
        return false;
    }
    let mut chars = text.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_punctuation()) && chars.next().is_none()
}

/// `*` of `\*` is a closed suffix: Left/Right must not sit on the escaped glyph.
pub fn backslash_escape_closed_suffix(
    source: &str,
    inner: &Range<usize>,
    outer: &Range<usize>,
) -> bool {
    if inner.end != inner.start + 1 {
        return false;
    }
    if source.as_bytes().get(inner.start) != Some(&b'\\') {
        return false;
    }
    let Some(ch) = source.get(inner.end..).and_then(|s| s.chars().next()) else {
        return false;
    };
    if !ch.is_ascii_punctuation() {
        return false;
    }
    let end = inner.end + ch.len_utf8();
    end > inner.end && end <= outer.end
}

/// `amp;` (and `](url)` wrapping a whole-entity label) is a closed suffix:
/// Left/Right must not sit on `a` / `m` / `p`.
pub fn character_reference_closed_suffix(
    source: &str,
    inner: &Range<usize>,
    outer: &Range<usize>,
) -> bool {
    if inner.end != inner.start + 1 {
        return false;
    }
    let Some(end) = character_reference_end(source, inner.start) else {
        return false;
    };
    end > inner.end && end <= outer.end
}

/// Split text runs so each decoded `&amp;` / `&#123;` is its own run whose
/// `source_range` is the entity literal. Inline code is left alone.
pub fn split_character_reference_inlines(
    source: &str,
    block_range: Range<usize>,
    inlines: &mut Vec<Inline>,
) {
    if !inlines
        .iter()
        .any(|inline| run_may_have_decoded(source, inline))
    {
        return;
    }
    let lo = block_range.start.min(source.len());
    let hi = block_range.end.min(source.len()).max(lo);
    let mut cursor = lo;
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
                let span = span_for_run(
                    source,
                    cursor,
                    hi,
                    source_range.clone(),
                    raw.as_deref(),
                    &text,
                );
                let src_slice = source.get(span.clone()).unwrap_or("");
                match lockstep_segments(src_slice, &text, true) {
                    Some(segs) if segs.iter().any(Seg::needs_split) => {
                        for seg in segs {
                            let src = span.start + seg.src.start..span.start + seg.src.end;
                            let piece = &text[seg.text.clone()];
                            out.push(Inline::Run {
                                text: piece.to_string(),
                                raw: Some(Box::<str>::from(
                                    source.get(src.clone()).unwrap_or(piece),
                                )),
                                source_range: src,
                                marks,
                                link: link.clone(),
                                fidelity,
                            });
                        }
                        cursor = span.end.max(cursor);
                    }
                    _ => {
                        cursor = cursor.max(span.end).max(source_range.end);
                        let recovered = src_slice == text.as_str()
                            || lockstep_segments(src_slice, &text, true).is_some();
                        out.push(Inline::Run {
                            text,
                            raw,
                            source_range: if recovered { span } else { source_range },
                            marks,
                            link,
                            fidelity,
                        });
                    }
                }
            }
            other => {
                cursor = cursor.max(other.source_range().end);
                out.push(other);
            }
        }
    }
    *inlines = out;
}

fn run_may_have_decoded(source: &str, inline: &Inline) -> bool {
    match inline {
        Inline::Run {
            text,
            raw,
            marks,
            source_range,
            ..
        } => {
            if marks.contains(MarkSet::CODE) {
                return false;
            }
            if raw
                .as_deref()
                .is_some_and(|s| (s.contains('&') || s.contains('\\')) && s != text.as_str())
                || text.contains('&')
            {
                return true;
            }
            // Comrak last-in-line `&amp;` / `\\` sourcepos is often the
            // decoded length (`A&` / `A\`), which locksteps as plain text.
            if decoded_span_extends_past(source, source_range, text) {
                return true;
            }
            let start = source_range.start;
            start > 0
                && source.as_bytes().get(start - 1) == Some(&b'\\')
                && odd_backslash_escape(source.as_bytes(), start)
                && text
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_punctuation())
        }
        _ => false,
    }
}

fn span_for_run(
    source: &str,
    lo: usize,
    hi: usize,
    reported: Range<usize>,
    raw: Option<&str>,
    text: &str,
) -> Range<usize> {
    let hi = hi.min(source.len());
    let lo = lo.min(hi);
    let start = include_omitted_backslash(source, lo, reported.start.clamp(lo, hi), text);
    let end = reported.end.clamp(lo, hi).max(start);
    let reported = start..end;
    if let Some(entity_end) = character_reference_end(source, reported.start) {
        if entity_end <= hi {
            if let Some(literal) = source.get(reported.start..entity_end) {
                if is_decoded_character_reference(literal, text) {
                    return reported.start..entity_end;
                }
            }
        }
    }
    if lockstep_segments(source.get(reported.clone()).unwrap_or(""), text, true).is_some() {
        return expand_truncated_decoded(source, reported, hi, text);
    }
    if let Some(raw) = raw {
        if lockstep_segments(raw, text, true).is_some() {
            if let Some(found) = find_literal(source, raw, lo, hi, reported.start) {
                return found;
            }
        }
    }
    find_decoded(source, text, lo, hi).unwrap_or(reported)
}

/// Comrak last-in-line `&amp;` / `\\` sourcepos is often the decoded length
/// (`A&` / `A\`). That prefix locksteps as plain text, so expand onto the
/// entity / escape literal before splitting dest chrome.
fn expand_truncated_decoded(
    source: &str,
    reported: Range<usize>,
    hi: usize,
    text: &str,
) -> Range<usize> {
    match match_decoded_end(source, reported.start, hi, text) {
        Some(end) if end > reported.end => reported.start..end,
        _ => reported,
    }
}

fn decoded_span_extends_past(source: &str, reported: &Range<usize>, text: &str) -> bool {
    match_decoded_end(source, reported.start, source.len(), text)
        .is_some_and(|end| end > reported.end)
}

/// Comrak often reports `\*` sourcepos as the punctuation only (the `*`),
/// leaving the backslash as a gap. Pull it in so dest-chrome split can see `\X`.
fn include_omitted_backslash(source: &str, lo: usize, start: usize, text: &str) -> usize {
    if start <= lo || source.as_bytes().get(start - 1) != Some(&b'\\') {
        return start;
    }
    if !odd_backslash_escape(source.as_bytes(), start) {
        return start;
    }
    let Some(ch) = text.chars().next() else {
        return start;
    };
    if !ch.is_ascii_punctuation() {
        return start;
    }
    if source.get(start..).is_some_and(|s| s.starts_with(ch)) {
        start - 1
    } else {
        start
    }
}

fn odd_backslash_escape(bytes: &[u8], i: usize) -> bool {
    let mut n = 0usize;
    let mut j = i;
    while j > 0 && bytes[j - 1] == b'\\' {
        n += 1;
        j -= 1;
    }
    n % 2 == 1
}

fn find_literal(
    source: &str,
    needle: &str,
    lo: usize,
    hi: usize,
    hint: usize,
) -> Option<Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    let search = source.get(lo..hi)?;
    let mut rel = 0usize;
    let mut best = None;
    let mut best_dist = usize::MAX;
    while let Some(at) = search[rel..].find(needle) {
        let start = lo + rel + at;
        let range = start..start + needle.len();
        let dist = start.abs_diff(hint);
        if dist < best_dist {
            best_dist = dist;
            best = Some(range);
        }
        rel += at + 1;
        if rel >= search.len() {
            break;
        }
    }
    best
}

fn find_decoded(source: &str, text: &str, lo: usize, hi: usize) -> Option<Range<usize>> {
    if text.is_empty() {
        return None;
    }
    let mut i = lo;
    while i < hi {
        if let Some(end) = match_decoded_end(source, i, hi, text) {
            return Some(i..end);
        }
        let step = source[i..].chars().next()?.len_utf8();
        i += step;
    }
    None
}

fn match_decoded_end(source: &str, start: usize, hi: usize, text: &str) -> Option<usize> {
    let slice = source.get(start..hi)?;
    let segs = lockstep_segments(slice, text, false)?;
    let end = segs.last().map(|s| start + s.src.end).unwrap_or(start);
    (end > start).then_some(end)
}

struct Seg {
    src: Range<usize>,
    text: Range<usize>,
    entity: bool,
    escape: bool,
}

impl Seg {
    fn needs_split(&self) -> bool {
        self.entity || self.escape
    }

    fn is_plain(&self) -> bool {
        !self.entity && !self.escape
    }
}

/// Walk source vs decoded text. `None` when they cannot be aligned.
/// `exact_src` requires consuming all of `src` (split of a known span);
/// prefix match stops when `text` is consumed (span recovery in a link label).
fn lockstep_segments(src: &str, text: &str, exact_src: bool) -> Option<Vec<Seg>> {
    let mut s = 0usize;
    let mut t = 0usize;
    let mut out: Vec<Seg> = Vec::new();
    while t < text.len() {
        if s >= src.len() {
            return None;
        }
        if src.as_bytes()[s] == b'\\' && s + 1 < src.len() {
            let ch = src[s + 1..].chars().next()?;
            if ch.is_ascii_punctuation() && text[t..].starts_with(ch) {
                let s_end = s + 1 + ch.len_utf8();
                let t_end = t + ch.len_utf8();
                out.push(Seg {
                    src: s..s_end,
                    text: t..t_end,
                    entity: false,
                    escape: true,
                });
                s = s_end;
                t = t_end;
                continue;
            }
            if !text[t..].starts_with('\\') {
                return None;
            }
            let s_end = s + 1;
            let t_end = t + 1;
            push_plain(&mut out, s, s_end, t, t_end);
            s = s_end;
            t = t_end;
            continue;
        }
        if let Some(end) = character_reference_end(src, s) {
            let literal = src.get(s..end)?;
            if !text[t..].starts_with(literal) {
                let ch = text[t..].chars().next()?;
                let t_end = t + ch.len_utf8();
                if let Some(last) = out.last_mut() {
                    if last.entity {
                        return None;
                    }
                }
                out.push(Seg {
                    src: s..end,
                    text: t..t_end,
                    entity: true,
                    escape: false,
                });
                s = end;
                t = t_end;
                continue;
            }
        }
        let ch = src[s..].chars().next()?;
        if !text[t..].starts_with(ch) {
            return None;
        }
        let s_end = s + ch.len_utf8();
        let t_end = t + ch.len_utf8();
        push_plain(&mut out, s, s_end, t, t_end);
        s = s_end;
        t = t_end;
    }
    if t != text.len() {
        return None;
    }
    if exact_src && s != src.len() {
        return None;
    }
    Some(out)
}

fn push_plain(out: &mut Vec<Seg>, s: usize, s_end: usize, t: usize, t_end: usize) {
    if let Some(last) = out.last_mut() {
        if last.is_plain() && last.src.end == s && last.text.end == t {
            last.src.end = s_end;
            last.text.end = t_end;
            return;
        }
    }
    out.push(Seg {
        src: s..s_end,
        text: t..t_end,
        entity: false,
        escape: false,
    });
}

#[cfg(test)]
mod tests {
    use super::super::tree::MarkFidelity;
    use super::*;

    #[test]
    fn named_and_numeric_literals_are_entities() {
        assert!(is_decoded_character_reference("&amp;", "&"));
        assert!(is_decoded_character_reference("&lt;", "<"));
        assert!(is_decoded_character_reference("&gt;", ">"));
        assert!(is_decoded_character_reference("&quot;", "\""));
        assert!(is_decoded_character_reference("&#39;", "'"));
        assert!(is_decoded_character_reference("&#123;", "{"));
        assert!(is_decoded_character_reference("&#x7B;", "{"));
        assert!(!is_decoded_character_reference("&amp;", "&amp;"));
        assert!(!is_decoded_character_reference("A&amp;B", "A&B"));
        assert!(!is_decoded_character_reference("&amp;", "A&B"));
        assert!(!is_decoded_character_reference("&", "&"));
        assert!(is_decoded_backslash_escape("\\*", "*"));
        assert!(is_decoded_backslash_escape("\\\\", "\\"));
        assert!(is_decoded_backslash_escape("\\[", "["));
        assert!(!is_decoded_backslash_escape("\\*", "\\*"));
        assert!(!is_decoded_backslash_escape("A\\*B", "A*B"));
        assert!(!is_decoded_backslash_escape("\\a", "a"));
        assert!(!is_decoded_backslash_escape("\\", "\\"));
    }

    #[test]
    fn lockstep_splits_mixed_text() {
        let segs = lockstep_segments("A&amp;B", "A&B", true).expect("align");
        assert_eq!(segs.len(), 3);
        assert!(segs[0].is_plain() && segs[0].src == (0..1));
        assert!(segs[1].entity && segs[1].src == (1..6));
        assert!(segs[2].is_plain() && segs[2].src == (6..7));
    }

    #[test]
    fn lockstep_splits_backslash_escapes() {
        let segs = lockstep_segments("A\\*B", "A*B", true).expect("align");
        assert_eq!(segs.len(), 3);
        assert!(segs[0].is_plain() && segs[0].src == (0..1));
        assert!(segs[1].escape && segs[1].src == (1..3));
        assert!(segs[2].is_plain() && segs[2].src == (3..4));
        assert!(lockstep_segments("A\\*B", "A\\*B", true)
            .unwrap()
            .iter()
            .all(|s| s.is_plain()));
    }

    #[test]
    fn lockstep_keeps_code_like_literals() {
        assert!(lockstep_segments("A&amp;B", "A&amp;B", true).is_some());
        assert!(lockstep_segments("A&amp;B", "A&amp;B", true)
            .unwrap()
            .iter()
            .all(|s| !s.entity));
    }

    #[test]
    fn lockstep_prefix_recovers_link_label() {
        let src = "[A&amp;B](https://e.com)\n";
        assert_eq!(
            match_decoded_end(src, 1, src.len(), "A&B"),
            Some(8),
            "label `A&B` must map onto `A&amp;B`"
        );
    }

    #[test]
    fn omitted_backslash_sourcepos_is_pulled_in() {
        let src = "[A\\*B](https://e.com)\n";
        assert_eq!(
            include_omitted_backslash(src, 0, 3, "*"),
            2,
            "comrak `*` sourcepos must expand onto `\\*`"
        );
        assert_eq!(
            include_omitted_backslash(src, 0, 1, "A"),
            1,
            "plain `A` must not steal a backslash"
        );
        let even = "A\\\\*B\n";
        let star = even.find('*').expect("star");
        assert_eq!(
            include_omitted_backslash(even, 0, star, "*"),
            star,
            "even `\\\\*` must not treat `*` as escaped"
        );
    }

    #[test]
    fn truncated_last_in_line_amp_sourcepos_expands() {
        let src = "A&amp;\n";
        assert_eq!(
            expand_truncated_decoded(src, 0..2, src.len(), "A&"),
            0..6,
            "decoded-length `A&` must expand onto `&amp;`"
        );
        assert_eq!(
            expand_truncated_decoded(src, 0..6, src.len(), "A&"),
            0..6,
            "a full entity span must stay put"
        );
        let escaped = "A\\\\\n";
        assert_eq!(
            expand_truncated_decoded(escaped, 0..2, escaped.len(), "A\\"),
            0..3,
            "decoded-length `A\\` must expand onto `\\\\`"
        );
        assert!(
            decoded_span_extends_past(src, &(0..2), "A&"),
            "truncated `&amp;` must be queued for split"
        );
        assert!(!decoded_span_extends_past(src, &(0..6), "A&"));
    }

    fn truncated_run(text: &str, raw: &str, range: Range<usize>) -> Inline {
        Inline::Run {
            text: text.into(),
            raw: Some(raw.into()),
            source_range: range,
            marks: MarkSet::empty(),
            link: None,
            fidelity: MarkFidelity::default(),
        }
    }

    #[test]
    fn split_recovers_truncated_last_in_line_amp() {
        let src = "A&amp;\n";
        let mut inlines = vec![truncated_run("A&", "A&", 0..2)];
        split_character_reference_inlines(src, 0..src.len(), &mut inlines);
        let got: Vec<_> = inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Run {
                    text, source_range, ..
                } => Some((text.as_str(), source_range.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            got,
            vec![("A", 0..1), ("&", 1..6)],
            "truncated last-in-line `&amp;` must split dest chrome, got {got:?}"
        );

        let escaped = "A\\\\\n";
        let mut inlines = vec![truncated_run("A\\", "A\\", 0..2)];
        split_character_reference_inlines(escaped, 0..escaped.len(), &mut inlines);
        let got: Vec<_> = inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Run {
                    text, source_range, ..
                } => Some((
                    text.as_str(),
                    escaped.get(source_range.clone()).unwrap_or(""),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            got,
            vec![("A", "A"), ("\\", "\\\\")],
            "truncated last-in-line `\\\\` must split dest chrome, got {got:?}"
        );
    }
}
