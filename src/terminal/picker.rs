//! Filterable list state for pickers such as the command palette.
//!
//! The list owns only presentation: labels, a query filter, and a selection.
//! The payload type carries whatever the caller does with the accepted entry.

use super::render::{OverlayRow, OverlaySnapshot};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickerEntry<T> {
    pub label: String,
    pub detail: String,
    pub enabled: bool,
    pub payload: T,
}

#[derive(Clone, Debug)]
pub struct PickerList<T> {
    entries: Vec<PickerEntry<T>>,
    visible: Vec<usize>,
    selected: usize,
}

impl<T: Clone> PickerList<T> {
    pub fn new(entries: Vec<PickerEntry<T>>) -> Self {
        let mut list = Self {
            entries,
            visible: Vec::new(),
            selected: 0,
        };
        list.filter("");
        list
    }

    /// Keeps entries whose label or detail contains `query`, ranking label
    /// prefixes first, then label matches, then detail matches.
    pub fn filter(&mut self, query: &str) {
        let query = query.trim().to_lowercase();
        let mut ranked = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let label = entry.label.to_lowercase();
                let detail = entry.detail.to_lowercase();
                let score = if query.is_empty() {
                    0
                } else if label.starts_with(&query) {
                    0
                } else if label.contains(&query) {
                    1
                } else if detail.contains(&query) {
                    2
                } else {
                    return None;
                };
                Some((score, index))
            })
            .collect::<Vec<_>>();
        ranked.sort_by_key(|(score, index)| (*score, *index));
        self.visible = ranked.into_iter().map(|(_, index)| index).collect();
        self.selected = 0;
    }

    pub fn select_next(&mut self) {
        if !self.visible.is_empty() {
            self.selected = (self.selected + 1) % self.visible.len();
        }
    }

    pub fn select_previous(&mut self) {
        if !self.visible.is_empty() {
            self.selected = (self.selected + self.visible.len() - 1) % self.visible.len();
        }
    }

    pub fn selected(&self) -> Option<&PickerEntry<T>> {
        self.visible
            .get(self.selected)
            .map(|index| &self.entries[*index])
    }

    /// Projects the rows around the selection into at most `row_budget` rows.
    pub fn snapshot(&self, title: &str, row_budget: usize) -> OverlaySnapshot {
        if row_budget == 0 || self.visible.is_empty() {
            return OverlaySnapshot {
                title: title.to_owned(),
                rows: Vec::new(),
                selected: None,
            };
        }
        let first = self
            .selected
            .saturating_sub(row_budget.saturating_sub(1))
            .min(self.visible.len().saturating_sub(row_budget));
        let rows = self.visible[first..]
            .iter()
            .take(row_budget)
            .map(|index| {
                let entry = &self.entries[*index];
                let text = if entry.detail.is_empty() {
                    entry.label.clone()
                } else {
                    format!("{}  {}", entry.label, entry.detail)
                };
                OverlayRow {
                    text,
                    enabled: entry.enabled,
                }
            })
            .collect();
        OverlaySnapshot {
            title: title.to_owned(),
            rows,
            selected: Some(self.selected - first),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> PickerList<u8> {
        PickerList::new(
            ["Save", "Save As", "Quit", "Open File"]
                .iter()
                .enumerate()
                .map(|(index, label)| PickerEntry {
                    label: (*label).to_owned(),
                    detail: String::new(),
                    enabled: true,
                    payload: index as u8,
                })
                .collect(),
        )
    }

    #[test]
    fn filters_by_prefix_then_substring_and_wraps_selection() {
        let mut list = list();
        list.filter("sa");
        assert_eq!(list.selected().map(|entry| entry.payload), Some(0));
        list.select_previous();
        assert_eq!(list.selected().map(|entry| entry.payload), Some(1));
        list.filter("file");
        assert_eq!(list.selected().map(|entry| entry.payload), Some(3));
        list.filter("zzz");
        assert!(list.selected().is_none());
    }

    #[test]
    fn snapshot_windows_around_the_selection() {
        let mut list = list();
        list.select_previous();
        let snapshot = list.snapshot("Commands", 2);
        assert_eq!(snapshot.rows.len(), 2);
        assert_eq!(snapshot.rows[1].text, "Open File");
        assert_eq!(snapshot.selected, Some(1));
    }
}
