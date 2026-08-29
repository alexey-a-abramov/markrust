// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use ropey::Rope;

/// Maps byte offsets to line/column positions and back.
///
/// Columns are **byte** offsets from the start of the line so that
/// [`Self::line_col_of_offset`] and [`Self::offset_of_line_col`] round-trip
/// for every byte in the document (including UTF-8 and `\r\n`).
#[derive(Debug, Clone)]
pub struct LineIndex {
    /// Byte offset of the start of each line (always includes 0).
    line_starts: Vec<usize>,
}

impl Default for LineIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LineIndex {
    pub fn new() -> Self {
        Self {
            line_starts: vec![0],
        }
    }

    pub fn from_rope(rope: &Rope) -> Self {
        let mut index = Self::new();
        for (char_idx, ch) in rope.chars().enumerate() {
            if ch == '\n' {
                let next_byte = rope.char_to_byte(char_idx + 1);
                index.line_starts.push(next_byte);
            }
        }
        index
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    pub fn line_starts(&self) -> &[usize] {
        &self.line_starts
    }

    pub fn line_col_of_offset(&self, byte_offset: usize, doc_len_bytes: usize) -> (usize, usize) {
        let offset = byte_offset.min(doc_len_bytes);
        let line = self
            .line_starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        let line_start = self.line_starts.get(line).copied().unwrap_or(0);
        (line, offset.saturating_sub(line_start))
    }

    /// Convert `(line, col)` to a byte offset.
    ///
    /// `col` is a byte offset from the start of `line`, matching
    /// [`Self::line_col_of_offset`]. Out-of-range values are clamped to the
    /// line (the newline byte for interior lines, or `rope.len_bytes()` for the
    /// last line).
    pub fn offset_of_line_col(&self, line: usize, col: usize, rope: &Rope) -> usize {
        let doc_len = rope.len_bytes();
        if self.line_starts.is_empty() {
            return 0;
        }
        let line = line.min(self.line_starts.len() - 1);
        let line_start = self.line_starts[line];
        let max_offset = self
            .line_starts
            .get(line + 1)
            .map_or(doc_len, |next| next.saturating_sub(1));
        line_start.saturating_add(col).min(max_offset)
    }

    pub fn on_insert(&mut self, byte_offset: usize, text: &str) {
        let newline_count = text.bytes().filter(|&b| b == b'\n').count();
        if newline_count == 0 {
            for start in &mut self.line_starts {
                if *start > byte_offset {
                    *start += text.len();
                }
            }
            return;
        }

        let mut new_starts = Vec::with_capacity(self.line_starts.len() + newline_count);
        let line_start_in_insert = byte_offset;
        for (idx, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                new_starts.push(line_start_in_insert + idx + 1);
            }
        }

        for start in &self.line_starts {
            if *start < byte_offset {
                new_starts.push(*start);
            } else {
                new_starts.push(*start + text.len());
            }
        }
        new_starts.sort_unstable();
        new_starts.dedup();
        if new_starts.first() != Some(&0) {
            new_starts.insert(0, 0);
        }
        self.line_starts = new_starts;
    }

