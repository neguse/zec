//! Stateless terminal rendering for an editor snapshot.
//!
//! This module deliberately has no dependency on Zed.  The adapter that reads
//! Zed's `Editor` is responsible for converting its display points to terminal
//! cell coordinates before constructing a [`RenderSnapshot`].

use ratatui::{
    buffer::{Buffer, CellWidth},
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::Span,
    widgets::{Clear, Widget},
};

/// A cursor position in the complete document.
///
/// `column` is a terminal-cell column, not a byte, char, or grapheme offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cursor {
    pub row: usize,
    pub column: usize,
}

/// The document position displayed at the top-left of the body.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Viewport {
    pub top_row: usize,
    pub left_column: usize,
}

/// Immutable, Zed-independent input to the terminal renderer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RenderSnapshot {
    pub lines: Vec<String>,
    pub cursor: Option<Cursor>,
    pub viewport: Viewport,
    pub status: String,
}

/// A stateless editor widget.
///
/// The last row is reserved for the status line.  Use [`Self::cursor_position`]
/// to place the terminal cursor after rendering the widget.
#[derive(Clone, Copy, Debug)]
pub struct EditorWidget<'a> {
    snapshot: &'a RenderSnapshot,
}

impl<'a> EditorWidget<'a> {
    pub const fn new(snapshot: &'a RenderSnapshot) -> Self {
        Self { snapshot }
    }

    /// Maps the document cursor to a terminal position when it is visible.
    pub fn cursor_position(&self, area: Rect) -> Option<Position> {
        if area.width == 0 || area.height <= 1 {
            return None;
        }

        let cursor = self.snapshot.cursor?;
        self.snapshot.lines.get(cursor.row)?;

        let row = cursor.row.checked_sub(self.snapshot.viewport.top_row)?;
        let column = cursor
            .column
            .checked_sub(self.snapshot.viewport.left_column)?;

        if row >= usize::from(area.height - 1) || column >= usize::from(area.width) {
            return None;
        }

        let x = area.x.checked_add(u16::try_from(column).ok()?)?;
        let y = area.y.checked_add(u16::try_from(row).ok()?)?;
        Some(Position::new(x, y))
    }

    fn status_text(&self) -> String {
        let cursor = self.snapshot.cursor.map(|cursor| {
            format!(
                "Ln {}, Col {}",
                cursor.row.saturating_add(1),
                cursor.column.saturating_add(1)
            )
        });

        match (self.snapshot.status.is_empty(), cursor) {
            (true, Some(cursor)) => cursor,
            (false, Some(cursor)) => format!("{}  {cursor}", self.snapshot.status),
            (false, None) => self.snapshot.status.clone(),
            (true, None) => String::new(),
        }
    }
}

impl Widget for EditorWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        render_snapshot(self.snapshot, area, buf);
    }
}

/// Renders a snapshot without requiring the caller to construct a widget.
pub fn render_snapshot(snapshot: &RenderSnapshot, area: Rect, buf: &mut Buffer) {
    let clipped = area.intersection(buf.area);
    if clipped.is_empty() {
        return;
    }

    // A widget can be rendered repeatedly over the same buffer.  Clearing the
    // whole clipped area prevents remnants of a previously longer line.
    Clear.render(clipped, buf);

    let body_height = area.height.saturating_sub(1);
    let body_area = Rect::new(area.x, area.y, area.width, body_height).intersection(clipped);

    for y in body_area.y..body_area.bottom() {
        let screen_row = usize::from(y.saturating_sub(area.y));
        let document_row = snapshot.viewport.top_row.saturating_add(screen_row);
        let Some(line) = snapshot.lines.get(document_row) else {
            continue;
        };

        render_line(
            line,
            Style::default(),
            y,
            area,
            clipped,
            snapshot.viewport.left_column,
            buf,
        );
    }

    if area.height == 0 {
        return;
    }

    let status_y = area.y.saturating_add(area.height - 1);
    let status_area = Rect::new(area.x, status_y, area.width, 1).intersection(clipped);
    if status_area.is_empty() {
        return;
    }

    let status_style = Style::new().add_modifier(Modifier::REVERSED);
    buf.set_style(status_area, status_style);
    render_line(
        &EditorWidget::new(snapshot).status_text(),
        status_style,
        status_y,
        area,
        clipped,
        0,
        buf,
    );
}

