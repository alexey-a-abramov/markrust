// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Literal document search and passive source-addressed paint state.

use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHighlights {
    pub revision: u64,
    pub ranges: Vec<Range<usize>>,
    pub active: Option<usize>,
}

pub fn match_color(current: bool) -> gpui::Hsla {
    if current {
        gpui::rgba(0xe69b36a6).into()
    } else {
        gpui::rgba(0xe6bb3d55).into()
    }
}

/// Lowercase matching retains original UTF-8 byte boundaries, including
/// lowercase expansions such as U+0130. A partial expansion is not a match.
pub fn literal_matches(source: &str, query: &str) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    // Use the same scalar mapping on both sides. String::to_lowercase has
    // contextual final-sigma behavior that scalar lowercase does not share.
    let query: String = query.chars().flat_map(char::to_lowercase).collect();
    let mut folded = String::with_capacity(source.len());
    let mut boundaries = Vec::with_capacity(source.chars().count() + 1);
    for (offset, ch) in source.char_indices() {
        boundaries.push((folded.len(), offset));
        folded.extend(ch.to_lowercase());
    }
    boundaries.push((folded.len(), source.len()));
    folded
        .match_indices(&query)
        .filter_map(|(start, text)| {
            let end = start + text.len();
            let start = boundaries
                .binary_search_by_key(&start, |entry| entry.0)
                .ok()?;
            let end = boundaries
                .binary_search_by_key(&end, |entry| entry.0)
                .ok()?;
            Some(boundaries[start].1..boundaries[end].1)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_search_is_case_insensitive_and_source_addressed() {
        let source = "# СЛОВО\n\nслово *слово* 👨‍👩‍👧‍👦";
        let ranges = literal_matches(source, "Слово");
        assert_eq!(ranges.len(), 3);
        assert_eq!(&source[ranges[0].clone()], "СЛОВО");
        assert_eq!(&source[ranges[1].clone()], "слово");
        let emoji = literal_matches(source, "👨‍👩‍👧‍👦");
        assert_eq!(emoji.len(), 1);
        assert_eq!(&source[emoji[0].clone()], "👨‍👩‍👧‍👦");
    }

    #[test]
    fn lowercase_expansions_do_not_corrupt_original_ranges() {
        assert_eq!(literal_matches("İ I i", "i"), vec![3..4, 5..6]);
        assert_eq!(literal_matches("İ", "İ"), vec![0..2]);
        assert!(literal_matches("İ", "\u{307}").is_empty());
    }

    #[test]
    fn uppercase_greek_query_uses_the_same_fold_as_the_source() {
        assert_eq!(literal_matches("ΟΣ οσ", "ΟΣ"), vec![0..4, 5..9]);
        assert_eq!(literal_matches("ΟΣ", "οσ"), vec![0..4]);
    }

    #[test]
    fn literal_search_handles_empty_missing_and_nonoverlapping_results() {
        assert!(literal_matches("text", "").is_empty());
        assert!(literal_matches("text", "absent").is_empty());
        assert_eq!(literal_matches("aaaa", "aa"), vec![0..2, 2..4]);
        assert_eq!(literal_matches("[a] .", "[a]"), vec![0..3]);
    }
}
