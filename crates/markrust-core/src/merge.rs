// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Source-preserving three-way merge for concurrent document edits.
//!
//! Similar's structured diff3 regions provide the line merge. Touching regions
//! are refined using exact base ranges; only a one-line replacement on both
//! sides can be refined further into extended graphemes. Markdown is never
//! parsed and reserialized: spaces, escapes, CRLF, fences, and final newlines
//! remain the exact bytes chosen by the edits. This is a lexical merge, not a
//! claim that concurrently changed Markdown has equivalent semantics.
//!
//! Work is bounded before invoking upstream algorithms, which do not expose a
//! deadline for TextMerge. Oversized or ambiguous work returns a conflict for
//! explicit review; it must not stall the native UI or select a winning branch.
//! No conflict-marker string is ever returned as merged text.

use std::ops::Range;

use similar::{capture_diff_slices, Algorithm, DiffTag, DiffableStr, MergeResolution, TextMerge};
use unicode_segmentation::UnicodeSegmentation;

/// Maximum bytes per input considered for automatic merging (4 MiB).
pub const MAX_MERGE_INPUT_BYTES: usize = 4 * 1024 * 1024;
/// Maximum line tokens per input, before trimming common prefix/suffix.
pub const MAX_MERGE_INPUT_LINES: usize = 100_000;
/// Bound each base-to-side changed-region line comparison.
const MAX_LINE_COMPARISON_WORK: usize = 2_000_000;
/// A conflicting line must fit both limits before grapheme refinement.
const MAX_INLINE_BYTES: usize = 16 * 1024;
const MAX_INLINE_GRAPHEMES: usize = 2_048;
const MAX_INLINE_COMPARISON_WORK: usize = 2_000_000;
/// All conflicting lines share this budget; many individually small lines
/// must not multiply native-UI work without a limit.
const MAX_TOTAL_INLINE_COMPARISON_WORK: usize = 4_000_000;

/// Result of merging in-memory and disk bytes against their common base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Ours already matches theirs, or theirs still equals base.
    Unchanged,
    /// Ours still equals base; take the disk version.
    TakeTheirs,
    /// Independent source edits combined without normalizing document bytes.
    Merged(String),
    /// Incompatible/ambiguous edits, or automatic-merge work limits exceeded.
    /// All three originals must remain available for explicit resolution.
    Conflict,
}

/// Conflicts are data, not instructions to reload or overwrite either version.
pub fn three_way_merge(base: &str, ours: &str, theirs: &str) -> MergeOutcome {
    if ours == theirs || theirs == base {
        return MergeOutcome::Unchanged;
    }
    if ours == base {
        return MergeOutcome::TakeTheirs;
    }
    match merge_source(base, ours, theirs) {
        Some(merged) => MergeOutcome::Merged(merged),
        None => MergeOutcome::Conflict,
    }
}

fn bounded_lines(source: &str) -> Option<Vec<&str>> {
    if source.len() > MAX_MERGE_INPUT_BYTES {
        return None;
    }
    // Match Similar's LF, CRLF, and standalone CR tokenization while stopping
    // before an input full of delimiters allocates millions of line tokens.
    let bytes = source.as_bytes();
    let mut lines = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if lines.len() == MAX_MERGE_INPUT_LINES {
            return None;
        }
        let start = cursor;
        while cursor < bytes.len() && !matches!(bytes[cursor], b'\r' | b'\n') {
            cursor += 1;
        }
        if cursor < bytes.len() {
            let carriage_return = bytes[cursor] == b'\r';
            cursor += 1;
            if carriage_return && bytes.get(cursor) == Some(&b'\n') {
                cursor += 1;
            }
        }
        lines.push(&source[start..cursor]);
    }
    Some(lines)
}

fn within_work_limit(base: usize, side: usize, limit: usize) -> bool {
    base.max(1)
        .checked_mul(side.max(1))
        .is_some_and(|work| work <= limit)
}

