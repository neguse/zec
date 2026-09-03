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
    widgets::{Block, Borders, Clear, Widget},
};

/// A cursor position in the complete document.
///
/// `column` is a terminal-cell column, not a byte, char, or grapheme offset.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Cursor {
    pub row: usize,
    pub column: usize,
}

/// A position in the display-ready text captured from Zed.
///
/// `byte_column` is a UTF-8 byte offset in the corresponding display line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextPosition {
    pub row: usize,
    pub byte_column: usize,
}

/// Returns the nearest UTF-8 byte boundary for a terminal-cell column.
/// Wide graphemes use their midpoint, matching editor mouse hit testing.
pub fn closest_text_byte_column(line: &str, target_column: usize) -> usize {
    let mut byte_column = 0usize;
    let mut terminal_column = 0usize;
    for grapheme in Span::raw(line).styled_graphemes(Style::default()) {
        let start_byte = byte_column;
        byte_column = byte_column.saturating_add(grapheme.symbol.len());
        let width = usize::from(grapheme.symbol.cell_width());
        if width == 0 {
            continue;
        }
        let end_column = terminal_column.saturating_add(width);
        if target_column < end_column {
            return if target_column
                .saturating_sub(terminal_column)
                .saturating_mul(2)
                < width
            {
                start_byte
            } else {
                byte_column
            };
        }
        terminal_column = end_column;
    }
    line.len()
}

/// A half-open selection range in terminal-cell coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionRange {
    pub start: Cursor,
    pub end: Cursor,
}

/// A half-open background highlight range in terminal-cell coordinates.
///
/// Only the background component of `style` is rendered.  Foreground colors
/// and modifiers remain owned by syntax highlighting and selections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackgroundRange {
    pub range: SelectionRange,
    pub style: Style,
}

/// A half-open style range on one line, in terminal-cell coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StyleSpan {
    pub start_column: usize,
    pub end_column: usize,
    pub style: Style,
}

/// A presentation-only decoration anchored to one terminal cell.
///
/// `glyph` is written only when the destination cell is blank. This lets
/// whitespace markers and vertical guides coexist with the immutable source
/// text used for hit testing. When `style_on_text` is true, the style is also
/// patched onto an occupied cell (used by wrap guides).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellDecoration {
    pub row: usize,
    pub column: usize,
    pub glyph: Option<String>,
    pub style: Style,
    pub style_on_text: bool,
}

/// Virtual text placed after a display line without changing its source text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InlineAnnotation {
    pub row: usize,
    pub column: usize,
    pub text: String,
    pub style: Style,
}

/// The document position displayed at the top-left of the body.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Viewport {
    pub top_row: usize,
    pub left_column: usize,
}

/// One presentation-only row in a terminal overlay collection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OverlayRow {
    pub text: String,
    pub enabled: bool,
}

/// A bounded terminal projection of a picker, menu, completion list, or panel.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OverlaySnapshot {
    pub title: String,
    pub rows: Vec<OverlayRow>,
    pub selected: Option<usize>,
}

/// Renders an [`OverlaySnapshot`] as a full workspace dock instead of a
/// floating editor overlay. The domain model chooses and windows the rows;
/// this widget only owns terminal-cell presentation and hit testing.
pub struct OverlayPanelWidget<'a> {
    snapshot: &'a OverlaySnapshot,
    focused: bool,
}

impl<'a> OverlayPanelWidget<'a> {
    pub fn new(snapshot: &'a OverlaySnapshot, focused: bool) -> Self {
        Self { snapshot, focused }
    }

    pub fn row_at(area: Rect, position: Position) -> Option<usize> {
        let inner = Block::default().borders(Borders::ALL).inner(area);
        if !inner.contains(position) {
            return None;
        }
        Some(usize::from(position.y.saturating_sub(inner.y)))
    }

    pub fn row_budget(area: Rect) -> usize {
        usize::from(Block::default().borders(Borders::ALL).inner(area).height)
    }
}

impl Widget for OverlayPanelWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 3 || area.height < 3 {
            return;
        }

        Clear.render(area, buf);
        let border_style = if self.focused {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.snapshot.title.as_str())
            .border_style(border_style);
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.is_empty() {
            return;
        }

        let selected = self
            .snapshot
            .selected
            .filter(|selected| *selected < self.snapshot.rows.len());
        for (screen_row, row) in self
            .snapshot
            .rows
            .iter()
            .take(usize::from(inner.height))
            .enumerate()
        {
            let y = inner
                .y
                .saturating_add(u16::try_from(screen_row).unwrap_or(inner.height));
            let row_area = Rect::new(inner.x, y, inner.width, 1);
            let mut style = Style::default();
            if selected == Some(screen_row) {
                style = style.add_modifier(Modifier::REVERSED);
            } else if !row.enabled {
                style = style.add_modifier(Modifier::DIM);
            }
            buf.set_style(row_area, style);
            let prefix = if selected == Some(screen_row) {
                "› "
            } else {
                "  "
            };
            render_line(
                &format!("{prefix}{}", row.text),
                style,
                &[],
                y,
                inner,
                inner,
                0,
                buf,
            );
        }
    }
}

