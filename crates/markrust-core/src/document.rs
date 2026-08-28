// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::buffer::DocumentBuffer;
use crate::mode::DocumentProcessingMode;
use crate::parser::{extract_syntax_spans, BackgroundMarkdownParser, ParseSnapshot, ParseUpdate};
use crate::spans::SyntaxNodeSpan;
use crate::undo::{EditOperation, UndoStack};

/// A document with buffer, processing mode, syntax spans, and undo history.
#[derive(Debug)]
pub struct Document {
    pub path: Option<PathBuf>,
    pub buffer: DocumentBuffer,
    pub mode: DocumentProcessingMode,
    pub dirty: bool,
    pub syntax_spans: Vec<SyntaxNodeSpan>,
    pub parsed_revision: u64,
    undo: UndoStack,
    parser: BackgroundMarkdownParser,
}

impl Document {
    pub fn new(content: &str) -> Self {
        Self::from_parts(
            DocumentBuffer::with_text(content),
            DocumentProcessingMode::MarkdownWysiwyg,
        )
    }

    pub fn plain_text(content: &str) -> Self {
        Self::from_parts(
            DocumentBuffer::with_text(content),
            DocumentProcessingMode::PlainText,
        )
    }

    fn from_parts(buffer: DocumentBuffer, mode: DocumentProcessingMode) -> Self {
        let mut doc = Self {
            path: None,
            buffer,
            mode,
            dirty: false,
            syntax_spans: Vec::new(),
            parsed_revision: 0,
            undo: UndoStack::new(),
            parser: BackgroundMarkdownParser::new(),
        };
        if mode.parses_markdown() && !doc.buffer.is_empty() {
            let content = doc.buffer.content();
            doc.syntax_spans = extract_syntax_spans(&content);
            doc.parsed_revision = doc.revision();
        }
        doc.schedule_parse();
        doc.apply_pending_parse();
        doc
    }

    pub fn revision(&self) -> u64 {
        self.buffer.revision()
    }

    pub fn undo_stack(&self) -> &UndoStack {
        &self.undo
    }

    pub fn insert(&mut self, byte_offset: usize, text: &str) {
        self.replace_range(byte_offset, byte_offset, text);
    }

    pub fn delete(&mut self, start_byte: usize, end_byte: usize) {
        self.replace_range(start_byte, end_byte, "");
    }

    /// Delete `[start, end)` and insert `text` as a single undo transaction.
    pub fn replace_range(&mut self, start: usize, end: usize, text: &str) {
        let len = self.buffer.len_bytes();
        let start = start.min(len);
        let end = end.min(len);
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        if start == end && text.is_empty() {
            return;
        }
        self.undo.begin_transaction();
        if start < end {
            let deleted = self.buffer.slice(start, end);
            if !deleted.is_empty() {
                self.undo.record(EditOperation::Delete {
                    byte_offset: start,
                    text: deleted,
                });
                self.buffer.delete(start, end);
            }
        }
        if !text.is_empty() {
            self.undo.record(EditOperation::Insert {
                byte_offset: start,
                text: text.to_string(),
            });
            self.buffer.insert(start, text);
        }
        self.undo.commit_transaction();
        self.dirty = true;
        self.schedule_parse();
    }

    pub fn undo(&mut self) -> bool {
        let Some(ops) = self.undo.undo() else {
            return false;
        };
        self.apply_ops(&ops);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(ops) = self.undo.redo() else {
            return false;
        };
        self.apply_ops(&ops);
        true
    }

    fn apply_ops(&mut self, ops: &[EditOperation]) {
        for op in ops {
            match op {
                EditOperation::Insert { byte_offset, text } => {
                    self.buffer.insert(*byte_offset, text);
                }
                EditOperation::Delete { byte_offset, text } => {
                    self.buffer.delete(*byte_offset, byte_offset + text.len());
                }
            }
        }
        self.dirty = true;
        self.schedule_parse();
    }

    pub fn schedule_parse(&mut self) {
        if !self.mode.parses_markdown() {
            self.syntax_spans.clear();
            self.parsed_revision = self.revision();
            return;
        }
        self.parser.request_parse(ParseSnapshot {
            revision: self.revision(),
            text: self.buffer.content(),
        });
    }

