//! Hidden Editor windows and the projection of their snapshots to cells.
//!
//! Zed's display columns are UTF-8 bytes and the terminal's are grapheme
//! cell widths. This module is the one place that converts between them;
//! the mouse hit test in the renderer is its inverse.

use anyhow::{Context as _, Result};
use async_channel::Sender;
use editor::{
    DisplayPoint, Editor, EditorStyle, SelectionEffects, SoftWrap as EditorSoftWrap,
    display_map::{DisplayRow, DisplaySnapshot},
};
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Context, Entity, Focusable as _, Keystroke,
    Window, WindowBounds, WindowHandle, WindowOptions,
};
use language::{Buffer, BufferEvent, LanguageAwareStyling};
use multi_buffer::Anchor;
use project::{Project, buffer_store::BufferStore};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
};
use text::Bias;
use theme::ActiveTheme as _;
use unicode_width::UnicodeWidthStr as _;

use super::Event;
use crate::terminal::render::{
    BackgroundRange, Cursor, EditorWidget, Follow, OverlaySnapshot, RenderSnapshot, SelectionRange,
    StyleSpan, TextPosition, Viewport,
};

/// Opens a hidden GPUI window rooted at an Editor for `buffer`.
pub fn open_window(
    buffer: Entity<Buffer>,
    project: Option<Entity<Project>>,
    cx: &mut App,
) -> Result<WindowHandle<Editor>> {
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(gpui::Bounds {
                origin: Default::default(),
                size: gpui::size(gpui::px(800.0), gpui::px(600.0)),
            })),
            focus: false,
            show: false,
            ..Default::default()
        },
        |window, cx| {
            let editor = cx.new(|cx| Editor::for_buffer(buffer, project, window, cx));
            editor.update(cx, |editor, cx| {
                editor.set_soft_wrap_mode(settings::SoftWrap::None, cx);
                editor
                    .display_map
                    .update(cx, |map, cx| map.set_wrap_width(None, cx));
            });
            window.focus(&editor.focus_handle(cx), cx);
            editor
        },
    )
    .context("GPUI could not create the hidden window")
}

pub fn close_window(window: &WindowHandle<Editor>, cx: &mut AsyncApp) -> Result<()> {
    window
        .update(cx, |_editor, window, _cx| window.remove_window())
        .context("close editor window")
}

/// Forwards Editor repaints and buffer changes as events. A clean buffer's
/// external change reloads through the store; the completion is reported so
/// the status can say so.
pub fn subscribe<T>(
    window: &WindowHandle<Editor>,
    buffer: &Entity<Buffer>,
    buffer_store: Entity<BufferStore>,
    sender: Sender<T>,
    cx: &mut AsyncApp,
) -> Result<()>
where
    T: From<Event> + Send + 'static,
{
    let buffer_id = buffer.read_with(cx, |buffer, _| buffer.remote_id().to_proto());
    window
        .update(cx, |_editor, _window, cx| {
            let redraw = sender.clone();
            cx.observe_self(move |_editor, _cx| {
                let _ = redraw.try_send(Event::Redraw.into());
            })
            .detach();
            cx.subscribe(buffer, move |_editor, buffer, event, cx| match event {
                BufferEvent::ReloadNeeded => {
                    let reload = buffer_store.update(cx, |store, cx| {
                        store.reload_buffers([buffer.clone()].into_iter().collect(), true, cx)
                    });
                    let sender = sender.clone();
                    cx.spawn(async move |_, _| {
                        let result = reload
                            .await
                            .map(|_| ())
                            .map_err(|error| format!("{error:#}"));
                        let _ = sender
                            .send(Event::ReloadFinished { buffer_id, result }.into())
                            .await;
                    })
                    .detach();
                }
                BufferEvent::LanguageChanged(_)
                | BufferEvent::Reparsed
                | BufferEvent::FileHandleChanged
                | BufferEvent::Reloaded
                | BufferEvent::DirtyChanged
                | BufferEvent::Saved
                | BufferEvent::CapabilityChanged => {
                    let _ = sender.try_send(Event::Redraw.into());
                }
                _ => {}
            })
            .detach();
        })
        .context("observe editor and buffer")
}