/// Immutable, Zed-independent input to the terminal renderer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RenderSnapshot {
    /// Global display row represented by the first entry in the row-local vectors.
    pub first_row: usize,
    /// Total number of display rows in the document.
    pub total_rows: usize,
    /// Display-ready rows starting at [`Self::first_row`].
    pub lines: Vec<String>,
    /// One-based buffer line number for each captured display row.
    ///
    /// Rows inserted by the display map, such as block rows, have no number.
    pub line_numbers: Vec<Option<u32>>,
    /// Largest one-based line number in the underlying buffer.
    ///
    /// Keeping this separate from the visible rows prevents the gutter width
    /// from changing while scrolling or folding.
    pub widest_line_number: u32,
    pub cursor: Option<Cursor>,
    /// Software-rendered cursors. The primary cursor remains the terminal's
    /// hardware cursor so prompts and IME behaviour stay native.
    pub secondary_cursors: Vec<Cursor>,
    /// One-based buffer line number containing the cursor, even when offscreen.
    pub cursor_line_number: Option<u32>,
    pub selections: Vec<SelectionRange>,
    pub text_style: Style,
    pub gutter_style: Style,
    pub line_styles: Vec<Vec<StyleSpan>>,
    pub background_ranges: Vec<BackgroundRange>,
    pub cell_decorations: Vec<CellDecoration>,
    pub inline_annotations: Vec<InlineAnnotation>,
    pub viewport: Viewport,
    pub status: String,
    /// Terminal-cell column for an input cursor on the status row.
    pub status_cursor_column: Option<usize>,
    /// The topmost overlay. Domain state remains outside the renderer.
    pub overlay: Option<OverlaySnapshot>,
}

impl RenderSnapshot {
    fn local_row_index(&self, document_row: usize) -> Option<usize> {
        let local_row = document_row.checked_sub(self.first_row)?;
        (local_row < self.lines.len()).then_some(local_row)
    }

    fn line(&self, document_row: usize) -> Option<&str> {
        self.lines
            .get(self.local_row_index(document_row)?)
            .map(String::as_str)
    }

    fn line_number(&self, document_row: usize) -> Option<u32> {
        self.line_numbers
            .get(self.local_row_index(document_row)?)
            .copied()
            .flatten()
    }
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

    /// Width available for document text after reserving the line-number gutter.
    #[cfg(test)]
    pub fn text_width(&self, area: Rect) -> u16 {
        area.width.saturating_sub(self.gutter_width(area))
    }

    /// Width available for document text for a known largest line number.
    ///
    /// This permits callers to calculate the viewport width before capturing
    /// the visible rows. A value of zero disables the gutter.
    pub fn text_width_for_widest_line_number(area: Rect, widest_line_number: u32) -> u16 {
        let digits = if widest_line_number == 0 {
            0
        } else {
            widest_line_number.to_string().len()
        };
        area.width
            .saturating_sub(Self::gutter_width_for_digits(area, digits))
    }

    /// Maps a terminal body cell to a UTF-8 byte position in a captured display line.
    ///
    /// Cells in the gutter, status row, outside `area`, or below the captured text
    /// do not identify an editor position.
    pub fn text_position_at(&self, area: Rect, position: Position) -> Option<TextPosition> {
        self.text_position_and_column_at(area, position)
            .map(|(position, _)| position)
    }

    /// Maps a terminal body cell to both its source position and unclipped
    /// document-cell column. The latter remains past EOL and is used to keep a
    /// rectangular selection aligned across short lines.
    pub fn text_position_and_column_at(
        &self,
        area: Rect,
        position: Position,
    ) -> Option<(TextPosition, usize)> {
        if area.width == 0 || area.height <= 1 {
            return None;
        }

        let body_bottom = area.y.saturating_add(area.height - 1);
        if position.y < area.y || position.y >= body_bottom {
            return None;
        }

        let text_area = self.text_area(area);
        if position.x < text_area.x || position.x >= text_area.right() {
            return None;
        }

        let screen_row = usize::from(position.y - area.y);
        let row = self.snapshot.viewport.top_row.checked_add(screen_row)?;
        let line = self.snapshot.line(row)?;
        let screen_column = usize::from(position.x - text_area.x);
        let target_column = self
            .snapshot
            .viewport
            .left_column
            .checked_add(screen_column)?;
        let viewport_left = self.snapshot.viewport.left_column;
        let viewport_right = viewport_left.saturating_add(usize::from(text_area.width));

        let mut byte_column = 0usize;
        let mut terminal_column = 0usize;
        for grapheme in Span::raw(line).styled_graphemes(Style::default()) {
            let start_byte = byte_column;
            byte_column = byte_column.saturating_add(grapheme.symbol.len());
            let width = usize::from(grapheme.symbol.cell_width());
            if width == 0 {
                continue;
            }

            let end_column = terminal_column.saturating_add(width);
            if target_column < end_column {
                // render_line leaves a grapheme blank when a viewport boundary
                // cuts through it.  Do not make that blank cell clickable.
                if terminal_column < viewport_left || end_column > viewport_right {
                    return None;
                }
                let offset = target_column.saturating_sub(terminal_column);
                let byte_column = if offset.saturating_mul(2) < width {
                    start_byte
                } else {
                    byte_column
                };
                return Some((TextPosition { row, byte_column }, target_column));
            }
            terminal_column = end_column;
        }

        Some((
            TextPosition {
                row,
                byte_column: line.len(),
            },
            target_column,
        ))
    }

    fn line_number_digits(&self) -> usize {
        self.snapshot
            .line_numbers
            .iter()
            .filter_map(|number| *number)
            .chain(
                (self.snapshot.widest_line_number > 0).then_some(self.snapshot.widest_line_number),
            )
            .max()
            .map_or(0, |number| number.to_string().len())
    }

    fn desired_gutter_width_for_digits(digits: usize) -> u16 {
        if digits == 0 {
            0
        } else {
            u16::try_from(digits.saturating_add(1)).unwrap_or(u16::MAX)
        }
    }

    fn gutter_width_for_digits(area: Rect, digits: usize) -> u16 {
        let desired = Self::desired_gutter_width_for_digits(digits);
        // Keep at least one cell for editable text on very narrow terminals.
        if desired < area.width { desired } else { 0 }
    }

    fn gutter_width(&self, area: Rect) -> u16 {
        Self::gutter_width_for_digits(area, self.line_number_digits())
    }

    fn text_area(&self, area: Rect) -> Rect {
        let gutter_width = self.gutter_width(area);
        Rect::new(
            area.x.saturating_add(gutter_width),
            area.y,
            area.width.saturating_sub(gutter_width),
            area.height,
        )
    }