    pub fn on_delete(&mut self, start_byte: usize, end_byte: usize) {
        let deleted = end_byte.saturating_sub(start_byte);
        if deleted == 0 {
            return;
        }

        self.line_starts
            .retain(|&start| start <= start_byte || start > end_byte);
        for start in &mut self.line_starts {
            if *start >= end_byte {
                *start -= deleted;
            }
        }
        if self.line_starts.is_empty() {
            self.line_starts.push(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_round_trip() {
        let rope = Rope::from_str("abc\ndef\nghi");
        let index = LineIndex::from_rope(&rope);
        assert_eq!(index.line_count(), 3);
        assert_eq!(index.line_col_of_offset(0, rope.len_bytes()), (0, 0));
        assert_eq!(index.line_col_of_offset(4, rope.len_bytes()), (1, 0));
        assert_eq!(index.line_col_of_offset(6, rope.len_bytes()), (1, 2));
    }

    #[test]
    fn incremental_insert_newline() {
        let mut index = LineIndex::from_rope(&Rope::from_str("ab"));
        index.on_insert(2, "\ncd");
        assert_eq!(index.line_starts, vec![0, 3]);
    }

    #[test]
    fn incremental_delete_line_break() {
        let mut index = LineIndex::from_rope(&Rope::from_str("ab\ncd"));
        index.on_delete(2, 3);
        assert_eq!(index.line_starts, vec![0]);
    }

    fn assert_round_trip(text: &str) {
        let rope = Rope::from_str(text);
        let index = LineIndex::from_rope(&rope);
        let len = rope.len_bytes();
        for byte in 0..=len {
            let (line, col) = index.line_col_of_offset(byte, len);
            let back = index.offset_of_line_col(line, col, &rope);
            assert_eq!(back, byte, "round-trip failed at byte {byte} in {text:?}");
        }
    }

    fn assert_matches_rebuild(index: &LineIndex, rope: &Rope) {
        let rebuilt = LineIndex::from_rope(rope);
        assert_eq!(index.line_starts(), rebuilt.line_starts());
    }

    #[test]
    fn empty_file_is_a_single_line() {
        let rope = Rope::from_str("");
        let index = LineIndex::from_rope(&rope);
        assert_eq!(index.line_count(), 1);
        assert_eq!(index.line_starts(), &[0]);
        assert_eq!(index.line_col_of_offset(0, 0), (0, 0));
        assert_eq!(index.offset_of_line_col(0, 0, &rope), 0);
        assert_eq!(LineIndex::default().line_starts(), &[0]);
        assert_round_trip("");
    }

    #[test]
    fn trailing_newline_adds_empty_last_line() {
        let rope = Rope::from_str("abc\n");
        let index = LineIndex::from_rope(&rope);
        assert_eq!(index.line_count(), 2);
        assert_eq!(index.line_starts(), &[0, 4]);
        assert_eq!(index.line_col_of_offset(4, rope.len_bytes()), (1, 0));
        assert_round_trip("abc\n");
        assert_round_trip("abc\n\n");
    }

    #[test]
    fn last_line_without_newline() {
        let rope = Rope::from_str("abc\ndef");
        let index = LineIndex::from_rope(&rope);
        assert_eq!(index.line_count(), 2);
        assert_eq!(index.line_col_of_offset(7, rope.len_bytes()), (1, 3));
        assert_eq!(index.offset_of_line_col(1, 3, &rope), 7);
        assert_round_trip("abc\ndef");
    }

    #[test]
    fn crlf_splits_on_lf_only() {
        let text = "ab\r\ncd";
        let rope = Rope::from_str(text);
        let index = LineIndex::from_rope(&rope);
        assert_eq!(index.line_starts(), &[0, 4]);
        assert_eq!(index.line_col_of_offset(2, rope.len_bytes()), (0, 2));
        assert_eq!(index.line_col_of_offset(3, rope.len_bytes()), (0, 3));
        assert_eq!(index.line_col_of_offset(4, rope.len_bytes()), (1, 0));
        assert_round_trip(text);
    }

    #[test]
    fn utf8_columns_are_byte_offsets() {
        let text = "👋\n你好";
        let rope = Rope::from_str(text);
        let index = LineIndex::from_rope(&rope);
        assert_eq!(index.line_starts(), &[0, 5]);
        assert_eq!(index.line_col_of_offset(0, rope.len_bytes()), (0, 0));
        assert_eq!(index.offset_of_line_col(1, 3, &rope), 8);
        assert_round_trip(text);
    }

    #[test]
    fn offset_of_line_col_clamps() {
        let rope = Rope::from_str("ab\ncd");
        let index = LineIndex::from_rope(&rope);
        assert_eq!(index.offset_of_line_col(0, 99, &rope), 2);
        assert_eq!(index.offset_of_line_col(1, 99, &rope), 5);
        assert_eq!(index.offset_of_line_col(9, 0, &rope), 3);
    }

    #[test]
    fn incremental_edits_match_rebuild() {
        let mut rope = Rope::from_str("ab");
        let mut index = LineIndex::from_rope(&rope);
        index.on_insert(2, "\ncd\n");
        rope.insert(2, "\ncd\n");
        assert_matches_rebuild(&index, &rope);

        index.on_insert(0, "xy");
        rope.insert(0, "xy");
        assert_matches_rebuild(&index, &rope);

        index.on_delete(2, 5);
        rope.remove(2..5);
        assert_matches_rebuild(&index, &rope);
        assert_round_trip(&rope.to_string());
    }

    #[test]
    fn line_col_byte_round_trip_table() {
        for text in [
            "",
            "a",
            "abc",
            "abc\n",
            "abc\ndef",
            "abc\ndef\n",
            "\n",
            "\n\n",
            "ab\r\ncd\r\n",
            "👋你好\n世界",
            "a\nb\nc",
            "e\u{0301}\ncafe\u{0301}",
        ] {
            assert_round_trip(text);
        }
    }
}
