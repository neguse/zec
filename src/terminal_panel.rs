//! Terminal-cell projection of Zed's terminal model.
//!
//! Zed owns the PTY, shell/task lifecycle, escape-sequence parser, scrollback,
//! cwd detection, and remote routing.  This module only converts the synced
//! [`zed_terminal::Content`] into a bounded ratatui widget.

use std::path::PathBuf;

use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, Widget},
};
use zed_terminal::{
    Cell, Color as ZedColor, Content, CursorShape, Modes, NamedColor, TaskStatus, Terminal,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalCell {
    pub(crate) row: usize,
    pub(crate) column: usize,
    pub(crate) symbol: String,
    pub(crate) style: Style,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalPanelSnapshot {
    pub(crate) title: String,
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) task_status: Option<&'static str>,
    pub(crate) rows: usize,
    pub(crate) columns: usize,
    pub(crate) cells: Vec<TerminalCell>,
    pub(crate) cursor: Option<(usize, usize)>,
    pub(crate) scrolled_to_bottom: bool,
}

impl TerminalPanelSnapshot {
    pub(crate) fn capture(terminal: &Terminal) -> Self {
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
        let cursor = terminal_cursor(content, first_line);
        let task_status = terminal.task().map(|task| match task.status {
            TaskStatus::Unknown => "unknown",
            TaskStatus::Running => "running",
            TaskStatus::Completed { success: true } => "passed",
            TaskStatus::Completed { success: false } => "failed",
        });

        Self {
            title: bounded_title(&terminal.title(false)),
            working_directory: terminal.working_directory(),
            task_status,
            rows: content.screen_lines,
            columns: content.columns,
            cells,
            cursor,
            scrolled_to_bottom: content.scrolled_to_bottom,
        }
    }

    fn border_title(&self) -> String {
        let mut title = format!(" Terminal · {}", self.title);
        if let Some(status) = self.task_status {
            title.push_str(" · ");
            title.push_str(status);
        }
        if !self.scrolled_to_bottom {
            title.push_str(" · scrollback");
        }
        if let Some(cwd) = self
            .working_directory
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
        {
            title.push_str(" · ");
            title.push_str(cwd);
        }
        title.push(' ');
        title
    }
}

pub(crate) struct TerminalPanelWidget<'a> {
    snapshot: &'a TerminalPanelSnapshot,
    focused: bool,
}

impl<'a> TerminalPanelWidget<'a> {
    pub(crate) fn new(snapshot: &'a TerminalPanelSnapshot, focused: bool) -> Self {
        Self { snapshot, focused }
    }

    pub(crate) fn inner(area: Rect) -> Rect {
        Block::default().borders(Borders::ALL).inner(area)
    }

    pub(crate) fn cursor_position(&self, area: Rect) -> Option<Position> {
        if !self.focused {
            return None;
        }
        let inner = Self::inner(area);
        let (row, column) = self.snapshot.cursor?;
        if row >= usize::from(inner.height) || column >= usize::from(inner.width) {
            return None;
        }
        Some(Position::new(
            inner.x.saturating_add(u16::try_from(column).ok()?),
            inner.y.saturating_add(u16::try_from(row).ok()?),
        ))
    }
}

impl Widget for TerminalPanelWidget<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.width < 3 || area.height < 3 {
            return;
        }
        Clear.render(area, buffer);
        let border_style = if self.focused {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.snapshot.border_title())
            .border_style(border_style);
        let inner = block.inner(area);
        block.render(area, buffer);

        for cell in &self.snapshot.cells {
            let Ok(row) = u16::try_from(cell.row) else {
                continue;
            };
            let Ok(column) = u16::try_from(cell.column) else {
                continue;
            };
            if row >= inner.height || column >= inner.width {
                continue;
            }
            if let Some(target) =
                buffer.cell_mut((inner.x.saturating_add(column), inner.y.saturating_add(row)))
            {
                target.set_symbol(&cell.symbol).set_style(cell.style);
            }
        }
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

fn bounded_title(title: &str) -> String {
    let mut output = title
        .chars()
        .filter(|character| !character.is_control())
        .take(80)
        .collect::<String>();
    if output.is_empty() {
        output.push_str("Terminal");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> TerminalPanelSnapshot {
        TerminalPanelSnapshot {
            title: "shell".to_owned(),
            working_directory: Some(PathBuf::from("/work/project")),
            task_status: None,
            rows: 2,
            columns: 4,
            cells: vec![TerminalCell {
                row: 0,
                column: 1,
                symbol: "λ".to_owned(),
                style: Style::default().fg(Color::LightGreen),
            }],
            cursor: Some((1, 2)),
            scrolled_to_bottom: true,
        }
    }

    #[test]
    fn renders_terminal_cells_and_cursor_inside_border() {
        let snapshot = snapshot();
        let area = Rect::new(3, 4, 28, 4);
        let mut buffer = Buffer::empty(area);
        TerminalPanelWidget::new(&snapshot, true).render(area, &mut buffer);

        assert_eq!(buffer[(5, 5)].symbol(), "λ");
        assert_eq!(
            TerminalPanelWidget::new(&snapshot, true).cursor_position(area),
            Some(Position::new(6, 6))
        );
        let border = (3..31).map(|x| buffer[(x, 4)].symbol()).collect::<String>();
        assert!(border.contains("Terminal"));
    }

    #[test]
    fn unfocused_or_scrollback_terminal_hides_hardware_cursor() {
        let mut snapshot = snapshot();
        assert_eq!(
            TerminalPanelWidget::new(&snapshot, false).cursor_position(Rect::new(0, 0, 8, 4)),
            None
        );
        snapshot.cursor = None;
        assert_eq!(
            TerminalPanelWidget::new(&snapshot, true).cursor_position(Rect::new(0, 0, 8, 4)),
            None
        );
    }

    #[test]
    fn strips_control_sequences_from_terminal_title() {
        assert_eq!(bounded_title("\u{1b}]0;hello\u{7}"), "]0;hello");
        assert_eq!(bounded_title("\n\r"), "Terminal");
    }
}
