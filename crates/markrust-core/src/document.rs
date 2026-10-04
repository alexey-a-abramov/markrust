// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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

    /// Keep the first typed character and the splices that open its draft
    /// paragraph together, while allowing the rest of this typing burst to
    /// join it. A command or an action without edits remains a history barrier.
    pub(crate) fn finish_typing_undo_group(
        &mut self,
        checkpoint: UndoGroupCheckpoint,
        selection_after: SelectionSnapshot,
    ) {
        let continues_typing = self.undo.undo_depth() > checkpoint.depth
            && self
                .undo
                .last()
                .is_some_and(|transaction| transaction.kind == TransactionKind::Typing);
        self.finish_undo_group(checkpoint, selection_after);
        if continues_typing {
            self.undo
                .last_mut()
                .expect("the typing group contains at least one edit")
                .kind = TransactionKind::Typing;
            self.coalescing_blocked = false;
        }
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
    /// merged twice. The merge is one undoable command, so undo first restores
    /// the local pre-merge buffer and then retains its prior local history.
    pub fn apply_merged_edit(&mut self, merged: &str, disk: &str, offsets: &[usize]) -> Vec<usize> {
        self.apply_merged_edit_with_selection(
            merged,
            disk,
            offsets,
            SelectionSnapshot::collapsed(offsets.first().copied().unwrap_or(0)),
        )
    }

    /// Preserve the active pane's full selection in the merge undo boundary.
    pub fn apply_merged_edit_with_selection(
        &mut self,
        merged: &str,
        disk: &str,
        offsets: &[usize],
        before: SelectionSnapshot,
    ) -> Vec<usize> {
        fn snap_offsets(content: &str, offsets: &mut [usize]) {
            use unicode_segmentation::UnicodeSegmentation;

            let mut pending = offsets
                .iter_mut()
                .enumerate()
                .filter_map(|(ix, offset)| {
                    if *offset >= content.len() {
                        *offset = content.len();
                        None
                    } else {
                        Some((*offset, ix))
                    }
                })
                .collect::<Vec<_>>();
            if pending.is_empty() {
                return;
            }
            pending.sort_unstable();
            let mut next = 0;
            let mut previous_boundary = 0;
            for boundary in content
                .grapheme_indices(true)
                .map(|(boundary, _)| boundary)
                .chain(std::iter::once(content.len()))
            {
                while next < pending.len() && pending[next].0 < boundary {
                    offsets[pending[next].1] = previous_boundary;
                    next += 1;
                }
                if next == pending.len() {
                    break;
                }
                previous_boundary = boundary;
            }
        }

        fn repair_selection(content: &str, snapshot: SelectionSnapshot) -> SelectionSnapshot {
            let mut endpoints = [snapshot.start, snapshot.end];
            snap_offsets(content, &mut endpoints);
            let mut reversed = snapshot.reversed;
            if endpoints[0] > endpoints[1] {
                endpoints.swap(0, 1);
                reversed = !reversed;
            }
            SelectionSnapshot {
                start: endpoints[0],
                end: endpoints[1],
                reversed: reversed && endpoints[0] != endpoints[1],
            }
        }

        let old = self.buffer.content();
        let mut valid_offsets = offsets.to_vec();
        snap_offsets(&old, &mut valid_offsets);
        let mut mapped = valid_offsets
            .iter()
            .map(|offset| map_offset_across_change(&old, merged, *offset))
            .collect::<Vec<_>>();
        // Byte-relative mapping inside a replacement can land within UTF-8,
        // a combining sequence, or a ZWJ emoji. Persist only legal caret edges
        // so Redo cannot reintroduce an endpoint that the live view repaired.
        snap_offsets(merged, &mut mapped);
        if old != merged {
            let before = repair_selection(&old, before);
            let after = repair_selection(
                merged,
                SelectionSnapshot {
                    start: map_offset_across_change(&old, merged, before.start),
                    end: map_offset_across_change(&old, merged, before.end),
                    reversed: before.reversed,
                },
            );
            self.replace_range_tx(
                0,
                old.len(),
                merged,
                TransactionKind::Command,
                before,
                after,
            );
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

    /// Save only when the target still has the exact bytes this document last
    /// reconciled. `None` means the target must not exist. The guard is checked
    /// before staging and immediately before publish; a mismatch returns
    /// [`io::ErrorKind::WouldBlock`] without changing this document's path,
    /// dirty flag, or saved base.
    ///
    /// This is a content precondition, not a portable cross-process compare-
    /// and-swap. A non-cooperating writer can still race after the final check
    /// and before `rename`; callers must retain a conflict/recovery path rather
    /// than treating a successful write as a merge protocol.
    pub fn save_checked_and_mark_clean(&mut self, expected_disk: Option<&str>) -> io::Result<()> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "document has no path"))?;
        let content = self.buffer.content();
        atomic_write_checked(&path, &content, expected_disk)?;
        self.saved_content = content;
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

    /// Save to a new path only when its current bytes match `expected_disk`.
    /// `None` requires that the destination does not already exist. On a
    /// rejected or failed write this document keeps its original path, dirty
    /// state, and saved base.
    pub fn save_as_checked(
        &mut self,
        path: PathBuf,
        expected_disk: Option<&str>,
    ) -> io::Result<()> {
        let content = self.buffer.content();
        atomic_write_checked(&path, &content, expected_disk)?;
        self.path = Some(path);
        self.saved_content = content;
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

#[derive(Debug, Clone, Copy)]
enum DiskExpectation<'a> {
    Unchecked,
    Exact(Option<&'a str>),
}

fn atomic_write(path: &Path, content: &str) -> io::Result<()> {
    atomic_write_with_expectation(path, content, DiskExpectation::Unchecked, || Ok(()))
}

fn atomic_write_checked(path: &Path, content: &str, expected_disk: Option<&str>) -> io::Result<()> {
    atomic_write_with_expectation(path, content, DiskExpectation::Exact(expected_disk), || {
        Ok(())
    })
}

#[cfg(test)]
fn atomic_write_checked_with_before_publish<F>(
    path: &Path,
    content: &str,
    expected_disk: Option<&str>,
    before_publish: F,
) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()>,
{
    atomic_write_with_expectation(
        path,
        content,
        DiskExpectation::Exact(expected_disk),
        before_publish,
    )
}

/// Stage a complete replacement and publish it atomically. Checked writes use
/// a content precondition before staging and immediately before publishing.
/// The latter cannot be an unconditional cross-process CAS on every supported
/// platform; it narrows the race and rejects stale callers before replacement.
fn atomic_write_with_expectation<F>(
    path: &Path,
    content: &str,
    expectation: DiskExpectation<'_>,
    before_publish: F,
) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()>,
{
    ensure_disk_expectation(path, expectation)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let permissions = match fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    fs::create_dir_all(parent)?;
    let (mut file, mut temporary) = create_save_temp(path, parent)?;
    // Make the replacement durable before publishing it. The original file
    // remains untouched if writing, setting permissions, or syncing fails.
    let write_result = (|| {
        file.write_all(content.as_bytes())?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()
    })();
    drop(file);
    write_result?;
    // Test hooks and future instrumentation belong between staging and this
    // second guard, the narrowest useful place to detect a stale replacement.
    before_publish()?;
    ensure_disk_expectation(path, expectation)?;
    fs::rename(&temporary.path, path)?;
    temporary.remove_on_drop = false;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn ensure_disk_expectation(path: &Path, expectation: DiskExpectation<'_>) -> io::Result<()> {
    let DiskExpectation::Exact(expected) = expectation else {
        return Ok(());
    };
    match expected {
        Some(expected) => match file_matches_expected_bytes(path, expected.as_bytes()) {
            Ok(true) => Ok(()),
            Ok(false) => Err(stale_disk_error(path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Err(stale_disk_error(path)),
            Err(error) => Err(error),
        },
        None => match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(stale_disk_error(path)),
            Err(error) => Err(error),
        },
    }
}

/// Compare an untrusted current file with the caller's already-owned saved
/// base without allocating a second copy of it. Metadata rejects obvious large
/// replacements before opening them; the chunked comparison catches changes
/// made between metadata and the read. A non-cooperating writer can still
/// replace the file after this final comparison and before rename, which is
/// the unavoidable small race documented on `atomic_write_with_expectation`.
fn file_matches_expected_bytes(path: &Path, expected: &[u8]) -> io::Result<bool> {
    let expected_len = u64::try_from(expected.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "document content is too large to compare",
        )
    })?;
    if fs::metadata(path)?.len() != expected_len {
        return Ok(false);
    }

    let mut file = fs::File::open(path)?;
    let mut offset = 0;
    let mut buffer = [0_u8; 64 * 1024];
    while offset < expected.len() {
        let read_len = buffer.len().min(expected.len() - offset);
        let count = file.read(&mut buffer[..read_len])?;
        if count == 0 || buffer[..count] != expected[offset..offset + count] {
            return Ok(false);
        }
        offset += count;
    }

    // Metadata can become stale while we stream. A final byte detects growth;
    // an early EOF above detects truncation without allocating arbitrary data.
    Ok(file.read(&mut buffer[..1])? == 0)
}

fn stale_disk_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        format!("document changed on disk before save: {}", path.display()),
    )
}

