//! Deterministic terminal projection of Zed document-outline items.

use std::{collections::BTreeSet, ops::Range};

use anyhow::{Result, ensure};
use language::Point;
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Widget},
};

const MAX_OUTLINE_ENTRIES: usize = 100_000;
const MAX_OUTLINE_TEXT_BYTES: usize = 4_096;
const MAX_FILTER_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct OutlineEntryId(pub(crate) u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OutlineEntry {
    pub(crate) id: OutlineEntryId,
    pub(crate) depth: usize,
    pub(crate) text: String,
    pub(crate) range: Range<Point>,
    pub(crate) selection_range: Range<Point>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OutlineSnapshot {
    pub(crate) generation: u64,
    pub(crate) source_id: u64,
    pub(crate) title: String,
    pub(crate) entries: Vec<OutlineEntry>,
}

#[derive(Clone, Debug)]
pub(crate) struct OutlinePanelState {
    generation: u64,
    source_id: u64,
    title: String,
    entries: Vec<OutlineEntry>,
    selected: Option<OutlineEntryId>,
    collapsed: BTreeSet<OutlineEntryId>,
    filter: String,
    follow_cursor: bool,
}

impl OutlinePanelState {
    pub(crate) fn new(snapshot: OutlineSnapshot) -> Result<Self> {
        validate_snapshot(&snapshot)?;
        let selected = snapshot.entries.first().map(|entry| entry.id);
        Ok(Self {
            generation: snapshot.generation,
            source_id: snapshot.source_id,
            title: snapshot.title,
            entries: snapshot.entries,
            selected,
            collapsed: BTreeSet::new(),
            filter: String::new(),
            follow_cursor: true,
        })
    }

    pub(crate) fn apply_snapshot(&mut self, snapshot: OutlineSnapshot) -> Result<bool> {
        validate_snapshot(&snapshot)?;
        if snapshot.generation <= self.generation {
            return Ok(false);
        }
        let same_source = snapshot.source_id == self.source_id;
        let previous_selected = same_source.then_some(self.selected).flatten();
        if !same_source {
            self.collapsed.clear();
            self.filter.clear();
        }
        self.generation = snapshot.generation;
        self.source_id = snapshot.source_id;
        self.title = snapshot.title;
        self.entries = snapshot.entries;
        let entry_ids = self
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect::<BTreeSet<_>>();
        self.collapsed.retain(|id| entry_ids.contains(id));
        self.selected = previous_selected.filter(|id| self.entry(*id).is_some());
        self.ensure_selected_visible();
        Ok(true)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn source_id(&self) -> u64 {
        self.source_id
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn filter(&self) -> &str {
        &self.filter
    }

    pub(crate) fn set_filter(&mut self, filter: String) -> Result<()> {
        ensure!(
            filter.len() <= MAX_FILTER_BYTES,
            "outline filter is too large"
        );
        self.filter = filter;
        self.ensure_selected_visible();
        Ok(())
    }

    pub(crate) fn follow_cursor(&self) -> bool {
        self.follow_cursor
    }

    pub(crate) fn toggle_follow_cursor(&mut self) -> bool {
        self.follow_cursor = !self.follow_cursor;
        self.follow_cursor
    }

    pub(crate) fn selected_entry(&self) -> Option<&OutlineEntry> {
        self.selected.and_then(|id| self.entry(id))
    }

    pub(crate) fn select(&mut self, id: OutlineEntryId) -> Result<()> {
        ensure!(
            self.visible_indices()
                .iter()
                .any(|index| self.entries[*index].id == id),
            "cannot select a hidden outline entry"
        );
        self.selected = Some(id);
        Ok(())
    }

    pub(crate) fn move_selection(&mut self, delta: isize) -> bool {
        let visible = self.visible_indices();
        if visible.is_empty() {
            self.selected = None;
            return false;
        }
        let current = self
            .selected
            .and_then(|id| {
                visible
                    .iter()
                    .position(|index| self.entries[*index].id == id)
            })
            .unwrap_or_default();
        let next = current
            .saturating_add_signed(delta)
            .min(visible.len().saturating_sub(1));
        self.selected = Some(self.entries[visible[next]].id);
        next != current
    }

    pub(crate) fn toggle_selected(&mut self) -> bool {
        let Some(selected) = self.selected else {
            return false;
        };
        let Some(index) = self.entries.iter().position(|entry| entry.id == selected) else {
            return false;
        };
        if !self.has_children(index) {
            return false;
        }
        if !self.collapsed.remove(&selected) {
            self.collapsed.insert(selected);
        }
        self.ensure_selected_visible();
        true
    }

    pub(crate) fn collapse_selected(&mut self) -> bool {
        let Some(selected) = self.selected else {
            return false;
        };
        let Some(index) = self.entries.iter().position(|entry| entry.id == selected) else {
            return false;
        };
        if !self.has_children(index) || self.collapsed.contains(&selected) {
            return false;
        }
        self.collapsed.insert(selected);
        true
    }

    pub(crate) fn expand_selected(&mut self) -> bool {
        self.selected
            .is_some_and(|selected| self.collapsed.remove(&selected))
    }

    pub(crate) fn select_parent(&mut self) -> bool {
        let Some(selected) = self.selected else {
            return false;
        };
        let Some(index) = self.entries.iter().position(|entry| entry.id == selected) else {
            return false;
        };
        let Some(parent) = self.ancestor_indices(index).last().copied() else {
            return false;
        };
        self.selected = Some(self.entries[parent].id);
        true
    }

    pub(crate) fn select_for_cursor(&mut self, cursor: Point) -> bool {
        if !self.follow_cursor {
            return false;
        }
        let target = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.range.start <= cursor && cursor <= entry.range.end)
            .max_by_key(|(_, entry)| entry.depth)
            .map(|(index, entry)| (index, entry.id));
        let Some((index, id)) = target else {
            return false;
        };
        let changed = self.selected != Some(id);
        self.selected = Some(id);
        for ancestor in self.ancestor_indices(index) {
            self.collapsed.remove(&self.entries[ancestor].id);
        }
        changed
    }

    pub(crate) fn breadcrumbs(&self, cursor: Point) -> Vec<&str> {
        let Some(index) = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.range.start <= cursor && cursor <= entry.range.end)
            .max_by_key(|(_, entry)| entry.depth)
            .map(|(index, _)| index)
        else {
            return Vec::new();
        };
        let mut indices = self.ancestor_indices(index);
        indices.push(index);
        indices
            .into_iter()
            .map(|index| self.entries[index].text.as_str())
            .collect()
    }

    pub(crate) fn rows(&self, max_rows: usize) -> Vec<OutlineRow<'_>> {
        let visible = self.visible_indices();
        if visible.is_empty() || max_rows == 0 {
            return Vec::new();
        }
        let selected = self
            .selected
            .and_then(|id| {
                visible
                    .iter()
                    .position(|index| self.entries[*index].id == id)
            })
            .unwrap_or_default();
        let start = selected
            .saturating_sub(max_rows / 2)
            .min(visible.len().saturating_sub(max_rows));
        visible
            .into_iter()
            .skip(start)
            .take(max_rows)
            .map(|index| OutlineRow {
                entry: &self.entries[index],
                selected: self.selected == Some(self.entries[index].id),
                collapsed: self.collapsed.contains(&self.entries[index].id),
                has_children: self.has_children(index),
            })
            .collect()
    }

    pub(crate) fn entry_id_at(&self, area: Rect, position: Position) -> Option<OutlineEntryId> {
        let inner = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        if inner.is_empty() || !inner.contains(position) {
            return None;
        }
        self.rows(usize::from(inner.height))
            .get(usize::from(position.y.saturating_sub(inner.y)))
            .map(|row| row.entry.id)
    }

    pub(crate) fn entry(&self, id: OutlineEntryId) -> Option<&OutlineEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    fn has_children(&self, index: usize) -> bool {
        self.entries
            .get(index + 1)
            .is_some_and(|next| next.depth > self.entries[index].depth)
    }

    fn ancestor_indices(&self, index: usize) -> Vec<usize> {
        let mut depth = self.entries[index].depth;
        let mut ancestors = Vec::new();
        for candidate in (0..index).rev() {
            if depth == 0 {
                break;
            }
            if self.entries[candidate].depth < depth {
                ancestors.push(candidate);
                depth = self.entries[candidate].depth;
            }
        }
        ancestors.reverse();
        ancestors
    }

    fn visible_indices(&self) -> Vec<usize> {
        let normalized_filter = self.filter.to_lowercase();
        if !normalized_filter.is_empty() {
            let mut included = BTreeSet::new();
            for (index, entry) in self.entries.iter().enumerate() {
                if entry.text.to_lowercase().contains(&normalized_filter) {
                    included.insert(index);
                    included.extend(self.ancestor_indices(index));
                }
            }
            return included.into_iter().collect();
        }

        let mut visible = Vec::new();
        let mut collapsed_depth: Option<usize> = None;
        for (index, entry) in self.entries.iter().enumerate() {
            if collapsed_depth.is_some_and(|depth| entry.depth > depth) {
                continue;
            }
            collapsed_depth = None;
            visible.push(index);
            if self.collapsed.contains(&entry.id) {
                collapsed_depth = Some(entry.depth);
            }
        }
        visible
    }

    fn ensure_selected_visible(&mut self) {
        let visible = self.visible_indices();
        let normalized_filter = self.filter.to_lowercase();
        let selected_is_eligible = self.selected.is_some_and(|id| {
            visible.iter().any(|index| {
                let entry = &self.entries[*index];
                entry.id == id
                    && (normalized_filter.is_empty()
                        || entry.text.to_lowercase().contains(&normalized_filter))
            })
        });
        if !selected_is_eligible {
            self.selected = visible
                .iter()
                .copied()
                .find(|index| {
                    !normalized_filter.is_empty()
                        && self.entries[*index]
                            .text
                            .to_lowercase()
                            .contains(&normalized_filter)
                })
                .or_else(|| visible.first().copied())
                .map(|index| self.entries[index].id);
        }
    }
}

pub(crate) struct OutlineRow<'a> {
    pub(crate) entry: &'a OutlineEntry,
    pub(crate) selected: bool,
    pub(crate) collapsed: bool,
    pub(crate) has_children: bool,
}

