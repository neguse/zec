mod input;
mod render;
mod terminal;

use std::{
    env,
    error::Error,
    io::{self, IsTerminal as _},
    sync::mpsc,
};

use editor::{Editor, actions::Undo, display_map::DisplayRow};
use gpui::{
    AnyWindowHandle, App, AppContext as _, Focusable as _, WindowBounds, WindowHandle,
    WindowOptions,
};
use language::{Buffer, language_settings::SoftWrap};
use render::{Cursor, EditorWidget, RenderSnapshot, Viewport};
use terminal::{InputReader, TerminalEvent, TerminalSession, ZecTerminal};
use unicode_width::UnicodeWidthStr as _;

fn main() -> Result<(), Box<dyn Error>> {
    if env::args_os().any(|argument| argument == "--smoke") {
        run_smoke();
        return Ok(());
    }

    run_interactive()
}

fn run_interactive() -> Result<(), Box<dyn Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            io::Error::other("zec requires a terminal; use --smoke for the headless PoC").into(),
        );
    }

    let terminal_session = TerminalSession::enter()?;
    let mut terminal = terminal_session.terminal()?;

    let (event_sender, event_receiver) = async_channel::unbounded();
    let input_reader = InputReader::spawn(event_sender);
    let (error_sender, error_receiver) = mpsc::sync_channel(1);

    gpui_platform::headless().run(move |cx| {
        init_zed(cx);
        let editor_window = open_editor(cx);
        let input_window: AnyWindowHandle = editor_window.into();

        cx.spawn(async move |cx| {
            let mut viewport = Viewport::default();
            let mut failure = None;

            loop {
                let mut snapshot = match editor_window.update(cx, |editor, _window, cx| {
                    capture_editor(editor, cx, viewport)
                }) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        failure = Some(format!("failed to read editor state: {error}"));
                        break;
                    }
                };

                if let Err(error) = draw(&mut terminal, &mut snapshot, &mut viewport) {
                    failure = Some(format!("failed to draw terminal: {error}"));
                    break;
                }

                let event = match event_receiver.recv().await {
                    Ok(event) => event,
                    Err(error) => {
                        failure = Some(format!("terminal input stopped: {error}"));
                        break;
                    }
                };

                match event {
                    TerminalEvent::Key(event) if input::is_quit(&event) => break,
                    TerminalEvent::Key(event) => {
                        if let Some(keystroke) = input::to_gpui_keystroke(event)
                            && let Err(error) = input_window.update(cx, |_root, window, cx| {
                                window.dispatch_keystroke(keystroke, cx)
                            })
                        {
                            failure = Some(format!("failed to dispatch keystroke: {error}"));
                            break;
                        }
                    }
                    TerminalEvent::Paste(text) => {
                        if let Err(error) = editor_window.update(cx, |editor, window, cx| {
                            editor.handle_input(&text, window, cx);
                        }) {
                            failure = Some(format!("failed to paste text: {error}"));
                            break;
                        }
                    }
                    TerminalEvent::Resize => {}
                    TerminalEvent::Error(error) => {
                        failure = Some(format!("failed to read terminal input: {error}"));
                        break;
                    }
                }
            }

            if let Some(error) = failure {
                let _ = error_sender.try_send(error);
            }
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
    });

    input_reader.stop();
    drop(input_reader);
    drop(terminal_session);

    if let Ok(error) = error_receiver.try_recv() {
        return Err(io::Error::other(error).into());
    }

    Ok(())
}

fn init_zed(cx: &mut App) {
    release_channel::init_test(
        semver::Version::new(0, 0, 0),
        release_channel::ReleaseChannel::Dev,
        cx,
    );
    settings::init(cx);
    theme::init(theme::LoadThemes::JustBase, cx);
    editor::init(cx);

    let bindings =
        settings::KeymapFile::load_asset_allow_partial_failure(settings::DEFAULT_KEYMAP_PATH, cx)
            .expect("failed to load Zed's default keymap");
    cx.bind_keys(bindings);
}

