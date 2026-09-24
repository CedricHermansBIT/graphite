//! Bounded undo history stores only the component affected by a drag.

use std::collections::VecDeque;

const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_EDITS: usize = 20;

pub struct PendingEdit {
    pub indices: Vec<usize>,
    pub before: Vec<[f32; 2]>,
}

struct Edit {
    indices: Vec<usize>,
    before: Vec<[f32; 2]>,
    after: Vec<[f32; 2]>,
}

impl Edit {
    fn bytes(&self) -> usize {
        self.indices.len() * (std::mem::size_of::<usize>() + 16)
    }
}

#[derive(Default)]
pub struct History {
    undo: VecDeque<Edit>,
    redo: Vec<Edit>,
    bytes: usize,
}

impl History {
    pub fn can_record(point_count: usize) -> bool {
        point_count <= MAX_BYTES / (std::mem::size_of::<usize>() + 16)
    }
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn record(&mut self, pending: PendingEdit, positions: &[[f32; 2]]) {
        let after: Vec<_> = pending.indices.iter().map(|&i| positions[i]).collect();
        if pending.before == after {
            return;
        }
        for edit in self.redo.drain(..) {
            self.bytes -= edit.bytes();
        }
        let edit = Edit {
            indices: pending.indices,
            before: pending.before,
            after,
        };
        // A single very large action invalidates the chain; never undo past it.
        if edit.bytes() > MAX_BYTES {
            self.clear();
            return;
        }
        while self.bytes + edit.bytes() > MAX_BYTES || self.undo.len() >= MAX_EDITS {
            if let Some(old) = self.undo.pop_front() {
                self.bytes -= old.bytes();
            } else {
                break;
            }
        }
        self.bytes += edit.bytes();
        self.undo.push_back(edit);
    }

    pub fn apply(&mut self, positions: &mut [[f32; 2]], redo: bool) -> bool {
        let edit = if redo {
            self.redo.pop()
        } else {
            self.undo.pop_back()
        };
        let Some(edit) = edit else {
            return false;
        };
        let values = if redo { &edit.after } else { &edit.before };
        for (&i, &p) in edit.indices.iter().zip(values) {
            positions[i] = p;
        }
        if redo {
            self.undo.push_back(edit);
        } else {
            self.redo.push(edit);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_redo_preserves_other_components_and_new_edit_clears_redo() {
        let mut history = History::default();
        let mut positions = [[1., 2.], [3., 4.], [5., 6.]];
        let before = positions[1];
        positions[1] = [30., 40.];
        history.record(
            PendingEdit {
                indices: vec![1],
                before: vec![before],
            },
            &positions,
        );
        assert!(history.apply(&mut positions, false));
        assert_eq!(positions, [[1., 2.], [3., 4.], [5., 6.]]);
        assert!(history.apply(&mut positions, true));
        assert_eq!(positions[1], [30., 40.]);
        history.apply(&mut positions, false);
        positions[1] = [7., 8.];
        history.record(
            PendingEdit {
                indices: vec![1],
                before: vec![before],
            },
            &positions,
        );
        assert!(!history.can_redo());
    }
}