/// Sends a keystroke through the window so Zed's keymap resolves it.
pub fn dispatch_keystroke(
    window: &WindowHandle<Editor>,
    keystroke: Keystroke,
    cx: &mut AsyncApp,
) -> Result<()> {
    let window: AnyWindowHandle = (*window).into();
    cx.update_window(window, |_root, window, cx| {
        window.dispatch_keystroke(keystroke, cx);
    })
    .context("dispatch keystroke")
}

pub fn dispatch_action(
    window: &WindowHandle<Editor>,
    action: Box<dyn gpui::Action>,
    cx: &mut AsyncApp,
) -> Result<()> {
    window
        .update(cx, |_editor, window, cx| window.dispatch_action(action, cx))
        .context("dispatch action")
}

/// Bracketed paste goes straight to Zed's paste path so selection
/// replacement, auto-indent, and undo granularity stay Zed's.
pub fn paste(window: &WindowHandle<Editor>, text: &str, cx: &mut AsyncApp) -> Result<()> {
    window
        .update(cx, |editor, window, cx| {
            editor.do_paste(&text.to_owned(), None, true, window, cx);
        })
        .context("paste text")
}

/// Collapses the selection to the caret at a display position hit-tested
/// from the previous frame.
pub fn place_caret(
    window: &WindowHandle<Editor>,
    position: TextPosition,
    cx: &mut AsyncApp,
) -> Result<()> {
    window
        .update(cx, |editor, window, cx| {
            let display = editor.display_snapshot(cx);
            let Some(point) = display_point_at(&display, position) else {
                return;
            };
            let anchor = display.display_point_to_anchor(point, Bias::Left);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_anchor_ranges(vec![anchor..anchor]);
            });
        })
        .context("place caret")
}

/// Collapses the selection to the caret at a buffer point, clipped to the
/// current text.
pub fn place_caret_at_point(
    window: &WindowHandle<Editor>,
    point: text::Point,
    cx: &mut AsyncApp,
) -> Result<()> {
    window
        .update(cx, |editor, window, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let point = snapshot.clip_point(point, Bias::Left);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([point..point]);
            });
        })
        .context("place caret at point")
}

/// Re-resolves a position from the previous frame against the current
/// snapshot, which an asynchronous reparse or reload may have changed, and
/// keeps the caret out of inlays.
fn display_point_at(display: &DisplaySnapshot, position: TextPosition) -> Option<DisplayPoint> {
    let row = u32::try_from(position.row).ok()?;
    let column = u32::try_from(position.byte_column).ok()?;
    let raw = DisplayPoint::new(DisplayRow(row), column);
    let previous = display.clip_point(raw, Bias::Left);
    let next = display.clip_point(raw, Bias::Right);
    Some(if previous == next {
        previous
    } else {
        match display.inlay_bias_at(raw) {
            Some(Bias::Left) => next,
            Some(Bias::Right) | None => previous,
        }
    })
}

/// The status row content the caller wants drawn under the document.
pub struct StatusRow {
    pub text: String,
    pub cursor_column: Option<usize>,
    pub overlay: Option<OverlaySnapshot>,
}

pub struct Capture {
    pub snapshot: RenderSnapshot,
    pub follow: Follow,
}

