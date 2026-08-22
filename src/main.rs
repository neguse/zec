mod clipboard;
mod input;
mod prompt;
mod render;
mod tabs;
mod terminal;

use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    io::{self, IsTerminal as _},
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
};

use anyhow::{Context as _, Result, bail};
use editor::{
    Anchor, Bias, Editor, EditorStyle, SelectionEffects,
    actions::{Cut, Undo},
    display_map::{DisplayPoint, DisplayRow, DisplaySnapshot},
    scroll::Autoscroll,
};
use gpui::{
    AnyWindowHandle, App, AppContext as _, Entity, Focusable as _, Task, WindowBounds,
    WindowHandle, WindowOptions,
};
use language::{
    Buffer, BufferEvent, LanguageAwareStyling, LanguageNotFound, LanguageRegistry, LoadedLanguage,
    language_settings::SoftWrap,
};
use project::{
    ProjectPath,
    buffer_store::BufferStore,
    search::SearchQuery,
    worktree_store::{WorktreeIdCounter, WorktreeStore},
};
use prompt::{LinePrompt, PromptAction};
use ratatui::{
    layout::{Position as TerminalPosition, Rect},
    style::{Color as TerminalColor, Modifier as TerminalModifier, Style as TerminalStyle},
};
use render::{
    BackgroundRange, Cursor, EditorWidget, RenderSnapshot, SelectionRange, StyleSpan, TextPosition,
    Viewport,
};
use tabs::{Direction as TabDirection, TabLabel};
use terminal::{InputReader, ScrollDirection, TerminalEvent, TerminalSession};
use theme::ActiveTheme as _;
use unicode_width::UnicodeWidthStr as _;
use workspace::searchable::{Direction, SearchToken, SearchableItem as _};
use zed_fs::{Fs, RealFs};

const USAGE: &str = "Usage: zec [FILE ...]\n       zec --smoke\n\nKeys: Ctrl-N new, Ctrl-O open, Ctrl-W close tab, Ctrl-PgUp/PgDn tabs, Alt-PgUp/PgDn scroll, Ctrl-C copy, Ctrl-X cut, Ctrl-F find, Ctrl-H replace, Ctrl-G go to line, Ctrl-R reload, Ctrl-S save, Ctrl-Q quit, Ctrl-Z undo";

#[derive(Debug, Eq, PartialEq)]
enum Command {
    Edit(Vec<PathBuf>),
    Smoke,
    Help,
}

#[derive(Debug)]
struct ActiveSearch {
    prompt: LinePrompt,
    replacement: LinePrompt,
    replace_enabled: bool,
    focused_field: SearchField,
    matches: Vec<Range<Anchor>>,
    active_match: Option<usize>,
    token: SearchToken,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SearchField {
    #[default]
    Query,
    Replacement,
}

#[derive(Debug, Default)]
struct SaveAsPrompt {
    prompt: LinePrompt,
    overwrite_path: Option<PathBuf>,
    feedback: Option<String>,
}

#[derive(Debug, Default)]
struct OpenPrompt {
    prompt: LinePrompt,
    feedback: Option<String>,
}

#[derive(Debug, Default)]
struct GoToLinePrompt {
    prompt: LinePrompt,
    feedback: Option<String>,
}

impl GoToLinePrompt {
    fn status(&self, message: Option<&str>) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prefix = format!("{message_prefix}Go to line: ");
        let cursor_column = prefix.width().saturating_add(
            self.prompt
                .text()
                .get(..self.prompt.cursor())
                .unwrap_or_default()
                .width(),
        );
        let mut status = format!(
            "{prefix}{}  line[:column]  Enter go  Esc cancel",
            self.prompt.text()
        );
        if let Some(feedback) = &self.feedback {
            status.push_str("  |  ");
            status.push_str(feedback);
        }
        (status, cursor_column)
    }

    fn text_changed(&mut self) {
        self.feedback = None;
    }
}

impl OpenPrompt {
    fn status(&self, message: Option<&str>) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prefix = format!("{message_prefix}Open: ");
        let cursor_column = prefix.width().saturating_add(
            self.prompt
                .text()
                .get(..self.prompt.cursor())
                .unwrap_or_default()
                .width(),
        );
        let mut status = format!("{prefix}{}  Enter open  Esc cancel", self.prompt.text());
        if let Some(feedback) = &self.feedback {
            status.push_str("  |  ");
            status.push_str(feedback);
        }
        (status, cursor_column)
    }

    fn text_changed(&mut self) {
        self.feedback = None;
    }
}

impl SaveAsPrompt {
    fn status(&self, message: Option<&str>) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prefix = format!("{message_prefix}Save as: ");
        let cursor_column = prefix.width().saturating_add(
            self.prompt
                .text()
                .get(..self.prompt.cursor())
                .unwrap_or_default()
                .width(),
        );
        let mut status = format!("{prefix}{}  Enter save  Esc cancel", self.prompt.text());
        if let Some(feedback) = &self.feedback {
            status.push_str("  |  ");
            status.push_str(feedback);
        }
        (status, cursor_column)
    }

    fn text_changed(&mut self) {
        self.overwrite_path = None;
        self.feedback = None;
    }
}

impl Default for ActiveSearch {
    fn default() -> Self {
        Self {
            prompt: LinePrompt::new(),
            replacement: LinePrompt::new(),
            replace_enabled: false,
            focused_field: SearchField::Query,
            matches: Vec::new(),
            active_match: None,
            token: SearchToken::default(),
            error: None,
        }
    }
}

impl ActiveSearch {
    fn status(&self, message: Option<&str>) -> (String, usize) {
        let position = self.active_match.map_or(0, |index| index.saturating_add(1));
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prompt_prefix = format!("{message_prefix}Find: ");
        let (mut status, cursor_column) = if self.replace_enabled {
            let replacement_prefix = format!("{prompt_prefix}{}  Replace: ", self.prompt.text());
            let cursor_column = match self.focused_field {
                SearchField::Query => prompt_prefix.width().saturating_add(
                    self.prompt
                        .text()
                        .get(..self.prompt.cursor())
                        .unwrap_or_default()
                        .width(),
                ),
                SearchField::Replacement => replacement_prefix.width().saturating_add(
                    self.replacement
                        .text()
                        .get(..self.replacement.cursor())
                        .unwrap_or_default()
                        .width(),
                ),
            };
            (
                format!(
                    "{replacement_prefix}{}  {position}/{}  Tab/BackTab field  Enter replace  Alt-Enter all  Esc close",
                    self.replacement.text(),
                    self.matches.len()
                ),
                cursor_column,
            )
        } else {
            (
                format!(
                    "{prompt_prefix}{}  {position}/{}",
                    self.prompt.text(),
                    self.matches.len()
                ),
                prompt_prefix.width().saturating_add(
                    self.prompt
                        .text()
                        .get(..self.prompt.cursor())
                        .unwrap_or_default()
                        .width(),
                ),
            )
        };
        if let Some(error) = &self.error {
            status.push_str("  ");
            status.push_str(error);
        }
        (status, cursor_column)
    }

    fn focused_prompt_mut(&mut self) -> &mut LinePrompt {
        match self.focused_field {
            SearchField::Query => &mut self.prompt,
            SearchField::Replacement => &mut self.replacement,
        }
    }
}