fn merge_source(base: &str, ours: &str, theirs: &str) -> Option<String> {
    let base_lines = bounded_lines(base)?;
    let ours_lines = bounded_lines(ours)?;
    let theirs_lines = bounded_lines(theirs)?;
    // Preserve large unchanged surroundings without charging them against
    // the comparison budget or running another diff over them.
    let prefix = base_lines
        .iter()
        .zip(&ours_lines)
        .zip(&theirs_lines)
        .take_while(|((base, ours), theirs)| base == ours && base == theirs)
        .count();
    let suffix = base_lines[prefix..]
        .iter()
        .rev()
        .zip(ours_lines[prefix..].iter().rev())
        .zip(theirs_lines[prefix..].iter().rev())
        .take_while(|((base, ours), theirs)| base == ours && base == theirs)
        .count();
    // Keep a context token at each boundary so trimming cannot conceal an
    // ambiguous deletion of identical adjacent lines.
    let prefix = prefix.saturating_sub(1);
    let suffix = suffix.saturating_sub(1);
    let prefix_bytes: usize = base_lines[..prefix].iter().map(|line| line.len()).sum();
    let suffix_bytes: usize = base_lines[base_lines.len() - suffix..]
        .iter()
        .map(|line| line.len())
        .sum();
    let base_changed = &base[prefix_bytes..base.len() - suffix_bytes];
    let ours_changed = &ours[prefix_bytes..ours.len() - suffix_bytes];
    let theirs_changed = &theirs[prefix_bytes..theirs.len() - suffix_bytes];
    let base_count = base_lines.len() - prefix - suffix;
    let ours_count = ours_lines.len() - prefix - suffix;
    let theirs_count = theirs_lines.len() - prefix - suffix;
    if !within_work_limit(base_count, ours_count, MAX_LINE_COMPARISON_WORK)
        || !within_work_limit(base_count, theirs_count, MAX_LINE_COMPARISON_WORK)
    {
        return None;
    }
    let base_tokens = &base_lines[prefix..base_lines.len() - suffix];
    let ours_tokens = &ours_lines[prefix..ours_lines.len() - suffix];
    let theirs_tokens = &theirs_lines[prefix..theirs_lines.len() - suffix];
    // A shortest edit script is not unique around repeated tokens. When the
    // reverse script chooses different ownership, do not guess which repeated
    // source line was deleted or edited.
    unambiguous_edits(base_tokens, ours_tokens)?;
    unambiguous_edits(base_tokens, theirs_tokens)?;
    let merge = TextMerge::from_lines(base_changed, ours_changed, theirs_changed);
    let mut merged = String::with_capacity(ours.len().max(theirs.len()));
    let mut inline_work_left = MAX_TOTAL_INLINE_COMPARISON_WORK;
    merged.push_str(&base[..prefix_bytes]);
    for region in merge.regions() {
        match region.resolution() {
            MergeResolution::Unchanged | MergeResolution::Ours | MergeResolution::Both => {
                for ix in region.ours_range() {
                    merged.push_str(merge.ours_line(ix)?);
                }
            }
            MergeResolution::Theirs => {
                for ix in region.theirs_range() {
                    merged.push_str(merge.theirs_line(ix)?);
                }
            }
            MergeResolution::Conflict => {
                merged.push_str(&merge_line_region(
                    base_tokens.get(region.base_range())?,
                    ours_tokens.get(region.ours_range())?,
                    theirs_tokens.get(region.theirs_range())?,
                    &mut inline_work_left,
                )?);
            }
            _ => return None,
        }
    }
    merged.push_str(&base[base.len() - suffix_bytes..]);
    Some(merged)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TokenEdit {
    base: Range<usize>,
    replacement: String,
}

fn token_edits(base: &[&str], side: &[&str]) -> Vec<TokenEdit> {
    capture_diff_slices(Algorithm::Myers, base, side)
        .into_iter()
        .filter(|op| op.tag() != DiffTag::Equal)
        .map(|op| TokenEdit {
            base: op.old_range(),
            replacement: side[op.new_range()].concat(),
        })
        .collect()
}

fn unambiguous_edits(base: &[&str], side: &[&str]) -> Option<Vec<TokenEdit>> {
    let forward = token_edits(base, side);
    let reversed_base: Vec<_> = base.iter().copied().rev().collect();
    let reversed_side: Vec<_> = side.iter().copied().rev().collect();
    let mut backward: Vec<_> =
        capture_diff_slices(Algorithm::Myers, &reversed_base, &reversed_side)
            .into_iter()
            .filter(|op| op.tag() != DiffTag::Equal)
            .map(|op| TokenEdit {
                base: base.len() - op.old_range().end..base.len() - op.old_range().start,
                replacement: side
                    [side.len() - op.new_range().end..side.len() - op.new_range().start]
                    .concat(),
            })
            .collect();
    backward.sort_by_key(|edit| (edit.base.start, edit.base.end));
    (forward == backward).then_some(forward)
}

fn edits_conflict(a: &TokenEdit, b: &TokenEdit) -> bool {
    if a == b {
        return false;
    }
    if a.base.is_empty() {
        // At a changed-range boundary, ordering/ownership is ambiguous too.
        return b.base.start <= a.base.start && a.base.start <= b.base.end;
    }
    if b.base.is_empty() {
        return a.base.start <= b.base.start && b.base.start <= a.base.end;
    }
    a.base.start < b.base.end && b.base.start < a.base.end
}

fn merge_line_region(
    base: &[&str],
    ours: &[&str],
    theirs: &[&str],
    inline_work_left: &mut usize,
) -> Option<String> {
    let mut ours_edits = unambiguous_edits(base, ours)?;
    let theirs_edits = unambiguous_edits(base, theirs)?;
    for theirs_edit in theirs_edits {
        if ours_edits.contains(&theirs_edit) {
            continue;
        }
        let overlaps: Vec<_> = ours_edits
            .iter()
            .enumerate()
            .filter_map(|(ix, ours)| edits_conflict(ours, &theirs_edit).then_some(ix))
            .collect();
        match overlaps.as_slice() {
            [] => ours_edits.push(theirs_edit),
            [ix] => {
                let ours_edit = &mut ours_edits[*ix];
                // Equal output line counts do not establish row ownership.
                // Refine only exact, matching one-line base replacements.
                if ours_edit.base != theirs_edit.base
                    || ours_edit.base.len() != 1
                    || ours_edit.replacement.tokenize_lines().len() != 1
                    || theirs_edit.replacement.tokenize_lines().len() != 1
                {
                    return None;
                }
                ours_edit.replacement = merge_graphemes(
                    base[ours_edit.base.start],
                    &ours_edit.replacement,
                    &theirs_edit.replacement,
                    inline_work_left,
                )?;
            }
            _ => return None,
        }
    }
    apply_edits(base, ours_edits)
}

fn bounded_graphemes(text: &str) -> Option<Vec<&str>> {
    if text.len() > MAX_INLINE_BYTES {
        return None;
    }
    let tokens: Vec<_> = text
        .graphemes(true)
        .take(MAX_INLINE_GRAPHEMES + 1)
        .collect();
    (tokens.len() <= MAX_INLINE_GRAPHEMES).then_some(tokens)
}

fn merge_graphemes(
    base: &str,
    ours: &str,
    theirs: &str,
    inline_work_left: &mut usize,
) -> Option<String> {
    let base = bounded_graphemes(base)?;
    let ours = bounded_graphemes(ours)?;
    let theirs = bounded_graphemes(theirs)?;
    if !within_work_limit(base.len(), ours.len(), MAX_INLINE_COMPARISON_WORK)
        || !within_work_limit(base.len(), theirs.len(), MAX_INLINE_COMPARISON_WORK)
    {
        return None;
    }
    let work = base
        .len()
        .max(1)
        .checked_mul(ours.len().max(1))?
        .checked_add(base.len().max(1).checked_mul(theirs.len().max(1))?)?;
    *inline_work_left = inline_work_left.checked_sub(work)?;
    let mut combined = unambiguous_edits(&base, &ours)?;
    let theirs_edits = unambiguous_edits(&base, &theirs)?;
    if combined
        .iter()
        .any(|a| theirs_edits.iter().any(|b| edits_conflict(a, b)))
    {
        return None;
    }
    for edit in theirs_edits {
        if !combined.contains(&edit) {
            combined.push(edit);
        }
    }
    apply_edits(&base, combined)
}

fn apply_edits(base: &[&str], mut edits: Vec<TokenEdit>) -> Option<String> {
    edits.sort_by_key(|edit| (edit.base.start, edit.base.end));
    let mut merged = String::new();
    let mut cursor = 0;
    for edit in edits {
        // Never clamp a stale range and apply it twice to a shorter result.
        if edit.base.start < cursor || edit.base.end > base.len() {
            return None;
        }
        for token in &base[cursor..edit.base.start] {
            merged.push_str(token);
        }
        merged.push_str(&edit.replacement);
        cursor = edit.base.end;
    }
    for token in &base[cursor..] {
        merged.push_str(token);
    }
    Some(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_merge(base: &str, ours: &str, theirs: &str, expected: &str) {
        assert_eq!(
            three_way_merge(base, ours, theirs),
            MergeOutcome::Merged(expected.into())
        );
        assert_eq!(
            three_way_merge(base, theirs, ours),
            MergeOutcome::Merged(expected.into()),
            "Merge must not silently prefer one branch by call order"
        );
    }

    #[test]
    fn unchanged_and_take_theirs_are_exact_identity_cases() {
        assert_eq!(
            three_way_merge("aaa\r\n", "aaa\r\n", "bbb"),
            MergeOutcome::TakeTheirs
        );
        assert_eq!(
            three_way_merge("aaa\n", "bbb\n", "bbb\n"),
            MergeOutcome::Unchanged
        );
        assert_eq!(
            three_way_merge("aaa\n", "bbb\n", "aaa\n"),
            MergeOutcome::Unchanged
        );
    }

    #[test]
    fn merges_disjoint_line_edits() {
        assert_merge(
            "aaa\nbbb\nccc\n",
            "aaa\nBBB\nccc\n",
            "aaa\nbbb\nCCC\n",
            "aaa\nBBB\nCCC\n",
        );
    }

    #[test]
    fn merges_independent_words_on_one_line() {
        assert_merge(
            "red blue green\n",
            "RED blue green\n",
            "red blue GREEN\n",
            "RED blue GREEN\n",
        );
        assert_merge("a😀b\n", "A😀b\n", "a😀B\n", "A😀B\n");
    }

    #[test]
    fn identical_edits_deduplicate_alongside_independent_edits() {
        assert_merge("a b c\n", "A B c\n", "A b C\n", "A B C\n");
    }

    #[test]
    fn inserts_at_distinct_adjacent_positions_merge_without_reordering() {
        assert_merge("ab\n", "aXb\n", "abY\n", "aXbY\n");
    }

    #[test]
    fn disjoint_unicode_replacements_merge_at_every_base_range() {
        let tokens = ["a", "😀", "e\u{301}", "👩‍👩‍👧‍👦", "z"];
        let base = format!("{}\r\n", tokens.concat());
        for a_start in 0..tokens.len() {
            for a_end in a_start + 1..=tokens.len() {
                for b_start in 0..tokens.len() {
                    for b_end in b_start + 1..=tokens.len() {
                        if a_start < b_end && b_start < a_end {
                            continue;
                        }
                        let ours = format!(
                            "{}X{}\r\n",
                            tokens[..a_start].concat(),
                            tokens[a_end..].concat()
                        );
                        let theirs = format!(
                            "{}Y{}\r\n",
                            tokens[..b_start].concat(),
                            tokens[b_end..].concat()
                        );
                        let mut edits = vec![
                            TokenEdit {
                                base: a_start..a_end,
                                replacement: "X".into(),
                            },
                            TokenEdit {
                                base: b_start..b_end,
                                replacement: "Y".into(),
                            },
                        ];
                        edits.sort_by_key(|edit| edit.base.start);
                        let expected = format!("{}\r\n", apply_edits(&tokens, edits).unwrap());
                        assert_merge(&base, &ours, &theirs, &expected);
                    }
                }
            }
        }
    }

    #[test]
    fn conflicts_are_explicit_and_never_return_marker_text() {
        for (base, ours, theirs) in [
            ("aaa\n", "bbb\n", "ccc\n"),
            ("ab\n", "aXb\n", "aYb\n"),
            ("abcd\n", "ad\n", "abXcd\n"),
            ("abcd\n", "ad\n", "abD\n"),
            ("one\ntwo\n", "insert\none\ntwo\n", "ONE\ntwo\n"),
            ("e\u{301}\n", "e\u{300}\n", "é\n"),
            ("👩‍👩‍👧‍👦\n", "👩‍👩‍👧\n", "👨‍👩‍👧‍👦\n"),
        ] {
            assert_eq!(
                three_way_merge(base, ours, theirs),
                MergeOutcome::Conflict,
                "{base:?}, {ours:?}, {theirs:?}"
            );
            assert_eq!(three_way_merge(base, theirs, ours), MergeOutcome::Conflict);
        }
    }

    #[test]
    fn independent_extended_graphemes_remain_whole() {
        assert_merge(
            "e\u{301} café 👩‍👩‍👧‍👦\r\n",
            "e\u{300} café 👩‍👩‍👧‍👦\r\n",
            "e\u{301} café 👨‍👩‍👧‍👦\r\n",
            "e\u{300} café 👨‍👩‍👧‍👦\r\n",
        );
    }

    #[test]
    fn preserves_crlf_trailing_empty_lines_and_missing_final_newline() {
        assert_merge(
            "a\r\nb\r\n\r\n",
            "A\r\nb\r\n\r\n",
            "a\r\nB\r\n\r\n",
            "A\r\nB\r\n\r\n",
        );
        assert_merge("a\nb\n", "A\nb\n", "a\nb", "A\nb");
        assert_merge("a\r\nb\nc", "A\r\nb\nc", "a\r\nb\nC", "A\r\nb\nC");
        assert_merge("a\rb\rc", "A\rb\rc", "a\rb\rC", "A\rb\rC");
        for source in [
            "first\nsecond\rthird\r\nfourth\nlast",
            "\r\r",
            "\r\n\n\r",
            "",
            "👩‍👩‍👧‍👦\r",
        ] {
            assert_eq!(bounded_lines(source).unwrap(), source.tokenize_lines());
        }
    }

    #[test]
    fn preserves_markdown_fences_escapes_and_table_cell_formatting() {
        let base = "\u{60}\u{60}\u{60}rust\r\nlet a = 1;\r\n\u{60}\u{60}\u{60}\r\n\r\n| A | B |\r\n| :--- | ---: |\r\n| one \\| pipe | two |\r\n";
        let ours = base.replace("one \\| pipe", "ONE \\| pipe");
        let theirs = base.replace("| two |", "| TWO |\t");
        let expected = ours.replace("| two |", "| TWO |\t");
        assert_merge(base, &ours, &theirs, &expected);
    }

    #[test]
    fn conflicting_table_cell_changes_are_not_guessed() {
        let base = "| one | two |\n";
        assert_eq!(
            three_way_merge(base, "| ours | two |\n", "| theirs | two |\n"),
            MergeOutcome::Conflict
        );
    }

    #[test]
    fn line_insertions_at_distinct_anchors_merge() {
        assert_merge(
            "aaa\nccc\n",
            "aaa\nBBB\nccc\n",
            "aaa\nccc\nDDD\n",
            "aaa\nBBB\nccc\nDDD\n",
        );
    }

    #[test]
    fn repeated_lines_with_independent_replacements_preserve_both() {
        assert_merge(
            "same\nsame\nsame\n",
            "ours\nsame\nsame\n",
            "same\nsame\ntheirs\n",
            "ours\nsame\ntheirs\n",
        );
    }

    #[test]
    fn identical_line_deletions_are_applied_only_once() {
        assert_merge(
            "a\nb\nc\nd\ne\nf\n",
            "A\nb\nc\ne\nf\n",
            "a\nb\nc\ne\nF\n",
            "A\nb\nc\ne\nF\n",
        );
    }

    #[test]
    fn ambiguous_deletions_of_repeated_lines_or_graphemes_require_review() {
        for (base, ours, theirs) in [
            ("same\nsame\n", "same\n", "same\nchanged\n"),
            (
                "same\nsame\nsame\n",
                "same\nsame\n",
                "same\nsame\nchanged\n",
            ),
            ("aba\n", "a\n", "abA\n"),
            ("aaa z\n", "aa z\n", "aaa Z\n"),
        ] {
            assert_eq!(
                three_way_merge(base, ours, theirs),
                MergeOutcome::Conflict,
                "{base:?}, {ours:?}, {theirs:?}"
            );
            assert_eq!(three_way_merge(base, theirs, ours), MergeOutcome::Conflict);
        }
    }

    #[test]
    fn overlapping_equal_text_on_different_ranges_never_deletes_a_tail_twice() {
        assert_eq!(
            three_way_merge("a\nb\nc\nd\n", "a\nd\n", "a\nb\n"),
            MergeOutcome::Conflict
        );
    }

    #[test]
    fn rejects_pathological_work_but_trims_large_unchanged_surroundings() {
        let too_large = "x".repeat(MAX_MERGE_INPUT_BYTES + 1);
        assert_eq!(
            three_way_merge(
                &too_large,
                &format!("{too_large}a"),
                &format!("{too_large}b")
            ),
            MergeOutcome::Conflict
        );
        let base = format!(
            "{}red blue green\n{}",
            "unchanged\n".repeat(2_000),
            "unchanged\n".repeat(2_000)
        );
        assert_merge(
            &base,
            &base.replace("red", "RED"),
            &base.replace("green", "GREEN"),
            &base.replace("red", "RED").replace("green", "GREEN"),
        );
        let line = "x".repeat(MAX_INLINE_GRAPHEMES + 1);
        assert_eq!(
            three_way_merge(
                &format!("a{line}b"),
                &format!("A{line}b"),
                &format!("a{line}B")
            ),
            MergeOutcome::Conflict
        );
        assert!(!within_work_limit(usize::MAX, 2, MAX_LINE_COMPARISON_WORK));
        let too_many_lines = "\n".repeat(MAX_MERGE_INPUT_LINES + 1);
        assert_eq!(
            three_way_merge(
                &too_many_lines,
                &format!("A{too_many_lines}"),
                &format!("B{too_many_lines}")
            ),
            MergeOutcome::Conflict
        );
        let many_changes = (0..1_500)
            .map(|ix| format!("line-{ix}\n"))
            .collect::<String>();
        assert_eq!(
            three_way_merge(
                &many_changes,
                &many_changes.replace("line", "ours"),
                &many_changes.replace("line", "theirs")
            ),
            MergeOutcome::Conflict
        );
    }

    #[test]
    fn many_inline_conflicts_share_one_work_budget() {
        let base = (0..100)
            .map(|ix| format!("row-{ix} red {} green\nunchanged-{ix}\n", "x".repeat(1_000)))
            .collect::<String>();
        let ours = base.replace("red", "RED");
        let theirs = base.replace("green", "GREEN");
        assert_eq!(
            three_way_merge(&base, &ours, &theirs),
            MergeOutcome::Conflict
        );
    }
}