pub(crate) struct OutlinePanelWidget<'a> {
    panel: &'a OutlinePanelState,
}

impl<'a> OutlinePanelWidget<'a> {
    pub(crate) fn new(panel: &'a OutlinePanelState) -> Self {
        Self { panel }
    }
}

impl Widget for OutlinePanelWidget<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let title = if self.panel.filter.is_empty() {
            format!(" Outline {} ", self.panel.title)
        } else {
            format!(" Outline /{} ", self.panel.filter)
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(area);
        block.render(area, buffer);
        let items = self
            .panel
            .rows(usize::from(inner.height))
            .into_iter()
            .map(|row| {
                let marker = if row.has_children {
                    if row.collapsed { "▸ " } else { "▾ " }
                } else {
                    "  "
                };
                let line = Line::from(vec![
                    Span::raw("  ".repeat(row.entry.depth)),
                    Span::raw(marker),
                    Span::raw(row.entry.text.clone()),
                ]);
                ListItem::new(line).style(if row.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                })
            })
            .collect::<Vec<_>>();
        List::new(items).render(inner, buffer);
    }
}

fn validate_snapshot(snapshot: &OutlineSnapshot) -> Result<()> {
    ensure!(
        snapshot.generation > 0,
        "outline generation must be positive"
    );
    ensure!(
        snapshot.entries.len() <= MAX_OUTLINE_ENTRIES,
        "outline has too many entries"
    );
    ensure!(
        snapshot.title.len() <= MAX_OUTLINE_TEXT_BYTES,
        "outline title is too large"
    );
    let mut ids = BTreeSet::new();
    let mut previous_depth = 0usize;
    for (index, entry) in snapshot.entries.iter().enumerate() {
        ensure!(ids.insert(entry.id), "outline contains a duplicate ID");
        ensure!(
            !entry.text.contains(['\n', '\r']) && entry.text.len() <= MAX_OUTLINE_TEXT_BYTES,
            "outline entry text is invalid"
        );
        ensure!(
            entry.range.start <= entry.range.end,
            "outline range is reversed"
        );
        ensure!(
            entry.selection_range.start <= entry.selection_range.end,
            "outline selection range is reversed"
        );
        ensure!(
            index == 0 || entry.depth <= previous_depth.saturating_add(1),
            "outline depth skips a parent level"
        );
        previous_depth = entry.depth;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(row: u32) -> Point {
        Point::new(row, 0)
    }

    fn entry(id: u64, depth: usize, text: &str, start: u32, end: u32) -> OutlineEntry {
        OutlineEntry {
            id: OutlineEntryId(id),
            depth,
            text: text.to_owned(),
            range: point(start)..point(end),
            selection_range: point(start)..point(start),
        }
    }

    fn snapshot(generation: u64) -> OutlineSnapshot {
        OutlineSnapshot {
            generation,
            source_id: 7,
            title: "main.rs".to_owned(),
            entries: vec![
                entry(1, 0, "module", 0, 20),
                entry(2, 1, "Type", 2, 15),
                entry(3, 2, "method", 5, 10),
                entry(4, 0, "other", 22, 25),
            ],
        }
    }

    #[test]
    fn collapse_filter_follow_and_breadcrumbs_are_deterministic() {
        let mut panel = OutlinePanelState::new(snapshot(1)).unwrap();
        assert_eq!(panel.rows(10).len(), 4);
        assert!(panel.toggle_selected());
        assert_eq!(panel.rows(10).len(), 2);
        panel.set_filter("method".to_owned()).unwrap();
        assert_eq!(panel.selected_entry().unwrap().text, "method");
        assert_eq!(
            panel
                .rows(10)
                .iter()
                .map(|row| row.entry.id)
                .collect::<Vec<_>>(),
            [OutlineEntryId(1), OutlineEntryId(2), OutlineEntryId(3)]
        );
        panel.select_for_cursor(point(7));
        assert_eq!(panel.selected_entry().unwrap().text, "method");
        assert_eq!(panel.breadcrumbs(point(7)), ["module", "Type", "method"]);
    }

    #[test]
    fn bordered_outline_rows_have_stable_mouse_hit_targets() {
        let mut panel = OutlinePanelState::new(snapshot(1)).unwrap();
        let area = Rect::new(40, 3, 32, 8);
        assert_eq!(
            panel.entry_id_at(area, Position::new(41, 4)),
            Some(OutlineEntryId(1))
        );
        assert_eq!(
            panel.entry_id_at(area, Position::new(41, 6)),
            Some(OutlineEntryId(3))
        );
        assert_eq!(panel.entry_id_at(area, Position::new(40, 6)), None);
        panel.select(OutlineEntryId(3)).unwrap();
        assert_eq!(panel.selected_entry().unwrap().text, "method");
    }

    #[test]
    fn stale_or_malformed_snapshots_do_not_replace_state() {
        let mut panel = OutlinePanelState::new(snapshot(2)).unwrap();
        assert!(!panel.apply_snapshot(snapshot(1)).unwrap());
        let mut malformed = snapshot(3);
        malformed.entries[2].depth = 5;
        assert!(panel.apply_snapshot(malformed).is_err());
        assert_eq!(panel.generation(), 2);
        assert_eq!(panel.source_id(), 7);
    }
}
