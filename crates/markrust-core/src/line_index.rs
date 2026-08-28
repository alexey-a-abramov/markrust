// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use ropey::Rope;

/// Maps byte offsets to line/column positions and back.
#[derive(Debug, Clone, Default)]
pub struct LineIndex {
    /// Byte offset of the start of each line (always includes 0).
    line_starts: Vec<usize>,
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

    pub fn offset_of_line_col(&self, line: usize, col: usize, rope: &Rope) -> usize {
        let line_start = self.line_starts.get(line).copied().unwrap_or(0);
        let char_idx = rope.byte_to_char(line_start);
        let line_end_char = if line + 1 < self.line_starts.len() {
            rope.byte_to_char(self.line_starts[line + 1])
        } else {
            rope.len_chars()
        };
        let target_char = (char_idx + col).min(line_end_char.saturating_sub(1).max(char_idx));
        rope.char_to_byte(target_char)
    }

    pub fn on_insert(&mut self, byte_offset: usize, text: &str) {
        let newline_count = text.bytes().filter(|&b| b == b'\n').count();
        if newline_count == 0 {
            for start in &mut self.line_starts {
                if *start >= byte_offset {
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
}
