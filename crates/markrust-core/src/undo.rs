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
    /// Return the operation that reverses `self`.
    pub fn inverse(&self) -> EditOperation {
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

    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_depth(&self) -> usize {
        self.redo.len()
    }

    pub fn has_open_transaction(&self) -> bool {
        self.open.is_some()
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

    fn insert(offset: usize, text: &str) -> EditOperation {
        EditOperation::Insert {
            byte_offset: offset,
            text: text.into(),
        }
    }

    #[test]
    fn inverse_swaps_insert_and_delete() {
        let op = insert(3, "ab");
        assert_eq!(
            op.inverse(),
            EditOperation::Delete {
                byte_offset: 3,
                text: "ab".into(),
            }
        );
        assert_eq!(op.inverse().inverse(), op);
    }

    #[test]
    fn empty_stack_cannot_undo_or_redo() {
        let mut stack = UndoStack::new();
        assert!(!stack.can_undo());
        assert!(!stack.can_redo());
        assert_eq!(stack.undo_depth(), 0);
        assert_eq!(stack.redo_depth(), 0);
        assert!(stack.undo().is_none());
        assert!(stack.redo().is_none());
    }

    #[test]
    fn multi_step_undo_redo() {
        let mut stack = UndoStack::new();
        stack.push_single(insert(0, "a"));
        stack.push_single(insert(1, "b"));
        stack.push_single(insert(2, "c"));
        assert_eq!(stack.undo_depth(), 3);

        let first = stack.undo().unwrap();
        assert_eq!(
            first[0],
            EditOperation::Delete {
                byte_offset: 2,
                text: "c".into(),
            }
        );
        let second = stack.undo().unwrap();
        assert_eq!(second[0], insert(1, "b").inverse());
        assert_eq!(stack.undo_depth(), 1);
        assert_eq!(stack.redo_depth(), 2);

        let redo = stack.redo().unwrap();
        assert_eq!(redo[0], insert(1, "b"));
        assert_eq!(stack.undo_depth(), 2);
        assert_eq!(stack.redo_depth(), 1);
    }

    #[test]
    fn new_edit_clears_redo_stack() {
        let mut stack = UndoStack::new();
        stack.push_single(insert(0, "a"));
        stack.push_single(insert(1, "b"));
        stack.undo();
        assert!(stack.can_redo());
        stack.push_single(insert(1, "z"));
        assert!(!stack.can_redo());
        assert_eq!(stack.redo_depth(), 0);
        assert_eq!(stack.undo_depth(), 2);
    }

    #[test]
    fn empty_transaction_is_not_recorded() {
        let mut stack = UndoStack::new();
        stack.push_single(insert(0, "keep"));
        stack.undo();
        assert!(stack.can_redo());

        stack.begin_transaction();
        assert!(stack.has_open_transaction());
        stack.commit_transaction();
        assert!(!stack.has_open_transaction());
        assert!(!stack.can_undo());
        assert!(stack.can_redo());
    }

    #[test]
    fn grouped_undo_returns_reverse_inverses() {
        let mut stack = UndoStack::new();
        stack.begin_transaction();
        stack.record(insert(0, "a"));
        stack.record(insert(1, "b"));
        stack.commit_transaction();
        assert_eq!(stack.undo_depth(), 1);

        let undo_ops = stack.undo().unwrap();
        assert_eq!(
            undo_ops,
            vec![
                EditOperation::Delete {
                    byte_offset: 1,
                    text: "b".into(),
                },
                EditOperation::Delete {
                    byte_offset: 0,
                    text: "a".into(),
                },
            ]
        );
        assert!(!stack.can_undo());
        assert!(stack.can_redo());
    }
}
