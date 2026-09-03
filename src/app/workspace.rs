//! Which items are open, how they are arranged, and which one is active.
//!
//! Zed owns everything inside an item. This model owns the pane tree, each
//! pane's ordered tab list, and the active pane and tab, and checks its own
//! invariants after every transition.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ItemId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PaneId(pub u64);

/// `Horizontal` places children side by side; `Vertical` stacks them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    /// The direction as seen from the current pane, for messages.
    pub fn relative(self) -> &'static str {
        match self {
            Self::Left => "to the left",
            Self::Right => "to the right",
            Self::Up => "above",
            Self::Down => "below",
        }
    }
}

/// Split ratios are thousandths of the extent given to the first child.
pub const MIN_RATIO: u16 = 100;
pub const MAX_RATIO: u16 = 900;
pub const DEFAULT_RATIO: u16 = 500;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutNode {
    Pane(PaneId),
    Split {
        axis: Axis,
        ratio: u16,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
    /// Leaves in layout order.
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_panes(&mut out);
        out
    }

    fn collect_panes(&self, out: &mut Vec<PaneId>) {
        match self {
            Self::Pane(pane) => out.push(*pane),
            Self::Split { first, second, .. } => {
                first.collect_panes(out);
                second.collect_panes(out);
            }
        }
    }

    pub fn first_pane(&self) -> PaneId {
        match self {
            Self::Pane(pane) => *pane,
            Self::Split { first, .. } => first.first_pane(),
        }
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        match self {
            Self::Pane(candidate) => *candidate == pane,
            Self::Split { first, second, .. } => first.contains(pane) || second.contains(pane),
        }
    }

    fn replace_pane(&mut self, pane: PaneId, replacement: LayoutNode) -> bool {
        match self {
            Self::Pane(candidate) if *candidate == pane => {
                *self = replacement;
                true
            }
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.replace_pane(pane, replacement.clone())
                    || second.replace_pane(pane, replacement)
            }
        }
    }

    /// Removes a leaf, lifting its sibling into the parent's place. Returns
    /// the first pane of that sibling, or `None` when the leaf is not below
    /// a split.
    fn remove_pane(&mut self, pane: PaneId) -> Option<PaneId> {
        let Self::Split { first, second, .. } = self else {
            return None;
        };
        let sibling = if **first == Self::Pane(pane) {
            std::mem::replace(second, Box::new(Self::Pane(pane)))
        } else if **second == Self::Pane(pane) {
            std::mem::replace(first, Box::new(Self::Pane(pane)))
        } else {
            return first.remove_pane(pane).or_else(|| second.remove_pane(pane));
        };
        *self = *sibling;
        Some(self.first_pane())
    }

    /// Moves the nearest split above `pane` by `delta` thousandths toward
    /// the side holding `pane`, and returns that side's new share.
    fn adjust_ratio(&mut self, pane: PaneId, delta: i32) -> Option<u16> {
        let Self::Split {
            ratio,
            first,
            second,
            ..
        } = self
        else {
            return None;
        };
        if let Some(adjusted) = first
            .adjust_ratio(pane, delta)
            .or_else(|| second.adjust_ratio(pane, delta))
        {
            return Some(adjusted);
        }
        let in_first = first.contains(pane);
        if !in_first && !second.contains(pane) {
            return None;
        }
        let signed = if in_first { delta } else { -delta };
        let adjusted =
            (i32::from(*ratio) + signed).clamp(i32::from(MIN_RATIO), i32::from(MAX_RATIO));
        *ratio = u16::try_from(adjusted).unwrap_or(DEFAULT_RATIO);
        Some(if in_first { *ratio } else { 1000 - *ratio })
    }
}

/// One pane's ordered tabs and its active tab.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pane {
    items: Vec<ItemId>,
    active: usize,
}

impl Pane {
    pub fn new(items: Vec<ItemId>, active: usize) -> Self {
        Self { items, active }
    }

