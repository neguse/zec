//! Navigation history: where the caret was before each jump, per pane.
//!
//! Zed keeps its history on `workspace::Pane`, which zec does not hold, so
//! zec records the jumps it performs itself: opening a file, a search hit,
//! a panel entry, a language server location, a line. An entry is an item
//! and an anchor, so it follows edits; entries of closed items are dropped.

use std::collections::BTreeMap;

use text::Anchor;

use super::workspace::{ItemId, PaneId};

/// The most entries a pane keeps in each direction.
const LIMIT: usize = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Location {
    pub item: ItemId,
    pub anchor: Anchor,
}

#[derive(Debug, Default)]
struct PaneHistory {
    back: Vec<Location>,
    forward: Vec<Location>,
}

#[derive(Debug, Default)]
pub struct NavHistory {
    panes: BTreeMap<PaneId, PaneHistory>,
}

impl NavHistory {
    /// Records where a jump that landed in `pane` started. A new jump
    /// forgets the forward entries, as in every editor.
    pub fn record(&mut self, pane: PaneId, from: Location) {
        let history = self.panes.entry(pane).or_default();
        history.forward.clear();
        if history.back.last() == Some(&from) {
            return;
        }
        history.back.push(from);
        if history.back.len() > LIMIT {
            history.back.remove(0);
        }
    }

    /// The location to return to from `current`, which becomes a forward
    /// entry. Entries `live` rejects are dropped on the way.
    pub fn back(
        &mut self,
        pane: PaneId,
        current: Location,
        live: impl Fn(ItemId) -> bool,
    ) -> Option<Location> {
        let history = self.panes.get_mut(&pane)?;
        let target = pop_live(&mut history.back, live)?;
        history.forward.push(current);
        Some(target)
    }

    /// The location to advance to from `current`, which becomes a back
    /// entry.
    pub fn forward(
        &mut self,
        pane: PaneId,
        current: Location,
        live: impl Fn(ItemId) -> bool,
    ) -> Option<Location> {
        let history = self.panes.get_mut(&pane)?;
        let target = pop_live(&mut history.forward, live)?;
        history.back.push(current);
        Some(target)
    }

    /// Drops every entry of a closed item.
    pub fn forget_item(&mut self, item: ItemId) {
        for history in self.panes.values_mut() {
            history.back.retain(|location| location.item != item);
            history.forward.retain(|location| location.item != item);
        }
    }

    /// Keeps only the panes that still exist.
    pub fn retain_panes(&mut self, exists: impl Fn(PaneId) -> bool) {
        self.panes.retain(|pane, _| exists(*pane));
    }
}

fn pop_live(stack: &mut Vec<Location>, live: impl Fn(ItemId) -> bool) -> Option<Location> {
    while let Some(location) = stack.pop() {
        if live(location.item) {
            return Some(location);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const PANE: PaneId = PaneId(1);

    fn at(item: u64, anchor: Anchor) -> Location {
        Location {
            item: ItemId(item),
            anchor,
        }
    }

    fn start() -> Anchor {
        Anchor::min_for_buffer(text::BufferId::new(1).unwrap())
    }

    fn end() -> Anchor {
        Anchor::max_for_buffer(text::BufferId::new(1).unwrap())
    }

    #[test]
    fn back_and_forward_walk_the_recorded_jumps() {
        let mut history = NavHistory::default();
        let first = at(1, start());
        let second = at(2, start());
        let third = at(2, end());
        history.record(PANE, first);
        history.record(PANE, second);

        assert_eq!(history.back(PANE, third, |_| true), Some(second));
        assert_eq!(history.back(PANE, second, |_| true), Some(first));
        assert_eq!(history.back(PANE, first, |_| true), None);
        assert_eq!(history.forward(PANE, first, |_| true), Some(second));
        assert_eq!(history.forward(PANE, second, |_| true), Some(third));
        assert_eq!(history.forward(PANE, third, |_| true), None);
    }

    #[test]
    fn a_new_jump_forgets_the_forward_entries_and_repeats_collapse() {
        let mut history = NavHistory::default();
        let first = at(1, start());
        let second = at(2, start());
        history.record(PANE, first);
        assert_eq!(history.back(PANE, second, |_| true), Some(first));
        history.record(PANE, first);
        history.record(PANE, first);
        assert_eq!(history.forward(PANE, first, |_| true), None);
        assert_eq!(history.back(PANE, second, |_| true), Some(first));
        assert_eq!(history.back(PANE, first, |_| true), None);
    }

    #[test]
    fn closed_items_and_panes_are_dropped() {
        let mut history = NavHistory::default();
        let gone = at(1, start());
        let kept = at(2, start());
        history.record(PANE, kept);
        history.record(PANE, gone);
        history.forget_item(ItemId(1));
        assert_eq!(history.back(PANE, kept, |_| true), Some(kept));

        history.record(PaneId(2), kept);
        history.retain_panes(|pane| pane == PANE);
        assert_eq!(history.back(PaneId(2), kept, |_| true), None);
    }

    #[test]
    fn stale_entries_are_skipped_on_the_way_back() {
        let mut history = NavHistory::default();
        let live = at(1, start());
        let stale = at(9, start());
        history.record(PANE, live);
        history.record(PANE, stale);
        let current = at(2, start());
        assert_eq!(
            history.back(PANE, current, |item| item != ItemId(9)),
            Some(live)
        );
        assert_eq!(history.forward(PANE, live, |_| true), Some(current));
    }

    #[test]
    fn the_back_stack_is_bounded() {
        let mut history = NavHistory::default();
        for item in 0..(LIMIT as u64 + 10) {
            history.record(PANE, at(item, start()));
        }
        let mut count = 0;
        let mut current = at(u64::MAX, start());
        while let Some(target) = history.back(PANE, current, |_| true) {
            current = target;
            count += 1;
        }
        assert_eq!(count, LIMIT);
    }
}
