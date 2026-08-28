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
        let start = self.text.byte_to_char(start_byte.min(self.len_bytes()));
        let end = self.text.byte_to_char(end_byte.min(self.len_bytes()));
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
}