    /// Maps the document cursor to a terminal position when it is visible.
    pub fn cursor_position(&self, area: Rect) -> Option<Position> {
        if area.width == 0 || area.height == 0 {
            return None;
        }

        if let Some(column) = self.snapshot.status_cursor_column {
            let column = column.min(usize::from(area.width - 1));
            let x = area.x.checked_add(u16::try_from(column).ok()?)?;
            let y = area.y.checked_add(area.height - 1)?;
            return Some(Position::new(x, y));
        }

        if area.height <= 1 {
            return None;
        }

        let text_area = self.text_area(area);
        if text_area.width == 0 {
            return None;
        }

        let cursor = self.snapshot.cursor?;
        self.snapshot.line(cursor.row)?;

        let row = cursor.row.checked_sub(self.snapshot.viewport.top_row)?;
        let column = cursor
            .column
            .checked_sub(self.snapshot.viewport.left_column)?;

        if row >= usize::from(text_area.height - 1) || column >= usize::from(text_area.width) {
            return None;
        }

        let x = text_area.x.checked_add(u16::try_from(column).ok()?)?;
        let y = text_area.y.checked_add(u16::try_from(row).ok()?)?;
        Some(Position::new(x, y))
    }

    fn status_text(&self) -> String {
        let cursor = self.snapshot.cursor.map(|cursor| {
            let line = self
                .snapshot
                .cursor_line_number
                .or_else(|| self.snapshot.line_number(cursor.row))
                .map(|line| line.to_string())
                .unwrap_or_else(|| cursor.row.saturating_add(1).to_string());
            let count = self.snapshot.secondary_cursors.len().saturating_add(1);
            if count > 1 {
                format!(
                    "Ln {line}, Col {}  {count} cursors",
                    cursor.column.saturating_add(1)
                )
            } else {
                format!("Ln {line}, Col {}", cursor.column.saturating_add(1))
            }
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

    let widget = EditorWidget::new(snapshot);
    let body_height = area.height.saturating_sub(1);
    let body_rect = Rect::new(area.x, area.y, area.width, body_height);
    let body_area = body_rect.intersection(clipped);
    let gutter_width = widget.gutter_width(area);
    let gutter_rect = Rect::new(area.x, area.y, gutter_width, body_height);
    let gutter_area = gutter_rect.intersection(clipped);
    let mut text_rect = widget.text_area(area);
    text_rect.height = body_height;
    let text_area = text_rect.intersection(clipped);
    let line_number_digits = widget.line_number_digits();
    buf.set_style(body_area, snapshot.text_style);
    buf.set_style(gutter_area, snapshot.gutter_style);

    for y in body_area.y..body_area.bottom() {
        let screen_row = usize::from(y.saturating_sub(area.y));
        let document_row = snapshot.viewport.top_row.saturating_add(screen_row);
        let Some(local_row) = snapshot.local_row_index(document_row) else {
            continue;
        };
        let Some(line) = snapshot.lines.get(local_row) else {
            continue;
        };

        if gutter_width > 0
            && let Some(number) = snapshot.line_numbers.get(local_row).copied().flatten()
        {
            render_line(
                &format!("{number:>line_number_digits$} "),
                snapshot.gutter_style,
                &[],
                y,
                gutter_rect,
                gutter_area,
                0,
                buf,
            );
        }

        render_line(
            line,
            snapshot.text_style,
            snapshot
                .line_styles
                .get(local_row)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            y,
            text_rect,
            text_area,
            snapshot.viewport.left_column,
            buf,
        );
        render_background_row(snapshot, document_row, line, y, text_rect, text_area, buf);
        render_cell_decorations(snapshot, document_row, y, text_rect, text_area, buf);
        render_inline_annotations(snapshot, document_row, y, text_rect, text_area, buf);
        render_selection_row(snapshot, document_row, line, y, text_rect, text_area, buf);
    }

    render_secondary_cursors(snapshot, text_rect, text_area, buf);

    if let Some(overlay) = &snapshot.overlay {
        render_overlay(overlay, body_rect, clipped, snapshot.text_style, buf);
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

fn render_inline_annotations(
    snapshot: &RenderSnapshot,
    document_row: usize,
    y: u16,
    area: Rect,
    clipped: Rect,
    buf: &mut Buffer,
) {
    if area.is_empty() || clipped.is_empty() {
        return;
    }

    let viewport_left = snapshot.viewport.left_column;
    let viewport_right = viewport_left.saturating_add(usize::from(area.width));
    for annotation in snapshot
        .inline_annotations
        .iter()
        .filter(|annotation| annotation.row == document_row)
    {
        let annotation_end = annotation
            .column
            .saturating_add(usize::from(annotation.text.cell_width()));
        if annotation_end <= viewport_left || annotation.column >= viewport_right {
            continue;
        }
        let visible_start = annotation.column.max(viewport_left);
        let screen_column = visible_start - viewport_left;
        let Some(x) = u16::try_from(screen_column)
            .ok()
            .and_then(|column| area.x.checked_add(column))
        else {
            continue;
        };
        let width = area
            .width
            .saturating_sub(u16::try_from(screen_column).unwrap_or(area.width));
        if width == 0 {
            continue;
        }
        let annotation_area = Rect::new(x, y, width, 1);
        render_line(
            &annotation.text,
            annotation.style,
            &[],
            y,
            annotation_area,
            clipped,
            visible_start - annotation.column,
            buf,
        );
    }
}

fn render_cell_decorations(
    snapshot: &RenderSnapshot,
    document_row: usize,
    y: u16,
    area: Rect,
    clipped: Rect,
    buf: &mut Buffer,
) {
    if area.is_empty() || clipped.is_empty() {
        return;
    }

    let viewport_left = snapshot.viewport.left_column;
    let viewport_right = viewport_left.saturating_add(usize::from(area.width));
    for decoration in snapshot
        .cell_decorations
        .iter()
        .filter(|decoration| decoration.row == document_row)
    {
        if decoration.column < viewport_left || decoration.column >= viewport_right {
            continue;
        }
        let screen_column = decoration.column - viewport_left;
        let Some(x) = u16::try_from(screen_column)
            .ok()
            .and_then(|column| area.x.checked_add(column))
        else {
            continue;
        };
        if x < clipped.x || x >= clipped.right() || y < clipped.y || y >= clipped.bottom() {
            continue;
        }
        let Some(cell) = buf.cell_mut((x, y)) else {
            continue;
        };
        let blank = cell.symbol() == " ";
        if blank {
            if let Some(glyph) = decoration.glyph.as_deref() {
                if glyph.cell_width() == 1 {
                    cell.set_symbol(glyph);
                }
            }
            cell.set_style(decoration.style);
        } else if decoration.style_on_text {
            cell.set_style(decoration.style);
        }
    }
}

fn render_secondary_cursors(
    snapshot: &RenderSnapshot,
    area: Rect,
    clipped: Rect,
    buf: &mut Buffer,
) {
    if area.is_empty() || clipped.is_empty() {
        return;
    }
    let body_height = usize::from(area.height);
    let body_width = usize::from(area.width);
    for cursor in &snapshot.secondary_cursors {
        if snapshot.line(cursor.row).is_none() {
            continue;
        }
        let Some(row) = cursor.row.checked_sub(snapshot.viewport.top_row) else {
            continue;
        };
        let Some(column) = cursor.column.checked_sub(snapshot.viewport.left_column) else {
            continue;
        };
        if row >= body_height || column >= body_width {
            continue;
        }
        let Some(x) = u16::try_from(column)
            .ok()
            .and_then(|column| area.x.checked_add(column))
        else {
            continue;
        };
        let Some(y) = u16::try_from(row)
            .ok()
            .and_then(|row| area.y.checked_add(row))
        else {
            continue;
        };
        let cursor_area = Rect::new(x, y, 1, 1).intersection(clipped);
        if !cursor_area.is_empty() {
            buf.set_style(
                cursor_area,
                Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD),
            );
        }
    }
}

fn render_overlay(
    overlay: &OverlaySnapshot,
    body: Rect,
    clipped: Rect,
    base_style: Style,
    buf: &mut Buffer,
) {
    if body.width < 4 || body.height < 3 {
        return;
    }

    let content_width = overlay
        .rows
        .iter()
        .map(|row| usize::from(row.text.cell_width()).saturating_add(2))
        .chain([usize::from(overlay.title.cell_width())])
        .max()
        .unwrap_or_default();
    let desired_width = u16::try_from(content_width.saturating_add(4)).unwrap_or(u16::MAX);
    let width = desired_width.clamp(4, body.width);
    let visible_rows = overlay
        .rows
        .len()
        .min(usize::from(body.height.saturating_sub(2)));
    let height = u16::try_from(visible_rows.saturating_add(2))
        .unwrap_or(body.height)
        .clamp(3, body.height);
    let x = body.x.saturating_add(body.width.saturating_sub(width) / 2);
    let y = body
        .y
        .saturating_add(body.height.saturating_sub(height).min(2));
    let area = Rect::new(x, y, width, height).intersection(clipped);
    if area.width < 4 || area.height < 3 {
        return;
    }

    Clear.render(area, buf);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(overlay.title.as_str())
        .style(base_style);
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.is_empty() {
        return;
    }

    let selected = overlay
        .selected
        .filter(|selected| *selected < overlay.rows.len());
    let visible_count = usize::from(inner.height);
    let first = selected
        .map(|selected| {
            selected
                .saturating_add(1)
                .saturating_sub(visible_count)
                .min(overlay.rows.len().saturating_sub(visible_count))
        })
        .unwrap_or_default();
    for (screen_row, (row_index, row)) in overlay
        .rows
        .iter()
        .enumerate()
        .skip(first)
        .take(visible_count)
        .enumerate()
    {
        let y = inner
            .y
            .saturating_add(u16::try_from(screen_row).unwrap_or(inner.height));
        let row_area = Rect::new(inner.x, y, inner.width, 1).intersection(clipped);
        let mut style = base_style;
        if selected == Some(row_index) {
            style = style.add_modifier(Modifier::REVERSED);
        } else if !row.enabled {
            style = style.add_modifier(Modifier::DIM);
        }
        buf.set_style(row_area, style);
        let prefix = if selected == Some(row_index) {
            "› "
        } else {
            "  "
        };
        render_line(
            &format!("{prefix}{}", row.text),
            style,
            &[],
            y,
            inner,
            clipped,
            0,
            buf,
        );
    }
}

fn render_background_row(
    snapshot: &RenderSnapshot,
    document_row: usize,
    line: &str,
    y: u16,
    area: Rect,
    clipped: Rect,
    buf: &mut Buffer,
) {
    let line_width = usize::from(line.cell_width());

    for background in &snapshot.background_ranges {
        let Some(color) = background.style.bg else {
            continue;
        };
        let Some(area) = visible_range_rect(
            background.range,
            document_row,
            line_width,
            y,
            area,
            clipped,
            snapshot.viewport.left_column,
        ) else {
            continue;
        };

        buf.set_style(area, Style::new().bg(color));
    }
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
    let selection_style = Style::new().add_modifier(Modifier::REVERSED);

    for selection in &snapshot.selections {
        let Some(area) = visible_range_rect(
            *selection,
            document_row,
            line_width,
            y,
            area,
            clipped,
            snapshot.viewport.left_column,
        ) else {
            continue;
        };

        buf.set_style(area, selection_style);
    }
}

fn visible_range_rect(
    range: SelectionRange,
    document_row: usize,
    line_width: usize,
    y: u16,
    area: Rect,
    clipped: Rect,
    viewport_left: usize,
) -> Option<Rect> {
    if range.start >= range.end || document_row < range.start.row || document_row > range.end.row {
        return None;
    }

    let start = if document_row == range.start.row {
        range.start.column
    } else {
        0
    };
    let end = if document_row == range.end.row {
        range.end.column
    } else {
        // Make a highlighted newline visible, including on an empty line.
        line_width.saturating_add(1)
    };
    let viewport_right = viewport_left.saturating_add(usize::from(area.width));
    let start = start.max(viewport_left);
    let end = end.min(viewport_right);
    if start >= end {
        return None;
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
    (x_start < x_end).then(|| Rect::new(x_start, y, x_end - x_start, 1))
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
        layout::{Position, Rect},
        style::{Color, Modifier, Style},
        widgets::Widget,
    };

    use super::{
        BackgroundRange, CellDecoration, Cursor, EditorWidget, OverlayPanelWidget, OverlayRow,
        OverlaySnapshot, RenderSnapshot, SelectionRange, StyleSpan, TextPosition, Viewport,
    };

    fn row(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.right())
            .map(|x| buf.cell((x, y)).expect("cell inside buffer").symbol())
            .collect()
    }

    #[test]
    fn renders_scrolled_body_and_status() {
        let snapshot = RenderSnapshot {
            first_row: 0,
            total_rows: 3,
            lines: vec!["zero".into(), "one".into(), "two".into()],
            line_numbers: Vec::new(),
            widest_line_number: 0,
            cursor: Some(Cursor { row: 2, column: 1 }),
            secondary_cursors: Vec::new(),
            cursor_line_number: None,
            selections: Vec::new(),
            text_style: Style::default(),
            gutter_style: Style::default(),
            line_styles: Vec::new(),
            background_ranges: Vec::new(),
            cell_decorations: Vec::new(),
            inline_annotations: Vec::new(),
            viewport: Viewport {
                top_row: 1,
                left_column: 0,
            },
            status: "NORMAL".into(),
            status_cursor_column: None,
            overlay: None,
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
    fn renders_secondary_cursors_and_reports_the_total_cursor_count() {
        let snapshot = RenderSnapshot {
            first_row: 0,
            total_rows: 1,
            lines: vec!["abc".into()],
            cursor: Some(Cursor { row: 0, column: 0 }),
            secondary_cursors: vec![Cursor { row: 0, column: 2 }],
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 24, 2);
        let mut buf = Buffer::empty(area);

        EditorWidget::new(&snapshot).render(area, &mut buf);

        let cursor = buf.cell((2, 0)).expect("secondary cursor cell");
        assert!(cursor.modifier.contains(Modifier::REVERSED));
        assert!(cursor.modifier.contains(Modifier::BOLD));
        assert!(row(&buf, 1).contains("2 cursors"));
    }

    #[test]
    fn renders_right_aligned_line_numbers_with_scroll_and_unnumbered_rows() {
        let snapshot = RenderSnapshot {
            lines: vec![
                "ten".into(),
                "block".into(),
                "forty-two".into(),
                "hundred-five".into(),
            ],
            line_numbers: vec![Some(10), None, Some(42), Some(105)],
            widest_line_number: 999,
            cursor: Some(Cursor { row: 3, column: 1 }),
            text_style: Style::new().fg(Color::White),
            gutter_style: Style::new().fg(Color::Yellow),
            viewport: Viewport {
                top_row: 1,
                left_column: 0,
            },
            status: "NORMAL".into(),
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 30, 4);
        let mut buf = Buffer::empty(area);

        EditorWidget::new(&snapshot).render(area, &mut buf);

        assert!(row(&buf, 0).starts_with("    block"));
        assert!(row(&buf, 1).starts_with(" 42 forty-two"));
        assert!(row(&buf, 2).starts_with("105 hundred-five"));
        assert!(row(&buf, 3).starts_with("NORMAL  Ln 105, Col 2"));
        assert_eq!(buf.cell((2, 1)).expect("line number").fg, Color::Yellow);
        assert_eq!(buf.cell((4, 1)).expect("document text").fg, Color::White);
    }

    #[test]
    fn maps_terminal_cells_to_display_byte_positions() {
        let snapshot = RenderSnapshot {
            lines: vec!["zero".into(), "a界e\u{301}".into(), "tail".into()],
            line_numbers: vec![Some(1), Some(2), Some(3)],
            widest_line_number: 99,
            viewport: Viewport {
                top_row: 1,
                left_column: 1,
            },
            ..RenderSnapshot::default()
        };
        let area = Rect::new(10, 5, 8, 4);
        let widget = EditorWidget::new(&snapshot);

        assert_eq!(
            widget.text_position_at(area, Position::new(13, 5)),
            Some(TextPosition {
                row: 1,
                byte_column: 1,
            })
        );
        assert_eq!(
            widget.text_position_at(area, Position::new(14, 5)),
            Some(TextPosition {
                row: 1,
                byte_column: 4,
            })
        );
        assert_eq!(
            widget.text_position_at(area, Position::new(15, 5)),
            Some(TextPosition {
                row: 1,
                byte_column: 4,
            })
        );
        assert_eq!(
            widget.text_position_at(area, Position::new(16, 5)),
            Some(TextPosition {
                row: 1,
                byte_column: 7,
            })
        );
        assert_eq!(
            widget.text_position_at(area, Position::new(13, 6)),
            Some(TextPosition {
                row: 2,
                byte_column: 1,
            })
        );
    }

    #[test]
    fn text_hit_testing_rejects_gutter_status_and_rows_after_end() {
        let snapshot = RenderSnapshot {
            lines: vec!["only".into()],
            line_numbers: vec![Some(1)],
            widest_line_number: 99,
            ..RenderSnapshot::default()
        };
        let area = Rect::new(10, 5, 8, 4);
        let widget = EditorWidget::new(&snapshot);

        assert_eq!(widget.text_position_at(area, Position::new(12, 5)), None);
        assert_eq!(widget.text_position_at(area, Position::new(13, 6)), None);
        assert_eq!(widget.text_position_at(area, Position::new(13, 8)), None);
        assert_eq!(widget.text_position_at(area, Position::new(9, 5)), None);
        assert_eq!(widget.text_position_at(area, Position::new(18, 5)), None);
    }

    #[test]
    fn text_hit_testing_rejects_wide_graphemes_cut_by_the_viewport() {
        let left_cut = RenderSnapshot {
            lines: vec!["a界b".into()],
            viewport: Viewport {
                top_row: 0,
                left_column: 2,
            },
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 4, 2);
        assert_eq!(
            EditorWidget::new(&left_cut).text_position_at(area, Position::new(0, 0)),
            None
        );

        let right_cut = RenderSnapshot {
            lines: vec!["a界".into()],
            ..RenderSnapshot::default()
        };
        let narrow_area = Rect::new(0, 0, 2, 2);
        assert_eq!(
            EditorWidget::new(&right_cut).text_position_at(narrow_area, Position::new(1, 0)),
            None
        );
    }

    #[test]
    fn offsets_horizontal_viewport_selection_and_cursor_by_gutter() {
        let snapshot = RenderSnapshot {
            lines: vec!["ab界cd".into()],
            line_numbers: vec![Some(1)],
            widest_line_number: 1,
            cursor: Some(Cursor { row: 0, column: 4 }),
            selections: vec![SelectionRange {
                start: Cursor { row: 0, column: 2 },
                end: Cursor { row: 0, column: 4 },
            }],
            viewport: Viewport {
                top_row: 0,
                left_column: 2,
            },
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 6, 2);
        let mut buf = Buffer::empty(area);
        let widget = EditorWidget::new(&snapshot);

        assert_eq!(widget.text_width(area), 4);
        assert_eq!(widget.cursor_position(area), Some((4, 0).into()));
        widget.render(area, &mut buf);

        assert_eq!(buf.cell((0, 0)).expect("line number").symbol(), "1");
        assert_eq!(buf.cell((2, 0)).expect("wide grapheme").symbol(), "界");
        assert_eq!(buf.cell((4, 0)).expect("first trailing cell").symbol(), "c");
        assert_eq!(buf.cell((5, 0)).expect("last trailing cell").symbol(), "d");
        for x in [2, 3] {
            assert!(
                buf.cell((x, 0))
                    .expect("selected wide grapheme cell")
                    .modifier
                    .contains(Modifier::REVERSED)
            );
        }
        assert!(
            !buf.cell((0, 0))
                .expect("unselected gutter")
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn hides_gutter_when_terminal_is_too_narrow() {
        let snapshot = RenderSnapshot {
            lines: vec!["abc".into()],
            line_numbers: vec![Some(1)],
            widest_line_number: 999,
            cursor: Some(Cursor { row: 0, column: 2 }),
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 3, 2);
        let mut buf = Buffer::empty(area);
        let widget = EditorWidget::new(&snapshot);

        assert_eq!(widget.text_width(area), 3);
        assert_eq!(widget.cursor_position(area), Some((2, 0).into()));
        widget.render(area, &mut buf);

        assert_eq!(row(&buf, 0), "abc");
    }

    #[test]
    fn layers_multiline_backgrounds_between_syntax_and_selection() {
        let snapshot = RenderSnapshot {
            lines: vec!["a界e\u{301}z".into(), String::new(), "xyz".into()],
            line_numbers: vec![Some(1), Some(2), Some(3)],
            widest_line_number: 3,
            selections: vec![SelectionRange {
                start: Cursor { row: 0, column: 3 },
                end: Cursor { row: 1, column: 0 },
            }],
            text_style: Style::new().fg(Color::White).bg(Color::Black),
            line_styles: vec![
                vec![StyleSpan {
                    start_column: 1,
                    end_column: 4,
                    style: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                }],
                Vec::new(),
                Vec::new(),
            ],
            background_ranges: vec![BackgroundRange {
                range: SelectionRange {
                    start: Cursor { row: 0, column: 1 },
                    end: Cursor { row: 2, column: 2 },
                },
                style: Style::new()
                    .fg(Color::Green)
                    .bg(Color::Blue)
                    .add_modifier(Modifier::ITALIC),
            }],
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 10, 4);
        let mut buf = Buffer::empty(area);

        EditorWidget::new(&snapshot).render(area, &mut buf);

        let wide = buf.cell((3, 0)).expect("wide highlighted grapheme");
        assert_eq!(wide.symbol(), "界");
        assert_eq!(wide.fg, Color::Red);
        assert_eq!(wide.bg, Color::Blue);
        assert!(wide.modifier.contains(Modifier::BOLD));
        assert!(!wide.modifier.contains(Modifier::ITALIC));

        let combining = buf.cell((5, 0)).expect("combining grapheme");
        assert_eq!(combining.symbol(), "e\u{301}");
        assert_eq!(combining.bg, Color::Blue);
        assert!(combining.modifier.contains(Modifier::REVERSED));

        let selected_newline = buf.cell((7, 0)).expect("selected newline");
        assert_eq!(selected_newline.bg, Color::Blue);
        assert!(selected_newline.modifier.contains(Modifier::REVERSED));

        let empty_line_newline = buf.cell((2, 1)).expect("empty-line newline");
        assert_eq!(empty_line_newline.bg, Color::Blue);
        assert!(!empty_line_newline.modifier.contains(Modifier::REVERSED));
        assert_eq!(buf.cell((2, 2)).expect("range end row").bg, Color::Blue);
        assert_eq!(buf.cell((3, 2)).expect("range end row").bg, Color::Blue);
        assert_eq!(buf.cell((4, 2)).expect("past range end").bg, Color::Black);
        assert_ne!(buf.cell((1, 0)).expect("gutter padding").bg, Color::Blue);
    }

    #[test]
    fn clips_backgrounds_with_gutter_and_horizontal_viewport() {
        let snapshot = RenderSnapshot {
            lines: vec!["a界bc".into()],
            line_numbers: vec![Some(1)],
            widest_line_number: 1,
            text_style: Style::new().bg(Color::Black),
            background_ranges: vec![BackgroundRange {
                range: SelectionRange {
                    start: Cursor { row: 0, column: 1 },
                    end: Cursor { row: 0, column: 4 },
                },
                style: Style::new().bg(Color::Blue),
            }],
            viewport: Viewport {
                top_row: 0,
                left_column: 2,
            },
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 5, 2);
        let mut buf = Buffer::empty(area);

        EditorWidget::new(&snapshot).render(area, &mut buf);

        assert_eq!(buf.cell((2, 0)).expect("cut wide cell").symbol(), " ");
        assert_eq!(buf.cell((2, 0)).expect("cut wide cell").bg, Color::Blue);
        assert_eq!(buf.cell((3, 0)).expect("visible match cell").symbol(), "b");
        assert_eq!(
            buf.cell((3, 0)).expect("visible match cell").bg,
            Color::Blue
        );
        assert_eq!(buf.cell((4, 0)).expect("past match").symbol(), "c");
        assert_eq!(buf.cell((4, 0)).expect("past match").bg, Color::Black);
        assert_ne!(buf.cell((0, 0)).expect("gutter").bg, Color::Blue);
    }

    #[test]
    fn status_cursor_takes_priority_and_clamps_to_the_status_row() {
        let mut snapshot = RenderSnapshot {
            lines: vec![String::new(); 5],
            cursor: Some(Cursor { row: 2, column: 2 }),
            status_cursor_column: Some(3),
            ..RenderSnapshot::default()
        };
        let area = Rect::new(10, 5, 8, 4);

        assert_eq!(
            EditorWidget::new(&snapshot).cursor_position(area),
            Some((13, 8).into())
        );

        snapshot.status_cursor_column = Some(usize::MAX);
        assert_eq!(
            EditorWidget::new(&snapshot).cursor_position(area),
            Some((17, 8).into())
        );
        assert_eq!(
            EditorWidget::new(&snapshot).cursor_position(Rect::new(2, 7, 4, 1)),
            Some((5, 7).into())
        );
        assert_eq!(
            EditorWidget::new(&snapshot).cursor_position(Rect::new(0, 0, 0, 1)),
            None
        );
        assert_eq!(
            EditorWidget::new(&snapshot).cursor_position(Rect::new(0, 0, 1, 0)),
            None
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
            first_row: 0,
            total_rows: 5,
            lines: vec![String::new(); 5],
            line_numbers: Vec::new(),
            widest_line_number: 0,
            cursor: Some(Cursor { row: 3, column: 7 }),
            secondary_cursors: Vec::new(),
            cursor_line_number: None,
            selections: Vec::new(),
            text_style: Style::default(),
            gutter_style: Style::default(),
            line_styles: Vec::new(),
            background_ranges: Vec::new(),
            cell_decorations: Vec::new(),
            inline_annotations: Vec::new(),
            viewport: Viewport {
                top_row: 2,
                left_column: 4,
            },
            status: String::new(),
            status_cursor_column: None,
            overlay: None,
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

    #[test]
    fn renders_a_local_row_slice_at_global_document_positions() {
        let snapshot = RenderSnapshot {
            first_row: 40,
            total_rows: 100,
            lines: vec!["forty".into(), "forty-one".into()],
            line_numbers: vec![Some(41), Some(42)],
            widest_line_number: 100,
            cursor: Some(Cursor { row: 41, column: 1 }),
            secondary_cursors: Vec::new(),
            cursor_line_number: Some(42),
            selections: vec![SelectionRange {
                start: Cursor { row: 40, column: 1 },
                end: Cursor { row: 41, column: 2 },
            }],
            text_style: Style::new().fg(Color::White).bg(Color::Black),
            gutter_style: Style::new().fg(Color::Yellow).bg(Color::Black),
            line_styles: vec![Vec::new(), Vec::new()],
            background_ranges: vec![BackgroundRange {
                range: SelectionRange {
                    start: Cursor { row: 41, column: 0 },
                    end: Cursor { row: 41, column: 1 },
                },
                style: Style::new().bg(Color::Blue),
            }],
            cell_decorations: Vec::new(),
            inline_annotations: Vec::new(),
            viewport: Viewport {
                top_row: 40,
                left_column: 0,
            },
            status: "NORMAL".into(),
            status_cursor_column: None,
            overlay: None,
        };
        let area = Rect::new(0, 0, 24, 3);
        let mut buf = Buffer::empty(area);
        let widget = EditorWidget::new(&snapshot);

        assert_eq!(widget.cursor_position(area), Some(Position::new(5, 1)));
        widget.render(area, &mut buf);

        assert!(row(&buf, 0).starts_with(" 41 forty"));
        assert!(row(&buf, 1).starts_with(" 42 forty-one"));
        assert!(row(&buf, 2).starts_with("NORMAL  Ln 42, Col 2"));
        assert!(
            buf.cell((5, 0))
                .expect("selected first visible row")
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(
            buf.cell((4, 1)).expect("visible background range").bg,
            Color::Blue
        );
    }

    #[test]
    fn cell_decorations_preserve_text_coordinates_and_layer_below_selection() {
        let snapshot = RenderSnapshot {
            total_rows: 1,
            lines: vec!["a b界".into()],
            selections: vec![SelectionRange {
                start: Cursor { row: 0, column: 1 },
                end: Cursor { row: 0, column: 2 },
            }],
            text_style: Style::new().fg(Color::White).bg(Color::Black),
            cell_decorations: vec![
                CellDecoration {
                    row: 0,
                    column: 1,
                    glyph: Some("·".into()),
                    style: Style::new().fg(Color::Yellow),
                    style_on_text: false,
                },
                CellDecoration {
                    row: 0,
                    column: 0,
                    glyph: Some("│".into()),
                    style: Style::new().fg(Color::Red),
                    style_on_text: false,
                },
                CellDecoration {
                    row: 0,
                    column: 3,
                    glyph: Some("│".into()),
                    style: Style::new().bg(Color::Blue),
                    style_on_text: true,
                },
                CellDecoration {
                    row: 0,
                    column: 5,
                    glyph: Some("│".into()),
                    style: Style::new().fg(Color::Green),
                    style_on_text: true,
                },
            ],
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 8, 2);
        let mut buf = Buffer::empty(area);
        let widget = EditorWidget::new(&snapshot);

        widget.render(area, &mut buf);

        assert_eq!(buf.cell((1, 0)).expect("space marker").symbol(), "·");
        assert_eq!(buf.cell((1, 0)).expect("space marker").fg, Color::Yellow);
        assert!(
            buf.cell((1, 0))
                .expect("selected marker")
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(
            buf.cell((0, 0)).expect("occupied source cell").symbol(),
            "a"
        );
        assert_eq!(
            buf.cell((0, 0)).expect("occupied source cell").fg,
            Color::White
        );
        assert_eq!(buf.cell((3, 0)).expect("wide source cell").symbol(), "界");
        assert_eq!(
            buf.cell((3, 0)).expect("styled source cell").bg,
            Color::Blue
        );
        assert_eq!(buf.cell((5, 0)).expect("blank guide cell").symbol(), "│");
        assert_eq!(
            widget.text_position_at(area, Position::new(1, 0)),
            Some(TextPosition {
                row: 0,
                byte_column: 1,
            })
        );
    }

    #[test]
    fn inline_annotations_clip_without_entering_source_text() {
        let snapshot = RenderSnapshot {
            total_rows: 1,
            lines: vec!["abc".into()],
            text_style: Style::new().fg(Color::White).bg(Color::Black),
            inline_annotations: vec![super::InlineAnnotation {
                row: 0,
                column: 5,
                text: "error".into(),
                style: Style::new().fg(Color::Red).bg(Color::Blue),
            }],
            viewport: Viewport {
                top_row: 0,
                left_column: 6,
            },
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 4, 2);
        let mut buf = Buffer::empty(area);
        let widget = EditorWidget::new(&snapshot);

        widget.render(area, &mut buf);

        assert_eq!(row(&buf, 0), "rror");
        assert_eq!(buf.cell((0, 0)).expect("clipped annotation").fg, Color::Red);
        assert_eq!(
            widget.text_position_at(area, Position::new(0, 0)),
            Some(TextPosition {
                row: 0,
                byte_column: 3,
            })
        );
    }

    #[test]
    fn reports_the_global_cursor_line_while_cursor_is_offscreen() {
        let snapshot = RenderSnapshot {
            first_row: 50,
            total_rows: 100,
            lines: vec!["fifty".into(), "fifty-one".into()],
            cursor: Some(Cursor { row: 7, column: 2 }),
            cursor_line_number: Some(8),
            viewport: Viewport {
                top_row: 50,
                left_column: 0,
            },
            status: "NORMAL".into(),
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 24, 3);

        assert_eq!(EditorWidget::new(&snapshot).cursor_position(area), None);
        assert_eq!(
            EditorWidget::new(&snapshot).status_text(),
            "NORMAL  Ln 8, Col 3"
        );
    }

    #[test]
    fn hit_testing_maps_local_rows_back_to_global_display_rows() {
        let snapshot = RenderSnapshot {
            first_row: 20,
            total_rows: 100,
            lines: vec!["aa".into(), "a界".into()],
            viewport: Viewport {
                top_row: 20,
                left_column: 0,
            },
            ..RenderSnapshot::default()
        };
        let area = Rect::new(10, 5, 8, 3);
        let widget = EditorWidget::new(&snapshot);

        assert_eq!(
            widget.text_position_at(area, Position::new(12, 6)),
            Some(TextPosition {
                row: 21,
                byte_column: 4,
            })
        );
    }

    #[test]
    fn overlays_are_bounded_virtualized_and_mark_the_selected_row() {
        let snapshot = RenderSnapshot {
            lines: vec!["underlay".into(); 20],
            total_rows: 20,
            text_style: Style::new().fg(Color::White).bg(Color::Black),
            overlay: Some(OverlaySnapshot {
                title: "Commands".into(),
                rows: (0..10)
                    .map(|index| OverlayRow {
                        text: format!("action {index}"),
                        enabled: index != 8,
                    })
                    .collect(),
                selected: Some(8),
            }),
            ..RenderSnapshot::default()
        };
        let area = Rect::new(0, 0, 24, 7);
        let mut buf = Buffer::empty(area);

        EditorWidget::new(&snapshot).render(area, &mut buf);

        assert!(row(&buf, 0).contains("Commands"));
        assert!(row(&buf, 4).contains("› action 8"));
        let selected_x = row(&buf, 4).find('›').expect("selected marker") as u16;
        assert!(
            buf.cell((selected_x, 4))
                .expect("selected overlay cell")
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert!(row(&buf, 5).contains('└'));
    }

    #[test]
    fn overlay_panel_uses_bordered_rows_for_rendering_and_hit_testing() {
        let snapshot = OverlaySnapshot {
            title: " Diagnostics ".into(),
            rows: vec![
                OverlayRow {
                    text: "W src/main.rs:1:4 warning".into(),
                    enabled: true,
                },
                OverlayRow {
                    text: "collecting".into(),
                    enabled: false,
                },
            ],
            selected: Some(0),
        };
        let area = Rect::new(4, 3, 32, 5);
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));

        OverlayPanelWidget::new(&snapshot, true).render(area, &mut buf);

        assert!(row(&buf, 3).contains("Diagnostics"));
        assert!(row(&buf, 4).contains("› W src/main.rs:1:4 warning"));
        assert!(
            buf.cell((5, 4))
                .expect("selected diagnostics row")
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(OverlayPanelWidget::row_budget(area), 3);
        assert_eq!(
            OverlayPanelWidget::row_at(area, Position::new(10, 4)),
            Some(0)
        );
        assert_eq!(OverlayPanelWidget::row_at(area, Position::new(10, 3)), None);
    }
}
