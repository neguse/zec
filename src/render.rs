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
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Cursor {
    pub row: usize,
    pub column: usize,
}

/// A half-open selection range in terminal-cell coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionRange {
    pub start: Cursor,
    pub end: Cursor,
}

/// A half-open style range on one line, in terminal-cell coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StyleSpan {
    pub start_column: usize,
    pub end_column: usize,
    pub style: Style,
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
    pub selections: Vec<SelectionRange>,
    pub text_style: Style,
    pub line_styles: Vec<Vec<StyleSpan>>,
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
    buf.set_style(body_area, snapshot.text_style);

    for y in body_area.y..body_area.bottom() {
        let screen_row = usize::from(y.saturating_sub(area.y));
        let document_row = snapshot.viewport.top_row.saturating_add(screen_row);
        let Some(line) = snapshot.lines.get(document_row) else {
            continue;
        };

        render_line(
            line,
            snapshot.text_style,
            snapshot
                .line_styles
                .get(document_row)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            y,
            area,
            clipped,
            snapshot.viewport.left_column,
            buf,
        );
        render_selection_row(snapshot, document_row, line, y, area, clipped, buf);
    }

    if area.height == 0 {
        return;
    }

    let status_y = area.y.saturating_add(area.height - 1);
    let status_area = Rect::new(area.x, status_y, area.width, 1).intersection(clipped);
    if status_area.is_empty() {
        return;
    }

    let status_style = snapshot.text_style.add_modifier(Modifier::REVERSED);
    buf.set_style(status_area, status_style);
    render_line(
        &EditorWidget::new(snapshot).status_text(),
        status_style,
        &[],
        status_y,
        area,
        clipped,
        0,
        buf,
    );
}

fn render_selection_row(
    snapshot: &RenderSnapshot,
    document_row: usize,
    line: &str,
    y: u16,
    area: Rect,
    clipped: Rect,
    buf: &mut Buffer,
) {
    let line_width = usize::from(line.cell_width());
    let viewport_left = snapshot.viewport.left_column;
    let viewport_right = viewport_left.saturating_add(usize::from(area.width));
    let selection_style = Style::new().add_modifier(Modifier::REVERSED);

    for selection in &snapshot.selections {
        if selection.start >= selection.end
            || document_row < selection.start.row
            || document_row > selection.end.row
        {
            continue;
        }

        let start = if document_row == selection.start.row {
            selection.start.column
        } else {
            0
        };
        let end = if document_row == selection.end.row {
            selection.end.column
        } else {
            // Make a selected newline visible, including on an empty line.
            line_width.saturating_add(1)
        };
        let start = start.max(viewport_left);
        let end = end.min(viewport_right);
        if start >= end {
            continue;
        }

        let screen_start = start - viewport_left;
        let screen_end = end - viewport_left;
        let x_start = area
            .x
            .saturating_add(u16::try_from(screen_start).unwrap_or(area.width))
            .max(clipped.x);
        let x_end = area
            .x
            .saturating_add(u16::try_from(screen_end).unwrap_or(area.width))
            .min(clipped.right());
        if x_start < x_end {
            buf.set_style(Rect::new(x_start, y, x_end - x_start, 1), selection_style);
        }
    }
}

/// Draw one display-ready line, clipping in terminal-cell coordinates.
fn render_line(
    line: &str,
    style: Style,
    style_spans: &[StyleSpan],
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
        let grapheme_style = style_spans
            .iter()
            .filter(|span| span.start_column <= source_column && end_column <= span.end_column)
            .fold(grapheme.style, |style, span| style.patch(span.style));
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
            buf.set_stringn(destination_x, y, grapheme.symbol, available, grapheme_style);
        }

        source_column = end_column;
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier, Style},
        widgets::Widget,
    };

    use super::{Cursor, EditorWidget, RenderSnapshot, SelectionRange, StyleSpan, Viewport};

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
            selections: Vec::new(),
            text_style: Style::default(),
            line_styles: Vec::new(),
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
            selections: Vec::new(),
            text_style: Style::default(),
            line_styles: Vec::new(),
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

    #[test]
    fn renders_multiline_selection_and_selected_newlines() {
        let snapshot = RenderSnapshot {
            lines: vec!["abc".into(), String::new(), "xyz".into()],
            selections: vec![SelectionRange {
                start: Cursor { row: 0, column: 1 },
                end: Cursor { row: 2, column: 2 },
            }],
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 5, 4);
        let mut buf = Buffer::empty(area);

        EditorWidget::new(&snapshot).render(area, &mut buf);

        for (x, y) in [(1, 0), (2, 0), (3, 0), (0, 1), (0, 2), (1, 2)] {
            assert!(
                buf.cell((x, y))
                    .expect("selected cell")
                    .modifier
                    .contains(Modifier::REVERSED),
                "expected ({x}, {y}) to be selected"
            );
        }
        for (x, y) in [(0, 0), (4, 0), (1, 1), (2, 2)] {
            assert!(
                !buf.cell((x, y))
                    .expect("unselected cell")
                    .modifier
                    .contains(Modifier::REVERSED),
                "expected ({x}, {y}) not to be selected"
            );
        }
    }

    #[test]
    fn clips_selection_in_terminal_cell_coordinates() {
        let snapshot = RenderSnapshot {
            lines: vec!["a界bc".into()],
            selections: vec![SelectionRange {
                start: Cursor { row: 0, column: 1 },
                end: Cursor { row: 0, column: 4 },
            }],
            viewport: Viewport {
                top_row: 0,
                left_column: 2,
            },
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 3, 2);
        let mut buf = Buffer::empty(area);

        EditorWidget::new(&snapshot).render(area, &mut buf);

        assert_eq!(row(&buf, 0), " bc");
        for x in [0, 1] {
            assert!(
                buf.cell((x, 0))
                    .expect("selected cell")
                    .modifier
                    .contains(Modifier::REVERSED)
            );
        }
        assert!(
            !buf.cell((2, 0))
                .expect("unselected cell")
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn combines_syntax_style_with_selection_on_wide_graphemes() {
        let snapshot = RenderSnapshot {
            lines: vec!["a界b".into()],
            selections: vec![SelectionRange {
                start: Cursor { row: 0, column: 1 },
                end: Cursor { row: 0, column: 3 },
            }],
            text_style: Style::new().fg(Color::White),
            line_styles: vec![vec![StyleSpan {
                start_column: 1,
                end_column: 3,
                style: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            }]],
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 4, 2);
        let mut buf = Buffer::empty(area);

        EditorWidget::new(&snapshot).render(area, &mut buf);

        let glyph = buf.cell((1, 0)).expect("wide grapheme start cell");
        assert_eq!(glyph.fg, Color::Red);
        assert!(glyph.modifier.contains(Modifier::BOLD));
        assert!(glyph.modifier.contains(Modifier::REVERSED));
        assert!(
            buf.cell((2, 0))
                .expect("wide grapheme continuation cell")
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(buf.cell((0, 0)).expect("base cell").fg, Color::White);
        assert!(
            !buf.cell((3, 0))
                .expect("unselected cell")
                .modifier
                .contains(Modifier::REVERSED)
        );
    }
}