/// Draw one display-ready line, clipping in terminal-cell coordinates.
fn render_line(
    line: &str,
    style: Style,
    y: u16,
    area: Rect,
    clipped: Rect,
    left_column: usize,
    buf: &mut Buffer,
) {
    if area.width == 0 || y < clipped.y || y >= clipped.bottom() {
        return;
    }

    let viewport_right = left_column.saturating_add(usize::from(area.width));
    let clip_left = usize::from(clipped.x.saturating_sub(area.x));
    let clip_right = clip_left.saturating_add(usize::from(clipped.width));
    let span = Span::styled(line, style);
    let mut source_column = 0usize;

    for grapheme in span.styled_graphemes(Style::default()) {
        let width = usize::from(grapheme.symbol.cell_width());
        if width == 0 {
            continue;
        }

        let end_column = source_column.saturating_add(width);
        if end_column <= left_column {
            source_column = end_column;
            continue;
        }
        if source_column >= viewport_right {
            break;
        }

        // A terminal cannot draw half of a wide grapheme.  If either clipping
        // boundary cuts through one, leave the affected cells blank.
        if source_column < left_column {
            source_column = end_column;
            continue;
        }

        let screen_column = source_column - left_column;
        if screen_column >= usize::from(area.width) {
            break;
        }
        if screen_column < clip_left || screen_column >= clip_right {
            source_column = end_column;
            continue;
        }

        let Some(destination_x) = usize::from(area.x)
            .checked_add(screen_column)
            .and_then(|x| u16::try_from(x).ok())
        else {
            break;
        };
        let available = usize::from(area.width)
            .saturating_sub(screen_column)
            .min(clip_right.saturating_sub(screen_column));

        if width <= available {
            buf.set_stringn(destination_x, y, grapheme.symbol, available, grapheme.style);
        }

        source_column = end_column;
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Modifier, Style},
        widgets::Widget,
    };

    use super::{Cursor, EditorWidget, RenderSnapshot, Viewport};

    fn row(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.right())
            .map(|x| buf.cell((x, y)).expect("cell inside buffer").symbol())
            .collect()
    }

    #[test]
    fn renders_scrolled_body_and_status() {
        let snapshot = RenderSnapshot {
            lines: vec!["zero".into(), "one".into(), "two".into()],
            cursor: Some(Cursor { row: 2, column: 1 }),
            viewport: Viewport {
                top_row: 1,
                left_column: 0,
            },
            status: "NORMAL".into(),
        };
        let area = Rect::new(0, 0, 20, 4);
        let mut buf = Buffer::empty(area);

        EditorWidget::new(&snapshot).render(area, &mut buf);

        assert!(row(&buf, 0).starts_with("one"));
        assert!(row(&buf, 1).starts_with("two"));
        assert!(row(&buf, 2).trim().is_empty());
        assert!(row(&buf, 3).starts_with("NORMAL  Ln 3, Col 2"));
        assert!(
            buf.cell((19, 3))
                .expect("status cell")
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn clips_horizontally_in_terminal_cells() {
        let snapshot = RenderSnapshot {
            lines: vec!["ab界cd".into()],
            viewport: Viewport {
                top_row: 0,
                left_column: 2,
            },
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 4, 2);
        let mut buf = Buffer::empty(area);

        EditorWidget::new(&snapshot).render(area, &mut buf);

        assert_eq!(buf.cell((0, 0)).expect("first cell").symbol(), "界");
        assert_eq!(buf.cell((2, 0)).expect("third cell").symbol(), "c");
        assert_eq!(buf.cell((3, 0)).expect("fourth cell").symbol(), "d");
    }

    #[test]
    fn leaves_a_blank_when_viewport_cuts_a_wide_grapheme() {
        let snapshot = RenderSnapshot {
            lines: vec!["ab界cd".into()],
            viewport: Viewport {
                top_row: 0,
                left_column: 3,
            },
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 3, 2);
        let mut buf = Buffer::empty(area);

        EditorWidget::new(&snapshot).render(area, &mut buf);

        assert_eq!(row(&buf, 0), " cd");
    }

    #[test]
    fn maps_only_visible_cursor_positions() {
        let mut snapshot = RenderSnapshot {
            lines: vec![String::new(); 5],
            cursor: Some(Cursor { row: 3, column: 7 }),
            viewport: Viewport {
                top_row: 2,
                left_column: 4,
            },
            status: String::new(),
        };
        let widget_area = Rect::new(10, 5, 8, 4);

        assert_eq!(
            EditorWidget::new(&snapshot).cursor_position(widget_area),
            Some((13, 6).into())
        );

        snapshot.cursor = Some(Cursor { row: 1, column: 7 });
        assert_eq!(
            EditorWidget::new(&snapshot).cursor_position(widget_area),
            None
        );
        snapshot.cursor = Some(Cursor { row: 3, column: 12 });
        assert_eq!(
            EditorWidget::new(&snapshot).cursor_position(widget_area),
            None
        );
    }

    #[test]
    fn safely_clips_widget_to_buffer() {
        let snapshot = RenderSnapshot {
            lines: vec!["abcdef".into(), "ghijkl".into()],
            status: "status".into(),
            ..RenderSnapshot::default()
        };
        let widget_area = Rect::new(0, 0, 6, 3);
        let buffer_area = Rect::new(2, 1, 3, 2);
        let mut buf = Buffer::empty(buffer_area);

        EditorWidget::new(&snapshot).render(widget_area, &mut buf);

        assert_eq!(row(&buf, 1), "ijk");
        assert_eq!(row(&buf, 2), "atu");
    }

    #[test]
    fn clears_stale_content_and_handles_empty_areas() {
        let snapshot = RenderSnapshot {
            lines: vec!["x".into()],
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 4, 2);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "old!", Style::default());

        EditorWidget::new(&snapshot).render(area, &mut buf);

        assert_eq!(row(&buf, 0), "x   ");

        let empty = Rect::new(0, 0, 0, 0);
        EditorWidget::new(&snapshot).render(empty, &mut buf);

        let outside = Rect::new(10, 10, 2, 2);
        EditorWidget::new(&snapshot).render(outside, &mut buf);
    }
}
