//! Project-panel projection over Zed Worktree entry identities.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context as _, Result, bail, ensure};
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem, Widget},
};
use worktree::ProjectEntryId;

use crate::repository::RepositoryRoot;

const MAX_PANEL_ENTRIES: usize = 1_000_000;
const MAX_FILTER_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PanelEntryKind {
    Directory,
    File,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PanelEntry {
    pub(crate) id: ProjectEntryId,
    pub(crate) path: PathBuf,
    pub(crate) kind: PanelEntryKind,
    pub(crate) ignored: bool,
    pub(crate) hidden: bool,
    pub(crate) external: bool,
    pub(crate) private: bool,
    pub(crate) fifo: bool,
}

impl PanelEntry {
    pub(crate) fn is_directory(&self) -> bool {
        self.kind == PanelEntryKind::Directory
    }

    pub(crate) fn openable(&self) -> bool {
        !self.fifo && !self.external
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PanelSnapshot {
    pub(crate) generation: u64,
    pub(crate) entries: Vec<PanelEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PanelRow {
    pub(crate) id: ProjectEntryId,
    pub(crate) depth: usize,
    pub(crate) selected: bool,
    pub(crate) expanded: bool,
    pub(crate) has_children: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectPanelState {
    generation: u64,
    entries: BTreeMap<ProjectEntryId, PanelEntry>,
    ids_by_path: BTreeMap<PathBuf, ProjectEntryId>,
    children: BTreeMap<ProjectEntryId, Vec<ProjectEntryId>>,
    root: ProjectEntryId,
    expanded: BTreeSet<ProjectEntryId>,
    selected: ProjectEntryId,
    filter: String,
    show_ignored: bool,
    show_hidden: bool,
}

impl ProjectPanelState {
    pub(crate) fn new(snapshot: PanelSnapshot) -> Result<Self> {
        let mut panel = Self {
            generation: 0,
            entries: BTreeMap::new(),
            ids_by_path: BTreeMap::new(),
            children: BTreeMap::new(),
            root: ProjectEntryId::MIN,
            expanded: BTreeSet::new(),
            selected: ProjectEntryId::MIN,
            filter: String::new(),
            show_ignored: false,
            show_hidden: false,
        };
        ensure!(
            panel.apply_snapshot(snapshot)?,
            "initial panel snapshot was stale"
        );
        Ok(panel)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn selected_entry(&self) -> &PanelEntry {
        &self.entries[&self.selected]
    }

    pub(crate) fn plan_remote_mutation(
        &self,
        root: &RepositoryRoot,
        kind: MutationKind,
        source_relative: Option<&Path>,
        target_relative: Option<&Path>,
        trusted: bool,
    ) -> Result<MutationPlan> {
        ensure!(trusted, "project mutation requires a trusted worktree");
        let source_entry = source_relative
            .map(|path| {
                validate_mutation_relative_path(path)?;
                self.ids_by_path
                    .get(path)
                    .and_then(|id| self.entries.get(id))
                    .with_context(|| {
                        format!("remote mutation source does not exist: {}", path.display())
                    })
            })
            .transpose()?;
        if let Some(source) = source_entry {
            ensure!(
                !source.external && !source.fifo,
                "remote mutation source is not a regular worktree entry"
            );
        }
        if let Some(target) = target_relative {
            validate_mutation_relative_path(target)?;
            ensure!(
                !self.ids_by_path.contains_key(target),
                "remote mutation target already exists"
            );
            let parent = target.parent().unwrap_or_else(|| Path::new(""));
            let parent_id = self.ids_by_path.get(parent).with_context(|| {
                format!(
                    "remote mutation target parent does not exist: {}",
                    parent.display()
                )
            })?;
            ensure!(
                self.entries[parent_id].is_directory(),
                "remote mutation target parent is not a directory"
            );
            let target_name = target
                .file_name()
                .context("remote mutation target has no filename")?
                .to_string_lossy()
                .to_lowercase();
            ensure!(
                !self.entries.values().any(|entry| {
                    entry.path.parent().unwrap_or_else(|| Path::new("")) == parent
                        && entry.path.file_name().is_some_and(|name| {
                            name.to_string_lossy().to_lowercase() == target_name
                        })
                }),
                "remote mutation target has a case-fold collision"
            );
        }
        validate_mutation_shape(kind, source_relative, target_relative)?;
        let source = source_relative.map(|path| root.join(path)).transpose()?;
        let target = target_relative.map(|path| root.join(path)).transpose()?;
        ensure!(
            source != target || source.is_none(),
            "source and target are identical"
        );
        Ok(MutationPlan {
            kind,
            source,
            target,
            requires_confirmation: true,
        })
    }

    pub(crate) fn entry(&self, id: ProjectEntryId) -> Option<&PanelEntry> {
        self.entries.get(&id)
    }

    pub(crate) fn filter(&self) -> &str {
        &self.filter
    }

    pub(crate) fn show_ignored(&self) -> bool {
        self.show_ignored
    }

    pub(crate) fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    pub(crate) fn is_expanded(&self, id: ProjectEntryId) -> bool {
        self.expanded.contains(&id)
    }

    pub(crate) fn apply_snapshot(&mut self, snapshot: PanelSnapshot) -> Result<bool> {
        if snapshot.generation <= self.generation {
            return Ok(false);
        }
        ensure!(
            !snapshot.entries.is_empty() && snapshot.entries.len() <= MAX_PANEL_ENTRIES,
            "project panel snapshot has an invalid entry count"
        );
        let previous_selected_path = self
            .entries
            .get(&self.selected)
            .map(|entry| entry.path.clone());
        let mut entries = BTreeMap::new();
        let mut ids_by_path = BTreeMap::new();
        let mut roots = Vec::new();
        for entry in snapshot.entries {
            validate_relative_path(&entry.path, true)?;
            ensure!(
                entries.insert(entry.id, entry.clone()).is_none(),
                "duplicate project entry ID"
            );
            ensure!(
                ids_by_path.insert(entry.path.clone(), entry.id).is_none(),
                "duplicate project entry path"
            );
            if entry.path.as_os_str().is_empty() {
                roots.push(entry.id);
                ensure!(
                    entry.is_directory(),
                    "project root entry is not a directory"
                );
            }
        }
        ensure!(
            roots.len() == 1,
            "project panel requires exactly one root entry"
        );
        let root = roots[0];
        let mut children: BTreeMap<ProjectEntryId, Vec<ProjectEntryId>> = BTreeMap::new();
        for entry in entries.values() {
            if entry.id == root {
                continue;
            }
            let parent_path = entry.path.parent().unwrap_or_else(|| Path::new(""));
            let parent = ids_by_path
                .get(parent_path)
                .with_context(|| format!("entry {} has no parent", entry.path.display()))?;
            ensure!(
                entries[parent].is_directory(),
                "entry parent is not a directory"
            );
            children.entry(*parent).or_default().push(entry.id);
        }
        for child_ids in children.values_mut() {
            child_ids.sort_by(|left, right| entry_order(&entries[left], &entries[right]));
        }

        let mut expanded = self
            .expanded
            .iter()
            .copied()
            .filter(|id| entries.get(id).is_some_and(PanelEntry::is_directory))
            .collect::<BTreeSet<_>>();
        expanded.insert(root);
        let selected = if entries.contains_key(&self.selected) {
            self.selected
        } else {
            previous_selected_path
                .as_deref()
                .and_then(|path| nearest_existing_ancestor(path, &ids_by_path))
                .unwrap_or(root)
        };

        self.generation = snapshot.generation;
        self.entries = entries;
        self.ids_by_path = ids_by_path;
        self.children = children;
        self.root = root;
        self.expanded = expanded;
        self.selected = selected;
        self.ensure_selected_visible();
        Ok(true)
    }

    pub(crate) fn set_filter(&mut self, filter: String) -> Result<()> {
        ensure!(
            filter.len() <= MAX_FILTER_BYTES,
            "project panel filter is too large"
        );
        self.filter = filter;
        self.ensure_selected_visible();
        let normalized_filter = self.filter.to_lowercase();
        if !normalized_filter.is_empty()
            && !self.entries[&self.selected]
                .path
                .to_string_lossy()
                .to_lowercase()
                .contains(&normalized_filter)
            && let Some(matching) = self.visible_ids().into_iter().find(|id| {
                self.entries[id]
                    .path
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(&normalized_filter)
            })
        {
            self.selected = matching;
        }
        Ok(())
    }

    pub(crate) fn set_show_ignored(&mut self, visible: bool) {
        self.show_ignored = visible;
        self.ensure_selected_visible();
    }

    pub(crate) fn set_show_hidden(&mut self, visible: bool) {
        self.show_hidden = visible;
        self.ensure_selected_visible();
    }

    pub(crate) fn toggle_expanded(&mut self, id: ProjectEntryId) -> Result<bool> {
        let entry = self
            .entries
            .get(&id)
            .with_context(|| format!("unknown project entry {id:?}"))?;
        ensure!(entry.is_directory(), "cannot expand a file entry");
        if id == self.root {
            return Ok(true);
        }
        let expanded = if self.expanded.remove(&id) {
            false
        } else {
            self.expanded.insert(id);
            true
        };
        self.ensure_selected_visible();
        Ok(expanded)
    }

    pub(crate) fn select(&mut self, id: ProjectEntryId) -> Result<()> {
        ensure!(
            self.visible_ids().contains(&id),
            "cannot select a hidden project entry"
        );
        self.selected = id;
        Ok(())
    }

    pub(crate) fn move_selection(&mut self, delta: isize) -> bool {
        let visible = self.visible_ids();
        let Some(index) = visible.iter().position(|id| *id == self.selected) else {
            return false;
        };
        let next = index
            .saturating_add_signed(delta)
            .min(visible.len().saturating_sub(1));
        if next == index {
            return false;
        }
        self.selected = visible[next];
        true
    }

    pub(crate) fn reveal_path(&mut self, path: &Path) -> Result<ProjectEntryId> {
        let id = *self
            .ids_by_path
            .get(path)
            .with_context(|| format!("project path {} is not indexed", path.display()))?;
        let mut ancestor = path.parent();
        while let Some(path) = ancestor {
            if let Some(id) = self.ids_by_path.get(path) {
                self.expanded.insert(*id);
            }
            ancestor = path.parent();
        }
        self.selected = id;
        Ok(id)
    }

    pub(crate) fn rows(&self, max_rows: usize) -> Vec<PanelRow> {
        let visible = self.visible_ids();
        if visible.is_empty() || max_rows == 0 {
            return Vec::new();
        }
        let selected = visible
            .iter()
            .position(|id| *id == self.selected)
            .unwrap_or_default();
        let start = selected
            .saturating_sub(max_rows / 2)
            .min(visible.len().saturating_sub(max_rows));
        visible
            .into_iter()
            .skip(start)
            .take(max_rows)
            .map(|id| {
                let entry = &self.entries[&id];
                PanelRow {
                    id,
                    depth: entry.path.components().count(),
                    selected: id == self.selected,
                    expanded: self.expanded.contains(&id),
                    has_children: self
                        .children
                        .get(&id)
                        .is_some_and(|children| !children.is_empty()),
                }
            })
            .collect()
    }

    pub(crate) fn entry_id_at(&self, area: Rect, position: Position) -> Option<ProjectEntryId> {
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
            .map(|row| row.id)
    }

    fn visible_ids(&self) -> Vec<ProjectEntryId> {
        let filter = self.filter.to_lowercase();
        let matches = self
            .entries
            .values()
            .filter(|entry| self.entry_allowed(entry))
            .filter(|entry| {
                filter.is_empty()
                    || entry
                        .path
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&filter)
            })
            .map(|entry| entry.id)
            .collect::<BTreeSet<_>>();
        let filter_ancestors = if filter.is_empty() {
            BTreeSet::new()
        } else {
            matches
                .iter()
                .flat_map(|id| self.ancestor_ids(*id))
                .collect::<BTreeSet<_>>()
        };
        let mut output = Vec::new();
        self.visit_visible(
            self.root,
            &matches,
            &filter_ancestors,
            !filter.is_empty(),
            &mut output,
        );
        output
    }

    fn visit_visible(
        &self,
        id: ProjectEntryId,
        matches: &BTreeSet<ProjectEntryId>,
        filter_ancestors: &BTreeSet<ProjectEntryId>,
        filtering: bool,
        output: &mut Vec<ProjectEntryId>,
    ) {
        let entry = &self.entries[&id];
        if !self.entry_allowed(entry) && id != self.root {
            return;
        }
        if !filtering || matches.contains(&id) || filter_ancestors.contains(&id) {
            output.push(id);
        }
        let descend = if filtering {
            filter_ancestors.contains(&id)
        } else {
            self.expanded.contains(&id)
        };
        if descend {
            for child in self.children.get(&id).into_iter().flatten() {
                self.visit_visible(*child, matches, filter_ancestors, filtering, output);
            }
        }
    }

    fn ancestor_ids(&self, id: ProjectEntryId) -> Vec<ProjectEntryId> {
        let mut ancestors = Vec::new();
        let mut path = self.entries[&id].path.parent();
        while let Some(parent) = path {
            if let Some(id) = self.ids_by_path.get(parent) {
                ancestors.push(*id);
            }
            path = parent.parent();
        }
        ancestors
    }

    fn entry_allowed(&self, entry: &PanelEntry) -> bool {
        (self.show_ignored || !entry.ignored) && (self.show_hidden || !entry.hidden)
    }

    fn ensure_selected_visible(&mut self) {
        let visible = self.visible_ids();
        if !visible.contains(&self.selected) {
            let normalized_filter = self.filter.to_lowercase();
            self.selected = (!normalized_filter.is_empty())
                .then(|| {
                    visible.iter().copied().find(|id| {
                        self.entries[id]
                            .path
                            .to_string_lossy()
                            .to_lowercase()
                            .contains(&normalized_filter)
                    })
                })
                .flatten()
                .or_else(|| visible.first().copied())
                .unwrap_or(self.root);
        }
    }
}

pub(crate) struct ProjectPanelWidget<'a> {
    panel: &'a ProjectPanelState,
}

impl<'a> ProjectPanelWidget<'a> {
    pub(crate) fn new(panel: &'a ProjectPanelState) -> Self {
        Self { panel }
    }
}

impl Widget for ProjectPanelWidget<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        Clear.render(area, buffer);
        let mut title = " Project".to_owned();
        if !self.panel.filter().is_empty() {
            title.push_str(&format!(" /{}", self.panel.filter()));
        }
        if self.panel.show_ignored() {
            title.push_str(" +ignored");
        }
        if self.panel.show_hidden() {
            title.push_str(" +hidden");
        }
        title.push(' ');
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(area);
        block.render(area, buffer);
        if inner.is_empty() {
            return;
        }

        let items = self
            .panel
            .rows(usize::from(inner.height))
            .into_iter()
            .filter_map(|row| {
                let entry = self.panel.entry(row.id)?;
                let marker = if entry.is_directory() {
                    if row.expanded {
                        "▾"
                    } else if row.has_children {
                        "▸"
                    } else {
                        "·"
                    }
                } else {
                    " "
                };
                let name = entry
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| ".".to_owned());
                let mut suffix = String::new();
                if entry.private {
                    suffix.push_str(" [private]");
                }
                if entry.ignored {
                    suffix.push_str(" [ignored]");
                }
                if entry.hidden {
                    suffix.push_str(" [hidden]");
                }
                if entry.external {
                    suffix.push_str(" [external]");
                }
                if entry.fifo {
                    suffix.push_str(" [special]");
                }
                let text = format!("{}{} {}{}", "  ".repeat(row.depth), marker, name, suffix);
                let style = if row.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                Some(ListItem::new(text).style(style))
            })
            .collect::<Vec<_>>();
        List::new(items).render(inner, buffer);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MutationKind {
    CreateFile,
    CreateDirectory,
    Rename,
    Copy,
    Delete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MutationPlan {
    pub(crate) kind: MutationKind,
    pub(crate) source: Option<PathBuf>,
    pub(crate) target: Option<PathBuf>,
    pub(crate) requires_confirmation: bool,
}

pub(crate) fn plan_mutation(
    root: &Path,
    kind: MutationKind,
    source_relative: Option<&Path>,
    target_relative: Option<&Path>,
    trusted: bool,
) -> Result<MutationPlan> {
    ensure!(trusted, "project mutation requires a trusted worktree");
    let canonical_root = fs_canonical_directory(root)?;
    let source = source_relative
        .map(|path| resolve_beneath(&canonical_root, path, true))
        .transpose()?;
    let target = target_relative
        .map(|path| resolve_beneath(&canonical_root, path, false))
        .transpose()?;
    if let Some(source_relative) = source_relative {
        let source_path = canonical_root.join(source_relative);
        let metadata = fs::symlink_metadata(&source_path)
            .with_context(|| format!("inspect mutation source {}", source_path.display()))?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "project mutation source must not be a symlink"
        );
        ensure!(
            metadata.is_file() || metadata.is_dir(),
            "project mutation source must be a regular file or directory"
        );
    }
    if let Some(target_relative) = target_relative {
        let target_path = canonical_root.join(target_relative);
        match fs::symlink_metadata(&target_path) {
            Ok(_) => bail!("project mutation target already exists"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect mutation target {}", target_path.display()));
            }
        }
        let parent = target_path
            .parent()
            .context("project mutation target has no parent")?;
        let parent_metadata = fs::metadata(parent)
            .with_context(|| format!("inspect mutation target parent {}", parent.display()))?;
        ensure!(
            parent_metadata.is_dir(),
            "project mutation target parent is not a directory"
        );
        let target_name = target_path
            .file_name()
            .context("project mutation target has no filename")?;
        let folded_target = target_name.to_string_lossy().to_lowercase();
        for entry in fs::read_dir(parent)
            .with_context(|| format!("read mutation target parent {}", parent.display()))?
        {
            let entry = entry.context("read mutation target sibling")?;
            let name = entry.file_name();
            ensure!(
                name.to_string_lossy().to_lowercase() != folded_target,
                "project mutation target has a case-fold collision"
            );
        }
    }
    validate_mutation_shape(kind, source_relative, target_relative)?;
    Ok(MutationPlan {
        kind,
        source,
        target,
        requires_confirmation: true,
    })
}

fn validate_mutation_shape(
    kind: MutationKind,
    source: Option<&Path>,
    target: Option<&Path>,
) -> Result<()> {
    match kind {
        MutationKind::CreateFile | MutationKind::CreateDirectory => {
            ensure!(
                source.is_none() && target.is_some(),
                "create requires only a target"
            );
        }
        MutationKind::Rename | MutationKind::Copy => {
            ensure!(
                source.is_some() && target.is_some(),
                "rename/copy requires source and target"
            );
            ensure!(source != target, "source and target are identical");
        }
        MutationKind::Delete => {
            ensure!(
                source.is_some() && target.is_none(),
                "delete requires only a source"
            );
        }
    }
    Ok(())
}

fn validate_mutation_relative_path(path: &Path) -> Result<()> {
    validate_relative_path(path, false)?;
    ensure!(
        !path
            .components()
            .any(|component| component.as_os_str() == ".git"),
        "project metadata paths cannot be mutated"
    );
    Ok(())
}

fn resolve_beneath(root: &Path, relative: &Path, must_exist: bool) -> Result<PathBuf> {
    validate_relative_path(relative, false)?;
    ensure!(
        !relative
            .components()
            .any(|component| component.as_os_str() == ".git"),
        "project metadata paths cannot be mutated"
    );
    let candidate = root.join(relative);
    let mut existing = candidate.as_path();
    let mut missing_suffix = Vec::new();
    while fs::symlink_metadata(existing).is_err() {
        let name = existing
            .file_name()
            .context("mutation target has no existing ancestor")?;
        missing_suffix.push(name.to_os_string());
        existing = existing
            .parent()
            .context("mutation target escaped while resolving parent")?;
    }
    if must_exist {
        ensure!(missing_suffix.is_empty(), "mutation source does not exist");
    }
    let canonical_existing = fs_canonical(existing)?;
    ensure!(
        canonical_existing.starts_with(root),
        "mutation path escapes the worktree through a symlink"
    );
    let mut resolved = canonical_existing;
    for component in missing_suffix.into_iter().rev() {
        resolved.push(component);
    }
    ensure!(
        resolved.starts_with(root),
        "mutation path escapes the worktree"
    );
    Ok(resolved)
}

fn fs_canonical(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))
}

fn fs_canonical_directory(path: &Path) -> Result<PathBuf> {
    let canonical = fs_canonical(path)?;
    ensure!(canonical.is_dir(), "project root is not a directory");
    Ok(canonical)
}

fn validate_relative_path(path: &Path, allow_empty: bool) -> Result<()> {
    ensure!(!path.is_absolute(), "project entry path must be relative");
    ensure!(
        allow_empty || !path.as_os_str().is_empty(),
        "project entry path must not be empty"
    );
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir if allow_empty => {}
            _ => bail!("project entry path contains a non-normal component"),
        }
    }
    Ok(())
}

