//! One frame: the render plan, one capture per visible pane inside the
//! draw callback, then widgets.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use gpui::AsyncApp;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
};

use super::{
    App,
    command::Command,
    layout::{self, RenderPlan},
    overlay::Presentation,
    tabs::{self, TabLabel},
    workspace::{Axis, PaneId},
};
use crate::{
    terminal::{
        self, Terminal,
        render::{EditorWidget, RenderSnapshot},
    },
    zed::editor::{self, StatusRow},
};

/// Keys shown on the status row, derived from the live keymap.
const HINTS: &[(Command, &str)] = &[
    (Command::CommandPalette, "commands"),
    (Command::NewFile, "new"),
    (Command::OpenFile, "open"),
    (Command::CloseItem, "close"),
    (Command::Reload, "reload"),
    (Command::Save, "save"),
    (Command::Quit, "quit"),
];

/// Rows a picker may use above the status row.
const OVERLAY_ROW_LIMIT: usize = 12;

/// What one pane showed in the last frame.
pub struct PaneFrame {
    pub area: Rect,
    pub snapshot: RenderSnapshot,
}

/// What the last frame showed, for mouse hit testing and scrolling.
pub struct Frame {
    pub plan: RenderPlan,
    pub panes: BTreeMap<PaneId, PaneFrame>,
}

impl Frame {
    pub fn pane(&self, pane: PaneId) -> Option<&PaneFrame> {
        self.panes.get(&pane)
    }
}

impl App {
    pub fn draw_frame(&mut self, terminal: &mut Terminal, cx: &mut AsyncApp) -> Result<()> {
        if self.needs_invalidate {
            terminal::session::invalidate(terminal).context("invalidate the terminal")?;
            self.needs_invalidate = false;
        }
        let mut drawn = None;
        terminal
            .try_draw(|frame| match self.draw(frame, cx) {
                Ok(state) => {
                    drawn = Some(state);
                    Ok(())
                }
                Err(error) => Err(std::io::Error::other(format!("{error:#}"))),
            })
            .context("draw the terminal frame")?;
        self.frame = drawn;
        self.resize = None;
        Ok(())
    }

    /// Reads the visible rows of every pane inside the draw callback so the
    /// viewports and the frame area cannot disagree.
    fn draw(&mut self, frame: &mut ratatui::Frame, cx: &mut AsyncApp) -> Result<Frame> {
        let area = frame.area();
        let plan = layout::plan(&self.workspace, area);
        let row_budget = usize::from(area.height.saturating_sub(2)).min(OVERLAY_ROW_LIMIT);
        let active_pane = self.workspace.active_pane();
        let mut panes = BTreeMap::new();

        for pane_area in &plan.panes {
            let focused = pane_area.pane == active_pane;
            let status = if focused {
                self.status_row(&self.tab_label(pane_area.pane, cx)?, row_budget)
            } else {
                StatusRow {
                    text: self.tab_strip(pane_area.pane, cx)?,
                    cursor_column: None,
                    overlay: None,
                }
            };
            let item = self
                .workspace
                .pane(pane_area.pane)
                .context("planned pane is not in the workspace")?
                .active_item();
            let document = self
                .documents
                .get_mut(item)
                .context("pane item has no document")?;
            let (viewport, follow) = (document.viewport, document.follow);
            let capture = document
                .editor
                .update(cx, |editor, window, cx| {
                    editor::capture(editor, window, cx, viewport, follow, pane_area.area, status)
                })
                .context("read editor state")?;
            document.viewport = capture.snapshot.viewport;
            document.follow = capture.follow;

            let widget = EditorWidget::new(&capture.snapshot);
            let cursor = widget.cursor_position(pane_area.area);
            frame.render_widget(widget, pane_area.area);
            if focused && let Some(cursor) = cursor {
                frame.set_cursor_position(cursor);
            }
            panes.insert(
                pane_area.pane,
                PaneFrame {
                    area: pane_area.area,
                    snapshot: capture.snapshot,
                },
            );
        }

        let buffer = frame.buffer_mut();
        let divider_style = Style::default().add_modifier(Modifier::DIM);
        for divider in &plan.dividers {
            let symbol = match divider.axis {
                Axis::Horizontal => "│",
                Axis::Vertical => "─",
            };
            for y in divider.area.y..divider.area.bottom() {
                for x in divider.area.x..divider.area.right() {
                    if let Some(cell) = buffer.cell_mut((x, y)) {
                        cell.set_symbol(symbol).set_style(divider_style);
                    }
                }
            }
        }
        Ok(Frame { plan, panes })
    }

    /// The tab strip of one pane.
    fn tab_strip(&self, pane: PaneId, cx: &AsyncApp) -> Result<String> {
        let pane = self
            .workspace
            .pane(pane)
            .context("pane is not in the workspace")?;
        let mut labels = Vec::with_capacity(pane.items().len());
        for item in pane.items() {
            let document = self
                .documents
                .get(*item)
                .context("workspace item has no document")?;
            let state = document.state(cx);
            labels.push(TabLabel {
                name: document.label(cx),
                dirty: state.dirty,
                conflict: state.has_external_change(),
            });
        }
        Ok(tabs::format_status(&labels, pane.active_index()))
    }

    fn tab_label(&self, pane: PaneId, cx: &AsyncApp) -> Result<String> {
        let status = self.tab_strip(pane, cx)?;
        Ok(match &self.root {
            Some(root) => format!(
                "{}  {status}",
                root.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root.display().to_string())
            ),
            None => status,
        })
    }

    fn status_row(&self, label: &str, row_budget: usize) -> StatusRow {
        let hints = HINTS
            .iter()
            .map(|(command, text)| format!("{} {text}", self.key_hint(*command)))
            .collect::<Vec<_>>()
            .join("  ");
        let base = format!("zec {label}  {hints}");
        let plain = |text| StatusRow {
            text,
            cursor_column: None,
            overlay: None,
        };
        match self.overlays.presentation(row_budget) {
            Presentation::Input {
                status,
                cursor_column,
                overlay,
            } => StatusRow {
                text: status,
                cursor_column: Some(cursor_column),
                overlay,
            },
            Presentation::Message(message) => plain(format!("{message}  |  {base}")),
            Presentation::None => plain(match self.status.message() {
                Some(message) => format!("{message}  |  {base}"),
                None => base,
            }),
        }
    }
}
