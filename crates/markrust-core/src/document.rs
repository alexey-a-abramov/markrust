// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::buffer::DocumentBuffer;
use crate::mode::DocumentProcessingMode;
use crate::parser::{BackgroundMarkdownParser, ParseSnapshot, ParseUpdate};
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
        let mut doc = Self {
            path: None,
            buffer: DocumentBuffer::with_text(content),
            mode: DocumentProcessingMode::MarkdownWysiwyg,
            dirty: false,
            syntax_spans: Vec::new(),
            parsed_revision: 0,
            undo: UndoStack::new(),
            parser: BackgroundMarkdownParser::new(),
        };
        doc.schedule_parse();
        doc.apply_pending_parse();
        doc
    }

    pub fn plain_text(content: &str) -> Self {
        let mut doc = Self::new(content);
        doc.mode = DocumentProcessingMode::PlainText;
        doc.syntax_spans.clear();
        doc
    }

    pub fn revision(&self) -> u64 {
        self.buffer.revision()
    }

    pub fn undo_stack(&self) -> &UndoStack {
        &self.undo
    }

    pub fn insert(&mut self, byte_offset: usize, text: &str) {
        self.undo.push_single(EditOperation::Insert {
            byte_offset,
            text: text.to_string(),
        });
        self.buffer.insert(byte_offset, text);
        self.dirty = true;
        self.schedule_parse();
    }

    pub fn delete(&mut self, start_byte: usize, end_byte: usize) {
        let deleted = self.buffer.slice(start_byte, end_byte);
        if deleted.is_empty() {
            return;
        }
        self.undo.push_single(EditOperation::Delete {
            byte_offset: start_byte,
            text: deleted,
        });
        self.buffer.delete(start_byte, end_byte);
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
        if latest.revision >= self.parsed_revision {
            self.syntax_spans = latest.spans.clone();
            self.parsed_revision = latest.revision;
        }
        Some(latest)
    }

    pub fn from_file(path: PathBuf) -> io::Result<Self> {
        let content = fs::read_to_string(&path)?;
        let mut doc = Self::new(&content);
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

    #[test]
    fn undo_redo_edits_buffer() {
        let mut doc = Document::new("hello");
        doc.insert(5, " world");
        assert_eq!(doc.buffer.content(), "hello world");
        doc.undo();
        assert_eq!(doc.buffer.content(), "hello");
        doc.redo();
        assert_eq!(doc.buffer.content(), "hello world");
    }

    #[test]
    fn plain_text_skips_spans() {
        let doc = Document::plain_text("# not a heading");
        assert!(doc.syntax_spans.is_empty());
    }

    #[test]
    fn save_and_reload_round_trip() {
        let dir = std::env::temp_dir().join("markrust-test-save");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("note.md");

        let mut doc = Document::new("# Hello");
        doc.path = Some(path.clone());
        doc.insert(7, " world");
        doc.save_and_mark_clean().unwrap();
        assert!(!doc.dirty);

        let reloaded = Document::from_file(path).unwrap();
        assert_eq!(reloaded.buffer.content(), "# Hello world");
        assert!(!reloaded.dirty);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
