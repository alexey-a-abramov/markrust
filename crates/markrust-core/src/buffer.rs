// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt;
use std::str::FromStr;

use ropey::Rope;

use crate::line_index::LineIndex;

/// Rope-backed UTF-8 document buffer with revision tracking for invalidation.
#[derive(Debug, Clone)]
pub struct DocumentBuffer {
    text: Rope,
    revision: u64,
    line_index: LineIndex,
}

impl Default for DocumentBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl DocumentBuffer {
    pub fn new() -> Self {
        Self {
            text: Rope::new(),
            revision: 0,
            line_index: LineIndex::new(),
        }
    }

    pub fn with_text(content: &str) -> Self {
        let text = Rope::from_str(content);
        let line_index = LineIndex::from_rope(&text);
        Self {
            text,
            revision: 0,
            line_index,
        }
    }

    pub fn content(&self) -> String {
        self.text.to_string()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn len_chars(&self) -> usize {
        self.text.len_chars()
    }

    pub fn len_bytes(&self) -> usize {
        self.text.len_bytes()
    }

    pub fn is_empty(&self) -> bool {
        self.text.len_chars() == 0
    }

    pub fn text(&self) -> &Rope {
        &self.text
    }

    pub fn slice(&self, start_byte: usize, end_byte: usize) -> String {
        let len = self.len_bytes();
        let start = self.text.byte_to_char(start_byte.min(len));
        let end = self.text.byte_to_char(end_byte.min(len));
        if start >= end {
            return String::new();
        }
        self.text.slice(start..end).to_string()
    }

    pub fn line_index(&self) -> &LineIndex {
        &self.line_index
    }

    pub fn line_col_of_offset(&self, byte_offset: usize) -> (usize, usize) {
        self.line_index
            .line_col_of_offset(byte_offset, self.len_bytes())
    }

    pub fn offset_of_line_col(&self, line: usize, col: usize) -> usize {
        self.line_index.offset_of_line_col(line, col, &self.text)
    }

    pub fn insert(&mut self, byte_offset: usize, text: &str) -> u64 {
        let char_idx = self.text.byte_to_char(byte_offset.min(self.len_bytes()));
        self.text.insert(char_idx, text);
        self.line_index.on_insert(byte_offset, text);
        self.revision += 1;
        self.revision
    }

    pub fn delete(&mut self, start_byte: usize, end_byte: usize) -> u64 {
        let start = self.text.byte_to_char(start_byte.min(self.len_bytes()));
        let end = self.text.byte_to_char(end_byte.min(self.len_bytes()));
        if start >= end {
            return self.revision;
        }
        self.text.remove(start..end);
        self.line_index.on_delete(start_byte, end_byte);
        self.revision += 1;
        self.revision
    }

    pub fn replace(&mut self, start_byte: usize, end_byte: usize, text: &str) -> u64 {
        self.delete(start_byte, end_byte);
        self.insert(start_byte, text)
    }

    /// Rebuild the line index from scratch (e.g. after bulk external load).
    pub fn rebuild_line_index(&mut self) {
        self.line_index = LineIndex::from_rope(&self.text);
    }
}

impl fmt::Display for DocumentBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.text)
    }
}

impl From<&str> for DocumentBuffer {
    fn from(content: &str) -> Self {
        Self::with_text(content)
    }
}

impl FromStr for DocumentBuffer {
    type Err = std::convert::Infallible;