/// Reads only the visible rows of the display snapshot. Runs inside the
/// draw callback so the viewport cannot shift between resize and capture.
pub fn capture(
    editor: &mut Editor,
    window: &mut Window,
    cx: &mut Context<Editor>,
    mut viewport: Viewport,
    mut follow: Follow,
    area: Rect,
    status: StatusRow,
) -> Capture {
    synchronize_wrap(editor, window, cx, area);
    let style = editor.style(cx).clone();
    let display = editor.display_snapshot(cx);
    let total_rows = display.max_point().row().0 as usize + 1;
    let widest_line_number = display.widest_line_number();
    let cursor_point = editor.selections.newest_display(&display).head();
    let cursor = display_cursor_at(&display, cursor_point);
    let follow_vertical = follow.observe(cursor);
    let body_height = usize::from(area.height.saturating_sub(1));
    let text_width = EditorWidget::text_width_for_widest_line_number(area, widest_line_number);
    viewport.follow_cursor(cursor, text_width, area.height, follow_vertical);
    viewport.clamp(total_rows, body_height);

    let first_row = viewport.top_row;
    let end_row = first_row.saturating_add(body_height).min(total_rows);
    let first_display_row = DisplayRow(u32::try_from(first_row).unwrap_or(u32::MAX));
    let end_display_row = DisplayRow(u32::try_from(end_row).unwrap_or(u32::MAX));
    let lines = (first_display_row.0..end_display_row.0)
        .map(|row| display.line(DisplayRow(row)))
        .collect::<Vec<_>>();
    let line_numbers = display
        .row_infos(first_display_row)
        .take(lines.len())
        .map(|row| row.buffer_row.map(|row| row.saturating_add(1)))
        .collect();
    let cursor_line_number = display
        .row_infos(cursor_point.row())
        .next()
        .and_then(|row| row.buffer_row.or(row.wrapped_buffer_row))
        .map(|row| row.saturating_add(1));
    let visible_start = DisplayPoint::new(first_display_row, 0);
    let visible_end = (end_row < total_rows).then_some(DisplayPoint::new(end_display_row, 0));
    let adjusted = editor.selections.all_adjusted_display(&display);
    let mut secondary_cursors = adjusted
        .iter()
        .map(|selection| display_cursor_at(&display, selection.head()))
        .filter(|secondary| *secondary != cursor)
        .collect::<Vec<_>>();
    secondary_cursors.sort_unstable();
    secondary_cursors.dedup();
    let selections = adjusted
        .into_iter()
        .filter(|selection| {
            !selection.is_empty()
                && selection.end > visible_start
                && visible_end.is_none_or(|end| selection.start < end)
        })
        .map(|selection| SelectionRange {
            start: display_cursor_in_rows(&lines, first_row, selection.start),
            end: display_cursor_in_rows(&lines, first_row, selection.end),
        })
        .collect::<Vec<_>>();
    let line_styles = line_styles(
        &display,
        &style,
        first_display_row,
        end_display_row,
        lines.len(),
    );
    let mut highlights = if first_row < end_row {
        let buffer = display.buffer_snapshot();
        let start = if first_row == 0 {
            Anchor::Min
        } else {
            buffer.anchor_before(visible_start.to_offset(&display, Bias::Left))
        };
        let end = match visible_end {
            Some(end) => buffer.anchor_before(end.to_offset(&display, Bias::Right)),
            None => Anchor::Max,
        };
        editor.background_highlights_in_range(start..end, &display, cx.theme())
    } else {
        Vec::new()
    };
    highlights.sort_by(|left, right| {
        left.0
            .start
            .cmp(&right.0.start)
            .then_with(|| left.0.end.cmp(&right.0.end))
            .then_with(|| left.1.cmp(&right.1))
    });
    let background_ranges = highlights
        .into_iter()
        .map(|(range, color)| BackgroundRange {
            range: SelectionRange {
                start: display_cursor_in_rows(&lines, first_row, range.start),
                end: display_cursor_in_rows(&lines, first_row, range.end),
            },
            style: Style::new().bg(terminal_color(style.background.blend(color))),
        })
        .collect();
    let text_style = terminal_text_style(&style.text, style.background);
    let gutter_style = Style::new()
        .fg(terminal_color(
            style
                .background
                .blend(cx.theme().colors().editor_line_number),
        ))
        .bg(terminal_color(style.background));

    Capture {
        snapshot: RenderSnapshot {
            first_row,
            total_rows,
            lines,
            line_numbers,
            widest_line_number,
            cursor: Some(cursor),
            secondary_cursors,
            cursor_line_number,
            selections,
            text_style,
            gutter_style,
            line_styles,
            background_ranges,
            cell_decorations: Vec::new(),
            inline_annotations: Vec::new(),
            viewport,
            status: status.text,
            status_cursor_column: status.cursor_column,
            overlay: status.overlay,
        },
        follow,
    }
}