fn main() -> Result<()> {
    match parse_command(env::args_os().skip(1))? {
        Command::Edit(paths) => run_interactive(paths),
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
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    let Some(first) = arguments.first() else {
        return Ok(Command::Edit(Vec::new()));
    };

    if first == "--help" || first == "-h" {
        if arguments.len() > 1 {
            bail!("--help does not accept arguments");
        }
        return Ok(Command::Help);
    }
    if first == "--smoke" {
        if arguments.len() > 1 {
            bail!("--smoke does not accept arguments");
        }
        return Ok(Command::Smoke);
    }

    let mut paths = Vec::new();
    let mut positional_only = false;
    for argument in arguments {
        if !positional_only && argument == "--" {
            positional_only = true;
            continue;
        }
        if !positional_only && argument.to_string_lossy().starts_with('-') {
            bail!("unknown option: {}", argument.to_string_lossy());
        }
        paths.push(argument.into());
    }
    if positional_only && paths.is_empty() {
        bail!("expected a file path after --");
    }

    Ok(Command::Edit(paths))
}

fn absolute_unique_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    let mut unique_paths = HashSet::new();
    paths
        .into_iter()
        .map(|path| {
            std::path::absolute(&path)
                .with_context(|| format!("failed to make {} absolute", path.display()))
        })
        .filter_map(|path| match path {
            Ok(path) if unique_paths.insert(path.clone()) => Some(Ok(path)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn run_interactive(paths: Vec<PathBuf>) -> Result<()> {
    let paths = absolute_unique_paths(paths)?;

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            io::Error::other("zec requires a terminal; use --smoke for the headless PoC").into(),
        );
    }

    // One pending event is enough: every event is followed by a fresh snapshot,
    // and redundant redraw notifications can be dropped safely. Input applies
    // backpressure to the reader thread instead of growing memory without bound.
    let (event_sender, event_receiver) = async_channel::bounded(1);
    let redraw_sender = event_sender.clone();
    let mut input_reader = InputReader::spawn(event_sender)?;
    let terminal_session = TerminalSession::enter()?;
    let mut terminal = terminal_session.terminal()?;
    let (error_sender, error_receiver) = mpsc::sync_channel(1);

    gpui_platform::headless().run(move |cx| {
        init_zed(cx);
        let services = file_services(cx);
        let documents = if paths.is_empty() {
            vec![open_document(None, services.clone(), cx)]
        } else {
            paths
                .into_iter()
                .map(|path| open_document(Some(path), services.clone(), cx))
                .collect()
        };

        cx.spawn(async move |cx| {
            let mut tabs = Vec::with_capacity(documents.len());
            let mut opened_buffer_ids = HashSet::new();
            let mut next_untitled_id = 1usize;
            for document in documents {
                let mut document = match document.await {
                    Ok(document) => document,
                    Err(error) => {
                        let _ = error_sender.try_send(format!("failed to open file: {error:#}"));
                        let _ = cx.update(|cx| cx.quit());
                        return;
                    }
                };
                if !opened_buffer_ids.insert(document.buffer.entity_id()) {
                    continue;
                }
                if document.untitled_label.is_some() {
                    document.untitled_label = Some(untitled_label(next_untitled_id));
                    next_untitled_id = next_untitled_id.saturating_add(1);
                }
                let tab = match create_document_tab(
                    document,
                    services.buffer_store.clone(),
                    redraw_sender.clone(),
                    cx,
                ) {
                    Ok(tab) => tab,
                    Err(error) => {
                        let _ = error_sender
                            .try_send(format!("failed to open headless editor window: {error:#}"));
                        let _ = cx.update(|cx| cx.quit());
                        return;
                    }
                };
                tabs.push(tab);
            }

            let mut active_index = 0;
            let mut failure = None;
            let mut message = None;
            let mut quit_armed = false;
            let mut close_armed = false;
            let mut reload_armed = false;
            let mut save_conflict_armed = false;
            let mut active_search: Option<ActiveSearch> = None;
            let mut save_as_prompt: Option<SaveAsPrompt> = None;
            let mut open_prompt: Option<OpenPrompt> = None;
            let mut go_to_line_prompt: Option<GoToLinePrompt> = None;

            loop {
                let editor_window = tabs[active_index].editor_window;
                let input_window: AnyWindowHandle = editor_window.into();
                let status_label = tab_status(&tabs, active_index, cx);
                let viewport = tabs[active_index].viewport;
                let manual_vertical_scroll = tabs[active_index].manual_vertical_scroll;
                let last_cursor = tabs[active_index].last_cursor;
                let mut captured = None;
                let mut frame_area = Rect::default();
                let draw_result = terminal
                    .try_draw(|frame| -> io::Result<()> {
                        frame_area = frame.area();
                        let frame_capture = editor_window
                            .update(cx, |editor, _window, cx| {
                                capture_editor(
                                    editor,
                                    cx,
                                    viewport,
                                    manual_vertical_scroll,
                                    last_cursor,
                                    frame_area,
                                    &status_label,
                                    message.as_deref(),
                                    active_search.as_ref(),
                                    save_as_prompt.as_ref(),
                                    open_prompt.as_ref(),
                                    go_to_line_prompt.as_ref(),
                                )
                            })
                            .map_err(|error| {
                                io::Error::other(format!(
                                    "failed to read editor state: {error}"
                                ))
                            })?;

                        let widget = EditorWidget::new(&frame_capture.snapshot);
                        let cursor_position = widget.cursor_position(frame_area);
                        frame.render_widget(widget, frame_area);
                        if let Some(cursor_position) = cursor_position {
                            frame.set_cursor_position(cursor_position);
                        }
                        captured = Some(frame_capture);
                        Ok(())
                    })
                    .map(|_| ());
                if let Err(error) = draw_result {
                    failure = Some(format!("failed to draw terminal: {error}"));
                    break;
                }
                let Some(captured) = captured else {
                    failure = Some("terminal draw completed without an editor snapshot".to_owned());
                    break;
                };
                tabs[active_index].viewport = captured.snapshot.viewport;
                tabs[active_index].manual_vertical_scroll = captured.manual_vertical_scroll;
                tabs[active_index].last_cursor = captured.last_cursor;
                let snapshot = captured.snapshot;
                let body_height = usize::from(frame_area.height.saturating_sub(1));

                let event = match event_receiver.recv().await {
                    Ok(event) => event,
                    Err(error) => {
                        failure = Some(format!("terminal input stopped: {error}"));
                        break;
                    }
                };

                let reset_close = resets_confirmation(&event, input::is_close_tab);
                let reset_quit = resets_confirmation(&event, input::is_quit);
                let reset_reload = resets_confirmation(&event, input::is_reload);
                let reset_save_conflict = resets_confirmation(&event, input::is_save);
                if (reset_close && close_armed)
                    || (reset_quit && quit_armed)
                    || (reset_reload && reload_armed)
                    || (reset_save_conflict && save_conflict_armed)
                {
                    message = None;
                }
                if reset_close {
                    close_armed = false;
                }
                if reset_quit {
                    quit_armed = false;
                }
                if reset_reload {
                    reload_armed = false;
                }
                if reset_save_conflict {
                    save_conflict_armed = false;
                }

                match event {
                    TerminalEvent::Key(event) if input::is_quit(&event) => {
                        let guarded_count = tabs
                            .iter()
                            .filter(|tab| {
                                document_state(&tab.document, cx).needs_discard_confirmation()
                            })
                            .count();
                        if guarded_count == 0 || quit_armed {
                            break;
                        }

                        quit_armed = true;
                        message = Some(format!(
                            "{guarded_count} unsaved or deleted tab(s); press Ctrl-Q again to discard"
                        ));
                    }
                    TerminalEvent::Key(event) if input::is_close_tab(&event) => {
                        quit_armed = false;
                        let needs_confirmation =
                            document_state(&tabs[active_index].document, cx)
                                .needs_discard_confirmation();
                        if needs_confirmation && !close_armed {
                            close_armed = true;
                            message = Some(
                                "unsaved changes or deleted file; press Ctrl-W again to discard this tab"
                                    .to_owned(),
                            );
                            continue;
                        }

                        if active_search.take().is_some()
                            && let Err(error) = close_search(&editor_window, cx)
                        {
                            failure = Some(format!("failed to close buffer search: {error:#}"));
                            break;
                        }
                        if let Err(error) = editor_window.update(cx, |_editor, window, _cx| {
                            window.remove_window();
                        }) {
                            failure = Some(format!("failed to close editor tab: {error}"));
                            break;
                        }

                        tabs.remove(active_index);
                        if tabs.is_empty() {
                            break;
                        }
                        active_index = active_index.min(tabs.len() - 1);
                        save_as_prompt = None;
                        open_prompt = None;
                        go_to_line_prompt = None;
                        close_armed = false;
                        message = Some("tab closed".to_owned());
                    }
                    TerminalEvent::Key(event)
                        if input::is_previous_tab(&event) || input::is_next_tab(&event) =>
                    {
                        let direction = if input::is_previous_tab(&event) {
                            TabDirection::Previous
                        } else {
                            TabDirection::Next
                        };
                        let Some(next_index) =
                            tabs::adjacent_index(active_index, tabs.len(), direction)
                        else {
                            continue;
                        };
                        if next_index != active_index {
                            if active_search.take().is_some()
                                && let Err(error) = close_search(&editor_window, cx)
                            {
                                failure = Some(format!("failed to close buffer search: {error:#}"));
                                break;
                            }
                            save_as_prompt = None;
                            open_prompt = None;
                            go_to_line_prompt = None;
                            active_index = next_index;
                            quit_armed = false;
                            message = None;
                        }
                    }
                    TerminalEvent::Key(event)
                        if input::is_scroll_page_up(&event)
                            || input::is_scroll_page_down(&event) =>
                    {
                        if save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                            && active_search.is_none()
                        {
                            let direction = if input::is_scroll_page_up(&event) {
                                ScrollDirection::Up
                            } else {
                                ScrollDirection::Down
                            };
                            let tab = &mut tabs[active_index];
                            if scroll_viewport(
                                &mut tab.viewport,
                                snapshot.total_rows,
                                body_height,
                                direction,
                                body_height.saturating_sub(1).max(1),
                            ) {
                                tab.manual_vertical_scroll = true;
                            }
                            message = None;
                        }
                    }
                    TerminalEvent::Key(event) if input::is_reload(&event) => {
                        quit_armed = false;
                        if save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                        {
                            let state = document_state(&tabs[active_index].document, cx);
                            if !state.has_file() {
                                reload_armed = false;
                                message = Some("reload failed: buffer has no file path".to_owned());
                                continue;
                            }

                            if state.dirty && !reload_armed {
                                reload_armed = true;
                                message = Some(
                                    "unsaved changes; press Ctrl-R again to reload from disk"
                                        .to_owned(),
                                );
                                continue;
                            }

                            reload_armed = false;
                            save_conflict_armed = false;
                            match reload_document(&tabs[active_index].document, &services, cx).await
                            {
                                Ok(()) => {
                                    let conflict = tabs[active_index]
                                        .document
                                        .buffer
                                        .read_with(cx, |buffer, _| buffer.has_conflict());
                                    if conflict {
                                        message = Some(
                                            "file changed again while reloading; local edits were kept"
                                                .to_owned(),
                                        );
                                    } else {
                                        if let Some(search) = active_search.as_mut()
                                            && let Err(error) =
                                                refresh_search(search, &editor_window, cx).await
                                        {
                                            failure = Some(format!(
                                                "buffer search failed after reload: {error:#}"
                                            ));
                                            break;
                                        }
                                        message = Some("reloaded from disk".to_owned());
                                    }
                                }
                                Err(error) => {
                                    message = Some(format!("reload failed: {error:#}"));
                                }
                            }
                        }
                    }
                    TerminalEvent::Key(event) if input::is_save(&event) => {
                        quit_armed = false;
                        if save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                        {
                            let state = document_state(&tabs[active_index].document, cx);
                            if state.has_file() {
                                if state.has_external_change() && !save_conflict_armed {
                                    save_conflict_armed = true;
                                    message = Some(if state.deleted {
                                        "deleted on disk; press Ctrl-S again to recreate, or Ctrl-R to reload"
                                            .to_owned()
                                    } else {
                                        "changed on disk; press Ctrl-S again to overwrite, or Ctrl-R to reload"
                                            .to_owned()
                                    });
                                    continue;
                                }
                                save_conflict_armed = false;
                                match save_document(&tabs[active_index].document, &services, cx)
                                    .await
                                {
                                    Ok(()) => message = Some("saved".to_owned()),
                                    Err(error) => message = Some(format!("save failed: {error:#}")),
                                }
                            } else {
                                if active_search.take().is_some()
                                    && let Err(error) = close_search(&editor_window, cx)
                                {
                                    failure =
                                        Some(format!("failed to close buffer search: {error:#}"));
                                    break;
                                }
                                message = None;
                                save_as_prompt = Some(SaveAsPrompt::default());
                            }
                        }
                    }
                    TerminalEvent::Key(event) if input::is_find(&event) => {
                        quit_armed = false;
                        if save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                        {
                            message = None;
                            if let Some(search) = active_search.as_mut() {
                                search.focused_field = SearchField::Query;
                            } else {
                                if let Err(error) =
                                    editor_window.update(cx, |editor, window, cx| {
                                        editor.search_bar_visibility_changed(true, window, cx);
                                    })
                                {
                                    failure =
                                        Some(format!("failed to start buffer search: {error}"));
                                    break;
                                }
                                active_search = Some(ActiveSearch::default());
                            }
                        }
                    }
                    TerminalEvent::Key(event) if input::is_replace(&event) => {
                        quit_armed = false;
                        if save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                        {
                            message = None;
                            if let Some(search) = active_search.as_mut() {
                                search.replace_enabled = !search.replace_enabled;
                                search.focused_field = if search.replace_enabled {
                                    SearchField::Replacement
                                } else {
                                    SearchField::Query
                                };
                            } else {
                                if let Err(error) =
                                    editor_window.update(cx, |editor, window, cx| {
                                        editor.search_bar_visibility_changed(true, window, cx);
                                    })
                                {
                                    failure =
                                        Some(format!("failed to start buffer search: {error}"));
                                    break;
                                }
                                let mut search = ActiveSearch::default();
                                search.replace_enabled = true;
                                active_search = Some(search);
                            }
                        }
                    }
                    TerminalEvent::Key(event) if input::is_go_to_line(&event) => {
                        quit_armed = false;
                        if save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                        {
                            if active_search.take().is_some()
                                && let Err(error) = close_search(&editor_window, cx)
                            {
                                failure = Some(format!("failed to close buffer search: {error:#}"));
                                break;
                            }
                            message = None;
                            go_to_line_prompt = Some(GoToLinePrompt::default());
                        }
                    }
                    TerminalEvent::Key(event) if input::is_open(&event) => {
                        quit_armed = false;
                        if save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                        {
                            if active_search.take().is_some()
                                && let Err(error) = close_search(&editor_window, cx)
                            {
                                failure = Some(format!("failed to close buffer search: {error:#}"));
                                break;
                            }
                            message = None;
                            open_prompt = Some(OpenPrompt::default());
                        }
                    }
                    TerminalEvent::Key(event) if input::is_new_tab(&event) => {
                        quit_armed = false;
                        if active_search.take().is_some()
                            && let Err(error) = close_search(&editor_window, cx)
                        {
                            failure = Some(format!("failed to close buffer search: {error:#}"));
                            break;
                        }
                        save_as_prompt = None;
                        open_prompt = None;
                        go_to_line_prompt = None;
                        let mut document = match cx
                            .update(|cx| open_document(None, services.clone(), cx))
                            .await
                        {
                            Ok(document) => document,
                            Err(error) => {
                                message = Some(format!("new tab failed: {error:#}"));
                                continue;
                            }
                        };
                        document.untitled_label = Some(untitled_label(next_untitled_id));
                        match create_document_tab(
                            document,
                            services.buffer_store.clone(),
                            redraw_sender.clone(),
                            cx,
                        ) {
                            Ok(tab) => {
                                tabs.push(tab);
                                active_index = tabs.len() - 1;
                                next_untitled_id = next_untitled_id.saturating_add(1);
                                message = Some("new tab".to_owned());
                            }
                            Err(error) => {
                                message = Some(format!("new tab failed: {error:#}"));
                            }
                        }
                    }
                    TerminalEvent::Key(event)
                        if save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                            && active_search.is_none()
                            && input::is_copy(&event) =>
                    {
                        quit_armed = false;
                        let item = match editor_window.update(cx, |editor, _window, cx| {
                            clipboard::item_for_copy(editor, cx)
                        }) {
                            Ok(item) => item,
                            Err(error) => {
                                failure =
                                    Some(format!("failed to read clipboard selection: {error}"));
                                break;
                            }
                        };
                        match clipboard::write_osc52(terminal.backend_mut(), &item) {
                            Ok(()) => message = Some("copied".to_owned()),
                            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                                message = Some(format!("copy failed: {error}"));
                            }
                            Err(error) => {
                                failure =
                                    Some(format!("failed to write terminal clipboard: {error}"));
                                break;
                            }
                        }
                    }
                    TerminalEvent::Key(event)
                        if save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                            && active_search.is_none()
                            && input::is_cut(&event) =>
                    {
                        quit_armed = false;
                        let item = match editor_window.update(cx, |editor, _window, cx| {
                            (!editor.read_only(cx)).then(|| clipboard::item_for_cut(editor, cx))
                        }) {
                            Ok(Some(item)) => item,
                            Ok(None) => {
                                message = Some("cut failed: buffer is read-only".to_owned());
                                continue;
                            }
                            Err(error) => {
                                failure =
                                    Some(format!("failed to read clipboard selection: {error}"));
                                break;
                            }
                        };
                        match clipboard::write_osc52(terminal.backend_mut(), &item) {
                            Ok(()) => {}
                            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                                message = Some(format!("cut failed: {error}"));
                                continue;
                            }
                            Err(error) => {
                                failure =
                                    Some(format!("failed to write terminal clipboard: {error}"));
                                break;
                            }
                        }
                        if let Err(error) = input_window.update(cx, |_root, window, cx| {
                            window.dispatch_action(Box::new(Cut), cx);
                        }) {
                            failure = Some(format!("failed to dispatch cut action: {error}"));
                            break;
                        }
                        message = Some("cut".to_owned());
                    }
                    TerminalEvent::Key(event) if input::is_intercepted_shortcut(&event) => {}
                    TerminalEvent::Key(event) if save_as_prompt.is_some() => {
                        let action = save_as_prompt
                            .as_mut()
                            .expect("Save As prompt checked above")
                            .prompt
                            .handle_key(&event);
                        if action != PromptAction::Ignored {
                            quit_armed = false;
                            message = None;
                        }

                        match action {
                            PromptAction::Changed => save_as_prompt
                                .as_mut()
                                .expect("Save As prompt checked above")
                                .text_changed(),
                            PromptAction::Submit | PromptAction::AlternateSubmit => {
                                let (path, overwrite_path) = {
                                    let save_as = save_as_prompt
                                        .as_ref()
                                        .expect("Save As prompt checked above");
                                    (
                                        save_as.prompt.text().to_owned(),
                                        save_as.overwrite_path.clone(),
                                    )
                                };
                                match save_document_as(
                                    &tabs[active_index].document,
                                    &services,
                                    &path,
                                    overwrite_path.as_deref(),
                                    cx,
                                )
                                .await
                                {
                                    Ok(SaveAsOutcome::ConfirmationRequired(path)) => {
                                        let save_as = save_as_prompt
                                            .as_mut()
                                            .expect("Save As prompt checked above");
                                        save_as.overwrite_path = Some(path);
                                        save_as.feedback = Some(
                                            "file exists; press Enter again to overwrite"
                                                .to_owned(),
                                        );
                                    }
                                    Ok(SaveAsOutcome::Saved(path)) => {
                                        save_as_prompt = None;
                                        message = Some("saved".to_owned());
                                        if let Err(error) = assign_file_language(
                                            &path,
                                            &tabs[active_index].document.buffer,
                                            services.language_registry.clone(),
                                            cx,
                                        )
                                        .await
                                        {
                                            message = Some(format!(
                                                "saved; language detection failed: {error:#}"
                                            ));
                                        }
                                    }
                                    Err(error) => {
                                        let save_as = save_as_prompt
                                            .as_mut()
                                            .expect("Save As prompt checked above");
                                        save_as.overwrite_path = None;
                                        save_as.feedback = Some(format!("save failed: {error:#}"));
                                    }
                                }
                            }
                            PromptAction::Cancel => {
                                save_as_prompt = None;
                                message = Some("save cancelled".to_owned());
                            }
                            PromptAction::CursorMoved
                            | PromptAction::Next
                            | PromptAction::Previous
                            | PromptAction::Ignored => {}
                        }
                    }
                    TerminalEvent::Key(event) if open_prompt.is_some() => {
                        let action = open_prompt
                            .as_mut()
                            .expect("Open prompt checked above")
                            .prompt
                            .handle_key(&event);
                        if action != PromptAction::Ignored {
                            quit_armed = false;
                            message = None;
                        }

                        match action {
                            PromptAction::Changed => open_prompt
                                .as_mut()
                                .expect("Open prompt checked above")
                                .text_changed(),
                            PromptAction::Submit | PromptAction::AlternateSubmit => {
                                let input = open_prompt
                                    .as_ref()
                                    .expect("Open prompt checked above")
                                    .prompt
                                    .text()
                                    .to_owned();
                                let document = match resolve_path(&input) {
                                    Ok(path) => {
                                        cx.update(|cx| {
                                            open_document(Some(path), services.clone(), cx)
                                        })
                                        .await
                                    }
                                    Err(error) => Err(error),
                                };

                                match document {
                                    Ok(document) => {
                                        if let Some(index) = tabs
                                            .iter()
                                            .position(|tab| tab.document.buffer == document.buffer)
                                        {
                                            active_index = index;
                                            open_prompt = None;
                                            message = Some("already open".to_owned());
                                        } else {
                                            match create_document_tab(
                                                document,
                                                services.buffer_store.clone(),
                                                redraw_sender.clone(),
                                                cx,
                                            ) {
                                                Ok(tab) => {
                                                    tabs.push(tab);
                                                    active_index = tabs.len() - 1;
                                                    open_prompt = None;
                                                    message = Some("opened".to_owned());
                                                }
                                                Err(error) => {
                                                    open_prompt
                                                        .as_mut()
                                                        .expect("Open prompt checked above")
                                                        .feedback =
                                                        Some(format!("open failed: {error:#}"));
                                                }
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        open_prompt
                                            .as_mut()
                                            .expect("Open prompt checked above")
                                            .feedback = Some(format!("open failed: {error:#}"));
                                    }
                                }
                            }
                            PromptAction::Cancel => {
                                open_prompt = None;
                                message = Some("open cancelled".to_owned());
                            }
                            PromptAction::CursorMoved
                            | PromptAction::Next
                            | PromptAction::Previous
                            | PromptAction::Ignored => {}
                        }
                    }
                    TerminalEvent::Key(event) if go_to_line_prompt.is_some() => {
                        let action = go_to_line_prompt
                            .as_mut()
                            .expect("Go to line prompt checked above")
                            .prompt
                            .handle_key(&event);
                        if action != PromptAction::Ignored {
                            quit_armed = false;
                            message = None;
                        }

                        match action {
                            PromptAction::Changed => go_to_line_prompt
                                .as_mut()
                                .expect("Go to line prompt checked above")
                                .text_changed(),
                            PromptAction::Submit | PromptAction::AlternateSubmit => {
                                let input = go_to_line_prompt
                                    .as_ref()
                                    .expect("Go to line prompt checked above")
                                    .prompt
                                    .text()
                                    .to_owned();
                                match parse_go_to_location(&input).and_then(|(line, column)| {
                                    go_to_location(&editor_window, line, column, cx)
                                }) {
                                    Ok(actual_line) => {
                                        go_to_line_prompt = None;
                                        message = Some(format!("line {actual_line}"));
                                    }
                                    Err(error) => {
                                        go_to_line_prompt
                                            .as_mut()
                                            .expect("Go to line prompt checked above")
                                            .feedback = Some(format!("go failed: {error:#}"));
                                    }
                                }
                            }
                            PromptAction::Cancel => {
                                go_to_line_prompt = None;
                                message = Some("go to line cancelled".to_owned());
                            }
                            PromptAction::CursorMoved
                            | PromptAction::Next
                            | PromptAction::Previous
                            | PromptAction::Ignored => {}
                        }
                    }
                    TerminalEvent::Key(event) if active_search.is_some() => {
                        let replace_enabled = active_search
                            .as_ref()
                            .expect("search checked above")
                            .replace_enabled;
                        if replace_enabled && let Some(field) = search_field_for_key(&event) {
                            let search = active_search.as_mut().expect("search checked above");
                            search.focused_field = field;
                            quit_armed = false;
                            message = None;
                            continue;
                        }

                        let focused_field = active_search
                            .as_ref()
                            .expect("search checked above")
                            .focused_field;
                        if replace_enabled
                            && focused_field == SearchField::Replacement
                            && is_replace_all(&event)
                        {
                            quit_armed = false;
                            message = None;
                            match replace_all_matches(
                                active_search.as_mut().expect("search checked above"),
                                &editor_window,
                                cx,
                            )
                            .await
                            {
                                Ok(Some(0)) => message = Some("no matches".to_owned()),
                                Ok(Some(count)) => {
                                    message = Some(format!("replaced {count} matches"))
                                }
                                Ok(None) => {
                                    active_search.as_mut().expect("search checked above").error =
                                        Some("buffer is read-only".to_owned());
                                }
                                Err(error) => {
                                    failure = Some(format!("buffer replace failed: {error:#}"));
                                    break;
                                }
                            }
                            continue;
                        }

                        let action = active_search
                            .as_mut()
                            .expect("search checked above")
                            .focused_prompt_mut()
                            .handle_key(&event);
                        if action != PromptAction::Ignored {
                            quit_armed = false;
                        }
                        if matches!(
                            action,
                            PromptAction::Changed
                                | PromptAction::Submit
                                | PromptAction::AlternateSubmit
                                | PromptAction::Next
                                | PromptAction::Previous
                                | PromptAction::Cancel
                        ) {
                            message = None;
                        }

                        let result = match (focused_field, action) {
                            (SearchField::Query, PromptAction::Changed) => {
                                refresh_search(
                                    active_search.as_mut().expect("search checked above"),
                                    &editor_window,
                                    cx,
                                )
                                .await
                            }
                            (SearchField::Replacement, PromptAction::Changed) => {
                                active_search.as_mut().expect("search checked above").error = None;
                                Ok(())
                            }
                            (SearchField::Replacement, PromptAction::Submit) => {
                                match replace_current_match(
                                    active_search.as_mut().expect("search checked above"),
                                    &editor_window,
                                    cx,
                                )
                                .await
                                {
                                    Ok(Some(0)) => {
                                        message = Some("no match".to_owned());
                                        Ok(())
                                    }
                                    Ok(Some(_)) => {
                                        message = Some("replaced 1 match".to_owned());
                                        Ok(())
                                    }
                                    Ok(None) => {
                                        active_search
                                            .as_mut()
                                            .expect("search checked above")
                                            .error = Some("buffer is read-only".to_owned());
                                        Ok(())
                                    }
                                    Err(error) => Err(error),
                                }
                            }
                            (_, PromptAction::Submit | PromptAction::Next) => step_search(
                                active_search.as_mut().expect("search checked above"),
                                Direction::Next,
                                &editor_window,
                                cx,
                            ),
                            (_, PromptAction::AlternateSubmit | PromptAction::Previous) => {
                                step_search(
                                    active_search.as_mut().expect("search checked above"),
                                    Direction::Prev,
                                    &editor_window,
                                    cx,
                                )
                            }
                            (_, PromptAction::Cancel) => close_search(&editor_window, cx),
                            (_, PromptAction::CursorMoved | PromptAction::Ignored) => Ok(()),
                        };
                        if let Err(error) = result {
                            failure = Some(format!("buffer search failed: {error:#}"));
                            break;
                        }
                        if action == PromptAction::Cancel {
                            active_search = None;
                        }
                    }
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
                    TerminalEvent::Paste(text) if save_as_prompt.is_some() => {
                        let save_as = save_as_prompt
                            .as_mut()
                            .expect("Save As prompt checked above");
                        if save_as.prompt.handle_paste(&text) == PromptAction::Changed {
                            quit_armed = false;
                            message = None;
                            save_as.text_changed();
                        }
                    }
                    TerminalEvent::Paste(text) if open_prompt.is_some() => {
                        let open = open_prompt.as_mut().expect("Open prompt checked above");
                        if open.prompt.handle_paste(&text) == PromptAction::Changed {
                            quit_armed = false;
                            message = None;
                            open.text_changed();
                        }
                    }
                    TerminalEvent::Paste(text) if go_to_line_prompt.is_some() => {
                        let go_to_line = go_to_line_prompt
                            .as_mut()
                            .expect("Go to line prompt checked above");
                        if go_to_line.prompt.handle_paste(&text) == PromptAction::Changed {
                            quit_armed = false;
                            message = None;
                            go_to_line.text_changed();
                        }
                    }
                    TerminalEvent::Paste(text) if active_search.is_some() => {
                        let focused_field = active_search
                            .as_ref()
                            .expect("search checked above")
                            .focused_field;
                        let action = active_search
                            .as_mut()
                            .expect("search checked above")
                            .focused_prompt_mut()
                            .handle_paste(&text);
                        if action == PromptAction::Changed {
                            quit_armed = false;
                            message = None;
                            if focused_field == SearchField::Query {
                                if let Err(error) = refresh_search(
                                    active_search.as_mut().expect("search checked above"),
                                    &editor_window,
                                    cx,
                                )
                                .await
                                {
                                    failure = Some(format!("buffer search failed: {error:#}"));
                                    break;
                                }
                            } else {
                                active_search.as_mut().expect("search checked above").error = None;
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
                    TerminalEvent::MouseScroll(direction) => {
                        if save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                            && active_search.is_none()
                        {
                            let tab = &mut tabs[active_index];
                            if scroll_viewport(
                                &mut tab.viewport,
                                snapshot.total_rows,
                                body_height,
                                direction,
                                3,
                            ) {
                                tab.manual_vertical_scroll = true;
                            }
                            message = None;
                        }
                    }
                    TerminalEvent::Mouse(mouse) => {
                        use crossterm::event::{
                            KeyModifiers, MouseButton, MouseEventKind,
                        };

                        if save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                            && active_search.is_none()
                            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
                            && mouse.modifiers == KeyModifiers::NONE
                            && let Some(position) = EditorWidget::new(&snapshot).text_position_at(
                                frame_area,
                                TerminalPosition::new(mouse.column, mouse.row),
                            )
                        {
                            if let Err(error) = move_caret_to_text_position(
                                &editor_window,
                                position,
                                cx,
                            ) {
                                failure = Some(format!(
                                    "failed to position editor caret: {error:#}"
                                ));
                                break;
                            }
                            message = None;
                        }
                    }
                    TerminalEvent::ReloadFinished { buffer_id, result } => {
                        let Some(reloaded_index) = tabs.iter().position(|tab| {
                            tab.document
                                .buffer
                                .read_with(cx, |buffer, _| buffer.remote_id().to_proto())
                                == buffer_id
                        }) else {
                            continue;
                        };

                        // An asynchronous status update must never leave a hidden
                        // destructive-action confirmation armed behind its message.
                        quit_armed = false;
                        close_armed = false;
                        reload_armed = false;
                        save_conflict_armed = false;
                        let label = document_label(&tabs[reloaded_index].document, true, cx);
                        let state = document_state(&tabs[reloaded_index].document, cx);

                        match result {
                            Ok(()) if state.has_external_change() => {
                                message = Some(format!(
                                    "{label} changed on disk; local edits were kept"
                                ));
                            }
                            Ok(()) => {
                                if reloaded_index == active_index
                                    && let Some(search) = active_search.as_mut()
                                    && let Err(error) =
                                        refresh_search(search, &editor_window, cx).await
                                {
                                    failure = Some(format!(
                                        "buffer search failed after automatic reload: {error:#}"
                                    ));
                                    break;
                                }
                                message = Some(format!("reloaded {label}"));
                            }
                            Err(error) => {
                                message = Some(format!("reload failed for {label}: {error}"));
                            }
                        }
                    }
                    TerminalEvent::Resize | TerminalEvent::Redraw => {}
                    TerminalEvent::Signal(signal) => {
                        failure = Some(format!("terminated by signal {signal}"));
                        break;
                    }
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

    input_reader.stop_and_join();
    terminal_session
        .restore()
        .context("failed to restore terminal")?;
    drop(input_reader);

    if let Ok(error) = error_receiver.try_recv() {
        return Err(io::Error::other(error).into());
    }

    Ok(())
}

fn resets_confirmation(
    event: &TerminalEvent,
    confirmation_key: fn(&crossterm::event::KeyEvent) -> bool,
) -> bool {
    matches!(
        event,
        TerminalEvent::Key(event)
            if event.kind == crossterm::event::KeyEventKind::Press
                && !confirmation_key(event)
    ) || matches!(
        event,
        TerminalEvent::Paste(_) | TerminalEvent::Mouse(_) | TerminalEvent::MouseScroll(_)
    )
}

fn search_field_for_key(event: &crossterm::event::KeyEvent) -> Option<SearchField> {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

    if event.kind != KeyEventKind::Press {
        return None;
    }
    match (event.code, event.modifiers) {
        (KeyCode::Tab, KeyModifiers::NONE) => Some(SearchField::Replacement),
        (KeyCode::BackTab, KeyModifiers::NONE | KeyModifiers::SHIFT) => Some(SearchField::Query),
        _ => None,
    }
}

fn is_replace_all(event: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Enter
        && matches!(event.modifiers, KeyModifiers::ALT | KeyModifiers::CONTROL)
}

#[derive(Clone)]
struct FileServices {
    buffer_store: Entity<BufferStore>,
    worktree_store: Entity<WorktreeStore>,
    file_system: Arc<dyn Fs>,
    language_registry: Arc<LanguageRegistry>,
}

struct OpenDocument {
    buffer: Entity<Buffer>,
    untitled_label: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DocumentState {
    path: Option<PathBuf>,
    dirty: bool,
    conflict: bool,
    deleted: bool,
}

impl DocumentState {
    fn has_file(&self) -> bool {
        self.path.is_some()
    }

    fn needs_discard_confirmation(&self) -> bool {
        self.dirty || self.deleted
    }

    fn has_external_change(&self) -> bool {
        self.conflict || self.deleted
    }
}

struct DocumentTab {
    document: OpenDocument,
    editor_window: WindowHandle<Editor>,
    viewport: Viewport,
    manual_vertical_scroll: bool,
    last_cursor: Option<Cursor>,
}

struct CapturedEditorFrame {
    snapshot: RenderSnapshot,
    manual_vertical_scroll: bool,
    last_cursor: Option<Cursor>,
}

fn file_services(cx: &mut App) -> FileServices {
    let language_registry = native_language_registry(cx);
    let file_system: Arc<dyn Fs> = Arc::new(RealFs::new(None, cx.background_executor().clone()));
    let worktree_store =
        cx.new(|cx| WorktreeStore::local(true, file_system.clone(), WorktreeIdCounter::get(cx)));
    let buffer_store = cx.new(|cx| BufferStore::local(worktree_store.clone(), cx));
    FileServices {
        buffer_store,
        worktree_store,
        file_system,
        language_registry,
    }
}

fn open_document(
    path: Option<PathBuf>,
    services: FileServices,
    cx: &mut App,
) -> Task<Result<OpenDocument>> {
    let Some(path) = path else {
        let buffer = services.buffer_store.update(cx, |store, cx| {
            store.create_local_buffer("", None, false, cx)
        });
        buffer
            .read(cx)
            .set_language_registry(services.language_registry.clone());
        return Task::ready(Ok(OpenDocument {
            buffer,
            untitled_label: Some("[No Name]".to_owned()),
        }));
    };

    cx.spawn(async move |cx| {
        let project_path = project_path_for_file(&path, &services, cx).await?;
        let buffer = services
            .buffer_store
            .update(cx, |store, cx| store.open_buffer(project_path, cx))
            .await
            .with_context(|| format!("could not load {}", path.display()))?;
        assign_file_language(&path, &buffer, services.language_registry.clone(), cx)
            .await
            .with_context(|| format!("could not select a language for {}", path.display()))?;

        Ok(OpenDocument {
            buffer,
            untitled_label: None,
        })
    })
}

fn document_state(document: &OpenDocument, cx: &gpui::AsyncApp) -> DocumentState {
    document.buffer.read_with(cx, |buffer, cx| {
        let (path, deleted) = match buffer.file() {
            Some(file) => {
                let path = file
                    .as_local()
                    .map(|file| file.abs_path(cx))
                    .unwrap_or_else(|| file.full_path(cx));
                (Some(path), file.disk_state().is_deleted())
            }
            None => (None, false),
        };
        DocumentState {
            path,
            dirty: buffer.is_dirty(),
            conflict: buffer.has_conflict(),
            deleted,
        }
    })
}

fn document_label(document: &OpenDocument, abbreviated: bool, cx: &gpui::AsyncApp) -> String {
    let state = document_state(document, cx);
    if let Some(path) = state.path {
        if abbreviated {
            return path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
        }
        return path.display().to_string();
    }
    document
        .untitled_label
        .clone()
        .unwrap_or_else(|| "[No Name]".to_owned())
}

async fn project_path_for_file(
    path: &Path,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<ProjectPath> {
    // A single-file worktree cannot observe a rename to a sibling. Use the
    // nearest existing directory, while never turning the filesystem root into
    // a recursive worktree. This also covers nested Save As paths.
    let mut candidate = path.parent();
    let worktree_root = loop {
        let Some(directory) = candidate.filter(|directory| directory.parent().is_some()) else {
            break path.to_path_buf();
        };
        match services
            .file_system
            .metadata(directory)
            .await
            .with_context(|| format!("could not inspect {}", directory.display()))?
        {
            Some(metadata) if metadata.is_dir => break directory.to_path_buf(),
            _ => candidate = directory.parent(),
        }
    };

    services
        .worktree_store
        .update(cx, |store, cx| {
            store.find_or_create_worktree(&worktree_root, false, cx)
        })
        .await
        .with_context(|| format!("could not create a worktree for {}", path.display()))?;

    services
        .worktree_store
        .read_with(cx, |store, cx| {
            store.project_path_for_absolute_path(path, cx)
        })
        .with_context(|| format!("worktree does not contain {}", path.display()))
}

fn native_language_registry(cx: &mut App) -> Arc<LanguageRegistry> {
    let registry = Arc::new(LanguageRegistry::new(cx.background_executor().clone()));
    registry.set_theme(cx.theme().clone());
    registry.register_native_grammars(grammars::native_grammars());

    for name in [
        "bash",
        "c",
        "cpp",
        "css",
        "diff",
        "go",
        "gomod",
        "gowork",
        "json",
        "jsonc",
        "markdown",
        "markdown-inline",
        "python",
        "rust",
        "tsx",
        "typescript",
        "javascript",
        "jsdoc",
        "regex",
        "yaml",
        "gitcommit",
        "zed-keybind-context",
    ] {
        let config = grammars::load_config(name);
        registry.register_language(
            config.name.clone(),
            config.grammar.clone(),
            config.matcher.clone(),
            config.hidden,
            None,
            Arc::new(move || {
                let config = config.clone();
                // Keep query compilation behind the registry loader. Eagerly constructing every
                // Language adds several seconds to startup even though one file selects one root.
                Box::pin(async move {
                    Ok(LoadedLanguage {
                        config,
                        queries: grammars::load_queries(name),
                        context_provider: None,
                        toolchain_provider: None,
                        manifest_name: None,
                    })
                })
            }),
        );
    }
    registry
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

async fn save_document(
    document: &OpenDocument,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let path = document_state(document, cx)
        .path
        .context("buffer has no file path")?;
    services
        .buffer_store
        .update(cx, |store, cx| {
            store.save_buffer(document.buffer.clone(), cx)
        })
        .await
        .with_context(|| format!("could not save {}", path.display()))
}

async fn reload_document(
    document: &OpenDocument,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let path = document_state(document, cx)
        .path
        .context("buffer has no file path")?;
    services
        .buffer_store
        .update(cx, |store, cx| {
            store.reload_buffers([document.buffer.clone()].into_iter().collect(), true, cx)
        })
        .await
        .with_context(|| format!("could not reload {}", path.display()))?;
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
enum SaveAsOutcome {
    ConfirmationRequired(PathBuf),
    Saved(PathBuf),
}

async fn save_document_as(
    document: &OpenDocument,
    services: &FileServices,
    input: &str,
    overwrite_path: Option<&Path>,
    cx: &mut gpui::AsyncApp,
) -> Result<SaveAsOutcome> {
    let path = resolve_path(input)?;
    if let Some(metadata) = services
        .file_system
        .metadata(&path)
        .await
        .with_context(|| format!("could not inspect {}", path.display()))?
    {
        if metadata.is_dir {
            bail!("{} is a directory", path.display());
        }
        if metadata.is_fifo || !services.file_system.is_file(&path).await {
            bail!("{} is not a regular file", path.display());
        }
        if overwrite_path != Some(path.as_path()) {
            return Ok(SaveAsOutcome::ConfirmationRequired(path));
        }
    }

    let project_path = project_path_for_file(&path, services, cx).await?;
    let open_buffer = services
        .buffer_store
        .read_with(cx, |store, _| store.get_by_path(&project_path));
    if open_buffer.is_some_and(|buffer| buffer != document.buffer) {
        bail!("{} is already open in another tab", path.display());
    }
    services
        .buffer_store
        .update(cx, |store, cx| {
            store.save_buffer_as(document.buffer.clone(), project_path, cx)
        })
        .await
        .with_context(|| format!("could not save {}", path.display()))?;

    Ok(SaveAsOutcome::Saved(path))
}

fn resolve_path(input: &str) -> Result<PathBuf> {
    if input.is_empty() {
        bail!("path is empty");
    }
    let input = PathBuf::from(input);
    std::path::absolute(&input)
        .with_context(|| format!("could not make {} absolute", input.display()))
}

fn untitled_label(id: usize) -> String {
    format!("Untitled {id}")
}

fn parse_go_to_location(input: &str) -> Result<(u32, Option<u32>)> {
    let mut components = input.splitn(2, ':').map(str::trim);
    let line = components
        .next()
        .unwrap_or_default()
        .parse::<u32>()
        .context("line must be an integer")?;

    let column = components
        .next()
        .map(|column| column.parse::<u32>().context("column must be an integer"))
        .transpose()?;
    Ok((line, column))
}

fn go_to_location(
    editor_window: &WindowHandle<Editor>,
    line: u32,
    column: Option<u32>,
    cx: &mut gpui::AsyncApp,
) -> Result<u32> {
    editor_window.update(cx, |editor, window, cx| -> Result<u32> {
        let buffer = editor
            .active_buffer(cx)
            .context("editor has no active buffer")?;
        let (anchor, actual_line) = {
            let snapshot = buffer.read(cx).snapshot();
            let row = line.saturating_sub(1).min(snapshot.max_point().row);
            let point =
                snapshot.point_from_external_input(row, column.unwrap_or(1).saturating_sub(1));
            let anchor = editor
                .buffer()
                .read(cx)
                .buffer_point_to_anchor(&buffer, point, cx)
                .context("target line is not present in the editor")?;
            (anchor, point.row.saturating_add(1))
        };

        editor.change_selections(
            SelectionEffects::scroll(Autoscroll::center()),
            window,
            cx,
            |selections| selections.select_anchor_ranges([anchor..anchor]),
        );
        Ok(actual_line)
    })?
}

fn move_caret_to_text_position(
    editor_window: &WindowHandle<Editor>,
    position: TextPosition,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    editor_window.update(cx, |editor, window, cx| {
        let Ok(row) = u32::try_from(position.row) else {
            return;
        };
        let Ok(column) = u32::try_from(position.byte_column) else {
            return;
        };

        // Resolve against the latest DisplaySnapshot. The screen hit test was
        // made from the previously rendered frame, so an asynchronous reparse
        // or reload may have changed which display positions are valid.
        let display = editor.display_snapshot(cx);
        let raw = DisplayPoint::new(DisplayRow(row), column);
        let previous = display.clip_point(raw, Bias::Left);
        let next = display.clip_point(raw, Bias::Right);
        let nearest = if previous == next {
            previous
        } else {
            match display.inlay_bias_at(raw) {
                Some(Bias::Left) => next,
                Some(Bias::Right) | None => previous,
            }
        };
        let anchor = display.display_point_to_anchor(nearest, Bias::Left);
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_anchor_ranges([anchor..anchor])
        });
    })?;
    Ok(())
}

async fn refresh_search(
    search: &mut ActiveSearch,
    editor_window: &WindowHandle<Editor>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    search.error = None;
    if search.prompt.text().is_empty() {
        editor_window.update(cx, |editor, window, cx| {
            editor.clear_matches(window, cx);
        })?;
        search.matches.clear();
        search.active_match = None;
        search.token = SearchToken::default();
        return Ok(());
    }

    let query = match build_search_query(search) {
        Ok(query) => query,
        Err(error) => {
            editor_window.update(cx, |editor, window, cx| {
                editor.clear_matches(window, cx);
            })?;
            search.matches.clear();
            search.active_match = None;
            search.error = Some(error.to_string());
            return Ok(());
        }
    };

    let find_matches = editor_window.update(cx, |editor, window, cx| {
        editor.find_matches_with_token(Arc::new(query), window, cx)
    })?;
    let (matches, token) = find_matches.await;
    let active_match = editor_window.update(cx, |editor, window, cx| {
        let active_match = editor.active_match_index(Direction::Next, &matches, token, window, cx);
        editor.update_matches(&matches, active_match, token, window, cx);
        if let Some(index) = active_match {
            editor.activate_match(index, &matches, token, window, cx);
        }
        active_match
    })?;

    search.matches = matches;
    search.active_match = active_match;
    search.token = token;
    Ok(())
}

fn build_search_query(search: &ActiveSearch) -> Result<SearchQuery> {
    SearchQuery::text(
        search.prompt.text(),
        false,
        false,
        false,
        Default::default(),
        Default::default(),
        false,
        None,
    )
}

async fn replace_current_match(
    search: &mut ActiveSearch,
    editor_window: &WindowHandle<Editor>,
    cx: &mut gpui::AsyncApp,
) -> Result<Option<usize>> {
    let Some(active_match) = search.active_match else {
        return Ok(Some(0));
    };
    let Some(search_match) = search.matches.get(active_match).cloned() else {
        return Ok(Some(0));
    };
    if editor_window.update(cx, |editor, _window, cx| editor.read_only(cx))? {
        return Ok(None);
    }

    let query = build_search_query(search)?.with_replacement(search.replacement.text().to_owned());
    editor_window.update(cx, |editor, window, cx| {
        editor.replace(&search_match, &query, search.token, window, cx);
    })?;

    // Advance through the pre-edit anchors before refreshing. This avoids selecting the same
    // occurrence again when its replacement also contains the query.
    step_search(search, Direction::Next, editor_window, cx)?;
    refresh_search(search, editor_window, cx).await?;
    Ok(Some(1))
}

async fn replace_all_matches(
    search: &mut ActiveSearch,
    editor_window: &WindowHandle<Editor>,
    cx: &mut gpui::AsyncApp,
) -> Result<Option<usize>> {
    if search.matches.is_empty() {
        return Ok(Some(0));
    }
    if editor_window.update(cx, |editor, _window, cx| editor.read_only(cx))? {
        return Ok(None);
    }

    let query = build_search_query(search)?.with_replacement(search.replacement.text().to_owned());
    let matches = search.matches.clone();
    editor_window.update(cx, |editor, window, cx| {
        editor.replace_all(&mut matches.iter(), &query, search.token, window, cx);
    })?;
    let replaced = matches.len();
    refresh_search(search, editor_window, cx).await?;
    Ok(Some(replaced))
}

fn step_search(
    search: &mut ActiveSearch,
    direction: Direction,
    editor_window: &WindowHandle<Editor>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    if search.matches.is_empty() {
        return Ok(());
    }

    let active_match = editor_window.update(cx, |editor, window, cx| {
        let index = match search.active_match {
            None => editor
                .active_match_index(direction, &search.matches, search.token, window, cx)
                .unwrap_or_default(),
            Some(current) => editor.match_index_for_direction(
                &search.matches,
                current,
                direction,
                1,
                search.token,
                window,
                cx,
            ),
        };
        editor.update_matches(&search.matches, Some(index), search.token, window, cx);
        editor.activate_match(index, &search.matches, search.token, window, cx);
        index
    })?;
    search.active_match = Some(active_match);
    Ok(())
}

fn close_search(editor_window: &WindowHandle<Editor>, cx: &mut gpui::AsyncApp) -> Result<()> {
    editor_window.update(cx, |editor, window, cx| {
        editor.clear_matches(window, cx);
        editor.search_bar_visibility_changed(false, window, cx);
    })
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

fn create_document_tab(
    document: OpenDocument,
    buffer_store: Entity<BufferStore>,
    redraw_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<DocumentTab> {
    let editor_window = cx.update(|cx| open_editor(document.buffer.clone(), cx))?;
    let buffer = document.buffer.clone();
    let buffer_id = buffer.read_with(cx, |buffer, _| buffer.remote_id().to_proto());
    if let Err(error) = editor_window.update(cx, |_editor, _window, cx| {
        cx.subscribe(&buffer, move |_, reload_buffer, event, cx| match event {
            BufferEvent::ReloadNeeded => {
                let reload = buffer_store.update(cx, |store, cx| {
                    store.reload_buffers([reload_buffer.clone()].into_iter().collect(), true, cx)
                });
                let sender = redraw_sender.clone();
                cx.spawn(async move |_, _| {
                    let result = match reload.await {
                        Ok(_) => Ok(()),
                        Err(error) => Err(format!("{error:#}")),
                    };
                    let _ = sender
                        .send(TerminalEvent::ReloadFinished { buffer_id, result })
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
                let _ = redraw_sender.try_send(TerminalEvent::Redraw);
            }
            _ => {}
        })
        .detach();
    }) {
        let _ = editor_window.update(cx, |_editor, window, _cx| window.remove_window());
        return Err(error).context("failed to observe buffer updates");
    }

    Ok(DocumentTab {
        document,
        editor_window,
        viewport: Viewport::default(),
        manual_vertical_scroll: false,
        last_cursor: None,
    })
}

fn tab_status(tabs: &[DocumentTab], active: usize, cx: &gpui::AsyncApp) -> String {
    let multiple = tabs.len() > 1;
    let labels = tabs
        .iter()
        .map(|tab| {
            let name = document_label(&tab.document, multiple, cx);
            let state = document_state(&tab.document, cx);
            TabLabel {
                name,
                dirty: state.dirty,
                conflict: state.has_external_change(),
            }
        })
        .collect::<Vec<_>>();
    tabs::format_status(&labels, active)
}

fn capture_editor(
    editor: &mut Editor,
    cx: &mut gpui::Context<Editor>,
    mut viewport: Viewport,
    mut manual_vertical_scroll: bool,
    mut last_cursor: Option<Cursor>,
    area: Rect,
    status_label: &str,
    message: Option<&str>,
    search: Option<&ActiveSearch>,
    save_as: Option<&SaveAsPrompt>,
    open: Option<&OpenPrompt>,
    go_to_line: Option<&GoToLinePrompt>,
) -> CapturedEditorFrame {
    let editor_style = editor.style(cx).clone();
    let display = editor.display_snapshot(cx);
    let total_rows = display.max_point().row().0 as usize + 1;
    let widest_line_number = display.widest_line_number();
    let cursor_point = editor.selections.newest_display(&display).head();
    let cursor = display_cursor_at(&display, cursor_point);
    let follow_vertical_cursor =
        update_vertical_follow(&mut manual_vertical_scroll, &mut last_cursor, Some(cursor));
    let body_height = usize::from(area.height.saturating_sub(1));
    let text_width = EditorWidget::text_width_for_widest_line_number(area, widest_line_number);
    keep_cursor_visible(
        &mut viewport,
        Some(cursor),
        text_width,
        area.height,
        follow_vertical_cursor,
    );
    viewport.top_row = viewport
        .top_row
        .min(max_viewport_top(total_rows, body_height));

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
    let selections = editor
        .selections
        .all_adjusted_display(&display)
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
        .collect();
    let line_styles = terminal_line_styles(
        &display,
        &editor_style,
        first_display_row,
        end_display_row,
        lines.len(),
    );
    let mut background_highlights = if first_row < end_row {
        let buffer = display.buffer_snapshot();
        let start_anchor = if first_row == 0 {
            Anchor::Min
        } else {
            buffer.anchor_before(visible_start.to_offset(&display, Bias::Left))
        };
        let end_anchor = if end_row >= total_rows {
            Anchor::Max
        } else {
            buffer.anchor_before(
                visible_end
                    .expect("a non-final visible range has an end point")
                    .to_offset(&display, Bias::Right),
            )
        };
        editor.background_highlights_in_range(start_anchor..end_anchor, &display, cx.theme())
    } else {
        Vec::new()
    };
    background_highlights.sort_by(|left, right| {
        left.0
            .start
            .cmp(&right.0.start)
            .then_with(|| left.0.end.cmp(&right.0.end))
            .then_with(|| left.1.cmp(&right.1))
    });
    let background_ranges = background_highlights
        .into_iter()
        .map(|(range, color)| BackgroundRange {
            range: SelectionRange {
                start: display_cursor_in_rows(&lines, first_row, range.start),
                end: display_cursor_in_rows(&lines, first_row, range.end),
            },
            style: TerminalStyle::new().bg(terminal_color(editor_style.background.blend(color))),
        })
        .collect();
    let text_style = terminal_text_style(&editor_style.text, editor_style.background);
    let gutter_style = TerminalStyle::new()
        .fg(terminal_color(
            editor_style
                .background
                .blend(cx.theme().colors().editor_line_number),
        ))
        .bg(terminal_color(editor_style.background));

    let (status, status_cursor_column) = if let Some(save_as) = save_as {
        let (status, cursor) = save_as.status(message);
        (status, Some(cursor))
    } else if let Some(open) = open {
        let (status, cursor) = open.status(message);
        (status, Some(cursor))
    } else if let Some(go_to_line) = go_to_line {
        let (status, cursor) = go_to_line.status(message);
        (status, Some(cursor))
    } else if let Some(search) = search {
        let (status, cursor) = search.status(message);
        (status, Some(cursor))
    } else {
        let mut status = format!(
            "zec {status_label}  Ctrl-N new  Ctrl-O open  Ctrl-W close  Ctrl-PgUp/PgDn tabs  Alt-PgUp/PgDn scroll  Ctrl-F find  Ctrl-H replace  Ctrl-G line  Ctrl-R reload  Ctrl-S save  Ctrl-Q quit"
        );
        if let Some(message) = message {
            status = format!("{message}  |  {status}");
        }
        (status, None)
    };

    CapturedEditorFrame {
        snapshot: RenderSnapshot {
            first_row,
            total_rows,
            lines,
            line_numbers,
            widest_line_number,
            cursor: Some(cursor),
            cursor_line_number,
            selections,
            text_style,
            gutter_style,
            line_styles,
            background_ranges,
            viewport,
            status,
            status_cursor_column,
        },
        manual_vertical_scroll,
        last_cursor,
    }
}

fn terminal_line_styles(
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

fn display_cursor_at(display: &DisplaySnapshot, point: DisplayPoint) -> Cursor {
    let row = point.row().0 as usize;
    let line = display.line(point.row());
    let column = terminal_column(&line, point.column() as usize);
    Cursor { row, column }
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

fn max_viewport_top(line_count: usize, body_height: usize) -> usize {
    line_count.saturating_sub(body_height.max(1))
}

fn update_vertical_follow(
    manual_vertical_scroll: &mut bool,
    last_cursor: &mut Option<Cursor>,
    cursor: Option<Cursor>,
) -> bool {
    if *last_cursor != cursor {
        *manual_vertical_scroll = false;
    }
    *last_cursor = cursor;
    !*manual_vertical_scroll
}

fn scroll_viewport(
    viewport: &mut Viewport,
    line_count: usize,
    body_height: usize,
    direction: ScrollDirection,
    amount: usize,
) -> bool {
    if body_height == 0 || amount == 0 {
        return false;
    }
    let max_top = max_viewport_top(line_count, body_height);
    let previous = viewport.top_row;
    let current = viewport.top_row.min(max_top);
    let next = match direction {
        ScrollDirection::Up => current.saturating_sub(amount),
        ScrollDirection::Down => current.saturating_add(amount).min(max_top),
    };
    viewport.top_row = next;
    next != previous
}

fn keep_cursor_visible(
    viewport: &mut Viewport,
    cursor: Option<Cursor>,
    width: u16,
    height: u16,
    follow_vertical_cursor: bool,
) {
    let Some(cursor) = cursor else {
        return;
    };

    let body_height = usize::from(height.saturating_sub(1));
    if follow_vertical_cursor && body_height > 0 {
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
    use editor::MultiBufferOffset;

    struct TemporaryTestFile {
        path: PathBuf,
        directory: PathBuf,
    }

    impl TemporaryTestFile {
        fn new(contents: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after the Unix epoch")
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "zec-external-reload-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir(&directory).expect("create external reload test directory");
            let path = directory.join("watched.txt");
            std::fs::write(&path, contents).expect("write initial external reload fixture");
            Self { path, directory }
        }
    }

    impl Drop for TemporaryTestFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_dir(&self.directory);
        }
    }

    fn command(arguments: &[&str]) -> Result<Command> {
        parse_command(arguments.iter().map(|argument| OsString::from(*argument)))
    }

    #[test]
    fn parses_cli_modes_and_file_paths() {
        assert_eq!(command(&[]).unwrap(), Command::Edit(Vec::new()));
        assert_eq!(
            command(&["notes.txt"]).unwrap(),
            Command::Edit(vec![PathBuf::from("notes.txt")])
        );
        assert_eq!(
            command(&["one.rs", "two.rs"]).unwrap(),
            Command::Edit(vec![PathBuf::from("one.rs"), PathBuf::from("two.rs")])
        );
        assert_eq!(command(&["--smoke"]).unwrap(), Command::Smoke);
        assert_eq!(command(&["--help"]).unwrap(), Command::Help);
        assert_eq!(
            command(&["--", "-draft.txt", "second.txt"]).unwrap(),
            Command::Edit(vec![
                PathBuf::from("-draft.txt"),
                PathBuf::from("second.txt")
            ])
        );
    }

    #[test]
    fn rejects_unknown_options_and_invalid_modes() {
        assert!(command(&["--wat"]).is_err());
        assert!(command(&["--"]).is_err());
        assert!(command(&["--help", "file.txt"]).is_err());
        assert!(command(&["--smoke", "file.txt"]).is_err());
    }

    #[test]
    fn makes_cli_paths_absolute_and_removes_exact_duplicates() {
        let paths = absolute_unique_paths(vec![
            PathBuf::from("one.rs"),
            PathBuf::from("one.rs"),
            PathBuf::from("two.rs"),
        ])
        .unwrap();

        assert_eq!(paths.len(), 2);
        assert!(paths.iter().all(|path| path.is_absolute()));
        assert!(paths[0].ends_with("one.rs"));
        assert!(paths[1].ends_with("two.rs"));
    }

    #[test]
    fn resolves_nonempty_prompt_paths_without_shell_expansion() {
        let relative = resolve_path("nested/file.rs").unwrap();
        assert!(relative.is_absolute());
        assert!(relative.ends_with("nested/file.rs"));

        let absolute = PathBuf::from("/tmp/zec-save-as.rs");
        assert_eq!(resolve_path(absolute.to_str().unwrap()).unwrap(), absolute);
        assert!(
            resolve_path("~/notes.txt")
                .unwrap()
                .ends_with("~/notes.txt")
        );
        assert!(resolve_path("").is_err());
    }

    #[test]
    fn captures_only_the_terminal_viewport_from_a_hundred_thousand_lines() {
        use std::fmt::Write as _;

        const DOCUMENT_ROWS: usize = 100_000;
        const BODY_ROWS: usize = 23;

        let mut text = String::with_capacity(DOCUMENT_ROWS * 12);
        for row in 0..DOCUMENT_ROWS {
            writeln!(&mut text, "row-{row:06}").expect("write large buffer fixture");
        }
        let (sender, receiver) = mpsc::sync_channel(1);

        gpui_platform::headless().run(move |cx| {
            init_zed(cx);
            let buffer = cx.new(|cx| Buffer::local(text, cx));
            let window = open_editor(buffer, cx).expect("open large editor");

            cx.spawn(async move |cx| {
                let result = window.update(cx, |editor, _window, cx| {
                    let area = Rect::new(0, 0, 80, 24);
                    let top = capture_editor(
                        editor,
                        cx,
                        Viewport::default(),
                        false,
                        None,
                        area,
                        "large",
                        None,
                        None,
                        None,
                        None,
                        None,
                    );
                    let middle = capture_editor(
                        editor,
                        cx,
                        Viewport {
                            top_row: 50_000,
                            left_column: 0,
                        },
                        true,
                        top.last_cursor,
                        area,
                        "large",
                        None,
                        None,
                        None,
                        None,
                        None,
                    );
                    let end = capture_editor(
                        editor,
                        cx,
                        Viewport {
                            top_row: usize::MAX,
                            left_column: 0,
                        },
                        true,
                        middle.last_cursor,
                        area,
                        "large",
                        None,
                        None,
                        None,
                        None,
                        None,
                    );
                    (top.snapshot, middle.snapshot, end.snapshot)
                });
                sender.send(result).expect("send large capture result");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });

        let (top, middle, end) = receiver
            .recv()
            .expect("receive large capture result")
            .expect("capture large editor");
        for snapshot in [&top, &middle, &end] {
            assert_eq!(snapshot.total_rows, DOCUMENT_ROWS + 1);
            assert_eq!(snapshot.lines.len(), BODY_ROWS);
            assert_eq!(snapshot.line_numbers.len(), BODY_ROWS);
            assert_eq!(snapshot.line_styles.len(), BODY_ROWS);
        }
        assert_eq!(top.first_row, 0);
        assert_eq!(top.lines.first().map(String::as_str), Some("row-000000"));
        assert_eq!(middle.first_row, 50_000);
        assert_eq!(middle.lines.first().map(String::as_str), Some("row-050000"));
        assert_eq!(end.first_row, DOCUMENT_ROWS + 1 - BODY_ROWS);
        assert_eq!(end.lines.last().map(String::as_str), Some(""));
    }

    #[test]
    fn gives_scratch_tabs_stable_distinct_labels() {
        assert_eq!(untitled_label(1), "Untitled 1");
        assert_eq!(untitled_label(2), "Untitled 2");
        assert_ne!(untitled_label(1), untitled_label(2));
    }

    #[test]
    fn parses_absolute_go_to_line_and_column_locations() {
        assert_eq!(parse_go_to_location("1").unwrap(), (1, None));
        assert_eq!(parse_go_to_location(" 12 : 3 ").unwrap(), (12, Some(3)));
        assert_eq!(parse_go_to_location("0:0").unwrap(), (0, Some(0)));
        assert_eq!(
            parse_go_to_location("4294967295").unwrap(),
            (u32::MAX, None)
        );

        for invalid in ["", "abc", "-1", "1:", "1:nope", "1:2:3", "4294967296"] {
            assert!(parse_go_to_location(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn clean_file_auto_reloads_without_project_and_reload_is_undoable() {
        use std::time::{Duration, Instant};

        const INITIAL: &str = "v1\n";
        const EXTERNAL: &str = "v2 changed externally\n";

        let file = TemporaryTestFile::new(INITIAL);
        let path = file.path.clone();
        let (sender, receiver) = mpsc::sync_channel(1);

        gpui_platform::headless().run(move |cx| {
            init_zed(cx);
            let services = file_services(cx);
            let document = open_document(Some(path.clone()), services.clone(), cx);

            cx.spawn(async move |cx| {
                let result: Result<_> = async {
                    let document = document.await?;
                    let (event_sender, _event_receiver) = async_channel::unbounded();
                    let tab = create_document_tab(
                        document,
                        services.buffer_store.clone(),
                        event_sender,
                        cx,
                    )?;

                    let (project_is_none, initial_text) = tab.editor_window.update(
                        cx,
                        |editor, _window, cx| (editor.project().is_none(), editor.text(cx)),
                    )?;
                    anyhow::ensure!(initial_text == INITIAL, "initial text was {initial_text:?}");

                    std::fs::write(&path, EXTERNAL)
                        .with_context(|| format!("could not externally write {}", path.display()))?;

                    let deadline = Instant::now() + Duration::from_secs(10);
                    let (reloaded_text, reloaded_dirty, reloaded_conflict) = loop {
                        let text = tab
                            .editor_window
                            .update(cx, |editor, _window, cx| editor.text(cx))?;
                        let (dirty, conflict) = tab.document.buffer.read_with(cx, |buffer, _| {
                            (buffer.is_dirty(), buffer.has_conflict())
                        });
                        if text == EXTERNAL {
                            break (text, dirty, conflict);
                        }
                        anyhow::ensure!(
                            Instant::now() < deadline,
                            "automatic reload timed out; last text was {text:?}, dirty={dirty}, conflict={conflict}"
                        );
                        cx.background_executor()
                            .timer(Duration::from_millis(25))
                            .await;
                    };

                    tab.editor_window
                        .update(cx, |editor, window, cx| editor.undo(&Undo, window, cx))?;
                    let text_after_undo = tab
                        .editor_window
                        .update(cx, |editor, _window, cx| editor.text(cx))?;
                    let (dirty_after_undo, conflict_after_undo) = tab
                        .document
                        .buffer
                        .read_with(cx, |buffer, _| (buffer.is_dirty(), buffer.has_conflict()));

                    Ok((
                        project_is_none,
                        reloaded_text,
                        reloaded_dirty,
                        reloaded_conflict,
                        text_after_undo,
                        dirty_after_undo,
                        conflict_after_undo,
                    ))
                }
                .await;

                sender.send(result).expect("send external reload result");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });

        let (
            project_is_none,
            reloaded_text,
            reloaded_dirty,
            reloaded_conflict,
            text_after_undo,
            dirty_after_undo,
            conflict_after_undo,
        ) = receiver
            .recv()
            .expect("receive external reload result")
            .expect("external reload should succeed");

        assert!(project_is_none);
        assert_eq!(reloaded_text, EXTERNAL);
        assert!(!reloaded_dirty);
        assert!(!reloaded_conflict);
        assert_eq!(text_after_undo, INITIAL);
        assert!(dirty_after_undo);
        assert!(!conflict_after_undo);
    }
    #[test]
    fn file_identity_follows_external_rename_and_delete_requires_confirmation() {
        use std::time::{Duration, Instant};

        const CONTENTS: &str = "identity stays in Zed\n";

        let file = TemporaryTestFile::new(CONTENTS);
        let original_path = file.path.clone();
        let renamed_path = file.directory.join("renamed.txt");
        let expected_renamed_path = renamed_path.clone();
        let (sender, receiver) = mpsc::sync_channel(1);

        gpui_platform::headless().run(move |cx| {
            init_zed(cx);
            let services = file_services(cx);
            let document = open_document(Some(original_path.clone()), services.clone(), cx);

            cx.spawn(async move |cx| {
                let result: Result<_> = async {
                    let document = document.await?;
                    let (event_sender, _event_receiver) = async_channel::unbounded();
                    let tab = create_document_tab(
                        document,
                        services.buffer_store.clone(),
                        event_sender,
                        cx,
                    )?;

                    std::fs::rename(&original_path, &renamed_path).with_context(|| {
                        format!(
                            "could not rename {} to {}",
                            original_path.display(),
                            renamed_path.display()
                        )
                    })?;

                    let deadline = Instant::now() + Duration::from_secs(10);
                    let renamed_state = loop {
                        let state = document_state(&tab.document, cx);
                        if state.path.as_deref() == Some(renamed_path.as_path()) {
                            break state;
                        }
                        anyhow::ensure!(
                            Instant::now() < deadline,
                            "rename was not reflected in Zed; last state was {state:?}"
                        );
                        cx.background_executor()
                            .timer(Duration::from_millis(25))
                            .await;
                    };

                    let reopened = cx
                        .update(|cx| {
                            open_document(Some(renamed_path.clone()), services.clone(), cx)
                        })
                        .await?;
                    let reopened_same_buffer = reopened.buffer == tab.document.buffer;
                    let text_after_rename = tab
                        .editor_window
                        .update(cx, |editor, _window, cx| editor.text(cx))?;

                    tab.editor_window.update(cx, |editor, window, cx| {
                        editor.insert("saved ", window, cx);
                    })?;
                    save_document(&tab.document, &services, cx).await?;
                    let saved_contents =
                        std::fs::read_to_string(&renamed_path).with_context(|| {
                            format!("could not read saved file {}", renamed_path.display())
                        })?;
                    let original_path_recreated = original_path.exists();

                    std::fs::remove_file(&renamed_path).with_context(|| {
                        format!("could not externally delete {}", renamed_path.display())
                    })?;
                    let deadline = Instant::now() + Duration::from_secs(10);
                    let deleted_state = loop {
                        let state = document_state(&tab.document, cx);
                        if state.deleted {
                            break state;
                        }
                        anyhow::ensure!(
                            Instant::now() < deadline,
                            "delete was not reflected in Zed; last state was {state:?}"
                        );
                        cx.background_executor()
                            .timer(Duration::from_millis(25))
                            .await;
                    };
                    let status = tab_status(std::slice::from_ref(&tab), 0, cx);

                    Ok((
                        renamed_state,
                        reopened_same_buffer,
                        text_after_rename,
                        saved_contents,
                        original_path_recreated,
                        deleted_state,
                        status,
                    ))
                }
                .await;

                sender.send(result).expect("send file identity result");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });

        let (renamed, same_buffer, text, saved, old_path_exists, deleted, status) = receiver
            .recv()
            .expect("receive file identity result")
            .expect("file identity scenario should succeed");
        assert_eq!(
            renamed.path.as_deref(),
            Some(expected_renamed_path.as_path())
        );
        assert!(!renamed.dirty);
        assert!(!renamed.deleted);
        assert!(same_buffer);
        assert_eq!(text, CONTENTS);
        assert_eq!(
            deleted.path.as_deref(),
            Some(expected_renamed_path.as_path())
        );
        assert!(
            !deleted.dirty,
            "Zed intentionally keeps clean deleted buffers clean"
        );
        assert!(deleted.deleted);
        assert!(deleted.needs_discard_confirmation());
        assert_eq!(saved, "saved identity stays in Zed\n");
        assert!(!old_path_exists);
        assert!(status.contains("renamed.txt!"), "status was {status:?}");
    }
    #[test]
    fn failed_zed_save_keeps_the_buffer_dirty_and_preserves_the_backup() {
        const INITIAL: &str = "disk original\n";
        const INSERTED: &str = "local ";

        let file = TemporaryTestFile::new(INITIAL);
        let path = file.path.clone();
        let backup_path = file.directory.join("original.backup");
        let (sender, receiver) = mpsc::sync_channel(1);

        gpui_platform::headless().run(move |cx| {
            init_zed(cx);
            let services = file_services(cx);
            let document = open_document(Some(path.clone()), services.clone(), cx);

            cx.spawn(async move |cx| {
                let result: Result<_> = async {
                    let document = document.await?;
                    let window = cx.update(|cx| open_editor(document.buffer.clone(), cx))?;
                    window.update(cx, |editor, window, cx| {
                        editor.insert(INSERTED, window, cx);
                    })?;

                    std::fs::copy(&path, &backup_path).with_context(|| {
                        format!(
                            "could not back up {} to {}",
                            path.display(),
                            backup_path.display()
                        )
                    })?;
                    std::fs::remove_file(&path)
                        .with_context(|| format!("could not remove {}", path.display()))?;
                    std::fs::create_dir(&path).with_context(|| {
                        format!("could not replace {} with a directory", path.display())
                    })?;

                    let save_error = save_document(&document, &services, cx)
                        .await
                        .expect_err("saving over a directory must fail")
                        .to_string();
                    let text = window.update(cx, |editor, _window, cx| editor.text(cx))?;
                    let state = document_state(&document, cx);
                    let backup_contents =
                        std::fs::read_to_string(&backup_path).with_context(|| {
                            format!("could not read backup {}", backup_path.display())
                        })?;

                    std::fs::remove_dir(&path).with_context(|| {
                        format!("could not remove directory {}", path.display())
                    })?;
                    std::fs::copy(&backup_path, &path).with_context(|| {
                        format!(
                            "could not restore {} from {}",
                            path.display(),
                            backup_path.display()
                        )
                    })?;
                    std::fs::remove_file(&backup_path).with_context(|| {
                        format!("could not remove backup {}", backup_path.display())
                    })?;

                    Ok((save_error, text, state, backup_contents))
                }
                .await;

                sender.send(result).expect("send failed save result");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });

        let (error, text, state, backup) = receiver
            .recv()
            .expect("receive failed save result")
            .expect("failed save scenario should complete");
        assert!(
            error.contains("could not save"),
            "unexpected save error: {error}"
        );
        assert_eq!(text, format!("{INSERTED}{INITIAL}"));
        assert!(state.dirty);
        assert_eq!(backup, INITIAL);
    }

    #[test]
    fn go_to_location_uses_zed_buffer_coordinates_and_preserves_buffer_undo() {
        use language::{Point, Selection, SelectionGoal};

        let (sender, receiver) = mpsc::sync_channel(1);

        gpui_platform::headless().run(move |cx| {
            init_zed(cx);
            let original = "first\n日本語abc\nlast\n";
            let buffer = cx.new(|cx| Buffer::local(original.to_owned(), cx));
            let window = open_editor(buffer, cx).expect("open editor");

            cx.spawn(async move |cx| {
                window
                    .update(cx, |editor, window, cx| {
                        editor.change_selections(Default::default(), window, cx, |selections| {
                            selections.select(vec![Selection {
                                id: 0,
                                start: Point::new(0, 0),
                                end: Point::new(0, 0),
                                reversed: false,
                                goal: SelectionGoal::None,
                            }]);
                        });
                        editor.insert("X", window, cx);
                        editor.change_selections(Default::default(), window, cx, |selections| {
                            selections.select(vec![
                                Selection {
                                    id: 0,
                                    start: Point::new(0, 0),
                                    end: Point::new(0, 0),
                                    reversed: false,
                                    goal: SelectionGoal::None,
                                },
                                Selection {
                                    id: 1,
                                    start: Point::new(2, 0),
                                    end: Point::new(2, 0),
                                    reversed: false,
                                    goal: SelectionGoal::None,
                                },
                            ]);
                        });
                    })
                    .expect("prepare editor");

                let actual_line =
                    go_to_location(&window, 2, Some(3), cx).expect("go to Unicode column");
                let (point, selection_count, text_after_jump) = window
                    .update(cx, |editor, _window, cx| {
                        let display = editor.display_snapshot(cx);
                        let selections = editor.selections.all::<MultiBufferOffset>(&display);
                        let point = editor
                            .buffer()
                            .read(cx)
                            .point_to_buffer_point(selections[0].head(), cx)
                            .expect("selection belongs to the singleton buffer")
                            .1;
                        (point, selections.len(), editor.text(cx))
                    })
                    .expect("read location");

                window
                    .update(cx, |editor, window, cx| editor.undo(&Undo, window, cx))
                    .expect("undo edit after jump");
                let text_after_undo = window
                    .update(cx, |editor, _window, cx| editor.text(cx))
                    .expect("read text after undo");

                go_to_location(&window, 2, Some(u32::MAX), cx).expect("clamp column");
                let column_clamped_point = window
                    .update(cx, |editor, _window, cx| {
                        let display = editor.display_snapshot(cx);
                        let head = editor
                            .selections
                            .newest::<MultiBufferOffset>(&display)
                            .head();
                        editor
                            .buffer()
                            .read(cx)
                            .point_to_buffer_point(head, cx)
                            .expect("selection belongs to the singleton buffer")
                            .1
                    })
                    .expect("read clamped column");

                let last_line =
                    go_to_location(&window, u32::MAX, Some(u32::MAX), cx).expect("clamp line");
                let final_point = window
                    .update(cx, |editor, _window, cx| {
                        let display = editor.display_snapshot(cx);
                        let head = editor
                            .selections
                            .newest::<MultiBufferOffset>(&display)
                            .head();
                        editor
                            .buffer()
                            .read(cx)
                            .point_to_buffer_point(head, cx)
                            .expect("selection belongs to the singleton buffer")
                            .1
                    })
                    .expect("read final location");

                sender
                    .send((
                        actual_line,
                        point,
                        selection_count,
                        text_after_jump,
                        text_after_undo,
                        column_clamped_point,
                        last_line,
                        final_point,
                    ))
                    .expect("send go-to-line results");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });

        let (
            actual_line,
            point,
            selection_count,
            text_after_jump,
            text_after_undo,
            column_clamped_point,
            last_line,
            final_point,
        ) = receiver.recv().expect("receive go-to-line results");

        assert_eq!(actual_line, 2);
        assert_eq!(point, Point::new(1, 6));
        assert_eq!(selection_count, 1);
        assert_eq!(text_after_jump, "Xfirst\n日本語abc\nlast\n");
        assert_eq!(text_after_undo, "first\n日本語abc\nlast\n");
        assert_eq!(column_clamped_point, Point::new(1, 12));
        assert_eq!(last_line, 4);
        assert_eq!(final_point, Point::new(3, 0));
    }

    #[test]
    fn mouse_caret_position_uses_zed_display_anchors_without_editing() {
        let (sender, receiver) = mpsc::sync_channel(1);

        gpui_platform::headless().run(move |cx| {
            init_zed(cx);
            let original = "first\n日本語abc\nlast\n";
            let buffer = cx.new(|cx| Buffer::local(original.to_owned(), cx));
            let window = open_editor(buffer.clone(), cx).expect("open editor");

            cx.spawn(async move |cx| {
                let result: Result<_> = (|| {
                    move_caret_to_text_position(
                        &window,
                        TextPosition {
                            row: 1,
                            byte_column: 3,
                        },
                        cx,
                    )?;
                    let (head, selection_count, text) =
                        window.update(cx, |editor, _window, cx| {
                            let display = editor.display_snapshot(cx);
                            let selections = editor.selections.all_display(&display);
                            (selections[0].head(), selections.len(), editor.text(cx))
                        })?;
                    let dirty = buffer.read_with(cx, |buffer, _| buffer.is_dirty());
                    Ok((head, selection_count, text, dirty))
                })();

                sender.send(result).expect("send mouse caret result");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });

        let (head, selection_count, text, dirty) = receiver
            .recv()
            .expect("receive mouse caret result")
            .expect("mouse caret positioning should succeed");
        assert_eq!(head, DisplayPoint::new(DisplayRow(1), 3));
        assert_eq!(selection_count, 1);
        assert_eq!(text, "first\n日本語abc\nlast\n");
        assert!(!dirty);
    }

    #[test]
    fn search_replace_uses_zed_anchors_and_undo_transactions() {
        let (sender, receiver) = mpsc::sync_channel(1);

        gpui_platform::headless().run(move |cx| {
            init_zed(cx);
            let original = "one two one";
            let buffer = cx.new(|cx| Buffer::local(original.to_owned(), cx));
            let window = open_editor(buffer, cx).expect("open editor");

            cx.spawn(async move |cx| {
                let mut search = ActiveSearch {
                    prompt: LinePrompt::with_text("one"),
                    replacement: LinePrompt::with_text("X"),
                    replace_enabled: true,
                    focused_field: SearchField::Replacement,
                    ..ActiveSearch::default()
                };

                refresh_search(&mut search, &window, cx)
                    .await
                    .expect("find initial matches");
                let initial_matches = search.matches.len();
                let single_count = replace_current_match(&mut search, &window, cx)
                    .await
                    .expect("replace current match");
                let single_text = window
                    .update(cx, |editor, _window, cx| editor.text(cx))
                    .expect("read single replacement");

                window
                    .update(cx, |editor, window, cx| editor.undo(&Undo, window, cx))
                    .expect("undo single replacement");
                let after_single_undo = window
                    .update(cx, |editor, _window, cx| editor.text(cx))
                    .expect("read single replacement undo");

                search.replacement = LinePrompt::with_text("one!");
                refresh_search(&mut search, &window, cx)
                    .await
                    .expect("refresh self-matching query");
                replace_current_match(&mut search, &window, cx)
                    .await
                    .expect("replace first self-matching occurrence");
                let self_matching_first_text = window
                    .update(cx, |editor, _window, cx| editor.text(cx))
                    .expect("read first self-matching replacement");
                replace_current_match(&mut search, &window, cx)
                    .await
                    .expect("advance past self-matching replacement");
                let self_matching_second_text = window
                    .update(cx, |editor, _window, cx| editor.text(cx))
                    .expect("read second self-matching replacement");

                window
                    .update(cx, |editor, window, cx| editor.undo(&Undo, window, cx))
                    .expect("undo self-matching replacements");
                let after_self_matching_undo = window
                    .update(cx, |editor, _window, cx| editor.text(cx))
                    .expect("read self-matching replacement undo");

                search.replacement = LinePrompt::with_text("X");
                refresh_search(&mut search, &window, cx)
                    .await
                    .expect("refresh matches after undo");
                let all_count = replace_all_matches(&mut search, &window, cx)
                    .await
                    .expect("replace all matches");
                let all_text = window
                    .update(cx, |editor, _window, cx| editor.text(cx))
                    .expect("read all replacements");

                window
                    .update(cx, |editor, window, cx| editor.undo(&Undo, window, cx))
                    .expect("undo all replacements");
                let after_all_undo = window
                    .update(cx, |editor, _window, cx| editor.text(cx))
                    .expect("read all replacement undo");

                sender
                    .send((
                        initial_matches,
                        single_count,
                        single_text,
                        after_single_undo,
                        self_matching_first_text,
                        self_matching_second_text,
                        after_self_matching_undo,
                        all_count,
                        all_text,
                        after_all_undo,
                    ))
                    .expect("send replacement results");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });

        let (
            initial_matches,
            single_count,
            single_text,
            after_single_undo,
            self_matching_first_text,
            self_matching_second_text,
            after_self_matching_undo,
            all_count,
            all_text,
            after_all_undo,
        ) = receiver.recv().expect("receive replacement results");

        assert_eq!(initial_matches, 2);
        assert_eq!(single_count, Some(1));
        assert_eq!(single_text.matches('X').count(), 1);
        assert_eq!(single_text.matches("one").count(), 1);
        assert_eq!(after_single_undo, "one two one");
        assert_eq!(self_matching_first_text, "one! two one");
        assert_eq!(self_matching_second_text, "one! two one!");
        assert_eq!(after_self_matching_undo, "one two one");
        assert_eq!(all_count, Some(2));
        assert_eq!(all_text, "X two X");
        assert_eq!(after_all_undo, "one two one");
    }

    #[test]
    fn open_status_tracks_unicode_cursor_and_clears_feedback_on_edit() {
        let mut open = OpenPrompt {
            prompt: LinePrompt::with_text("日本.rs"),
            feedback: Some("open failed".to_owned()),
        };

        let (status, cursor) = open.status(Some("notice"));
        assert!(status.starts_with("notice  |  Open: 日本.rs"));
        assert_eq!(cursor, "notice  |  Open: 日本.rs".width());

        open.text_changed();
        assert_eq!(open.feedback, None);
    }

    #[test]
    fn go_to_line_status_tracks_unicode_cursor_and_clears_feedback_on_edit() {
        let mut go_to_line = GoToLinePrompt {
            prompt: LinePrompt::with_text("日本:3"),
            feedback: Some("invalid line".to_owned()),
        };

        let (status, cursor) = go_to_line.status(Some("notice"));
        assert!(status.starts_with("notice  |  Go to line: 日本:3"));
        assert_eq!(cursor, "notice  |  Go to line: 日本:3".width());

        go_to_line.text_changed();
        assert_eq!(go_to_line.feedback, None);
    }

    #[test]
    fn replace_status_tracks_the_focused_unicode_prompt() {
        let mut search = ActiveSearch {
            prompt: LinePrompt::with_text("日本"),
            replacement: LinePrompt::with_text("世界"),
            replace_enabled: true,
            focused_field: SearchField::Query,
            ..ActiveSearch::default()
        };

        let (status, query_cursor) = search.status(Some("notice"));
        assert!(status.starts_with("notice  |  Find: 日本  Replace: 世界"));
        assert_eq!(query_cursor, "notice  |  Find: 日本".width());

        search.focused_field = SearchField::Replacement;
        let (_, replacement_cursor) = search.status(Some("notice"));
        assert_eq!(
            replacement_cursor,
            "notice  |  Find: 日本  Replace: 世界".width()
        );
    }

    #[test]
    fn replace_prompt_shortcuts_are_unambiguous_and_press_only() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

        assert_eq!(
            search_field_for_key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Some(SearchField::Replacement)
        );
        assert_eq!(
            search_field_for_key(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(SearchField::Query)
        );
        assert!(is_replace_all(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT
        )));
        assert!(is_replace_all(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL
        )));

        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert_eq!(
                search_field_for_key(&KeyEvent::new_with_kind(
                    KeyCode::Tab,
                    KeyModifiers::NONE,
                    kind,
                )),
                None
            );
            assert!(!is_replace_all(&KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::ALT,
                kind,
            )));
        }
        assert!(!is_replace_all(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        )));
        assert!(!is_replace_all(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT
        )));
    }

    #[test]
    fn another_input_action_resets_discard_confirmation() {
        use crossterm::event::{
            KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };

        let quit = TerminalEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        let other_press =
            TerminalEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let other_repeat = TerminalEvent::Key(KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Repeat,
        ));

        assert!(!resets_confirmation(&quit, input::is_quit));
        assert!(resets_confirmation(&other_press, input::is_quit));
        assert!(!resets_confirmation(&other_repeat, input::is_quit));
        assert!(resets_confirmation(
            &TerminalEvent::Paste("text".to_owned()),
            input::is_quit
        ));
        assert!(resets_confirmation(
            &TerminalEvent::MouseScroll(ScrollDirection::Down),
            input::is_quit
        ));
        assert!(resets_confirmation(
            &TerminalEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 3,
                row: 2,
                modifiers: KeyModifiers::NONE,
            }),
            input::is_quit
        ));
        assert!(!resets_confirmation(&TerminalEvent::Redraw, input::is_quit));
    }

    #[test]
    fn save_as_status_tracks_unicode_cursor_and_clears_overwrite_state_on_edit() {
        let mut save_as = SaveAsPrompt {
            prompt: LinePrompt::with_text("日本.rs"),
            overwrite_path: Some(PathBuf::from("/tmp/existing.rs")),
            feedback: Some("file exists".to_owned()),
        };

        let (status, cursor) = save_as.status(Some("unsaved"));
        assert!(status.starts_with("unsaved  |  Save as: 日本.rs"));
        assert_eq!(cursor, "unsaved  |  Save as: 日本.rs".width());

        save_as.text_changed();
        assert_eq!(save_as.overwrite_path, None);
        assert_eq!(save_as.feedback, None);
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

        keep_cursor_visible(
            &mut viewport,
            Some(Cursor { row: 4, column: 9 }),
            10,
            6,
            true,
        );
        assert_eq!(viewport, Viewport::default());

        keep_cursor_visible(
            &mut viewport,
            Some(Cursor { row: 5, column: 10 }),
            10,
            6,
            true,
        );
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
            true,
        );

        assert_eq!(text_width, 4);
        assert_eq!(viewport.left_column, 1);
    }

    #[test]
    fn manual_vertical_scroll_clamps_and_moves_in_display_rows() {
        let mut viewport = Viewport::default();

        let page_height = 9usize;
        assert!(scroll_viewport(
            &mut viewport,
            40,
            page_height,
            ScrollDirection::Down,
            page_height.saturating_sub(1),
        ));
        assert_eq!(viewport.top_row, 8);

        viewport.top_row = 0;
        assert!(scroll_viewport(
            &mut viewport,
            40,
            1,
            ScrollDirection::Down,
            1,
        ));
        assert_eq!(viewport.top_row, 1);

        viewport.top_row = 0;
        assert!(scroll_viewport(
            &mut viewport,
            20,
            5,
            ScrollDirection::Down,
            3,
        ));
        assert_eq!(viewport.top_row, 3);
        assert!(scroll_viewport(
            &mut viewport,
            20,
            5,
            ScrollDirection::Down,
            usize::MAX,
        ));
        assert_eq!(viewport.top_row, 15);
        assert!(scroll_viewport(
            &mut viewport,
            20,
            5,
            ScrollDirection::Up,
            4,
        ));
        assert_eq!(viewport.top_row, 11);

        viewport.top_row = 8;
        assert!(scroll_viewport(
            &mut viewport,
            3,
            5,
            ScrollDirection::Down,
            3,
        ));
        assert_eq!(viewport.top_row, 0);
        assert!(!scroll_viewport(
            &mut viewport,
            3,
            5,
            ScrollDirection::Up,
            3,
        ));
        assert!(!scroll_viewport(
            &mut viewport,
            20,
            0,
            ScrollDirection::Down,
            3,
        ));
        assert!(!scroll_viewport(
            &mut viewport,
            20,
            5,
            ScrollDirection::Down,
            0,
        ));
    }

    #[test]
    fn cursor_motion_resumes_vertical_follow_and_horizontal_follow_stays_active() {
        let cursor = Some(Cursor {
            row: 10,
            column: 10,
        });
        let mut manual_vertical_scroll = true;
        let mut last_cursor = cursor;

        assert!(!update_vertical_follow(
            &mut manual_vertical_scroll,
            &mut last_cursor,
            cursor,
        ));

        let moved_cursor = Some(Cursor {
            row: 11,
            column: 10,
        });
        assert!(update_vertical_follow(
            &mut manual_vertical_scroll,
            &mut last_cursor,
            moved_cursor,
        ));
        assert!(!manual_vertical_scroll);

        let mut viewport = Viewport {
            top_row: 3,
            left_column: 0,
        };
        keep_cursor_visible(&mut viewport, moved_cursor, 5, 6, false);
        assert_eq!(viewport.top_row, 3);
        assert_eq!(viewport.left_column, 6);
    }
}
