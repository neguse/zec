//! Terminal panel: Zed's terminals in the bottom dock.
//!
//! Zed owns the PTY, the shell, the escape-sequence parser, and the
//! scrollback. zec keeps the list of terminals, forwards keys as the bytes
//! Zed's terminal maps them to, and projects the synced grid onto the
//! dock's cells on every frame, resizing the terminal to the dock first.

use std::path::Path;

use anyhow::{Context as _, Result};
use gpui::{AsyncApp, Bounds, Entity, Keystroke, Point as GpuiPoint, Size, Subscription, px};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
};
use zed_terminal::{
    Cell, Color as ZedColor, Content, CursorShape, Modes, NamedColor, TaskStatus, Terminal,
    TerminalBounds,
};

use crate::{
    app::{
        event::Event,
        feature::{Ctx, FeatureEvent},
    },
    terminal::{
        ScrollDirection,
        render::{TerminalCell, TerminalPanelSnapshot, TerminalPanelWidget},
    },
};

/// The key context of this panel's bindings. It leaves out the shared
/// panel context: arrows, Enter, and Esc belong to the shell.
pub const KEY_CONTEXT: &str = "zec_terminal_panel";

#[derive(Debug)]
pub enum TerminalPanelEvent {
    /// The shell or task in a terminal exited.
    Closed { entity_id: u64 },
}

/// What the app does after the panel handled an event.
pub enum TerminalOutcome {
    Continue,
    /// The last terminal closed; the dock has nothing left to show.
    Emptied,
}

struct Slot {
    terminal: Entity<Terminal>,
    /// Process-monotonic, so a closed "Terminal 1" is never reused.
    number: usize,
    _subscription: Subscription,
}

#[derive(Default)]
pub struct TerminalPanel {
    slots: Vec<Slot>,
    active: usize,
    opened: usize,
}

impl TerminalPanel {
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The number shown in the active terminal's title.
    pub fn active_number(&self) -> Option<usize> {
        self.slots.get(self.active).map(|slot| slot.number)
    }

    /// Starts a shell in the root, or where zec was started without one,
    /// and makes it the active terminal.
    pub async fn create(&mut self, ctx: &mut Ctx<'_>, cx: &mut AsyncApp) -> Result<usize> {
        let cwd = ctx.root.map(Path::to_path_buf);
        let terminal = ctx
            .services
            .project
            .update(cx, |project, cx| project.create_terminal_shell(cwd, cx))
            .await
            .context("start a shell")?;
        Ok(self.adopt(terminal, ctx, cx))
    }

    /// Takes over a terminal Zed created elsewhere, such as a task, and
    /// makes it the active one.
    pub fn adopt(&mut self, terminal: Entity<Terminal>, ctx: &mut Ctx, cx: &mut AsyncApp) -> usize {
        self.opened += 1;
        let number = self.opened;
        let events = ctx.events.clone();
        let entity_id = terminal.entity_id().as_u64();
        let subscription = cx.update(|cx| {
            cx.subscribe(&terminal, move |_, event: &zed_terminal::Event, _| {
                let event =
                    match event {
                        zed_terminal::Event::CloseTerminal => Event::Feature(
                            FeatureEvent::TerminalPanel(TerminalPanelEvent::Closed { entity_id }),
                        ),
                        _ => Event::Redraw,
                    };
                let _ = events.try_send(event);
            })
        });
        self.slots.push(Slot {
            terminal,
            number,
            _subscription: subscription,
        });
        self.active = self.slots.len() - 1;
        number
    }

    /// Activates the next or previous terminal, wrapping; the new number.
    pub fn select_adjacent(&mut self, forward: bool) -> Option<usize> {
        let len = self.slots.len();
        if len < 2 {
            return None;
        }
        self.active = if forward {
            (self.active + 1) % len
        } else {
            (self.active + len - 1) % len
        };
        self.active_number()
    }