static SAVE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct SaveTemp {
    path: PathBuf,
    remove_on_drop: bool,
}

impl Drop for SaveTemp {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn create_save_temp(path: &Path, parent: &Path) -> io::Result<(fs::File, SaveTemp)> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "document path has no file name",
        )
    })?;
    for _ in 0..64 {
        let sequence = SAVE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = std::ffi::OsString::from(".");
        temporary_name.push(name);
        temporary_name.push(format!(".markrust-{}-{sequence}.tmp", std::process::id()));
        let temporary_path = parent.join(temporary_name);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary_path) {
            Ok(file) => {
                return Ok((
                    file,
                    SaveTemp {
                        path: temporary_path,
                        remove_on_drop: true,
                    },
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique document save temporary file",
    ))
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
    fn checked_save_rejects_stale_disk_and_retains_document_state() {
        let dir = TempDir::new("doc-checked-stale");
        let path = dir.join("note.md");
        fs::write(&path, "base").unwrap();
        let mut doc = Document::from_file(path.clone()).unwrap();
        doc.insert(4, " ours");

        fs::write(&path, "theirs").unwrap();
        let error = doc.save_checked_and_mark_clean(Some("base")).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(fs::read_to_string(&path).unwrap(), "theirs");
        assert_eq!(doc.path.as_deref(), Some(path.as_path()));
        assert_eq!(doc.buffer.content(), "base ours");
        assert_eq!(doc.saved_content(), "base");
        assert!(doc.dirty);
        assert_eq!(fs::read_dir(dir.join("")).unwrap().count(), 1);
    }

    #[test]
    fn checked_save_rejects_an_oversized_current_file_without_replacing_it() {
        let dir = TempDir::new("doc-checked-oversized");
        let path = dir.join("note.md");
        fs::write(&path, "base").unwrap();
        let mut doc = Document::from_file(path.clone()).unwrap();
        doc.insert(4, " ours");

        // The precondition comparator must reject on metadata length before it
        // can allocate an unbounded replacement merely to compare bytes.
        let external = vec![b'x'; 8 * 1024 * 1024];
        fs::write(&path, &external).unwrap();
        let error = doc.save_checked_and_mark_clean(Some("base")).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(fs::metadata(&path).unwrap().len(), external.len() as u64);
        assert_eq!(doc.buffer.content(), "base ours");
        assert_eq!(doc.saved_content(), "base");
        assert!(doc.dirty);
    }

    #[test]
    fn checked_save_rejects_same_length_but_different_disk_bytes() {
        let dir = TempDir::new("doc-checked-same-length");
        let path = dir.join("note.md");
        fs::write(&path, "base").unwrap();
        let mut doc = Document::from_file(path.clone()).unwrap();
        doc.insert(4, " ours");

        // This reaches the chunked byte comparison rather than the metadata
        // length fast path.
        fs::write(&path, "disk").unwrap();
        let error = doc.save_checked_and_mark_clean(Some("base")).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(fs::read_to_string(&path).unwrap(), "disk");
        assert_eq!(doc.buffer.content(), "base ours");
        assert_eq!(doc.saved_content(), "base");
        assert!(doc.dirty);
    }

    #[test]
    fn checked_save_rechecks_before_publish_after_delete_and_recreate() {
        let dir = TempDir::new("doc-checked-recreate");
        let path = dir.join("note.md");
        fs::write(&path, "base").unwrap();

        let error = atomic_write_checked_with_before_publish(&path, "ours", Some("base"), || {
            fs::remove_file(&path)?;
            fs::write(&path, "theirs")
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(fs::read_to_string(&path).unwrap(), "theirs");
        assert_eq!(fs::read_dir(dir.join("")).unwrap().count(), 1);
    }

    #[test]
    fn checked_save_as_requires_an_absent_destination_before_publish() {
        let dir = TempDir::new("doc-checked-save-as");
        let path = dir.join("note.md");

        let error = atomic_write_checked_with_before_publish(&path, "ours", None, || {
            fs::write(&path, "theirs")
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(fs::read_to_string(&path).unwrap(), "theirs");
        assert_eq!(fs::read_dir(dir.join("")).unwrap().count(), 1);

        let mut doc = Document::new("draft");
        doc.insert(5, " ours");
        let error = doc.save_as_checked(path.clone(), None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(doc.path, None);
        assert_eq!(doc.buffer.content(), "draft ours");
        assert_eq!(doc.saved_content(), "draft");
        assert!(doc.dirty);
    }

    #[test]
    fn atomic_save_does_not_touch_unrelated_temporary_files() {
        let dir = TempDir::new("doc-foreign-temp");
        let path = dir.join("note.md");
        let legacy_temp = path.with_extension("markrust-tmp");
        fs::write(&path, "original").unwrap();
        fs::write(&legacy_temp, "unrelated data").unwrap();

        atomic_write(&path, "replacement").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "replacement");
        assert_eq!(fs::read_to_string(&legacy_temp).unwrap(), "unrelated data");
        assert_eq!(fs::read_dir(dir.join("")).unwrap().count(), 2);
    }

    #[test]
    fn failed_atomic_save_cleans_its_temp_and_keeps_document_dirty() {
        let dir = TempDir::new("doc-failed-save");
        let target_directory = dir.join("note.md");
        fs::create_dir(&target_directory).unwrap();
        let original = target_directory.join("original.md");
        fs::write(&original, "original data").unwrap();
        let mut doc = Document::new("new data");
        doc.insert(8, "!");
        doc.set_path(Some(target_directory.clone()));

        assert!(doc.save_and_mark_clean().is_err());

        assert!(doc.dirty);
        assert_eq!(doc.saved_content(), "new data");
        assert_eq!(fs::read_to_string(original).unwrap(), "original data");
        assert_eq!(fs::read_dir(dir.join("")).unwrap().count(), 1);
        assert_eq!(doc.path.as_deref(), Some(target_directory.as_path()));
    }

    #[test]
    fn concurrent_atomic_saves_publish_only_complete_replacements() {
        let dir = TempDir::new("doc-concurrent-save");
        let path = dir.join("note.md");
        let contents = std::sync::Arc::new([
            "original".repeat(2048),
            "alpha".repeat(2048),
            "beta".repeat(2048),
        ]);
        atomic_write(&path, &contents[0]).unwrap();
        let writers: Vec<_> = (1..=2)
            .map(|index| {
                let path = path.clone();
                let contents = contents.clone();
                std::thread::spawn(move || {
                    for _ in 0..4 {
                        atomic_write(&path, &contents[index]).unwrap();
                    }
                })
            })
            .collect();
        while writers.iter().any(|writer| !writer.is_finished()) {
            let published = fs::read_to_string(&path).unwrap();
            assert!(contents.iter().any(|content| content == &published));
        }
        for writer in writers {
            writer.join().unwrap();
        }
        let published = fs::read_to_string(&path).unwrap();
        assert!(contents.iter().any(|content| content == &published));
        assert_eq!(fs::read_dir(dir.join("")).unwrap().count(), 1);
    }

    #[test]
    fn atomic_save_temps_are_unique_and_removed_when_abandoned() {
        let dir = TempDir::new("doc-unique-temp");
        let parent = dir.join("");
        let path = dir.join("note.md");
        let (first_file, first_temp) = create_save_temp(&path, &parent).unwrap();
        let (second_file, second_temp) = create_save_temp(&path, &parent).unwrap();
        assert_ne!(first_temp.path, second_temp.path);
        assert!(first_temp.path.exists());
        assert!(second_temp.path.exists());
        drop(first_file);
        drop(second_file);
        drop(first_temp);
        drop(second_temp);
        assert_eq!(fs::read_dir(parent).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_save_preserves_existing_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("doc-save-permissions");
        let path = dir.join("note.md");
        fs::write(&path, "private original").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        atomic_write(&path, "private replacement").unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "private replacement");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_save_creates_new_files_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("doc-save-new-permissions");
        let path = dir.join("new.md");
        atomic_write(&path, "private new document").unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
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
    fn paragraph_draft_typing_group_keeps_separator_and_character_burst_together() {
        let source = "- First\n\n- Following";
        let before = SelectionSnapshot::collapsed(8);
        let mut doc = Document::new(source);
        let group = doc.begin_undo_group(before);
        doc.replace_range_tx(
            9,
            9,
            "\n\n",
            TransactionKind::Command,
            before,
            SelectionSnapshot::collapsed(9),
        );
        doc.replace_range_tx(
            9,
            9,
            "N",
            TransactionKind::Typing,
            SelectionSnapshot::collapsed(9),
            SelectionSnapshot::collapsed(10),
        );
        doc.finish_typing_undo_group(group, SelectionSnapshot::collapsed(10));
        for (at, ch) in [(10, "e"), (11, "w")] {
            doc.replace_range_tx(
                at,
                at,
                ch,
                TransactionKind::Typing,
                SelectionSnapshot::collapsed(at),
                SelectionSnapshot::collapsed(at + 1),
            );
        }
        assert_eq!(doc.buffer.content(), "- First\n\nNew\n\n- Following");
        assert_eq!(doc.undo_stack().undo_depth(), 1);
        let undo = doc.undo_tx().unwrap();
        assert_eq!(doc.buffer.content(), source);
        assert_eq!(undo.selection_after, before);
        let redo = doc.redo_tx().unwrap();
        assert_eq!(doc.buffer.content(), "- First\n\nNew\n\n- Following");
        assert_eq!(redo.selection_after, SelectionSnapshot::collapsed(12));
    }

    #[test]
    fn empty_typing_group_does_not_change_the_previous_command_kind() {
        let mut doc = Document::new("old");
        doc.replace_range_tx(
            3,
            3,
            "!",
            TransactionKind::Command,
            SelectionSnapshot::collapsed(3),
            SelectionSnapshot::collapsed(4),
        );
        let empty = doc.begin_undo_group(SelectionSnapshot::collapsed(4));
        doc.finish_typing_undo_group(empty, SelectionSnapshot::collapsed(4));
        assert_eq!(
            doc.undo_stack().last().unwrap().kind,
            TransactionKind::Command
        );
        doc.replace_range_tx(
            4,
            4,
            "x",
            TransactionKind::Typing,
            SelectionSnapshot::collapsed(4),
            SelectionSnapshot::collapsed(5),
        );
        assert_eq!(doc.undo_stack().undo_depth(), 2);
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
        assert!(doc.undo_stack().can_undo());
    }

    #[test]
    fn merge_undo_redo_preserves_active_reversed_unicode_selection_not_inactive_source_caret() {
        let base = "# Notes\n\nПривет e\u{301} 👩‍👩‍👧‍👦 tail\n";
        let mut doc = Document::new(base);
        doc.insert(base.len(), "local\n");
        let ours = doc.buffer.content();
        let selected = "e\u{301} 👩‍👩‍👧‍👦";
        let start = ours.find(selected).unwrap();
        let before = SelectionSnapshot {
            start,
            end: start + selected.len(),
            reversed: true,
        };
        let prefix = "external heading\n";
        let merged = format!("{prefix}{ours}");
        let disk = format!("{prefix}{base}");
        let after = SelectionSnapshot {
            start: before.start + prefix.len(),
            end: before.end + prefix.len(),
            reversed: true,
        };

        // Source remains at zero while the rich pane owns this reversed range.
        // The explicit snapshot must win over offsets.first() for undo state.
        let mapped = doc.apply_merged_edit_with_selection(
            &merged,
            &disk,
            &[0, before.start, before.end],
            before,
        );
        assert_eq!(&mapped[1..], &[after.start, after.end]);
        assert_eq!(doc.buffer.content(), merged);
        assert_eq!(doc.saved_content(), disk);
        assert!(doc.dirty);
        assert_eq!(&merged[after.start..after.end], selected);

        let undo = doc.undo_tx().expect("merge must be independently undoable");
        assert_eq!(undo.selection_after, before);
        assert_eq!(doc.buffer.content(), ours);
        assert_eq!(&ours[before.start..before.end], selected);

        let redo = doc
            .redo_tx()
            .expect("merge must restore its mapped rich range");
        assert_eq!(redo.selection_after, after);
        assert_eq!(doc.buffer.content(), merged);
        assert_eq!(doc.saved_content(), disk);

        assert!(doc.undo());
        assert!(doc.undo(), "pre-merge local history must remain available");
        assert_eq!(doc.buffer.content(), base);
    }

    #[test]
    fn merge_redo_selection_repairs_ascii_to_greek_combining_and_zwj_replacements() {
        use unicode_segmentation::UnicodeSegmentation;

        for merged in ["αβγ", "q\u{301}yz", "👩‍👩‍👧‍👦x"] {
            let mut doc = Document::new("abcdef");
            let before = SelectionSnapshot {
                start: 2,
                end: 3,
                reversed: true,
            };
            let mapped =
                doc.apply_merged_edit_with_selection(merged, merged, &[2, 3, usize::MAX], before);
            let boundaries = merged
                .grapheme_indices(true)
                .map(|(boundary, _)| boundary)
                .chain(std::iter::once(merged.len()))
                .collect::<Vec<_>>();
            assert!(mapped.iter().all(|offset| boundaries.contains(offset)));
            assert_eq!(mapped[2], merged.len());

            let undo = doc.undo_tx().unwrap();
            assert_eq!(undo.selection_after, before);
            assert_eq!(doc.buffer.content(), "abcdef");
            let redo = doc.redo_tx().unwrap();
            assert_eq!(doc.buffer.content(), merged);
            let after = redo.selection_after;
            assert_eq!((after.start, after.end), (mapped[0], mapped[1]));
            assert!(boundaries.contains(&after.start));
            assert!(boundaries.contains(&after.end));
            assert!(merged.is_char_boundary(after.start));
            assert!(merged.is_char_boundary(after.end));
            assert_eq!(
                after.reversed,
                after.start != after.end,
                "preserve reversed direction only while the repaired range is nonempty"
            );
            assert!(merged.get(after.range()).is_some());
        }
    }

    #[test]
    fn merge_undo_repairs_stale_pre_merge_grapheme_edges_and_preserves_direction() {
        let before_text = "e\u{301} 👩‍👩‍👧‍👦 end";
        let mut doc = Document::new(before_text);
        let emoji_start = before_text.find('👩').unwrap();
        let before = SelectionSnapshot {
            start: emoji_start + 4,
            end: 1,
            reversed: false,
        };
        let merged = format!("prefix {before_text}");
        doc.apply_merged_edit_with_selection(&merged, &merged, &[1, emoji_start + 4], before);
        let undo = doc.undo_tx().unwrap();
        assert_eq!(
            undo.selection_after,
            SelectionSnapshot {
                start: 0,
                end: emoji_start,
                reversed: true,
            }
        );
        assert_eq!(doc.buffer.content(), before_text);
    }

    #[test]
    fn undoing_a_merge_restores_our_buffer_then_prior_local_history() {
        let mut doc = Document::new("base\n");
        doc.insert(4, " local");
        let ours_before_merge = doc.buffer.content();

        doc.apply_merged_edit(
            "base local\nremote\n",
            "base\nremote\n",
            &[ours_before_merge.len()],
        );
        assert_eq!(doc.buffer.content(), "base local\nremote\n");
        assert_eq!(doc.saved_content(), "base\nremote\n");

        assert!(doc.undo(), "merge should be the newest undo command");
        assert_eq!(doc.buffer.content(), ours_before_merge);
        assert!(
            doc.undo(),
            "the local edit before the merge must remain undoable"
        );
        assert_eq!(doc.buffer.content(), "base\n");
    }
}
