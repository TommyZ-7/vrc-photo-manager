//! Ordered multi-selection with constant-time membership tests.
//!
//! The grid renders a membership check per visible card and the selection bar
//! is rebuilt on every app render, so the previous `Vec<i64>` representation
//! made selecting a large library O(visible cards × selection) and cloned the
//! whole list during rendering. The ordered vector is kept for range actions;
//! a hash set answers membership questions.

use std::collections::HashSet;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PhotoSelection {
    ids: Vec<i64>,
    members: HashSet<i64>,
}

impl PhotoSelection {
    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub(crate) fn contains(&self, id: i64) -> bool {
        self.members.contains(&id)
    }

    pub(crate) fn last(&self) -> Option<i64> {
        self.ids.last().copied()
    }

    pub(crate) fn to_vec(&self) -> Vec<i64> {
        self.ids.clone()
    }

    pub(crate) fn clear(&mut self) {
        self.ids.clear();
        self.members.clear();
    }

    /// Replaces the selection, preserving the order of `ids` and dropping
    /// duplicates.
    pub(crate) fn replace(&mut self, ids: impl IntoIterator<Item = i64>) {
        self.clear();
        for id in ids {
            self.insert(id);
        }
    }

    /// Adds `id` when it is not selected yet and reports whether the
    /// selection changed.
    pub(crate) fn insert(&mut self, id: i64) -> bool {
        if self.members.insert(id) {
            self.ids.push(id);
            true
        } else {
            false
        }
    }

    pub(crate) fn remove(&mut self, id: i64) -> bool {
        if !self.members.remove(&id) {
            return false;
        }
        if let Some(index) = self.ids.iter().position(|selected| *selected == id) {
            self.ids.remove(index);
        }
        true
    }

    /// Adds `id` when missing or removes it when present. Returns whether it
    /// is selected afterwards.
    pub(crate) fn toggle(&mut self, id: i64) -> bool {
        if self.remove(id) {
            false
        } else {
            self.insert(id);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PhotoSelection;

    #[test]
    fn replace_preserves_order_and_drops_duplicates() {
        let mut selection = PhotoSelection::default();
        selection.replace([4, 2, 4, 9]);
        assert_eq!(selection.to_vec(), vec![4, 2, 9]);
        assert_eq!(selection.len(), 3);
        assert!(selection.contains(2));
        assert!(!selection.contains(3));
        assert_eq!(selection.last(), Some(9));
    }

    #[test]
    fn toggle_and_remove_keep_membership_consistent() {
        let mut selection = PhotoSelection::default();
        assert!(selection.toggle(7));
        assert!(selection.contains(7));
        assert!(!selection.toggle(7));
        assert!(!selection.contains(7));
        assert!(selection.is_empty());

        selection.replace([1, 2, 3]);
        assert!(selection.remove(2));
        assert!(!selection.remove(2));
        assert_eq!(selection.to_vec(), vec![1, 3]);
        assert!(!selection.contains(2));
    }

    #[test]
    fn clear_empties_the_selection() {
        let mut selection = PhotoSelection::default();
        selection.replace([1, 2, 3, 4]);
        selection.clear();
        assert!(selection.is_empty());
        assert!(selection.to_vec().is_empty());
        assert!(!selection.contains(1));
    }

    #[test]
    fn insert_reports_whether_the_selection_changed() {
        let mut selection = PhotoSelection::default();
        assert!(selection.insert(5));
        assert!(!selection.insert(5));
        assert_eq!(selection.len(), 1);
    }
}