    pub fn apply_pending_parse(&mut self) -> Option<ParseUpdate> {
        let updates = self.parser.drain_updates();
        let latest = updates.into_iter().last()?;
        if !self.mode.parses_markdown() {
            self.syntax_spans.clear();
            self.parsed_revision = self.revision();
            return Some(latest);
        }
        if latest.revision >= self.parsed_revision {
            let ignore_empty_clobber = latest.spans.is_empty()
                && !self.syntax_spans.is_empty()
                && !self.buffer.is_empty()
                && latest.revision == self.parsed_revision;
            if !ignore_empty_clobber {
                self.syntax_spans = latest.spans.clone();
                self.parsed_revision = latest.revision;
            }
        }
        Some(latest)
    }

    /// Block until syntax spans match the current buffer revision (or PlainText).
    pub fn wait_for_parse(&mut self, timeout: Duration) -> bool {
        if !self.mode.parses_markdown() {
            self.apply_pending_parse();
            self.syntax_spans.clear();
            self.parsed_revision = self.revision();
            return true;
        }
        let revision = self.revision();
        match self.parser.wait_for_revision(revision, timeout) {
            Some(update) => {
                if update.revision >= self.parsed_revision {
                    self.syntax_spans = update.spans;
                    self.parsed_revision = update.revision;
                }
                self.parsed_revision >= revision
            }
            None => {
                self.apply_pending_parse();
                self.parsed_revision >= revision
            }
        }
    }

    /// Set processing mode from a file path and reschedule parsing.
    pub fn configure_mode_from_path(&mut self, path: &Path) {
        self.mode = DocumentProcessingMode::from_path(path);
        self.schedule_parse();
    }

    pub fn from_file(path: PathBuf) -> io::Result<Self> {
        let content = fs::read_to_string(&path)?;
        let mode = DocumentProcessingMode::from_path(&path);
        let mut doc = Self::from_parts(DocumentBuffer::with_text(&content), mode);
        doc.path = Some(path);
        doc.dirty = false;
        Ok(doc)
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "document has no path",
            ));
        };
        atomic_write(path, &self.buffer.content())
    }

    pub fn save_and_mark_clean(&mut self) -> io::Result<()> {
        self.save()?;
        self.mark_clean();
        Ok(())
    }

    pub fn save_as(&mut self, path: PathBuf) -> io::Result<()> {
        atomic_write(&path, &self.buffer.content())?;
        self.path = Some(path);
        self.dirty = false;
        Ok(())
    }

    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    pub fn set_path(&mut self, path: Option<PathBuf>) {
        self.path = path;
    }

    pub fn replace_content(&mut self, content: &str) {
        self.buffer = DocumentBuffer::with_text(content);
        self.dirty = false;
        self.undo = UndoStack::new();
        self.schedule_parse();
        self.apply_pending_parse();
    }

    pub fn word_count(&self) -> usize {
        self.buffer
            .content()
            .split_whitespace()
            .filter(|word| !word.is_empty())
            .count()
    }
}

