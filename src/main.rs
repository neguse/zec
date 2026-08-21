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
    Anchor, Editor, EditorStyle, MultiBufferOffset,
    actions::{Cut, Undo},
    display_map::{DisplayPoint, DisplayRow, DisplaySnapshot},
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
use ratatui::style::{
    Color as TerminalColor, Modifier as TerminalModifier, Style as TerminalStyle,
};
use render::{
    BackgroundRange, Cursor, EditorWidget, RenderSnapshot, SelectionRange, StyleSpan, Viewport,
};
use tabs::{Direction as TabDirection, TabLabel};
use terminal::{InputReader, TerminalEvent, TerminalSession, ZecTerminal};
use theme::ActiveTheme as _;
use unicode_width::UnicodeWidthStr as _;
use workspace::searchable::{Direction, SearchToken, SearchableItem as _};
use zed_fs::{Fs, RealFs};

const USAGE: &str = "Usage: zec [FILE ...]\n       zec --smoke\n\nKeys: Ctrl-PgUp/PgDn tabs, Ctrl-C copy, Ctrl-X cut, Ctrl-F find, Ctrl-S save, Ctrl-Q quit, Ctrl-Z undo";

#[derive(Debug, Eq, PartialEq)]
enum Command {
    Edit(Vec<PathBuf>),
    Smoke,
    Help,
}

