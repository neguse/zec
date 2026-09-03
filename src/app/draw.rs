//! One frame: capture inside the draw callback, then widgets.

use anyhow::{Context as _, Result};
use gpui::AsyncApp;
use ratatui::layout::Rect;

use super::{
    App,
    command::Command,
    overlay::Presentation,
    tabs::{self, TabLabel},
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

/// What the last frame showed, for mouse hit testing and scrolling.
pub struct Frame {
    pub area: Rect,
    pub snapshot: RenderSnapshot,
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

    /// Reads the visible rows inside the draw callback so the viewport and
    /// the frame area cannot disagree.
    fn draw(&mut self, frame: &mut ratatui::Frame, cx: &mut AsyncApp) -> Result<Frame> {
        let area = frame.area();
        let label = self.tab_label(cx)?;
        let row_budget = usize::from(area.height.saturating_sub(2)).min(OVERLAY_ROW_LIMIT);
        let status = self.status_row(&label, row_budget);
        let document = self
            .documents
            .get_mut(self.workspace.active_item())
            .context("active item has no document")?;
        let (viewport, follow) = (document.viewport, document.follow);
        let capture = document
            .editor
            .update(cx, |editor, window, cx| {
                editor::capture(editor, window, cx, viewport, follow, area, status)
            })
            .context("read editor state")?;
        document.viewport = capture.snapshot.viewport;
        document.follow = capture.follow;

        let widget = EditorWidget::new(&capture.snapshot);
        let cursor = widget.cursor_position(area);
        frame.render_widget(widget, area);
        if let Some(cursor) = cursor {
            frame.set_cursor_position(cursor);
        }
        Ok(Frame {
            area,
            snapshot: capture.snapshot,
        })
    }

    fn tab_label(&self, cx: &AsyncApp) -> Result<String> {
        let mut labels = Vec::with_capacity(self.workspace.items().len());
        for item in self.workspace.items() {
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
        let status = tabs::format_status(&labels, self.workspace.active_index());
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
