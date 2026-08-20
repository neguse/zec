mod input;
mod render;
mod terminal;

use std::{
    env,
    ffi::OsString,
    io::{self, IsTerminal as _},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
};

use anyhow::{Context as _, Result, bail};
use editor::{
    Editor, EditorStyle,
    actions::Undo,
    display_map::{DisplayPoint, DisplayRow, DisplaySnapshot},
};
use gpui::{
    AnyWindowHandle, App, AppContext as _, Entity, Focusable as _, Task, WindowBounds,
    WindowHandle, WindowOptions,
};
use language::{
    Buffer, BufferEvent, Language, LanguageAwareStyling, LanguageNotFound, LanguageRegistry,
    language_settings::SoftWrap,
};
use project::{
    ProjectPath,
    buffer_store::BufferStore,
    worktree_store::{WorktreeIdCounter, WorktreeStore},
};
use ratatui::style::{
    Color as TerminalColor, Modifier as TerminalModifier, Style as TerminalStyle,
};
use render::{Cursor, EditorWidget, RenderSnapshot, SelectionRange, StyleSpan, Viewport};
use terminal::{InputReader, TerminalEvent, TerminalSession, ZecTerminal};
use theme::ActiveTheme as _;
use unicode_width::UnicodeWidthStr as _;
use zed_fs::{Fs, RealFs};

const USAGE: &str =
    "Usage: zec [FILE]\n       zec --smoke\n\nKeys: Ctrl-S save, Ctrl-Q quit, Ctrl-Z undo";

#[derive(Debug, Eq, PartialEq)]
enum Command {
    Edit(Option<PathBuf>),
    Smoke,
    Help,
}

fn main() -> Result<()> {
    match parse_command(env::args_os().skip(1))? {
        Command::Edit(path) => run_interactive(path),
        Command::Smoke => {
            run_smoke();
            Ok(())
        }
        Command::Help => {
            println!("{USAGE}");
            Ok(())
        }
    }
}

fn parse_command(arguments: impl IntoIterator<Item = OsString>) -> Result<Command> {
    let mut arguments = arguments.into_iter();
    let Some(first) = arguments.next() else {
        return Ok(Command::Edit(None));
    };

    if first == "--help" || first == "-h" {
        if arguments.next().is_some() {
            bail!("--help does not accept arguments");
        }
        return Ok(Command::Help);
    }
    if first == "--smoke" {
        if arguments.next().is_some() {
            bail!("--smoke does not accept arguments");
        }
        return Ok(Command::Smoke);
    }

    let path = if first == "--" {
        arguments.next().context("expected a file path after --")?
    } else {
        if first.to_string_lossy().starts_with('-') {
            bail!("unknown option: {}", first.to_string_lossy());
        }
        first
    };
    if arguments.next().is_some() {
        bail!("zec currently opens one file at a time");
    }

    Ok(Command::Edit(Some(path.into())))
}