fn atomic_write(path: &Path, content: &str) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temp_path = path.with_extension("markrust-tmp");
    fs::write(&temp_path, content)?;
    fs::rename(temp_path, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spans::SyntaxKind;
    use crate::test_support::{TempDir, PARSE_TIMEOUT};

    #[test]
    fn undo_redo_edits_buffer() {
        let mut doc = Document::new("hello");
        doc.insert(5, " world");
        assert_eq!(doc.buffer.content(), "hello world");
        assert!(doc.dirty);
        doc.undo();
        assert_eq!(doc.buffer.content(), "hello");
        doc.redo();
        assert_eq!(doc.buffer.content(), "hello world");
    }

    #[test]
    fn replace_range_groups_delete_and_insert() {
        let mut doc = Document::new("alpha beta");
        doc.replace_range(0, 5, "gamma");
        assert_eq!(doc.buffer.content(), "gamma beta");
        assert_eq!(doc.undo_stack().undo_depth(), 1);
        assert!(doc.undo());
        assert_eq!(doc.buffer.content(), "alpha beta");
        assert!(doc.redo());
        assert_eq!(doc.buffer.content(), "gamma beta");
    }

    #[test]
    fn empty_undo_returns_false() {
        let mut doc = Document::new("hello");
        assert!(!doc.undo());
        assert!(!doc.redo());
        assert_eq!(doc.buffer.content(), "hello");
        assert!(!doc.dirty);
    }

    #[test]
    fn undo_after_new_edit_clears_redo() {
        let mut doc = Document::new("a");
        doc.insert(1, "b");
        doc.undo();
        assert!(doc.undo_stack().can_redo());
        doc.insert(1, "c");
        assert!(!doc.undo_stack().can_redo());
        assert_eq!(doc.buffer.content(), "ac");
    }

    #[test]
    fn plain_text_skips_spans() {
        let doc = Document::plain_text("# not a heading");
        assert_eq!(doc.mode, DocumentProcessingMode::PlainText);
        assert!(doc.syntax_spans.is_empty());
    }

    #[test]
    fn new_parses_markdown_immediately() {
        let doc = Document::new("# Hello\n\n**bold**");
        assert!(doc
            .syntax_spans
            .iter()
            .any(|span| span.kind == SyntaxKind::Heading));
        assert!(doc
            .syntax_spans
            .iter()
            .any(|span| span.kind == SyntaxKind::Bold));
    }

    #[test]
    fn configure_mode_from_path_switches() {
        let mut doc = Document::new("# Hello\n\n**bold**");
        assert!(
            !doc.syntax_spans.is_empty(),
            "expected markdown spans for a heading"
        );
        doc.configure_mode_from_path(Path::new("notes.txt"));
        assert_eq!(doc.mode, DocumentProcessingMode::PlainText);
        assert!(doc.syntax_spans.is_empty());

        doc.configure_mode_from_path(Path::new("notes.md"));
        assert_eq!(doc.mode, DocumentProcessingMode::MarkdownWysiwyg);
        assert!(doc.wait_for_parse(PARSE_TIMEOUT));
        assert!(doc
            .syntax_spans
            .iter()
            .any(|span| span.kind == SyntaxKind::Heading));
    }

    #[test]
    fn concurrent_edit_and_parse() {
        let mut doc = Document::new("# Title");
        doc.insert(7, "\n\n**bold**");
        assert!(doc.wait_for_parse(PARSE_TIMEOUT), "parse timed out");
        assert_eq!(doc.parsed_revision, doc.revision());
        assert!(doc
            .syntax_spans
            .iter()
            .any(|span| span.kind == SyntaxKind::Heading));
        assert!(doc
            .syntax_spans
            .iter()
            .any(|span| span.kind == SyntaxKind::Bold));
    }

    #[test]
    fn save_and_reload_round_trip() {
        let dir = TempDir::new("doc-save");
        let path = dir.join("note.md");

        let mut doc = Document::new("# Hello");
        doc.path = Some(path.clone());
        doc.insert(7, " world");
        assert!(doc.dirty);
        doc.save_and_mark_clean().unwrap();
        assert!(!doc.dirty);

        let reloaded = Document::from_file(path).unwrap();
        assert_eq!(reloaded.buffer.content(), "# Hello world");
        assert!(!reloaded.dirty);
        assert_eq!(reloaded.mode, DocumentProcessingMode::MarkdownWysiwyg);
    }

    #[test]
    fn from_file_configures_plain_text_mode() {
        let dir = TempDir::new("doc-txt");
        let path = dir.join("notes.txt");
        std::fs::write(&path, "# not markdown mode").unwrap();
        let doc = Document::from_file(path).unwrap();
        assert_eq!(doc.mode, DocumentProcessingMode::PlainText);
        assert!(doc.syntax_spans.is_empty());
        assert!(!doc.dirty);
    }

    #[test]
    fn from_file_missing_path_errors() {
        let err = Document::from_file(PathBuf::from("/no/such/markrust-doc.md")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn save_without_path_errors() {
        let doc = Document::new("x");
        let err = doc.save().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn word_count_table() {
        let cases: &[(&str, usize)] = &[
            ("", 0),
            ("   \n\t", 0),
            ("hello", 1),
            ("hello world", 2),
            ("  one   two  three ", 3),
        ];
        for &(text, expected) in cases {
            assert_eq!(Document::new(text).word_count(), expected, "text {text:?}");
        }
    }

    #[test]
    fn replace_content_resets_undo_and_dirty() {
        let mut doc = Document::new("hello");
        doc.insert(5, "!");
        assert!(doc.dirty);
        doc.replace_content("# Reset");
        assert!(!doc.dirty);
        assert!(!doc.undo_stack().can_undo());
        assert_eq!(doc.buffer.content(), "# Reset");
        assert_eq!(doc.word_count(), 2);
    }
}