    pub fn items(&self) -> &[ItemId] {
        &self.items
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active_item(&self) -> ItemId {
        self.items[self.active]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceModel {
    root: LayoutNode,
    panes: BTreeMap<PaneId, Pane>,
    active_pane: PaneId,
    next_pane: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceError {
    DuplicateItem(ItemId),
    UnknownItem(ItemId),
    UnknownPane(PaneId),
    LastItem(ItemId),
    EmptyPane(PaneId),
    InvalidActiveIndex(PaneId),
    InvalidRatio(u16),
    LayoutMismatch,
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateItem(item) => write!(formatter, "item {item:?} occurs more than once"),
            Self::UnknownItem(item) => write!(formatter, "unknown item {item:?}"),
            Self::UnknownPane(pane) => write!(formatter, "unknown pane {pane:?}"),
            Self::LastItem(item) => write!(formatter, "item {item:?} is the last item"),
            Self::EmptyPane(pane) => write!(formatter, "pane {pane:?} has no items"),
            Self::InvalidActiveIndex(pane) => {
                write!(formatter, "pane {pane:?} has an invalid active index")
            }
            Self::InvalidRatio(ratio) => write!(formatter, "split ratio {ratio} is out of range"),
            Self::LayoutMismatch => {
                write!(formatter, "layout leaves and panes do not match one to one")
            }
        }
    }
}

impl Error for WorkspaceError {}

impl WorkspaceModel {
    pub fn new(first: ItemId) -> Self {
        let pane = PaneId(1);
        Self {
            root: LayoutNode::Pane(pane),
            panes: BTreeMap::from([(pane, Pane::new(vec![first], 0))]),
            active_pane: pane,
            next_pane: 2,
        }
    }

    /// Rebuilds a model from its parts, as a restored session does.
    pub fn from_layout(
        root: LayoutNode,
        panes: BTreeMap<PaneId, Pane>,
        active_pane: PaneId,
    ) -> Result<Self, WorkspaceError> {
        let next_pane = panes.keys().last().map_or(1, |pane| pane.0 + 1);
        let model = Self {
            root,
            panes,
            active_pane,
            next_pane,
        };
        model.validate()?;
        Ok(model)
    }

    pub fn root(&self) -> &LayoutNode {
        &self.root
    }

    pub fn panes(&self) -> impl Iterator<Item = (PaneId, &Pane)> {
        self.panes.iter().map(|(pane, state)| (*pane, state))
    }

    pub fn pane(&self, pane: PaneId) -> Option<&Pane> {
        self.panes.get(&pane)
    }

    pub fn active_pane(&self) -> PaneId {
        self.active_pane
    }

    fn active(&self) -> &Pane {
        &self.panes[&self.active_pane]
    }

    fn active_mut(&mut self) -> &mut Pane {
        self.panes
            .get_mut(&self.active_pane)
            .expect("the active pane always exists")
    }

    /// The active pane's tabs.
    pub fn items(&self) -> &[ItemId] {
        &self.active().items
    }

    /// Every open item across all panes.
    pub fn item_ids(&self) -> BTreeSet<ItemId> {
        self.panes
            .values()
            .flat_map(|pane| pane.items.iter().copied())
            .collect()
    }

    pub fn active_item(&self) -> ItemId {
        self.active().active_item()
    }

    pub fn pane_of(&self, item: ItemId) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|(_, pane)| pane.items.contains(&item))
            .map(|(pane, _)| *pane)
    }

    /// Appends a new item to the active pane and activates it.
    pub fn open_item(&mut self, item: ItemId) -> Result<(), WorkspaceError> {
        if self.pane_of(item).is_some() {
            return Err(WorkspaceError::DuplicateItem(item));
        }
        let pane = self.active_mut();
        pane.items.push(item);
        pane.active = pane.items.len() - 1;
        self.validate()
    }

    pub fn focus_item(&mut self, item: ItemId) -> Result<(), WorkspaceError> {
        let pane = self
            .pane_of(item)
            .ok_or(WorkspaceError::UnknownItem(item))?;
        self.active_pane = pane;
        let pane = self.active_mut();
        pane.active = pane
            .items
            .iter()
            .position(|candidate| *candidate == item)
            .expect("pane_of found the item");
        self.validate()
    }