    pub fn update(&mut self, event: TerminalPanelEvent) -> TerminalOutcome {
        let TerminalPanelEvent::Closed { entity_id } = event;
        if let Some(index) = self
            .slots
            .iter()
            .position(|slot| slot.terminal.entity_id().as_u64() == entity_id)
        {
            self.slots.remove(index);
            self.active = self.active.min(self.slots.len().saturating_sub(1));
        }
        if self.slots.is_empty() {
            TerminalOutcome::Emptied
        } else {
            TerminalOutcome::Continue
        }
    }

    /// Forwards a key the keymap did not claim. Zed maps special keys to
    /// escape sequences; printable text goes through as its bytes.
    pub fn key(&mut self, keystroke: &Keystroke, cx: &mut AsyncApp) -> bool {
        let Some(slot) = self.slots.get(self.active) else {
            return false;
        };
        slot.terminal.update(cx, |terminal, _| {
            if terminal.try_keystroke(keystroke, false) {
                return true;
            }
            match keystroke.key_char.as_deref() {
                Some(text) => {
                    terminal.input(text.as_bytes().to_vec());
                    true
                }
                None => false,
            }
        })
    }

    pub fn paste(&mut self, text: &str, cx: &mut AsyncApp) {
        if let Some(slot) = self.slots.get(self.active) {
            slot.terminal.update(cx, |terminal, _| terminal.paste(text));
        }
    }

    pub fn scroll(&mut self, direction: ScrollDirection, cx: &mut AsyncApp) {
        if let Some(slot) = self.slots.get(self.active) {
            slot.terminal.update(cx, |terminal, _| match direction {
                ScrollDirection::Up => terminal.scroll_up_by(3),
                ScrollDirection::Down => terminal.scroll_down_by(3),
            });
        }
    }

    /// Resizes the active terminal to the dock body, syncs Zed's grid, and
    /// projects it. Runs inside the draw callback.
    pub fn view(
        &mut self,
        ctx: &mut Ctx,
        area: Rect,
        cx: &mut AsyncApp,
    ) -> Result<TerminalPanelSnapshot> {
        let slot = self.slots.get(self.active).context("no terminal")?;
        let inner = TerminalPanelWidget::inner(area);
        let number = slot.number;
        let terminal = slot.terminal.clone();
        // `sync` wants a window for hyperlink hover; the active document's
        // hidden window serves.
        ctx.editor
            .update(cx, move |_, window, cx| {
                terminal.update(cx, |terminal, cx| {
                    terminal.set_size(TerminalBounds::new(
                        px(1.0),
                        px(1.0),
                        Bounds {
                            origin: GpuiPoint::default(),
                            size: Size {
                                width: px(f32::from(inner.width.max(1))),
                                height: px(f32::from(inner.height.max(1))),
                            },
                        },
                    ));
                    terminal.sync(window, cx);
                    capture(terminal, number)
                })
            })
            .context("synchronize the terminal")
    }

    /// Drops every terminal; Zed kills their processes.
    pub fn close_all(&mut self) {
        self.slots.clear();
        self.active = 0;
    }
}

fn capture(terminal: &Terminal, number: usize) -> TerminalPanelSnapshot {
    let content = terminal.last_content();
    let first_line = content
        .cells
        .first()
        .map(|cell| cell.point.line)
        .unwrap_or(content.cursor.point.line);
    let selection = content.selection.map(|selection| selection.point_range());
    let cells = content
        .cells
        .iter()
        .filter_map(|indexed| {
            if indexed.cell.is_wide_char_spacer() {
                return None;
            }
            let row = usize::try_from(indexed.point.line.saturating_sub(first_line)).ok()?;
            if row >= content.screen_lines || indexed.point.column >= content.columns {
                return None;
            }
            let mut symbol = indexed.cell.character().to_string();
            if let Some(extra) = indexed.cell.zerowidth() {
                symbol.extend(extra);
            }
            Some(TerminalCell {
                row,
                column: indexed.point.column,
                symbol,
                style: cell_style(
                    &indexed.cell,
                    selection.is_some_and(|range| range.contains(indexed.point)),
                ),
            })
        })
        .collect();
    let mut title = format!(
        " Terminal {number} · {}",
        bounded_title(&terminal.title(true))
    );
    if let Some(task) = terminal.task() {
        title.push_str(match task.status {
            TaskStatus::Unknown => " · unknown",
            TaskStatus::Running => " · running",
            TaskStatus::Completed { success: true } => " · passed",
            TaskStatus::Completed { success: false } => " · failed",
        });
    }
    if !content.scrolled_to_bottom {
        title.push_str(" · scrollback");
    }
    title.push(' ');
    TerminalPanelSnapshot {
        title,
        cells,
        cursor: terminal_cursor(content, first_line),
    }
}