fn open_editor(cx: &mut App) -> WindowHandle<Editor> {
    let buffer = cx.new(|cx| Buffer::local(String::new(), cx));

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
            let editor = cx.new(|cx| Editor::for_buffer(buffer, None, window, cx));
            editor.update(cx, |editor, cx| {
                editor.set_soft_wrap_mode(SoftWrap::None, cx);
                editor
                    .display_map
                    .update(cx, |map, cx| map.set_wrap_width(None, cx));
            });
            window.focus(&editor.focus_handle(cx), cx);
            editor
        },
    )
    .expect("failed to open headless editor window")
}

fn capture_editor(
    editor: &mut Editor,
    cx: &mut gpui::Context<Editor>,
    viewport: Viewport,
) -> RenderSnapshot {
    let display = editor.display_snapshot(cx);
    let cursor = editor.selections.newest_display(&display).head();
    let lines = (0..=display.max_point().row().0)
        .map(|row| display.line(DisplayRow(row)))
        .collect::<Vec<_>>();
    let row = cursor.row().0 as usize;
    let column = lines
        .get(row)
        .map(|line| terminal_column(line, cursor.column() as usize))
        .unwrap_or_default();

    RenderSnapshot {
        lines,
        cursor: Some(Cursor { row, column }),
        viewport,
        status: "zec  Ctrl-Q quit  Ctrl-Z undo".to_owned(),
    }
}

fn terminal_column(line: &str, byte_column: usize) -> usize {
    let mut byte_column = byte_column.min(line.len());
    while !line.is_char_boundary(byte_column) {
        byte_column -= 1;
    }
    line[..byte_column].width()
}

fn draw(
    terminal: &mut ZecTerminal,
    snapshot: &mut RenderSnapshot,
    viewport: &mut Viewport,
) -> io::Result<()> {
    terminal
        .draw(|frame| {
            let area = frame.area();
            keep_cursor_visible(viewport, snapshot.cursor, area.width, area.height);
            snapshot.viewport = *viewport;

            let widget = EditorWidget::new(snapshot);
            let cursor_position = widget.cursor_position(area);
            frame.render_widget(widget, area);
            if let Some(cursor_position) = cursor_position {
                frame.set_cursor_position(cursor_position);
            }
        })
        .map(|_| ())
}

fn keep_cursor_visible(viewport: &mut Viewport, cursor: Option<Cursor>, width: u16, height: u16) {
    let Some(cursor) = cursor else {
        return;
    };

    let body_height = usize::from(height.saturating_sub(1));
    if body_height > 0 {
        if cursor.row < viewport.top_row {
            viewport.top_row = cursor.row;
        } else if cursor.row >= viewport.top_row.saturating_add(body_height) {
            viewport.top_row = cursor.row.saturating_add(1).saturating_sub(body_height);
        }
    }

    let width = usize::from(width);
    if width > 0 {
        if cursor.column < viewport.left_column {
            viewport.left_column = cursor.column;
        } else if cursor.column >= viewport.left_column.saturating_add(width) {
            viewport.left_column = cursor.column.saturating_add(1).saturating_sub(width);
        }
    }
}

fn run_smoke() {
    gpui_platform::headless().run(|cx| {
        init_zed(cx);
        let window = open_editor(cx);

        cx.spawn(async move |cx| {
            window
                .update(cx, |editor, window, cx| {
                    println!("initial: {:?}", editor.text(cx));
                    editor.insert("hello from zec", window, cx);
                    println!("after insert: {:?}", editor.text(cx));
                    editor.undo(&Undo, window, cx);
                    println!("after undo: {:?}", editor.text(cx));
                })
                .expect("failed to update editor");

            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_utf8_byte_columns_to_terminal_cells() {
        assert_eq!(terminal_column("a界e\u{301}", 0), 0);
        assert_eq!(terminal_column("a界e\u{301}", 1), 1);
        assert_eq!(terminal_column("a界e\u{301}", 4), 3);
        assert_eq!(terminal_column("a界e\u{301}", 7), 4);
    }

    #[test]
    fn scrolls_only_when_cursor_leaves_viewport() {
        let mut viewport = Viewport::default();

        keep_cursor_visible(&mut viewport, Some(Cursor { row: 4, column: 9 }), 10, 6);
        assert_eq!(viewport, Viewport::default());

        keep_cursor_visible(&mut viewport, Some(Cursor { row: 5, column: 10 }), 10, 6);
        assert_eq!(
            viewport,
            Viewport {
                top_row: 1,
                left_column: 1,
            }
        );
    }
}
