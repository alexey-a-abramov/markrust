// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// A single reversible buffer edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOperation {
    Insert { byte_offset: usize, text: String },
    Delete { byte_offset: usize, text: String },
}

impl EditOperation {
    fn inverse(&self) -> EditOperation {
        match self {
            EditOperation::Insert { byte_offset, text } => EditOperation::Delete {
                byte_offset: *byte_offset,
                text: text.clone(),
            },
            EditOperation::Delete { byte_offset, text } => EditOperation::Insert {
                byte_offset: *byte_offset,
                text: text.clone(),
            },
        }
    }
}

/// Undo/redo stack storing grouped edit transactions.
#[derive(Debug, Default)]
pub struct UndoStack {
    undo: Vec<Vec<EditOperation>>,
    redo: Vec<Vec<EditOperation>>,
    open: Option<Vec<EditOperation>>,
}

impl UndoStack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin_transaction(&mut self) {
        self.open = Some(Vec::new());
    }

    pub fn record(&mut self, edit: EditOperation) {
        match &mut self.open {
            Some(group) => group.push(edit),
            None => {
                self.open = Some(vec![edit]);
            }
        }
    }

    pub fn commit_transaction(&mut self) {
        if let Some(group) = self.open.take() {
            if !group.is_empty() {
                self.redo.clear();
                self.undo.push(group);
            }
        }
    }

    pub fn push_single(&mut self, edit: EditOperation) {
        self.begin_transaction();
        self.record(edit);
        self.commit_transaction();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self) -> Option<Vec<EditOperation>> {
        let group = self.undo.pop()?;
        let inverse: Vec<_> = group.iter().rev().map(EditOperation::inverse).collect();
        self.redo.push(group);
        Some(inverse)
    }

    pub fn redo(&mut self) -> Option<Vec<EditOperation>> {
        let group = self.redo.pop()?;
        let forward = group.to_vec();
        self.undo.push(group);
        Some(forward)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_redo_round_trip() {
        let mut stack = UndoStack::new();
        stack.push_single(EditOperation::Insert {
            byte_offset: 0,
            text: "hi".into(),
        });
        let undo_ops = stack.undo().unwrap();
        assert_eq!(undo_ops.len(), 1);
        assert!(matches!(undo_ops[0], EditOperation::Delete { .. }));
        let redo_ops = stack.redo().unwrap();
        assert!(matches!(redo_ops[0], EditOperation::Insert { .. }));
    }

    #[test]
    fn transaction_groups_edits() {
        let mut stack = UndoStack::new();
        stack.begin_transaction();
        stack.record(EditOperation::Insert {
            byte_offset: 0,
            text: "a".into(),
        });
        stack.record(EditOperation::Insert {
            byte_offset: 1,
            text: "b".into(),
        });
        stack.commit_transaction();
        let undo_ops = stack.undo().unwrap();
        assert_eq!(undo_ops.len(), 2);
    }
}