fn terminal_cursor(content: &Content, first_line: i32) -> Option<(usize, usize)> {
    if !content.mode.contains(Modes::SHOW_CURSOR)
        || content.cursor.shape == CursorShape::Hidden
        || !content.scrolled_to_bottom
    {
        return None;
    }
    let row = usize::try_from(content.cursor.point.line.saturating_sub(first_line)).ok()?;
    (row < content.screen_lines && content.cursor.point.column < content.columns)
        .then_some((row, content.cursor.point.column))
}

fn cell_style(cell: &Cell, selected: bool) -> Style {
    let (mut foreground, mut background) = (cell.foreground(), cell.background());
    if cell.is_inverse() {
        std::mem::swap(&mut foreground, &mut background);
    }
    let mut style = Style::default();
    if let Some(color) = color(foreground, true) {
        style = style.fg(color);
    }
    if let Some(color) = color(background, false) {
        style = style.bg(color);
    }
    if cell.is_bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.is_dim() {
        style = style.add_modifier(Modifier::DIM);
    }
    if cell.is_italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.has_underline() || cell.has_undercurl() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.has_strikeout() {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

/// Terminal colors map to the host terminal's own palette; the default
/// foreground and background stay unset so the theme shows through.
fn color(color: ZedColor, foreground: bool) -> Option<Color> {
    match color {
        ZedColor::Spec(rgb) => Some(Color::Rgb(rgb.r, rgb.g, rgb.b)),
        ZedColor::Indexed(index) => Some(Color::Indexed(index)),
        ZedColor::Named(named) => named_color(named, foreground),
    }
}

fn named_color(color: NamedColor, foreground: bool) -> Option<Color> {
    Some(match color {
        NamedColor::Black | NamedColor::DimBlack => Color::Black,
        NamedColor::Red | NamedColor::DimRed => Color::Red,
        NamedColor::Green | NamedColor::DimGreen => Color::Green,
        NamedColor::Yellow | NamedColor::DimYellow => Color::Yellow,
        NamedColor::Blue | NamedColor::DimBlue => Color::Blue,
        NamedColor::Magenta | NamedColor::DimMagenta => Color::Magenta,
        NamedColor::Cyan | NamedColor::DimCyan => Color::Cyan,
        NamedColor::White | NamedColor::DimWhite => Color::Gray,
        NamedColor::BrightBlack => Color::DarkGray,
        NamedColor::BrightRed => Color::LightRed,
        NamedColor::BrightGreen => Color::LightGreen,
        NamedColor::BrightYellow => Color::LightYellow,
        NamedColor::BrightBlue => Color::LightBlue,
        NamedColor::BrightMagenta => Color::LightMagenta,
        NamedColor::BrightCyan => Color::LightCyan,
        NamedColor::BrightWhite => Color::White,
        NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground
            if foreground =>
        {
            return None;
        }
        NamedColor::Background if !foreground => return None,
        NamedColor::Foreground
        | NamedColor::BrightForeground
        | NamedColor::DimForeground
        | NamedColor::Cursor => Color::White,
        NamedColor::Background => Color::Black,
    })
}

/// A shell can put anything in its title; keep it printable and short.
fn bounded_title(title: &str) -> String {
    let output = title
        .chars()
        .filter(|character| !character.is_control())
        .take(60)
        .collect::<String>();
    if output.is_empty() {
        "shell".to_owned()
    } else {
        output
    }
}