    pub fn focus_pane(&mut self, pane: PaneId) -> Result<(), WorkspaceError> {
        if !self.panes.contains_key(&pane) {
            return Err(WorkspaceError::UnknownPane(pane));
        }
        self.active_pane = pane;
        self.validate()
    }

    /// Removes an item; the neighbour toward the front becomes active when
    /// the active item closes, and a pane left empty collapses into its
    /// sibling. The last item overall cannot be closed.
    pub fn close_item(&mut self, item: ItemId) -> Result<(), WorkspaceError> {
        let pane_id = self
            .pane_of(item)
            .ok_or(WorkspaceError::UnknownItem(item))?;
        if self.item_ids().len() == 1 {
            return Err(WorkspaceError::LastItem(item));
        }
        let pane = self
            .panes
            .get_mut(&pane_id)
            .expect("pane_of found the pane");
        let index = pane
            .items
            .iter()
            .position(|candidate| *candidate == item)
            .expect("pane_of found the item");
        pane.items.remove(index);
        if pane.items.is_empty() {
            self.remove_pane(pane_id);
        } else if index < pane.active || pane.active >= pane.items.len() {
            pane.active = pane.active.saturating_sub(1);
        }
        self.validate()
    }

    fn remove_pane(&mut self, pane: PaneId) {
        let sibling = self
            .root
            .remove_pane(pane)
            .expect("an emptied pane always sits below a split");
        self.panes.remove(&pane);
        if self.active_pane == pane {
            self.active_pane = sibling;
        }
    }

    /// Activates the next or previous tab of the active pane, wrapping at
    /// either end. Returns whether the active item changed.
    pub fn activate_adjacent_item(&mut self, forward: bool) -> bool {
        let pane = self.active_mut();
        let len = pane.items.len();
        if len < 2 {
            return false;
        }
        pane.active = if forward {
            (pane.active + 1) % len
        } else {
            (pane.active + len - 1) % len
        };
        true
    }

    /// Splits the active pane along `axis`; the new pane holds `item` and
    /// becomes active.
    pub fn split_active(&mut self, axis: Axis, item: ItemId) -> Result<PaneId, WorkspaceError> {
        if self.pane_of(item).is_some() {
            return Err(WorkspaceError::DuplicateItem(item));
        }
        let pane = PaneId(self.next_pane);
        self.next_pane += 1;
        let split = LayoutNode::Split {
            axis,
            ratio: DEFAULT_RATIO,
            first: Box::new(LayoutNode::Pane(self.active_pane)),
            second: Box::new(LayoutNode::Pane(pane)),
        };
        self.root.replace_pane(self.active_pane, split);
        self.panes.insert(pane, Pane::new(vec![item], 0));
        self.active_pane = pane;
        self.validate()?;
        Ok(pane)
    }

    /// Moves the active tab into `target`, which becomes active; a pane
    /// left empty collapses. Returns whether anything moved.
    pub fn move_active_item(&mut self, target: PaneId) -> Result<bool, WorkspaceError> {
        if !self.panes.contains_key(&target) {
            return Err(WorkspaceError::UnknownPane(target));
        }
        if target == self.active_pane {
            return Ok(false);
        }
        let source = self.active_pane;
        let item = self.active_item();
        let pane = self.active_mut();
        pane.items.remove(pane.active);
        if pane.items.is_empty() {
            self.remove_pane(source);
        } else if pane.active >= pane.items.len() {
            pane.active = pane.items.len() - 1;
        }
        let pane = self.panes.get_mut(&target).expect("target exists");
        pane.items.push(item);
        pane.active = pane.items.len() - 1;
        self.active_pane = target;
        self.validate()?;
        Ok(true)
    }

    /// Grows (positive) or shrinks the active pane inside its nearest
    /// split and returns its share in thousandths; `None` without a split.
    pub fn adjust_split(&mut self, delta: i32) -> Option<u16> {
        self.root.adjust_ratio(self.active_pane, delta)
    }

