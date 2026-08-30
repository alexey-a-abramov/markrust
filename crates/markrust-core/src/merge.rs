// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Three-way line merge for dirty-tab external edits.

use std::ops::Range;

use similar::{ChangeTag, TextDiff};

/// Result of merging `ours` and `theirs` against a common `base`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// `ours` already matches `theirs`, or `theirs` still equals `base`.
    Unchanged,
    /// `ours` still equals `base`; take the disk version.
    TakeTheirs,
    /// Disjoint line edits combined cleanly.
    Merged(String),
    /// Both sides changed the same base lines differently.
    Conflict,
}

/// Merge `ours` (in-memory) and `theirs` (on disk) against `base` (last known disk).
pub fn three_way_merge(base: &str, ours: &str, theirs: &str) -> MergeOutcome {
    if ours == theirs {
        return MergeOutcome::Unchanged;
    }
    if ours == base {
        return MergeOutcome::TakeTheirs;
    }
    if theirs == base {
        return MergeOutcome::Unchanged;
    }
    match merge_lines(base, ours, theirs) {
        Some(merged) => MergeOutcome::Merged(merged),
        None => MergeOutcome::Conflict,
    }
}

#[derive(Debug, Clone)]
struct LineHunk {
    base: Range<usize>,
    new: Vec<String>,
}

fn diff_hunks(old: &str, new: &str) -> Vec<LineHunk> {
    let diff = TextDiff::from_lines(old, new);
    let mut hunks = Vec::new();
    let mut old_line = 0usize;
    let mut current: Option<LineHunk> = None;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                if let Some(hunk) = current.take() {
                    hunks.push(hunk);
                }
                old_line += 1;
            }
            ChangeTag::Delete => {
                let hunk = current.get_or_insert(LineHunk {
                    base: old_line..old_line,
                    new: Vec::new(),
                });
                hunk.base.end += 1;
                old_line += 1;
            }
            ChangeTag::Insert => {
                let hunk = current.get_or_insert(LineHunk {
                    base: old_line..old_line,
                    new: Vec::new(),
                });
                hunk.new
                    .push(change.value().trim_end_matches(['\r', '\n']).to_string());
            }
        }
    }
    if let Some(hunk) = current {
        hunks.push(hunk);
    }
    hunks
}

fn hunks_conflict(a: &LineHunk, b: &LineHunk) -> bool {
    let overlap = if a.base.start == a.base.end && b.base.start == b.base.end {
        a.base.start == b.base.start
    } else {
        a.base.start < b.base.end && b.base.start < a.base.end
    };
    overlap && a.new != b.new
}

fn merge_lines(base: &str, ours: &str, theirs: &str) -> Option<String> {
    let ours_hunks = diff_hunks(base, ours);
    let theirs_hunks = diff_hunks(base, theirs);
    for a in &ours_hunks {
        for b in &theirs_hunks {
            if hunks_conflict(a, b) {
                return None;
            }
        }
    }
    let mut combined = ours_hunks;
    for hunk in theirs_hunks {
        if !combined
            .iter()
            .any(|existing| existing.base == hunk.base && existing.new == hunk.new)
        {
            combined.push(hunk);
        }
    }
    combined.sort_by(|a, b| {
        a.base
            .start
            .cmp(&b.base.start)
            .then(a.base.end.cmp(&b.base.end))
    });
    let mut lines: Vec<String> = if base.is_empty() {
        Vec::new()
    } else {
        base.lines().map(str::to_string).collect()
    };
    // Apply from the end so earlier line indices stay valid.
    for hunk in combined.into_iter().rev() {
        let start = hunk.base.start.min(lines.len());
        let end = hunk.base.end.min(lines.len());
        lines.splice(start..end, hunk.new);
    }
    let mut merged = lines.join("\n");
    if (base.ends_with('\n') || ours.ends_with('\n') || theirs.ends_with('\n'))
        && !merged.ends_with('\n')
        && !merged.is_empty()
    {
        merged.push('\n');
    }
    Some(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_theirs_when_ours_matches_base() {
        assert_eq!(
            three_way_merge("aaa\n", "aaa\n", "bbb\n"),
            MergeOutcome::TakeTheirs
        );
    }

    #[test]
    fn ignore_when_disk_matches_ours_or_base() {
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
        let base = "aaa\nbbb\nccc\n";
        let ours = "aaa\nBBB\nccc\n";
        let theirs = "aaa\nbbb\nCCC\n";
        match three_way_merge(base, ours, theirs) {
            MergeOutcome::Merged(merged) => assert_eq!(merged, "aaa\nBBB\nCCC\n"),
            other => panic!("expected merge, got {other:?}"),
        }
    }

    #[test]
    fn conflicts_on_the_same_line() {
        assert_eq!(
            three_way_merge("aaa\n", "bbb\n", "ccc\n"),
            MergeOutcome::Conflict
        );
    }

    #[test]
    fn maps_with_insertion_on_one_side() {
        let base = "aaa\nccc\n";
        let ours = "aaa\nBBB\nccc\n";
        let theirs = "aaa\nccc\nDDD\n";
        match three_way_merge(base, ours, theirs) {
            MergeOutcome::Merged(merged) => assert_eq!(merged, "aaa\nBBB\nccc\nDDD\n"),
            other => panic!("expected merge, got {other:?}"),
        }
    }
}