#[derive(Debug)]
struct ActiveSearch {
    prompt: LinePrompt,
    matches: Vec<Range<Anchor>>,
    active_match: Option<usize>,
    token: SearchToken,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct SaveAsPrompt {
    prompt: LinePrompt,
    overwrite_path: Option<PathBuf>,
    feedback: Option<String>,
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
        let cursor_column = prompt_prefix.width().saturating_add(
            self.prompt
                .text()
                .get(..self.prompt.cursor())
                .unwrap_or_default()
                .width(),
        );
        let mut status = format!(
            "{prompt_prefix}{}  {position}/{}",
            self.prompt.text(),
            self.matches.len()
        );
        if let Some(error) = &self.error {
            status.push_str("  ");
            status.push_str(error);
        }
        (status, cursor_column)
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

    let terminal_session = TerminalSession::enter()?;
    let mut terminal = terminal_session.terminal()?;

    let (event_sender, event_receiver) = async_channel::unbounded();
    let redraw_sender = event_sender.clone();
    let input_reader = InputReader::spawn(event_sender);
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
            for document in documents {
                let document = match document.await {
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
                let editor_window = match cx.update(|cx| open_editor(document.buffer.clone(), cx)) {
                    Ok(window) => window,
                    Err(error) => {
                        let _ = error_sender
                            .try_send(format!("failed to open headless editor window: {error:#}"));
                        let _ = cx.update(|cx| cx.quit());
                        return;
                    }
                };
                let tab_redraw_sender = redraw_sender.clone();
                if let Err(error) = editor_window.update(cx, |_editor, _window, cx| {
                    cx.subscribe(&document.buffer, move |_, _, event, _| {
                        if matches!(
                            event,
                            BufferEvent::LanguageChanged(_) | BufferEvent::Reparsed
                        ) {
                            let _ = tab_redraw_sender.try_send(TerminalEvent::Redraw);
                        }
                    })
                    .detach();
                }) {
                    let _ = error_sender
                        .try_send(format!("failed to observe syntax updates: {error:#}"));
                    let _ = cx.update(|cx| cx.quit());
                    return;
                }
                tabs.push(DocumentTab {
                    document,
                    editor_window,
                    viewport: Viewport::default(),
                });
            }

            let mut active_index = 0;
            let mut failure = None;
            let mut message = None;
            let mut quit_armed = false;
            let mut active_search: Option<ActiveSearch> = None;
            let mut save_as_prompt: Option<SaveAsPrompt> = None;

            loop {
                let editor_window = tabs[active_index].editor_window;
                let input_window: AnyWindowHandle = editor_window.into();
                let status_label = tab_status(&tabs, active_index, cx);
                let viewport = tabs[active_index].viewport;
                let mut snapshot = match editor_window.update(cx, |editor, _window, cx| {
                    capture_editor(
                        editor,
                        cx,
                        viewport,
                        &status_label,
                        message.as_deref(),
                        active_search.as_ref(),
                        save_as_prompt.as_ref(),
                    )
                }) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        failure = Some(format!("failed to read editor state: {error}"));
                        break;
                    }
                };

                if let Err(error) = draw(
                    &mut terminal,
                    &mut snapshot,
                    &mut tabs[active_index].viewport,
                ) {
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
                        let dirty_count = tabs
                            .iter()
                            .filter(|tab| {
                                tab.document
                                    .buffer
                                    .read_with(cx, |buffer, _| buffer.is_dirty())
                            })
                            .count();
                        if dirty_count == 0 || quit_armed {
                            break;
                        }

                        quit_armed = true;
                        message = Some(format!(
                            "{dirty_count} unsaved tab(s); press Ctrl-Q again to discard"
                        ));
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
                            active_index = next_index;
                            quit_armed = false;
                            message = None;
                        }
                    }
                    TerminalEvent::Key(event) if input::is_save(&event) => {
                        quit_armed = false;
                        if save_as_prompt.is_none() {
                            if tabs[active_index].document.path.is_some() {
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
                        if save_as_prompt.is_none() && active_search.is_none() {
                            message = None;
                            if let Err(error) = editor_window.update(cx, |editor, window, cx| {
                                editor.search_bar_visibility_changed(true, window, cx);
                            }) {
                                failure = Some(format!("failed to start buffer search: {error}"));
                                break;
                            }
                            active_search = Some(ActiveSearch::default());
                        }
                    }
                    TerminalEvent::Key(event)
                        if save_as_prompt.is_none()
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
                                        tabs[active_index].document.path = Some(path.clone());
                                        tabs[active_index].document.label =
                                            path.display().to_string();
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
                    TerminalEvent::Key(event) if active_search.is_some() => {
                        let action = active_search
                            .as_mut()
                            .expect("search checked above")
                            .prompt
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

                        let result = match action {
                            PromptAction::Changed => {
                                refresh_search(
                                    active_search.as_mut().expect("search checked above"),
                                    &editor_window,
                                    cx,
                                )
                                .await
                            }
                            PromptAction::Submit | PromptAction::Next => step_search(
                                active_search.as_mut().expect("search checked above"),
                                Direction::Next,
                                &editor_window,
                                cx,
                            ),
                            PromptAction::AlternateSubmit | PromptAction::Previous => step_search(
                                active_search.as_mut().expect("search checked above"),
                                Direction::Prev,
                                &editor_window,
                                cx,
                            ),
                            PromptAction::Cancel => close_search(&editor_window, cx),
                            PromptAction::CursorMoved | PromptAction::Ignored => Ok(()),
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
                    TerminalEvent::Paste(text) if active_search.is_some() => {
                        let action = active_search
                            .as_mut()
                            .expect("search checked above")
                            .prompt
                            .handle_paste(&text);
                        if action == PromptAction::Changed {
                            quit_armed = false;
                            message = None;
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

#[derive(Clone)]
struct FileServices {
    buffer_store: Entity<BufferStore>,
    worktree_store: Entity<WorktreeStore>,
    file_system: Arc<dyn Fs>,
    language_registry: Arc<LanguageRegistry>,
}

struct OpenDocument {
    buffer: Entity<Buffer>,
    path: Option<PathBuf>,
    label: String,
}

struct DocumentTab {
    document: OpenDocument,
    editor_window: WindowHandle<Editor>,
    viewport: Viewport,
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
            path: None,
            label: "[No Name]".to_owned(),
        }));
    };

    let find_worktree = services.worktree_store.update(cx, |store, cx| {
        store.find_or_create_worktree(&path, false, cx)
    });

    cx.spawn(async move |cx| {
        let (worktree, relative_path) = find_worktree
            .await
            .with_context(|| format!("could not create a worktree for {}", path.display()))?;
        let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
        let buffer = services
            .buffer_store
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
        assign_file_language(&path, &buffer, services.language_registry.clone(), cx)
            .await
            .with_context(|| format!("could not select a language for {}", path.display()))?;

        Ok(OpenDocument {
            buffer,
            path: Some(path.clone()),
            label: path.display().to_string(),
        })
    })
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
    let path = document.path.as_ref().context("buffer has no file path")?;
    services
        .buffer_store
        .update(cx, |store, cx| {
            store.save_buffer(document.buffer.clone(), cx)
        })
        .await
        .with_context(|| format!("could not save {}", path.display()))
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
    let path = resolve_save_path(input)?;
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

    let (worktree, relative_path) = services
        .worktree_store
        .update(cx, |store, cx| {
            store.find_or_create_worktree(&path, false, cx)
        })
        .await
        .with_context(|| format!("could not create a worktree for {}", path.display()))?;
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    let project_path = ProjectPath {
        worktree_id,
        path: relative_path,
    };
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

fn resolve_save_path(input: &str) -> Result<PathBuf> {
    if input.is_empty() {
        bail!("path is empty");
    }
    let input = PathBuf::from(input);
    std::path::absolute(&input)
        .with_context(|| format!("could not make {} absolute", input.display()))
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

    let query = match SearchQuery::text(
        search.prompt.text(),
        false,
        false,
        false,
        Default::default(),
        Default::default(),
        false,
        None,
    ) {
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

fn tab_status(tabs: &[DocumentTab], active: usize, cx: &gpui::AsyncApp) -> String {
    let multiple = tabs.len() > 1;
    let labels = tabs
        .iter()
        .map(|tab| {
            let name = if multiple {
                tab.document
                    .path
                    .as_deref()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| tab.document.label.clone())
            } else {
                tab.document.label.clone()
            };
            let dirty = tab
                .document
                .buffer
                .read_with(cx, |buffer, _| buffer.is_dirty());
            TabLabel { name, dirty }
        })
        .collect::<Vec<_>>();
    tabs::format_status(&labels, active)
}

fn capture_editor(
    editor: &mut Editor,
    cx: &mut gpui::Context<Editor>,
    viewport: Viewport,
    status_label: &str,
    message: Option<&str>,
    search: Option<&ActiveSearch>,
    save_as: Option<&SaveAsPrompt>,
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
    let buffer = display.buffer_snapshot();
    let whole_buffer =
        buffer.anchor_before(MultiBufferOffset(0))..buffer.anchor_after(buffer.len());
    let mut background_highlights =
        editor.background_highlights_in_range(whole_buffer, &display, cx.theme());
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
                start: display_cursor(&lines, range.start),
                end: display_cursor(&lines, range.end),
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
    } else if let Some(search) = search {
        let (status, cursor) = search.status(message);
        (status, Some(cursor))
    } else {
        let mut status = format!(
            "zec {status_label}  Ctrl-PgUp/PgDn tabs  Ctrl-F find  Ctrl-S save  Ctrl-Q quit  Ctrl-Z undo"
        );
        if let Some(message) = message {
            status = format!("{message}  |  {status}");
        }
        (status, None)
    };

    RenderSnapshot {
        lines,
        line_numbers,
        widest_line_number,
        cursor: Some(cursor),
        selections,
        text_style,
        gutter_style,
        line_styles,
        background_ranges,
        viewport,
        status,
        status_cursor_column,
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
    fn resolves_nonempty_save_paths_without_shell_expansion() {
        let relative = resolve_save_path("nested/file.rs").unwrap();
        assert!(relative.is_absolute());
        assert!(relative.ends_with("nested/file.rs"));

        let absolute = PathBuf::from("/tmp/zec-save-as.rs");
        assert_eq!(
            resolve_save_path(absolute.to_str().unwrap()).unwrap(),
            absolute
        );
        assert!(resolve_save_path("").is_err());
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
