//! Outline panel: the symbols of the active buffer in the right dock.
//!
//! The rows are Zed's outline of the active buffer, read on every frame
//! while the dock is visible; the panel keeps only the selection. Enter
//! moves the caret to the symbol and returns focus to the editor.

use gpui::AsyncApp;

use crate::{
    app::{
        command::Command,
        feature::{Ctx, PanelOutcome},
    },
    terminal::render::{OverlayRow, OverlaySnapshot, window_start},
    zed::services::{self, OutlineRow},
};

/// The key context of this panel's own bindings.
pub const KEY_CONTEXT: &str = "zec_outline_panel";

#[derive(Default)]
pub struct OutlinePanel {
    selected: usize,
    /// The buffer the selection belongs to; another buffer starts at the top.
    buffer: Option<u64>,
    /// The first row the last frame showed, for mouse hit testing.
    first_row: usize,
}

impl OutlinePanel {
    /// Runs one panel command while this panel has focus.
    pub fn execute(&mut self, ctx: &mut Ctx, command: Command, cx: &mut AsyncApp) -> PanelOutcome {
        let rows = self.rows(ctx, cx);
        match command {
            Command::PanelSelectNext | Command::PanelExpand => {
                if self.selected + 1 < rows.len() {
                    self.selected += 1;
                }
            }
            Command::PanelSelectPrevious | Command::PanelCollapse => {
                self.selected = self.selected.saturating_sub(1);
            }
            Command::PanelActivate => {
                if let Some(row) = rows.get(self.selected) {
                    return PanelOutcome::Jump {
                        point: row.point,
                        label: row.text.clone(),
                    };
                }
            }
            _ => {}
        }
        PanelOutcome::Consumed
    }

    /// The rows to draw, windowed so the selection stays visible.
    pub fn view(&mut self, ctx: &mut Ctx, row_budget: usize, cx: &mut AsyncApp) -> OverlaySnapshot {
        let rows = self.rows(ctx, cx);
        let selected = (!rows.is_empty()).then(|| self.selected.min(rows.len() - 1));
        self.first_row = window_start(rows.len(), selected, row_budget);
        let label = ctx
            .documents
            .get(ctx.workspace.active_item())
            .map(|document| document.label(cx))
            .unwrap_or_default();
        OverlaySnapshot {
            title: format!(" Outline {label} "),
            rows: rows
                .iter()
                .map(|row| OverlayRow {
                    text: format!("{}{}", "  ".repeat(row.depth), row.text),
                    enabled: true,
                })
                .collect(),
            selected,
        }
    }

    /// A click on the panel body selects the row under the pointer.
    pub fn click(&mut self, ctx: &mut Ctx, screen_row: usize, cx: &mut AsyncApp) {
        let rows = self.rows(ctx, cx);
        let index = self.first_row + screen_row;
        if index < rows.len() {
            self.selected = index;
        }
    }

    fn rows(&mut self, ctx: &Ctx, cx: &AsyncApp) -> Vec<OutlineRow> {
        let Some(document) = ctx.documents.get(ctx.workspace.active_item()) else {
            return Vec::new();
        };
        let buffer = document.buffer_id(cx);
        if self.buffer != Some(buffer) {
            self.buffer = Some(buffer);
            self.selected = 0;
        }
        services::outline(&document.buffer, cx)
    }
}
