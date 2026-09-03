//! Which items are open and which one is active.
//!
//! Zed owns everything inside an item. This model owns only the ordered tab
//! list and the active index, and checks its own invariants after every
//! transition.

use std::{collections::BTreeSet, error::Error, fmt};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ItemId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceModel {
    items: Vec<ItemId>,
    active: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceError {
    DuplicateItem(ItemId),
    UnknownItem(ItemId),
    LastItem(ItemId),
    Empty,
    InvalidActiveIndex,
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateItem(item) => write!(formatter, "item {item:?} occurs more than once"),
            Self::UnknownItem(item) => write!(formatter, "unknown item {item:?}"),
            Self::LastItem(item) => write!(formatter, "item {item:?} is the last item"),
            Self::Empty => write!(formatter, "workspace has no items"),
            Self::InvalidActiveIndex => write!(formatter, "workspace active index is invalid"),
        }
    }
}

impl Error for WorkspaceError {}

impl WorkspaceModel {
    pub fn new(first: ItemId) -> Self {
        Self {
            items: vec![first],
            active: 0,
        }
    }

    pub fn items(&self) -> &[ItemId] {
        &self.items
    }

    pub fn item_ids(&self) -> BTreeSet<ItemId> {
        self.items.iter().copied().collect()
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active_item(&self) -> ItemId {
        self.items[self.active]
    }

    /// Appends a new item and activates it.
    pub fn open_item(&mut self, item: ItemId) -> Result<(), WorkspaceError> {
        if self.items.contains(&item) {
            return Err(WorkspaceError::DuplicateItem(item));
        }
        self.items.push(item);
        self.active = self.items.len() - 1;
        self.validate()
    }

    pub fn focus_item(&mut self, item: ItemId) -> Result<(), WorkspaceError> {
        self.active = self
            .items
            .iter()
            .position(|candidate| *candidate == item)
            .ok_or(WorkspaceError::UnknownItem(item))?;
        self.validate()
    }

    /// Removes an item; the neighbour toward the front becomes active when
    /// the active item closes. The last item cannot be closed.
    pub fn close_item(&mut self, item: ItemId) -> Result<(), WorkspaceError> {
        let index = self
            .items
            .iter()
            .position(|candidate| *candidate == item)
            .ok_or(WorkspaceError::UnknownItem(item))?;
        if self.items.len() == 1 {
            return Err(WorkspaceError::LastItem(item));
        }
        self.items.remove(index);
        if index < self.active || self.active >= self.items.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.validate()
    }

    /// Activates the next or previous item, wrapping at either end. Returns
    /// whether the active item changed.
    pub fn activate_adjacent_item(&mut self, forward: bool) -> bool {
        let len = self.items.len();
        if len < 2 {
            return false;
        }
        self.active = if forward {
            (self.active + 1) % len
        } else {
            (self.active + len - 1) % len
        };
        true
    }

    pub fn validate(&self) -> Result<(), WorkspaceError> {
        if self.items.is_empty() {
            return Err(WorkspaceError::Empty);
        }
        if self.active >= self.items.len() {
            return Err(WorkspaceError::InvalidActiveIndex);
        }
        let mut seen = BTreeSet::new();
        for item in &self.items {
            if !seen.insert(*item) {
                return Err(WorkspaceError::DuplicateItem(*item));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_focuses_and_closes_with_valid_invariants() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        workspace.open_item(ItemId(2)).unwrap();
        workspace.open_item(ItemId(3)).unwrap();
        assert_eq!(workspace.active_item(), ItemId(3));
        assert_eq!(
            workspace.open_item(ItemId(2)),
            Err(WorkspaceError::DuplicateItem(ItemId(2)))
        );

        workspace.focus_item(ItemId(1)).unwrap();
        workspace.close_item(ItemId(3)).unwrap();
        assert_eq!(workspace.active_item(), ItemId(1));
        workspace.close_item(ItemId(1)).unwrap();
        assert_eq!(workspace.active_item(), ItemId(2));
        assert_eq!(
            workspace.close_item(ItemId(2)),
            Err(WorkspaceError::LastItem(ItemId(2)))
        );
    }

    #[test]
    fn adjacent_activation_wraps() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        assert!(!workspace.activate_adjacent_item(true));
        workspace.open_item(ItemId(2)).unwrap();
        assert!(workspace.activate_adjacent_item(true));
        assert_eq!(workspace.active_item(), ItemId(1));
        assert!(workspace.activate_adjacent_item(false));
        assert_eq!(workspace.active_item(), ItemId(2));
    }
}
