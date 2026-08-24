mod clipboard;
mod input;
mod prompt;
mod render;
mod repository;
mod tabs;
mod terminal;
mod tracing_fs;

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    env,
    ffi::OsString,
    io::{self, IsTerminal as _},
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::Duration,
};

use anyhow::{Context as _, Result, bail, ensure};
use editor::{
    Anchor, Bias, Editor, EditorStyle, SelectionEffects,
    actions::{Cut, SelectAll, Undo},
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
use repository::{
    CompletionDisposition, LatestSearch, LatestSearchState, ProjectSearchOutput, QuickOpenMatch,
    RepositoryIndex, RepositoryRoot, RunningLiteralSearchCancellation, SearchGeneration,
    start_literal_project_search,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tabs::{Direction as TabDirection, TabLabel};
use terminal::{InputReader, ScrollDirection, TerminalEvent, TerminalSession};
use theme::ActiveTheme as _;
use tracing_fs::{FsPathKind, RecordingFs, classify_single_file_accesses};
use unicode_width::UnicodeWidthStr as _;
use workspace::searchable::{Direction, SearchToken, SearchableItem as _};
use zed_fs::{Fs, RealFs};

const USAGE: &str = "Usage: zec [DIRECTORY | FILE ...]\n       zec --smoke\n\nKeys: Ctrl-N new, Ctrl-O open, Ctrl-P quick open, Alt-F project search, Ctrl-W close tab, Ctrl-PgUp/PgDn tabs, Alt-PgUp/PgDn scroll, Ctrl-C copy, Ctrl-X cut, Ctrl-F find, Ctrl-H replace, Ctrl-G line, Ctrl-R reload, Ctrl-S save, Ctrl-Q quit, Ctrl-Z undo";
const QUICK_OPEN_LIMIT: usize = 100;
const MAX_CONCURRENT_PROJECT_SEARCHES: usize = 2;
const PROJECT_SEARCH_DEBOUNCE: Duration = Duration::from_millis(16);

#[derive(Debug, Eq, PartialEq)]
enum Command {
    Edit(Vec<PathBuf>),
    Alpha1Probe(Alpha1Probe),
    Smoke,
    Help,
}
#[derive(Debug, Eq, PartialEq)]
enum Alpha1Probe {
    RootIdentity {
        root: PathBuf,
        inputs: Vec<Alpha1RootInput>,
    },
    OutsideTrace(PathBuf),
    ProjectSearch {
        root: PathBuf,
        query: String,
    },
    StaleResult(PathBuf),
    SearchFailure(PathBuf),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Alpha1RootInput {
    id: String,
    cwd: PathBuf,
    argument: Option<PathBuf>,
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

#[derive(Debug)]
struct QuickOpenPrompt {
    prompt: LinePrompt,
    matches: Vec<QuickOpenMatch>,
    selected: usize,
    feedback: Option<String>,
}

impl QuickOpenPrompt {
    fn new(index: &RepositoryIndex) -> Self {
        let mut prompt = Self {
            prompt: LinePrompt::new(),
            matches: Vec::new(),
            selected: 0,
            feedback: None,
        };
        prompt.refresh(index);
        prompt
    }

    fn refresh(&mut self, index: &RepositoryIndex) {
        self.matches = index.quick_open(self.prompt.text(), QUICK_OPEN_LIMIT);
        self.selected = 0;
        self.feedback = None;
    }

    fn step(&mut self, direction: TabDirection) {
        let Some(next) = tabs::adjacent_index(self.selected, self.matches.len(), direction) else {
            return;
        };
        self.selected = next;
    }

    fn selected_file_index(&self) -> Option<usize> {
        self.matches
            .get(self.selected)
            .map(|matched| matched.file_index())
    }

    fn status(&self, message: Option<&str>, index: &RepositoryIndex) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prefix = format!("{message_prefix}Quick open: ");
        let cursor_column = prefix.width().saturating_add(
            self.prompt
                .text()
                .get(..self.prompt.cursor())
                .unwrap_or_default()
                .width(),
        );
        let position = self
            .selected_file_index()
            .map_or(0, |_| self.selected.saturating_add(1));
        let selected_path = self
            .selected_file_index()
            .and_then(|file_index| index.file(file_index))
            .map(|file| file.relative_path())
            .unwrap_or("no matches");
        let mut status = format!(
            "{prefix}{}  {position}/{}  {selected_path}  Enter open  ↑/↓ select  Esc cancel",
            self.prompt.text(),
            self.matches.len()
        );
        if let Some(feedback) = &self.feedback {
            status.push_str("  |  ");
            status.push_str(feedback);
        }
        (status, cursor_column)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct ProjectSearchSessionId(u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ProjectSearchRequestKey {
    session: ProjectSearchSessionId,
    generation: SearchGeneration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ScheduledProjectSearch {
    key: ProjectSearchRequestKey,
    query: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProjectSearchChange {
    Key,
    Paste,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PendingProjectSearchReadiness {
    Debouncing,
    KeyEligible,
    PasteEligible,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingProjectSearch {
    request: ScheduledProjectSearch,
    readiness: PendingProjectSearchReadiness,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectSearchSchedule {
    next: Option<ScheduledProjectSearch>,
    debounce: Option<ProjectSearchRequestKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectSearchDebounceCompletion {
    accepted: bool,
    next: Option<ScheduledProjectSearch>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectSearchCompletion {
    was_active: bool,
    next: Option<ScheduledProjectSearch>,
}

/// Pure scheduling state for expensive Zed searches. A running request keeps
/// its slot until its completion event; replacing a query never treats Task
/// drop as proof that the underlying Zed workers have stopped.
#[derive(Default)]
struct ProjectSearchScheduler {
    next_session: u64,
    current_session: Option<ProjectSearchSessionId>,
    active: BTreeSet<ProjectSearchRequestKey>,
    pending: Option<PendingProjectSearch>,
    debounce: Option<ProjectSearchRequestKey>,
}

impl ProjectSearchScheduler {
    fn open_session(&mut self) -> Result<ProjectSearchSessionId> {
        self.next_session = self
            .next_session
            .checked_add(1)
            .context("project search session exhausted")?;
        let session = ProjectSearchSessionId(self.next_session);
        self.current_session = Some(session);
        self.pending = None;
        self.debounce = None;
        Ok(session)
    }

    fn close_session(&mut self, session: ProjectSearchSessionId) {
        if self.current_session == Some(session) {
            self.current_session = None;
            self.pending = None;
            self.debounce = None;
        }
    }

    fn clear_query(&mut self, session: ProjectSearchSessionId) -> Result<()> {
        ensure!(
            self.current_session == Some(session),
            "project search query belongs to an inactive prompt session"
        );
        self.pending = None;
        self.debounce = None;
        Ok(())
    }

    fn request(
        &mut self,
        request: ScheduledProjectSearch,
        change: ProjectSearchChange,
    ) -> Result<ProjectSearchSchedule> {
        ensure!(
            self.current_session == Some(request.key.session),
            "project search request belongs to an inactive prompt session"
        );
        let readiness = match change {
            ProjectSearchChange::Key => PendingProjectSearchReadiness::Debouncing,
            ProjectSearchChange::Paste => PendingProjectSearchReadiness::PasteEligible,
        };
        self.debounce = (change == ProjectSearchChange::Key).then_some(request.key);
        self.pending = Some(PendingProjectSearch { request, readiness });
        let next = self.take_dispatchable();
        Ok(ProjectSearchSchedule {
            next,
            debounce: self.debounce,
        })
    }

    fn debounce_elapsed(
        &mut self,
        key: ProjectSearchRequestKey,
    ) -> ProjectSearchDebounceCompletion {
        if self.debounce != Some(key) {
            return ProjectSearchDebounceCompletion {
                accepted: false,
                next: None,
            };
        }
        self.debounce = None;
        let Some(pending) = self.pending.as_mut() else {
            return ProjectSearchDebounceCompletion {
                accepted: true,
                next: None,
            };
        };
        if pending.request.key != key {
            return ProjectSearchDebounceCompletion {
                accepted: true,
                next: None,
            };
        }
        pending.readiness = PendingProjectSearchReadiness::KeyEligible;
        let next = self.take_dispatchable();
        ProjectSearchDebounceCompletion {
            accepted: true,
            next,
        }
    }

    fn finish(&mut self, key: ProjectSearchRequestKey) -> ProjectSearchCompletion {
        let was_active = self.active.remove(&key);
        let next = was_active.then(|| self.take_dispatchable()).flatten();
        ProjectSearchCompletion { was_active, next }
    }

    fn take_dispatchable(&mut self) -> Option<ScheduledProjectSearch> {
        let dispatchable = self
            .pending
            .as_ref()
            .is_some_and(|pending| match pending.readiness {
                PendingProjectSearchReadiness::Debouncing => false,
                PendingProjectSearchReadiness::KeyEligible => self.active.is_empty(),
                PendingProjectSearchReadiness::PasteEligible => {
                    self.active.len() < MAX_CONCURRENT_PROJECT_SEARCHES
                }
            });
        if !dispatchable {
            return None;
        }
        let pending = self
            .pending
            .take()
            .expect("eligible project search pending state disappeared");
        let inserted = self.active.insert(pending.request.key);
        debug_assert!(inserted, "project search request was dispatched twice");
        Some(pending.request)
    }

    #[cfg(test)]
    fn active_count(&self) -> usize {
        self.active.len()
    }

    #[cfg(test)]
    fn pending_query(&self) -> Option<&str> {
        self.pending
            .as_ref()
            .map(|pending| pending.request.query.as_str())
    }
}

struct ActiveProjectSearch {
    task: Task<()>,
    cancellation: Option<RunningLiteralSearchCancellation>,
}

/// Owns every in-flight task across prompt lifetimes. Debounce tasks may be
/// replaced freely; Zed search tasks are removed only after their finish event.
#[derive(Default)]
struct ProjectSearchCoordinator {
    scheduler: ProjectSearchScheduler,
    tasks: BTreeMap<ProjectSearchRequestKey, ActiveProjectSearch>,
    debounce_task: Option<Task<()>>,
}

impl ProjectSearchCoordinator {
    fn open_session(&mut self) -> Result<ProjectSearchSessionId> {
        self.debounce_task = None;
        self.scheduler.open_session()
    }

    fn close_session(&mut self, session: ProjectSearchSessionId) {
        self.cancel_session_searches(session);
        self.scheduler.close_session(session);
        self.debounce_task = None;
    }

    fn clear_query(&mut self, session: ProjectSearchSessionId) -> Result<()> {
        self.scheduler.clear_query(session)?;
        self.cancel_session_searches(session);
        self.debounce_task = None;
        Ok(())
    }

    fn schedule(
        &mut self,
        request: ScheduledProjectSearch,
        change: ProjectSearchChange,
        event_sender: async_channel::Sender<TerminalEvent>,
        cx: &mut gpui::AsyncApp,
    ) -> Result<Option<ScheduledProjectSearch>> {
        let session = request.key.session;
        let schedule = self.scheduler.request(request, change)?;
        if change == ProjectSearchChange::Paste {
            self.cancel_session_searches(session);
        }
        self.debounce_task = schedule.debounce.map(|request| {
            let timer = cx.background_executor().timer(PROJECT_SEARCH_DEBOUNCE);
            cx.spawn(async move |_cx| {
                timer.await;
                let _ = event_sender
                    .send(TerminalEvent::ProjectSearchDebounceElapsed { request })
                    .await;
            })
        });
        Ok(schedule.next)
    }

    fn debounce_elapsed(
        &mut self,
        request: ProjectSearchRequestKey,
    ) -> Option<ScheduledProjectSearch> {
        let completion = self.scheduler.debounce_elapsed(request);
        if completion.accepted {
            self.debounce_task = None;
        }
        completion.next
    }

    fn cancel_session_searches(&self, session: ProjectSearchSessionId) {
        for (request, active) in &self.tasks {
            if request.session == session {
                if let Some(cancellation) = &active.cancellation {
                    cancellation.cancel();
                }
            }
        }
    }

    fn attach(
        &mut self,
        request: ProjectSearchRequestKey,
        task: Task<()>,
        cancellation: Option<RunningLiteralSearchCancellation>,
    ) -> Result<()> {
        if !self.scheduler.active.contains(&request) {
            task.detach();
            bail!("project search task has no reserved scheduler slot");
        }
        match self.tasks.entry(request) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(ActiveProjectSearch { task, cancellation });
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                task.detach();
                bail!("project search request was dispatched twice")
            }
        }
    }

    fn finish(&mut self, request: ProjectSearchRequestKey) -> ProjectSearchCompletion {
        let completion = self.scheduler.finish(request);
        if completion.was_active {
            self.tasks.remove(&request);
        }
        completion
    }

    #[cfg(test)]
    fn task_count(&self) -> usize {
        self.tasks.len()
    }
}

impl Drop for ProjectSearchCoordinator {
    fn drop(&mut self) {
        for (_, active) in std::mem::take(&mut self.tasks) {
            if let Some(cancellation) = active.cancellation {
                cancellation.cancel();
            }
            // App shutdown must not synchronously drop Zed's scoped worker
            // pool on one of the same workers needed to drain that scope.
            active.task.detach();
        }
    }
}

struct ProjectSearchPrompt {
    session: ProjectSearchSessionId,
    prompt: LinePrompt,
    reducer: LatestSearch<String, ProjectSearchOutput, String>,
    selected: usize,
}

impl ProjectSearchPrompt {
    fn new(session: ProjectSearchSessionId) -> Self {
        Self {
            session,
            prompt: LinePrompt::new(),
            reducer: LatestSearch::default(),
            selected: 0,
        }
    }

    fn request(&mut self, query: String) -> Result<ScheduledProjectSearch> {
        let generation = self.reducer.begin(query.clone())?;
        Ok(ScheduledProjectSearch {
            key: ProjectSearchRequestKey {
                session: self.session,
                generation,
            },
            query,
        })
    }

    fn cancel_search(&mut self) {
        self.reducer.cancel();
    }

    fn complete(
        &mut self,
        request: ProjectSearchRequestKey,
        result: std::result::Result<ProjectSearchOutput, String>,
    ) -> CompletionDisposition {
        if request.session != self.session {
            return CompletionDisposition::DiscardedStale;
        }
        self.reducer.complete(request.generation, result)
    }

    fn output(&self) -> Option<&ProjectSearchOutput> {
        match self.reducer.state() {
            LatestSearchState::Ready { result, .. } => Some(result),
            _ => None,
        }
    }

    fn selected_hit(&self) -> Option<&repository::ProjectSearchHit> {
        self.output()
            .and_then(|output| output.matches.get(self.selected))
    }

    fn step(&mut self, direction: TabDirection) {
        let len = self.output().map_or(0, |output| output.matches.len());
        let Some(next) = tabs::adjacent_index(self.selected, len, direction) else {
            return;
        };
        self.selected = next;
    }

    fn status(&self, message: Option<&str>) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prefix = format!("{message_prefix}Project search: ");
        let cursor_column = prefix.width().saturating_add(
            self.prompt
                .text()
                .get(..self.prompt.cursor())
                .unwrap_or_default()
                .width(),
        );
        let detail = match self.reducer.state() {
            LatestSearchState::Idle => "type a case-sensitive literal query".to_owned(),
            LatestSearchState::Running { .. } => "searching…".to_owned(),
            LatestSearchState::Failed { error, .. } => format!("search failed: {error}"),
            LatestSearchState::Ready { result, .. } => {
                let position = if result.matches.is_empty() {
                    0
                } else {
                    self.selected.saturating_add(1)
                };
                let total = if result.source_limit_reached {
                    format!("{}+", result.total_hits)
                } else {
                    result.total_hits.to_string()
                };
                let selected = result
                    .matches
                    .get(self.selected)
                    .map(|hit| {
                        format!(
                            "{}:{}:{}  {}",
                            hit.summary.path,
                            hit.summary.line,
                            hit.summary.column,
                            hit.summary.preview
                        )
                    })
                    .unwrap_or_else(|| "no matches".to_owned());
                format!("{position}/{total}  {selected}")
            }
        };
        (
            format!(
                "{prefix}{}  {detail}  Enter open  ↑/↓ select  Esc cancel",
                self.prompt.text()
            ),
            cursor_column,
        )
    }
}

fn close_project_search_prompt(
    prompt: &mut Option<ProjectSearchPrompt>,
    coordinator: &mut ProjectSearchCoordinator,
) {
    if let Some(prompt) = prompt.take() {
        coordinator.close_session(prompt.session);
    }
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
        Command::Alpha1Probe(probe) => run_alpha_1_probe(probe),
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
    if first == "--alpha-1-probe" {
        let case = arguments
            .get(1)
            .and_then(|case| case.to_str())
            .context("--alpha-1-probe requires a UTF-8 case name")?;
        let one_path = |name: &str| -> Result<PathBuf> {
            ensure!(
                arguments.len() == 3,
                "--alpha-1-probe {name} requires exactly one path"
            );
            Ok(arguments[2].clone().into())
        };
        let probe = match case {
            "root-identity" => {
                ensure!(
                    arguments.len() == 4,
                    "--alpha-1-probe root-identity requires ROOT INPUTS_JSON"
                );
                let encoded = arguments[3]
                    .to_str()
                    .context("root-identity inputs must be UTF-8 JSON")?;
                let inputs =
                    serde_json::from_str(encoded).context("parse root-identity inputs JSON")?;
                Alpha1Probe::RootIdentity {
                    root: arguments[2].clone().into(),
                    inputs,
                }
            }
            "outside-trace" => Alpha1Probe::OutsideTrace(one_path(case)?),
            "stale-result" => Alpha1Probe::StaleResult(one_path(case)?),
            "search-failure" => Alpha1Probe::SearchFailure(one_path(case)?),
            "project-search" => {
                ensure!(
                    arguments.len() == 4,
                    "--alpha-1-probe project-search requires ROOT QUERY"
                );
                let query = arguments[3]
                    .clone()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("project-search query must be UTF-8"))?;
                Alpha1Probe::ProjectSearch {
                    root: arguments[2].clone().into(),
                    query,
                }
            }
            _ => bail!("unknown --alpha-1-probe case: {case}"),
        };
        return Ok(Command::Alpha1Probe(probe));
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

fn absolute_unique_paths_from(cwd: &Path, paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    ensure!(
        cwd.is_absolute(),
        "startup cwd must be absolute: {}",
        cwd.display()
    );
    let mut unique_paths = HashSet::new();
    paths
        .into_iter()
        .map(|input| {
            let path = if input.is_absolute() {
                input.clone()
            } else {
                cwd.join(&input)
            };
            std::path::absolute(&path).with_context(|| {
                format!(
                    "failed to make {} absolute from {}",
                    input.display(),
                    cwd.display()
                )
            })
        })
        .filter_map(|path| match path {
            Ok(path) if unique_paths.insert(path.clone()) => Some(Ok(path)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

#[cfg(test)]
fn absolute_unique_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    let cwd = env::current_dir().context("could not determine the current directory")?;
    absolute_unique_paths_from(&cwd, paths)
}

fn resolve_startup_invocation(cwd: &Path, arguments: Vec<PathBuf>) -> Result<(Vec<PathBuf>, bool)> {
    let implicit_root = arguments.is_empty();
    let paths = if implicit_root {
        vec![
            std::path::absolute(cwd)
                .with_context(|| format!("failed to make cwd {} absolute", cwd.display()))?,
        ]
    } else {
        absolute_unique_paths_from(cwd, arguments)?
    };
    Ok((paths, implicit_root))
}

fn run_interactive(paths: Vec<PathBuf>) -> Result<()> {
    let cwd = env::current_dir().context("could not determine the current directory")?;
    let (paths, implicit_root) = resolve_startup_invocation(&cwd, paths)?;

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
        cx.spawn(async move |cx| {
            let startup = match prepare_startup(paths, implicit_root, &services, cx).await {
                Ok(startup) => startup,
                Err(error) => {
                    let _ = error_sender.try_send(format!("failed to open repository: {error:#}"));
                    let _ = cx.update(|cx| cx.quit());
                    return;
                }
            };
            let repository = startup.repository;
            let mut startup_errors = startup.errors;
            let mut documents = startup.documents;
            if documents.is_empty() {
                match cx
                    .update(|cx| open_document(None, services.clone(), cx))
                    .await
                {
                    Ok(document) => documents.push(document),
                    Err(error) => {
                        let _ = error_sender
                            .try_send(format!("failed to create an empty buffer: {error:#}"));
                        let _ = cx.update(|cx| cx.quit());
                        return;
                    }
                }
            }

            let mut tabs = Vec::with_capacity(documents.len());
            let mut opened_buffer_ids = HashSet::new();
            let mut next_untitled_id = 1usize;
            for mut document in documents {
                if !opened_buffer_ids.insert(document.buffer.entity_id()) {
                    continue;
                }
                if document.untitled_label.is_some() {
                    document.untitled_label = Some(untitled_label(next_untitled_id));
                    next_untitled_id = next_untitled_id.saturating_add(1);
                }
                match create_document_tab(
                    document,
                    services.buffer_store.clone(),
                    redraw_sender.clone(),
                    cx,
                ) {
                    Ok(tab) => tabs.push(tab),
                    Err(error) => startup_errors
                        .push(format!("failed to open headless editor window: {error:#}")),
                }
            }
            if tabs.is_empty() {
                let error = if startup_errors.is_empty() {
                    "startup produced no editor tabs".to_owned()
                } else {
                    startup_errors.join("  |  ")
                };
                let _ = error_sender.try_send(error);
                let _ = cx.update(|cx| cx.quit());
                return;
            }

            let mut active_index = 0;
            let mut failure = None;
            let mut message = (!startup_errors.is_empty()).then(|| startup_errors.join("  |  "));
            let mut quit_armed = false;
            let mut close_armed = false;
            let mut reload_armed = false;
            let mut save_conflict_armed = false;
            let mut active_search: Option<ActiveSearch> = None;
            let mut save_as_prompt: Option<SaveAsPrompt> = None;
            let mut open_prompt: Option<OpenPrompt> = None;
            let mut go_to_line_prompt: Option<GoToLinePrompt> = None;
            let mut quick_open_prompt: Option<QuickOpenPrompt> = None;
            let mut project_search_prompt: Option<ProjectSearchPrompt> = None;
            let mut project_search_coordinator = ProjectSearchCoordinator::default();

            loop {
                let editor_window = tabs[active_index].editor_window;
                let input_window: AnyWindowHandle = editor_window.into();
                let mut status_label = tab_status(&tabs, active_index, cx);
                if let Some(repository) = &repository {
                    status_label = format!("{}  {status_label}", repository.root.label());
                }
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
                                    quick_open_prompt
                                        .as_ref()
                                        .zip(repository.as_ref().map(|repository| repository.index.as_ref())),
                                    project_search_prompt.as_ref(),
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
                        quick_open_prompt = None;
                        close_project_search_prompt(&mut project_search_prompt, &mut project_search_coordinator);
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
                            quick_open_prompt = None;
                            close_project_search_prompt(&mut project_search_prompt, &mut project_search_coordinator);
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
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
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
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
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
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
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
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
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
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
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
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
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
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
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
                    TerminalEvent::Key(event) if input::is_quick_open(&event) => {
                        quit_armed = false;
                        let Some(repository) = &repository else {
                            quick_open_prompt = None;
                            message =
                                Some("quick open requires a repository directory".to_owned());
                            continue;
                        };
                        if active_search.take().is_some()
                            && let Err(error) = close_search(&editor_window, cx)
                        {
                            failure = Some(format!("failed to close buffer search: {error:#}"));
                            break;
                        }
                        save_as_prompt = None;
                        open_prompt = None;
                        go_to_line_prompt = None;
                        close_project_search_prompt(&mut project_search_prompt, &mut project_search_coordinator);
                        quick_open_prompt = Some(QuickOpenPrompt::new(&repository.index));
                        message = None;
                    }

                    TerminalEvent::Key(event) if input::is_project_search(&event) => {
                        quit_armed = false;
                        let Some(_repository) = &repository else {
                            close_project_search_prompt(&mut project_search_prompt, &mut project_search_coordinator);
                            message =
                                Some("project search requires a repository directory".to_owned());
                            continue;
                        };
                        if active_search.take().is_some()
                            && let Err(error) = close_search(&editor_window, cx)
                        {
                            failure = Some(format!("failed to close buffer search: {error:#}"));
                            break;
                        }
                        save_as_prompt = None;
                        open_prompt = None;
                        go_to_line_prompt = None;
                        quick_open_prompt = None;
                        close_project_search_prompt(
                            &mut project_search_prompt,
                            &mut project_search_coordinator,
                        );
                        let session = match project_search_coordinator.open_session() {
                            Ok(session) => session,
                            Err(error) => {
                                failure =
                                    Some(format!("failed to open project search: {error:#}"));
                                break;
                            }
                        };
                        project_search_prompt = Some(ProjectSearchPrompt::new(session));
                        message = None;
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
                        quick_open_prompt = None;
                        close_project_search_prompt(&mut project_search_prompt, &mut project_search_coordinator);
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
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
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
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
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
                    TerminalEvent::Key(event) if quick_open_prompt.is_some() => {
                        let action = quick_open_prompt
                            .as_mut()
                            .expect("Quick open prompt checked above")
                            .prompt
                            .handle_key(&event);
                        if action != PromptAction::Ignored {
                            quit_armed = false;
                            message = None;
                        }

                        match action {
                            PromptAction::Changed => {
                                let repository =
                                    repository.as_ref().expect("Quick open requires repository");
                                quick_open_prompt
                                    .as_mut()
                                    .expect("Quick open prompt checked above")
                                    .refresh(&repository.index);
                            }
                            PromptAction::Next => quick_open_prompt
                                .as_mut()
                                .expect("Quick open prompt checked above")
                                .step(TabDirection::Next),
                            PromptAction::Previous => quick_open_prompt
                                .as_mut()
                                .expect("Quick open prompt checked above")
                                .step(TabDirection::Previous),
                            PromptAction::Submit | PromptAction::AlternateSubmit => {
                                let selected = {
                                    let repository =
                                        repository.as_ref().expect("Quick open requires repository");
                                    quick_open_prompt
                                        .as_ref()
                                        .expect("Quick open prompt checked above")
                                        .selected_file_index()
                                        .and_then(|file_index| {
                                            repository.index.file(file_index)
                                        })
                                        .and_then(|file| {
                                            file.project_path().cloned().map(|project_path| {
                                                (
                                                    project_path,
                                                    file.canonical_path().to_path_buf(),
                                                    file.relative_path().to_owned(),
                                                )
                                            })
                                        })
                                };
                                let Some((project_path, path, label)) = selected else {
                                    quick_open_prompt
                                        .as_mut()
                                        .expect("Quick open prompt checked above")
                                        .feedback = Some("no matching file".to_owned());
                                    continue;
                                };

                                match load_project_document(
                                    project_path,
                                    &path,
                                    &services,
                                    cx,
                                )
                                .await
                                {
                                    Ok(document) => {
                                        if let Some(index) = tabs.iter().position(|tab| {
                                            tab.document.buffer == document.buffer
                                        }) {
                                            active_index = index;
                                            quick_open_prompt = None;
                                            message = Some(format!("already open {label}"));
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
                                                    quick_open_prompt = None;
                                                    message = Some(format!("opened {label}"));
                                                }
                                                Err(error) => {
                                                    quick_open_prompt
                                                        .as_mut()
                                                        .expect(
                                                            "Quick open prompt checked above",
                                                        )
                                                        .feedback = Some(format!(
                                                        "open failed: {error:#}"
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        quick_open_prompt
                                            .as_mut()
                                            .expect("Quick open prompt checked above")
                                            .feedback =
                                            Some(format!("open failed: {error:#}"));
                                    }
                                }
                            }
                            PromptAction::Cancel => {
                                quick_open_prompt = None;
                                message = Some("quick open cancelled".to_owned());
                            }
                            PromptAction::CursorMoved | PromptAction::Ignored => {}
                        }
                    }

                    TerminalEvent::Key(event) if project_search_prompt.is_some() => {
                        let action = project_search_prompt
                            .as_mut()
                            .expect("Project search prompt checked above")
                            .prompt
                            .handle_key(&event);
                        if action != PromptAction::Ignored {
                            quit_armed = false;
                            message = None;
                        }

                        match action {
                            PromptAction::Changed => {
                                let repository = repository
                                    .as_ref()
                                    .expect("Project search requires repository");
                                if let Err(error) = begin_project_search(
                                    project_search_prompt
                                        .as_mut()
                                        .expect("Project search prompt checked above"),
                                    &mut project_search_coordinator,
                                    ProjectSearchChange::Key,
                                    repository,
                                    &services,
                                    redraw_sender.clone(),
                                    cx,
                                ) {
                                    message =
                                        Some(format!("project search failed: {error:#}"));
                                }
                            }
                            PromptAction::Next => project_search_prompt
                                .as_mut()
                                .expect("Project search prompt checked above")
                                .step(TabDirection::Next),
                            PromptAction::Previous => project_search_prompt
                                .as_mut()
                                .expect("Project search prompt checked above")
                                .step(TabDirection::Previous),
                            PromptAction::Submit | PromptAction::AlternateSubmit => {
                                let selected = project_search_prompt
                                    .as_ref()
                                    .expect("Project search prompt checked above")
                                    .selected_hit()
                                    .cloned();
                                let Some(hit) = selected else {
                                    message = Some("no project search result".to_owned());
                                    continue;
                                };
                                let repository = repository
                                    .as_ref()
                                    .expect("Project search requires repository");
                                let Some(indexed_file) =
                                    repository.index.file(hit.file_index)
                                else {
                                    message = Some(
                                        "project search result is no longer indexed".to_owned(),
                                    );
                                    continue;
                                };
                                let path = indexed_file.canonical_path().to_path_buf();
                                let label = format!(
                                    "{}:{}:{}",
                                    hit.summary.path,
                                    hit.summary.line,
                                    hit.summary.column
                                );

                                if let Err(error) = assign_file_language(
                                    &path,
                                    &hit.buffer,
                                    services.language_registry.clone(),
                                    cx,
                                )
                                .await
                                {
                                    message = Some(format!(
                                        "project search open failed: {error:#}"
                                    ));
                                    continue;
                                }

                                let already_open = tabs
                                    .iter()
                                    .position(|tab| tab.document.buffer == hit.buffer);
                                if let Some(index) = already_open {
                                    active_index = index;
                                } else {
                                    let document = OpenDocument {
                                        buffer: hit.buffer.clone(),
                                        untitled_label: None,
                                    };
                                    match create_document_tab(
                                        document,
                                        services.buffer_store.clone(),
                                        redraw_sender.clone(),
                                        cx,
                                    ) {
                                        Ok(tab) => {
                                            tabs.push(tab);
                                            active_index = tabs.len() - 1;
                                        }
                                        Err(error) => {
                                            message = Some(format!(
                                                "project search open failed: {error:#}"
                                            ));
                                            continue;
                                        }
                                    }
                                }
                                tabs[active_index].manual_vertical_scroll = false;
                                tabs[active_index].last_cursor = None;
                                let target_window = tabs[active_index].editor_window;
                                match move_caret_to_project_search_hit(&target_window, &hit, cx) {
                                    Ok(()) => {
                                        close_project_search_prompt(&mut project_search_prompt, &mut project_search_coordinator);
                                        message = Some(if already_open.is_some() {
                                            format!("already open {label}")
                                        } else {
                                            format!("opened {label}")
                                        });
                                    }
                                    Err(error) => {
                                        message = Some(format!(
                                            "project search open failed: {error:#}"
                                        ));
                                    }
                                }
                            }
                            PromptAction::Cancel => {
                                close_project_search_prompt(&mut project_search_prompt, &mut project_search_coordinator);
                                message = Some("project search cancelled".to_owned());
                            }
                            PromptAction::CursorMoved | PromptAction::Ignored => {}
                        }
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
                                let path = match &repository {
                                    Some(repository) => resolve_path_from(
                                        repository.root.requested_path(),
                                        &input,
                                    ),
                                    None => resolve_path(&input),
                                };
                                let document = match path {
                                    Ok(path) => match &repository {
                                        Some(repository) => {
                                            open_repository_document(
                                                &path,
                                                repository,
                                                &services,
                                                cx,
                                            )
                                            .await
                                        }
                                        None => {
                                            cx.update(|cx| {
                                                open_document(
                                                    Some(path),
                                                    services.clone(),
                                                    cx,
                                                )
                                            })
                                            .await
                                        }
                                    },
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
                    TerminalEvent::Paste(text) if project_search_prompt.is_some() => {
                        let changed = project_search_prompt
                            .as_mut()
                            .expect("Project search prompt checked above")
                            .prompt
                            .handle_paste(&text)
                            == PromptAction::Changed;
                        if changed {
                            quit_armed = false;
                            message = None;
                            let repository = repository
                                .as_ref()
                                .expect("Project search requires repository");
                            if let Err(error) = begin_project_search(
                                project_search_prompt
                                    .as_mut()
                                    .expect("Project search prompt checked above"),
                                &mut project_search_coordinator,
                                ProjectSearchChange::Paste,
                                repository,
                                &services,
                                redraw_sender.clone(),
                                cx,
                            ) {
                                message = Some(format!("project search failed: {error:#}"));
                            }
                        }
                    }
                    TerminalEvent::Paste(text) if quick_open_prompt.is_some() => {
                        let quick_open = quick_open_prompt
                            .as_mut()
                            .expect("Quick open prompt checked above");
                        if quick_open.prompt.handle_paste(&text) == PromptAction::Changed {
                            quit_armed = false;
                            message = None;
                            let repository =
                                repository.as_ref().expect("Quick open requires repository");
                            quick_open.refresh(&repository.index);
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
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
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
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
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
                    TerminalEvent::ProjectSearchDebounceElapsed { request } => {
                        let Some(next) = project_search_coordinator.debounce_elapsed(request) else {
                            continue;
                        };
                        let repository = repository
                            .as_ref()
                            .expect("scheduled project search requires repository");
                        if let Err(error) = dispatch_project_search(
                            &mut project_search_coordinator,
                            next,
                            repository,
                            &services,
                            redraw_sender.clone(),
                            cx,
                        ) {
                            message = Some(format!("project search failed: {error:#}"));
                        }
                    }
                    TerminalEvent::ProjectSearchFinished { request, result } => {
                        let completion = project_search_coordinator.finish(request);
                        let disposition = if completion.was_active {
                            project_search_prompt
                                .as_mut()
                                .map_or(CompletionDisposition::DiscardedStale, |prompt| {
                                    finish_project_search(prompt, request, result)
                                })
                        } else {
                            CompletionDisposition::DiscardedStale
                        };
                        if disposition == CompletionDisposition::Published {
                            quit_armed = false;
                            close_armed = false;
                            reload_armed = false;
                            save_conflict_armed = false;
                            message = None;
                        }
                        if let Some(next) = completion.next {
                            let repository = repository
                                .as_ref()
                                .expect("scheduled project search requires repository");
                            if let Err(error) = dispatch_project_search(
                                &mut project_search_coordinator,
                                next,
                                repository,
                                &services,
                                redraw_sender.clone(),
                                cx,
                            ) {
                                message = Some(format!("project search failed: {error:#}"));
                            }
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
                        if terminal::is_suspend_signal(signal) {
                            if let Err(error) = terminal::suspend_and_resume(&mut terminal) {
                                failure = Some(format!(
                                    "failed to suspend and resume terminal: {error}"
                                ));
                                break;
                            }
                            message = Some("resumed".to_owned());
                            continue;
                        }
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
    let file_system: Arc<dyn Fs> = Arc::new(RealFs::new(None, cx.background_executor().clone()));
    file_services_with_fs(cx, file_system)
}

fn file_services_with_fs(cx: &mut App, file_system: Arc<dyn Fs>) -> FileServices {
    let language_registry = native_language_registry(cx);
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

struct RepositorySession {
    root: RepositoryRoot,
    index: Arc<RepositoryIndex>,
    // The visible worktree is retained with the immutable index so every
    // repository path continues to resolve through the same Zed authority.
    _worktree: Entity<project::Worktree>,
}

struct StartupState {
    repository: Option<RepositorySession>,
    documents: Vec<OpenDocument>,
    errors: Vec<String>,
}

async fn prepare_startup(
    paths: Vec<PathBuf>,
    implicit_root: bool,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<StartupState> {
    let first_path = paths
        .first()
        .context("startup requires a repository or file path")?;
    let first_is_directory = if implicit_root {
        true
    } else {
        matches!(
            services.file_system.metadata(first_path).await,
            Ok(Some(metadata)) if metadata.is_dir
        )
    };

    if !first_is_directory {
        let mut documents = Vec::with_capacity(paths.len());
        let mut errors = Vec::new();
        for path in paths {
            let result = cx
                .update(|cx| open_document(Some(path.clone()), services.clone(), cx))
                .await;
            match result {
                Ok(document) => documents.push(document),
                Err(error) => errors.push(format_open_error(&path, &error)),
            }
        }
        return Ok(StartupState {
            repository: None,
            documents,
            errors,
        });
    }

    let repository = prepare_repository(first_path, services, cx).await?;
    let mut documents = Vec::new();
    let mut errors = Vec::new();

    let initial_file = repository
        .index
        .file_for_alias("README.md")
        .or_else(|| repository.index.files().first())
        .map(|file| {
            (
                file.project_path().cloned(),
                file.canonical_path().to_path_buf(),
            )
        });
    if let Some((project_path, path)) = initial_file {
        let result = match project_path {
            Some(project_path) => load_project_document(project_path, &path, services, cx).await,
            None => Err(anyhow::anyhow!(
                "indexed repository file has no Zed project path: {}",
                path.display()
            )),
        };
        match result {
            Ok(document) => documents.push(document),
            Err(error) => errors.push(format_open_error(&path, &error)),
        }
    }

    for path in paths.into_iter().skip(1) {
        match open_repository_document(&path, &repository, services, cx).await {
            Ok(document) => documents.push(document),
            Err(error) => errors.push(format_open_error(&path, &error)),
        }
    }

    Ok(StartupState {
        repository: Some(repository),
        documents,
        errors,
    })
}

async fn prepare_repository(
    root_path: &Path,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<RepositorySession> {
    let root = RepositoryRoot::open(root_path, services.file_system.as_ref()).await?;
    let canonical_root = root.canonical_path().to_path_buf();
    let (worktree, relative_path) = services
        .worktree_store
        .update(cx, |store, cx| {
            store.find_or_create_worktree(&canonical_root, true, cx)
        })
        .await
        .with_context(|| {
            format!(
                "could not create repository worktree {}",
                canonical_root.display()
            )
        })?;
    ensure!(
        relative_path.as_unix_str().is_empty(),
        "repository root was absorbed by another worktree"
    );
    let (actual_root, visible, scan_complete) = worktree.read_with(cx, |worktree, _| {
        (
            worktree.abs_path(),
            worktree.is_visible(),
            worktree.as_local().map(|worktree| worktree.scan_complete()),
        )
    });
    ensure!(
        actual_root.as_ref() == canonical_root,
        "Zed worktree root {} did not match repository root {}",
        actual_root.display(),
        canonical_root.display()
    );
    ensure!(visible, "repository worktree must be visible");
    let scan_complete = scan_complete.context("repository worktree must be local")?;
    scan_complete.await;

    let index = Arc::new(worktree.read_with(cx, |worktree, _| {
        RepositoryIndex::from_worktree(root.clone(), worktree)
    })?);

    Ok(RepositorySession {
        root,
        index,
        _worktree: worktree,
    })
}

async fn open_repository_document(
    requested_path: &Path,
    repository: &RepositorySession,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<OpenDocument> {
    let absolute_path = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        repository.root.requested_path().join(requested_path)
    };
    let canonical_path = services
        .file_system
        .canonicalize(&absolute_path)
        .await
        .with_context(|| format!("could not resolve {}", absolute_path.display()))?;

    if let Some(indexed) = repository.index.file_for_canonical_path(&canonical_path) {
        let project_path = indexed.project_path().cloned().with_context(|| {
            format!(
                "indexed repository file has no Zed project path: {}",
                indexed.relative_path()
            )
        })?;
        return load_project_document(project_path, indexed.canonical_path(), services, cx).await;
    }

    if canonical_path.starts_with(repository.root.canonical_path()) {
        let project_path = services
            .worktree_store
            .read_with(cx, |store, cx| {
                store.project_path_for_absolute_path(&canonical_path, cx)
            })
            .with_context(|| {
                format!(
                    "repository worktree does not contain {}",
                    canonical_path.display()
                )
            })?;
        return load_project_document(project_path, &canonical_path, services, cx).await;
    }

    open_single_file_document(&canonical_path, services, cx).await
}

async fn open_single_file_document(
    canonical_path: &Path,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<OpenDocument> {
    let metadata = services
        .file_system
        .metadata(canonical_path)
        .await
        .with_context(|| format!("could not inspect {}", canonical_path.display()))?
        .with_context(|| format!("file does not exist: {}", canonical_path.display()))?;
    ensure!(
        !metadata.is_dir && !metadata.is_fifo,
        "path is not a regular file: {}",
        canonical_path.display()
    );

    let (worktree, relative_path) = services
        .worktree_store
        .update(cx, |store, cx| {
            // A repository-external file is intentionally a non-scanning
            // single-file worktree. Existing repository worktrees keep their
            // scanners; this prevents the outside file from probing or
            // watching its parent and siblings.
            store.disable_scanner();
            store.find_or_create_worktree(canonical_path, false, cx)
        })
        .await
        .with_context(|| {
            format!(
                "could not create a single-file worktree for {}",
                canonical_path.display()
            )
        })?;
    ensure!(
        worktree.read_with(cx, |worktree, _| worktree.is_single_file()),
        "outside file was not opened through a single-file worktree: {}",
        canonical_path.display()
    );
    let project_path = ProjectPath {
        worktree_id: worktree.read_with(cx, |worktree, _| worktree.id()),
        path: relative_path,
    };
    load_project_document(project_path, canonical_path, services, cx).await
}

async fn load_project_document(
    project_path: ProjectPath,
    path: &Path,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<OpenDocument> {
    let buffer = services
        .buffer_store
        .update(cx, |store, cx| store.open_buffer(project_path, cx))
        .await
        .with_context(|| format!("could not load {}", path.display()))?;
    assign_file_language(path, &buffer, services.language_registry.clone(), cx)
        .await
        .with_context(|| format!("could not select a language for {}", path.display()))?;

    Ok(OpenDocument {
        buffer,
        untitled_label: None,
    })
}

fn format_open_error(path: &Path, error: &anyhow::Error) -> String {
    let details = format!("{error:#}");
    let is_eloop = details.contains("Too many levels of symbolic links")
        || error.chain().any(|cause| {
            cause
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.raw_os_error() == Some(nix::libc::ELOOP))
        })
        // BufferStore's asynchronous load path can flatten the source error.
        // Re-classify the same local path for the user-facing startup error so
        // a real symlink loop remains distinguishable from an ordinary open
        // failure. This is diagnostic-only; Zed still owns the actual load.
        || std::fs::canonicalize(path)
            .is_err_and(|error| error.raw_os_error() == Some(nix::libc::ELOOP));
    if is_eloop {
        format!("ELOOP opening {}: {details}", path.display())
    } else {
        format!("failed to open {}: {details}", path.display())
    }
}

struct ProjectSearchCommand {
    request: ProjectSearchRequestKey,
    completion: Task<std::result::Result<ProjectSearchOutput, String>>,
    cancellation: Option<RunningLiteralSearchCancellation>,
}

fn start_project_search_command_with<F>(
    request: ScheduledProjectSearch,
    cx: &mut gpui::AsyncApp,
    start: F,
) -> Result<ProjectSearchCommand>
where
    F: FnOnce(
        String,
        &mut gpui::AsyncApp,
    ) -> Result<Task<std::result::Result<ProjectSearchOutput, String>>>,
{
    let key = request.key;
    let completion = start(request.query, cx)?;
    Ok(ProjectSearchCommand {
        request: key,
        completion,
        cancellation: None,
    })
}

fn start_zed_project_search_command(
    request: ScheduledProjectSearch,
    repository: &RepositorySession,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<ProjectSearchCommand> {
    let key = request.key;
    let index = repository.index.clone();
    let file_system = services.file_system.clone();
    let buffer_store = services.buffer_store.clone();
    let worktree_store = services.worktree_store.clone();
    let running = cx.update(|cx| {
        start_literal_project_search(request.query, file_system, buffer_store, worktree_store, cx)
    })?;
    let cancellation = running.cancellation_handle();
    let completion = cx.spawn(async move |cx| {
        running
            .collect(index.as_ref(), cx)
            .await
            .map_err(|error| format!("{error:#}"))
    });
    Ok(ProjectSearchCommand {
        request: key,
        completion,
        cancellation: Some(cancellation),
    })
}

fn project_search_command_parts(
    key: ProjectSearchRequestKey,
    command: Result<ProjectSearchCommand>,
) -> (
    Task<std::result::Result<ProjectSearchOutput, String>>,
    Option<RunningLiteralSearchCancellation>,
) {
    match command {
        Ok(command) => {
            debug_assert_eq!(command.request, key);
            (command.completion, command.cancellation)
        }
        Err(error) => (Task::ready(Err(format!("{error:#}"))), None),
    }
}

fn dispatch_project_search(
    coordinator: &mut ProjectSearchCoordinator,
    request: ScheduledProjectSearch,
    repository: &RepositorySession,
    services: &FileServices,
    event_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let key = request.key;
    let command = start_zed_project_search_command(request, repository, services, cx);
    let (completion, cancellation) = project_search_command_parts(key, command);
    let task = cx.spawn(async move |_cx| {
        let result = completion.await;
        let _ = event_sender
            .send(TerminalEvent::ProjectSearchFinished {
                request: key,
                result,
            })
            .await;
    });
    coordinator.attach(key, task, cancellation)
}

fn begin_project_search(
    prompt: &mut ProjectSearchPrompt,
    coordinator: &mut ProjectSearchCoordinator,
    change: ProjectSearchChange,
    repository: &RepositorySession,
    services: &FileServices,
    event_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    prompt.selected = 0;
    let query = prompt.prompt.text().to_owned();
    if query.is_empty() {
        prompt.cancel_search();
        coordinator.clear_query(prompt.session)?;
        return Ok(());
    }

    let request = prompt.request(query)?;
    let next = coordinator.schedule(request, change, event_sender.clone(), cx)?;
    if let Some(next) = next {
        dispatch_project_search(coordinator, next, repository, services, event_sender, cx)?;
    }
    Ok(())
}

fn project_search_json(output: &ProjectSearchOutput) -> Value {
    serde_json::json!({
        "query": output.query.as_ref(),
        "total_hits": output.total_hits,
        "visible_results": output.matches.iter().map(|hit| {
            serde_json::json!({
                "path": hit.summary.path.as_ref(),
                "line": hit.summary.line,
                "column": hit.summary.column,
                "preview": hit.summary.preview,
            })
        }).collect::<Vec<_>>(),
    })
}

fn complete_project_search(
    prompt: &mut ProjectSearchPrompt,
    request: ProjectSearchRequestKey,
    result: std::result::Result<ProjectSearchOutput, String>,
) -> CompletionDisposition {
    let disposition = prompt.complete(request, result);
    if disposition == CompletionDisposition::Published {
        prompt.selected = 0;
    }
    disposition
}

fn finish_project_search(
    prompt: &mut ProjectSearchPrompt,
    request: ProjectSearchRequestKey,
    result: std::result::Result<ProjectSearchOutput, String>,
) -> CompletionDisposition {
    complete_project_search(prompt, request, result)
}

fn move_caret_to_project_search_hit(
    editor_window: &WindowHandle<Editor>,
    hit: &repository::ProjectSearchHit,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    editor_window.update(cx, |editor, window, cx| -> Result<()> {
        let active_buffer = editor
            .active_buffer(cx)
            .context("project-search editor has no active buffer")?;
        ensure!(
            active_buffer == hit.buffer,
            "project-search result buffer is not active"
        );
        let start = hit.anchor_range.start;
        let multi_buffer = editor.buffer().read(cx).snapshot(cx);
        let range = multi_buffer
            .buffer_anchor_range_to_anchor_range(start..start)
            .context("project-search anchor is not present in editor")?;
        editor.change_selections(
            SelectionEffects::scroll(Autoscroll::center()),
            window,
            cx,
            |selections| selections.select_anchor_ranges([range]),
        );
        Ok(())
    })?
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
        load_project_document(project_path, &path, &services, cx).await
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

fn resolve_path_from(base: &Path, input: &str) -> Result<PathBuf> {
    if input.is_empty() {
        bail!("path is empty");
    }
    let input = PathBuf::from(input);
    if input.is_absolute() {
        Ok(input)
    } else {
        Ok(base.join(input))
    }
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
    quick_open: Option<(&QuickOpenPrompt, &RepositoryIndex)>,
    project_search: Option<&ProjectSearchPrompt>,
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

    let (status, status_cursor_column) = if let Some((quick_open, index)) = quick_open {
        let (status, cursor) = quick_open.status(message, index);
        (status, Some(cursor))
    } else if let Some(project_search) = project_search {
        let (status, cursor) = project_search.status(message);
        (status, Some(cursor))
    } else if let Some(save_as) = save_as {
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
            "zec {status_label}  Ctrl-N new  Ctrl-O open  Ctrl-P quick open  Alt-F project search  Ctrl-W close  Ctrl-PgUp/PgDn tabs  Alt-PgUp/PgDn scroll  Ctrl-F find  Ctrl-H replace  Ctrl-G line  Ctrl-R reload  Ctrl-S save  Ctrl-Q quit"
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

fn run_alpha_1_probe(probe: Alpha1Probe) -> Result<()> {
    let (sender, receiver) = mpsc::sync_channel(1);
    gpui_platform::headless().run(move |cx| {
        init_zed(cx);
        let services = file_services(cx);
        cx.spawn(async move |cx| {
            let result = execute_alpha_1_probe(probe, &services, cx)
                .await
                .and_then(|value| {
                    serde_json::to_string(&value).context("serialize Alpha 1 probe result")
                });
            sender.send(result).expect("send Alpha 1 probe result");
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
    });

    let json = receiver
        .recv()
        .context("Alpha 1 probe runtime exited without a result")??;
    println!("{json}");
    Ok(())
}

async fn execute_alpha_1_probe(
    probe: Alpha1Probe,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    match probe {
        Alpha1Probe::RootIdentity { root, inputs } => {
            alpha_1_root_identity_probe(&root, &inputs, services, cx).await
        }
        Alpha1Probe::OutsideTrace(path) => alpha_1_outside_trace_probe(&path, cx).await,
        Alpha1Probe::ProjectSearch { root, query } => {
            let repository = prepare_repository(&root, services, cx).await?;
            let output = collect_project_search(&repository, query, services, cx).await?;
            Ok(project_search_json(&output))
        }
        Alpha1Probe::StaleResult(root) => alpha_1_stale_result_probe(&root, services, cx).await,
        Alpha1Probe::SearchFailure(root) => alpha_1_search_failure_probe(&root, services, cx).await,
    }
}

async fn collect_project_search(
    repository: &RepositorySession,
    query: String,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<ProjectSearchOutput> {
    let mut scheduler = ProjectSearchScheduler::default();
    let session = scheduler.open_session()?;
    let mut prompt = ProjectSearchPrompt::new(session);
    let requested = prompt.request(query)?;
    let request = scheduler
        .request(requested, ProjectSearchChange::Paste)?
        .next
        .context("project search did not start")?;
    let command = start_zed_project_search_command(request, repository, services, cx)?;
    let output = command.completion.await.map_err(anyhow::Error::msg)?;
    let finished = scheduler.finish(command.request);
    ensure!(finished.was_active, "completed search had no active slot");
    ensure!(
        complete_project_search(&mut prompt, command.request, Ok(output.clone()))
            == CompletionDisposition::Published,
        "completed project search was unexpectedly stale"
    );
    Ok(output)
}

async fn alpha_1_root_identity_probe(
    root_path: &Path,
    root_inputs: &[Alpha1RootInput],
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    let canonical_root = services
        .file_system
        .canonicalize(root_path)
        .await
        .with_context(|| format!("canonicalize probe root {}", root_path.display()))?;
    ensure!(
        !root_inputs.is_empty(),
        "root-identity probe requires at least one root input"
    );
    let mut root_identities = HashSet::new();
    let mut worktree_identities = HashSet::new();
    let mut root_input_results = Vec::with_capacity(root_inputs.len());
    let mut repositories = Vec::new();
    let mut directory_opened_as_file = false;
    for input in root_inputs {
        let arguments = input.argument.clone().into_iter().collect();
        let (paths, implicit_root) = resolve_startup_invocation(&input.cwd, arguments)?;
        ensure!(
            paths.len() == 1,
            "root input {} resolved to {} paths",
            input.id,
            paths.len()
        );
        let resolved_path = services
            .file_system
            .canonicalize(&paths[0])
            .await
            .with_context(|| format!("canonicalize root input {}", input.id))?;
        let mut startup = prepare_startup(paths, implicit_root, services, cx).await?;
        ensure!(
            startup.errors.is_empty(),
            "root spelling startup errors: {:?}",
            startup.errors
        );
        directory_opened_as_file |= startup.documents.iter().any(|document| {
            document_state(document, cx).path.as_deref() == Some(canonical_root.as_path())
        });
        let repository = startup
            .repository
            .take()
            .context("root spelling did not produce repository state")?;
        let repository_root = repository.root.canonical_path();
        ensure!(
            resolved_path == canonical_root && repository_root == canonical_root,
            "root input {} escaped canonical repository {}",
            input.id,
            canonical_root.display()
        );
        let worktree_id = repository
            ._worktree
            .read_with(cx, |worktree, _| worktree.id().to_proto())
            .to_string();
        root_identities.insert(repository.root.clone());
        worktree_identities.insert(worktree_id.clone());
        root_input_results.push(serde_json::json!({
            "id": input.id.as_str(),
            "cwd": input.cwd.display().to_string(),
            "argument": input.argument.as_ref().map(|path| path.display().to_string()),
            "resolved_path": resolved_path.display().to_string(),
            "repository_root": repository_root.display().to_string(),
            "worktree_id": worktree_id,
        }));
        repositories.push(repository);
    }
    ensure!(
        repositories.len() == root_inputs.len(),
        "not all specified root inputs reached production startup"
    );
    ensure!(
        !directory_opened_as_file,
        "a repository directory was opened as a file"
    );
    ensure!(
        worktree_identities.len() == 1,
        "root inputs produced more than one Zed worktree identity"
    );
    let repository = repositories.swap_remove(0);

    let absolute_alias = canonical_root.join("src/日本 語.rs");
    let alias_paths = [
        ("src/日本 語.rs".to_owned(), PathBuf::from("src/日本 語.rs")),
        (
            "./src/日本 語.rs".to_owned(),
            PathBuf::from("./src/日本 語.rs"),
        ),
        (
            "src/../src/日本 語.rs".to_owned(),
            PathBuf::from("src/../src/日本 語.rs"),
        ),
        (
            "aliases/日本 語.rs".to_owned(),
            PathBuf::from("aliases/日本 語.rs"),
        ),
        (absolute_alias.display().to_string(), absolute_alias.clone()),
    ];
    let (redraw_sender, _redraw_receiver) = async_channel::bounded(64);
    let mut tabs = Vec::<DocumentTab>::new();
    let mut aliases = Vec::new();
    for (label, path) in alias_paths {
        let document = open_repository_document(&path, &repository, services, cx).await?;
        let buffer_id = document
            .buffer
            .read_with(cx, |buffer, _| buffer.remote_id().to_proto())
            .to_string();
        let tab_index = if let Some(index) = tabs
            .iter()
            .position(|tab| tab.document.buffer == document.buffer)
        {
            index
        } else {
            tabs.push(create_document_tab(
                document,
                services.buffer_store.clone(),
                redraw_sender.clone(),
                cx,
            )?);
            tabs.len() - 1
        };
        let tab_handle: AnyWindowHandle = tabs[tab_index].editor_window.into();
        let tab_id = format!("{:?}", tab_handle.window_id());
        aliases.push(serde_json::json!({
            "path": label,
            "buffer_id": buffer_id,
            "tab_id": tab_id,
        }));
    }

    let outside = canonical_root
        .parent()
        .context("repository root has no parent")?
        .join("outside-control.txt");
    let exclusion_queries = [
        ".git/alpha1-excluded.txt".to_owned(),
        "ignored/excluded.txt".to_owned(),
        "target/excluded.txt".to_owned(),
        outside.display().to_string(),
    ];
    let quick_open_excluded_results = exclusion_queries
        .iter()
        .map(|query| {
            let results = repository
                .index
                .quick_open(query, QUICK_OPEN_LIMIT)
                .into_iter()
                .filter_map(|matched| repository.index.file(matched.file_index()))
                .map(|file| file.relative_path().to_owned())
                .collect::<Vec<_>>();
            serde_json::json!({"query": query, "results": results})
        })
        .collect::<Vec<_>>();

    let excluded_search = collect_project_search(
        &repository,
        "ALPHA1_EXCLUDED_SENTINEL".to_owned(),
        services,
        cx,
    )
    .await?;
    let project_search_excluded_results =
        project_search_json(&excluded_search)["visible_results"].clone();

    let worktree_root_count = services.worktree_store.read_with(cx, |store, cx| {
        store
            .worktrees()
            .filter(|worktree| worktree.read(cx).abs_path().starts_with(&canonical_root))
            .count()
    });
    ensure!(
        worktree_root_count == worktree_identities.len(),
        "root subtree contains an unreported duplicate worktree"
    );
    Ok(serde_json::json!({
        "repository_root_count": root_identities.len(),
        "worktree_root_count": worktree_identities.len(),
        "root_inputs": root_input_results,
        "directory_opened_as_file": directory_opened_as_file,
        "aliases": aliases,
        "quick_open_excluded_results": quick_open_excluded_results,
        "project_search_excluded_results": project_search_excluded_results,
    }))
}

async fn alpha_1_outside_trace_probe(path: &Path, cx: &mut gpui::AsyncApp) -> Result<Value> {
    let absolute =
        std::path::absolute(path).with_context(|| format!("make {} absolute", path.display()))?;
    let canonical = std::fs::canonicalize(&absolute)
        .with_context(|| format!("resolve controlled outside file {}", absolute.display()))?;
    let expected_bytes = std::fs::read(&canonical)
        .with_context(|| format!("read controlled outside file {}", canonical.display()))?;

    let (recording, services) = cx.update(|cx| {
        let real: Arc<dyn Fs> = Arc::new(RealFs::new(None, cx.background_executor().clone()));
        let recording = RecordingFs::new(real);
        let services = file_services_with_fs(cx, recording.clone());
        (recording, services)
    });
    recording.clear();

    let document = open_single_file_document(&canonical, &services, cx).await?;
    let (worktree, relative_path) = services
        .worktree_store
        .read_with(cx, |store, cx| store.find_worktree(&canonical, cx))
        .context("outside probe single-file worktree is missing")?;
    ensure!(
        relative_path.as_unix_str().is_empty(),
        "outside file was nested under a parent worktree"
    );
    let (worktree_root, is_single_file, scan_complete) = worktree.read_with(cx, |worktree, _| {
        (
            worktree.abs_path(),
            worktree.is_single_file(),
            worktree.as_local().map(|local| local.scan_complete()),
        )
    });
    ensure!(
        worktree_root.as_ref() == canonical,
        "outside worktree root {} differs from file {}",
        worktree_root.display(),
        canonical.display()
    );
    ensure!(is_single_file, "outside worktree is not single-file");
    scan_complete
        .context("outside worktree must be local")?
        .await;

    let opened_path = document_state(&document, cx)
        .path
        .context("outside probe buffer has no file")?;
    ensure!(
        opened_path == canonical,
        "outside probe buffer path {} differs from {}",
        opened_path.display(),
        canonical.display()
    );
    let opened_bytes = document
        .buffer
        .read_with(cx, |buffer, _| buffer.text().to_string().into_bytes());
    ensure!(
        opened_bytes == expected_bytes,
        "outside probe buffer bytes differ"
    );

    let accesses = recording.accesses();
    ensure!(!accesses.is_empty(), "outside filesystem trace is empty");
    let parent = canonical.parent();
    let read_dir_accesses = accesses
        .iter()
        .filter(|access| access.kind == FsPathKind::ReadDir)
        .collect::<Vec<_>>();
    let outside_parent_read_dir_count = read_dir_accesses
        .iter()
        .filter(|access| Some(access.path.as_path()) == parent)
        .count();
    let outside_sibling_read_dir_count = read_dir_accesses
        .iter()
        .filter(|access| Some(access.path.as_path()) != parent)
        .count();
    ensure!(
        read_dir_accesses.is_empty(),
        "single-file worktree issued read_dir at {:?}",
        read_dir_accesses
            .iter()
            .map(|access| access.path.as_path())
            .collect::<Vec<_>>()
    );
    let operations = classify_single_file_accesses(&accesses, &canonical)?;
    ensure!(
        operations.contains(&"open-self") && operations.contains(&"stat-self"),
        "outside trace must observe both open and stat: {operations:?}"
    );

    Ok(serde_json::json!({
        "opened_path": opened_path.display().to_string(),
        "outside_parent_read_dir_count": outside_parent_read_dir_count,
        "outside_sibling_read_dir_count": outside_sibling_read_dir_count,
        "operations": operations,
    }))
}

async fn alpha_1_stale_result_probe(
    root: &Path,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    let repository = prepare_repository(root, services, cx).await?;
    let mut scheduler = ProjectSearchScheduler::default();
    let session = scheduler.open_session()?;
    let mut prompt = ProjectSearchPrompt::new(session);

    let request_a = scheduler
        .request(
            prompt.request("ALPHA1_STALE_A".to_owned())?,
            ProjectSearchChange::Paste,
        )?
        .next
        .context("query A did not start")?;
    let command_a = start_zed_project_search_command(request_a, &repository, services, cx)?;
    let request_b = scheduler
        .request(
            prompt.request("ALPHA1_STALE_B".to_owned())?,
            ProjectSearchChange::Paste,
        )?
        .next
        .context("query B did not start")?;
    let command_b = start_zed_project_search_command(request_b, &repository, services, cx)?;

    let mut publish_log = Vec::new();
    let key_b = command_b.request;
    let output_b = command_b.completion.await.map_err(anyhow::Error::msg)?;
    ensure!(scheduler.finish(key_b).was_active, "query B lost its slot");
    if complete_project_search(&mut prompt, key_b, Ok(output_b)) == CompletionDisposition::Published
    {
        publish_log.push("B");
    }
    let key_a = command_a.request;
    let output_a = command_a.completion.await.map_err(anyhow::Error::msg)?;
    ensure!(scheduler.finish(key_a).was_active, "query A lost its slot");
    if complete_project_search(&mut prompt, key_a, Ok(output_a)) == CompletionDisposition::Published
    {
        publish_log.push("A");
    }

    let (final_query, final_path) = match prompt.reducer.state() {
        LatestSearchState::Ready { query, result, .. } => (
            query.clone(),
            result
                .matches
                .first()
                .context("query B produced no result")?
                .summary
                .path
                .to_string(),
        ),
        state => bail!("unexpected final stale-result state: {state:?}"),
    };
    Ok(serde_json::json!({
        "publish_log": publish_log,
        "final_query": final_query,
        "final_path": final_path,
    }))
}

fn alpha_1_document_trace(document: &OpenDocument, tab_count: usize, cx: &gpui::AsyncApp) -> Value {
    let body = document
        .buffer
        .read_with(cx, |buffer, _| buffer.text().to_string());
    let state = document_state(document, cx);
    serde_json::json!({
        "tab_count": tab_count,
        "body_sha256": format!("{:x}", Sha256::digest(body.as_bytes())),
        "dirty": state.dirty,
    })
}

async fn alpha_1_search_failure_probe(
    root: &Path,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    let repository = prepare_repository(root, services, cx).await?;
    let control_file = repository
        .index
        .file_for_alias("README.md")
        .context("search-failure repository has no README.md")?;
    let project_path = control_file
        .project_path()
        .cloned()
        .context("README.md has no project path")?;
    let document =
        load_project_document(project_path, control_file.canonical_path(), services, cx).await?;
    let (redraw_sender, _redraw_receiver) = async_channel::bounded(64);
    let tabs = vec![create_document_tab(
        document,
        services.buffer_store.clone(),
        redraw_sender,
        cx,
    )?];
    let before = alpha_1_document_trace(&tabs[0].document, tabs.len(), cx);

    let mut scheduler = ProjectSearchScheduler::default();
    let session = scheduler.open_session()?;
    let mut prompt = ProjectSearchPrompt::new(session);
    let (provider_sender, provider_receiver) =
        async_channel::bounded::<std::result::Result<ProjectSearchOutput, String>>(1);
    let request = scheduler
        .request(
            prompt.request("ALPHA1_SEARCH_FAILURE".to_owned())?,
            ProjectSearchChange::Paste,
        )?
        .next
        .context("controlled failing search did not start")?;
    let command = start_project_search_command_with(request, cx, move |_query, cx| {
        Ok(cx.spawn(async move |_cx| {
            provider_receiver
                .recv()
                .await
                .unwrap_or_else(|_| Err("controlled provider disconnected".to_owned()))
        }))
    })?;
    provider_sender
        .send(Err("EIO".to_owned()))
        .await
        .context("send controlled EIO")?;
    let key = command.request;
    let completion = command.completion.await;
    ensure!(
        scheduler.finish(key).was_active,
        "failed search lost its slot"
    );
    ensure!(
        complete_project_search(&mut prompt, key, completion) == CompletionDisposition::Published,
        "controlled EIO completion was unexpectedly stale"
    );
    let error = match prompt.reducer.state() {
        LatestSearchState::Failed { error, .. } => error.clone(),
        state => bail!("controlled EIO did not reach failed state: {state:?}"),
    };
    let rendered_status = tabs[0].editor_window.update(cx, |editor, _window, cx| {
        capture_editor(
            editor,
            cx,
            Viewport::default(),
            false,
            None,
            Rect::new(0, 0, 120, 40),
            "repo",
            None,
            None,
            Some(&prompt),
            None,
            None,
            None,
            None,
        )
        .snapshot
        .status
    })?;
    ensure!(
        rendered_status.contains("search failed: EIO"),
        "controlled EIO was not rendered in project-search status"
    );
    let after = alpha_1_document_trace(&tabs[0].document, tabs.len(), cx);
    ensure!(before == after, "search failure mutated editor state");

    tabs[0].editor_window.update(cx, |editor, window, cx| {
        editor.select_all(&SelectAll, window, cx);
        editor.insert("ALPHA1_SEARCH_FAILURE_CONTINUED", window, cx);
    })?;
    save_document(&tabs[0].document, services, cx).await?;
    let control_disk_token = services
        .file_system
        .load(control_file.canonical_path())
        .await?;
    let continued_edit_saved = control_disk_token == "ALPHA1_SEARCH_FAILURE_CONTINUED"
        && !document_state(&tabs[0].document, cx).dirty;

    Ok(serde_json::json!({
        "error": error,
        "before": before,
        "after": after,
        "continued_edit_saved": continued_edit_saved,
        "control_disk_token": control_disk_token,
    }))
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
    struct TemporaryRepository {
        directory: PathBuf,
        root: PathBuf,
        root_alias: PathBuf,
        outside: PathBuf,
    }

    impl TemporaryRepository {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after the Unix epoch")
                .as_nanos();
            let directory =
                std::env::temp_dir().join(format!("zec-repository-{}-{nonce}", std::process::id()));
            let root = directory.join("repo");
            let root_alias = directory.join("repo-alias");
            let outside = directory.join("outside.txt");
            std::fs::create_dir_all(root.join("src")).expect("create repository src");
            std::fs::create_dir_all(root.join("aliases")).expect("create repository aliases");
            std::fs::create_dir_all(root.join("target")).expect("create ignored target");
            std::fs::create_dir_all(root.join(".git")).expect("create excluded git metadata");
            std::fs::write(root.join("README.md"), "ALPHA1_READY_SENTINEL\n")
                .expect("write repository README");
            std::fs::write(
                root.join("src/日本 語.rs"),
                "pub const TOKEN: &str = \"inside\";\n",
            )
            .expect("write canonical repository file");
            std::fs::write(
                root.join("target/excluded.rs"),
                "ALPHA1_EXCLUDED_SENTINEL\n",
            )
            .expect("write ignored repository file");
            std::fs::write(root.join(".git/hidden"), "ALPHA1_EXCLUDED_SENTINEL\n")
                .expect("write git metadata fixture");
            std::fs::write(root.join(".gitignore"), "target/\n")
                .expect("write repository ignore rules");
            std::fs::write(&outside, "outside control\n").expect("write outside file");
            std::os::unix::fs::symlink("../src/日本 語.rs", root.join("aliases/日本 語.rs"))
                .expect("create file alias");
            std::os::unix::fs::symlink("root-loop", root.join("root-loop"))
                .expect("create self-referential symlink");
            std::os::unix::fs::symlink(&root, &root_alias).expect("create root alias");
            Self {
                directory,
                root,
                root_alias,
                outside,
            }
        }
    }

    impl Drop for TemporaryRepository {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn command(arguments: &[&str]) -> Result<Command> {
        parse_command(arguments.iter().map(|argument| OsString::from(*argument)))
    }

    #[test]
    fn project_search_scheduler_coalesces_backspace_before_atomic_paste() {
        let mut scheduler = ProjectSearchScheduler::default();
        let session = scheduler.open_session().expect("open search session");
        let mut prompt = ProjectSearchPrompt::new(session);

        let first = scheduler
            .request(
                prompt.request("ALPHA1_STALE_A".to_owned()).unwrap(),
                ProjectSearchChange::Paste,
            )
            .unwrap()
            .next
            .expect("initial paste starts immediately");
        let prefix = prompt.request("ALPHA1_STALE_".to_owned()).unwrap();
        let prefix_key = prefix.key;
        let delayed = scheduler.request(prefix, ProjectSearchChange::Key).unwrap();
        assert!(delayed.next.is_none());
        assert_eq!(delayed.debounce, Some(prefix_key));
        assert_eq!(scheduler.active_count(), 1);

        let expired = scheduler.debounce_elapsed(prefix_key);
        assert!(expired.accepted);
        assert!(
            expired.next.is_none(),
            "debounced key query consumed the replacement slot"
        );
        assert_eq!(scheduler.pending_query(), Some("ALPHA1_STALE_"));

        let latest = scheduler
            .request(
                prompt.request("ALPHA1_STALE_B".to_owned()).unwrap(),
                ProjectSearchChange::Paste,
            )
            .unwrap()
            .next
            .expect("atomic replacement paste uses the reserved second slot");
        assert_eq!(
            [first.query.as_str(), latest.query.as_str()],
            ["ALPHA1_STALE_A", "ALPHA1_STALE_B"]
        );
        assert_eq!(scheduler.active_count(), 2);

        assert!(scheduler.finish(latest.key).was_active);
        assert_eq!(
            prompt.complete(latest.key, Err("latest".to_owned())),
            CompletionDisposition::Published
        );
        assert!(scheduler.finish(first.key).was_active);
        assert_eq!(
            prompt.complete(first.key, Err("stale".to_owned())),
            CompletionDisposition::DiscardedStale
        );
    }

    #[test]
    fn project_search_scheduler_keeps_only_latest_rapid_key_request() {
        let mut scheduler = ProjectSearchScheduler::default();
        let session = scheduler.open_session().expect("open search session");
        let mut prompt = ProjectSearchPrompt::new(session);
        let first = scheduler
            .request(
                prompt.request("first".to_owned()).unwrap(),
                ProjectSearchChange::Paste,
            )
            .unwrap()
            .next
            .expect("initial search");

        let mut debounce_keys = Vec::new();
        for index in 0..1_000 {
            let request = prompt.request(format!("query-{index:04}")).unwrap();
            debounce_keys.push(request.key);
            let schedule = scheduler
                .request(request, ProjectSearchChange::Key)
                .unwrap();
            assert!(schedule.next.is_none());
            assert_eq!(scheduler.active_count(), 1);
            assert!(scheduler.active_count() <= MAX_CONCURRENT_PROJECT_SEARCHES);
        }
        let latest_key = *debounce_keys.last().expect("latest debounce key");
        for obsolete in &debounce_keys[..debounce_keys.len() - 1] {
            assert!(!scheduler.debounce_elapsed(*obsolete).accepted);
        }
        let expired = scheduler.debounce_elapsed(latest_key);
        assert!(expired.accepted);
        assert!(
            expired.next.is_none(),
            "key edits preserve the spare slot for an atomic paste"
        );
        assert_eq!(scheduler.pending_query(), Some("query-0999"));

        let latest = scheduler
            .finish(first.key)
            .next
            .expect("latest key query starts after the active search acknowledges completion");
        assert_eq!(latest.query, "query-0999");
        assert_eq!(scheduler.active_count(), 1);
        assert!(scheduler.pending_query().is_none());
        assert!(scheduler.finish(latest.key).was_active);
    }

    #[test]
    fn project_search_scheduler_bounds_two_active_and_one_latest_pending() {
        let mut scheduler = ProjectSearchScheduler::default();
        let session = scheduler.open_session().expect("open search session");
        let mut prompt = ProjectSearchPrompt::new(session);
        let first = scheduler
            .request(
                prompt.request("first".to_owned()).unwrap(),
                ProjectSearchChange::Paste,
            )
            .unwrap()
            .next
            .expect("first search");
        let second = scheduler
            .request(
                prompt.request("second".to_owned()).unwrap(),
                ProjectSearchChange::Paste,
            )
            .unwrap()
            .next
            .expect("second search");

        for index in 0..1_000 {
            let schedule = scheduler
                .request(
                    prompt.request(format!("queued-{index:04}")).unwrap(),
                    ProjectSearchChange::Paste,
                )
                .unwrap();
            assert!(schedule.next.is_none());
            assert_eq!(scheduler.active_count(), MAX_CONCURRENT_PROJECT_SEARCHES);
        }
        assert_eq!(scheduler.pending_query(), Some("queued-0999"));

        let completion = scheduler.finish(first.key);
        let latest = completion.next.expect("freed slot starts latest only");
        assert_eq!(latest.query, "queued-0999");
        assert_eq!(scheduler.active_count(), MAX_CONCURRENT_PROJECT_SEARCHES);
        assert!(scheduler.finish(second.key).was_active);
        assert!(scheduler.finish(latest.key).was_active);
    }

    #[test]
    fn project_search_session_key_blocks_reopen_stale_completion_and_keeps_task() {
        let mut coordinator = ProjectSearchCoordinator::default();
        let first_session = coordinator.open_session().expect("first session");
        let mut first_prompt = ProjectSearchPrompt::new(first_session);
        let first = coordinator
            .scheduler
            .request(
                first_prompt.request("old".to_owned()).unwrap(),
                ProjectSearchChange::Paste,
            )
            .unwrap()
            .next
            .expect("old search");
        coordinator
            .attach(first.key, Task::ready(()), None)
            .expect("attach old task");
        coordinator.close_session(first_session);
        assert_eq!(coordinator.task_count(), 1);
        assert_eq!(coordinator.scheduler.active_count(), 1);

        let second_session = coordinator.open_session().expect("second session");
        let mut second_prompt = ProjectSearchPrompt::new(second_session);
        let second = coordinator
            .scheduler
            .request(
                second_prompt.request("new".to_owned()).unwrap(),
                ProjectSearchChange::Paste,
            )
            .unwrap()
            .next
            .expect("new search");
        coordinator
            .attach(second.key, Task::ready(()), None)
            .expect("attach new task");
        assert_eq!(first.key.generation, second.key.generation);
        assert_ne!(first.key.session, second.key.session);
        assert_eq!(coordinator.task_count(), 2);

        assert!(coordinator.finish(first.key).was_active);
        assert_eq!(
            second_prompt.complete(first.key, Err("old".to_owned())),
            CompletionDisposition::DiscardedStale
        );
        assert_eq!(coordinator.task_count(), 1);
        assert!(coordinator.finish(second.key).was_active);
        assert_eq!(
            second_prompt.complete(second.key, Err("new".to_owned())),
            CompletionDisposition::Published
        );
        assert_eq!(coordinator.task_count(), 0);
    }

    #[test]
    fn project_search_coordinator_retains_live_task_through_supersede_close_and_drop() {
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        gpui_platform::headless().run(move |cx| {
            cx.spawn(async move |cx| {
                let result: Result<()> = async {
                    let mut coordinator = ProjectSearchCoordinator::default();
                    let session = coordinator.open_session().context("search session")?;
                    let mut prompt = ProjectSearchPrompt::new(session);
                    let first = coordinator
                        .scheduler
                        .request(
                            prompt.request("first".to_owned())?,
                            ProjectSearchChange::Paste,
                        )?
                        .next
                        .context("first search")?;
                    let (release_sender, release_receiver) = async_channel::bounded(1);
                    let (finished_sender, finished_receiver) = async_channel::bounded(1);
                    let live_task = cx.spawn(async move |_cx| {
                        let _ = release_receiver.recv().await;
                        let _ = finished_sender.send(()).await;
                    });
                    let cancellation = RunningLiteralSearchCancellation::test_probe();
                    let cancellation_probe = cancellation.clone();
                    coordinator.attach(first.key, live_task, Some(cancellation))?;

                    let replacement = prompt.request("second".to_owned())?;
                    let (event_sender, _event_receiver) = async_channel::unbounded();
                    let second = coordinator
                        .schedule(replacement, ProjectSearchChange::Paste, event_sender, cx)?
                        .context("paste replacement uses spare slot")?;
                    coordinator.attach(second.key, Task::ready(()), None)?;
                    ensure!(
                        cancellation_probe.is_cancelled(),
                        "supersede did not cancel"
                    );
                    ensure!(coordinator.task_count() == 2, "supersede lost a task");
                    ensure!(!release_sender.is_closed(), "supersede dropped live task");

                    coordinator.clear_query(session)?;
                    ensure!(coordinator.task_count() == 2, "query clear lost a task");
                    ensure!(!release_sender.is_closed(), "query clear dropped live task");

                    let mut prompt = Some(prompt);
                    close_project_search_prompt(&mut prompt, &mut coordinator);
                    ensure!(prompt.is_none(), "prompt remained open");
                    ensure!(coordinator.task_count() == 2, "prompt close lost a task");
                    ensure!(
                        !release_sender.is_closed(),
                        "prompt close dropped live task"
                    );

                    drop(coordinator);
                    ensure!(
                        !release_sender.is_closed(),
                        "coordinator teardown dropped instead of detached live task"
                    );
                    release_sender
                        .send(())
                        .await
                        .context("release detached task")?;
                    finished_receiver
                        .recv()
                        .await
                        .context("detached task did not finish")?;
                    Ok(())
                }
                .await;
                result_sender.send(result).expect("send lifecycle result");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });
        result_receiver
            .recv()
            .expect("receive lifecycle result")
            .expect("project search lifecycle");
    }

    #[test]
    fn project_search_finish_without_prompt_releases_task_and_slot() {
        let mut coordinator = ProjectSearchCoordinator::default();
        let session = coordinator.open_session().expect("search session");
        let mut prompt = ProjectSearchPrompt::new(session);
        let request = coordinator
            .scheduler
            .request(
                prompt.request("search".to_owned()).unwrap(),
                ProjectSearchChange::Paste,
            )
            .unwrap()
            .next
            .expect("search starts");
        coordinator
            .attach(request.key, Task::ready(()), None)
            .expect("attach completed task");
        coordinator.close_session(session);
        drop(prompt);

        let completion = coordinator.finish(request.key);
        assert!(completion.was_active);
        assert!(completion.next.is_none());
        assert_eq!(coordinator.task_count(), 0);
        assert_eq!(coordinator.scheduler.active_count(), 0);
    }

    #[test]
    fn project_search_synchronous_start_error_releases_reserved_slot() {
        let mut scheduler = ProjectSearchScheduler::default();
        let session = scheduler.open_session().expect("search session");
        let mut prompt = ProjectSearchPrompt::new(session);
        let request = scheduler
            .request(
                prompt.request("search".to_owned()).unwrap(),
                ProjectSearchChange::Paste,
            )
            .unwrap()
            .next
            .expect("reserved search slot");
        let command: Result<ProjectSearchCommand> = Err(anyhow::anyhow!("synchronous start EIO"));
        let (completion, cancellation) = project_search_command_parts(request.key, command);
        assert!(cancellation.is_none());
        assert_eq!(
            futures::executor::block_on(completion).expect_err("start error became success"),
            "synchronous start EIO"
        );

        assert!(scheduler.finish(request.key).was_active);
        assert_eq!(scheduler.active_count(), 0);
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
    fn repository_startup_uses_one_root_identity_and_exact_outside_worktree() {
        let fixture = TemporaryRepository::new();
        let root = fixture.root.clone();
        let root_alias = fixture.root_alias.clone();
        let outside = fixture.outside.clone();
        let expected_root = std::fs::canonicalize(&root).expect("canonicalize fixture root");
        let (sender, receiver) = mpsc::sync_channel(1);

        gpui_platform::headless().run(move |cx| {
            init_zed(cx);
            let services = file_services(cx);

            cx.spawn(async move |cx| {
                let result: Result<_> = async {
                    let startup =
                        prepare_startup(vec![root_alias.clone()], false, &services, cx).await?;
                    ensure!(
                        startup.errors.is_empty(),
                        "startup errors: {:?}",
                        startup.errors
                    );
                    ensure!(
                        startup.documents.len() == 1,
                        "expected one startup document"
                    );
                    let repository = startup
                        .repository
                        .as_ref()
                        .context("directory startup must retain repository state")?;
                    let initial_text = startup.documents[0]
                        .buffer
                        .read_with(cx, |buffer, _| buffer.text().to_string());
                    let indexed_paths = repository
                        .index
                        .files()
                        .iter()
                        .map(|file| file.relative_path().to_owned())
                        .collect::<Vec<_>>();

                    let mut quick_open = QuickOpenPrompt::new(&repository.index);
                    ensure!(quick_open.prompt.handle_paste("日本 語.rs") == PromptAction::Changed);
                    quick_open.refresh(&repository.index);
                    let selected_path = quick_open
                        .selected_file_index()
                        .and_then(|file_index| repository.index.file(file_index))
                        .map(|file| file.relative_path().to_owned());
                    let quick_status = quick_open.status(None, &repository.index).0;

                    let canonical_document = open_repository_document(
                        Path::new("src/日本 語.rs"),
                        repository,
                        &services,
                        cx,
                    )
                    .await?;
                    let alias_document = open_repository_document(
                        Path::new("aliases/日本 語.rs"),
                        repository,
                        &services,
                        cx,
                    )
                    .await?;
                    let aliases_share_buffer = canonical_document.buffer == alias_document.buffer;

                    let outside_document =
                        open_repository_document(&outside, repository, &services, cx).await?;
                    let outside_path = outside.clone();
                    let canonical_root = repository.root.canonical_path().to_path_buf();
                    let (visible_count, root_count, outside_single_file, outside_parent_worktree) =
                        services.worktree_store.read_with(cx, |store, cx| {
                            let worktrees = store.worktrees().collect::<Vec<_>>();
                            (
                                store.visible_worktrees(cx).count(),
                                worktrees
                                    .iter()
                                    .filter(|worktree| {
                                        worktree.read(cx).abs_path().as_ref()
                                            == canonical_root.as_path()
                                    })
                                    .count(),
                                worktrees.iter().any(|worktree| {
                                    let worktree = worktree.read(cx);
                                    worktree.abs_path().as_ref() == outside_path.as_path()
                                        && worktree.is_single_file()
                                }),
                                worktrees.iter().any(|worktree| {
                                    worktree.read(cx).abs_path().as_ref()
                                        == outside_path
                                            .parent()
                                            .expect("outside fixture has parent")
                                }),
                            )
                        });
                    let outside_text = outside_document
                        .buffer
                        .read_with(cx, |buffer, _| buffer.text().to_string());

                    let partial = prepare_startup(
                        vec![root_alias, root.join("README.md"), root.join("root-loop")],
                        false,
                        &services,
                        cx,
                    )
                    .await?;
                    ensure!(partial.documents.len() == 2);
                    let partial_alias_deduped =
                        partial.documents[0].buffer == partial.documents[1].buffer;
                    let partial_error = partial.errors.join("  |  ");
                    let control = partial
                        .documents
                        .first()
                        .context("partial startup kept no control document")?;
                    let control_window = cx.update(|cx| open_editor(control.buffer.clone(), cx))?;
                    control_window.update(cx, |editor, window, cx| {
                        editor.insert("ALPHA1_PARTIAL_STARTUP_EDIT ", window, cx);
                    })?;
                    save_document(control, &services, cx).await?;
                    let continued_saved = std::fs::read_to_string(root.join("README.md"))?
                        .starts_with("ALPHA1_PARTIAL_STARTUP_EDIT ");

                    Ok((
                        repository.root.label(),
                        repository.root.canonical_path().to_path_buf(),
                        initial_text,
                        indexed_paths,
                        selected_path,
                        quick_status,
                        aliases_share_buffer,
                        visible_count,
                        root_count,
                        outside_single_file,
                        outside_parent_worktree,
                        outside_text,
                        partial_alias_deduped,
                        partial_error,
                        continued_saved,
                    ))
                }
                .await;

                sender.send(result).expect("send repository startup result");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });

        let (
            root_label,
            canonical_root,
            initial_text,
            indexed_paths,
            selected_path,
            quick_status,
            aliases_share_buffer,
            visible_count,
            root_count,
            outside_single_file,
            outside_parent_worktree,
            outside_text,
            partial_alias_deduped,
            partial_error,
            continued_saved,
        ) = receiver
            .recv()
            .expect("receive repository startup result")
            .expect("repository startup scenario should succeed");

        assert_eq!(root_label, "repo");
        assert_eq!(canonical_root, expected_root);
        assert_eq!(initial_text, "ALPHA1_READY_SENTINEL\n");
        assert!(indexed_paths.iter().any(|path| path == "README.md"));
        assert!(!indexed_paths.iter().any(|path| path.starts_with(".git/")));
        assert!(!indexed_paths.iter().any(|path| path.starts_with("target/")));
        assert_eq!(selected_path.as_deref(), Some("src/日本 語.rs"));
        assert!(quick_status.contains("src/日本 語.rs"));
        assert!(aliases_share_buffer);
        assert_eq!(visible_count, 1);
        assert_eq!(root_count, 1);
        assert!(outside_single_file);
        assert!(!outside_parent_worktree);
        assert_eq!(outside_text, "outside control\n");
        assert!(partial_alias_deduped);
        assert!(partial_error.contains("ELOOP"), "{partial_error}");
        assert!(continued_saved);
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