fn run_interactive(path: Option<PathBuf>) -> Result<()> {
    let path = path
        .map(|path| {
            std::path::absolute(&path)
                .with_context(|| format!("failed to make {} absolute", path.display()))
        })
        .transpose()?;

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            io::Error::other("zec requires a terminal; use --smoke for the headless PoC").into(),
        );
    }

    let terminal_session = TerminalSession::enter()?;
    let mut terminal = terminal_session.terminal()?;

    let (event_sender, event_receiver) = async_channel::unbounded();
    let redraw_sender = event_sender.clone();
    let input_reader = InputReader::spawn(event_sender);
    let (error_sender, error_receiver) = mpsc::sync_channel(1);

    gpui_platform::headless().run(move |cx| {
        init_zed(cx);
        let document = open_document(path, cx);

        cx.spawn(async move |cx| {
            let document = match document.await {
                Ok(document) => document,
                Err(error) => {
                    let _ = error_sender.try_send(format!("failed to open file: {error:#}"));
                    let _ = cx.update(|cx| cx.quit());
                    return;
                }
            };
            let editor_window = match cx.update(|cx| open_editor(document.buffer.clone(), cx)) {
                Ok(window) => window,
                Err(error) => {
                    let _ = error_sender
                        .try_send(format!("failed to open headless editor window: {error:#}"));
                    let _ = cx.update(|cx| cx.quit());
                    return;
                }
            };
            let input_window: AnyWindowHandle = editor_window.into();
            if let Err(error) = editor_window.update(cx, |_editor, _window, cx| {
                cx.subscribe(&document.buffer, move |_, _, event, _| {
                    if matches!(
                        event,
                        BufferEvent::LanguageChanged(_) | BufferEvent::Reparsed
                    ) {
                        let _ = redraw_sender.try_send(TerminalEvent::Redraw);
                    }
                })
                .detach();
            }) {
                let _ =
                    error_sender.try_send(format!("failed to observe syntax updates: {error:#}"));
                let _ = cx.update(|cx| cx.quit());
                return;
            }
            let mut viewport = Viewport::default();
            let mut failure = None;
            let mut message = None;
            let mut quit_armed = false;

            loop {
                let mut snapshot = match editor_window.update(cx, |editor, _window, cx| {
                    let dirty = document.buffer.read(cx).is_dirty();
                    capture_editor(
                        editor,
                        cx,
                        viewport,
                        &document.label,
                        dirty,
                        message.as_deref(),
                    )
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
                    TerminalEvent::Key(event) if input::is_quit(&event) => {
                        let dirty = document.buffer.read_with(cx, |buffer, _| buffer.is_dirty());
                        if !dirty || quit_armed {
                            break;
                        }

                        quit_armed = true;
                        message = Some("unsaved changes; press Ctrl-Q again to discard".to_owned());
                    }
                    TerminalEvent::Key(event) if input::is_save(&event) => {
                        quit_armed = false;
                        match save_document(&document, cx).await {
                            Ok(()) => message = Some("saved".to_owned()),
                            Err(error) => message = Some(format!("save failed: {error:#}")),
                        }
                    }
                    TerminalEvent::Key(event) if input::is_intercepted_shortcut(&event) => {}
                    TerminalEvent::Key(event) => {
                        if let Some(keystroke) = input::to_gpui_keystroke(event) {
                            quit_armed = false;
                            message = None;
                            if let Err(error) = input_window.update(cx, |_root, window, cx| {
                                window.dispatch_keystroke(keystroke, cx)
                            }) {
                                failure = Some(format!("failed to dispatch keystroke: {error}"));
                                break;
                            }
                        }
                    }
                    TerminalEvent::Paste(text) => {
                        quit_armed = false;
                        message = None;
                        if let Err(error) = editor_window.update(cx, |editor, window, cx| {
                            editor.do_paste(&text, None, true, window, cx);
                        }) {
                            failure = Some(format!("failed to paste text: {error}"));
                            break;
                        }
                    }
                    TerminalEvent::Resize | TerminalEvent::Redraw => {}
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

struct OpenDocument {
    buffer: Entity<Buffer>,
    buffer_store: Option<Entity<BufferStore>>,
    label: String,
}

fn open_document(path: Option<PathBuf>, cx: &mut App) -> Task<Result<OpenDocument>> {
    let Some(path) = path else {
        let buffer = cx.new(|cx| Buffer::local(String::new(), cx));
        return Task::ready(Ok(OpenDocument {
            buffer,
            buffer_store: None,
            label: "[No Name]".to_owned(),
        }));
    };

    let language_registry = match native_language_registry(cx) {
        Ok(registry) => registry,
        Err(error) => return Task::ready(Err(error)),
    };
    let file_system: Arc<dyn Fs> = Arc::new(RealFs::new(None, cx.background_executor().clone()));
    let worktree_store =
        cx.new(|cx| WorktreeStore::local(true, file_system, WorktreeIdCounter::get(cx)));
    let buffer_store = cx.new(|cx| BufferStore::local(worktree_store.clone(), cx));
    let find_worktree = worktree_store.update(cx, |store, cx| {
        store.find_or_create_worktree(&path, false, cx)
    });

    cx.spawn(async move |cx| {
        let (worktree, relative_path) = find_worktree
            .await
            .with_context(|| format!("could not create a worktree for {}", path.display()))?;
        let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
        let buffer = buffer_store
            .update(cx, |store, cx| {
                store.open_buffer(
                    ProjectPath {
                        worktree_id,
                        path: relative_path,
                    },
                    cx,
                )
            })
            .await
            .with_context(|| format!("could not load {}", path.display()))?;
        assign_file_language(&path, &buffer, language_registry, cx)
            .await
            .with_context(|| format!("could not select a language for {}", path.display()))?;

        Ok(OpenDocument {
            buffer,
            buffer_store: Some(buffer_store),
            label: path.display().to_string(),
        })
    })
}

fn native_language_registry(cx: &mut App) -> Result<Arc<LanguageRegistry>> {
    let registry = Arc::new(LanguageRegistry::new(cx.background_executor().clone()));
    registry.set_theme(cx.theme().clone());
    let native_grammars = grammars::native_grammars();
    let tsx_grammar = native_grammars
        .iter()
        .find(|(name, _)| *name == "tsx")
        .map(|(_, grammar)| grammar.clone())
        .context("bundled TSX grammar is missing")?;
    let rust_grammar = native_grammars
        .iter()
        .find(|(name, _)| *name == "rust")
        .map(|(_, grammar)| grammar.clone())
        .context("bundled Rust grammar is missing")?;

    for (name, grammar) in native_grammars.into_iter().chain([
        ("javascript", tsx_grammar),
        ("zed-keybind-context", rust_grammar),
    ]) {
        let language = Language::new(grammars::load_config(name), Some(grammar))
            .with_queries(grammars::load_queries(name))
            .with_context(|| format!("could not initialize the bundled {name} grammar"))?;
        registry.add(Arc::new(language));
    }
    Ok(registry)
}

async fn assign_file_language(
    path: &Path,
    buffer: &Entity<Buffer>,
    registry: Arc<LanguageRegistry>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let language = match registry.load_language_for_file_path(path).await {
        Ok(language) => Some(language),
        Err(error) if error.is::<LanguageNotFound>() => None,
        Err(error) => return Err(error),
    };
    buffer.update(cx, move |buffer, cx| {
        buffer.set_language_registry(registry);
        if let Some(language) = language {
            buffer.set_language_async(Some(language), cx);
        }
    });
    Ok(())
}

async fn save_document(document: &OpenDocument, cx: &mut gpui::AsyncApp) -> Result<()> {
    let buffer_store = document
        .buffer_store
        .as_ref()
        .context("scratch buffer has no file path")?;
    buffer_store
        .update(cx, |store, cx| {
            store.save_buffer(document.buffer.clone(), cx)
        })
        .await
        .with_context(|| format!("could not save {}", document.label))
}

fn init_zed(cx: &mut App) {
    release_channel::init_test(
        semver::Version::new(0, 0, 0),
        release_channel::ReleaseChannel::Dev,
        cx,
    );
    settings::init(cx);
    theme_settings::init(theme::LoadThemes::JustBase, cx);
    editor::init(cx);

    let bindings =
        settings::KeymapFile::load_asset_allow_partial_failure(settings::DEFAULT_KEYMAP_PATH, cx)
            .expect("failed to load Zed's default keymap");
    cx.bind_keys(bindings);
}

fn open_editor(buffer: Entity<Buffer>, cx: &mut App) -> Result<WindowHandle<Editor>> {
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
    .context("GPUI could not create the hidden window")
}

fn capture_editor(
    editor: &mut Editor,
    cx: &mut gpui::Context<Editor>,
    viewport: Viewport,
    label: &str,
    dirty: bool,
    message: Option<&str>,
) -> RenderSnapshot {
    let editor_style = editor.style(cx).clone();
    let display = editor.display_snapshot(cx);
    let lines = (0..=display.max_point().row().0)
        .map(|row| display.line(DisplayRow(row)))
        .collect::<Vec<_>>();
    let line_numbers = display
        .row_infos(DisplayRow(0))
        .take(lines.len())
        .map(|row| row.buffer_row.map(|row| row.saturating_add(1)))
        .collect();
    let widest_line_number = display.widest_line_number();
    let cursor = display_cursor(&lines, editor.selections.newest_display(&display).head());
    let selections = editor
        .selections
        .all_adjusted_display(&display)
        .into_iter()
        .filter(|selection| !selection.is_empty())
        .map(|selection| SelectionRange {
            start: display_cursor(&lines, selection.start),
            end: display_cursor(&lines, selection.end),
        })
        .collect();
    let line_styles = terminal_line_styles(&display, &editor_style, lines.len());
    let text_style = terminal_text_style(&editor_style.text, editor_style.background);
    let gutter_style = TerminalStyle::new()
        .fg(terminal_color(
            editor_style
                .background
                .blend(cx.theme().colors().editor_line_number),
        ))
        .bg(terminal_color(editor_style.background));

    let dirty_marker = if dirty { " [+]" } else { "" };
    let mut status = format!("zec {label}{dirty_marker}  Ctrl-S save  Ctrl-Q quit  Ctrl-Z undo");
    if let Some(message) = message {
        status = format!("{message}  |  {status}");
    }

    RenderSnapshot {
        lines,
        line_numbers,
        widest_line_number,
        cursor: Some(cursor),
        selections,
        text_style,
        gutter_style,
        line_styles,
        viewport,
        status,
    }
}

fn terminal_line_styles(
    display: &DisplaySnapshot,
    editor_style: &EditorStyle,
    line_count: usize,
) -> Vec<Vec<StyleSpan>> {
    let mut lines = vec![Vec::new(); line_count];
    let mut row = 0usize;
    let mut column = 0usize;
    let end_row = display.max_point().row().0.saturating_add(1);

    for chunk in display.highlighted_chunks(
        DisplayRow(0)..DisplayRow(end_row),
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

fn terminal_text_style(text: &gpui::TextStyle, editor_background: gpui::Hsla) -> TerminalStyle {
    let background = text
        .background_color
        .map(|color| editor_background.blend(color))
        .unwrap_or(editor_background);
    let mut style = TerminalStyle::new()
        .fg(terminal_color(background.blend(text.color)))
        .bg(terminal_color(background));

    if text.font_weight >= gpui::FontWeight::BOLD {
        style = style.add_modifier(TerminalModifier::BOLD);
    }
    if matches!(
        text.font_style,
        gpui::FontStyle::Italic | gpui::FontStyle::Oblique
    ) {
        style = style.add_modifier(TerminalModifier::ITALIC);
    }
    if text.underline.is_some() {
        style = style.add_modifier(TerminalModifier::UNDERLINED);
    }
    if text.strikethrough.is_some() {
        style = style.add_modifier(TerminalModifier::CROSSED_OUT);
    }
    style
}

fn terminal_color(color: gpui::Hsla) -> TerminalColor {
    let color = color.to_rgb();
    TerminalColor::Rgb(
        (color.r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (color.g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (color.b.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

fn display_cursor(lines: &[String], point: DisplayPoint) -> Cursor {
    let row = point.row().0 as usize;
    let column = lines
        .get(row)
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

fn draw(
    terminal: &mut ZecTerminal,
    snapshot: &mut RenderSnapshot,
    viewport: &mut Viewport,
) -> io::Result<()> {
    terminal
        .draw(|frame| {
            let area = frame.area();
            let text_width = EditorWidget::new(snapshot).text_width(area);
            keep_cursor_visible(viewport, snapshot.cursor, text_width, area.height);
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
        let buffer = cx.new(|cx| Buffer::local(String::new(), cx));
        let window = open_editor(buffer, cx).expect("failed to open editor");

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

    fn command(arguments: &[&str]) -> Result<Command> {
        parse_command(arguments.iter().map(|argument| OsString::from(*argument)))
    }

    #[test]
    fn parses_cli_modes_and_file_paths() {
        assert_eq!(command(&[]).unwrap(), Command::Edit(None));
        assert_eq!(
            command(&["notes.txt"]).unwrap(),
            Command::Edit(Some(PathBuf::from("notes.txt")))
        );
        assert_eq!(command(&["--smoke"]).unwrap(), Command::Smoke);
        assert_eq!(command(&["--help"]).unwrap(), Command::Help);
        assert_eq!(
            command(&["--", "-draft.txt"]).unwrap(),
            Command::Edit(Some(PathBuf::from("-draft.txt")))
        );
    }

    #[test]
    fn rejects_unknown_options_and_multiple_files() {
        assert!(command(&["--wat"]).is_err());
        assert!(command(&["one", "two"]).is_err());
        assert!(command(&["--"]).is_err());
    }

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

    #[test]
    fn horizontal_scroll_uses_width_remaining_after_line_number_gutter() {
        let snapshot = RenderSnapshot {
            lines: vec!["abcdef".into()],
            line_numbers: vec![Some(1)],
            widest_line_number: 9,
            ..RenderSnapshot::default()
        };
        let area = ratatui::layout::Rect::new(0, 0, 6, 2);
        let text_width = EditorWidget::new(&snapshot).text_width(area);
        let mut viewport = Viewport::default();

        keep_cursor_visible(
            &mut viewport,
            Some(Cursor { row: 0, column: 4 }),
            text_width,
            area.height,
        );

        assert_eq!(text_width, 4);
        assert_eq!(viewport.left_column, 1);
    }
}
