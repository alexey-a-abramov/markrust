// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Save candidates for the Normalize review dialog.
//!
//! The default save path writes the buffer verbatim (block preservation is
//! structural — untouched blocks are untouched bytes). This module computes
//! the alternative "house style" serialization and a line diff between the
//! two so the view can offer *Keep original / Normalize / Cancel* with a
//! preview.

use similar::{ChangeTag, TextDiff};

use crate::document::Document;

use super::engine::RichEngine;
use super::serialize::{serialize_tree, SerializeMode};

/// One line-level difference region between preserved and normalized output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffHunk {
    /// Line range (0-based, in the preserved text) that is identical.
    Equal(std::ops::Range<usize>),
    /// `old` lines in the preserved text replaced by `new` lines in the
    /// normalized text.
    Replace {
        old: std::ops::Range<usize>,
        new: std::ops::Range<usize>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SaveCandidates {
    /// The buffer bytes, verbatim — the default save.
    pub preserved: String,
    /// Full house-style serialization.
    pub normalized: String,
    /// Line diff preserved → normalized.
    pub hunks: Vec<DiffHunk>,
}

impl SaveCandidates {
    /// True when normalizing would change anything at all.
    pub fn differs(&self) -> bool {
        self.preserved != self.normalized
    }
}

/// Compute both save candidates for the document.
pub fn save_candidates(doc: &Document, engine: &mut RichEngine) -> SaveCandidates {
    let preserved = doc.buffer.content();
    let tree = engine.sync(doc).clone();
    let normalized = serialize_tree(
        &tree,
        &preserved,
        SerializeMode::Normalize,
        &Default::default(),
    );
    let hunks = line_hunks(&preserved, &normalized);
    SaveCandidates {
        preserved,
        normalized,
        hunks,
    }
}

fn line_hunks(old: &str, new: &str) -> Vec<DiffHunk> {
    let diff = TextDiff::from_lines(old, new);
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let (mut old_line, mut new_line) = (0usize, 0usize);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                if let Some(DiffHunk::Equal(range)) = hunks.last_mut() {
                    range.end += 1;
                } else {
                    hunks.push(DiffHunk::Equal(old_line..old_line + 1));
                }
                old_line += 1;
                new_line += 1;
            }
            ChangeTag::Delete => {
                if let Some(DiffHunk::Replace { old, .. }) = hunks.last_mut() {
                    old.end += 1;
                } else {
                    hunks.push(DiffHunk::Replace {
                        old: old_line..old_line + 1,
                        new: new_line..new_line,
                    });
                }
                old_line += 1;
            }
            ChangeTag::Insert => {
                if let Some(DiffHunk::Replace { new, .. }) = hunks.last_mut() {
                    new.end += 1;
                } else {
                    hunks.push(DiffHunk::Replace {
                        old: old_line..old_line,
                        new: new_line..new_line + 1,
                    });
                }
                new_line += 1;
            }
        }
    }
    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserved_equals_buffer_and_differs_only_when_style_deviates() {
        let source = "Title\n=====\n\npara\n";
        let doc = Document::new(source);
        let mut engine = RichEngine::new();
        let candidates = save_candidates(&doc, &mut engine);
        assert_eq!(candidates.preserved, source);
        // House style rewrites single-line setext headings to ATX.
        assert!(candidates.normalized.contains("# Title"));
        assert!(candidates.differs());
        assert!(candidates
            .hunks
            .iter()
            .any(|h| matches!(h, DiffHunk::Replace { .. })));
    }

    #[test]
    fn no_hunks_differ_when_already_house_style() {
        let source = "# H\n\npara\n";
        let doc = Document::new(source);
        let mut engine = RichEngine::new();
        let candidates = save_candidates(&doc, &mut engine);
        assert_eq!(candidates.preserved, candidates.normalized);
        assert!(!candidates.differs());
    }
}
