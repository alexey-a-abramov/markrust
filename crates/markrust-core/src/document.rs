// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::buffer::DocumentBuffer;
use crate::mode::DocumentProcessingMode;
use crate::offset_map::map_offset_across_change;
use crate::parser::{BackgroundMarkdownParser, ParseSnapshot, ParseUpdate};
use crate::spans::SyntaxNodeSpan;
use crate::undo::{
    is_typing_burst, EditOperation, SelectionSnapshot, Transaction, TransactionKind, UndoStack,
};

/// A document with buffer, processing mode, syntax spans, and undo history.
#[derive(Debug)]
pub struct Document {
    pub path: Option<PathBuf>,
    pub buffer: DocumentBuffer,
    pub mode: DocumentProcessingMode,
    pub dirty: bool,
    pub syntax_spans: Vec<SyntaxNodeSpan>,
    pub parsed_revision: u64,
    saved_content: String,
    undo: UndoStack,
    coalescing_blocked: bool,
    parser: BackgroundMarkdownParser,
}

/// A compound editor action's history boundary. Finish on the same document
/// after all of the action's splices have been recorded.
#[derive(Debug)]
pub struct UndoGroupCheckpoint {
    depth: usize,
    selection_before: SelectionSnapshot,
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
        let saved_content = buffer.content();
        let mut doc = Self {
            path: None,
            buffer,
            mode,
            dirty: false,
            syntax_spans: Vec::new(),
            parsed_revision: 0,
            saved_content,
            undo: UndoStack::new(),
            coalescing_blocked: false,
            parser: BackgroundMarkdownParser::new(),
        };
        // Parse on the background worker only. Callers on the GPUI UI thread
        // must never wait here — drain with `apply_pending_parse` (non-blocking)
        // or `wait_for_parse` from tests.
        doc.schedule_parse();
        doc
    }

    pub fn revision(&self) -> u64 {
        self.buffer.revision()
    }

    pub fn undo_stack(&self) -> &UndoStack {
        &self.undo
    }

    /// Start an action such as Paste that must remain separate from typing.
    /// Nested selection replacements can safely record their own groups;
    /// this checkpoint does not replace an open undo transaction.
    pub fn begin_undo_group(&mut self, selection_before: SelectionSnapshot) -> UndoGroupCheckpoint {
        self.coalescing_blocked = true;
        UndoGroupCheckpoint {
            depth: self.undo.undo_depth(),
            selection_before,
        }
    }

    /// Consolidate this action's recorded splices and prevent later typing
    /// from joining them. An action with no document edits adds no history.
    pub fn finish_undo_group(
        &mut self,
        checkpoint: UndoGroupCheckpoint,
        selection_after: SelectionSnapshot,
    ) {
        self.undo.group_since(
            checkpoint.depth,
            checkpoint.selection_before,
            selection_after,
        );
        self.coalescing_blocked = true;
    }

    /// Last reconciled on-disk snapshot (load, save, or last merged disk bytes).
    pub fn saved_content(&self) -> &str {
        &self.saved_content
    }

    /// Undo the last transaction and drop it from redo, as if it never happened.
    /// Used to fold a typing prefix into a following input-rule rewrite.
    pub fn revert_last_quietly(&mut self) -> Option<Transaction> {
        let tx = self.undo_tx()?;
        self.undo.discard_redo();
        Some(tx)
    }

    /// Peel `range` out of the last Typing insert so an input-rule Replace can
    /// own those bytes. Works when the prefix is a slice of a longer coalesced
    /// Typing transaction, not only when it *is* that transaction.
    ///
    /// On success the buffer no longer contains `range` and the caller should
    /// insert at `range.start`. Returns the caret to restore if the following
    /// command is undone.
    pub fn peel_typing_range(
        &mut self,
        range: std::ops::Range<usize>,
    ) -> Option<SelectionSnapshot> {
        // A pasted input-rule trigger must not absorb typing from before
        // the explicit Paste boundary into its rewrite/undo step.
        if self.coalescing_blocked {
            return None;
        }
        let last = self.undo.last()?;
        if last.kind != TransactionKind::Typing {
            return None;
        }
        let (byte_offset, text_len) = match last.ops.as_slice() {
            [EditOperation::Insert { byte_offset, text }] => (*byte_offset, text.len()),
            _ => return None,
        };
        let typing_end = byte_offset + text_len;
        if range.start < byte_offset || range.end > typing_end || range.start > range.end {
            return None;
        }
        if range.start == byte_offset && range.end == typing_end {
            let tx = self.revert_last_quietly()?;
            return Some(tx.selection_after);
        }
        let last = self.undo.last_mut()?;
        let EditOperation::Insert { byte_offset, text } = last.ops.first_mut()? else {
            return None;
        };
        let local_s = range.start - *byte_offset;
        let local_e = range.end - *byte_offset;
        if local_s > text.len() || local_e > text.len() || local_s > local_e {
            return None;
        }
        text.replace_range(local_s..local_e, "");
        last.selection_after = SelectionSnapshot::collapsed(range.start);
        self.buffer.delete(range.start, range.end);
        self.dirty = true;
        self.schedule_parse();
        Some(SelectionSnapshot::collapsed(range.start))
    }

    /// Replace the buffer with on-disk bytes from an external editor, mapping
    /// each caret in `offsets` across the change. Clears undo (the old ops
    /// would not invert against the new text). Parse stays on the worker.
    pub fn apply_external_edit(&mut self, new_content: &str, offsets: &[usize]) -> Vec<usize> {
        let old = self.buffer.content();
        let mapped = offsets
            .iter()
            .map(|offset| map_offset_across_change(&old, new_content, *offset))
            .collect();
        if old == new_content {
            return mapped;
        }
        self.buffer = DocumentBuffer::with_text(new_content);
        self.dirty = false;
        self.saved_content = new_content.to_string();
        self.undo = UndoStack::new();
        self.schedule_parse();
        mapped
    }

    /// Apply a 3-way merge of dirty in-memory edits with on-disk bytes.
    /// Keeps the tab dirty when `merged` still differs from `disk`.
    /// `disk` becomes the last-known disk snapshot so the same change is not
    /// merged twice. Undo is cleared (ops would not invert against the merge).
    pub fn apply_merged_edit(&mut self, merged: &str, disk: &str, offsets: &[usize]) -> Vec<usize> {
        let old = self.buffer.content();
        let mapped = offsets
            .iter()
            .map(|offset| map_offset_across_change(&old, merged, *offset))
            .collect();
        if old != merged {
            self.buffer = DocumentBuffer::with_text(merged);
            self.undo = UndoStack::new();
            self.schedule_parse();
        }
        self.dirty = merged != disk;
        self.saved_content = disk.to_string();
        mapped
    }

    pub fn insert(&mut self, byte_offset: usize, text: &str) {
        self.replace_range(byte_offset, byte_offset, text);
    }

    pub fn delete(&mut self, start_byte: usize, end_byte: usize) {
        self.replace_range(start_byte, end_byte, "");
    }

    /// Delete `[start, end)` and insert `text` as a single undo transaction.
    pub fn replace_range(&mut self, start: usize, end: usize, text: &str) {
        let before = SelectionSnapshot {
            start,
            end,
            reversed: false,
        };
        let after = SelectionSnapshot::collapsed(start.min(end) + text.len());
        self.replace_range_tx(start, end, text, TransactionKind::Command, before, after);
    }

    /// Like [`replace_range`] with explicit undo kind and caret snapshots.
    /// Consecutive [`TransactionKind::Typing`] inserts at the growing caret
    /// coalesce into one undo step; consecutive [`TransactionKind::DeleteBack`]
    /// deletes do the same.
    pub fn replace_range_tx(
        &mut self,
        start: usize,
        end: usize,
        text: &str,
        kind: TransactionKind,
        selection_before: SelectionSnapshot,
        selection_after: SelectionSnapshot,
    ) {
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

        if !std::mem::take(&mut self.coalescing_blocked)
            && self.try_coalesce(start, end, text, kind, selection_after)
        {
            if start < end {
                self.buffer.delete(start, end);
            }
            if !text.is_empty() {
                self.buffer.insert(start, text);
            }
            self.dirty = true;
            self.schedule_parse();
            return;
        }

        self.undo
            .begin_transaction_ex(selection_before, selection_after, kind);
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

    fn try_coalesce(
        &mut self,
        start: usize,
        end: usize,
        text: &str,
        kind: TransactionKind,
        selection_after: SelectionSnapshot,
    ) -> bool {
        if self.undo.has_open_transaction() {
            return false;
        }
        let Some(last) = self.undo.last_mut() else {
            return false;
        };
        if last.kind != kind {
            return false;
        }
        match kind {
            TransactionKind::Typing if end == start && is_typing_burst(text) => {
                match last.ops.last_mut() {
                    Some(EditOperation::Insert {
                        byte_offset,
                        text: prev,
                    }) if *byte_offset + prev.len() == start && is_typing_burst(prev) => {
                        prev.push_str(text);
                        last.selection_after = selection_after;
                        true
                    }
                    _ => false,
                }
            }
            TransactionKind::DeleteBack if text.is_empty() && start < end => {
                match last.ops.last_mut() {
                    Some(EditOperation::Delete {
                        byte_offset,
                        text: prev,
                    }) if end == *byte_offset => {
                        let deleted = self.buffer.slice(start, end);
                        let mut combined = deleted;
                        combined.push_str(prev);
                        *byte_offset = start;
                        *prev = combined;
                        last.selection_after = selection_after;
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    pub fn undo(&mut self) -> bool {
        self.undo_tx().is_some()
    }

    pub fn redo(&mut self) -> bool {
        self.redo_tx().is_some()
    }

    /// Undo and return the transaction (ops already inverted; restore
    /// `selection_after`).
    pub fn undo_tx(&mut self) -> Option<Transaction> {
        let tx = self.undo.undo()?;
        self.apply_ops(&tx.ops);
        Some(tx)
    }

    /// Redo and return the original transaction (restore `selection_after`).
    pub fn redo_tx(&mut self) -> Option<Transaction> {
        let tx = self.undo.redo()?;
        self.apply_ops(&tx.ops);
        Some(tx)
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
        self.saved_content = self.buffer.content();
        self.mark_clean();
        Ok(())
    }

    pub fn save_as(&mut self, path: PathBuf) -> io::Result<()> {
        atomic_write(&path, &self.buffer.content())?;
        self.path = Some(path);
        self.saved_content = self.buffer.content();
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
        self.saved_content = content.to_string();
        self.undo = UndoStack::new();
        self.schedule_parse();
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
    fn new_does_not_parse_on_the_calling_thread() {
        let doc = Document::new("# Hello\n\n**bold**");
        assert!(
            doc.syntax_spans.is_empty(),
            "Document::new must not parse Markdown on the caller (got {} spans)",
            doc.syntax_spans.len()
        );
    }

    #[test]
    fn new_parses_markdown_on_background_worker() {
        let mut doc = Document::new("# Hello\n\n**bold**");
        assert!(doc.wait_for_parse(PARSE_TIMEOUT));
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
        assert!(doc.wait_for_parse(PARSE_TIMEOUT));
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

    #[test]
    fn revision_increments_on_document_edits() {
        let mut doc = Document::new("a");
        assert_eq!(doc.revision(), 0);
        doc.insert(1, "b");
        assert_eq!(doc.revision(), 1);
        doc.delete(0, 1);
        assert_eq!(doc.revision(), 2);
        doc.replace_range(0, 1, "xy");
        assert!(doc.revision() > 2);
        doc.undo();
        assert!(doc.revision() > 0);
        doc.replace_range(0, 0, "");
        let after_noop = doc.revision();
        doc.replace_range(0, 0, "");
        assert_eq!(doc.revision(), after_noop);
    }

    #[test]
    fn interleaved_edits_clear_redo_and_restore() {
        let mut doc = Document::new("");
        doc.insert(0, "a");
        doc.insert(1, "b");
        doc.insert(2, "c");
        assert_eq!(doc.buffer.content(), "abc");
        assert!(doc.undo());
        assert_eq!(doc.buffer.content(), "ab");
        assert!(doc.undo());
        assert_eq!(doc.buffer.content(), "a");
        assert!(doc.undo_stack().can_redo());
        doc.insert(1, "X");
        assert_eq!(doc.buffer.content(), "aX");
        assert!(!doc.undo_stack().can_redo());
        assert!(doc.undo());
        assert_eq!(doc.buffer.content(), "a");
        assert!(doc.redo());
        assert_eq!(doc.buffer.content(), "aX");
        assert!(doc.dirty);
    }

    #[test]
    fn save_as_round_trip_and_atomic_temp_is_gone() {
        let dir = TempDir::new("doc-save-as");
        let path = dir.join("note.md");
        let mut doc = Document::new("# Hello");
        doc.insert(7, " world");
        doc.save_as(path.clone()).unwrap();
        assert!(!doc.dirty);
        assert_eq!(doc.path.as_deref(), Some(path.as_path()));
        assert!(!path.with_extension("markrust-tmp").exists());
        let reloaded = Document::from_file(path).unwrap();
        assert_eq!(reloaded.buffer.content(), "# Hello world");
        assert!(!reloaded.dirty);
    }

    #[test]
    fn save_creates_parent_dirs_and_clears_tmp() {
        let dir = TempDir::new("doc-atomic");
        let path = dir.join("nested/out.md");
        let mut doc = Document::new("body");
        doc.set_path(Some(path.clone()));
        doc.save_and_mark_clean().unwrap();
        assert!(!doc.dirty);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "body");
        assert!(!path.with_extension("markrust-tmp").exists());
    }

    #[test]
    fn word_count_edge_cases() {
        let cases: &[(&str, usize)] = &[
            ("hello, world!", 2),
            ("foo\nbar", 2),
            ("foo\tbar", 2),
            ("你好 世界", 2),
            ("你好世界", 1),
            ("e\u{0301} acute", 2),
            ("one two  three\n\nfour", 4),
            ("a\u{00a0}b", 2),
        ];
        for &(text, expected) in cases {
            assert_eq!(Document::new(text).word_count(), expected, "text {text:?}");
        }
    }

    #[test]
    fn markdown_extension_from_file_is_wysiwyg() {
        let dir = TempDir::new("doc-md");
        let path = dir.join("readme.markdown");
        std::fs::write(&path, "# Title\n").unwrap();
        let mut doc = Document::from_file(path).unwrap();
        assert_eq!(doc.mode, DocumentProcessingMode::MarkdownWysiwyg);
        assert!(doc.wait_for_parse(PARSE_TIMEOUT));
        assert!(
            doc.syntax_spans
                .iter()
                .any(|span| span.kind == SyntaxKind::Heading),
            "spans: {:?}",
            doc.syntax_spans
        );
    }

    #[test]
    fn typing_coalesces_and_undo_restores_caret() {
        let mut doc = Document::new("ab");
        let before = SelectionSnapshot::collapsed(2);
        doc.replace_range_tx(
            2,
            2,
            "c",
            TransactionKind::Typing,
            before,
            SelectionSnapshot::collapsed(3),
        );
        doc.replace_range_tx(
            3,
            3,
            "d",
            TransactionKind::Typing,
            SelectionSnapshot::collapsed(3),
            SelectionSnapshot::collapsed(4),
        );
        assert_eq!(doc.buffer.content(), "abcd");
        assert_eq!(doc.undo_stack().undo_depth(), 1);
        let tx = doc.undo_tx().unwrap();
        assert_eq!(doc.buffer.content(), "ab");
        assert_eq!(tx.selection_after, before);
    }

    #[test]
    fn explicit_paste_group_is_separate_from_typing_before_and_after() {
        fn type_at_end(doc: &mut Document, text: &str) {
            let at = doc.buffer.len_bytes();
            doc.replace_range_tx(
                at,
                at,
                text,
                TransactionKind::Typing,
                SelectionSnapshot::collapsed(at),
                SelectionSnapshot::collapsed(at + text.len()),
            );
        }
        let mut doc = Document::new("");
        type_at_end(&mut doc, "a");
        type_at_end(&mut doc, "b");
        let paste = doc.begin_undo_group(SelectionSnapshot::collapsed(2));
        type_at_end(&mut doc, "Café");
        doc.finish_undo_group(paste, SelectionSnapshot::collapsed("abCafé".len()));
        type_at_end(&mut doc, "c");
        type_at_end(&mut doc, "d");
        assert_eq!(doc.buffer.content(), "abCafécd");
        assert_eq!(doc.undo_stack().undo_depth(), 3);
        assert!(doc.undo());
        assert_eq!(doc.buffer.content(), "abCafé");
        assert!(doc.undo());
        assert_eq!(doc.buffer.content(), "ab");
        assert!(doc.undo());
        assert_eq!(doc.buffer.content(), "");
        assert!(doc.redo());
        assert!(doc.redo());
        assert!(doc.redo());
        assert_eq!(doc.buffer.content(), "abCafécd");
    }

    #[test]
    fn nested_undo_groups_preserve_outer_selection_and_all_splices() {
        let mut doc = Document::new("old");
        let before = SelectionSnapshot {
            start: 0,
            end: 3,
            reversed: true,
        };
        let paste = doc.begin_undo_group(before);
        let replacement = doc.begin_undo_group(before);
        doc.delete(0, 3);
        doc.insert(0, "new");
        doc.finish_undo_group(replacement, SelectionSnapshot::collapsed(3));
        doc.insert(3, "!");
        doc.finish_undo_group(paste, SelectionSnapshot::collapsed(4));
        assert_eq!(doc.undo_stack().undo_depth(), 1);
        let undo = doc.undo_tx().unwrap();
        assert_eq!(doc.buffer.content(), "old");
        assert_eq!(undo.selection_after, before);
        assert!(doc.redo());
        assert_eq!(doc.buffer.content(), "new!");
    }

    #[test]
    fn undo_group_without_document_edits_preserves_redo() {
        let mut doc = Document::new("old");
        doc.insert(3, "!");
        assert!(doc.undo());
        let empty = doc.begin_undo_group(SelectionSnapshot::collapsed(3));
        doc.finish_undo_group(empty, SelectionSnapshot::collapsed(3));
        assert_eq!(doc.undo_stack().undo_depth(), 0);
        assert!(doc.redo());
        assert_eq!(doc.buffer.content(), "old!");
    }

    #[test]
    fn load_and_parse_large_markdown_does_not_hang() {
        let dir = TempDir::new("large-md");
        let path = dir.join("large.md");
        let mut body = String::with_capacity(256 * 1024);
        for i in 0..2_000 {
            body.push_str("# Heading ");
            body.push_str(&i.to_string());
            body.push_str("\n\nParagraph with **bold**, *italic*, and `code`.\n\n- item\n\n");
        }
        std::fs::write(&path, &body).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut doc = Document::from_file(path).expect("from_file");
            let construct = started.elapsed();
            let parsed = doc.wait_for_parse(Duration::from_secs(5));
            let heading_count = doc
                .syntax_spans
                .iter()
                .filter(|span| span.kind == SyntaxKind::Heading)
                .count();
            let _ = tx.send((construct, parsed, heading_count, doc.buffer.len_bytes()));
        });

        let (construct, parsed, heading_count, len) = rx
            .recv_timeout(Duration::from_secs(8))
            .expect("Document::from_file + parse hung (timed out)");
        assert!(
            construct < Duration::from_millis(500),
            "from_file blocked the caller for {construct:?}; parse must stay off this thread"
        );
        assert!(parsed, "background parse did not finish");
        assert!(len > 100_000, "fixture too small: {len} bytes");
        assert!(
            heading_count >= 1_000,
            "expected many heading spans, got {heading_count}"
        );
    }

    #[test]
    fn peel_typing_range_exact_reverts_the_tx() {
        let mut doc = Document::new("");
        doc.replace_range_tx(
            0,
            0,
            "#",
            TransactionKind::Typing,
            SelectionSnapshot::collapsed(0),
            SelectionSnapshot::collapsed(1),
        );
        let before = doc.peel_typing_range(0..1).unwrap();
        assert_eq!(doc.buffer.content(), "");
        assert_eq!(doc.undo_stack().undo_depth(), 0);
        assert_eq!(before, SelectionSnapshot::collapsed(0));
    }

    #[test]
    fn peel_typing_range_takes_a_prefix_of_coalesced_typing() {
        let mut doc = Document::new("");
        doc.replace_range_tx(
            0,
            0,
            "#z",
            TransactionKind::Typing,
            SelectionSnapshot::collapsed(0),
            SelectionSnapshot::collapsed(2),
        );
        doc.peel_typing_range(0..1).unwrap();
        assert_eq!(doc.buffer.content(), "z");
        assert_eq!(doc.undo_stack().undo_depth(), 1);
        assert!(doc.undo());
        assert_eq!(doc.buffer.content(), "");
    }

    #[test]
    fn apply_external_edit_maps_caret_and_clears_undo() {
        let mut doc = Document::new("abcdef");
        doc.insert(6, "x");
        assert!(doc.undo_stack().can_undo());
        let mapped = doc.apply_external_edit("abcXYZdef", &[2, 5]);
        assert_eq!(mapped, vec![2, 8]);
        assert_eq!(doc.buffer.content(), "abcXYZdef");
        assert!(!doc.dirty);
        assert!(!doc.undo_stack().can_undo());
        assert_eq!(doc.saved_content(), "abcXYZdef");
    }

    #[test]
    fn apply_merged_edit_keeps_dirty_and_maps_caret() {
        let mut doc = Document::new("aaa\nbbb\nccc\n");
        doc.insert(4, "X");
        assert!(doc.dirty);
        let mapped = doc.apply_merged_edit("aaa\nXbbb\nCCC\n", "aaa\nbbb\nCCC\n", &[5]);
        assert_eq!(doc.buffer.content(), "aaa\nXbbb\nCCC\n");
        assert!(doc.dirty);
        assert_eq!(doc.saved_content(), "aaa\nbbb\nCCC\n");
        assert_eq!(mapped.len(), 1);
        assert!(!doc.undo_stack().can_undo());
    }
}