fn nearest_existing_ancestor(
    path: &Path,
    ids_by_path: &BTreeMap<PathBuf, ProjectEntryId>,
) -> Option<ProjectEntryId> {
    let mut candidate = Some(path);
    while let Some(path) = candidate {
        if let Some(id) = ids_by_path.get(path) {
            return Some(*id);
        }
        candidate = path.parent();
    }
    None
}

fn entry_order(left: &PanelEntry, right: &PanelEntry) -> std::cmp::Ordering {
    right
        .is_directory()
        .cmp(&left.is_directory())
        .then_with(|| left.path.cmp(&right.path))
        .then_with(|| left.id.cmp(&right.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: usize) -> ProjectEntryId {
        ProjectEntryId::from_usize(value)
    }

    fn entry(value: usize, path: &str, kind: PanelEntryKind) -> PanelEntry {
        PanelEntry {
            id: id(value),
            path: PathBuf::from(path),
            kind,
            ignored: false,
            hidden: false,
            external: false,
            private: false,
            fifo: false,
        }
    }

    fn snapshot(generation: u64) -> PanelSnapshot {
        PanelSnapshot {
            generation,
            entries: vec![
                entry(1, "", PanelEntryKind::Directory),
                entry(2, "src", PanelEntryKind::Directory),
                entry(3, "src/main.rs", PanelEntryKind::File),
                entry(4, "src/lib.rs", PanelEntryKind::File),
                entry(5, "README.md", PanelEntryKind::File),
            ],
        }
    }

    #[test]
    fn expansion_filter_selection_and_virtualization_are_deterministic() {
        let mut panel = ProjectPanelState::new(snapshot(1)).unwrap();
        assert_eq!(
            panel.rows(10).iter().map(|row| row.id).collect::<Vec<_>>(),
            [id(1), id(2), id(5)]
        );
        panel.toggle_expanded(id(2)).unwrap();
        panel.select(id(4)).unwrap();
        assert_eq!(panel.selected_entry().path, Path::new("src/lib.rs"));
        panel.set_filter("main".to_owned()).unwrap();
        assert_eq!(
            panel.rows(2).iter().map(|row| row.id).collect::<Vec<_>>(),
            [id(2), id(3)]
        );
        assert_eq!(panel.rows(2).iter().filter(|row| row.selected).count(), 1);
        assert!(panel.move_selection(-1));
        assert_eq!(panel.selected_entry().id, id(2));
    }

    #[test]
    fn filtering_moves_the_root_selection_to_the_first_real_match() {
        let mut panel = ProjectPanelState::new(snapshot(1)).unwrap();
        assert_eq!(panel.selected_entry().id, id(1));
        panel.set_filter("main.rs".to_owned()).unwrap();
        assert_eq!(panel.selected_entry().id, id(3));
    }

    #[test]
    fn bordered_panel_rows_have_stable_mouse_hit_targets() {
        let mut panel = ProjectPanelState::new(snapshot(1)).unwrap();
        let area = Rect::new(10, 5, 30, 8);
        assert_eq!(panel.entry_id_at(area, Position::new(11, 6)), Some(id(1)));
        assert_eq!(panel.entry_id_at(area, Position::new(11, 7)), Some(id(2)));
        assert_eq!(panel.entry_id_at(area, Position::new(10, 7)), None);
        panel.select(id(2)).unwrap();
        panel.toggle_expanded(id(2)).unwrap();
        assert_eq!(panel.entry_id_at(area, Position::new(11, 8)), Some(id(4)));
    }

    #[test]
    fn stale_snapshot_is_ignored_and_removed_selection_falls_back_to_parent() {
        let mut panel = ProjectPanelState::new(snapshot(2)).unwrap();
        panel.toggle_expanded(id(2)).unwrap();
        panel.select(id(3)).unwrap();
        assert!(!panel.apply_snapshot(snapshot(1)).unwrap());
        let mut updated = snapshot(3);
        updated.entries.retain(|entry| entry.id != id(3));
        assert!(panel.apply_snapshot(updated).unwrap());
        assert_eq!(panel.selected_entry().id, id(2));
        assert_eq!(panel.generation(), 3);
    }

    #[test]
    fn ignored_hidden_and_special_entries_have_explicit_visibility_and_capability() {
        let mut data = snapshot(1);
        data.entries[3].ignored = true;
        data.entries[4].hidden = true;
        data.entries.push(PanelEntry {
            fifo: true,
            ..entry(6, "pipe", PanelEntryKind::File)
        });
        let mut panel = ProjectPanelState::new(data).unwrap();
        assert_eq!(panel.rows(20).len(), 3);
        panel.set_show_ignored(true);
        panel.set_show_hidden(true);
        panel.toggle_expanded(id(2)).unwrap();
        assert_eq!(panel.rows(20).len(), 6);
        assert!(!panel.entries[&id(6)].openable());
    }

    #[test]
    fn reveal_expands_ancestors_and_preserves_zed_entry_identity() {
        let mut panel = ProjectPanelState::new(snapshot(1)).unwrap();
        assert_eq!(panel.reveal_path(Path::new("src/main.rs")).unwrap(), id(3));
        assert_eq!(panel.selected_entry().id, id(3));
        assert!(panel.expanded.contains(&id(2)));
    }

    #[test]
    fn malformed_snapshot_is_rejected_without_replacing_last_good_state() {
        let mut panel = ProjectPanelState::new(snapshot(1)).unwrap();
        let mut invalid = snapshot(2);
        invalid
            .entries
            .push(entry(6, "missing/child", PanelEntryKind::File));
        assert!(panel.apply_snapshot(invalid).is_err());
        assert_eq!(panel.generation(), 1);
        assert_eq!(panel.entries.len(), 5);
    }

    #[test]
    fn mutation_plans_are_canonical_trust_gated_and_shape_checked() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("src")).unwrap();
        std::fs::write(directory.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        assert!(
            plan_mutation(
                directory.path(),
                MutationKind::Delete,
                Some(Path::new("src/main.rs")),
                None,
                false,
            )
            .is_err()
        );
        let rename = plan_mutation(
            directory.path(),
            MutationKind::Rename,
            Some(Path::new("src/main.rs")),
            Some(Path::new("src/app.rs")),
            true,
        )
        .unwrap();
        assert_eq!(rename.kind, MutationKind::Rename);
        assert!(rename.requires_confirmation);
        assert!(rename.source.unwrap().ends_with("src/main.rs"));
        assert!(rename.target.unwrap().ends_with("src/app.rs"));
        let create_directory = plan_mutation(
            directory.path(),
            MutationKind::CreateDirectory,
            None,
            Some(Path::new("src/nested")),
            true,
        )
        .unwrap();
        assert!(create_directory.requires_confirmation);
        let copy = plan_mutation(
            directory.path(),
            MutationKind::Copy,
            Some(Path::new("src/main.rs")),
            Some(Path::new("src/copied.rs")),
            true,
        )
        .unwrap();
        assert!(copy.requires_confirmation);
        assert!(
            plan_mutation(
                directory.path(),
                MutationKind::CreateFile,
                Some(Path::new("src/main.rs")),
                Some(Path::new("other")),
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn mutation_targets_never_overwrite_or_case_fold_collide() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("src")).unwrap();
        std::fs::write(directory.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(directory.path().join("src/Already.rs"), "existing\n").unwrap();

        assert!(
            plan_mutation(
                directory.path(),
                MutationKind::CreateFile,
                None,
                Some(Path::new("src/main.rs")),
                true,
            )
            .unwrap_err()
            .to_string()
            .contains("already exists")
        );
        assert!(
            plan_mutation(
                directory.path(),
                MutationKind::Copy,
                Some(Path::new("src/main.rs")),
                Some(Path::new("src/already.rs")),
                true,
            )
            .unwrap_err()
            .to_string()
            .contains("case-fold collision")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_metadata_escape_mutations_are_rejected() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), directory.path().join("escape")).unwrap();
        assert!(
            plan_mutation(
                directory.path(),
                MutationKind::CreateFile,
                None,
                Some(Path::new("escape/stolen")),
                true,
            )
            .is_err()
        );
        assert!(
            plan_mutation(
                directory.path(),
                MutationKind::CreateFile,
                None,
                Some(Path::new(".git/config")),
                true,
            )
            .is_err()
        );
        assert!(
            plan_mutation(
                directory.path(),
                MutationKind::CreateFile,
                None,
                Some(Path::new("../outside")),
                true,
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_fifo_sources_are_rejected() {
        use nix::{sys::stat::Mode, unistd::mkfifo};
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("real.txt"), "real\n").unwrap();
        symlink("real.txt", directory.path().join("alias.txt")).unwrap();
        mkfifo(
            &directory.path().join("pipe"),
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .unwrap();

        for source in ["alias.txt", "pipe"] {
            assert!(
                plan_mutation(
                    directory.path(),
                    MutationKind::Delete,
                    Some(Path::new(source)),
                    None,
                    true,
                )
                .is_err(),
                "special source {source} must be rejected"
            );
        }
    }
}