/// Keeps Zed's soft wrap width equal to the pane width, in cells. The
/// display map wraps by glyph advance; a representative ASCII advance keeps
/// one wrap column close to one terminal cell on every platform.
fn synchronize_wrap(
    editor: &mut Editor,
    window: &mut Window,
    cx: &mut Context<Editor>,
    area: Rect,
) {
    let widest_line_number = editor.display_snapshot(cx).widest_line_number();
    let columns = EditorWidget::text_width_for_widest_line_number(area, widest_line_number).max(1);
    let style = editor.style(cx).clone();
    let font_id = window.text_system().resolve_font(&style.text.font());
    let font_size = style.text.font_size.to_pixels(window.rem_size());
    let em_width = window
        .text_system()
        .em_width(font_id, font_size)
        .unwrap_or_else(|_| gpui::px(1.0));
    let cell_width = window
        .text_system()
        .advance(font_id, font_size, 'x')
        .map(|advance| advance.width)
        .ok()
        .filter(|width| *width > gpui::px(0.0))
        .unwrap_or(em_width);
    let editor_width = cell_width * f32::from(columns);
    let wrap_width = match editor.soft_wrap_mode(cx) {
        EditorSoftWrap::None | EditorSoftWrap::GitDiff => None,
        EditorSoftWrap::EditorWidth => Some(editor_width),
        EditorSoftWrap::Bounded(columns) => {
            Some(editor_width.min(cell_width * columns.max(1) as f32))
        }
    };
    editor
        .display_map
        .update(cx, |map, cx| map.set_wrap_width(wrap_width, cx));
}

fn line_styles(
    display: &DisplaySnapshot,
    editor_style: &EditorStyle,
    start_row: DisplayRow,
    end_row: DisplayRow,
    line_count: usize,
) -> Vec<Vec<StyleSpan>> {
    let mut lines = vec![Vec::new(); line_count];
    if line_count == 0 {
        return lines;
    }
    let mut row = 0usize;
    let mut column = 0usize;
    for chunk in display.highlighted_chunks(
        start_row..end_row,
        LanguageAwareStyling {
            tree_sitter: true,
            diagnostics: false,
        },
        editor_style,
    ) {
        let style = chunk.style.map(|highlight| {
            terminal_text_style(
                &editor_style.text.clone().highlight(highlight),
                editor_style.background,
            )
        });
        for fragment in chunk.text.split_inclusive('\n') {
            let (text, has_newline) = fragment
                .strip_suffix('\n')
                .map_or((fragment, false), |text| (text, true));
            let width = text.width();
            if let Some(style) = style
                && width > 0
                && let Some(line) = lines.get_mut(row)
            {
                line.push(StyleSpan {
                    start_column: column,
                    end_column: column.saturating_add(width),
                    style,
                });
            }
            column = column.saturating_add(width);
            if has_newline {
                row = row.saturating_add(1);
                column = 0;
            }
        }
    }
    lines
}

fn terminal_text_style(text: &gpui::TextStyle, editor_background: gpui::Hsla) -> Style {
    let background = text
        .background_color
        .map(|color| editor_background.blend(color))
        .unwrap_or(editor_background);
    let mut style = Style::new()
        .fg(terminal_color(background.blend(text.color)))
        .bg(terminal_color(background));
    if text.font_weight >= gpui::FontWeight::BOLD {
        style = style.add_modifier(Modifier::BOLD);
    }
    if matches!(
        text.font_style,
        gpui::FontStyle::Italic | gpui::FontStyle::Oblique
    ) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if text.underline.is_some() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if text.strikethrough.is_some() {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    style
}

fn terminal_color(color: gpui::Hsla) -> Color {
    let color = color.to_rgb();
    Color::Rgb(
        (color.r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (color.g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (color.b.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

fn display_cursor_at(display: &DisplaySnapshot, point: DisplayPoint) -> Cursor {
    let line = display.line(point.row());
    Cursor {
        row: point.row().0 as usize,
        column: terminal_column(&line, point.column() as usize),
    }
}

fn display_cursor_in_rows(lines: &[String], first_row: usize, point: DisplayPoint) -> Cursor {
    let row = point.row().0 as usize;
    let column = row
        .checked_sub(first_row)
        .and_then(|row| lines.get(row))
        .map(|line| terminal_column(line, point.column() as usize))
        .unwrap_or_default();
    Cursor { row, column }
}

fn terminal_column(line: &str, byte_column: usize) -> usize {
    let mut byte_column = byte_column.min(line.len());
    while !line.is_char_boundary(byte_column) {
        byte_column -= 1;
    }
    line[..byte_column].width()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_utf8_byte_columns_to_terminal_cells() {
        assert_eq!(terminal_column("abc", 2), 2);
        assert_eq!(terminal_column("日本語", 3), 2);
        assert_eq!(terminal_column("日本語", 4), 2);
        assert_eq!(terminal_column("日本語", 99), 6);
    }
}