    fn from_str(content: &str) -> Result<Self, Self::Err> {
        Ok(Self::with_text(content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_slice() {
        let mut buf = DocumentBuffer::with_text("hello");
        buf.insert(5, " world");
        assert_eq!(buf.content(), "hello world");
        assert_eq!(buf.slice(0, 5), "hello");
        assert_eq!(buf.slice(6, 11), "world");
    }

    #[test]
    fn delete_range() {
        let mut buf = DocumentBuffer::with_text("hello world");
        buf.delete(5, 11);
        assert_eq!(buf.content(), "hello");
    }

    #[test]
    fn revision_increments_on_edit() {
        let mut buf = DocumentBuffer::with_text("a");
        assert_eq!(buf.revision(), 0);
        buf.insert(1, "b");
        assert_eq!(buf.revision(), 1);
        buf.delete(0, 1);
        assert_eq!(buf.revision(), 2);
    }

    #[test]
    fn empty_delete_is_noop() {
        let mut buf = DocumentBuffer::with_text("abc");
        let rev = buf.delete(2, 2);
        assert_eq!(rev, 0);
        assert_eq!(buf.content(), "abc");
    }

    fn assert_line_index_consistent(buf: &DocumentBuffer) {
        let rebuilt = LineIndex::from_rope(buf.text());
        assert_eq!(buf.line_index().line_starts(), rebuilt.line_starts());
    }

    #[test]
    fn empty_buffer_insert_at_zero() {
        let mut buf = DocumentBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.len_chars(), 0);
        assert_eq!(buf.len_bytes(), 0);
        assert_eq!(buf.revision(), 0);
        buf.insert(0, "a");
        assert!(!buf.is_empty());
        assert_eq!(buf.content(), "a");
        assert_eq!(buf.revision(), 1);
        assert_eq!(DocumentBuffer::default().content(), "");
    }

    #[test]
    fn slice_clamps_and_inverted_range_is_empty() {
        let buf = DocumentBuffer::with_text("hello");
        assert_eq!(buf.slice(0, 100), "hello");
        assert_eq!(buf.slice(5, 5), "");
        assert_eq!(buf.slice(4, 1), "");
        assert_eq!(buf.slice(0, 0), "");
    }

    #[test]
    fn insert_past_end_appends() {
        let mut buf = DocumentBuffer::with_text("ab");
        buf.insert(99, "c");
        assert_eq!(buf.content(), "abc");
    }

    #[test]
    fn delete_inverted_or_empty_is_noop() {
        let mut buf = DocumentBuffer::with_text("abc");
        assert_eq!(buf.delete(5, 1), 0);
        assert_eq!(buf.content(), "abc");
        buf.delete(0, 1);
        assert_eq!(buf.content(), "bc");
        assert_eq!(buf.revision(), 1);
    }

    #[test]
    fn replace_deletes_then_inserts() {
        let mut buf = DocumentBuffer::with_text("hello");
        let rev = buf.replace(1, 4, "i");
        assert_eq!(buf.content(), "hio");
        assert_eq!(rev, 2);
    }

    #[test]
    fn utf8_emoji_and_cjk_byte_vs_char() {
        let mut buf = DocumentBuffer::with_text("👋你好");
        assert_eq!(buf.len_chars(), 3);
        assert_eq!(buf.len_bytes(), 10);
        assert_eq!(buf.slice(0, 4), "👋");
        assert_eq!(buf.slice(4, 10), "你好");
        assert_eq!(buf.text().byte_to_char(4), 1);
        assert_eq!(buf.text().char_to_byte(1), 4);
        buf.insert(4, "!");
        assert_eq!(buf.content(), "👋!你好");
        buf.delete(4, 5);
        assert_eq!(buf.content(), "👋你好");
        assert_line_index_consistent(&buf);
    }

    #[test]
    fn accented_char_is_one_char_two_bytes() {
        let buf = DocumentBuffer::with_text("é");
        assert_eq!(buf.len_chars(), 1);
        assert_eq!(buf.len_bytes(), 2);
        assert_eq!(buf.line_col_of_offset(0), (0, 0));
        assert_eq!(buf.offset_of_line_col(0, 2), 2);
    }

    #[test]
    fn large_rope_edits_are_correct() {
        let mut buf = DocumentBuffer::with_text(&"x".repeat(100_000));
        assert_eq!(buf.len_chars(), 100_000);
        assert_eq!(buf.insert(50_000, "Y"), 1);
        assert_eq!(buf.slice(49_999, 50_002), "xYx");
        buf.delete(50_000, 50_001);
        assert_eq!(buf.len_chars(), 100_000);
        assert_eq!(buf.slice(49_999, 50_001), "xx");
        buf.insert(0, "\n");
        buf.insert(buf.len_bytes(), "\n");
        assert_line_index_consistent(&buf);
        buf.rebuild_line_index();
        assert_line_index_consistent(&buf);
    }

    #[test]
    fn revision_does_not_change_on_noop_delete() {
        let mut buf = DocumentBuffer::with_text("ab");
        assert_eq!(buf.delete(1, 1), 0);
        assert_eq!(buf.delete(8, 2), 0);
        assert_eq!(buf.revision(), 0);
        buf.insert(2, "");
        assert_eq!(buf.revision(), 1);
        assert_eq!(buf.content(), "ab");
    }

    #[test]
    fn display_and_from_str() {
        let buf: DocumentBuffer = "hello".parse().unwrap();
        assert_eq!(buf.to_string(), "hello");
        assert_eq!(DocumentBuffer::from("x").content(), "x");
    }

    #[test]
    fn line_index_tracks_newline_edits() {
        let mut buf = DocumentBuffer::with_text("ab");
        buf.insert(2, "\ncd");
        assert_eq!(buf.line_index().line_count(), 2);
        assert_eq!(buf.line_col_of_offset(3), (1, 0));
        buf.delete(2, 3);
        assert_eq!(buf.content(), "abcd");
        assert_line_index_consistent(&buf);
    }

    #[test]
    fn insert_and_delete_at_start_middle_end() {
        let mut buf = DocumentBuffer::with_text("ace");
        buf.insert(0, "!");
        assert_eq!(buf.content(), "!ace");
        buf.insert(2, "b");
        assert_eq!(buf.content(), "!abce");
        buf.insert(buf.len_bytes(), "!");
        assert_eq!(buf.content(), "!abce!");
        buf.delete(0, 1);
        buf.delete(buf.len_bytes() - 1, buf.len_bytes());
        buf.delete(1, 2);
        assert_eq!(buf.content(), "ace");
        assert_line_index_consistent(&buf);
    }

    #[test]
    fn empty_buffer_slice_and_delete_are_empty() {
        let mut buf = DocumentBuffer::new();
        assert_eq!(buf.slice(0, 0), "");
        assert_eq!(buf.slice(0, 10), "");
        assert_eq!(buf.delete(0, 4), 0);
        buf.insert(0, "");
        assert_eq!(buf.content(), "");
        assert_eq!(buf.revision(), 1);
    }

    #[test]
    fn combining_character_is_two_chars() {
        let nfd = "e\u{0301}";
        let mut buf = DocumentBuffer::with_text(nfd);
        assert_eq!(buf.len_chars(), 2);
        assert_eq!(buf.len_bytes(), 3);
        assert_eq!(buf.slice(0, 1), "e");
        assert_eq!(buf.slice(1, 3), "\u{0301}");
        buf.insert(1, "x");
        assert_eq!(buf.content(), "ex\u{0301}");
        buf.delete(1, 2);
        assert_eq!(buf.content(), nfd);
        assert_line_index_consistent(&buf);
    }

    #[test]
    fn cjk_insert_middle_and_end() {
        let mut buf = DocumentBuffer::with_text("你好");
        buf.insert(3, "，");
        assert_eq!(buf.content(), "你，好");
        buf.insert(buf.len_bytes(), "。");
        assert_eq!(buf.content(), "你，好。");
        buf.delete(0, 3);
        assert_eq!(buf.content(), "，好。");
        assert_line_index_consistent(&buf);
    }
}
