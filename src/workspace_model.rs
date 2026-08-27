//! Presentation-neutral terminal workspace layout and focus authority.
//!
//! Zed owns editor buffers, selections, transactions, and project services.
//! This model owns only the terminal projection that Zed's GUI workspace would
//! normally provide: panes, tab placement, docks, overlays, and focus.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use serde::{Deserialize, Serialize};

const MIN_SPLIT_RATIO_MILLIS: u16 = 100;
const MAX_SPLIT_RATIO_MILLIS: u16 = 900;
const DEFAULT_SPLIT_RATIO_MILLIS: u16 = 500;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct ItemId(pub(crate) u64);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct PaneId(pub(crate) u64);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SplitAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DockPosition {
    Left,
    Right,
    Bottom,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PanelKind {
    Project,
    Outline,
    Diagnostics,
    Git,
    Terminal,
    Debugger,
    Agent,
    Collaboration,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OverlayKind {
    CommandPalette,
    Extensions,
    Themes,
    Tasks,
    DebugConfigurations,
    DebugRepl,
    QuickOpen,
    ProjectSearch,
    ProjectPanel,
    OutlinePanel,
    BufferSearch,
    Completion,
    Hover,
    Diagnostics,
    Locations,
    Rename,
    CodeActions,
    InlineAssistant,
    SaveAs,
    OpenFile,
    GoToLine,
    Trust,
    Confirmation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub(crate) enum FocusTarget {
    Pane(PaneId),
    Dock(DockPosition),
    Overlay(u64),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OpenDisposition {
    Active,
    Preview,
    Pinned,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PaneState {
    pub(crate) id: PaneId,
    pub(crate) items: Vec<ItemId>,
    pub(crate) active: usize,
    pub(crate) preview: Option<ItemId>,
    pub(crate) pinned: BTreeSet<ItemId>,
}

impl PaneState {
    pub(crate) fn active_item(&self) -> ItemId {
        self.items[self.active]
    }

    fn focus_item(&mut self, item: ItemId) -> Result<(), WorkspaceError> {
        self.active = self
            .items
            .iter()
            .position(|candidate| *candidate == item)
            .ok_or(WorkspaceError::UnknownItem(item))?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LayoutNode {
    Pane {
        pane: PaneId,
    },
    Split {
        axis: SplitAxis,
        ratio_millis: u16,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DockState {
    pub(crate) visible: bool,
    pub(crate) size_cells: u16,
    pub(crate) active_panel: Option<PanelKind>,
    pub(crate) panels: Vec<PanelKind>,
}

impl DockState {
    fn new(size_cells: u16) -> Self {
        Self {
            visible: false,
            size_cells,
            active_panel: None,
            panels: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct OverlayEntry {
    pub(crate) id: u64,
    pub(crate) kind: OverlayKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct WorkspaceModel {
    pub(crate) root: LayoutNode,
    pub(crate) panes: BTreeMap<PaneId, PaneState>,
    pub(crate) active_pane: PaneId,
    pub(crate) docks: BTreeMap<DockPosition, DockState>,
    pub(crate) overlays: Vec<OverlayEntry>,
    pub(crate) focus: FocusTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    focus_before_overlays: Option<FocusTarget>,
    next_pane_id: u64,
    next_overlay_id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CloseOutcome {
    pub(crate) item: ItemId,
    pub(crate) removed_pane: Option<PaneId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OpenOutcome {
    pub(crate) pane: PaneId,
    pub(crate) replaced_preview: Option<ItemId>,
    pub(crate) already_open: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceError {
    DuplicateItem(ItemId),
    EmptyPane(PaneId),
    InvalidActiveIndex(PaneId),
    InvalidFocus,
    InvalidOverlaySequence,
    InvalidPaneSequence,
    InvalidSplitRatio(u16),
    MissingLayoutPane(PaneId),
    RepeatedLayoutPane(PaneId),
    UnknownDock(DockPosition),
    UnknownItem(ItemId),
    UnknownPane(PaneId),
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateItem(item) => write!(formatter, "item {item:?} occurs more than once"),
            Self::EmptyPane(pane) => write!(formatter, "pane {pane:?} has no items"),
            Self::InvalidActiveIndex(pane) => {
                write!(formatter, "pane {pane:?} has an invalid active index")
            }
            Self::InvalidFocus => write!(formatter, "workspace focus target is invalid"),
            Self::InvalidOverlaySequence => {
                write!(formatter, "workspace overlay ID sequence is not monotonic")
            }
            Self::InvalidPaneSequence => {
                write!(formatter, "workspace pane ID sequence is not monotonic")
            }
            Self::InvalidSplitRatio(ratio) => write!(formatter, "invalid split ratio {ratio}"),
            Self::MissingLayoutPane(pane) => {
                write!(formatter, "pane {pane:?} is absent from the layout tree")
            }
            Self::RepeatedLayoutPane(pane) => {
                write!(formatter, "pane {pane:?} occurs twice in the layout tree")
            }
            Self::UnknownDock(position) => write!(formatter, "unknown dock {position:?}"),
            Self::UnknownItem(item) => write!(formatter, "unknown item {item:?}"),
            Self::UnknownPane(pane) => write!(formatter, "unknown pane {pane:?}"),
        }
    }
}

impl Error for WorkspaceError {}

impl WorkspaceModel {
    pub(crate) fn new(first_item: ItemId) -> Self {
        let pane = PaneId(1);
        let panes = BTreeMap::from([(
            pane,
            PaneState {
                id: pane,
                items: vec![first_item],
                active: 0,
                preview: None,
                pinned: BTreeSet::from([first_item]),
            },
        )]);
        let docks = BTreeMap::from([
            (DockPosition::Left, DockState::new(30)),
            (DockPosition::Right, DockState::new(30)),
            (DockPosition::Bottom, DockState::new(12)),
        ]);
        Self {
            root: LayoutNode::Pane { pane },
            panes,
            active_pane: pane,
            docks,
            overlays: Vec::new(),
            focus: FocusTarget::Pane(pane),
            focus_before_overlays: None,
            next_pane_id: 2,
            next_overlay_id: 1,
        }
    }

    pub(crate) fn active_pane(&self) -> &PaneState {
        &self.panes[&self.active_pane]
    }

    pub(crate) fn active_item(&self) -> ItemId {
        self.active_pane().active_item()
    }

    pub(crate) fn pane_for_item(&self, item: ItemId) -> Option<PaneId> {
        self.panes
            .iter()
            .find_map(|(pane, state)| state.items.contains(&item).then_some(*pane))
    }

    pub(crate) fn item_ids(&self) -> BTreeSet<ItemId> {
        self.panes
            .values()
            .flat_map(|pane| pane.items.iter().copied())
            .collect()
    }

    pub(crate) fn focus_pane(&mut self, pane: PaneId) -> Result<(), WorkspaceError> {
        if !self.panes.contains_key(&pane) {
            return Err(WorkspaceError::UnknownPane(pane));
        }
        self.active_pane = pane;
        if self.overlays.is_empty() {
            self.focus = FocusTarget::Pane(pane);
        } else if matches!(self.focus_before_overlays, Some(FocusTarget::Pane(_))) {
            self.focus_before_overlays = Some(FocusTarget::Pane(pane));
        }
        Ok(())
    }

    pub(crate) fn focus_item(&mut self, item: ItemId) -> Result<(), WorkspaceError> {
        let pane = self
            .pane_for_item(item)
            .ok_or(WorkspaceError::UnknownItem(item))?;
        self.panes
            .get_mut(&pane)
            .expect("pane found by item must exist")
            .focus_item(item)?;
        self.focus_pane(pane)
    }

    pub(crate) fn open_item(
        &mut self,
        item: ItemId,
        disposition: OpenDisposition,
    ) -> Result<OpenOutcome, WorkspaceError> {
        if let Some(pane) = self.pane_for_item(item) {
            self.focus_item(item)?;
            if disposition == OpenDisposition::Pinned {
                self.pin_item(item)?;
            }
            return Ok(OpenOutcome {
                pane,
                replaced_preview: None,
                already_open: true,
            });
        }

        let pane = self.active_pane;
        let state = self.panes.get_mut(&pane).expect("active pane must exist");
        let replaced_preview = if disposition == OpenDisposition::Preview {
            state.preview.and_then(|preview| {
                (!state.pinned.contains(&preview))
                    .then(|| {
                        state
                            .items
                            .iter()
                            .position(|candidate| *candidate == preview)
                    })
                    .flatten()
                    .map(|index| {
                        state.items[index] = item;
                        state.active = index;
                        preview
                    })
            })
        } else {
            None
        };

        if replaced_preview.is_none() {
            state.items.push(item);
            state.active = state.items.len() - 1;
        }
        match disposition {
            OpenDisposition::Preview => state.preview = Some(item),
            OpenDisposition::Pinned => {
                state.pinned.insert(item);
            }
            OpenDisposition::Active => {}
        }
        self.focus_pane(pane)?;
        Ok(OpenOutcome {
            pane,
            replaced_preview,
            already_open: false,
        })
    }

    pub(crate) fn pin_item(&mut self, item: ItemId) -> Result<(), WorkspaceError> {
        let pane = self
            .pane_for_item(item)
            .ok_or(WorkspaceError::UnknownItem(item))?;
        let state = self
            .panes
            .get_mut(&pane)
            .expect("pane found by item must exist");
        state.pinned.insert(item);
        if state.preview == Some(item) {
            state.preview = None;
        }
        Ok(())
    }

    pub(crate) fn split_active(
        &mut self,
        axis: SplitAxis,
        new_item: ItemId,
        place_after: bool,
    ) -> Result<PaneId, WorkspaceError> {
        if self.pane_for_item(new_item).is_some() {
            return Err(WorkspaceError::DuplicateItem(new_item));
        }
        let original = self.active_pane;
        let pane = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        self.panes.insert(
            pane,
            PaneState {
                id: pane,
                items: vec![new_item],
                active: 0,
                preview: None,
                pinned: BTreeSet::from([new_item]),
            },
        );
        let replacement = if place_after {
            LayoutNode::Split {
                axis,
                ratio_millis: DEFAULT_SPLIT_RATIO_MILLIS,
                first: Box::new(LayoutNode::Pane { pane: original }),
                second: Box::new(LayoutNode::Pane { pane }),
            }
        } else {
            LayoutNode::Split {
                axis,
                ratio_millis: DEFAULT_SPLIT_RATIO_MILLIS,
                first: Box::new(LayoutNode::Pane { pane }),
                second: Box::new(LayoutNode::Pane { pane: original }),
            }
        };
        replace_pane_node(&mut self.root, original, replacement)
            .then_some(())
            .ok_or(WorkspaceError::MissingLayoutPane(original))?;
        self.focus_pane(pane)?;
        self.validate()?;
        Ok(pane)
    }

    pub(crate) fn resize_split_for_pane(
        &mut self,
        pane: PaneId,
        ratio_millis: u16,
    ) -> Result<(), WorkspaceError> {
        if !(MIN_SPLIT_RATIO_MILLIS..=MAX_SPLIT_RATIO_MILLIS).contains(&ratio_millis) {
            return Err(WorkspaceError::InvalidSplitRatio(ratio_millis));
        }
        if !set_parent_split_ratio(&mut self.root, pane, ratio_millis) {
            return Err(WorkspaceError::MissingLayoutPane(pane));
        }
        Ok(())
    }

    pub(crate) fn resize_split_between(
        &mut self,
        first_pane: PaneId,
        second_pane: PaneId,
        ratio_millis: u16,
    ) -> Result<(), WorkspaceError> {
        if !(MIN_SPLIT_RATIO_MILLIS..=MAX_SPLIT_RATIO_MILLIS).contains(&ratio_millis) {
            return Err(WorkspaceError::InvalidSplitRatio(ratio_millis));
        }
        if !self.panes.contains_key(&first_pane) {
            return Err(WorkspaceError::UnknownPane(first_pane));
        }
        if !self.panes.contains_key(&second_pane) {
            return Err(WorkspaceError::UnknownPane(second_pane));
        }
        if !set_split_ratio_between(&mut self.root, first_pane, second_pane, ratio_millis) {
            return Err(WorkspaceError::MissingLayoutPane(first_pane));
        }
        self.validate()
    }

    /// Changes the nearest split containing `pane` by an amount expressed as
    /// the active pane's share. A positive delta always grows the active side,
    /// regardless of whether it is the first or second child in the tree.
    pub(crate) fn adjust_split_for_pane(
        &mut self,
        pane: PaneId,
        delta_millis: i16,
    ) -> Result<u16, WorkspaceError> {
        if !self.panes.contains_key(&pane) {
            return Err(WorkspaceError::UnknownPane(pane));
        }
        let ratio = adjust_parent_split_ratio(&mut self.root, pane, delta_millis)
            .ok_or(WorkspaceError::MissingLayoutPane(pane))?;
        self.validate()?;
        Ok(ratio)
    }

    pub(crate) fn activate_adjacent_item(&mut self, forward: bool) -> bool {
        let pane = self.active_pane;
        let state = self.panes.get_mut(&pane).expect("active pane must exist");
        if state.items.len() < 2 {
            return false;
        }
        state.active = if forward {
            (state.active + 1) % state.items.len()
        } else if state.active == 0 {
            state.items.len() - 1
        } else {
            state.active - 1
        };
        self.focus = FocusTarget::Pane(pane);
        true
    }

    pub(crate) fn reorder_active_item(&mut self, toward_end: bool) -> bool {
        let pane = self.active_pane;
        let state = self.panes.get_mut(&pane).expect("active pane must exist");
        if state.items.len() < 2 {
            return false;
        }
        let target = if toward_end {
            (state.active + 1) % state.items.len()
        } else if state.active == 0 {
            state.items.len() - 1
        } else {
            state.active - 1
        };
        state.items.swap(state.active, target);
        state.active = target;
        self.validate()
            .expect("tab reordering must preserve workspace invariants");
        true
    }

    pub(crate) fn focus_direction(&mut self, direction: Direction) -> bool {
        let Some(target) = adjacent_pane(&self.root, self.active_pane, direction) else {
            return false;
        };
        self.focus_pane(target)
            .expect("layout adjacency must reference a known pane");
        true
    }

    pub(crate) fn move_active_item(&mut self, direction: Direction) -> bool {
        let source = self.active_pane;
        let Some(target) = adjacent_pane(&self.root, source, direction) else {
            return false;
        };
        let item = self.panes[&source].active_item();
        {
            let source_state = self.panes.get_mut(&source).expect("active pane must exist");
            let index = source_state.active;
            source_state.items.remove(index);
            source_state.pinned.remove(&item);
            if source_state.preview == Some(item) {
                source_state.preview = None;
            }
            if !source_state.items.is_empty() {
                source_state.active = index.min(source_state.items.len() - 1);
            }
        }
        {
            let target_state = self
                .panes
                .get_mut(&target)
                .expect("adjacent pane must exist");
            target_state.items.push(item);
            target_state.active = target_state.items.len() - 1;
            target_state.pinned.insert(item);
        }
        if self.panes[&source].items.is_empty() {
            self.panes.remove(&source);
            remove_pane_node(&mut self.root, source);
        }
        self.focus_pane(target)
            .expect("target pane remains after moving an item");
        self.validate()
            .expect("moving an item must preserve workspace invariants");
        true
    }

    pub(crate) fn close_item(&mut self, item: ItemId) -> Result<CloseOutcome, WorkspaceError> {
        let pane = self
            .pane_for_item(item)
            .ok_or(WorkspaceError::UnknownItem(item))?;
        if self.panes.len() == 1 && self.panes[&pane].items.len() == 1 {
            return Err(WorkspaceError::EmptyPane(pane));
        }
        let removed_pane = {
            let state = self
                .panes
                .get_mut(&pane)
                .expect("pane found by item must exist");
            let index = state
                .items
                .iter()
                .position(|candidate| *candidate == item)
                .expect("item lookup must remain stable");
            state.items.remove(index);
            state.pinned.remove(&item);
            if state.preview == Some(item) {
                state.preview = None;
            }
            if state.items.is_empty() {
                Some(pane)
            } else {
                state.active = index.min(state.items.len() - 1);
                None
            }
        };

        if let Some(removed) = removed_pane {
            self.panes.remove(&removed);
            remove_pane_node(&mut self.root, removed);
            let next = first_pane(&self.root);
            self.focus_pane(next)
                .expect("a non-final pane close must leave another pane");
        }
        self.validate()?;
        Ok(CloseOutcome { item, removed_pane })
    }

    pub(crate) fn reorder_item(
        &mut self,
        pane: PaneId,
        from: usize,
        to: usize,
    ) -> Result<(), WorkspaceError> {
        let state = self
            .panes
            .get_mut(&pane)
            .ok_or(WorkspaceError::UnknownPane(pane))?;
        if from >= state.items.len() || to >= state.items.len() {
            return Err(WorkspaceError::InvalidActiveIndex(pane));
        }
        let active_item = state.active_item();
        let item = state.items.remove(from);
        state.items.insert(to, item);
        state.focus_item(active_item)
    }

    pub(crate) fn show_panel(
        &mut self,
        position: DockPosition,
        panel: PanelKind,
    ) -> Result<(), WorkspaceError> {
        let dock = self
            .docks
            .get_mut(&position)
            .ok_or(WorkspaceError::UnknownDock(position))?;
        if !dock.panels.contains(&panel) {
            dock.panels.push(panel);
        }
        dock.visible = true;
        dock.active_panel = Some(panel);
        self.focus = FocusTarget::Dock(position);
        Ok(())
    }

    pub(crate) fn toggle_dock(&mut self, position: DockPosition) -> Result<bool, WorkspaceError> {
        let dock = self
            .docks
            .get_mut(&position)
            .ok_or(WorkspaceError::UnknownDock(position))?;
        dock.visible = !dock.visible;
        if dock.visible {
            self.focus = FocusTarget::Dock(position);
        } else if self.focus == FocusTarget::Dock(position) {
            self.focus = FocusTarget::Pane(self.active_pane);
        }
        Ok(dock.visible)
    }

    pub(crate) fn resize_dock(
        &mut self,
        position: DockPosition,
        size_cells: u16,
    ) -> Result<(), WorkspaceError> {
        let dock = self
            .docks
            .get_mut(&position)
            .ok_or(WorkspaceError::UnknownDock(position))?;
        dock.size_cells = size_cells.max(1);
        Ok(())
    }

    pub(crate) fn push_overlay(&mut self, kind: OverlayKind) -> u64 {
        if self.overlays.is_empty() {
            self.focus_before_overlays = Some(self.focus);
        }
        let id = self.next_overlay_id;
        self.next_overlay_id += 1;
        self.overlays.push(OverlayEntry { id, kind });
        self.focus = FocusTarget::Overlay(id);
        id
    }

    pub(crate) fn pop_overlay(&mut self) -> Option<OverlayEntry> {
        let entry = self.overlays.pop()?;
        self.focus = if let Some(overlay) = self.overlays.last() {
            FocusTarget::Overlay(overlay.id)
        } else {
            self.focus_before_overlays
                .take()
                .unwrap_or(FocusTarget::Pane(self.active_pane))
        };
        Some(entry)
    }

    pub(crate) fn validate(&self) -> Result<(), WorkspaceError> {
        let mut layout_panes = BTreeSet::new();
        collect_layout_panes(&self.root, &mut layout_panes)?;
        for pane in self.panes.keys() {
            if !layout_panes.contains(pane) {
                return Err(WorkspaceError::MissingLayoutPane(*pane));
            }
        }
        if layout_panes.len() != self.panes.len() {
            let unknown = layout_panes
                .into_iter()
                .find(|pane| !self.panes.contains_key(pane))
                .expect("different pane sets must have an unknown layout pane");
            return Err(WorkspaceError::UnknownPane(unknown));
        }
        if !self.panes.contains_key(&self.active_pane) {
            return Err(WorkspaceError::UnknownPane(self.active_pane));
        }
        if self.panes.keys().any(|pane| pane.0 >= self.next_pane_id) {
            return Err(WorkspaceError::InvalidPaneSequence);
        }
        if self
            .overlays
            .iter()
            .any(|overlay| overlay.id >= self.next_overlay_id)
        {
            return Err(WorkspaceError::InvalidOverlaySequence);
        }
        if self.overlays.is_empty() != self.focus_before_overlays.is_none() {
            return Err(WorkspaceError::InvalidFocus);
        }

        let mut all_items = BTreeSet::new();
        for (pane, state) in &self.panes {
            if state.items.is_empty() {
                return Err(WorkspaceError::EmptyPane(*pane));
            }
            if state.active >= state.items.len() {
                return Err(WorkspaceError::InvalidActiveIndex(*pane));
            }
            for item in &state.items {
                if !all_items.insert(*item) {
                    return Err(WorkspaceError::DuplicateItem(*item));
                }
            }
            if state
                .preview
                .is_some_and(|item| !state.items.contains(&item))
                || state.pinned.iter().any(|item| !state.items.contains(item))
            {
                return Err(WorkspaceError::InvalidActiveIndex(*pane));
            }
        }

        let focus_is_valid = match self.focus {
            FocusTarget::Pane(pane) => self.panes.contains_key(&pane),
            FocusTarget::Dock(position) => {
                self.docks.get(&position).is_some_and(|dock| dock.visible)
            }
            FocusTarget::Overlay(id) => self.overlays.last().is_some_and(|entry| entry.id == id),
        };
        if !focus_is_valid {
            return Err(WorkspaceError::InvalidFocus);
        }
        if let Some(focus) = self.focus_before_overlays {
            let underlying_focus_is_valid = match focus {
                FocusTarget::Pane(pane) => self.panes.contains_key(&pane),
                FocusTarget::Dock(position) => {
                    self.docks.get(&position).is_some_and(|dock| dock.visible)
                }
                FocusTarget::Overlay(_) => false,
            };
            if !underlying_focus_is_valid {
                return Err(WorkspaceError::InvalidFocus);
            }
        }
        Ok(())
    }
}

fn replace_pane_node(root: &mut LayoutNode, pane: PaneId, replacement: LayoutNode) -> bool {
    match root {
        LayoutNode::Pane { pane: candidate } if *candidate == pane => {
            *root = replacement;
            true
        }
        LayoutNode::Pane { .. } => false,
        LayoutNode::Split { first, second, .. } => {
            replace_pane_node(first, pane, replacement.clone())
                || replace_pane_node(second, pane, replacement)
        }
    }
}

fn remove_pane_node(root: &mut LayoutNode, pane: PaneId) -> bool {
    let LayoutNode::Split { first, second, .. } = root else {
        return false;
    };
    if matches!(first.as_ref(), LayoutNode::Pane { pane: candidate } if *candidate == pane) {
        *root = (**second).clone();
        return true;
    }
    if matches!(second.as_ref(), LayoutNode::Pane { pane: candidate } if *candidate == pane) {
        *root = (**first).clone();
        return true;
    }
    remove_pane_node(first, pane) || remove_pane_node(second, pane)
}

fn set_parent_split_ratio(root: &mut LayoutNode, pane: PaneId, ratio_millis: u16) -> bool {
    let LayoutNode::Split {
        ratio_millis: ratio,
        first,
        second,
        ..
    } = root
    else {
        return false;
    };
    if matches!(first.as_ref(), LayoutNode::Pane { pane: candidate } if *candidate == pane)
        || matches!(second.as_ref(), LayoutNode::Pane { pane: candidate } if *candidate == pane)
    {
        *ratio = ratio_millis;
        true
    } else {
        set_parent_split_ratio(first, pane, ratio_millis)
            || set_parent_split_ratio(second, pane, ratio_millis)
    }
}

fn set_split_ratio_between(
    root: &mut LayoutNode,
    first_pane: PaneId,
    second_pane: PaneId,
    ratio_millis: u16,
) -> bool {
    let LayoutNode::Split {
        ratio_millis: ratio,
        first,
        second,
        ..
    } = root
    else {
        return false;
    };

    if contains_layout_pane(first, first_pane) && contains_layout_pane(second, second_pane) {
        *ratio = ratio_millis;
        return true;
    }
    if contains_layout_pane(first, second_pane) && contains_layout_pane(second, first_pane) {
        *ratio = 1000u16.saturating_sub(ratio_millis);
        return true;
    }
    set_split_ratio_between(first, first_pane, second_pane, ratio_millis)
        || set_split_ratio_between(second, first_pane, second_pane, ratio_millis)
}

fn adjust_parent_split_ratio(
    root: &mut LayoutNode,
    pane: PaneId,
    delta_millis: i16,
) -> Option<u16> {
    let LayoutNode::Split {
        ratio_millis,
        first,
        second,
        ..
    } = root
    else {
        return None;
    };

    if let Some(ratio) = adjust_parent_split_ratio(first, pane, delta_millis) {
        return Some(ratio);
    }
    if let Some(ratio) = adjust_parent_split_ratio(second, pane, delta_millis) {
        return Some(ratio);
    }

    let signed_delta = if contains_layout_pane(first, pane) {
        i32::from(delta_millis)
    } else if contains_layout_pane(second, pane) {
        -i32::from(delta_millis)
    } else {
        return None;
    };
    let adjusted = (i32::from(*ratio_millis) + signed_delta).clamp(
        i32::from(MIN_SPLIT_RATIO_MILLIS),
        i32::from(MAX_SPLIT_RATIO_MILLIS),
    ) as u16;
    *ratio_millis = adjusted;
    Some(adjusted)
}

fn contains_layout_pane(node: &LayoutNode, pane: PaneId) -> bool {
    match node {
        LayoutNode::Pane { pane: candidate } => *candidate == pane,
        LayoutNode::Split { first, second, .. } => {
            contains_layout_pane(first, pane) || contains_layout_pane(second, pane)
        }
    }
}

fn first_pane(root: &LayoutNode) -> PaneId {
    match root {
        LayoutNode::Pane { pane } => *pane,
        LayoutNode::Split { first, .. } => first_pane(first),
    }
}

fn collect_layout_panes(
    root: &LayoutNode,
    panes: &mut BTreeSet<PaneId>,
) -> Result<(), WorkspaceError> {
    match root {
        LayoutNode::Pane { pane } => {
            if !panes.insert(*pane) {
                return Err(WorkspaceError::RepeatedLayoutPane(*pane));
            }
        }
        LayoutNode::Split { first, second, .. } => {
            collect_layout_panes(first, panes)?;
            collect_layout_panes(second, panes)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct NormalizedRect {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

fn pane_rects(root: &LayoutNode) -> BTreeMap<PaneId, NormalizedRect> {
    fn visit(
        node: &LayoutNode,
        rect: NormalizedRect,
        output: &mut BTreeMap<PaneId, NormalizedRect>,
    ) {
        match node {
            LayoutNode::Pane { pane } => {
                output.insert(*pane, rect);
            }
            LayoutNode::Split {
                axis,
                ratio_millis,
                first,
                second,
            } => {
                let ratio = u32::from(*ratio_millis);
                match axis {
                    SplitAxis::Horizontal => {
                        let split = rect.left + (rect.right - rect.left) * ratio / 1000;
                        visit(
                            first,
                            NormalizedRect {
                                right: split,
                                ..rect
                            },
                            output,
                        );
                        visit(
                            second,
                            NormalizedRect {
                                left: split,
                                ..rect
                            },
                            output,
                        );
                    }
                    SplitAxis::Vertical => {
                        let split = rect.top + (rect.bottom - rect.top) * ratio / 1000;
                        visit(
                            first,
                            NormalizedRect {
                                bottom: split,
                                ..rect
                            },
                            output,
                        );
                        visit(second, NormalizedRect { top: split, ..rect }, output);
                    }
                }
            }
        }
    }

    let mut output = BTreeMap::new();
    visit(
        root,
        NormalizedRect {
            left: 0,
            top: 0,
            right: 1_000_000,
            bottom: 1_000_000,
        },
        &mut output,
    );
    output
}

fn adjacent_pane(root: &LayoutNode, current: PaneId, direction: Direction) -> Option<PaneId> {
    let rects = pane_rects(root);
    let current_rect = rects.get(&current)?;
    let current_center_x = u64::from(current_rect.left + current_rect.right);
    let current_center_y = u64::from(current_rect.top + current_rect.bottom);
    rects
        .iter()
        .filter(|(pane, _)| **pane != current)
        .filter_map(|(pane, candidate)| {
            let candidate_center_x = u64::from(candidate.left + candidate.right);
            let candidate_center_y = u64::from(candidate.top + candidate.bottom);
            let (is_directional, primary_gap, overlap, secondary_distance) = match direction {
                Direction::Left => (
                    candidate.right <= current_rect.left,
                    current_rect.left.saturating_sub(candidate.right),
                    interval_overlap(
                        current_rect.top,
                        current_rect.bottom,
                        candidate.top,
                        candidate.bottom,
                    ),
                    current_center_y.abs_diff(candidate_center_y),
                ),
                Direction::Right => (
                    candidate.left >= current_rect.right,
                    candidate.left.saturating_sub(current_rect.right),
                    interval_overlap(
                        current_rect.top,
                        current_rect.bottom,
                        candidate.top,
                        candidate.bottom,
                    ),
                    current_center_y.abs_diff(candidate_center_y),
                ),
                Direction::Up => (
                    candidate.bottom <= current_rect.top,
                    current_rect.top.saturating_sub(candidate.bottom),
                    interval_overlap(
                        current_rect.left,
                        current_rect.right,
                        candidate.left,
                        candidate.right,
                    ),
                    current_center_x.abs_diff(candidate_center_x),
                ),
                Direction::Down => (
                    candidate.top >= current_rect.bottom,
                    candidate.top.saturating_sub(current_rect.bottom),
                    interval_overlap(
                        current_rect.left,
                        current_rect.right,
                        candidate.left,
                        candidate.right,
                    ),
                    current_center_x.abs_diff(candidate_center_x),
                ),
            };
            is_directional.then_some((
                (!overlap).then_some(1).unwrap_or(0),
                primary_gap,
                secondary_distance,
                *pane,
            ))
        })
        .min()
        .map(|(_, _, _, pane)| pane)
}

fn interval_overlap(a_start: u32, a_end: u32, b_start: u32, b_end: u32) -> bool {
    a_start < b_end && b_start < a_end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_focus_and_direction_follow_spatial_layout() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        let right = workspace
            .split_active(SplitAxis::Horizontal, ItemId(2), true)
            .unwrap();
        let bottom_right = workspace
            .split_active(SplitAxis::Vertical, ItemId(3), true)
            .unwrap();
        assert_eq!(workspace.active_pane, bottom_right);
        assert!(workspace.focus_direction(Direction::Up));
        assert_eq!(workspace.active_pane, right);
        assert!(workspace.focus_direction(Direction::Left));
        assert_eq!(workspace.active_item(), ItemId(1));
        assert!(workspace.focus_direction(Direction::Right));
        assert_eq!(workspace.active_item(), ItemId(2));
        assert!(workspace.focus_direction(Direction::Down));
        assert_eq!(workspace.active_item(), ItemId(3));
        workspace.validate().unwrap();
    }

    #[test]
    fn moving_last_item_collapses_source_pane_without_duplication() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        let right = workspace
            .split_active(SplitAxis::Horizontal, ItemId(2), true)
            .unwrap();
        assert_eq!(workspace.active_pane, right);
        assert!(workspace.move_active_item(Direction::Left));
        assert_eq!(workspace.panes.len(), 1);
        assert_eq!(workspace.active_pane().items, [ItemId(1), ItemId(2)]);
        assert_eq!(workspace.active_item(), ItemId(2));
        workspace.validate().unwrap();
    }

    #[test]
    fn adjacent_activation_and_reordering_are_pane_local_and_wrap() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        workspace
            .open_item(ItemId(2), OpenDisposition::Pinned)
            .unwrap();
        workspace
            .open_item(ItemId(3), OpenDisposition::Pinned)
            .unwrap();
        assert_eq!(workspace.active_item(), ItemId(3));
        assert!(workspace.activate_adjacent_item(true));
        assert_eq!(workspace.active_item(), ItemId(1));
        assert!(workspace.reorder_active_item(false));
        assert_eq!(
            workspace.active_pane().items,
            [ItemId(3), ItemId(2), ItemId(1)]
        );
        assert_eq!(workspace.active_item(), ItemId(1));
        workspace.validate().unwrap();
    }

    #[test]
    fn split_adjustment_grows_active_side_and_clamps_at_bounds() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        workspace
            .split_active(SplitAxis::Horizontal, ItemId(2), true)
            .unwrap();
        assert_eq!(workspace.adjust_split_for_pane(PaneId(2), 75).unwrap(), 425);
        assert_eq!(
            workspace.adjust_split_for_pane(PaneId(2), 500).unwrap(),
            100
        );
        workspace.focus_pane(PaneId(1)).unwrap();
        assert_eq!(workspace.adjust_split_for_pane(PaneId(1), 50).unwrap(), 150);
        workspace.validate().unwrap();
    }

    #[test]
    fn split_pair_identity_resizes_the_intended_nested_boundary() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        let right = workspace
            .split_active(SplitAxis::Horizontal, ItemId(2), true)
            .unwrap();
        let bottom_right = workspace
            .split_active(SplitAxis::Vertical, ItemId(3), true)
            .unwrap();

        workspace
            .resize_split_between(PaneId(1), right, 700)
            .unwrap();
        workspace
            .resize_split_between(right, bottom_right, 300)
            .unwrap();

        let LayoutNode::Split {
            ratio_millis: root_ratio,
            second,
            ..
        } = &workspace.root
        else {
            panic!("root split disappeared");
        };
        let LayoutNode::Split {
            ratio_millis: nested_ratio,
            ..
        } = second.as_ref()
        else {
            panic!("nested split disappeared");
        };
        assert_eq!((*root_ratio, *nested_ratio), (700, 300));
        workspace.validate().unwrap();
    }

    #[test]
    fn preview_replacement_pin_and_reopen_preserve_item_identity() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        let first = workspace
            .open_item(ItemId(2), OpenDisposition::Preview)
            .unwrap();
        assert_eq!(first.replaced_preview, None);
        let second = workspace
            .open_item(ItemId(3), OpenDisposition::Preview)
            .unwrap();
        assert_eq!(second.replaced_preview, Some(ItemId(2)));
        workspace.pin_item(ItemId(3)).unwrap();
        let third = workspace
            .open_item(ItemId(4), OpenDisposition::Preview)
            .unwrap();
        assert_eq!(third.replaced_preview, None);
        let reopened = workspace
            .open_item(ItemId(3), OpenDisposition::Active)
            .unwrap();
        assert!(reopened.already_open);
        assert_eq!(workspace.active_item(), ItemId(3));
        workspace.validate().unwrap();
    }

    #[test]
    fn overlays_are_lifo_and_restore_workspace_focus() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        let palette = workspace.push_overlay(OverlayKind::CommandPalette);
        let confirmation = workspace.push_overlay(OverlayKind::Confirmation);
        assert_eq!(workspace.focus, FocusTarget::Overlay(confirmation));
        assert_eq!(
            workspace.pop_overlay().unwrap(),
            OverlayEntry {
                id: confirmation,
                kind: OverlayKind::Confirmation,
            }
        );
        assert_eq!(workspace.focus, FocusTarget::Overlay(palette));
        workspace.pop_overlay();
        assert_eq!(workspace.focus, FocusTarget::Pane(PaneId(1)));
        workspace.validate().unwrap();
    }

    #[test]
    fn docks_keep_panel_identity_size_and_focus_consistent() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        workspace
            .show_panel(DockPosition::Left, PanelKind::Project)
            .unwrap();
        workspace.resize_dock(DockPosition::Left, 42).unwrap();
        let left = &workspace.docks[&DockPosition::Left];
        assert!(left.visible);
        assert_eq!(left.size_cells, 42);
        assert_eq!(left.active_panel, Some(PanelKind::Project));
        assert_eq!(workspace.focus, FocusTarget::Dock(DockPosition::Left));
        assert!(!workspace.toggle_dock(DockPosition::Left).unwrap());
        assert_eq!(workspace.focus, FocusTarget::Pane(PaneId(1)));
        workspace.validate().unwrap();
    }

    #[test]
    fn close_and_reorder_keep_active_item_stable() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        workspace
            .open_item(ItemId(2), OpenDisposition::Pinned)
            .unwrap();
        workspace
            .open_item(ItemId(3), OpenDisposition::Pinned)
            .unwrap();
        workspace.reorder_item(PaneId(1), 0, 2).unwrap();
        assert_eq!(workspace.active_item(), ItemId(3));
        workspace.close_item(ItemId(3)).unwrap();
        assert_eq!(workspace.active_item(), ItemId(1));
        assert_eq!(workspace.active_pane().items, [ItemId(2), ItemId(1)]);
        workspace.validate().unwrap();
    }

    #[test]
    fn validation_rejects_duplicate_items_and_layout_panes() {
        let mut duplicate_item = WorkspaceModel::new(ItemId(1));
        duplicate_item.panes.insert(
            PaneId(2),
            PaneState {
                id: PaneId(2),
                items: vec![ItemId(1)],
                active: 0,
                preview: None,
                pinned: BTreeSet::new(),
            },
        );
        duplicate_item.root = LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio_millis: 500,
            first: Box::new(LayoutNode::Pane { pane: PaneId(1) }),
            second: Box::new(LayoutNode::Pane { pane: PaneId(2) }),
        };
        duplicate_item.next_pane_id = 3;
        assert_eq!(
            duplicate_item.validate(),
            Err(WorkspaceError::DuplicateItem(ItemId(1)))
        );

        let mut duplicate_pane = WorkspaceModel::new(ItemId(1));
        duplicate_pane.root = LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio_millis: 500,
            first: Box::new(LayoutNode::Pane { pane: PaneId(1) }),
            second: Box::new(LayoutNode::Pane { pane: PaneId(1) }),
        };
        assert_eq!(
            duplicate_pane.validate(),
            Err(WorkspaceError::RepeatedLayoutPane(PaneId(1)))
        );
    }

    #[test]
    fn split_and_dock_sizes_are_bounded() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        workspace
            .split_active(SplitAxis::Horizontal, ItemId(2), true)
            .unwrap();
        assert_eq!(
            workspace.resize_split_for_pane(PaneId(2), 99),
            Err(WorkspaceError::InvalidSplitRatio(99))
        );
        workspace.resize_split_for_pane(PaneId(2), 750).unwrap();
        workspace.resize_dock(DockPosition::Bottom, 0).unwrap();
        assert_eq!(workspace.docks[&DockPosition::Bottom].size_cells, 1);

        let bottom_right = workspace
            .split_active(SplitAxis::Vertical, ItemId(3), true)
            .unwrap();
        workspace.resize_split_for_pane(bottom_right, 700).unwrap();
        let LayoutNode::Split {
            ratio_millis: outer_ratio,
            second,
            ..
        } = &workspace.root
        else {
            panic!("expected outer split");
        };
        assert_eq!(*outer_ratio, 750);
        let LayoutNode::Split {
            ratio_millis: inner_ratio,
            ..
        } = second.as_ref()
        else {
            panic!("expected nested split");
        };
        assert_eq!(*inner_ratio, 700);
    }
}