    pub fn validate(&self) -> Result<(), WorkspaceError> {
        let leaves = self.root.panes();
        let leaf_set = leaves.iter().copied().collect::<BTreeSet<_>>();
        if leaf_set.len() != leaves.len() || leaf_set != self.panes.keys().copied().collect() {
            return Err(WorkspaceError::LayoutMismatch);
        }
        if !self.panes.contains_key(&self.active_pane) {
            return Err(WorkspaceError::UnknownPane(self.active_pane));
        }
        let mut seen = BTreeSet::new();
        for (id, pane) in &self.panes {
            if pane.items.is_empty() {
                return Err(WorkspaceError::EmptyPane(*id));
            }
            if pane.active >= pane.items.len() {
                return Err(WorkspaceError::InvalidActiveIndex(*id));
            }
            for item in &pane.items {
                if !seen.insert(*item) {
                    return Err(WorkspaceError::DuplicateItem(*item));
                }
            }
        }
        validate_ratios(&self.root)
    }
}

fn validate_ratios(node: &LayoutNode) -> Result<(), WorkspaceError> {
    match node {
        LayoutNode::Pane(_) => Ok(()),
        LayoutNode::Split {
            ratio,
            first,
            second,
            ..
        } => {
            if !(MIN_RATIO..=MAX_RATIO).contains(ratio) {
                return Err(WorkspaceError::InvalidRatio(*ratio));
            }
            validate_ratios(first)?;
            validate_ratios(second)
        }
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

    #[test]
    fn splits_move_and_collapse() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        let right = workspace.split_active(Axis::Horizontal, ItemId(2)).unwrap();
        assert_eq!(workspace.active_pane(), right);
        assert_eq!(workspace.root().panes(), vec![PaneId(1), right]);
        assert_eq!(
            workspace.split_active(Axis::Vertical, ItemId(1)),
            Err(WorkspaceError::DuplicateItem(ItemId(1)))
        );

        // Moving the only tab out of the right pane collapses it.
        assert!(workspace.move_active_item(PaneId(1)).unwrap());
        assert_eq!(workspace.root(), &LayoutNode::Pane(PaneId(1)));
        assert_eq!(workspace.items(), &[ItemId(1), ItemId(2)]);
        assert_eq!(workspace.active_item(), ItemId(2));
        assert!(workspace.adjust_split(50).is_none());

        // Closing the last tab of a split pane collapses it and refocuses
        // the sibling.
        let below = workspace.split_active(Axis::Vertical, ItemId(3)).unwrap();
        assert_eq!(workspace.adjust_split(50), Some(550));
        workspace.close_item(ItemId(3)).unwrap();
        assert_eq!(workspace.pane(below), None);
        assert_eq!(workspace.active_pane(), PaneId(1));
        assert_eq!(workspace.active_item(), ItemId(2));
    }

    #[test]
    fn rebuilds_from_parts_and_rejects_mismatches() {
        let root = LayoutNode::Split {
            axis: Axis::Horizontal,
            ratio: 300,
            first: Box::new(LayoutNode::Pane(PaneId(1))),
            second: Box::new(LayoutNode::Pane(PaneId(2))),
        };
        let panes = BTreeMap::from([
            (PaneId(1), Pane::new(vec![ItemId(1)], 0)),
            (PaneId(2), Pane::new(vec![ItemId(2), ItemId(3)], 1)),
        ]);
        let workspace =
            WorkspaceModel::from_layout(root.clone(), panes.clone(), PaneId(2)).unwrap();
        assert_eq!(workspace.active_item(), ItemId(3));
        assert_eq!(workspace.item_ids().len(), 3);

        let mut missing = panes.clone();
        missing.remove(&PaneId(2));
        assert_eq!(
            WorkspaceModel::from_layout(root.clone(), missing, PaneId(1)).unwrap_err(),
            WorkspaceError::LayoutMismatch
        );
        let mut duplicate = panes;
        duplicate.insert(PaneId(2), Pane::new(vec![ItemId(1)], 0));
        assert_eq!(
            WorkspaceModel::from_layout(root, duplicate, PaneId(1)).unwrap_err(),
            WorkspaceError::DuplicateItem(ItemId(1))
        );
    }
}
