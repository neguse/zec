//! Deterministic cell layout for a [`WorkspaceModel`].

use std::collections::BTreeMap;

use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    widgets::Widget,
};

use crate::workspace_model::{DockPosition, LayoutNode, PaneId, SplitAxis, WorkspaceModel};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PaneRenderArea {
    pub(crate) pane: PaneId,
    pub(crate) area: Rect,
    pub(crate) focused: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceResizeHandle {
    Split {
        axis: SplitAxis,
        first_pane: PaneId,
        second_pane: PaneId,
        area: Rect,
        container: Rect,
    },
    Dock {
        position: DockPosition,
        area: Rect,
        container: Rect,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceResizeUpdate {
    Split {
        first_pane: PaneId,
        second_pane: PaneId,
        ratio_millis: u16,
    },
    Dock {
        position: DockPosition,
        size_cells: u16,
    },
}

impl WorkspaceResizeHandle {
    pub(crate) fn area(self) -> Rect {
        match self {
            Self::Split { area, .. } | Self::Dock { area, .. } => area,
        }
    }

    fn container(self) -> Rect {
        match self {
            Self::Split { container, .. } | Self::Dock { container, .. } => container,
        }
    }

    pub(crate) fn contains(self, position: Position) -> bool {
        self.area().contains(position)
    }

    pub(crate) fn update_at(self, pointer: Position) -> WorkspaceResizeUpdate {
        match self {
            Self::Split {
                axis,
                first_pane,
                second_pane,
                container,
                ..
            } => {
                let (coordinate, origin, extent) = match axis {
                    SplitAxis::Horizontal => (pointer.x, container.x, container.width),
                    SplitAxis::Vertical => (pointer.y, container.y, container.height),
                };
                let usable = extent.saturating_sub(1).max(2);
                let first_extent = coordinate
                    .saturating_sub(origin)
                    .clamp(1, usable.saturating_sub(1));
                let ratio_millis = ((u32::from(first_extent) * 1000 + u32::from(usable) / 2)
                    / u32::from(usable))
                .clamp(100, 900) as u16;
                WorkspaceResizeUpdate::Split {
                    first_pane,
                    second_pane,
                    ratio_millis,
                }
            }
            Self::Dock {
                position,
                container,
                ..
            } => {
                let size_cells = match position {
                    DockPosition::Left => pointer
                        .x
                        .saturating_sub(container.x)
                        .saturating_add(1)
                        .min(container.width.saturating_sub(1).max(1)),
                    DockPosition::Right => container
                        .x
                        .saturating_add(container.width)
                        .saturating_sub(pointer.x)
                        .clamp(1, container.width.saturating_sub(1).max(1)),
                    DockPosition::Bottom => container
                        .y
                        .saturating_add(container.height)
                        .saturating_sub(pointer.y)
                        .clamp(1, container.height.saturating_sub(1).max(1)),
                };
                WorkspaceResizeUpdate::Dock {
                    position,
                    size_cells,
                }
            }
        }
    }

    fn is_vertical(self) -> bool {
        matches!(
            self,
            Self::Split {
                axis: SplitAxis::Horizontal,
                ..
            } | Self::Dock {
                position: DockPosition::Left | DockPosition::Right,
                ..
            }
        )
    }
}

impl Widget for &WorkspaceResizeHandle {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let symbol = if self.is_vertical() { "│" } else { "─" };
        let style = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD);
        for y in area.y..area.y.saturating_add(area.height) {
            for x in area.x..area.x.saturating_add(area.width) {
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.set_symbol(symbol).set_style(style);
                }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceRenderPlan {
    pub(crate) panes: Vec<PaneRenderArea>,
    pub(crate) docks: BTreeMap<DockPosition, Rect>,
    pub(crate) resize_handles: Vec<WorkspaceResizeHandle>,
    pub(crate) hidden_docks: Vec<DockPosition>,
    pub(crate) editor_area: Rect,
}

impl WorkspaceRenderPlan {
    pub(crate) fn resize_handle_at(&self, position: Position) -> Option<WorkspaceResizeHandle> {
        self.resize_handles
            .iter()
            .copied()
            .filter(|handle| handle.contains(position))
            .min_by_key(|handle| {
                let area = handle.container();
                u32::from(area.width) * u32::from(area.height)
            })
    }
}

pub(crate) fn render_plan(workspace: &WorkspaceModel, area: Rect) -> WorkspaceRenderPlan {
    if area.is_empty() {
        return WorkspaceRenderPlan {
            panes: Vec::new(),
            docks: BTreeMap::new(),
            resize_handles: Vec::new(),
            hidden_docks: workspace
                .docks
                .iter()
                .filter_map(|(position, dock)| dock.visible.then_some(*position))
                .collect(),
            editor_area: area,
        };
    }

    let mut editor_area = area;
    let mut docks = BTreeMap::new();
    let mut resize_handles = Vec::new();
    let mut hidden_docks = Vec::new();

    reserve_bottom_dock(
        workspace,
        area,
        &mut editor_area,
        &mut docks,
        &mut resize_handles,
        &mut hidden_docks,
    );
    reserve_side_dock(
        workspace,
        DockPosition::Left,
        area,
        &mut editor_area,
        &mut docks,
        &mut resize_handles,
        &mut hidden_docks,
    );
    reserve_side_dock(
        workspace,
        DockPosition::Right,
        area,
        &mut editor_area,
        &mut docks,
        &mut resize_handles,
        &mut hidden_docks,
    );

    let mut panes = Vec::new();
    project_layout(
        &workspace.root,
        editor_area,
        workspace.active_pane,
        &mut panes,
        &mut resize_handles,
    );
    panes.sort_by_key(|pane| pane.pane);
    WorkspaceRenderPlan {
        panes,
        docks,
        resize_handles,
        hidden_docks,
        editor_area,
    }
}

fn reserve_bottom_dock(
    workspace: &WorkspaceModel,
    workspace_area: Rect,
    editor_area: &mut Rect,
    docks: &mut BTreeMap<DockPosition, Rect>,
    resize_handles: &mut Vec<WorkspaceResizeHandle>,
    hidden: &mut Vec<DockPosition>,
) {
    let position = DockPosition::Bottom;
    let dock = &workspace.docks[&position];
    if !dock.visible {
        return;
    }
    let available = editor_area.height.saturating_sub(1);
    let height = dock.size_cells.min(available);
    if height == 0 {
        hidden.push(position);
        return;
    }
    let y = editor_area.y.saturating_add(editor_area.height - height);
    let area = Rect {
        x: editor_area.x,
        y,
        width: editor_area.width,
        height,
    };
    docks.insert(position, area);
    resize_handles.push(WorkspaceResizeHandle::Dock {
        position,
        area: Rect::new(area.x, area.y, area.width, 1),
        container: workspace_area,
    });
    editor_area.height -= height;
}

fn reserve_side_dock(
    workspace: &WorkspaceModel,
    position: DockPosition,
    workspace_area: Rect,
    editor_area: &mut Rect,
    docks: &mut BTreeMap<DockPosition, Rect>,
    resize_handles: &mut Vec<WorkspaceResizeHandle>,
    hidden: &mut Vec<DockPosition>,
) {
    let dock = &workspace.docks[&position];
    if !dock.visible {
        return;
    }
    let available = editor_area.width.saturating_sub(1);
    let width = dock.size_cells.min(available);
    if width == 0 {
        hidden.push(position);
        return;
    }
    let x = if position == DockPosition::Left {
        editor_area.x
    } else {
        editor_area.x.saturating_add(editor_area.width - width)
    };
    let area = Rect {
        x,
        y: editor_area.y,
        width,
        height: editor_area.height,
    };
    docks.insert(position, area);
    let handle_x = if position == DockPosition::Left {
        area.x.saturating_add(area.width.saturating_sub(1))
    } else {
        area.x
    };
    resize_handles.push(WorkspaceResizeHandle::Dock {
        position,
        area: Rect::new(handle_x, area.y, 1, area.height),
        container: workspace_area,
    });
    if position == DockPosition::Left {
        editor_area.x = editor_area.x.saturating_add(width);
    }
    editor_area.width -= width;
}

fn project_layout(
    node: &LayoutNode,
    area: Rect,
    active_pane: PaneId,
    output: &mut Vec<PaneRenderArea>,
    resize_handles: &mut Vec<WorkspaceResizeHandle>,
) {
    match node {
        LayoutNode::Pane { pane } => {
            if !area.is_empty() {
                output.push(PaneRenderArea {
                    pane: *pane,
                    area,
                    focused: *pane == active_pane,
                });
            }
        }
        LayoutNode::Split {
            axis,
            ratio_millis,
            first,
            second,
        } => {
            let extent = match axis {
                crate::workspace_model::SplitAxis::Horizontal => area.width,
                crate::workspace_model::SplitAxis::Vertical => area.height,
            };
            if extent < 3 {
                let visible = if contains_pane(first, active_pane) {
                    first
                } else {
                    second
                };
                project_layout(visible, area, active_pane, output, resize_handles);
                return;
            }
            let usable = extent - 1;
            let first_extent = ((u32::from(usable) * u32::from(*ratio_millis)) / 1000)
                .clamp(1, u32::from(usable - 1)) as u16;
            let second_extent = usable - first_extent;
            match axis {
                crate::workspace_model::SplitAxis::Horizontal => {
                    project_layout(
                        first,
                        Rect {
                            width: first_extent,
                            ..area
                        },
                        active_pane,
                        output,
                        resize_handles,
                    );
                    let handle = Rect {
                        x: area.x.saturating_add(first_extent),
                        width: 1,
                        ..area
                    };
                    resize_handles.push(WorkspaceResizeHandle::Split {
                        axis: *axis,
                        first_pane: representative_pane(first),
                        second_pane: representative_pane(second),
                        area: handle,
                        container: area,
                    });
                    project_layout(
                        second,
                        Rect {
                            x: area.x.saturating_add(first_extent).saturating_add(1),
                            width: second_extent,
                            ..area
                        },
                        active_pane,
                        output,
                        resize_handles,
                    );
                }
                crate::workspace_model::SplitAxis::Vertical => {
                    project_layout(
                        first,
                        Rect {
                            height: first_extent,
                            ..area
                        },
                        active_pane,
                        output,
                        resize_handles,
                    );
                    let handle = Rect {
                        y: area.y.saturating_add(first_extent),
                        height: 1,
                        ..area
                    };
                    resize_handles.push(WorkspaceResizeHandle::Split {
                        axis: *axis,
                        first_pane: representative_pane(first),
                        second_pane: representative_pane(second),
                        area: handle,
                        container: area,
                    });
                    project_layout(
                        second,
                        Rect {
                            y: area.y.saturating_add(first_extent).saturating_add(1),
                            height: second_extent,
                            ..area
                        },
                        active_pane,
                        output,
                        resize_handles,
                    );
                }
            }
        }
    }
}

fn representative_pane(node: &LayoutNode) -> PaneId {
    match node {
        LayoutNode::Pane { pane } => *pane,
        LayoutNode::Split { first, .. } => representative_pane(first),
    }
}

fn contains_pane(node: &LayoutNode, pane: PaneId) -> bool {
    match node {
        LayoutNode::Pane { pane: candidate } => *candidate == pane,
        LayoutNode::Split { first, second, .. } => {
            contains_pane(first, pane) || contains_pane(second, pane)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_model::{DockPosition, ItemId, PanelKind, SplitAxis, WorkspaceModel};

    fn overlaps(left: Rect, right: Rect) -> bool {
        left.x < right.x.saturating_add(right.width)
            && right.x < left.x.saturating_add(left.width)
            && left.y < right.y.saturating_add(right.height)
            && right.y < left.y.saturating_add(left.height)
    }

    #[test]
    fn nested_splits_cover_editor_area_without_overlap() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        workspace
            .split_active(SplitAxis::Horizontal, ItemId(2), true)
            .unwrap();
        workspace
            .split_active(SplitAxis::Vertical, ItemId(3), true)
            .unwrap();
        let plan = render_plan(&workspace, Rect::new(0, 0, 120, 40));
        assert_eq!(plan.panes.len(), 3);
        assert_eq!(plan.resize_handles.len(), 2);
        assert_eq!(plan.editor_area, Rect::new(0, 0, 120, 40));
        let split_handles = plan
            .resize_handles
            .iter()
            .filter(|handle| matches!(handle, WorkspaceResizeHandle::Split { .. }))
            .map(|handle| handle.area())
            .collect::<Vec<_>>();
        assert_eq!(
            plan.panes
                .iter()
                .map(|pane| u32::from(pane.area.width) * u32::from(pane.area.height))
                .chain(
                    split_handles
                        .iter()
                        .map(|area| u32::from(area.width) * u32::from(area.height)),
                )
                .sum::<u32>(),
            120 * 40
        );
        let occupied = plan
            .panes
            .iter()
            .map(|pane| pane.area)
            .chain(split_handles)
            .collect::<Vec<_>>();
        for (index, area) in occupied.iter().enumerate() {
            assert!(
                occupied[index + 1..]
                    .iter()
                    .all(|other| !overlaps(*area, *other))
            );
        }
        let root_handle = plan
            .resize_handle_at(Position::new(59, 5))
            .expect("root split handle");
        assert_eq!(
            root_handle.update_at(Position::new(89, 5)),
            WorkspaceResizeUpdate::Split {
                first_pane: PaneId(1),
                second_pane: PaneId(2),
                ratio_millis: 748,
            }
        );
        assert_eq!(plan.panes.iter().filter(|pane| pane.focused).count(), 1);
    }

    #[test]
    fn visible_docks_reserve_disjoint_cells_and_keep_an_editor_cell() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        workspace
            .show_panel(DockPosition::Left, PanelKind::Project)
            .unwrap();
        workspace
            .show_panel(DockPosition::Right, PanelKind::Outline)
            .unwrap();
        workspace
            .show_panel(DockPosition::Bottom, PanelKind::Diagnostics)
            .unwrap();
        workspace.resize_dock(DockPosition::Left, 40).unwrap();
        workspace.resize_dock(DockPosition::Right, 40).unwrap();
        workspace.resize_dock(DockPosition::Bottom, 20).unwrap();
        let plan = render_plan(&workspace, Rect::new(4, 3, 80, 30));
        assert_eq!(plan.docks.len(), 3);
        assert_eq!(plan.resize_handles.len(), 3);
        assert_eq!(plan.editor_area.width, 1);
        assert_eq!(plan.editor_area.height, 10);
        assert_eq!(plan.panes[0].area, plan.editor_area);
        let all = plan
            .docks
            .values()
            .copied()
            .chain(plan.panes.iter().map(|pane| pane.area))
            .collect::<Vec<_>>();
        for (index, area) in all.iter().enumerate() {
            assert!(
                all[index + 1..]
                    .iter()
                    .all(|other| !overlaps(*area, *other))
            );
        }
        let left_handle = plan
            .resize_handle_at(Position::new(43, 5))
            .expect("left dock handle");
        assert_eq!(
            left_handle.update_at(Position::new(53, 5)),
            WorkspaceResizeUpdate::Dock {
                position: DockPosition::Left,
                size_cells: 50,
            }
        );
    }

    #[test]
    fn tiny_terminals_show_only_active_pane_and_report_hidden_docks() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        workspace
            .split_active(SplitAxis::Horizontal, ItemId(2), true)
            .unwrap();
        workspace
            .show_panel(DockPosition::Left, PanelKind::Project)
            .unwrap();
        workspace.focus_pane(PaneId(2)).unwrap();
        let plan = render_plan(&workspace, Rect::new(0, 0, 1, 1));
        assert_eq!(plan.panes.len(), 1);
        assert_eq!(plan.panes[0].pane, PaneId(2));
        assert_eq!(plan.panes[0].area, Rect::new(0, 0, 1, 1));
        assert_eq!(plan.hidden_docks, [DockPosition::Left]);
    }

    #[test]
    fn zero_area_has_no_panes_and_preserves_visible_dock_diagnostics() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        workspace
            .show_panel(DockPosition::Bottom, PanelKind::Diagnostics)
            .unwrap();
        let plan = render_plan(&workspace, Rect::default());
        assert!(plan.panes.is_empty());
        assert!(plan.docks.is_empty());
        assert_eq!(plan.hidden_docks, [DockPosition::Bottom]);
    }
}
