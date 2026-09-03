//! The render plan: non-overlapping rects for docks, panes, and dividers,
//! derived from the workspace for one frame and kept for mouse hit testing.

use ratatui::layout::{Position, Rect};

use super::workspace::{Axis, Direction, DockPosition, LayoutNode, PaneId, WorkspaceModel};

/// The fewest cells a dock leaves the editor along its axis.
const MIN_EDITOR_EXTENT: u16 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaneArea {
    pub pane: PaneId,
    pub area: Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Divider {
    pub axis: Axis,
    pub area: Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DockArea {
    pub position: DockPosition,
    pub area: Rect,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RenderPlan {
    pub panes: Vec<PaneArea>,
    pub dividers: Vec<Divider>,
    pub docks: Vec<DockArea>,
}

impl RenderPlan {
    pub fn dock_at(&self, position: Position) -> Option<DockArea> {
        self.docks
            .iter()
            .copied()
            .find(|candidate| candidate.area.contains(position))
    }

    pub fn area_of(&self, pane: PaneId) -> Option<Rect> {
        self.panes
            .iter()
            .find(|candidate| candidate.pane == pane)
            .map(|candidate| candidate.area)
    }

    pub fn pane_at(&self, position: Position) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|candidate| candidate.area.contains(position))
            .map(|candidate| candidate.pane)
    }

    /// The nearest pane in `direction` that shares rows (or columns) with
    /// `from`.
    pub fn neighbour(&self, from: PaneId, direction: Direction) -> Option<PaneId> {
        let origin = self.area_of(from)?;
        self.panes
            .iter()
            .filter(|candidate| candidate.pane != from)
            .filter_map(|candidate| {
                let area = candidate.area;
                let (distance, overlaps) = match direction {
                    Direction::Left => (
                        origin.x.checked_sub(area.right())?,
                        rows_overlap(origin, area),
                    ),
                    Direction::Right => (
                        area.x.checked_sub(origin.right())?,
                        rows_overlap(origin, area),
                    ),
                    Direction::Up => (
                        origin.y.checked_sub(area.bottom())?,
                        columns_overlap(origin, area),
                    ),
                    Direction::Down => (
                        area.y.checked_sub(origin.bottom())?,
                        columns_overlap(origin, area),
                    ),
                };
                overlaps.then_some((distance, area.y, area.x, candidate.pane))
            })
            .min()
            .map(|(_, _, _, pane)| pane)
    }
}

fn rows_overlap(left: Rect, right: Rect) -> bool {
    left.y < right.bottom() && right.y < left.bottom()
}

fn columns_overlap(left: Rect, right: Rect) -> bool {
    left.x < right.right() && right.x < left.right()
}

/// Assigns `area` to the docks and the pane tree. Visible docks take their
/// edges first, each with a one-cell divider, and a dock that would leave
/// the editor fewer than [`MIN_EDITOR_EXTENT`] cells stays off this frame.
/// A split needs three cells along its axis (one per child plus the
/// divider); a narrower split shows only the branch holding the active
/// pane, so no pane is ever zero cells wide.
pub fn plan(workspace: &WorkspaceModel, area: Rect) -> RenderPlan {
    let mut plan = RenderPlan::default();
    let mut editor = area;
    for (position, dock) in workspace.docks().filter(|(_, dock)| dock.visible) {
        let size = dock.size.max(1);
        if editor.width < size + 1 + MIN_EDITOR_EXTENT {
            continue;
        }
        let (dock_area, divider) = match position {
            DockPosition::Left => {
                let dock_area = Rect::new(editor.x, editor.y, size, editor.height);
                let divider = Rect::new(editor.x + size, editor.y, 1, editor.height);
                editor.x += size + 1;
                editor.width -= size + 1;
                (dock_area, divider)
            }
            DockPosition::Right => {
                let dock_area = Rect::new(editor.right() - size, editor.y, size, editor.height);
                let divider = Rect::new(editor.right() - size - 1, editor.y, 1, editor.height);
                editor.width -= size + 1;
                (dock_area, divider)
            }
        };
        plan.docks.push(DockArea {
            position,
            area: dock_area,
        });
        plan.dividers.push(Divider {
            axis: Axis::Horizontal,
            area: divider,
        });
    }
    place(workspace.root(), editor, workspace.active_pane(), &mut plan);
    plan
}

fn place(node: &LayoutNode, area: Rect, active: PaneId, plan: &mut RenderPlan) {
    match node {
        LayoutNode::Pane(pane) => plan.panes.push(PaneArea { pane: *pane, area }),
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let extent = match axis {
                Axis::Horizontal => area.width,
                Axis::Vertical => area.height,
            };
            if extent < 3 {
                let shown = if second.contains(active) {
                    second
                } else {
                    first
                };
                return place(shown, area, active, plan);
            }
            let usable = extent - 1;
            let first_extent = u16::try_from(u32::from(usable) * u32::from(*ratio) / 1000)
                .unwrap_or(1)
                .clamp(1, usable - 1);
            let second_extent = usable - first_extent;
            let (first_area, divider, second_area) = match axis {
                Axis::Horizontal => (
                    Rect::new(area.x, area.y, first_extent, area.height),
                    Rect::new(area.x + first_extent, area.y, 1, area.height),
                    Rect::new(
                        area.x + first_extent + 1,
                        area.y,
                        second_extent,
                        area.height,
                    ),
                ),
                Axis::Vertical => (
                    Rect::new(area.x, area.y, area.width, first_extent),
                    Rect::new(area.x, area.y + first_extent, area.width, 1),
                    Rect::new(area.x, area.y + first_extent + 1, area.width, second_extent),
                ),
            };
            place(first, first_area, active, plan);
            plan.dividers.push(Divider {
                axis: *axis,
                area: divider,
            });
            place(second, second_area, active, plan);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::workspace::ItemId;

    #[test]
    fn splits_share_the_area_around_a_divider_and_never_vanish() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        let right = workspace.split_active(Axis::Horizontal, ItemId(2)).unwrap();
        let rendered = plan(&workspace, Rect::new(0, 0, 11, 4));
        assert_eq!(rendered.area_of(PaneId(1)), Some(Rect::new(0, 0, 5, 4)));
        assert_eq!(rendered.dividers[0].area, Rect::new(5, 0, 1, 4));
        assert_eq!(rendered.area_of(right), Some(Rect::new(6, 0, 5, 4)));
        assert_eq!(rendered.pane_at(Position::new(7, 1)), Some(right));
        assert_eq!(rendered.pane_at(Position::new(5, 1)), None);

        // Too narrow for a divider: only the active branch is shown.
        let rendered = plan(&workspace, Rect::new(0, 0, 2, 4));
        assert_eq!(rendered.panes.len(), 1);
        assert_eq!(rendered.panes[0].pane, right);
        assert!(rendered.dividers.is_empty());

        // An extreme ratio still leaves the small side one cell.
        for _ in 0..20 {
            workspace.adjust_split(50);
        }
        let rendered = plan(&workspace, Rect::new(0, 0, 3, 1));
        assert_eq!(rendered.area_of(PaneId(1)).map(|area| area.width), Some(1));
        assert_eq!(rendered.area_of(right).map(|area| area.width), Some(1));
    }

    #[test]
    fn docks_take_their_edges_first_and_yield_when_the_editor_would_vanish() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        workspace.toggle_dock(DockPosition::Left).unwrap();
        workspace.toggle_dock(DockPosition::Right).unwrap();
        let rendered = plan(&workspace, Rect::new(0, 0, 100, 10));
        assert_eq!(
            rendered
                .dock_at(Position::new(3, 3))
                .map(|dock| dock.position),
            Some(DockPosition::Left)
        );
        assert_eq!(rendered.docks[0].area, Rect::new(0, 0, 32, 10));
        assert_eq!(rendered.docks[1].area, Rect::new(64, 0, 36, 10));
        assert_eq!(rendered.area_of(PaneId(1)), Some(Rect::new(33, 0, 30, 10)));
        assert_eq!(rendered.dividers.len(), 2);

        // Too narrow for both: the right dock stays off, the pane keeps
        // its minimum.
        let rendered = plan(&workspace, Rect::new(0, 0, 40, 10));
        assert_eq!(rendered.docks.len(), 1);
        assert_eq!(rendered.area_of(PaneId(1)), Some(Rect::new(33, 0, 7, 10)));
        assert_eq!(rendered.dock_at(Position::new(39, 1)), None);
    }

    #[test]
    fn neighbours_follow_the_geometry() {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        let right = workspace.split_active(Axis::Horizontal, ItemId(2)).unwrap();
        let below = workspace.split_active(Axis::Vertical, ItemId(3)).unwrap();
        let rendered = plan(&workspace, Rect::new(0, 0, 21, 9));
        assert_eq!(rendered.neighbour(PaneId(1), Direction::Right), Some(right));
        assert_eq!(rendered.neighbour(right, Direction::Down), Some(below));
        assert_eq!(rendered.neighbour(below, Direction::Left), Some(PaneId(1)));
        assert_eq!(rendered.neighbour(PaneId(1), Direction::Left), None);
        assert_eq!(rendered.neighbour(below, Direction::Up), Some(right));
    }
}
