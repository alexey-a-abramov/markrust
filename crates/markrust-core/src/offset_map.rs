// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Map a byte offset through an external text replacement.
//!
//! Port of nimbalyst `mapOffsetAcrossChange`: each disjoint changed span is
//! applied independently so an edit on both sides of the caret does not treat
//! the untouched middle as changed.

use similar::{ChangeTag, TextDiff};

/// Move `offset` in `before` to the equivalent offset in `after`.
pub fn map_offset_across_change(before: &str, after: &str, offset: usize) -> usize {
    let target = offset.min(before.len());
    let diff = TextDiff::from_chars(before, after);
    let mut before_at = 0usize;
    let mut after_at = 0usize;
    let mut pending_removed = 0usize;
    let mut pending_added = 0usize;
    let mut pending_before_start = 0usize;
    let mut pending_after_start = 0usize;
    let mut in_change = false;

    let flush = |pending_removed: usize,
                 pending_added: usize,
                 pending_before_start: usize,
                 pending_after_start: usize,
                 target: usize|
     -> Option<usize> {
        let before_end = pending_before_start + pending_removed;
        let after_end = pending_after_start + pending_added;
        if target <= pending_before_start {
            return Some(pending_after_start);
        }
        if target < before_end {
            return Some((pending_after_start + (target - pending_before_start)).min(after_end));
        }
        if target == before_end && pending_removed > 0 {
            return Some(after_end);
        }
        None
    };

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                if in_change {
                    if let Some(mapped) = flush(
                        pending_removed,
                        pending_added,
                        pending_before_start,
                        pending_after_start,
                        target,
                    ) {
                        return mapped;
                    }
                    in_change = false;
                    pending_removed = 0;
                    pending_added = 0;
                }
                let length = change.value().len();
                if target < before_at + length {
                    return after_at + (target - before_at);
                }
                before_at += length;
                after_at += length;
            }
            ChangeTag::Delete | ChangeTag::Insert => {
                if !in_change {
                    in_change = true;
                    pending_before_start = before_at;
                    pending_after_start = after_at;
                    pending_removed = 0;
                    pending_added = 0;
                }
                let length = change.value().len();
                if change.tag() == ChangeTag::Delete {
                    pending_removed += length;
                    before_at += length;
                } else {
                    pending_added += length;
                    after_at += length;
                }
            }
        }
    }
    if in_change {
        if let Some(mapped) = flush(
            pending_removed,
            pending_added,
            pending_before_start,
            pending_after_start,
            target,
        ) {
            return mapped;
        }
    }
    after_at
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shifts_offsets_after_an_insertion() {
        assert_eq!(map_offset_across_change("abcdef", "abcXYZdef", 2), 2);
        assert_eq!(map_offset_across_change("abcdef", "abcXYZdef", 5), 8);
        assert_eq!(map_offset_across_change("abcdef", "abQQef", 3), 3);
        assert_eq!(map_offset_across_change("abcdef", "abQQef", 4), 4);
    }

    #[test]
    fn maps_through_separate_edits_on_both_sides() {
        let before = "alpha\nmiddle\nfooter";
        let after = "intro\nalpha\nmiddle\nrevised footer";
        let caret = before.find("middle").unwrap() + 3;
        assert_eq!(
            map_offset_across_change(before, after, caret),
            after.find("middle").unwrap() + 3
        );
    }

    #[test]
    fn clamps_past_end_and_identity() {
        assert_eq!(map_offset_across_change("hi", "hi", 2), 2);
        assert_eq!(map_offset_across_change("hi", "hi", 99), 2);
        assert_eq!(map_offset_across_change("", "abc", 0), 0);
        assert_eq!(map_offset_across_change("abc", "", 2), 0);
    }
}
