mod actions;
mod clipboard;
mod input;
mod prompt;
mod render;
mod repository;
mod tabs;
mod terminal;
mod tracing_fs;

// Key bindings resolve to presentation-neutral terminal actions first. User
// keymaps may keep using the familiar Zed action IDs; the loader aliases those
// IDs to these actions so an Editor handler cannot perform the operation once
// in GPUI and then a second time in the terminal workspace reducer.
mod terminal_gpui_actions {
    gpui::actions!(
        zec,
        [
            CommandPalette,
            NewFile,
            OpenFile,
            QuickOpen,
            ProjectSearch,
            CloseTab,
            PreviousTab,
            NextTab,
            Find,
            Replace,
            GoToLine,
            Reload,
            Save,
            Quit,
            ShowCompletions,
            Hover,
            ProjectDiagnostics,
            GoToDefinition,
            GoToTypeDefinition,
            FindReferences,
            ProjectSymbols,
            NavigateBack,
            NavigateForward,
            RenameSymbol,
            CodeActions,
            FormatDocument,
            FormatSelection,
            Undo,
            Redo,
            Copy,
            Cut
        ]
    );
}

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    env,
    ffi::OsString,
    io::{self, IsTerminal as _},
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
        mpsc,
    },
    time::{Duration, Instant},
};

use actions::{ActionContext, ActionMatch};
use anyhow::{Context as _, Result, bail, ensure};
use client::{Client, UserStore};
use editor::{
    Anchor, Bias, CompletionProvider, Editor, EditorStyle, MultiBufferOffset, SelectionEffects,
    actions::{ConfirmCompletion, Cut, Redo, SelectAll, ShowCompletions, Undo},
    display_map::{DisplayPoint, DisplayRow, DisplaySnapshot},
    scroll::Autoscroll,
};
use futures::StreamExt as _;
use gpui::{
    AnyWindowHandle, App, AppContext as _, Entity, Focusable as _, KeyBinding, Keystroke, Task,
    UpdateGlobal as _, WindowBounds, WindowHandle, WindowOptions,
};
use language::{
    Buffer, BufferEvent, Capability, LanguageAwareStyling, LanguageNotFound, LanguageRegistry,
    ToPointUtf16 as _,
    language_settings::{FormatOnSave, LanguageSettings, SoftWrap},
};
use multi_buffer::MultiBuffer;
use project::{
    CodeAction, LocalProjectFlags, LspAction, PrepareRenameResponse, Project, ProjectPath,
    ProjectTransaction,
    buffer_store::BufferStore,
    lsp_store::{FormatTrigger, LspFormatTarget},
    search::SearchQuery,
    trusted_worktrees::{PathTrust, TrustedWorktrees, TrustedWorktreesEvent},
    worktree_store::WorktreeStore,
};
use prompt::{LinePrompt, PromptAction};
use ratatui::{
    layout::{Position as TerminalPosition, Rect},
    style::{Color as TerminalColor, Modifier as TerminalModifier, Style as TerminalStyle},
};
use render::{
    BackgroundRange, Cursor, EditorWidget, OverlayRow, OverlaySnapshot, RenderSnapshot,
    SelectionRange, StyleSpan, TextPosition, Viewport,
};
use repository::{
    CompletionDisposition, LatestSearch, LatestSearchState, ProjectSearchOutput, QuickOpenMatch,
    RepositoryIndex, RepositoryRoot, RunningLiteralSearchCancellation, SearchGeneration,
    start_literal_project_search,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tabs::{Direction as TabDirection, TabLabel};
use terminal::{
    DiagnosticPresentation, HoverPresentation, InputReader, LocationPresentation,
    LocationRequestKind, RenamePreparation, ScrollDirection, TerminalEvent, TerminalSession,
};
use text::{ToOffset as _, ToPoint as _};
use theme::ActiveTheme as _;
use tracing_fs::{FsPathKind, ZecFs, classify_single_file_accesses};
use unicode_width::UnicodeWidthStr as _;
use workspace::searchable::{Direction, SearchToken, SearchableItem as _};
use zed_fs::{Fs, RealFs};

const USAGE: &str = "Usage: zec [DIRECTORY | FILE ...]\n       zec --smoke\n\nKeys: F1/Ctrl-Shift-P commands, Ctrl-Space/Alt-/ completion, F2 hover, F6 rename, F8 diagnostics, F12 definition, Alt-F12 type definition, Shift-F12 references, Ctrl-. code actions, Shift-Alt-F format, Ctrl-Alt-F format selection, Ctrl-T symbols, Alt-Left/Right history, Ctrl-N new, Ctrl-O open, Ctrl-P quick open, Alt-F project search, Ctrl-W close tab, Ctrl-PgUp/PgDn tabs, Alt-PgUp/PgDn scroll, Ctrl-C copy, Ctrl-X cut, Ctrl-F find, Ctrl-H replace, Ctrl-G line, Ctrl-R reload, Ctrl-S save, Ctrl-Q quit, Ctrl-Z/Y undo/redo";
const QUICK_OPEN_LIMIT: usize = 100;
const PROJECT_SYMBOL_LIMIT: usize = 100;
const MAX_CONCURRENT_PROJECT_SEARCHES: usize = 2;
const PROJECT_SEARCH_DEBOUNCE: Duration = Duration::from_millis(16);
const RENAME_PREVIEW_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RENAME_PREVIEW_OPERATIONS: usize = 10_000;
const MAX_RENAME_PREVIEW_BYTES: usize = 2 * 1024 * 1024;
const MAX_LANGUAGE_RESPONSE_ITEMS: usize = 10_000;
const MAX_LANGUAGE_TEXT_BYTES: usize = 64 * 1024;
const MAX_OVERLAY_SNAPSHOT_ROWS: usize = 200;
const TERMINAL_DEFAULT_KEYMAP: &str = r#"
[
  {
    "context": "Editor",
    "bindings": {
      "f1": "command_palette::Toggle",
      "ctrl-shift-p": "command_palette::Toggle",
      "ctrl-n": "workspace::NewFile",
      "ctrl-o": "workspace::Open",
      "ctrl-p": "file_finder::Toggle",
      "alt-f": "project_search::ToggleFocus",
      "ctrl-w": "pane::CloseActiveItem",
      "ctrl-pageup": "pane::ActivatePreviousItem",
      "ctrl-pagedown": "pane::ActivateNextItem",
      "ctrl-f": "buffer_search::Deploy",
      "ctrl-h": "buffer_search::DeployReplace",
      "ctrl-g": "go_to_line::Toggle",
      "ctrl-r": "workspace::ReloadActiveItem",
      "ctrl-s": "workspace::Save",
      "ctrl-q": "zed::Quit",
      "ctrl-space": "editor::ShowCompletions",
      "alt-/": "editor::ShowCompletions",
      "f2": "editor::Hover",
      "f8": "diagnostics::Deploy",
      "f12": "editor::GoToDefinition",
      "alt-f12": "editor::GoToTypeDefinition",
      "shift-f12": "editor::FindAllReferences",
      "ctrl-t": "project_symbols::Toggle",
      "alt-left": "pane::GoBack",
      "alt-right": "pane::GoForward",
      "f6": "editor::Rename",
      "ctrl-.": "editor::ToggleCodeActions",
      "shift-alt-f": "editor::Format",
      "ctrl-alt-f": "editor::FormatSelections",
      "ctrl-z": "editor::Undo",
      "ctrl-y": "editor::Redo",
      "ctrl-c": "editor::Copy",
      "ctrl-x": "editor::Cut"
    }
  }
]
"#;

#[derive(Debug, Eq, PartialEq)]
enum Command {
    Edit(Vec<PathBuf>),
    Alpha1Probe(Alpha1Probe),
    Alpha2Probe(Alpha2Probe),
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

#[derive(Debug, Eq, PartialEq)]
enum Alpha2Probe {
    LanguageService {
        root: PathBuf,
        file: PathBuf,
    },
    SettingsReload {
        root: PathBuf,
        file: PathBuf,
    },
    LspFailure {
        root: PathBuf,
        file: PathBuf,
        scenario: String,
    },
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

fn bounded_terminal_text(text: &str) -> String {
    if text.len() <= MAX_LANGUAGE_TEXT_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_LANGUAGE_TEXT_BYTES.saturating_sub('…'.len_utf8());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = text[..end].to_owned();
    bounded.push('…');
    bounded
}

fn language_service_unavailable_message(action: &str) -> String {
    format!("{action} unavailable: no ready language server")
}

fn overlay_window(len: usize, selected: usize, budget: usize) -> Range<usize> {
    let budget = budget.max(1).min(len);
    let selected = selected.min(len.saturating_sub(1));
    let start = selected
        .saturating_sub(budget / 2)
        .min(len.saturating_sub(budget));
    start..start.saturating_add(budget)
}

fn bounded_overlay_rows(
    rows: Vec<OverlayRow>,
    selected: Option<usize>,
) -> (Vec<OverlayRow>, Option<usize>) {
    if rows.len() <= MAX_OVERLAY_SNAPSHOT_ROWS {
        let len = rows.len();
        return (rows, selected.filter(|selected| *selected < len));
    }
    let selected = selected
        .filter(|selected| *selected < rows.len())
        .unwrap_or_default();
    let window = overlay_window(rows.len(), selected, MAX_OVERLAY_SNAPSHOT_ROWS);
    let local_selected = selected.saturating_sub(window.start);
    (
        rows.into_iter()
            .skip(window.start)
            .take(window.len())
            .collect(),
        Some(local_selected),
    )
}

#[derive(Debug)]
struct CommandPalettePrompt {
    prompt: LinePrompt,
    context: ActionContext,
    matches: Vec<ActionMatch>,
    selected: usize,
    feedback: Option<String>,
}

impl CommandPalettePrompt {
    fn new(context: ActionContext) -> Self {
        let mut this = Self {
            prompt: LinePrompt::new(),
            context,
            matches: Vec::new(),
            selected: 0,
            feedback: None,
        };
        this.refresh();
        this
    }

    fn refresh(&mut self) {
        self.matches = actions::search(self.context, self.prompt.text());
        self.selected = 0;
        self.feedback = None;
    }

    fn step(&mut self, direction: TabDirection) {
        let Some(next) = tabs::adjacent_index(self.selected, self.matches.len(), direction) else {
            return;
        };
        self.selected = next;
        self.feedback = None;
    }

    fn selected_action(&self) -> Option<actions::ActionDescriptor> {
        self.matches.get(self.selected).map(|item| item.descriptor)
    }

    fn status(&self, message: Option<&str>) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prefix = format!("{message_prefix}Command palette: ");
        let cursor_column = prefix.width().saturating_add(
            self.prompt
                .text()
                .get(..self.prompt.cursor())
                .unwrap_or_default()
                .width(),
        );
        let mut status = format!(
            "{prefix}{}  {}/{}  Enter run  ↑/↓ select  Esc cancel",
            self.prompt.text(),
            usize::from(!self.matches.is_empty()).saturating_mul(self.selected.saturating_add(1)),
            self.matches.len(),
        );
        if let Some(feedback) = &self.feedback {
            status.push_str("  |  ");
            status.push_str(feedback);
        }
        (status, cursor_column)
    }

    fn overlay(&self) -> OverlaySnapshot {
        OverlaySnapshot {
            title: " Commands ".to_owned(),
            rows: self
                .matches
                .iter()
                .map(|item| {
                    let descriptor = item.descriptor;
                    let disabled = (!descriptor.enabled)
                        .then_some("  [disabled]")
                        .unwrap_or("");
                    OverlayRow {
                        text: format!(
                            "{}  {}  {}{}",
                            descriptor.name, descriptor.key_binding, descriptor.id, disabled
                        ),
                        enabled: descriptor.enabled,
                    }
                })
                .collect(),
            selected: (!self.matches.is_empty()).then_some(self.selected),
        }
    }
}

#[derive(Debug)]
enum CompletionPromptState {
    Running,
    Ready {
        items: Vec<terminal::CompletionPresentation>,
        visible: Vec<usize>,
    },
    Failed(String),
}

#[derive(Debug)]
struct CompletionPrompt {
    buffer_id: u64,
    generation: u64,
    prompt: LinePrompt,
    selected: usize,
    state: CompletionPromptState,
}

impl CompletionPrompt {
    fn running(buffer_id: u64, generation: u64) -> Self {
        Self {
            buffer_id,
            generation,
            prompt: LinePrompt::new(),
            selected: 0,
            state: CompletionPromptState::Running,
        }
    }

    fn complete(
        &mut self,
        buffer_id: u64,
        generation: u64,
        result: std::result::Result<Vec<terminal::CompletionPresentation>, String>,
    ) -> bool {
        if self.buffer_id != buffer_id || self.generation != generation {
            return false;
        }
        self.selected = 0;
        self.state = match result {
            Ok(mut items) => {
                items.truncate(MAX_LANGUAGE_RESPONSE_ITEMS);
                for item in &mut items {
                    item.label = bounded_terminal_text(&item.label);
                    item.detail = item.detail.as_deref().map(bounded_terminal_text);
                    item.kind = item.kind.as_deref().map(bounded_terminal_text);
                    item.documentation = item.documentation.as_deref().map(bounded_terminal_text);
                }
                CompletionPromptState::Ready {
                    visible: (0..items.len()).collect(),
                    items,
                }
            }
            Err(error) => CompletionPromptState::Failed(error),
        };
        self.refresh();
        true
    }

    fn refresh(&mut self) {
        let CompletionPromptState::Ready { items, visible } = &mut self.state else {
            return;
        };
        let query = self.prompt.text().to_lowercase();
        *visible = items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                query.is_empty()
                    || item.label.to_lowercase().contains(&query)
                    || item
                        .detail
                        .as_deref()
                        .is_some_and(|detail| detail.to_lowercase().contains(&query))
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
    }

    fn step(&mut self, direction: TabDirection) {
        let len = match &self.state {
            CompletionPromptState::Ready { visible, .. } => visible.len(),
            CompletionPromptState::Running | CompletionPromptState::Failed(_) => 0,
        };
        let Some(next) = tabs::adjacent_index(self.selected, len, direction) else {
            return;
        };
        self.selected = next;
    }

    fn selected_item_index(&self) -> Option<usize> {
        match &self.state {
            CompletionPromptState::Ready { visible, .. } => visible.get(self.selected).copied(),
            CompletionPromptState::Running | CompletionPromptState::Failed(_) => None,
        }
    }

    fn status(&self, message: Option<&str>) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prefix = format!("{message_prefix}Completion filter: ");
        let cursor = prefix.width().saturating_add(
            self.prompt
                .text()
                .get(..self.prompt.cursor())
                .unwrap_or_default()
                .width(),
        );
        let detail = match &self.state {
            CompletionPromptState::Running => "requesting…".to_owned(),
            CompletionPromptState::Ready { visible, .. } => format!(
                "{}/{}",
                usize::from(!visible.is_empty()).saturating_mul(self.selected.saturating_add(1)),
                visible.len()
            ),
            CompletionPromptState::Failed(error) => format!("failed: {error}"),
        };
        (
            format!(
                "{prefix}{}  {detail}  Enter apply  ↑/↓ select  Esc cancel",
                self.prompt.text()
            ),
            cursor,
        )
    }

    fn overlay(&self) -> OverlaySnapshot {
        let (rows, selected) = match &self.state {
            CompletionPromptState::Running => (
                vec![OverlayRow {
                    text: "Waiting for language server…".to_owned(),
                    enabled: false,
                }],
                None,
            ),
            CompletionPromptState::Failed(error) => (
                vec![OverlayRow {
                    text: format!("Completion failed: {error}"),
                    enabled: false,
                }],
                None,
            ),
            CompletionPromptState::Ready { items, visible } => {
                let documentation = visible
                    .get(self.selected)
                    .and_then(|index| items.get(*index))
                    .and_then(|item| item.documentation.as_deref());
                let item_budget = MAX_OVERLAY_SNAPSHOT_ROWS
                    .saturating_sub(usize::from(documentation.is_some()))
                    .max(1);
                let window = overlay_window(visible.len(), self.selected, item_budget);
                let mut rows = visible[window.clone()]
                    .iter()
                    .filter_map(|index| items.get(*index))
                    .map(|item| {
                        let kind = item
                            .kind
                            .as_deref()
                            .map(|kind| format!(" [{kind}]"))
                            .unwrap_or_default();
                        let detail = item
                            .detail
                            .as_deref()
                            .filter(|detail| !item.label.contains(*detail))
                            .map(|detail| format!(" — {detail}"))
                            .unwrap_or_default();
                        OverlayRow {
                            text: format!("{}{}{}", item.label, kind, detail),
                            enabled: true,
                        }
                    })
                    .collect::<Vec<_>>();
                if let Some(documentation) = documentation {
                    rows.push(OverlayRow {
                        text: format!(
                            "Docs: {}",
                            documentation
                                .split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ")
                        ),
                        enabled: false,
                    });
                }
                (
                    rows,
                    (!visible.is_empty()).then_some(self.selected.saturating_sub(window.start)),
                )
            }
        };
        OverlaySnapshot {
            title: " Completions ".to_owned(),
            rows,
            selected,
        }
    }
}

#[derive(Debug)]
enum HoverPromptState {
    Running,
    Ready(Vec<HoverPresentation>),
    Failed(String),
}

#[derive(Debug)]
struct HoverPrompt {
    buffer_id: u64,
    generation: u64,
    selected_row: usize,
    state: HoverPromptState,
}

impl HoverPrompt {
    fn running(buffer_id: u64, generation: u64) -> Self {
        Self {
            buffer_id,
            generation,
            selected_row: 0,
            state: HoverPromptState::Running,
        }
    }

    fn complete(
        &mut self,
        buffer_id: u64,
        generation: u64,
        result: std::result::Result<Vec<HoverPresentation>, String>,
    ) -> bool {
        if self.buffer_id != buffer_id || self.generation != generation {
            return false;
        }
        self.selected_row = 0;
        self.state = match result {
            Ok(mut items) => {
                items.truncate(MAX_LANGUAGE_RESPONSE_ITEMS);
                for item in &mut items {
                    item.kind = bounded_terminal_text(&item.kind);
                    item.text = bounded_terminal_text(&item.text);
                }
                HoverPromptState::Ready(items)
            }
            Err(error) => HoverPromptState::Failed(error),
        };
        true
    }

    fn presentation_rows(&self) -> Vec<OverlayRow> {
        match &self.state {
            HoverPromptState::Running => vec![OverlayRow {
                text: "Waiting for language server…".to_owned(),
                enabled: false,
            }],
            HoverPromptState::Failed(error) => vec![OverlayRow {
                text: format!("Hover failed: {error}"),
                enabled: false,
            }],
            HoverPromptState::Ready(items) if items.is_empty() => vec![OverlayRow {
                text: "No hover information at the cursor".to_owned(),
                enabled: false,
            }],
            HoverPromptState::Ready(items) => items
                .iter()
                .flat_map(|item| {
                    let mut lines = Vec::new();
                    lines.push(OverlayRow {
                        text: format!("[{}]", item.kind),
                        enabled: false,
                    });
                    if item.text.is_empty() {
                        lines.push(OverlayRow {
                            text: String::new(),
                            enabled: true,
                        });
                    } else {
                        lines.extend(item.text.lines().map(|line| OverlayRow {
                            text: line.replace('\t', "    "),
                            enabled: true,
                        }));
                    }
                    lines
                })
                .collect(),
        }
    }

    fn step(&mut self, direction: TabDirection) {
        let len = self.presentation_rows().len();
        let Some(next) = tabs::adjacent_index(self.selected_row, len, direction) else {
            return;
        };
        self.selected_row = next;
    }

    fn status(&self, message: Option<&str>) -> (String, usize) {
        let prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let detail = match &self.state {
            HoverPromptState::Running => "requesting…".to_owned(),
            HoverPromptState::Failed(error) => format!("failed: {error}"),
            HoverPromptState::Ready(_) => {
                let count = self.presentation_rows().len();
                format!(
                    "{}/{}",
                    self.selected_row.saturating_add(1).min(count),
                    count
                )
            }
        };
        (format!("{prefix}Hover  {detail}  ↑/↓ scroll  Esc close"), 0)
    }

    fn overlay(&self) -> OverlaySnapshot {
        let rows = self.presentation_rows();
        let selected =
            matches!(self.state, HoverPromptState::Ready(ref items) if !items.is_empty())
                .then_some(self.selected_row.min(rows.len().saturating_sub(1)));
        let (rows, selected) = bounded_overlay_rows(rows, selected);
        OverlaySnapshot {
            title: " Hover ".to_owned(),
            rows,
            selected,
        }
    }
}

#[derive(Debug)]
enum DiagnosticsPromptState {
    Running,
    Ready {
        items: Vec<DiagnosticPresentation>,
        visible: Vec<usize>,
    },
    Failed(String),
}

#[derive(Debug)]
struct DiagnosticsPrompt {
    generation: u64,
    prompt: LinePrompt,
    selected: usize,
    feedback: Option<String>,
    state: DiagnosticsPromptState,
}

impl DiagnosticsPrompt {
    fn running(generation: u64) -> Self {
        Self {
            generation,
            prompt: LinePrompt::new(),
            selected: 0,
            feedback: None,
            state: DiagnosticsPromptState::Running,
        }
    }

    fn complete(
        &mut self,
        generation: u64,
        result: std::result::Result<Vec<DiagnosticPresentation>, String>,
    ) -> bool {
        if self.generation != generation {
            return false;
        }
        self.selected = 0;
        self.feedback = None;
        self.state = match result {
            Ok(mut items) => {
                items.truncate(MAX_LANGUAGE_RESPONSE_ITEMS);
                for item in &mut items {
                    item.label = bounded_terminal_text(&item.label);
                    item.severity = bounded_terminal_text(&item.severity);
                    item.message = bounded_terminal_text(&item.message);
                    item.source = item.source.as_deref().map(bounded_terminal_text);
                }
                DiagnosticsPromptState::Ready {
                    visible: (0..items.len()).collect(),
                    items,
                }
            }
            Err(error) => DiagnosticsPromptState::Failed(error),
        };
        self.refresh();
        true
    }

    fn refresh(&mut self) {
        let DiagnosticsPromptState::Ready { items, visible } = &mut self.state else {
            return;
        };
        let query = self.prompt.text().to_lowercase();
        *visible = items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                query.is_empty()
                    || item.label.to_lowercase().contains(&query)
                    || item.message.to_lowercase().contains(&query)
                    || item.severity.to_lowercase().contains(&query)
                    || item
                        .source
                        .as_deref()
                        .is_some_and(|source| source.to_lowercase().contains(&query))
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
        self.feedback = None;
    }

    fn step(&mut self, direction: TabDirection) {
        let len = match &self.state {
            DiagnosticsPromptState::Ready { visible, .. } => visible.len(),
            DiagnosticsPromptState::Running | DiagnosticsPromptState::Failed(_) => 0,
        };
        let Some(next) = tabs::adjacent_index(self.selected, len, direction) else {
            return;
        };
        self.selected = next;
        self.feedback = None;
    }

    fn selected_item(&self) -> Option<DiagnosticPresentation> {
        let DiagnosticsPromptState::Ready { items, visible } = &self.state else {
            return None;
        };
        visible
            .get(self.selected)
            .and_then(|index| items.get(*index))
            .cloned()
    }

    fn visible_items(&self) -> Vec<DiagnosticPresentation> {
        let DiagnosticsPromptState::Ready { items, visible } = &self.state else {
            return Vec::new();
        };
        visible
            .iter()
            .filter_map(|index| items.get(*index))
            .cloned()
            .collect()
    }

    fn status(&self, message: Option<&str>) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prefix = format!("{message_prefix}Diagnostics filter: ");
        let cursor = prefix.width().saturating_add(
            self.prompt
                .text()
                .get(..self.prompt.cursor())
                .unwrap_or_default()
                .width(),
        );
        let detail = match &self.state {
            DiagnosticsPromptState::Running => "collecting…".to_owned(),
            DiagnosticsPromptState::Ready { visible, .. } => format!(
                "{}/{}",
                usize::from(!visible.is_empty()).saturating_mul(self.selected.saturating_add(1)),
                visible.len()
            ),
            DiagnosticsPromptState::Failed(error) => format!("failed: {error}"),
        };
        let feedback = self
            .feedback
            .as_deref()
            .map(|feedback| format!("  |  {feedback}"))
            .unwrap_or_default();
        (
            format!(
                "{prefix}{}  {detail}  Enter open  F9 MultiBuffer  ↑/↓ select  Esc close{feedback}",
                self.prompt.text()
            ),
            cursor,
        )
    }

    fn overlay(&self) -> OverlaySnapshot {
        let (rows, selected) = match &self.state {
            DiagnosticsPromptState::Running => (
                vec![OverlayRow {
                    text: "Collecting project diagnostics…".to_owned(),
                    enabled: false,
                }],
                None,
            ),
            DiagnosticsPromptState::Failed(error) => (
                vec![OverlayRow {
                    text: format!("Diagnostics failed: {error}"),
                    enabled: false,
                }],
                None,
            ),
            DiagnosticsPromptState::Ready { items: _, visible } if visible.is_empty() => (
                vec![OverlayRow {
                    text: "No matching diagnostics".to_owned(),
                    enabled: false,
                }],
                None,
            ),
            DiagnosticsPromptState::Ready { items, visible } => {
                let window =
                    overlay_window(visible.len(), self.selected, MAX_OVERLAY_SNAPSHOT_ROWS);
                (
                    visible[window.clone()]
                        .iter()
                        .filter_map(|index| items.get(*index))
                        .map(|item| {
                            let source = item
                                .source
                                .as_deref()
                                .map(|source| format!(" [{source}]"))
                                .unwrap_or_default();
                            OverlayRow {
                                text: format!(
                                    "{} {}:{}:{} {}{}",
                                    item.severity,
                                    item.label,
                                    item.row.saturating_add(1),
                                    item.column.saturating_add(1),
                                    item.message
                                        .split_whitespace()
                                        .collect::<Vec<_>>()
                                        .join(" "),
                                    source
                                ),
                                enabled: true,
                            }
                        })
                        .collect(),
                    Some(self.selected.saturating_sub(window.start)),
                )
            }
        };
        OverlaySnapshot {
            title: " Diagnostics ".to_owned(),
            rows,
            selected,
        }
    }
}

#[derive(Debug)]
enum LocationsPromptState {
    Running,
    Ready {
        items: Vec<LocationPresentation>,
        visible: Vec<usize>,
    },
    Failed(String),
}

#[derive(Debug)]
struct LocationsPrompt {
    buffer_id: u64,
    generation: u64,
    kind: LocationRequestKind,
    prompt: LinePrompt,
    selected: usize,
    feedback: Option<String>,
    state: LocationsPromptState,
}

impl LocationsPrompt {
    fn running(buffer_id: u64, generation: u64, kind: LocationRequestKind) -> Self {
        Self {
            buffer_id,
            generation,
            kind,
            prompt: LinePrompt::new(),
            selected: 0,
            feedback: None,
            state: LocationsPromptState::Running,
        }
    }

    fn complete(
        &mut self,
        buffer_id: u64,
        generation: u64,
        kind: LocationRequestKind,
        result: std::result::Result<Vec<LocationPresentation>, String>,
    ) -> bool {
        if self.buffer_id != buffer_id || self.generation != generation || self.kind != kind {
            return false;
        }
        self.selected = 0;
        self.feedback = None;
        self.state = match result {
            Ok(mut items) => {
                items.truncate(MAX_LANGUAGE_RESPONSE_ITEMS);
                for item in &mut items {
                    item.label = bounded_terminal_text(&item.label);
                    item.snippet = bounded_terminal_text(&item.snippet);
                }
                LocationsPromptState::Ready {
                    visible: (0..items.len()).collect(),
                    items,
                }
            }
            Err(error) => LocationsPromptState::Failed(error),
        };
        self.refresh();
        true
    }

    fn begin_request(&mut self, generation: u64) {
        self.generation = generation;
        self.selected = 0;
        self.feedback = None;
        self.state = LocationsPromptState::Running;
    }

    fn refresh(&mut self) {
        let LocationsPromptState::Ready { items, visible } = &mut self.state else {
            return;
        };
        let query = self.prompt.text().to_lowercase();
        *visible = items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                query.is_empty()
                    || item.label.to_lowercase().contains(&query)
                    || item.snippet.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
        self.feedback = None;
    }

    fn step(&mut self, direction: TabDirection) {
        let len = match &self.state {
            LocationsPromptState::Ready { visible, .. } => visible.len(),
            LocationsPromptState::Running | LocationsPromptState::Failed(_) => 0,
        };
        let Some(next) = tabs::adjacent_index(self.selected, len, direction) else {
            return;
        };
        self.selected = next;
        self.feedback = None;
    }

    fn selected_item(&self) -> Option<LocationPresentation> {
        let LocationsPromptState::Ready { items, visible } = &self.state else {
            return None;
        };
        visible
            .get(self.selected)
            .and_then(|index| items.get(*index))
            .cloned()
    }

    fn status(&self, message: Option<&str>) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prefix = format!("{message_prefix}{} filter: ", self.kind.title());
        let cursor = prefix.width().saturating_add(
            self.prompt
                .text()
                .get(..self.prompt.cursor())
                .unwrap_or_default()
                .width(),
        );
        let detail = match &self.state {
            LocationsPromptState::Running => "requesting…".to_owned(),
            LocationsPromptState::Ready { visible, .. } => format!(
                "{}/{}",
                usize::from(!visible.is_empty()).saturating_mul(self.selected.saturating_add(1)),
                visible.len()
            ),
            LocationsPromptState::Failed(error) => format!("failed: {error}"),
        };
        let feedback = self
            .feedback
            .as_deref()
            .map(|feedback| format!("  |  {feedback}"))
            .unwrap_or_default();
        (
            format!(
                "{prefix}{}  {detail}  Enter open  ↑/↓ select  Esc close{feedback}",
                self.prompt.text()
            ),
            cursor,
        )
    }

    fn overlay(&self) -> OverlaySnapshot {
        let (rows, selected) = match &self.state {
            LocationsPromptState::Running => (
                vec![OverlayRow {
                    text: format!("Requesting {}…", self.kind.title().to_lowercase()),
                    enabled: false,
                }],
                None,
            ),
            LocationsPromptState::Failed(error) => (
                vec![OverlayRow {
                    text: format!("{} failed: {error}", self.kind.title()),
                    enabled: false,
                }],
                None,
            ),
            LocationsPromptState::Ready { visible, .. } if visible.is_empty() => (
                vec![OverlayRow {
                    text: format!("No matching {}", self.kind.title().to_lowercase()),
                    enabled: false,
                }],
                None,
            ),
            LocationsPromptState::Ready { items, visible } => {
                let window =
                    overlay_window(visible.len(), self.selected, MAX_OVERLAY_SNAPSHOT_ROWS);
                (
                    visible[window.clone()]
                        .iter()
                        .filter_map(|index| items.get(*index))
                        .map(|item| OverlayRow {
                            text: format!(
                                "{}:{}:{}  {}",
                                item.label,
                                item.row.saturating_add(1),
                                item.column.saturating_add(1),
                                item.snippet
                            ),
                            enabled: true,
                        })
                        .collect(),
                    Some(self.selected.saturating_sub(window.start)),
                )
            }
        };
        OverlaySnapshot {
            title: format!(" {} ", self.kind.title()),
            rows,
            selected,
        }
    }
}

#[derive(Debug)]
enum RenamePromptState {
    Running,
    Ready(RenamePreparation),
    Previewing(RenamePreparation),
    Failed(String),
}

#[derive(Debug)]
struct RenamePrompt {
    buffer: Entity<Buffer>,
    buffer_id: u64,
    generation: u64,
    point: language::Point,
    prompt: LinePrompt,
    feedback: Option<String>,
    state: RenamePromptState,
}

impl RenamePrompt {
    fn running(
        buffer: Entity<Buffer>,
        buffer_id: u64,
        generation: u64,
        point: language::Point,
    ) -> Self {
        Self {
            buffer,
            buffer_id,
            generation,
            point,
            prompt: LinePrompt::new(),
            feedback: None,
            state: RenamePromptState::Running,
        }
    }

    fn complete(
        &mut self,
        buffer_id: u64,
        generation: u64,
        result: std::result::Result<RenamePreparation, String>,
    ) -> bool {
        if self.buffer_id != buffer_id || self.generation != generation {
            return false;
        }
        self.feedback = None;
        self.state = match result {
            Ok(preparation) => {
                self.prompt = LinePrompt::with_text(preparation.placeholder.clone());
                RenamePromptState::Ready(preparation)
            }
            Err(error) => RenamePromptState::Failed(error),
        };
        true
    }

    fn can_submit(&self) -> bool {
        matches!(self.state, RenamePromptState::Ready(_)) && !self.prompt.text().trim().is_empty()
    }

    fn begin_preview(&mut self) -> bool {
        let RenamePromptState::Ready(preparation) = &self.state else {
            return false;
        };
        let preparation = preparation.clone();
        self.feedback = None;
        self.state = RenamePromptState::Previewing(preparation);
        true
    }

    fn finish_preview(
        &mut self,
        buffer_id: u64,
        generation: u64,
        new_name: &str,
        result: &std::result::Result<(lsp::LanguageServerId, lsp::WorkspaceEdit), String>,
    ) -> bool {
        if self.buffer_id != buffer_id
            || self.generation != generation
            || self.prompt.text() != new_name
        {
            return false;
        }
        let RenamePromptState::Previewing(preparation) = &self.state else {
            return false;
        };
        if let Err(error) = result {
            let preparation = preparation.clone();
            self.state = RenamePromptState::Ready(preparation);
            self.feedback = Some(format!("preview failed: {error}"));
        }
        true
    }

    fn status(&self, message: Option<&str>) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prefix = format!("{message_prefix}Rename: ");
        let cursor = prefix.width().saturating_add(
            self.prompt
                .text()
                .get(..self.prompt.cursor())
                .unwrap_or_default()
                .width(),
        );
        let state = match &self.state {
            RenamePromptState::Running => "preparing…".to_owned(),
            RenamePromptState::Ready(_) => "Enter preview  Esc cancel".to_owned(),
            RenamePromptState::Previewing(_) => "building safe preview…  Esc cancel".to_owned(),
            RenamePromptState::Failed(error) => format!("failed: {error}  Esc close"),
        };
        let feedback = self
            .feedback
            .as_deref()
            .map(|feedback| format!("  |  {feedback}"))
            .unwrap_or_default();
        (
            format!("{prefix}{}  {state}{feedback}", self.prompt.text()),
            cursor,
        )
    }

    fn overlay(&self) -> OverlaySnapshot {
        let row = match &self.state {
            RenamePromptState::Running => OverlayRow {
                text: "Checking whether the symbol can be renamed…".to_owned(),
                enabled: false,
            },
            RenamePromptState::Ready(preparation) => OverlayRow {
                text: format!(
                    "{}  bytes {}..{} → {}",
                    preparation.placeholder,
                    preparation.start,
                    preparation.end,
                    self.prompt.text()
                ),
                enabled: self.can_submit(),
            },
            RenamePromptState::Previewing(preparation) => OverlayRow {
                text: format!(
                    "Validating WorkspaceEdit for {} → {}…",
                    preparation.placeholder,
                    self.prompt.text()
                ),
                enabled: false,
            },
            RenamePromptState::Failed(error) => OverlayRow {
                text: format!("Rename unavailable: {error}"),
                enabled: false,
            },
        };
        OverlaySnapshot {
            title: " Rename Symbol ".to_owned(),
            rows: vec![row],
            selected: matches!(self.state, RenamePromptState::Ready(_)).then_some(0),
        }
    }
}

fn code_action_title(action: &CodeAction) -> &str {
    action.lsp_action.title()
}

fn code_action_kind(action: &CodeAction) -> Option<String> {
    action
        .lsp_action
        .action_kind()
        .map(|kind| kind.as_str().to_owned())
}

fn code_action_preferred(action: &CodeAction) -> bool {
    matches!(
        &action.lsp_action,
        LspAction::Action(action) if action.is_preferred.unwrap_or(false)
    )
}

fn code_action_disabled_reason(action: &CodeAction) -> Option<&str> {
    match &action.lsp_action {
        LspAction::Action(action) => action
            .disabled
            .as_ref()
            .map(|disabled| disabled.reason.as_str()),
        LspAction::Command(_) | LspAction::CodeLens(_) => None,
    }
}

#[derive(Debug)]
enum CodeActionsPromptState {
    Running,
    Ready {
        items: Vec<CodeAction>,
        visible: Vec<usize>,
    },
    Failed(String),
}

#[derive(Debug)]
struct CodeActionsPrompt {
    buffer: Entity<Buffer>,
    buffer_id: u64,
    generation: u64,
    prompt: LinePrompt,
    selected: usize,
    feedback: Option<String>,
    state: CodeActionsPromptState,
}

impl CodeActionsPrompt {
    fn running(buffer: Entity<Buffer>, buffer_id: u64, generation: u64) -> Self {
        Self {
            buffer,
            buffer_id,
            generation,
            prompt: LinePrompt::new(),
            selected: 0,
            feedback: None,
            state: CodeActionsPromptState::Running,
        }
    }

    fn complete(
        &mut self,
        buffer_id: u64,
        generation: u64,
        result: std::result::Result<Vec<CodeAction>, String>,
    ) -> bool {
        if self.buffer_id != buffer_id || self.generation != generation {
            return false;
        }
        self.selected = 0;
        self.feedback = None;
        self.state = match result {
            Ok(mut items) => {
                items.truncate(MAX_LANGUAGE_RESPONSE_ITEMS);
                CodeActionsPromptState::Ready {
                    visible: (0..items.len()).collect(),
                    items,
                }
            }
            Err(error) => CodeActionsPromptState::Failed(error),
        };
        self.refresh();
        true
    }

    fn refresh(&mut self) {
        let CodeActionsPromptState::Ready { items, visible } = &mut self.state else {
            return;
        };
        let query = self.prompt.text().to_lowercase();
        *visible = items
            .iter()
            .enumerate()
            .filter(|(_, action)| {
                query.is_empty()
                    || code_action_title(action).to_lowercase().contains(&query)
                    || code_action_kind(action)
                        .is_some_and(|kind| kind.to_lowercase().contains(&query))
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
        self.feedback = None;
    }

    fn step(&mut self, direction: TabDirection) {
        let len = match &self.state {
            CodeActionsPromptState::Ready { visible, .. } => visible.len(),
            CodeActionsPromptState::Running | CodeActionsPromptState::Failed(_) => 0,
        };
        let Some(next) = tabs::adjacent_index(self.selected, len, direction) else {
            return;
        };
        self.selected = next;
        self.feedback = None;
    }

    fn selected_action(&self) -> Option<CodeAction> {
        let CodeActionsPromptState::Ready { items, visible } = &self.state else {
            return None;
        };
        visible
            .get(self.selected)
            .and_then(|index| items.get(*index))
            .filter(|action| code_action_disabled_reason(action).is_none())
            .cloned()
    }

    fn status(&self, message: Option<&str>) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prefix = format!("{message_prefix}Code actions: ");
        let cursor = prefix.width().saturating_add(
            self.prompt
                .text()
                .get(..self.prompt.cursor())
                .unwrap_or_default()
                .width(),
        );
        let detail = match &self.state {
            CodeActionsPromptState::Running => "requesting…".to_owned(),
            CodeActionsPromptState::Ready { visible, .. } => format!(
                "{}/{}",
                usize::from(!visible.is_empty()).saturating_mul(self.selected.saturating_add(1)),
                visible.len()
            ),
            CodeActionsPromptState::Failed(error) => format!("failed: {error}"),
        };
        let feedback = self
            .feedback
            .as_deref()
            .map(|feedback| format!("  |  {feedback}"))
            .unwrap_or_default();
        (
            format!(
                "{prefix}{}  {detail}  Enter apply  ↑/↓ select  Esc close{feedback}",
                self.prompt.text()
            ),
            cursor,
        )
    }

    fn overlay(&self) -> OverlaySnapshot {
        let (rows, selected) = match &self.state {
            CodeActionsPromptState::Running => (
                vec![OverlayRow {
                    text: "Requesting code actions…".to_owned(),
                    enabled: false,
                }],
                None,
            ),
            CodeActionsPromptState::Failed(error) => (
                vec![OverlayRow {
                    text: format!("Code actions failed: {error}"),
                    enabled: false,
                }],
                None,
            ),
            CodeActionsPromptState::Ready { visible, .. } if visible.is_empty() => (
                vec![OverlayRow {
                    text: "No matching code actions".to_owned(),
                    enabled: false,
                }],
                None,
            ),
            CodeActionsPromptState::Ready { items, visible } => {
                let window =
                    overlay_window(visible.len(), self.selected, MAX_OVERLAY_SNAPSHOT_ROWS);
                (
                    visible[window.clone()]
                        .iter()
                        .filter_map(|index| items.get(*index))
                        .map(|action| {
                            let kind = code_action_kind(action)
                                .map(|kind| format!(" [{kind}]"))
                                .unwrap_or_default();
                            let preferred = code_action_preferred(action)
                                .then_some(" ★ preferred")
                                .unwrap_or_default();
                            let disabled = code_action_disabled_reason(action)
                                .map(|reason| format!(" — disabled: {reason}"))
                                .unwrap_or_default();
                            OverlayRow {
                                text: format!(
                                    "{}{}{}{}",
                                    code_action_title(action),
                                    kind,
                                    preferred,
                                    disabled
                                ),
                                enabled: code_action_disabled_reason(action).is_none(),
                            }
                        })
                        .collect(),
                    Some(self.selected.saturating_sub(window.start)),
                )
            }
        };
        OverlaySnapshot {
            title: " Code Actions ".to_owned(),
            rows,
            selected,
        }
    }
}

#[derive(Debug)]
struct WorktreeTrustPrompt {
    worktree_id: settings::WorktreeId,
    path: PathBuf,
}

impl WorktreeTrustPrompt {
    fn status(&self, message: Option<&str>) -> (String, usize) {
        let prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        (
            format!(
                "{prefix}Restricted worktree: {}  Enter trust and enable project processes  Esc keep restricted",
                self.path.display()
            ),
            0,
        )
    }

    fn overlay(&self) -> OverlaySnapshot {
        OverlaySnapshot {
            title: " Worktree Trust ".to_owned(),
            rows: vec![
                OverlayRow {
                    text: self.path.display().to_string(),
                    enabled: false,
                },
                OverlayRow {
                    text: "Trusting permits project settings, language servers, tasks, and other repository-controlled processes.".to_owned(),
                    enabled: false,
                },
                OverlayRow {
                    text: "Enter: trust for this session    Esc: keep restricted".to_owned(),
                    enabled: true,
                },
            ],
            selected: Some(2),
        }
    }
}

#[derive(Debug)]
enum LanguageOverlay {
    Trust(WorktreeTrustPrompt),
    Hover(HoverPrompt),
    Diagnostics(DiagnosticsPrompt),
    Locations(LocationsPrompt),
    Rename(RenamePrompt),
    CodeActions(CodeActionsPrompt),
}

impl LanguageOverlay {
    fn status(&self, message: Option<&str>) -> (String, usize) {
        match self {
            Self::Trust(prompt) => prompt.status(message),
            Self::Hover(prompt) => prompt.status(message),
            Self::Diagnostics(prompt) => prompt.status(message),
            Self::Locations(prompt) => prompt.status(message),
            Self::Rename(prompt) => prompt.status(message),
            Self::CodeActions(prompt) => prompt.status(message),
        }
    }

    fn overlay(&self) -> OverlaySnapshot {
        match self {
            Self::Trust(prompt) => prompt.overlay(),
            Self::Hover(prompt) => prompt.overlay(),
            Self::Diagnostics(prompt) => prompt.overlay(),
            Self::Locations(prompt) => prompt.overlay(),
            Self::Rename(prompt) => prompt.overlay(),
            Self::CodeActions(prompt) => prompt.overlay(),
        }
    }

    fn has_status_cursor(&self) -> bool {
        matches!(
            self,
            Self::Diagnostics(_) | Self::Locations(_) | Self::Rename(_) | Self::CodeActions(_)
        )
    }
}

struct TerminalCompletionProvider {
    project: Entity<Project>,
    generation: Arc<AtomicU64>,
    event_sender: async_channel::Sender<TerminalEvent>,
}

impl CompletionProvider for TerminalCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: text::Anchor,
        trigger: editor::CompletionContext,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Editor>,
    ) -> Task<Result<Vec<project::CompletionResponse>>> {
        let task = self.project.update(cx, |project, cx| {
            let task = project.completions(buffer, buffer_position, trigger, cx);
            cx.background_spawn(task)
        });
        let generation = self.generation.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        let buffer_id = buffer.read(cx).remote_id().to_proto();
        let sender = self.event_sender.clone();
        cx.spawn(async move |_editor, _cx| {
            let result = task.await;
            let presentation_result = result
                .as_ref()
                .map(|responses| completion_presentations(responses))
                .map_err(|error| format!("{error:#}"));
            let _ = sender
                .send(TerminalEvent::CompletionFinished {
                    buffer_id,
                    generation,
                    menu_wait_attempt: 0,
                    result: presentation_result,
                })
                .await;
            result
        })
    }

    fn resolve_completions(
        &self,
        buffer: Entity<Buffer>,
        completion_indices: Vec<usize>,
        completions: Rc<RefCell<Box<[project::Completion]>>>,
        cx: &mut gpui::Context<Editor>,
    ) -> Task<Result<bool>> {
        CompletionProvider::resolve_completions(
            &self.project,
            buffer,
            completion_indices,
            completions,
            cx,
        )
    }

    fn apply_additional_edits_for_completion(
        &self,
        buffer: Entity<Buffer>,
        completions: Rc<RefCell<Box<[project::Completion]>>>,
        completion_index: usize,
        push_to_history: bool,
        all_commit_ranges: Vec<Range<language::Anchor>>,
        cx: &mut gpui::Context<Editor>,
    ) -> Task<Result<Option<language::Transaction>>> {
        CompletionProvider::apply_additional_edits_for_completion(
            &self.project,
            buffer,
            completions,
            completion_index,
            push_to_history,
            all_commit_ranges,
            cx,
        )
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        text: &str,
        trigger_in_words: bool,
        cx: &mut gpui::Context<Editor>,
    ) -> bool {
        CompletionProvider::is_completion_trigger(
            &self.project,
            buffer,
            position,
            text,
            trigger_in_words,
            cx,
        )
    }

    fn sort_completions(&self) -> bool {
        false
    }

    fn filter_completions(&self) -> bool {
        false
    }

    fn show_snippets(&self) -> bool {
        false
    }
}

fn completion_presentations(
    responses: &[project::CompletionResponse],
) -> Vec<terminal::CompletionPresentation> {
    responses
        .iter()
        .flat_map(|response| &response.completions)
        .take(MAX_LANGUAGE_RESPONSE_ITEMS)
        .map(|completion| {
            let lsp_completion = completion.source.lsp_completion(false);
            let detail = lsp_completion
                .as_ref()
                .and_then(|completion| completion.detail.as_deref())
                .map(bounded_terminal_text);
            let kind = lsp_completion
                .as_ref()
                .and_then(|completion| completion.kind)
                .map(|kind| bounded_terminal_text(&format!("{kind:?}")));
            let documentation = lsp_completion
                .as_ref()
                .and_then(|completion| completion.documentation.as_ref())
                .map(|documentation| match documentation {
                    lsp::Documentation::String(text) => bounded_terminal_text(text),
                    lsp::Documentation::MarkupContent(markup) => {
                        bounded_terminal_text(&markup.value)
                    }
                });
            terminal::CompletionPresentation {
                label: bounded_terminal_text(&completion.label.text),
                detail,
                kind,
                documentation,
            }
        })
        .collect()
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

/// Pure scheduling state for bounded project searches. A running request keeps
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
/// replaced freely; project-search tasks are removed only after their finish event.
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
        // Every edit invalidates the old query immediately. Key requests still
        // wait for the 16 ms trailing debounce before dispatch, but their old
        // file workers stop consuming CPU during that debounce window.
        self.cancel_session_searches(session);
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
            // The detached outer task retains and naturally joins every fixed
            // worker after app-level cancellation.
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
        Command::Alpha2Probe(probe) => run_alpha_2_probe(probe),
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
    if first == "--alpha-2-probe" {
        let case = arguments
            .get(1)
            .and_then(|case| case.to_str())
            .context("--alpha-2-probe requires a UTF-8 case name")?;
        let probe = match case {
            "language-service" => {
                ensure!(
                    arguments.len() == 4,
                    "--alpha-2-probe language-service requires ROOT FILE"
                );
                Alpha2Probe::LanguageService {
                    root: arguments[2].clone().into(),
                    file: arguments[3].clone().into(),
                }
            }
            "settings-reload" => {
                ensure!(
                    arguments.len() == 4,
                    "--alpha-2-probe settings-reload requires ROOT FILE"
                );
                Alpha2Probe::SettingsReload {
                    root: arguments[2].clone().into(),
                    file: arguments[3].clone().into(),
                }
            }
            "lsp-failure" => {
                ensure!(
                    arguments.len() == 5,
                    "--alpha-2-probe lsp-failure requires ROOT FILE SCENARIO"
                );
                Alpha2Probe::LspFailure {
                    root: arguments[2].clone().into(),
                    file: arguments[3].clone().into(),
                    scenario: arguments[4]
                        .clone()
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("lsp-failure scenario must be UTF-8"))?,
                }
            }
            _ => bail!("unknown --alpha-2-probe case: {case}"),
        };
        return Ok(Command::Alpha2Probe(probe));
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

    // Keep action/configuration notifications lossless while retaining a hard
    // queue bound. Redundant redraw notifications still use try_send.
    let (event_sender, event_receiver) = async_channel::bounded(64);
    let redraw_sender = event_sender.clone();
    let mut input_reader = InputReader::spawn(event_sender)?;
    let terminal_session = TerminalSession::enter()?;
    let mut terminal = terminal_session.terminal()?;
    let (error_sender, error_receiver) = mpsc::sync_channel(1);

    editor_application().run(move |cx| {
        init_zed(cx);
        let configuration_file_system = cx.global::<ProjectRuntime>().file_system.clone();
        start_configuration_watchers(
            configuration_file_system,
            redraw_sender.clone(),
            cx,
        );
        let pending_terminal_actions = Rc::new(RefCell::new(VecDeque::new()));
        start_terminal_action_interceptor(pending_terminal_actions.clone(), None, cx);
        let services = file_services(cx);
        start_project_configuration_notifications(
            &services.project,
            redraw_sender.clone(),
            cx,
        );
        start_worktree_trust_notifications(
            services.worktree_store.clone(),
            redraw_sender.clone(),
            cx,
        );
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
                    &services,
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
            let mut startup_notice =
                (!startup_errors.is_empty()).then(|| startup_errors.join("  |  "));
            let mut message: Option<String> = None;
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
            let mut command_palette_prompt: Option<CommandPalettePrompt> = None;
            let mut completion_prompt: Option<CompletionPrompt> = None;
            let mut language_overlay: Option<LanguageOverlay> = None;
            let mut language_request_generation = 0u64;
            let mut navigation_history = NavigationHistory::default();
            let mut project_edit_history = ProjectEditHistory::default();
            let mut project_search_coordinator = ProjectSearchCoordinator::default();
            loop {
                let editor_window = tabs[active_index].editor_window;
                let input_window: AnyWindowHandle = editor_window.into();
                let display_message = match (startup_notice.as_deref(), message.as_deref()) {
                    (Some(startup), Some(message)) => Some(format!("{startup}  |  {message}")),
                    (Some(startup), None) => Some(startup.to_owned()),
                    (None, Some(message)) => Some(message.to_owned()),
                    (None, None) => None,
                };
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
                                    display_message.as_deref(),
                                    completion_prompt.as_ref(),
                                    command_palette_prompt.as_ref(),
                                    language_overlay.as_ref(),
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
                let (mut event, action_from_keymap) = match event {
                    TerminalEvent::Action(action) => {
                        (TerminalEvent::Key(action.shortcut_event()), true)
                    }
                    event => (event, false),
                };
                if matches!(
                    &event,
                    TerminalEvent::Key(key)
                        if key.kind != crossterm::event::KeyEventKind::Release
                ) {
                    startup_notice = None;
                }

                let pending_rename = tabs[active_index]
                    .multi_buffer
                    .as_ref()
                    .and_then(|multi_buffer| multi_buffer.pending_rename.clone());
                if let (Some(pending), TerminalEvent::Key(key)) = (pending_rename, &event)
                    && key.kind != crossterm::event::KeyEventKind::Release
                    && key.modifiers == crossterm::event::KeyModifiers::NONE
                    && matches!(
                        key.code,
                        crossterm::event::KeyCode::Enter | crossterm::event::KeyCode::Esc
                    )
                {
                    if key.code == crossterm::event::KeyCode::Esc {
                        if let Err(error) = editor_window.update(cx, |_editor, window, _cx| {
                            window.remove_window();
                        }) {
                            failure = Some(format!(
                                "failed to close rejected rename preview: {error}"
                            ));
                            break;
                        }
                        tabs.remove(active_index);
                        if tabs.is_empty() {
                            break;
                        }
                        active_index = active_index.min(tabs.len().saturating_sub(1));
                        message = Some(format!(
                            "rename to {} rejected; no workspace edit was applied",
                            pending.new_name
                        ));
                        quit_armed = false;
                        close_armed = false;
                        reload_armed = false;
                        save_conflict_armed = false;
                        continue;
                    }

                    let new_name = pending.new_name.clone();
                    let accept_result: Result<ProjectTransaction> = async {
                        validate_pending_rename_guards(&pending, cx)?;
                        let (_, request) = request_rename_workspace_edit(
                            &services.project,
                            &pending.origin_buffer,
                            pending.origin_point,
                            pending.new_name.clone(),
                            Some(pending.language_server_id),
                            cx,
                        )?;
                        let latest_edit = request.await?;
                        let latest_plan = normalize_rename_workspace_edit(
                            &latest_edit,
                            &pending.workspace_root,
                        )?;
                        ensure!(
                            latest_plan.signature == pending.plan.signature,
                            "language server changed the rename WorkspaceEdit after preview; preview again"
                        );
                        validate_pending_rename_guards(&pending, cx)?;
                        services
                            .project
                            .update(cx, |project, cx| {
                                project.perform_rename(
                                    pending.origin_buffer.clone(),
                                    pending.origin_point,
                                    pending.new_name.clone(),
                                    cx,
                                )
                            })
                            .await
                            .context("apply accepted rename through Project")
                    }
                    .await;
                    match accept_result {
                        Ok(transaction) => {
                            let buffer_count = project_edit_history.push(transaction);
                            let file_operation_count = pending.plan.file_operation_count;
                            if let Err(error) =
                                editor_window.update(cx, |_editor, window, _cx| {
                                    window.remove_window();
                                })
                            {
                                failure = Some(format!(
                                    "rename applied but preview window could not close: {error}"
                                ));
                                break;
                            }
                            tabs.remove(active_index);
                            if tabs.is_empty() {
                                break;
                            }
                            active_index = active_index.min(tabs.len().saturating_sub(1));
                            message = Some(format!(
                                "renamed to {new_name}: {buffer_count} text buffer(s), {file_operation_count} file op(s); Ctrl-Z undoes text edits"
                            ));
                            quit_armed = false;
                            close_armed = false;
                            reload_armed = false;
                            save_conflict_armed = false;
                        }
                        Err(error) => {
                            message = Some(format!("rename acceptance failed: {error:#}"));
                        }
                    }
                    continue;
                }

                let other_overlay_is_open = save_as_prompt.is_some()
                    || open_prompt.is_some()
                    || go_to_line_prompt.is_some()
                    || quick_open_prompt.is_some()
                    || project_search_prompt.is_some()
                    || command_palette_prompt.is_some()
                    || active_search.is_some()
                    || completion_prompt.is_some()
                    || language_overlay.is_some();
                if !action_from_keymap
                    && !other_overlay_is_open
                    && let TerminalEvent::Key(key) = &event
                    && !input::is_scroll_page_up(key)
                    && !input::is_scroll_page_down(key)
                    && let Some(keystroke) = input::to_gpui_keystroke(*key)
                {
                    if let Err(error) = cx.update_window(input_window, |_root, window, cx| {
                        window.activate_window();
                        window.dispatch_keystroke(keystroke, cx)
                    }) {
                        failure = Some(format!("failed to dispatch keymap input: {error}"));
                        break;
                    }
                    let Some(action) = pending_terminal_actions.borrow_mut().pop_front() else {
                        continue;
                    };
                    event = TerminalEvent::Key(action.shortcut_event());
                }
                if command_palette_prompt.is_none()
                    && !other_overlay_is_open
                    && matches!(&event, TerminalEvent::Key(key) if input::is_command_palette(key))
                {
                    command_palette_prompt = Some(CommandPalettePrompt::new(action_context(
                        repository.as_ref(),
                        &tabs[active_index].document,
                        &services,
                        &navigation_history,
                        cx,
                    )));
                    message = None;
                    continue;
                }

                let mut palette_consumed_input = false;
                let mut close_palette = false;
                let mut invoke_palette_action = None;
                if let Some(palette) = command_palette_prompt.as_mut() {
                    match &event {
                        TerminalEvent::Key(key) => {
                            palette_consumed_input = true;
                            match palette.prompt.handle_key(key) {
                                PromptAction::Changed => palette.refresh(),
                                PromptAction::Next => palette.step(TabDirection::Next),
                                PromptAction::Previous => palette.step(TabDirection::Previous),
                                PromptAction::Submit | PromptAction::AlternateSubmit => {
                                    match palette.selected_action() {
                                        Some(descriptor) if descriptor.enabled => {
                                            invoke_palette_action = Some(descriptor.action);
                                        }
                                        Some(descriptor) => {
                                            palette.feedback = Some(format!(
                                                "{} is unavailable in the current focus",
                                                descriptor.name
                                            ));
                                        }
                                        None => {
                                            palette.feedback = Some("no matching action".to_owned());
                                        }
                                    }
                                }
                                PromptAction::Cancel => close_palette = true,
                                PromptAction::CursorMoved | PromptAction::Ignored => {}
                            }
                        }
                        TerminalEvent::Paste(text) => {
                            palette_consumed_input = true;
                            if palette.prompt.handle_paste(text) == PromptAction::Changed {
                                palette.refresh();
                            }
                        }
                        TerminalEvent::Mouse(_) | TerminalEvent::MouseScroll(_) => {
                            palette_consumed_input = true;
                        }
                        _ => {}
                    }
                }
                if close_palette {
                    command_palette_prompt = None;
                    message = None;
                    continue;
                }
                if let Some(action) = invoke_palette_action {
                    command_palette_prompt = None;
                    message = None;
                    event = TerminalEvent::Key(action.shortcut_event());
                } else if palette_consumed_input {
                    continue;
                }

                let mut completion_consumed_input = false;
                let mut cancel_completion = false;
                let mut confirm_completion = None;
                if let Some(completion) = completion_prompt.as_mut() {
                    match &event {
                        TerminalEvent::Key(key) => {
                            completion_consumed_input = true;
                            match completion.prompt.handle_key(key) {
                                PromptAction::Changed => completion.refresh(),
                                PromptAction::Next => completion.step(TabDirection::Next),
                                PromptAction::Previous => completion.step(TabDirection::Previous),
                                PromptAction::Submit | PromptAction::AlternateSubmit => {
                                    confirm_completion = completion.selected_item_index();
                                }
                                PromptAction::Cancel => cancel_completion = true,
                                PromptAction::CursorMoved | PromptAction::Ignored => {}
                            }
                        }
                        TerminalEvent::Paste(text) => {
                            completion_consumed_input = true;
                            if completion.prompt.handle_paste(text) == PromptAction::Changed {
                                completion.refresh();
                            }
                        }
                        TerminalEvent::Mouse(_) | TerminalEvent::MouseScroll(_) => {
                            completion_consumed_input = true;
                        }
                        _ => {}
                    }
                }
                if cancel_completion {
                    completion_prompt = None;
                    message = None;
                    // Forward Escape to Zed so its hidden completion menu is also closed.
                } else if let Some(item_index) = confirm_completion {
                    completion_prompt = None;
                    let task = match editor_window.update(cx, |editor, window, cx| {
                        editor.confirm_completion(
                            &ConfirmCompletion {
                                item_ix: Some(item_index),
                            },
                            window,
                            cx,
                        )
                    }) {
                        Ok(task) => task,
                        Err(error) => {
                            failure = Some(format!("failed to confirm completion: {error}"));
                            break;
                        }
                    };
                    match task {
                        Some(task) => match task.await {
                            Ok(()) => message = Some("completion applied".to_owned()),
                            Err(error) => {
                                message = Some(format!("completion failed: {error:#}"));
                            }
                        },
                        None => message = Some("completion is no longer available".to_owned()),
                    }
                    continue;
                } else if completion_consumed_input && !cancel_completion {
                    continue;
                }

                let mut language_consumed_input = false;
                let mut close_language_overlay = false;
                let mut diagnostic_to_open = None;
                let mut diagnostics_multibuffer_to_open = None;
                let mut location_to_open = None;
                let mut project_symbol_request = None;
                let mut rename_to_preview = None;
                let mut code_action_to_apply = None;
                let mut worktree_to_trust = None;
                if let Some(overlay) = language_overlay.as_mut() {
                    match overlay {
                        LanguageOverlay::Trust(prompt) => match &event {
                            TerminalEvent::Key(key)
                                if key.kind != crossterm::event::KeyEventKind::Release
                                    && key.modifiers
                                        == crossterm::event::KeyModifiers::NONE =>
                            {
                                language_consumed_input = true;
                                match key.code {
                                    crossterm::event::KeyCode::Enter => {
                                        worktree_to_trust = Some(prompt.worktree_id)
                                    }
                                    crossterm::event::KeyCode::Esc => {
                                        close_language_overlay = true
                                    }
                                    _ => {}
                                }
                            }
                            TerminalEvent::Key(_)
                            | TerminalEvent::Paste(_)
                            | TerminalEvent::Mouse(_)
                            | TerminalEvent::MouseScroll(_) => {
                                language_consumed_input = true;
                            }
                            _ => {}
                        },
                        LanguageOverlay::Hover(prompt) => match &event {
                            TerminalEvent::Key(key) => {
                                language_consumed_input = true;
                                if input::is_hover(key)
                                    || (key.kind != crossterm::event::KeyEventKind::Release
                                        && key.modifiers
                                            == crossterm::event::KeyModifiers::NONE
                                        && matches!(
                                            key.code,
                                            crossterm::event::KeyCode::Esc
                                                | crossterm::event::KeyCode::Enter
                                        ))
                                {
                                    close_language_overlay = true;
                                } else if key.kind
                                    != crossterm::event::KeyEventKind::Release
                                    && key.modifiers == crossterm::event::KeyModifiers::NONE
                                {
                                    match key.code {
                                        crossterm::event::KeyCode::Down => {
                                            prompt.step(TabDirection::Next)
                                        }
                                        crossterm::event::KeyCode::Up => {
                                            prompt.step(TabDirection::Previous)
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            TerminalEvent::Paste(_)
                            | TerminalEvent::Mouse(_)
                            | TerminalEvent::MouseScroll(_) => {
                                language_consumed_input = true;
                            }
                            _ => {}
                        },
                        LanguageOverlay::Diagnostics(prompt) => match &event {
                            TerminalEvent::Key(key) => {
                                language_consumed_input = true;
                                if key.kind != crossterm::event::KeyEventKind::Release
                                    && key.code == crossterm::event::KeyCode::F(9)
                                    && key.modifiers == crossterm::event::KeyModifiers::NONE
                                {
                                    let items = prompt.visible_items();
                                    if items.is_empty() {
                                        prompt.feedback =
                                            Some("no matching diagnostics".to_owned());
                                    } else {
                                        diagnostics_multibuffer_to_open = Some(items);
                                    }
                                } else {
                                    match prompt.prompt.handle_key(key) {
                                    PromptAction::Changed => prompt.refresh(),
                                    PromptAction::Next => prompt.step(TabDirection::Next),
                                    PromptAction::Previous => {
                                        prompt.step(TabDirection::Previous)
                                    }
                                    PromptAction::Submit | PromptAction::AlternateSubmit => {
                                        if let Some(item) = prompt.selected_item() {
                                            diagnostic_to_open = Some(item);
                                        } else {
                                            prompt.feedback =
                                                Some("no matching diagnostic".to_owned());
                                        }
                                    }
                                    PromptAction::Cancel => close_language_overlay = true,
                                    PromptAction::CursorMoved | PromptAction::Ignored => {}
                                    }
                                }
                            }
                            TerminalEvent::Paste(text) => {
                                language_consumed_input = true;
                                if prompt.prompt.handle_paste(text) == PromptAction::Changed {
                                    prompt.refresh();
                                }
                            }
                            TerminalEvent::Mouse(_) | TerminalEvent::MouseScroll(_) => {
                                language_consumed_input = true;
                            }
                            _ => {}
                        },
                        LanguageOverlay::Locations(prompt) => match &event {
                            TerminalEvent::Key(key) => {
                                language_consumed_input = true;
                                match prompt.prompt.handle_key(key) {
                                    PromptAction::Changed
                                        if prompt.kind
                                            == LocationRequestKind::ProjectSymbols =>
                                    {
                                        language_request_generation =
                                            language_request_generation.wrapping_add(1).max(1);
                                        prompt.begin_request(language_request_generation);
                                        project_symbol_request = Some((
                                            prompt.buffer_id,
                                            language_request_generation,
                                            prompt.prompt.text().to_owned(),
                                        ));
                                    }
                                    PromptAction::Changed => prompt.refresh(),
                                    PromptAction::Next => prompt.step(TabDirection::Next),
                                    PromptAction::Previous => {
                                        prompt.step(TabDirection::Previous)
                                    }
                                    PromptAction::Submit | PromptAction::AlternateSubmit => {
                                        if let Some(item) = prompt.selected_item() {
                                            location_to_open = Some(item);
                                        } else {
                                            prompt.feedback =
                                                Some("no matching location".to_owned());
                                        }
                                    }
                                    PromptAction::Cancel => close_language_overlay = true,
                                    PromptAction::CursorMoved | PromptAction::Ignored => {}
                                }
                            }
                            TerminalEvent::Paste(text) => {
                                language_consumed_input = true;
                                if prompt.prompt.handle_paste(text) == PromptAction::Changed {
                                    if prompt.kind == LocationRequestKind::ProjectSymbols {
                                        language_request_generation =
                                            language_request_generation.wrapping_add(1).max(1);
                                        prompt.begin_request(language_request_generation);
                                        project_symbol_request = Some((
                                            prompt.buffer_id,
                                            language_request_generation,
                                            prompt.prompt.text().to_owned(),
                                        ));
                                    } else {
                                        prompt.refresh();
                                    }
                                }
                            }
                            TerminalEvent::Mouse(_) | TerminalEvent::MouseScroll(_) => {
                                language_consumed_input = true;
                            }
                            _ => {}
                        },
                        LanguageOverlay::Rename(prompt) => match &event {
                            TerminalEvent::Key(key) => {
                                language_consumed_input = true;
                                if matches!(
                                    &prompt.state,
                                    RenamePromptState::Running
                                        | RenamePromptState::Previewing(_)
                                        | RenamePromptState::Failed(_)
                                ) {
                                    if key.kind != crossterm::event::KeyEventKind::Release
                                        && key.code == crossterm::event::KeyCode::Esc
                                        && key.modifiers
                                            == crossterm::event::KeyModifiers::NONE
                                    {
                                        close_language_overlay = true;
                                    }
                                } else {
                                    match prompt.prompt.handle_key(key) {
                                        PromptAction::Changed => prompt.feedback = None,
                                        PromptAction::Submit
                                        | PromptAction::AlternateSubmit => {
                                            if prompt.can_submit() {
                                                rename_to_preview = Some((
                                                    prompt.buffer.clone(),
                                                    prompt.point,
                                                    prompt.buffer_id,
                                                    prompt.generation,
                                                    prompt.prompt.text().to_owned(),
                                                ));
                                                let _ = prompt.begin_preview();
                                            } else {
                                                prompt.feedback =
                                                    Some("new name must not be empty".to_owned());
                                            }
                                        }
                                        PromptAction::Cancel => close_language_overlay = true,
                                        PromptAction::CursorMoved
                                        | PromptAction::Next
                                        | PromptAction::Previous
                                        | PromptAction::Ignored => {}
                                    }
                                }
                            }
                            TerminalEvent::Paste(text) => {
                                language_consumed_input = true;
                                if matches!(&prompt.state, RenamePromptState::Ready(_))
                                    && prompt.prompt.handle_paste(text) == PromptAction::Changed
                                {
                                    prompt.feedback = None;
                                }
                            }
                            TerminalEvent::Mouse(_) | TerminalEvent::MouseScroll(_) => {
                                language_consumed_input = true;
                            }
                            _ => {}
                        },
                        LanguageOverlay::CodeActions(prompt) => match &event {
                            TerminalEvent::Key(key) => {
                                language_consumed_input = true;
                                match prompt.prompt.handle_key(key) {
                                    PromptAction::Changed => prompt.refresh(),
                                    PromptAction::Next => prompt.step(TabDirection::Next),
                                    PromptAction::Previous => {
                                        prompt.step(TabDirection::Previous)
                                    }
                                    PromptAction::Submit
                                    | PromptAction::AlternateSubmit => {
                                        if let Some(action) = prompt.selected_action() {
                                            code_action_to_apply = Some((
                                                prompt.buffer.clone(),
                                                action,
                                            ));
                                        } else {
                                            prompt.feedback = Some(
                                                "no enabled matching code action".to_owned(),
                                            );
                                        }
                                    }
                                    PromptAction::Cancel => close_language_overlay = true,
                                    PromptAction::CursorMoved | PromptAction::Ignored => {}
                                }
                            }
                            TerminalEvent::Paste(text) => {
                                language_consumed_input = true;
                                if prompt.prompt.handle_paste(text) == PromptAction::Changed {
                                    prompt.refresh();
                                }
                            }
                            TerminalEvent::Mouse(_) | TerminalEvent::MouseScroll(_) => {
                                language_consumed_input = true;
                            }
                            _ => {}
                        },
                    }
                }
                if let Some(worktree_id) = worktree_to_trust {
                    let Some(trusted_worktrees) =
                        cx.update(|cx| TrustedWorktrees::try_get_global(cx))
                    else {
                        failure = Some("worktree trust service is unavailable".to_owned());
                        break;
                    };
                    trusted_worktrees.update(cx, |trusted_worktrees, cx| {
                        trusted_worktrees.trust(
                            &services.worktree_store,
                            [PathTrust::Worktree(worktree_id)].into_iter().collect(),
                            cx,
                        );
                    });
                    language_overlay = None;
                    message = Some("worktree trusted for this session; project processes enabled".to_owned());
                    continue;
                }
                if let Some((buffer_id, generation, query)) = project_symbol_request {
                    let root = repository
                        .as_ref()
                        .map(|repository| repository.root.canonical_path().to_path_buf());
                    start_project_symbols_request(
                        &services.project,
                        buffer_id,
                        generation,
                        query,
                        root,
                        redraw_sender.clone(),
                        cx,
                    );
                }
                if close_language_overlay {
                    language_overlay = None;
                    message = None;
                    continue;
                }
                if let Some((buffer, point, buffer_id, generation, new_name)) = rename_to_preview {
                    if let Err(error) = start_rename_preview_request(
                        &services.project,
                        buffer,
                        point,
                        buffer_id,
                        generation,
                        new_name.clone(),
                        redraw_sender.clone(),
                        cx,
                    ) {
                        let result = Err(format!("{error:#}"));
                        if let Some(LanguageOverlay::Rename(prompt)) = language_overlay.as_mut() {
                            let _ = prompt.finish_preview(
                                buffer_id,
                                generation,
                                &new_name,
                                &result,
                            );
                        }
                    } else {
                        message = Some("building rename preview…".to_owned());
                    }
                    continue;
                }
                if let Some((buffer, action)) = code_action_to_apply {
                    let title = code_action_title(&action).to_owned();
                    let task = services.project.update(cx, |project, cx| {
                        project.apply_code_action(buffer, action, true, cx)
                    });
                    match task.await {
                        Ok(transaction) => {
                            let buffer_count = project_edit_history.push(transaction);
                            language_overlay = None;
                            message = Some(format!(
                                "applied {title} in {buffer_count} buffer(s); Ctrl-Z undoes all"
                            ));
                        }
                        Err(error) => {
                            if let Some(LanguageOverlay::CodeActions(prompt)) =
                                language_overlay.as_mut()
                            {
                                prompt.feedback =
                                    Some(format!("code action failed: {error:#}"));
                            }
                        }
                    }
                    continue;
                }
                if let Some(items) = diagnostics_multibuffer_to_open {
                    let locations = items
                        .iter()
                        .map(|item| LocationPresentation {
                            path: item.path.clone(),
                            label: item.label.clone(),
                            row: item.row,
                            column: item.column,
                            end_row: item.row,
                            end_column: item.column.saturating_add(1),
                            snippet: item.message.clone(),
                        })
                        .collect::<Vec<_>>();
                    match create_locations_multibuffer_tab(
                        "Project Diagnostics".to_owned(),
                        &locations,
                        repository.as_ref(),
                        &services,
                        redraw_sender.clone(),
                        cx,
                    )
                    .await
                    {
                        Ok(tab) => {
                            tabs.push(tab);
                            active_index = tabs.len().saturating_sub(1);
                            language_overlay = None;
                            message = Some(format!(
                                "opened {} diagnostic(s) in an editable MultiBuffer",
                                locations.len()
                            ));
                        }
                        Err(error) => {
                            if let Some(LanguageOverlay::Diagnostics(prompt)) =
                                language_overlay.as_mut()
                            {
                                prompt.feedback =
                                    Some(format!("MultiBuffer open failed: {error:#}"));
                            }
                        }
                    }
                    continue;
                }
                if let Some(item) = diagnostic_to_open {
                    let origin = current_navigation_point(&tabs, active_index, cx).ok();
                    match navigate_to_diagnostic(
                        &item,
                        repository.as_ref(),
                        &services,
                        &mut tabs,
                        &mut active_index,
                        redraw_sender.clone(),
                        cx,
                    )
                    .await
                    {
                        Ok(opened) => {
                            if let Some(origin) = origin {
                                navigation_history.record_jump(origin);
                            }
                            language_overlay = None;
                            message = Some(opened);
                        }
                        Err(error) => {
                            if let Some(LanguageOverlay::Diagnostics(prompt)) =
                                language_overlay.as_mut()
                            {
                                prompt.feedback =
                                    Some(format!("open failed: {error:#}"));
                            }
                        }
                    }
                    continue;
                }
                if let Some(item) = location_to_open {
                    let origin = current_navigation_point(&tabs, active_index, cx).ok();
                    match navigate_to_location(
                        &item,
                        repository.as_ref(),
                        &services,
                        &mut tabs,
                        &mut active_index,
                        redraw_sender.clone(),
                        cx,
                    )
                    .await
                    {
                        Ok(opened) => {
                            if let Some(origin) = origin {
                                navigation_history.record_jump(origin);
                            }
                            language_overlay = None;
                            message = Some(opened);
                        }
                        Err(error) => {
                            if let Some(LanguageOverlay::Locations(prompt)) =
                                language_overlay.as_mut()
                            {
                                prompt.feedback = Some(format!("open failed: {error:#}"));
                            }
                        }
                    }
                    continue;
                }
                if language_consumed_input {
                    continue;
                }

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
                            .filter(|tab| tab_state(tab, cx).needs_discard_confirmation())
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
                            tab_state(&tabs[active_index], cx).needs_discard_confirmation();
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
                            if tabs[active_index]
                                .multi_buffer
                                .as_ref()
                                .is_some_and(|multi_buffer| multi_buffer.pending_rename.is_some())
                            {
                                message = Some(
                                    "rename preview cannot be reloaded; Enter accepts or Esc rejects"
                                        .to_owned(),
                                );
                                continue;
                            }
                            let state = tab_state(&tabs[active_index], cx);
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
                            match reload_tab(&tabs[active_index], &services, cx).await
                            {
                                Ok(()) => {
                                    let conflict = tab_state(&tabs[active_index], cx).conflict;
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
                            if tabs[active_index]
                                .multi_buffer
                                .as_ref()
                                .is_some_and(|multi_buffer| multi_buffer.pending_rename.is_some())
                            {
                                message = Some(
                                    "rename preview cannot be saved; Enter accepts or Esc rejects"
                                        .to_owned(),
                                );
                                continue;
                            }
                            let state = tab_state(&tabs[active_index], cx);
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
                                match save_tab(&tabs[active_index], &services, cx).await
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
                            &services,
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
                                            tab.multi_buffer.is_none()
                                                && tab.document.buffer == document.buffer
                                        }) {
                                            active_index = index;
                                            quick_open_prompt = None;
                                            message = Some(format!("already open {label}"));
                                        } else {
                                            match create_document_tab(
                                                document,
                                                &services,
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
                                let open_buffers = project_searchable_buffers(&tabs);
                                if let Err(error) = begin_project_search(
                                    project_search_prompt
                                        .as_mut()
                                        .expect("Project search prompt checked above"),
                                    &mut project_search_coordinator,
                                    ProjectSearchChange::Key,
                                    repository,
                                    &services,
                                    open_buffers,
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
                                    .position(|tab| {
                                        tab.multi_buffer.is_none()
                                            && tab.document.buffer == hit.buffer
                                    });
                                if let Some(index) = already_open {
                                    active_index = index;
                                } else {
                                    let document = OpenDocument {
                                        buffer: hit.buffer.clone(),
                                        untitled_label: None,
                                        project_searchable: true,
                                    };
                                    match create_document_tab(
                                        document,
                                        &services,
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
                    TerminalEvent::Key(event)
                        if input::is_completion(&event)
                            && save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
                            && active_search.is_none() =>
                    {
                        quit_armed = false;
                        let context = action_context(
                            repository.as_ref(),
                            &tabs[active_index].document,
                            &services,
                            &navigation_history,
                            cx,
                        );
                        if !context.has_language_server {
                            message = Some(language_service_unavailable_message("completion"));
                            continue;
                        }
                        let buffer_id = tabs[active_index]
                            .document
                            .buffer
                            .read_with(cx, |buffer, _| buffer.remote_id().to_proto());
                        let generation = tabs[active_index]
                            .completion_generation
                            .load(AtomicOrdering::SeqCst)
                            .saturating_add(1);
                        completion_prompt =
                            Some(CompletionPrompt::running(buffer_id, generation));
                        if let Err(error) = editor_window.update(cx, |editor, window, cx| {
                            editor.show_completions(&ShowCompletions, window, cx);
                        }) {
                            completion_prompt = None;
                            message = Some(format!("completion failed: {error}"));
                        } else {
                            message = None;
                        }
                    }
                    TerminalEvent::Key(event)
                        if input::is_hover(&event)
                            && save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
                            && active_search.is_none() =>
                    {
                        quit_armed = false;
                        let context = action_context(
                            repository.as_ref(),
                            &tabs[active_index].document,
                            &services,
                            &navigation_history,
                            cx,
                        );
                        if !context.has_language_server {
                            message = Some(language_service_unavailable_message("hover"));
                            continue;
                        }
                        language_request_generation =
                            language_request_generation.wrapping_add(1).max(1);
                        match start_hover_request(
                            &editor_window,
                            &services.project,
                            language_request_generation,
                            redraw_sender.clone(),
                            cx,
                        ) {
                            Ok(buffer_id) => {
                                language_overlay = Some(LanguageOverlay::Hover(
                                    HoverPrompt::running(
                                        buffer_id,
                                        language_request_generation,
                                    ),
                                ));
                                message = None;
                            }
                            Err(error) => {
                                message = Some(format!("hover failed: {error:#}"));
                            }
                        }
                    }
                    TerminalEvent::Key(event)
                        if input::is_project_diagnostics(&event)
                            && save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
                            && active_search.is_none() =>
                    {
                        quit_armed = false;
                        language_request_generation =
                            language_request_generation.wrapping_add(1).max(1);
                        let generation = language_request_generation;
                        language_overlay = Some(LanguageOverlay::Diagnostics(
                            DiagnosticsPrompt::running(generation),
                        ));
                        message = None;
                        let project = services.project.clone();
                        let root = repository
                            .as_ref()
                            .map(|repository| repository.root.canonical_path().to_path_buf());
                            let open_buffers = open_file_buffers(&tabs, cx);
                        let sender = redraw_sender.clone();
                        cx.spawn(async move |cx| {
                            let result = collect_project_diagnostics(
                                project,
                                root,
                                open_buffers,
                                cx,
                            )
                            .await
                            .map_err(|error| format!("{error:#}"));
                            let _ = sender
                                .send(TerminalEvent::DiagnosticsFinished {
                                    generation,
                                    result,
                                })
                                .await;
                        })
                        .detach();
                    }
                    TerminalEvent::Key(event)
                        if (input::is_go_to_definition(&event)
                            || input::is_go_to_type_definition(&event)
                            || input::is_find_references(&event))
                            && save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
                            && active_search.is_none() =>
                    {
                        quit_armed = false;
                        let context = action_context(
                            repository.as_ref(),
                            &tabs[active_index].document,
                            &services,
                            &navigation_history,
                            cx,
                        );
                        if !context.has_language_server {
                            message = Some(language_service_unavailable_message(
                                "semantic navigation",
                            ));
                            continue;
                        }
                        let kind = if input::is_go_to_type_definition(&event) {
                            LocationRequestKind::TypeDefinition
                        } else if input::is_find_references(&event) {
                            LocationRequestKind::References
                        } else {
                            LocationRequestKind::Definition
                        };
                        language_request_generation =
                            language_request_generation.wrapping_add(1).max(1);
                        let root = repository
                            .as_ref()
                            .map(|repository| repository.root.canonical_path().to_path_buf());
                        match start_locations_request(
                            kind,
                            &editor_window,
                            &services.project,
                            language_request_generation,
                            root,
                            redraw_sender.clone(),
                            cx,
                        ) {
                            Ok(buffer_id) => {
                                language_overlay = Some(LanguageOverlay::Locations(
                                    LocationsPrompt::running(
                                        buffer_id,
                                        language_request_generation,
                                        kind,
                                    ),
                                ));
                                message = None;
                            }
                            Err(error) => {
                                message = Some(format!(
                                    "{} failed: {error:#}",
                                    kind.title().to_lowercase()
                                ));
                            }
                        }
                    }
                    TerminalEvent::Key(event)
                        if input::is_project_symbols(&event)
                            && save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
                            && active_search.is_none() =>
                    {
                        quit_armed = false;
                        let context = action_context(
                            repository.as_ref(),
                            &tabs[active_index].document,
                            &services,
                            &navigation_history,
                            cx,
                        );
                        if !context.has_repository || !context.has_language_server {
                            message = Some(
                                "project symbols unavailable: repository or language server is not ready"
                                    .to_owned(),
                            );
                            continue;
                        }
                        let (_, _, buffer_id) = match active_editor_buffer_point(
                            &editor_window,
                            cx,
                        ) {
                            Ok(location) => location,
                            Err(error) => {
                                message = Some(format!("project symbols failed: {error:#}"));
                                continue;
                            }
                        };
                        language_request_generation =
                            language_request_generation.wrapping_add(1).max(1);
                        let generation = language_request_generation;
                        language_overlay = Some(LanguageOverlay::Locations(
                            LocationsPrompt::running(
                                buffer_id,
                                generation,
                                LocationRequestKind::ProjectSymbols,
                            ),
                        ));
                        let root = repository
                            .as_ref()
                            .map(|repository| repository.root.canonical_path().to_path_buf());
                        start_project_symbols_request(
                            &services.project,
                            buffer_id,
                            generation,
                            String::new(),
                            root,
                            redraw_sender.clone(),
                            cx,
                        );
                        message = None;
                    }
                    TerminalEvent::Key(event)
                        if input::is_rename_symbol(&event)
                            && save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
                            && active_search.is_none() =>
                    {
                        quit_armed = false;
                        let context = action_context(
                            repository.as_ref(),
                            &tabs[active_index].document,
                            &services,
                            &navigation_history,
                            cx,
                        );
                        if !context.has_language_server {
                            message = Some(language_service_unavailable_message("rename"));
                            continue;
                        }
                        language_request_generation =
                            language_request_generation.wrapping_add(1).max(1);
                        match start_rename_request(
                            &editor_window,
                            &services.project,
                            language_request_generation,
                            redraw_sender.clone(),
                            cx,
                        ) {
                            Ok((buffer, point, buffer_id)) => {
                                language_overlay = Some(LanguageOverlay::Rename(
                                    RenamePrompt::running(
                                        buffer,
                                        buffer_id,
                                        language_request_generation,
                                        point,
                                    ),
                                ));
                                message = None;
                            }
                            Err(error) => {
                                message = Some(format!("rename failed: {error:#}"));
                            }
                        }
                    }
                    TerminalEvent::Key(event)
                        if input::is_code_actions(&event)
                            && save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
                            && active_search.is_none() =>
                    {
                        quit_armed = false;
                        let context = action_context(
                            repository.as_ref(),
                            &tabs[active_index].document,
                            &services,
                            &navigation_history,
                            cx,
                        );
                        if !context.has_language_server {
                            message = Some(language_service_unavailable_message("code actions"));
                            continue;
                        }
                        language_request_generation =
                            language_request_generation.wrapping_add(1).max(1);
                        match start_code_actions_request(
                            &editor_window,
                            &services.project,
                            language_request_generation,
                            redraw_sender.clone(),
                            cx,
                        ) {
                            Ok((buffer, buffer_id)) => {
                                language_overlay = Some(LanguageOverlay::CodeActions(
                                    CodeActionsPrompt::running(
                                        buffer,
                                        buffer_id,
                                        language_request_generation,
                                    ),
                                ));
                                message = None;
                            }
                            Err(error) => {
                                message = Some(format!("code actions failed: {error:#}"));
                            }
                        }
                    }
                    TerminalEvent::Key(event)
                        if (input::is_format_document(&event)
                            || input::is_format_selection(&event))
                            && save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
                            && active_search.is_none() =>
                    {
                        quit_armed = false;
                        let selection_only = input::is_format_selection(&event);
                        let context = action_context(
                            repository.as_ref(),
                            &tabs[active_index].document,
                            &services,
                            &navigation_history,
                            cx,
                        );
                        if !context.has_language_server {
                            message = Some(language_service_unavailable_message("format"));
                            continue;
                        }
                        let task = match format_active_editor(
                            &editor_window,
                            &services.project,
                            selection_only,
                            cx,
                        ) {
                            Ok(task) => task,
                            Err(error) => {
                                message = Some(format!("format failed: {error:#}"));
                                continue;
                            }
                        };
                        match task.await {
                            Ok(transaction) => {
                                let buffer_count = project_edit_history.push(transaction);
                                let scope = if selection_only { "selection" } else { "document" };
                                message = Some(format!(
                                    "formatted {scope} in {buffer_count} buffer(s)"
                                ));
                            }
                            Err(error) => {
                                message = Some(format!("format failed: {error:#}"));
                            }
                        }
                    }
                    TerminalEvent::Key(event)
                        if input::is_undo(&event)
                            && save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
                            && active_search.is_none() =>
                    {
                        quit_armed = false;
                        if project_edit_history.can_undo() {
                            match project_edit_history.undo_latest(cx) {
                                Ok(buffer_count) => {
                                    message = Some(format!(
                                        "undid project edit in {buffer_count} buffer(s)"
                                    ));
                                }
                                Err(error) => {
                                    message = Some(format!("project undo failed: {error:#}"));
                                }
                            }
                        } else if let Err(error) = editor_window.update(cx, |editor, window, cx| {
                            editor.undo(&Undo, window, cx)
                        }) {
                            message = Some(format!("undo failed: {error}"));
                        } else {
                            message = None;
                        }
                    }
                    TerminalEvent::Key(event)
                        if input::is_redo(&event)
                            && save_as_prompt.is_none()
                            && open_prompt.is_none()
                            && go_to_line_prompt.is_none()
                            && quick_open_prompt.is_none()
                            && project_search_prompt.is_none()
                            && active_search.is_none() =>
                    {
                        quit_armed = false;
                        if project_edit_history.can_redo() {
                            match project_edit_history.redo_latest(cx) {
                                Ok(buffer_count) => {
                                    message = Some(format!(
                                        "redid project edit in {buffer_count} buffer(s)"
                                    ));
                                }
                                Err(error) => {
                                    message = Some(format!("project redo failed: {error:#}"));
                                }
                            }
                        } else if let Err(error) = editor_window.update(cx, |editor, window, cx| {
                            editor.redo(&Redo, window, cx)
                        }) {
                            message = Some(format!("redo failed: {error}"));
                        } else {
                            message = None;
                        }
                    }
                    TerminalEvent::Key(event)
                        if input::is_navigation_back(&event)
                            || input::is_navigation_forward(&event) =>
                    {
                        quit_armed = false;
                        let current = match current_navigation_point(&tabs, active_index, cx) {
                            Ok(current) => current,
                            Err(error) => {
                                message = Some(format!("navigation failed: {error:#}"));
                                continue;
                            }
                        };
                        let previous_history = navigation_history.clone();
                        let target = if input::is_navigation_back(&event) {
                            navigation_history.go_back(current)
                        } else {
                            navigation_history.go_forward(current)
                        };
                        let Some(target) = target else {
                            message = Some(if input::is_navigation_back(&event) {
                                "no previous location".to_owned()
                            } else {
                                "no next location".to_owned()
                            });
                            continue;
                        };
                        match navigate_to_history_point(
                            &target,
                            repository.as_ref(),
                            &services,
                            &mut tabs,
                            &mut active_index,
                            redraw_sender.clone(),
                            cx,
                        )
                        .await
                        {
                            Ok(status) => message = Some(status),
                            Err(error) => {
                                navigation_history = previous_history;
                                message = Some(format!("navigation failed: {error:#}"));
                            }
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
                                            .position(|tab| {
                                                tab.multi_buffer.is_none()
                                                    && tab.document.buffer == document.buffer
                                            })
                                        {
                                            active_index = index;
                                            open_prompt = None;
                                            message = Some("already open".to_owned());
                                        } else {
                                            match create_document_tab(
                                                document,
                                                &services,
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
                            if let Err(error) = cx.update_window(input_window, |_root, window, cx| {
                                window.activate_window();
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
                            let open_buffers = project_searchable_buffers(&tabs);
                            if let Err(error) = begin_project_search(
                                project_search_prompt
                                    .as_mut()
                                    .expect("Project search prompt checked above"),
                                &mut project_search_coordinator,
                                ProjectSearchChange::Paste,
                                repository,
                                &services,
                                open_buffers,
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
                        let open_buffers = project_searchable_buffers(&tabs);
                        if let Err(error) = dispatch_project_search(
                            &mut project_search_coordinator,
                            next,
                            repository,
                            &services,
                            open_buffers,
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
                            let open_buffers = project_searchable_buffers(&tabs);
                            if let Err(error) = dispatch_project_search(
                                &mut project_search_coordinator,
                                next,
                                repository,
                                &services,
                                open_buffers,
                                redraw_sender.clone(),
                                cx,
                            ) {
                                message = Some(format!("project search failed: {error:#}"));
                            }
                        }
                    }
                    TerminalEvent::CompletionFinished {
                        buffer_id,
                        generation,
                        menu_wait_attempt,
                        result,
                    } => {
                        let prompt_is_current = completion_prompt.as_ref().is_some_and(|prompt| {
                            prompt.buffer_id == buffer_id && prompt.generation == generation
                        });
                        if !prompt_is_current {
                            continue;
                        }
                        let menu_ready = if result.is_err() {
                            true
                        } else {
                            match editor_window.update(cx, |editor, _window, _cx| {
                                editor.has_visible_completions_menu()
                            }) {
                                Ok(menu_ready) => menu_ready,
                                Err(error) => {
                                    failure = Some(format!(
                                        "failed to inspect Zed completion menu readiness: {error}"
                                    ));
                                    break;
                                }
                            }
                        };
                        if !menu_ready && menu_wait_attempt < 500 {
                            let sender = redraw_sender.clone();
                            cx.spawn(async move |cx| {
                                cx.background_executor()
                                    .timer(Duration::from_millis(2))
                                    .await;
                                let _ = sender
                                    .send(TerminalEvent::CompletionFinished {
                                        buffer_id,
                                        generation,
                                        menu_wait_attempt: menu_wait_attempt + 1,
                                        result,
                                    })
                                    .await;
                            })
                            .detach();
                            continue;
                        }
                        if let Some(prompt) = completion_prompt.as_mut()
                            && prompt.complete(
                                buffer_id,
                                generation,
                                if menu_ready {
                                    result
                                } else {
                                    Err("Zed completion menu did not become ready within one second"
                                        .to_owned())
                                },
                            )
                        {
                            quit_armed = false;
                            close_armed = false;
                            reload_armed = false;
                            save_conflict_armed = false;
                            message = None;
                        }
                    }
                    TerminalEvent::HoverFinished {
                        buffer_id,
                        generation,
                        result,
                    } => {
                        if let Some(LanguageOverlay::Hover(prompt)) =
                            language_overlay.as_mut()
                            && prompt.complete(buffer_id, generation, result)
                        {
                            quit_armed = false;
                            close_armed = false;
                            reload_armed = false;
                            save_conflict_armed = false;
                            message = None;
                        }
                    }
                    TerminalEvent::DiagnosticsFinished { generation, result } => {
                        if let Some(LanguageOverlay::Diagnostics(prompt)) =
                            language_overlay.as_mut()
                            && prompt.complete(generation, result)
                        {
                            quit_armed = false;
                            close_armed = false;
                            reload_armed = false;
                            save_conflict_armed = false;
                            message = None;
                        }
                    }
                    TerminalEvent::LocationsFinished {
                        buffer_id,
                        generation,
                        kind,
                        result,
                        } => {
                            let mut accepted = false;
                            let mut automatic_target = None;
                            let mut multibuffer_targets = None;
                            if let Some(LanguageOverlay::Locations(prompt)) =
                                language_overlay.as_mut()
                            {
                                accepted = prompt.complete(buffer_id, generation, kind, result);
                            if accepted
                                && matches!(
                                    kind,
                                    LocationRequestKind::Definition
                                        | LocationRequestKind::TypeDefinition
                                )
                            {
                                    automatic_target = match &prompt.state {
                                    LocationsPromptState::Ready { items, .. }
                                        if items.len() == 1 => items.first().cloned(),
                                        _ => None,
                                    };
                                }
                                if accepted {
                                    multibuffer_targets = match &prompt.state {
                                        LocationsPromptState::Ready { items, .. }
                                            if !items.is_empty()
                                                && (kind == LocationRequestKind::References
                                                    || (matches!(
                                                        kind,
                                                        LocationRequestKind::Definition
                                                            | LocationRequestKind::TypeDefinition
                                                    ) && items.len() > 1)) =>
                                        {
                                            Some(items.clone())
                                        }
                                        _ => None,
                                    };
                                }
                            }
                        if accepted {
                            quit_armed = false;
                            close_armed = false;
                            reload_armed = false;
                            save_conflict_armed = false;
                                message = None;
                            }
                            if let Some(items) = multibuffer_targets {
                                match create_locations_multibuffer_tab(
                                    kind.title().to_owned(),
                                    &items,
                                    repository.as_ref(),
                                    &services,
                                    redraw_sender.clone(),
                                    cx,
                                )
                                .await
                                {
                                    Ok(tab) => {
                                        tabs.push(tab);
                                        active_index = tabs.len().saturating_sub(1);
                                        language_overlay = None;
                                        message = Some(format!(
                                            "opened {} in an editable MultiBuffer ({} target(s))",
                                            kind.title().to_lowercase(),
                                            items.len()
                                        ));
                                    }
                                    Err(error) => {
                                        if let Some(LanguageOverlay::Locations(prompt)) =
                                            language_overlay.as_mut()
                                        {
                                            prompt.feedback = Some(format!(
                                                "MultiBuffer open failed: {error:#}"
                                            ));
                                        }
                                    }
                                }
                                continue;
                            }
                            if let Some(item) = automatic_target {
                            let origin = current_navigation_point(&tabs, active_index, cx).ok();
                            match navigate_to_location(
                                &item,
                                repository.as_ref(),
                                &services,
                                &mut tabs,
                                &mut active_index,
                                redraw_sender.clone(),
                                cx,
                            )
                            .await
                            {
                                Ok(opened) => {
                                    if let Some(origin) = origin {
                                        navigation_history.record_jump(origin);
                                    }
                                    language_overlay = None;
                                    message = Some(opened);
                                }
                                Err(error) => {
                                    if let Some(LanguageOverlay::Locations(prompt)) =
                                        language_overlay.as_mut()
                                    {
                                        prompt.feedback =
                                            Some(format!("open failed: {error:#}"));
                                    }
                                }
                            }
                        }
                    }
                    TerminalEvent::RenamePrepared {
                        buffer_id,
                        generation,
                        result,
                    } => {
                        if let Some(LanguageOverlay::Rename(prompt)) =
                            language_overlay.as_mut()
                            && prompt.complete(buffer_id, generation, result)
                        {
                            quit_armed = false;
                            close_armed = false;
                            reload_armed = false;
                            save_conflict_armed = false;
                            message = None;
                        }
                    }
                    TerminalEvent::RenamePreviewFinished {
                        buffer_id,
                        generation,
                        new_name,
                        result,
                    } => {
                        let origin = if let Some(LanguageOverlay::Rename(prompt)) =
                            language_overlay.as_mut()
                            && prompt.finish_preview(
                                buffer_id,
                                generation,
                                &new_name,
                                &result,
                            )
                        {
                            Some((prompt.buffer.clone(), prompt.point))
                        } else {
                            None
                        };
                        let Some((origin_buffer, origin_point)) = origin else {
                            continue;
                        };
                        let Ok((language_server_id, edit)) = result else {
                            message = None;
                            continue;
                        };
                        let preview = rename_workspace_root(
                            repository.as_ref(),
                            &origin_buffer,
                            cx,
                        )
                        .and_then(|workspace_root| {
                            Ok((
                                workspace_root.clone(),
                                create_rename_preview_tab(
                                    origin_buffer,
                                    origin_point,
                                    new_name.clone(),
                                    language_server_id,
                                    edit,
                                    workspace_root,
                                    repository.as_ref(),
                                    &services,
                                    cx,
                                ),
                            ))
                        });
                        let preview = match preview {
                            Ok((_, preview)) => preview.await,
                            Err(error) => Err(error),
                        };
                        match preview {
                            Ok(tab) => {
                                let (edit_count, file_operation_count) = tab
                                    .multi_buffer
                                    .as_ref()
                                    .and_then(|multi_buffer| multi_buffer.pending_rename.as_ref())
                                    .map(|pending| {
                                        (
                                            pending.plan.edit_count,
                                            pending.plan.file_operation_count,
                                        )
                                    })
                                    .unwrap_or_default();
                                tabs.push(tab);
                                active_index = tabs.len().saturating_sub(1);
                                language_overlay = None;
                                quit_armed = false;
                                close_armed = false;
                                reload_armed = false;
                                save_conflict_armed = false;
                                message = Some(format!(
                                    "rename preview: {edit_count} edit(s), {file_operation_count} file op(s); Enter accepts, Esc rejects"
                                ));
                            }
                            Err(error) => {
                                let failure = Err(format!("{error:#}"));
                                if let Some(LanguageOverlay::Rename(prompt)) =
                                    language_overlay.as_mut()
                                {
                                    let _ = prompt.finish_preview(
                                        buffer_id,
                                        generation,
                                        &new_name,
                                        &failure,
                                    );
                                }
                                message = None;
                            }
                        }
                    }
                    TerminalEvent::CodeActionsFinished {
                        buffer_id,
                        generation,
                        result,
                    } => {
                        if let Some(LanguageOverlay::CodeActions(prompt)) =
                            language_overlay.as_mut()
                            && prompt.complete(buffer_id, generation, result)
                        {
                            quit_armed = false;
                            close_armed = false;
                            reload_armed = false;
                            save_conflict_armed = false;
                            message = None;
                        }
                    }
                    TerminalEvent::WorktreeTrustRequired { worktree_id, path } => {
                        quit_armed = false;
                        close_armed = false;
                        reload_armed = false;
                        save_conflict_armed = false;
                        completion_prompt = None;
                        command_palette_prompt = None;
                        language_overlay = Some(LanguageOverlay::Trust(WorktreeTrustPrompt {
                            worktree_id,
                            path,
                        }));
                        message = None;
                    }
                    TerminalEvent::ConfigurationReloaded { kind, result } => {
                        quit_armed = false;
                        close_armed = false;
                        reload_armed = false;
                        save_conflict_armed = false;
                        message = Some(match result {
                            Ok(status) => format!("{kind} {status}"),
                            Err(error) => format!("{kind} reload failed: {error}"),
                        });
                    }
                    TerminalEvent::LanguageServiceNotice { level, message: notice } => {
                        quit_armed = false;
                        close_armed = false;
                        reload_armed = false;
                        save_conflict_armed = false;
                        message = Some(format!("language service {level}: {notice}"));
                    }
                    TerminalEvent::ReloadFinished { buffer_id, result } => {
                        let Some(reloaded_index) = tabs.iter().position(|tab| {
                            let primary_matches = tab.document.buffer.read_with(cx, |buffer, _| {
                                buffer.remote_id().to_proto() == buffer_id
                            });
                            primary_matches
                                || tab.multi_buffer.as_ref().is_some_and(|multi_buffer| {
                                    multi_buffer.source_buffers.iter().any(|buffer| {
                                        buffer.read_with(cx, |buffer, _| {
                                            buffer.remote_id().to_proto() == buffer_id
                                        })
                                    })
                                })
                        }) else {
                            continue;
                        };

                        // An asynchronous status update must never leave a hidden
                        // destructive-action confirmation armed behind its message.
                        quit_armed = false;
                        close_armed = false;
                        reload_armed = false;
                        save_conflict_armed = false;
                        let label = tabs[reloaded_index].multi_buffer.as_ref().map_or_else(
                            || document_label(&tabs[reloaded_index].document, true, cx),
                            |multi_buffer| multi_buffer.title.clone(),
                        );
                        let state = tab_state(&tabs[reloaded_index], cx);

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
                    TerminalEvent::Action(_) => unreachable!("actions are normalized before dispatch"),
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
    project: Entity<Project>,
    buffer_store: Entity<BufferStore>,
    worktree_store: Entity<WorktreeStore>,
    _lsp_store: Entity<project::lsp_store::LspStore>,
    file_system: Arc<dyn Fs>,
    language_registry: Arc<LanguageRegistry>,
}

struct OpenDocument {
    buffer: Entity<Buffer>,
    untitled_label: Option<String>,
    /// Mirrors Zed's project-search eligibility for documents owned by zec.
    ///
    /// Scratch buffers stay false even after Save As makes them file-backed.
    project_searchable: bool,
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
    multi_buffer: Option<MultiBufferTab>,
    editor_window: WindowHandle<Editor>,
    completion_generation: Arc<AtomicU64>,
    viewport: Viewport,
    manual_vertical_scroll: bool,
    last_cursor: Option<Cursor>,
}

struct MultiBufferTab {
    title: String,
    buffer: Entity<MultiBuffer>,
    source_buffers: Vec<Entity<Buffer>>,
    pending_rename: Option<PendingRename>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RenamePreviewEdit {
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
    new_text: String,
    annotation_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RenamePreviewOperation {
    Text {
        path: PathBuf,
        version: Option<i32>,
        edits: Vec<RenamePreviewEdit>,
    },
    Create {
        path: PathBuf,
        overwrite: bool,
        ignore_if_exists: bool,
        annotation_id: Option<String>,
    },
    Rename {
        old_path: PathBuf,
        new_path: PathBuf,
        overwrite: bool,
        ignore_if_exists: bool,
        annotation_id: Option<String>,
    },
    Delete {
        path: PathBuf,
        recursive: bool,
        ignore_if_not_exists: bool,
        annotation_id: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RenamePreviewAnnotation {
    label: String,
    needs_confirmation: bool,
    description: Option<String>,
}

#[derive(Clone, Debug)]
struct RenamePreviewPlan {
    operations: Vec<RenamePreviewOperation>,
    annotations: BTreeMap<String, RenamePreviewAnnotation>,
    signature: String,
    edit_count: usize,
    file_operation_count: usize,
}

#[derive(Clone, Debug)]
struct RenameBufferGuard {
    path: PathBuf,
    buffer: Entity<Buffer>,
    text_hash: String,
}

#[derive(Clone, Debug)]
struct RenamePathGuard {
    path: PathBuf,
    fingerprint: String,
}

#[derive(Clone, Debug)]
struct PendingRename {
    origin_buffer: Entity<Buffer>,
    origin_point: language::Point,
    new_name: String,
    language_server_id: lsp::LanguageServerId,
    workspace_root: PathBuf,
    plan: RenamePreviewPlan,
    buffer_guards: Vec<RenameBufferGuard>,
    path_guards: Vec<RenamePathGuard>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NavigationPoint {
    buffer_id: u64,
    path: Option<PathBuf>,
    row: u32,
    column: u32,
    viewport: Viewport,
}

#[derive(Clone, Debug, Default)]
struct NavigationHistory {
    back: Vec<NavigationPoint>,
    forward: Vec<NavigationPoint>,
}

#[derive(Debug, Default)]
struct ProjectEditHistory {
    undo: Vec<ProjectTransaction>,
    redo: Vec<ProjectTransaction>,
}

impl ProjectEditHistory {
    fn push(&mut self, transaction: ProjectTransaction) -> usize {
        let buffer_count = transaction.0.len();
        if buffer_count > 0 {
            if self.undo.len() == 100 {
                self.undo.remove(0);
            }
            self.undo.push(transaction);
            self.redo.clear();
        }
        buffer_count
    }

    fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    fn undo_latest(&mut self, cx: &mut gpui::AsyncApp) -> Result<usize> {
        let transaction = self
            .undo
            .last()
            .cloned()
            .context("no project edit transaction to undo")?;
        ensure!(
            transaction.0.iter().all(|(buffer, edit)| {
                buffer.read_with(cx, |buffer, _| buffer.get_transaction(edit.id).is_some())
            }),
            "one or more project edit transactions are no longer in buffer history"
        );
        for (buffer, edit) in &transaction.0 {
            ensure!(
                buffer.update(cx, |buffer, cx| buffer.undo_transaction(edit.id, cx)),
                "failed to undo project edit for buffer {:?}",
                buffer.entity_id()
            );
        }
        let buffer_count = transaction.0.len();
        self.undo.pop();
        self.redo.push(transaction);
        Ok(buffer_count)
    }

    fn redo_latest(&mut self, cx: &mut gpui::AsyncApp) -> Result<usize> {
        let transaction = self
            .redo
            .last()
            .cloned()
            .context("no project edit transaction to redo")?;
        for (buffer, edit) in &transaction.0 {
            ensure!(
                buffer.update(cx, |buffer, cx| buffer.redo_to_transaction(edit.id, cx)),
                "failed to redo project edit for buffer {:?}",
                buffer.entity_id()
            );
        }
        let buffer_count = transaction.0.len();
        self.redo.pop();
        if self.undo.len() == 100 {
            self.undo.remove(0);
        }
        self.undo.push(transaction);
        Ok(buffer_count)
    }
}

impl NavigationHistory {
    const LIMIT: usize = 100;

    fn record_jump(&mut self, origin: NavigationPoint) {
        if self.back.last() != Some(&origin) {
            self.back.push(origin);
            if self.back.len() > Self::LIMIT {
                self.back.remove(0);
            }
        }
        self.forward.clear();
    }

    fn go_back(&mut self, current: NavigationPoint) -> Option<NavigationPoint> {
        let target = self.back.pop()?;
        if self.forward.last() != Some(&current) {
            self.forward.push(current);
        }
        Some(target)
    }

    fn go_forward(&mut self, current: NavigationPoint) -> Option<NavigationPoint> {
        let target = self.forward.pop()?;
        if self.back.last() != Some(&current) {
            self.back.push(current);
        }
        Some(target)
    }

    fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

fn current_navigation_point(
    tabs: &[DocumentTab],
    active_index: usize,
    cx: &mut gpui::AsyncApp,
) -> Result<NavigationPoint> {
    let tab = tabs.get(active_index).context("active tab is missing")?;
    let (buffer, point, buffer_id) = active_editor_buffer_point(&tab.editor_window, cx)?;
    let path = buffer.read_with(cx, |buffer, cx| {
        buffer.file().map(|file| {
            file.as_local()
                .map(|file| file.abs_path(cx))
                .unwrap_or_else(|| file.full_path(cx))
        })
    });
    Ok(NavigationPoint {
        buffer_id,
        path,
        row: point.row,
        column: point.column,
        viewport: tab.viewport,
    })
}

async fn navigate_to_history_point(
    point: &NavigationPoint,
    repository: Option<&RepositorySession>,
    services: &FileServices,
    tabs: &mut Vec<DocumentTab>,
    active_index: &mut usize,
    redraw_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<String> {
    if let Some(index) = tabs.iter().position(|tab| {
        tab.multi_buffer.is_none()
            && tab
                .document
                .buffer
                .read_with(cx, |buffer, _| buffer.remote_id().to_proto())
                == point.buffer_id
    }) {
        *active_index = index;
        move_caret_to_text_position(
            &tabs[index].editor_window,
            TextPosition {
                row: usize::try_from(point.row).unwrap_or(usize::MAX),
                byte_column: usize::try_from(point.column).unwrap_or(usize::MAX),
            },
            cx,
        )?;
    } else {
        let path = point
            .path
            .as_deref()
            .context("navigation target buffer is closed and has no path")?;
        navigate_to_path_position(
            path,
            &path.display().to_string(),
            point.row,
            point.column,
            repository,
            services,
            tabs,
            active_index,
            redraw_sender,
            cx,
        )
        .await?;
    }

    let cursor = tabs[*active_index]
        .editor_window
        .update(cx, |editor, _window, cx| {
            let display = editor.display_snapshot(cx);
            display_cursor_at(&display, editor.selections.newest_display(&display).head())
        })?;
    tabs[*active_index].viewport = point.viewport;
    tabs[*active_index].manual_vertical_scroll = true;
    tabs[*active_index].last_cursor = Some(cursor);
    Ok(format!(
        "navigated to {}:{}:{}",
        point
            .path
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "untitled buffer".to_owned()),
        point.row.saturating_add(1),
        point.column.saturating_add(1)
    ))
}

struct CapturedEditorFrame {
    snapshot: RenderSnapshot,
    manual_vertical_scroll: bool,
    last_cursor: Option<Cursor>,
}

fn project_searchable_buffers(tabs: &[DocumentTab]) -> Vec<Entity<Buffer>> {
    let mut seen = HashSet::new();
    tabs.iter()
        .filter(|tab| tab.document.project_searchable)
        .flat_map(|tab| {
            tab.multi_buffer.as_ref().map_or_else(
                || vec![tab.document.buffer.clone()],
                |multi_buffer| multi_buffer.source_buffers.clone(),
            )
        })
        .filter(|buffer| seen.insert(buffer.entity_id()))
        .collect()
}

fn open_file_buffers(tabs: &[DocumentTab], cx: &gpui::AsyncApp) -> Vec<(PathBuf, Entity<Buffer>)> {
    let mut seen = HashSet::new();
    tabs.iter()
        .flat_map(|tab| {
            tab.multi_buffer.as_ref().map_or_else(
                || vec![tab.document.buffer.clone()],
                |multi_buffer| multi_buffer.source_buffers.clone(),
            )
        })
        .filter(|buffer| seen.insert(buffer.entity_id()))
        .filter_map(|buffer| {
            let path = buffer.read_with(cx, |buffer, cx| {
                buffer.file().map(|file| {
                    file.as_local()
                        .map(|file| file.abs_path(cx))
                        .unwrap_or_else(|| file.full_path(cx))
                })
            })?;
            Some((path, buffer))
        })
        .collect()
}

fn action_context(
    repository: Option<&RepositorySession>,
    document: &OpenDocument,
    services: &FileServices,
    navigation_history: &NavigationHistory,
    cx: &gpui::AsyncApp,
) -> ActionContext {
    let adapter_names = document.buffer.read_with(cx, |buffer, _| {
        buffer
            .language()
            .map(|language| {
                services
                    .language_registry
                    .lsp_adapters(&language.name())
                    .into_iter()
                    .map(|adapter| adapter.name().to_string())
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default()
    });
    let has_language_server = services.project.read_with(cx, |project, cx| {
        project.language_server_statuses(cx).any(|(_, status)| {
            status.process_id.is_some() && adapter_names.contains(&status.name.to_string())
        })
    });
    ActionContext {
        has_repository: repository.is_some(),
        has_file: document_state(document, cx).has_file(),
        has_language_server,
        can_navigate_back: navigation_history.can_go_back(),
        can_navigate_forward: navigation_history.can_go_forward(),
    }
}

fn active_editor_buffer_point(
    editor_window: &WindowHandle<Editor>,
    cx: &mut gpui::AsyncApp,
) -> Result<(Entity<Buffer>, language::Point, u64)> {
    editor_window.update(cx, |editor, _window, cx| -> Result<_> {
        let display = editor.display_snapshot(cx);
        let head = editor
            .selections
            .newest::<MultiBufferOffset>(&display)
            .head();
        let (buffer, point) = editor
            .buffer()
            .read(cx)
            .point_to_buffer_point(head, cx)
            .context("cursor does not belong to an editor buffer")?;
        let buffer_id = buffer.read(cx).remote_id().to_proto();
        Ok((buffer, point, buffer_id))
    })?
}

fn start_hover_request(
    editor_window: &WindowHandle<Editor>,
    project: &Entity<Project>,
    generation: u64,
    event_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<u64> {
    let (buffer, point, buffer_id) = active_editor_buffer_point(editor_window, cx)?;
    let request = project.update(cx, |project, cx| project.hover(&buffer, point, cx));
    cx.spawn(async move |_cx| {
        let items = request
            .await
            .unwrap_or_default()
            .into_iter()
            .flat_map(|hover| hover.contents)
            .take(MAX_LANGUAGE_RESPONSE_ITEMS)
            .map(|block| {
                let kind = match block.kind {
                    project::HoverBlockKind::PlainText => "PlainText".to_owned(),
                    project::HoverBlockKind::Markdown => "Markdown".to_owned(),
                    project::HoverBlockKind::Code { language } => format!("Code: {language}"),
                };
                HoverPresentation {
                    kind: bounded_terminal_text(&kind),
                    text: bounded_terminal_text(&block.text),
                }
            })
            .collect();
        let _ = event_sender
            .send(TerminalEvent::HoverFinished {
                buffer_id,
                generation,
                result: Ok(items),
            })
            .await;
    })
    .detach();
    Ok(buffer_id)
}

fn rename_preparation(
    buffer: &Entity<Buffer>,
    point: language::Point,
    response: PrepareRenameResponse,
    cx: &gpui::AsyncApp,
) -> Result<RenamePreparation> {
    buffer.read_with(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        let mut range = match response {
            PrepareRenameResponse::Success(range) => {
                range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot)
            }
            PrepareRenameResponse::OnlyUnpreparedRenameSupported => {
                snapshot.surrounding_word(point, None).0
            }
            PrepareRenameResponse::InvalidPosition => {
                bail!("the language server rejected this cursor position")
            }
        };
        if range.is_empty() {
            range = snapshot.surrounding_word(point, None).0;
        }
        ensure!(
            range.start <= range.end && range.end <= snapshot.len(),
            "language server returned an invalid rename range"
        );
        let placeholder = snapshot.text_for_range(range.clone()).collect::<String>();
        ensure!(!placeholder.is_empty(), "rename target is empty");
        Ok(RenamePreparation {
            placeholder,
            start: range.start,
            end: range.end,
        })
    })
}

fn start_rename_request(
    editor_window: &WindowHandle<Editor>,
    project: &Entity<Project>,
    generation: u64,
    event_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<(Entity<Buffer>, language::Point, u64)> {
    let (buffer, point, buffer_id) = active_editor_buffer_point(editor_window, cx)?;
    let request = project.update(cx, |project, cx| {
        project.prepare_rename(buffer.clone(), point, cx)
    });
    let result_buffer = buffer.clone();
    cx.spawn(async move |cx| {
        let result = match request.await {
            Ok(response) => rename_preparation(&result_buffer, point, response, cx)
                .map_err(|error| format!("{error:#}")),
            Err(error) => Err(format!("{error:#}")),
        };
        let _ = event_sender
            .send(TerminalEvent::RenamePrepared {
                buffer_id,
                generation,
                result,
            })
            .await;
    })
    .detach();
    Ok((buffer, point, buffer_id))
}

fn request_rename_workspace_edit(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    point: language::Point,
    new_name: String,
    expected_server_id: Option<lsp::LanguageServerId>,
    cx: &mut gpui::AsyncApp,
) -> Result<(lsp::LanguageServerId, Task<Result<lsp::WorkspaceEdit>>)> {
    let (path, position) = buffer.read_with(cx, |buffer, cx| -> Result<_> {
        let path = buffer
            .file()
            .map(|file| {
                file.as_local()
                    .map(|file| file.abs_path(cx))
                    .unwrap_or_else(|| file.full_path(cx))
            })
            .context("rename requires a file-backed buffer")?;
        Ok((path, point.to_point_utf16(buffer)))
    })?;
    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    let server = lsp_store.update(cx, |lsp_store, cx| {
        if let Some(server_id) = expected_server_id {
            lsp_store
                .language_server_for_id(server_id)
                .with_context(|| {
                    format!("language server {server_id} stopped before rename acceptance")
                })
        } else {
            buffer.update(cx, |buffer, cx| {
                lsp_store
                    .running_language_servers_for_local_buffer(buffer, cx)
                    .next()
                    .map(|(_, server)| server.clone())
                    .context("no running language server serves the rename buffer")
            })
        }
    })?;
    let server_id = server.server_id();
    let params = lsp::RenameParams {
        text_document_position: lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: project::lsp_command::file_path_to_lsp_url(&path)?,
            },
            position: language::point_to_lsp(position),
        },
        new_name,
        work_done_progress_params: Default::default(),
    };
    let task = cx.background_spawn(async move {
        server
            .request::<lsp::request::Rename>(params, RENAME_PREVIEW_TIMEOUT)
            .await
            .into_response()
            .context("request rename WorkspaceEdit preview")
            .map(Option::unwrap_or_default)
    });
    Ok((server_id, task))
}

fn start_rename_preview_request(
    project: &Entity<Project>,
    buffer: Entity<Buffer>,
    point: language::Point,
    buffer_id: u64,
    generation: u64,
    new_name: String,
    event_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let (server_id, request) =
        request_rename_workspace_edit(project, &buffer, point, new_name.clone(), None, cx)?;
    cx.spawn(async move |_cx| {
        let result = request
            .await
            .map(|edit| (server_id, edit))
            .map_err(|error| format!("{error:#}"));
        let _ = event_sender
            .send(TerminalEvent::RenamePreviewFinished {
                buffer_id,
                generation,
                new_name,
                result,
            })
            .await;
        drop(buffer);
    })
    .detach();
    Ok(())
}

fn start_code_actions_request(
    editor_window: &WindowHandle<Editor>,
    project: &Entity<Project>,
    generation: u64,
    event_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<(Entity<Buffer>, u64)> {
    let (buffer, point, buffer_id) = active_editor_buffer_point(editor_window, cx)?;
    let request = project.update(cx, |project, cx| {
        project.code_actions(&buffer, point..point, None, cx)
    });
    let result_buffer = buffer.clone();
    cx.spawn(async move |_cx| {
        let result = request
            .await
            .map(|actions| actions.unwrap_or_default())
            .map_err(|error| format!("{error:#}"));
        let _ = event_sender
            .send(TerminalEvent::CodeActionsFinished {
                buffer_id,
                generation,
                result,
            })
            .await;
        drop(result_buffer);
    })
    .detach();
    Ok((buffer, buffer_id))
}

fn format_active_editor(
    editor_window: &WindowHandle<Editor>,
    project: &Entity<Project>,
    selection_only: bool,
    cx: &mut gpui::AsyncApp,
) -> Result<Task<Result<ProjectTransaction>>> {
    let (buffers, target) = editor_window.update(cx, |editor, _window, cx| -> Result<_> {
        if !selection_only {
            let buffers = editor
                .buffer()
                .read(cx)
                .all_buffers_iter()
                .collect::<Vec<_>>();
            ensure!(!buffers.is_empty(), "editor has no buffers to format");
            return Ok((buffers, LspFormatTarget::Buffers));
        }

        let display = editor.display_snapshot(cx);
        let selection = editor.selections.newest::<MultiBufferOffset>(&display);
        ensure!(!selection.is_empty(), "select a range before formatting it");
        let multi_buffer = editor.buffer().read(cx);
        let (start_buffer, start) = multi_buffer
            .point_to_buffer_point(selection.start, cx)
            .context("selection start does not belong to an editor buffer")?;
        let (end_buffer, end) = multi_buffer
            .point_to_buffer_point(selection.end, cx)
            .context("selection end does not belong to an editor buffer")?;
        ensure!(
            start_buffer == end_buffer,
            "range formatting cannot span multiple buffers"
        );
        let (buffer_id, range) = start_buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            (
                buffer.remote_id(),
                snapshot.anchor_before(start)..snapshot.anchor_after(end),
            )
        });
        let mut ranges = BTreeMap::new();
        ranges.insert(buffer_id, vec![range]);
        Ok((vec![start_buffer], LspFormatTarget::Ranges(ranges)))
    })??;
    let buffers = buffers.into_iter().collect();
    Ok(project.update(cx, |project, cx| {
        project.format(buffers, target, true, FormatTrigger::Manual, cx)
    }))
}

fn location_presentations(
    locations: Vec<language::Location>,
    root: Option<&Path>,
    cx: &gpui::AsyncApp,
) -> Vec<LocationPresentation> {
    locations
        .into_iter()
        .take(MAX_LANGUAGE_RESPONSE_ITEMS)
        .map(|location| {
            location.buffer.read_with(cx, |buffer, cx| {
                let snapshot = buffer.snapshot();
                let start = location.range.start.to_point(&snapshot);
                let end = location.range.end.to_point(&snapshot);
                let path = buffer
                    .file()
                    .map(|file| {
                        file.as_local()
                            .map(|file| file.abs_path(cx))
                            .unwrap_or_else(|| file.full_path(cx))
                    })
                    .unwrap_or_else(|| PathBuf::from("<untitled>"));
                let line_len = snapshot.line_len(start.row);
                let snippet = snapshot
                    .text_for_range(
                        language::Point::new(start.row, 0)
                            ..language::Point::new(start.row, line_len),
                    )
                    .flat_map(|chunk| chunk.chars())
                    .skip_while(|character| character.is_whitespace())
                    .take(200)
                    .collect::<String>()
                    .trim_end()
                    .to_owned();
                LocationPresentation {
                    label: bounded_terminal_text(&diagnostic_label(&path, root)),
                    path,
                    row: start.row,
                    column: start.column,
                    end_row: end.row,
                    end_column: end.column,
                    snippet,
                }
            })
        })
        .collect()
}

fn spawn_location_links_request(
    request: Task<Result<Option<Vec<project::LocationLink>>>>,
    kind: LocationRequestKind,
    buffer_id: u64,
    generation: u64,
    root: Option<PathBuf>,
    event_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) {
    cx.spawn(async move |cx| {
        let result = request
            .await
            .map(|links| {
                location_presentations(
                    links
                        .unwrap_or_default()
                        .into_iter()
                        .map(|link| link.target)
                        .collect(),
                    root.as_deref(),
                    cx,
                )
            })
            .map_err(|error| format!("{error:#}"));
        let _ = event_sender
            .send(TerminalEvent::LocationsFinished {
                buffer_id,
                generation,
                kind,
                result,
            })
            .await;
    })
    .detach();
}

fn spawn_locations_request(
    request: Task<Result<Option<Vec<language::Location>>>>,
    kind: LocationRequestKind,
    buffer_id: u64,
    generation: u64,
    root: Option<PathBuf>,
    event_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) {
    cx.spawn(async move |cx| {
        let result = request
            .await
            .map(|locations| {
                location_presentations(locations.unwrap_or_default(), root.as_deref(), cx)
            })
            .map_err(|error| format!("{error:#}"));
        let _ = event_sender
            .send(TerminalEvent::LocationsFinished {
                buffer_id,
                generation,
                kind,
                result,
            })
            .await;
    })
    .detach();
}

fn start_locations_request(
    kind: LocationRequestKind,
    editor_window: &WindowHandle<Editor>,
    project: &Entity<Project>,
    generation: u64,
    root: Option<PathBuf>,
    event_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<u64> {
    let (buffer, point, buffer_id) = active_editor_buffer_point(editor_window, cx)?;
    match kind {
        LocationRequestKind::Definition => {
            let request = project.update(cx, |project, cx| project.definitions(&buffer, point, cx));
            spawn_location_links_request(
                request,
                kind,
                buffer_id,
                generation,
                root,
                event_sender,
                cx,
            );
        }
        LocationRequestKind::TypeDefinition => {
            let request = project.update(cx, |project, cx| {
                project.type_definitions(&buffer, point, cx)
            });
            spawn_location_links_request(
                request,
                kind,
                buffer_id,
                generation,
                root,
                event_sender,
                cx,
            );
        }
        LocationRequestKind::References => {
            let request = project.update(cx, |project, cx| project.references(&buffer, point, cx));
            spawn_locations_request(request, kind, buffer_id, generation, root, event_sender, cx);
        }
        LocationRequestKind::ProjectSymbols => {
            bail!("project symbols require a query request")
        }
    }
    Ok(buffer_id)
}

fn start_project_symbols_request(
    project: &Entity<Project>,
    buffer_id: u64,
    generation: u64,
    query: String,
    root: Option<PathBuf>,
    event_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) {
    let request = project.update(cx, |project, cx| project.symbols(&query, cx));
    let project = project.clone();
    cx.spawn(async move |cx| {
        let result: Result<Vec<LocationPresentation>> = async {
            let symbols = request.await.context("request project symbols")?;
            let mut items = Vec::with_capacity(symbols.len().min(PROJECT_SYMBOL_LIMIT));
            for symbol in symbols.into_iter().take(PROJECT_SYMBOL_LIMIT) {
                let buffer = project
                    .update(cx, |project, cx| {
                        project.open_buffer_for_symbol(&symbol, cx)
                    })
                    .await
                    .with_context(|| format!("open project symbol {}", symbol.name))?;
                let item = buffer.read_with(cx, |buffer, cx| {
                    let snapshot = buffer.snapshot();
                    let start = snapshot.unclipped_point_utf16_to_point(symbol.range.start);
                    let end = snapshot.unclipped_point_utf16_to_point(symbol.range.end);
                    let path = buffer
                        .file()
                        .map(|file| {
                            file.as_local()
                                .map(|file| file.abs_path(cx))
                                .unwrap_or_else(|| file.full_path(cx))
                        })
                        .unwrap_or_else(|| PathBuf::from("<untitled>"));
                    let line_len = snapshot.line_len(start.row);
                    let line = snapshot
                        .text_for_range(
                            language::Point::new(start.row, 0)
                                ..language::Point::new(start.row, line_len),
                        )
                        .flat_map(|chunk| chunk.chars())
                        .skip_while(|character| character.is_whitespace())
                        .take(160)
                        .collect::<String>()
                        .trim_end()
                        .to_owned();
                    LocationPresentation {
                        label: diagnostic_label(&path, root.as_deref()),
                        path,
                        row: start.row,
                        column: start.column,
                        end_row: end.row,
                        end_column: end.column,
                        snippet: if line.is_empty() {
                            symbol.name.clone()
                        } else {
                            format!("{} — {line}", symbol.name)
                        },
                    }
                });
                items.push(item);
            }
            Ok(items)
        }
        .await;
        let _ = event_sender
            .send(TerminalEvent::LocationsFinished {
                buffer_id,
                generation,
                kind: LocationRequestKind::ProjectSymbols,
                result: result.map_err(|error| format!("{error:#}")),
            })
            .await;
    })
    .detach();
}

fn diagnostic_severity_name(severity: lsp::DiagnosticSeverity) -> &'static str {
    if severity == lsp::DiagnosticSeverity::ERROR {
        "E"
    } else if severity == lsp::DiagnosticSeverity::WARNING {
        "W"
    } else if severity == lsp::DiagnosticSeverity::INFORMATION {
        "I"
    } else {
        "H"
    }
}

fn diagnostic_severity_rank(severity: &str) -> u8 {
    match severity {
        "E" => 0,
        "W" => 1,
        "I" => 2,
        _ => 3,
    }
}

fn diagnostic_label(path: &Path, root: Option<&Path>) -> String {
    root.and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path)
        .display()
        .to_string()
}

fn diagnostic_presentations(
    path: &Path,
    root: Option<&Path>,
    buffer: &Buffer,
    limit: usize,
) -> Vec<DiagnosticPresentation> {
    let label = bounded_terminal_text(&diagnostic_label(path, root));
    let snapshot = buffer.snapshot();
    snapshot
        .diagnostics_in_range::<_, language::Point>(0..buffer.len(), false)
        .filter(|entry| entry.diagnostic.is_primary)
        .take(limit)
        .map(|entry| DiagnosticPresentation {
            path: path.to_path_buf(),
            label: label.clone(),
            row: entry.range.start.row,
            column: entry.range.start.column,
            severity: diagnostic_severity_name(entry.diagnostic.severity).to_owned(),
            message: bounded_terminal_text(&entry.diagnostic.message),
            source: entry
                .diagnostic
                .source
                .as_deref()
                .map(bounded_terminal_text),
        })
        .collect()
}

async fn collect_project_diagnostics(
    project: Entity<Project>,
    root: Option<PathBuf>,
    open_buffers: Vec<(PathBuf, Entity<Buffer>)>,
    cx: &mut gpui::AsyncApp,
) -> Result<Vec<DiagnosticPresentation>> {
    let mut items = Vec::new();
    let mut visited = HashSet::new();
    for (path, buffer) in open_buffers {
        if items.len() >= MAX_LANGUAGE_RESPONSE_ITEMS {
            break;
        }
        visited.insert(path.clone());
        let remaining = MAX_LANGUAGE_RESPONSE_ITEMS.saturating_sub(items.len());
        items.extend(buffer.read_with(cx, |buffer, _| {
            diagnostic_presentations(&path, root.as_deref(), buffer, remaining)
        }));
    }

    let targets = project.read_with(cx, |project, cx| {
        let mut targets = BTreeMap::new();
        for (project_path, _, summary) in project.diagnostic_summaries(false, cx) {
            if summary.error_count == 0 && summary.warning_count == 0 {
                continue;
            }
            if let Some(path) = project.absolute_path(&project_path, cx) {
                targets.entry(path).or_insert(project_path);
            }
        }
        targets.into_iter().collect::<Vec<_>>()
    });
    for (path, project_path) in targets {
        if items.len() >= MAX_LANGUAGE_RESPONSE_ITEMS {
            break;
        }
        if !visited.insert(path.clone()) {
            continue;
        }
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await
            .with_context(|| format!("open diagnostic buffer {}", path.display()))?;
        let remaining = MAX_LANGUAGE_RESPONSE_ITEMS.saturating_sub(items.len());
        items.extend(buffer.read_with(cx, |buffer, _| {
            diagnostic_presentations(&path, root.as_deref(), buffer, remaining)
        }));
    }

    items.sort_by(|left, right| {
        diagnostic_severity_rank(&left.severity)
            .cmp(&diagnostic_severity_rank(&right.severity))
            .then_with(|| left.label.cmp(&right.label))
            .then_with(|| left.row.cmp(&right.row))
            .then_with(|| left.column.cmp(&right.column))
            .then_with(|| left.message.cmp(&right.message))
            .then_with(|| left.source.cmp(&right.source))
    });
    items.dedup();
    items.truncate(MAX_LANGUAGE_RESPONSE_ITEMS);
    Ok(items)
}

fn confined_workspace_path(canonical_root: &Path, path: &Path) -> Result<PathBuf> {
    ensure!(
        canonical_root.is_absolute() && path.is_absolute(),
        "rename paths must be absolute"
    );
    ensure!(
        !path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir)),
        "rename path contains a parent traversal: {}",
        path.display()
    );

    let mut existing_ancestor = path;
    while std::fs::symlink_metadata(existing_ancestor).is_err() {
        existing_ancestor = existing_ancestor.parent().with_context(|| {
            format!("rename target has no existing ancestor: {}", path.display())
        })?;
    }
    let canonical_ancestor = std::fs::canonicalize(existing_ancestor).with_context(|| {
        format!(
            "could not resolve rename path ancestor {}",
            existing_ancestor.display()
        )
    })?;
    ensure!(
        canonical_ancestor.starts_with(canonical_root),
        "language server proposed a path outside the workspace: {}",
        path.display()
    );
    if path.exists() {
        let canonical_path = std::fs::canonicalize(path)
            .with_context(|| format!("could not resolve rename path {}", path.display()))?;
        ensure!(
            canonical_path.starts_with(canonical_root),
            "language server proposed a symlink escape outside the workspace: {}",
            path.display()
        );
    }
    Ok(path.to_path_buf())
}

fn workspace_path_from_uri(canonical_root: &Path, uri: &lsp::Uri) -> Result<PathBuf> {
    let path = uri
        .to_file_path()
        .map_err(|()| anyhow::anyhow!("rename WorkspaceEdit contains a non-file URI: {uri}"))?;
    confined_workspace_path(canonical_root, &path)
}

fn rename_preview_edit(edit: &lsp::Edit) -> Result<RenamePreviewEdit> {
    let (edit, annotation_id) = match edit {
        lsp::Edit::Plain(edit) => (edit, None),
        lsp::Edit::Annotated(edit) => (&edit.text_edit, Some(edit.annotation_id.clone())),
        lsp::Edit::Snippet(_) => {
            bail!("rename preview rejects snippet edits because their expansion is focus-dependent")
        }
    };
    ensure!(
        (edit.range.start.line, edit.range.start.character)
            <= (edit.range.end.line, edit.range.end.character),
        "rename WorkspaceEdit contains an inverted text range"
    );
    Ok(RenamePreviewEdit {
        start_line: edit.range.start.line,
        start_character: edit.range.start.character,
        end_line: edit.range.end.line,
        end_character: edit.range.end.character,
        new_text: edit.new_text.clone(),
        annotation_id,
    })
}

fn normalize_rename_workspace_edit(
    edit: &lsp::WorkspaceEdit,
    workspace_root: &Path,
) -> Result<RenamePreviewPlan> {
    let canonical_root = std::fs::canonicalize(workspace_root).with_context(|| {
        format!(
            "could not resolve rename workspace root {}",
            workspace_root.display()
        )
    })?;
    ensure!(
        canonical_root.is_dir(),
        "rename workspace root is not a directory: {}",
        canonical_root.display()
    );

    let annotations = edit
        .change_annotations
        .as_ref()
        .map(|annotations| {
            annotations
                .iter()
                .map(|(id, annotation)| {
                    (
                        id.clone(),
                        RenamePreviewAnnotation {
                            label: annotation.label.clone(),
                            needs_confirmation: annotation.needs_confirmation.unwrap_or(false),
                            description: annotation.description.clone(),
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    let text_operation = |text_edit: &lsp::TextDocumentEdit| -> Result<_> {
        let path = workspace_path_from_uri(&canonical_root, &text_edit.text_document.uri)?;
        let edits = text_edit
            .edits
            .iter()
            .map(rename_preview_edit)
            .collect::<Result<Vec<_>>>()?;
        Ok(RenamePreviewOperation::Text {
            path,
            version: text_edit.text_document.version,
            edits,
        })
    };

    let mut operations = Vec::new();
    if let Some(document_changes) = &edit.document_changes {
        let document_changes = match document_changes {
            lsp::DocumentChanges::Edits(edits) => edits
                .iter()
                .cloned()
                .map(lsp::DocumentChangeOperation::Edit)
                .collect::<Vec<_>>(),
            lsp::DocumentChanges::Operations(operations) => operations.clone(),
        };
        for operation in &document_changes {
            let operation = match operation {
                lsp::DocumentChangeOperation::Edit(edit) => text_operation(edit)?,
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Create(create)) => {
                    let path = workspace_path_from_uri(&canonical_root, &create.uri)?;
                    ensure!(
                        path != canonical_root,
                        "rename may not create the workspace root"
                    );
                    RenamePreviewOperation::Create {
                        path,
                        overwrite: create
                            .options
                            .as_ref()
                            .and_then(|options| options.overwrite)
                            .unwrap_or(false),
                        ignore_if_exists: create
                            .options
                            .as_ref()
                            .and_then(|options| options.ignore_if_exists)
                            .unwrap_or(false),
                        annotation_id: create.annotation_id.clone(),
                    }
                }
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Rename(rename)) => {
                    let old_path = workspace_path_from_uri(&canonical_root, &rename.old_uri)?;
                    let new_path = workspace_path_from_uri(&canonical_root, &rename.new_uri)?;
                    ensure!(
                        old_path != canonical_root && new_path != canonical_root,
                        "rename may not move the workspace root"
                    );
                    RenamePreviewOperation::Rename {
                        old_path,
                        new_path,
                        overwrite: rename
                            .options
                            .as_ref()
                            .and_then(|options| options.overwrite)
                            .unwrap_or(false),
                        ignore_if_exists: rename
                            .options
                            .as_ref()
                            .and_then(|options| options.ignore_if_exists)
                            .unwrap_or(false),
                        annotation_id: rename.annotation_id.clone(),
                    }
                }
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Delete(delete)) => {
                    let path = workspace_path_from_uri(&canonical_root, &delete.uri)?;
                    ensure!(
                        path != canonical_root,
                        "rename may not delete the workspace root"
                    );
                    RenamePreviewOperation::Delete {
                        path,
                        recursive: delete
                            .options
                            .as_ref()
                            .and_then(|options| options.recursive)
                            .unwrap_or(false),
                        ignore_if_not_exists: delete
                            .options
                            .as_ref()
                            .and_then(|options| options.ignore_if_not_exists)
                            .unwrap_or(false),
                        annotation_id: delete
                            .options
                            .as_ref()
                            .and_then(|options| options.annotation_id.clone()),
                    }
                }
            };
            operations.push(operation);
        }
    } else if let Some(changes) = &edit.changes {
        let mut changes = changes
            .iter()
            .map(|(uri, edits)| {
                let path = workspace_path_from_uri(&canonical_root, uri)?;
                let edits = edits
                    .iter()
                    .cloned()
                    .map(lsp::Edit::Plain)
                    .collect::<Vec<_>>();
                Ok((path, edits))
            })
            .collect::<Result<Vec<_>>>()?;
        changes.sort_by(|left, right| left.0.cmp(&right.0));
        for (path, edits) in changes {
            operations.push(RenamePreviewOperation::Text {
                path,
                version: None,
                edits: edits
                    .iter()
                    .map(rename_preview_edit)
                    .collect::<Result<Vec<_>>>()?,
            });
        }
    }

    ensure!(
        !operations.is_empty(),
        "language server returned an empty rename WorkspaceEdit"
    );
    ensure!(
        operations.len() <= MAX_RENAME_PREVIEW_OPERATIONS,
        "rename WorkspaceEdit has too many operations ({} > {})",
        operations.len(),
        MAX_RENAME_PREVIEW_OPERATIONS
    );
    let edit_count = operations
        .iter()
        .map(|operation| match operation {
            RenamePreviewOperation::Text { edits, .. } => edits.len(),
            _ => 0,
        })
        .sum::<usize>();
    ensure!(
        edit_count <= MAX_RENAME_PREVIEW_OPERATIONS,
        "rename WorkspaceEdit has too many text edits ({edit_count} > {MAX_RENAME_PREVIEW_OPERATIONS})"
    );
    let file_operation_count = operations
        .iter()
        .filter(|operation| !matches!(operation, RenamePreviewOperation::Text { .. }))
        .count();
    let serialized = serde_json::to_vec(&(&operations, &annotations))
        .context("serialize normalized rename WorkspaceEdit")?;
    ensure!(
        serialized.len() <= MAX_RENAME_PREVIEW_BYTES,
        "rename WorkspaceEdit preview is too large ({} bytes > {})",
        serialized.len(),
        MAX_RENAME_PREVIEW_BYTES
    );
    let signature = format!("{:x}", Sha256::digest(&serialized));
    Ok(RenamePreviewPlan {
        operations,
        annotations,
        signature,
        edit_count,
        file_operation_count,
    })
}

fn utf16_position_to_byte(text: &str, line: u32, character: u32) -> Result<usize> {
    let target_line = usize::try_from(line).context("LSP line does not fit usize")?;
    let mut line_start = 0usize;
    for _ in 0..target_line {
        let newline = text[line_start..]
            .find('\n')
            .with_context(|| format!("LSP line {line} is past the end of the buffer"))?;
        line_start = line_start.saturating_add(newline).saturating_add(1);
    }
    let line_end = text[line_start..]
        .find('\n')
        .map(|offset| line_start + offset)
        .unwrap_or(text.len());
    let line_text = &text[line_start..line_end];
    let mut utf16_column = 0u32;
    for (byte, scalar) in line_text.char_indices() {
        if utf16_column == character {
            return Ok(line_start + byte);
        }
        let next = utf16_column.saturating_add(scalar.len_utf16() as u32);
        ensure!(
            character >= next,
            "LSP position splits a UTF-16 surrogate pair at {line}:{character}"
        );
        utf16_column = next;
    }
    ensure!(
        utf16_column == character,
        "LSP column {character} is past line {line} (UTF-16 length {utf16_column})"
    );
    Ok(line_end)
}

fn preview_fragment(text: &str) -> String {
    let mut fragment = String::new();
    let mut truncated = false;
    for (index, character) in text.chars().enumerate() {
        if index == 240 {
            truncated = true;
            break;
        }
        match character {
            '\n' => fragment.push_str("\\n"),
            '\r' => fragment.push_str("\\r"),
            '\t' => fragment.push_str("\\t"),
            character => fragment.push(character),
        }
    }
    if truncated {
        fragment.push('…');
    }
    fragment
}

fn apply_preview_text_edits(
    text: &str,
    edits: &[RenamePreviewEdit],
) -> Result<(String, Vec<String>)> {
    let mut resolved = edits
        .iter()
        .map(|edit| {
            let start = utf16_position_to_byte(text, edit.start_line, edit.start_character)?;
            let end = utf16_position_to_byte(text, edit.end_line, edit.end_character)?;
            ensure!(start <= end, "rename text edit has an inverted byte range");
            Ok((start, end, edit))
        })
        .collect::<Result<Vec<_>>>()?;
    resolved.sort_by_key(|(start, end, _)| (*start, *end));
    for pair in resolved.windows(2) {
        ensure!(
            pair[0].1 <= pair[1].0,
            "rename WorkspaceEdit contains overlapping text edits"
        );
    }
    let summaries = resolved
        .iter()
        .map(|(start, end, edit)| {
            format!(
                "@@ {}:{}-{}:{} @@\n- {}\n+ {}{}",
                edit.start_line.saturating_add(1),
                edit.start_character.saturating_add(1),
                edit.end_line.saturating_add(1),
                edit.end_character.saturating_add(1),
                preview_fragment(&text[*start..*end]),
                preview_fragment(&edit.new_text),
                edit.annotation_id
                    .as_ref()
                    .map(|id| format!("  [{id}]"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>();
    let mut updated = text.to_owned();
    for (start, end, edit) in resolved.into_iter().rev() {
        let replacement = edit.new_text.replace("\r\n", "\n").replace('\r', "\n");
        updated.replace_range(start..end, &replacement);
    }
    Ok((updated, summaries))
}

fn rename_plan_paths(plan: &RenamePreviewPlan) -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();
    for operation in &plan.operations {
        match operation {
            RenamePreviewOperation::Text { path, .. }
            | RenamePreviewOperation::Create { path, .. }
            | RenamePreviewOperation::Delete { path, .. } => {
                paths.insert(path.clone());
            }
            RenamePreviewOperation::Rename {
                old_path, new_path, ..
            } => {
                paths.insert(old_path.clone());
                paths.insert(new_path.clone());
            }
        }
    }
    paths
}

fn path_fingerprint(path: &Path) -> String {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            let kind = if metadata.file_type().is_symlink() {
                "symlink"
            } else if metadata.is_dir() {
                "directory"
            } else if metadata.is_file() {
                "file"
            } else {
                "other"
            };
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            let canonical = std::fs::canonicalize(path)
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            format!("{kind}:{}:{modified}:{canonical}", metadata.len())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => "missing".to_owned(),
        Err(error) => format!("error:{:?}:{error}", error.kind()),
    }
}

fn rename_workspace_root(
    repository: Option<&RepositorySession>,
    origin_buffer: &Entity<Buffer>,
    cx: &gpui::AsyncApp,
) -> Result<PathBuf> {
    if let Some(repository) = repository {
        return Ok(repository.root.canonical_path().to_path_buf());
    }
    let origin_path = origin_buffer.read_with(cx, |buffer, cx| {
        buffer.file().map(|file| {
            file.as_local()
                .map(|file| file.abs_path(cx))
                .unwrap_or_else(|| file.full_path(cx))
        })
    });
    let parent = origin_path
        .as_deref()
        .and_then(Path::parent)
        .context("rename origin has no workspace directory")?;
    std::fs::canonicalize(parent)
        .with_context(|| format!("could not resolve rename root {}", parent.display()))
}

async fn create_rename_preview_tab(
    origin_buffer: Entity<Buffer>,
    origin_point: language::Point,
    new_name: String,
    language_server_id: lsp::LanguageServerId,
    edit: lsp::WorkspaceEdit,
    workspace_root: PathBuf,
    repository: Option<&RepositorySession>,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<DocumentTab> {
    let plan = normalize_rename_workspace_edit(&edit, &workspace_root)?;
    let paths = rename_plan_paths(&plan);
    let path_guards = paths
        .iter()
        .map(|path| RenamePathGuard {
            path: path.clone(),
            fingerprint: path_fingerprint(path),
        })
        .collect::<Vec<_>>();

    let mut source_by_path = BTreeMap::<PathBuf, Entity<Buffer>>::new();
    for path in &paths {
        let metadata = services
            .file_system
            .metadata(path)
            .await
            .with_context(|| format!("could not inspect rename target {}", path.display()))?;
        let Some(metadata) = metadata else {
            continue;
        };
        if metadata.is_dir {
            continue;
        }
        let document = if let Some(repository) = repository {
            open_repository_document(path, repository, services, cx).await?
        } else {
            cx.update(|cx| open_document(Some(path.clone()), services.clone(), cx))
                .await?
        };
        source_by_path.insert(path.clone(), document.buffer);
    }

    let buffer_guards = source_by_path
        .iter()
        .map(|(path, buffer)| RenameBufferGuard {
            path: path.clone(),
            buffer: buffer.clone(),
            text_hash: buffer.read_with(cx, |buffer, _| {
                format!("{:x}", Sha256::digest(buffer.text().as_bytes()))
            }),
        })
        .collect::<Vec<_>>();
    let mut virtual_texts = source_by_path
        .iter()
        .map(|(path, buffer)| {
            (
                path.clone(),
                buffer.read_with(cx, |buffer, _| buffer.text()),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let path_label = |path: &Path| {
        path.strip_prefix(&workspace_root)
            .unwrap_or(path)
            .display()
            .to_string()
    };
    let mut preview_documents = Vec::with_capacity(plan.operations.len());
    for operation in &plan.operations {
        let preview = match operation {
            RenamePreviewOperation::Text {
                path,
                version,
                edits,
            } => {
                let text = virtual_texts.entry(path.clone()).or_default();
                let (updated, summaries) = apply_preview_text_edits(text, edits)?;
                *text = updated;
                format!(
                    "TEXT {}{}\n{}\n",
                    path_label(path),
                    version
                        .map(|version| format!(" @ version {version}"))
                        .unwrap_or_default(),
                    summaries.join("\n")
                )
            }
            RenamePreviewOperation::Create {
                path,
                overwrite,
                ignore_if_exists,
                annotation_id,
            } => {
                if *overwrite || !virtual_texts.contains_key(path) {
                    virtual_texts.insert(path.clone(), String::new());
                }
                format!(
                    "CREATE {}  overwrite={} ignore_if_exists={}{}\n",
                    path_label(path),
                    overwrite,
                    ignore_if_exists,
                    annotation_id
                        .as_ref()
                        .map(|id| format!("  [{id}]"))
                        .unwrap_or_default()
                )
            }
            RenamePreviewOperation::Rename {
                old_path,
                new_path,
                overwrite,
                ignore_if_exists,
                annotation_id,
            } => {
                if let Some(text) = virtual_texts.remove(old_path) {
                    virtual_texts.insert(new_path.clone(), text);
                }
                format!(
                    "RENAME {} → {}  overwrite={} ignore_if_exists={}{}\n",
                    path_label(old_path),
                    path_label(new_path),
                    overwrite,
                    ignore_if_exists,
                    annotation_id
                        .as_ref()
                        .map(|id| format!("  [{id}]"))
                        .unwrap_or_default()
                )
            }
            RenamePreviewOperation::Delete {
                path,
                recursive,
                ignore_if_not_exists,
                annotation_id,
            } => {
                virtual_texts.remove(path);
                format!(
                    "DELETE {}  recursive={} ignore_if_not_exists={}{}\n",
                    path_label(path),
                    recursive,
                    ignore_if_not_exists,
                    annotation_id
                        .as_ref()
                        .map(|id| format!("  [{id}]"))
                        .unwrap_or_default()
                )
            }
        };
        preview_documents.push(preview);
    }
    if !plan.annotations.is_empty() {
        let mut annotations = String::from("CHANGE ANNOTATIONS\n");
        for (id, annotation) in &plan.annotations {
            annotations.push_str(&format!(
                "[{id}] {}  confirmation={}{}\n",
                annotation.label,
                annotation.needs_confirmation,
                annotation
                    .description
                    .as_ref()
                    .map(|description| format!(" — {description}"))
                    .unwrap_or_default()
            ));
        }
        preview_documents.push(annotations);
    }
    let preview_bytes = preview_documents.iter().map(String::len).sum::<usize>();
    ensure!(
        preview_bytes <= MAX_RENAME_PREVIEW_BYTES,
        "rendered rename preview is too large ({preview_bytes} bytes > {MAX_RENAME_PREVIEW_BYTES})"
    );

    let preview_buffers = preview_documents
        .into_iter()
        .map(|text| {
            cx.update(|cx| {
                cx.new(|cx| {
                    let mut buffer = Buffer::local(text, cx);
                    buffer.set_capability(Capability::ReadOnly, cx);
                    buffer
                })
            })
        })
        .collect::<Vec<_>>();
    let title = format!(
        "Rename Preview: {new_name} ({} edit(s), {} file op(s))",
        plan.edit_count, plan.file_operation_count
    );
    let multi_buffer_title = title.clone();
    let preview_buffer_entities = preview_buffers.clone();
    let multi_buffer = cx.update(|cx| {
        cx.new(|cx| {
            let mut multi_buffer =
                MultiBuffer::new(Capability::ReadOnly).with_title(multi_buffer_title);
            for buffer in preview_buffer_entities {
                let max_point = buffer.read(cx).max_point();
                multi_buffer.set_excerpts_for_buffer(
                    buffer,
                    vec![language::Point::zero()..max_point],
                    0,
                    cx,
                );
            }
            multi_buffer
        })
    });
    let editor_window = cx.update(|cx| {
        open_multibuffer_editor_with_project(multi_buffer.clone(), services.project.clone(), cx)
    })?;
    let source_buffers = source_by_path.values().cloned().collect::<Vec<_>>();
    let document = OpenDocument {
        buffer: origin_buffer.clone(),
        untitled_label: None,
        project_searchable: false,
    };
    Ok(DocumentTab {
        document,
        multi_buffer: Some(MultiBufferTab {
            title,
            buffer: multi_buffer,
            source_buffers,
            pending_rename: Some(PendingRename {
                origin_buffer,
                origin_point,
                new_name,
                language_server_id,
                workspace_root,
                plan,
                buffer_guards,
                path_guards,
            }),
        }),
        editor_window,
        completion_generation: Arc::new(AtomicU64::new(0)),
        viewport: Viewport::default(),
        manual_vertical_scroll: false,
        last_cursor: None,
    })
}

fn validate_pending_rename_guards(pending: &PendingRename, cx: &gpui::AsyncApp) -> Result<()> {
    for guard in &pending.buffer_guards {
        let current_hash = guard.buffer.read_with(cx, |buffer, _| {
            format!("{:x}", Sha256::digest(buffer.text().as_bytes()))
        });
        ensure!(
            current_hash == guard.text_hash,
            "{} changed after the rename preview was built; preview again",
            guard.path.display()
        );
    }
    for guard in &pending.path_guards {
        ensure!(
            path_fingerprint(&guard.path) == guard.fingerprint,
            "{} changed on disk after the rename preview was built; preview again",
            guard.path.display()
        );
    }
    Ok(())
}

async fn create_locations_multibuffer_tab(
    title: String,
    items: &[LocationPresentation],
    repository: Option<&RepositorySession>,
    services: &FileServices,
    redraw_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<DocumentTab> {
    ensure!(
        !items.is_empty(),
        "cannot create an empty MultiBuffer result"
    );
    let mut grouped = BTreeMap::<PathBuf, Vec<Range<language::Point>>>::new();
    for item in items {
        grouped.entry(item.path.clone()).or_default().push(
            language::Point::new(item.row, item.column)
                ..language::Point::new(item.end_row, item.end_column),
        );
    }

    let mut documents = Vec::with_capacity(grouped.len());
    let mut excerpt_sources = Vec::with_capacity(grouped.len());
    for (path, ranges) in grouped {
        let document = if let Some(repository) = repository {
            open_repository_document(&path, repository, services, cx).await?
        } else {
            cx.update(|cx| open_document(Some(path.clone()), services.clone(), cx))
                .await?
        };
        let ranges = document.buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            ranges
                .into_iter()
                .map(|range| {
                    snapshot.clip_point(range.start, Bias::Left)
                        ..snapshot.clip_point(range.end, Bias::Right)
                })
                .collect::<Vec<_>>()
        });
        excerpt_sources.push((document.buffer.clone(), ranges));
        documents.push(document);
    }

    let source_buffers = excerpt_sources
        .iter()
        .map(|(buffer, _)| buffer.clone())
        .collect::<Vec<_>>();
    let multi_buffer_title = title.clone();
    let multi_buffer = cx.update(|cx| {
        cx.new(|cx| {
            let mut multi_buffer =
                MultiBuffer::new(Capability::ReadWrite).with_title(multi_buffer_title);
            for (buffer, ranges) in excerpt_sources {
                multi_buffer.set_excerpts_for_buffer(buffer, ranges, 2, cx);
            }
            multi_buffer
        })
    });
    let editor_window = cx.update(|cx| {
        open_multibuffer_editor_with_project(multi_buffer.clone(), services.project.clone(), cx)
    })?;
    let completion_generation = Arc::new(AtomicU64::new(0));
    let completion_provider = Rc::new(TerminalCompletionProvider {
        project: services.project.clone(),
        generation: completion_generation.clone(),
        event_sender: redraw_sender.clone(),
    });
    let buffer_store = services.buffer_store.clone();
    editor_window.update(cx, |editor, _window, cx| {
        editor.set_completion_provider(Some(completion_provider));
        for source_buffer in &source_buffers {
            let source_buffer_id = source_buffer.read(cx).remote_id().to_proto();
            let source_buffer = source_buffer.clone();
            let buffer_store = buffer_store.clone();
            let redraw_sender = redraw_sender.clone();
            cx.subscribe(
                &source_buffer,
                move |_, reload_buffer, event, cx| match event {
                    BufferEvent::ReloadNeeded => {
                        let reload = buffer_store.update(cx, |store, cx| {
                            store.reload_buffers(
                                [reload_buffer.clone()].into_iter().collect(),
                                true,
                                cx,
                            )
                        });
                        let sender = redraw_sender.clone();
                        cx.spawn(async move |_, _| {
                            let result = reload
                                .await
                                .map(|_| ())
                                .map_err(|error| format!("{error:#}"));
                            let _ = sender
                                .send(TerminalEvent::ReloadFinished {
                                    buffer_id: source_buffer_id,
                                    result,
                                })
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
                },
            )
            .detach();
        }
    })?;

    let document = documents
        .into_iter()
        .next()
        .context("MultiBuffer result lost its primary document")?;
    Ok(DocumentTab {
        document,
        multi_buffer: Some(MultiBufferTab {
            title,
            buffer: multi_buffer,
            source_buffers,
            pending_rename: None,
        }),
        editor_window,
        completion_generation,
        viewport: Viewport::default(),
        manual_vertical_scroll: false,
        last_cursor: None,
    })
}

async fn navigate_to_path_position(
    path: &Path,
    label: &str,
    row: u32,
    column: u32,
    repository: Option<&RepositorySession>,
    services: &FileServices,
    tabs: &mut Vec<DocumentTab>,
    active_index: &mut usize,
    redraw_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<String> {
    let existing = tabs.iter().position(|tab| {
        tab.multi_buffer.is_none()
            && document_state(&tab.document, cx).path.as_deref() == Some(path)
    });
    if let Some(index) = existing {
        *active_index = index;
    } else {
        let document = if let Some(repository) = repository {
            open_repository_document(path, repository, services, cx).await?
        } else {
            cx.update(|cx| open_document(Some(path.to_path_buf()), services.clone(), cx))
                .await?
        };
        tabs.push(create_document_tab(document, services, redraw_sender, cx)?);
        *active_index = tabs.len().saturating_sub(1);
    }

    tabs[*active_index].manual_vertical_scroll = false;
    tabs[*active_index].last_cursor = None;
    move_caret_to_text_position(
        &tabs[*active_index].editor_window,
        TextPosition {
            row: usize::try_from(row).unwrap_or(usize::MAX),
            byte_column: usize::try_from(column).unwrap_or(usize::MAX),
        },
        cx,
    )?;
    Ok(format!(
        "opened {label}:{}:{}",
        row.saturating_add(1),
        column.saturating_add(1)
    ))
}

async fn navigate_to_diagnostic(
    item: &DiagnosticPresentation,
    repository: Option<&RepositorySession>,
    services: &FileServices,
    tabs: &mut Vec<DocumentTab>,
    active_index: &mut usize,
    redraw_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<String> {
    navigate_to_path_position(
        &item.path,
        &item.label,
        item.row,
        item.column,
        repository,
        services,
        tabs,
        active_index,
        redraw_sender,
        cx,
    )
    .await
}

async fn navigate_to_location(
    item: &LocationPresentation,
    repository: Option<&RepositorySession>,
    services: &FileServices,
    tabs: &mut Vec<DocumentTab>,
    active_index: &mut usize,
    redraw_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<String> {
    navigate_to_path_position(
        &item.path,
        &item.label,
        item.row,
        item.column,
        repository,
        services,
        tabs,
        active_index,
        redraw_sender,
        cx,
    )
    .await
}

fn file_services(cx: &mut App) -> FileServices {
    let file_system = cx.global::<ProjectRuntime>().file_system.clone();
    file_services_with_fs(cx, file_system, true)
}

fn file_services_with_fs(
    cx: &mut App,
    file_system: Arc<dyn Fs>,
    watch_global_configs: bool,
) -> FileServices {
    let runtime = cx.global::<ProjectRuntime>().clone();
    let language_registry = runtime.language_registry.clone();
    let project = Project::local(
        runtime.client.clone(),
        runtime.node_runtime.clone(),
        runtime.user_store.clone(),
        language_registry.clone(),
        file_system.clone(),
        Some(env::vars().collect()),
        LocalProjectFlags {
            init_worktree_trust: true,
            watch_global_configs,
        },
        cx,
    );
    let (worktree_store, buffer_store, lsp_store) = project.read_with(cx, |project, _| {
        (
            project.worktree_store(),
            project.buffer_store().clone(),
            project.lsp_store(),
        )
    });
    FileServices {
        project,
        buffer_store,
        worktree_store,
        _lsp_store: lsp_store,
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
        .project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(&canonical_root, true, cx)
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

    // A repository-external file is intentionally a non-scanning single-file
    // worktree. Existing repository worktrees keep their scanners; this
    // prevents the outside file from probing or watching its parent and
    // siblings.
    services
        .worktree_store
        .update(cx, |store, _| store.disable_scanner());
    let (worktree, relative_path) = services
        .project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(canonical_path, false, cx)
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
        project_searchable: true,
    })
}

fn format_open_error(path: &Path, error: &anyhow::Error) -> String {
    let details = format!("{error:#}");
    let is_eloop = details.contains("Too many levels of symbolic links")
        || error.chain().any(|cause| {
            cause
                .downcast_ref::<io::Error>()
                .is_some_and(is_filesystem_loop_error)
        })
        // BufferStore's asynchronous load path can flatten the source error.
        // Re-classify the same local path for the user-facing startup error so
        // a real symlink loop remains distinguishable from an ordinary open
        // failure. This is diagnostic-only; Zed still owns the actual load.
        || std::fs::canonicalize(path)
            .is_err_and(|error| is_filesystem_loop_error(&error));
    if is_eloop {
        format!("ELOOP opening {}: {details}", path.display())
    } else {
        format!("failed to open {}: {details}", path.display())
    }
}

fn is_filesystem_loop_error(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(nix::libc::ELOOP)
    }
    #[cfg(windows)]
    {
        // Win32 ERROR_CANT_RESOLVE_FILENAME. This is what CreateFileW reports
        // when resolving a symbolic-link cycle.
        error.raw_os_error() == Some(1921)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = error;
        false
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
    open_buffers: Vec<Entity<Buffer>>,
    cx: &mut gpui::AsyncApp,
) -> Result<ProjectSearchCommand> {
    let key = request.key;
    let index = repository.index.clone();
    let file_system = services.file_system.clone();
    let buffer_store = services.buffer_store.clone();
    let running = cx.update(|cx| {
        start_literal_project_search(
            request.query,
            index.clone(),
            file_system,
            buffer_store,
            open_buffers,
            cx,
        )
    })?;
    let cancellation = running.cancellation_handle();
    let completion = cx.spawn(async move |cx| {
        running
            .collect(cx)
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
    open_buffers: Vec<Entity<Buffer>>,
    event_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let key = request.key;
    let command = start_zed_project_search_command(request, repository, services, open_buffers, cx);
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
    open_buffers: Vec<Entity<Buffer>>,
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
        dispatch_project_search(
            coordinator,
            next,
            repository,
            services,
            open_buffers,
            event_sender,
            cx,
        )?;
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
            project_searchable: false,
        }));
    };

    cx.spawn(async move |cx| {
        let (project_path, worktree) = project_path_for_file(&path, &services, cx).await?;
        let document = load_project_document(project_path, &path, &services, cx).await;
        drop(worktree);
        document
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

fn aggregate_buffer_state(buffers: &[Entity<Buffer>], cx: &gpui::AsyncApp) -> DocumentState {
    let mut state = DocumentState {
        path: None,
        dirty: false,
        conflict: false,
        deleted: false,
    };
    for buffer in buffers {
        buffer.read_with(cx, |buffer, cx| {
            if state.path.is_none() {
                state.path = buffer.file().map(|file| {
                    file.as_local()
                        .map(|file| file.abs_path(cx))
                        .unwrap_or_else(|| file.full_path(cx))
                });
            }
            state.dirty |= buffer.is_dirty();
            state.conflict |= buffer.has_conflict();
            state.deleted |= buffer
                .file()
                .is_some_and(|file| file.disk_state().is_deleted());
        });
    }
    state
}

fn tab_state(tab: &DocumentTab, cx: &gpui::AsyncApp) -> DocumentState {
    if tab
        .multi_buffer
        .as_ref()
        .is_some_and(|multi_buffer| multi_buffer.pending_rename.is_some())
    {
        return DocumentState {
            path: None,
            dirty: false,
            conflict: false,
            deleted: false,
        };
    }
    tab.multi_buffer.as_ref().map_or_else(
        || document_state(&tab.document, cx),
        |multi_buffer| aggregate_buffer_state(&multi_buffer.source_buffers, cx),
    )
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
) -> Result<(ProjectPath, Entity<project::Worktree>)> {
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

    let (worktree, _) = services
        .project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(&worktree_root, false, cx)
        })
        .await
        .with_context(|| format!("could not create a worktree for {}", path.display()))?;

    let project_path = services
        .worktree_store
        .read_with(cx, |store, cx| {
            store.project_path_for_absolute_path(path, cx)
        })
        .with_context(|| format!("worktree does not contain {}", path.display()))?;
    Ok((project_path, worktree))
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

struct SaveFormatPlan {
    buffers: Vec<Entity<Buffer>>,
    target: LspFormatTarget,
}

fn save_format_plan(
    buffers: &[Entity<Buffer>],
    multi_buffer: &Entity<MultiBuffer>,
    project: &Entity<Project>,
    cx: &App,
) -> Option<SaveFormatPlan> {
    let multi_buffer_snapshot = multi_buffer.read(cx).snapshot(cx);
    let project = project.read(cx);
    let git_store = project.git_store().read(cx);
    let mut fall_back_to_full_format = false;
    let mut modified_ranges = Vec::new();

    for buffer_entity in buffers {
        let buffer = buffer_entity.read(cx);
        let settings = LanguageSettings::for_buffer(buffer, cx);
        match settings.format_on_save {
            FormatOnSave::On | FormatOnSave::Off => {
                return Some(SaveFormatPlan {
                    buffers: buffers.to_vec(),
                    target: LspFormatTarget::Buffers,
                });
            }
            FormatOnSave::Modifications | FormatOnSave::ModificationsIfAvailable => {}
        }

        let Some(diff_snapshot) = git_store
            .get_unstaged_diff(buffer.remote_id(), cx)
            .map(|diff| diff.read(cx).snapshot(cx))
        else {
            if settings.format_on_save == FormatOnSave::ModificationsIfAvailable {
                fall_back_to_full_format = true;
            }
            continue;
        };

        let buffer_snapshot = buffer.snapshot();
        let mut merged: Vec<Range<text::Anchor>> = Vec::new();
        for hunk in diff_snapshot.hunks(&buffer_snapshot) {
            let range = hunk.buffer_range;
            if range.start.cmp(&range.end, &buffer_snapshot).is_eq() {
                continue;
            }
            let start_point = range.start.to_point(&buffer_snapshot);
            let end_point = range.end.to_point(&buffer_snapshot);
            let start_row = start_point.row;
            let end_row = if end_point.column == 0 && end_point.row > start_point.row {
                end_point.row - 1
            } else {
                end_point.row
            };
            let line_start = text::Point::new(start_row, 0);
            let line_end = text::Point::new(end_row, buffer_snapshot.line_len(end_row));
            let expanded =
                buffer_snapshot.anchor_before(line_start)..buffer_snapshot.anchor_after(line_end);
            if let Some(last) = merged.last_mut() {
                let last_end_point = last.end.to_point(&buffer_snapshot);
                if start_row <= last_end_point.row + 1 {
                    if expanded.end.to_point(&buffer_snapshot) > last_end_point {
                        last.end = expanded.end;
                    }
                    continue;
                }
            }
            merged.push(expanded);
        }

        let flat_anchors = merged
            .iter()
            .flat_map(|range| [range.start, range.end])
            .collect::<Vec<_>>();
        let multi_buffer_anchors =
            multi_buffer_snapshot.text_anchors_to_visible_anchors(flat_anchors);
        for pair in multi_buffer_anchors.chunks_exact(2) {
            let (Some(start), Some(end)) = (&pair[0], &pair[1]) else {
                continue;
            };
            modified_ranges.push(
                editor::ToPoint::to_point(start, &multi_buffer_snapshot)
                    ..editor::ToPoint::to_point(end, &multi_buffer_snapshot),
            );
        }
    }

    if fall_back_to_full_format {
        return Some(SaveFormatPlan {
            buffers: buffers.to_vec(),
            target: LspFormatTarget::Buffers,
        });
    }
    if modified_ranges.is_empty() {
        return None;
    }

    let multi_buffer = multi_buffer.read(cx);
    let snapshot = multi_buffer.read(cx);
    let mut buffer_id_to_ranges = BTreeMap::new();
    for selection_range in modified_ranges {
        for (buffer_snapshot, buffer_range, _) in
            snapshot.range_to_buffer_ranges(selection_range.start..selection_range.end)
        {
            let buffer_id = buffer_snapshot.remote_id();
            let start = buffer_snapshot.anchor_before(buffer_range.start);
            let end = buffer_snapshot.anchor_after(buffer_range.end);
            buffer_id_to_ranges
                .entry(buffer_id)
                .or_insert_with(Vec::new)
                .push(start..end);
        }
    }
    let targeted_buffers = buffer_id_to_ranges
        .keys()
        .filter_map(|buffer_id| multi_buffer.buffer(*buffer_id))
        .collect::<Vec<_>>();
    if targeted_buffers.is_empty() {
        None
    } else {
        Some(SaveFormatPlan {
            buffers: targeted_buffers,
            target: LspFormatTarget::Ranges(buffer_id_to_ranges),
        })
    }
}

async fn save_tab(
    tab: &DocumentTab,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    if let Some(multi_buffer) = &tab.multi_buffer {
        ensure!(
            multi_buffer.pending_rename.is_none(),
            "rename previews cannot be saved; Enter accepts or Esc rejects"
        );
    }

    // Build the same format-on-save target as Zed's Editor Item, but await the
    // Project format task without `log_err()`. The graphical Editor intentionally
    // logs formatter errors and proceeds with the write; the console contract keeps
    // the dirty buffer and reports the failed save instead. Saving with formatting
    // disabled afterward also guarantees at most one formatting transaction.
    let (editor_buffer, format_plan) = tab.editor_window.update(cx, |editor, _window, cx| {
        let editor_buffer = editor.buffer().clone();
        let is_singleton = editor_buffer.read(cx).is_singleton();
        let mut buffers = Vec::new();
        for handle in editor_buffer.read(cx).all_buffers() {
            let handle = handle.read(cx).base_buffer().unwrap_or(handle.clone());
            let should_save = is_singleton || {
                let buffer = handle.read(cx);
                buffer.is_dirty() && buffer.file().is_some()
            };
            if should_save && !buffers.contains(&handle) {
                buffers.push(handle);
            }
        }
        let format_plan = save_format_plan(&buffers, &editor_buffer, &services.project, cx);
        (editor_buffer, format_plan)
    })?;
    if let Some(format_plan) = format_plan {
        let format = services.project.update(cx, |project, cx| {
            project.format(
                format_plan.buffers.into_iter().collect(),
                format_plan.target,
                true,
                FormatTrigger::Save,
                cx,
            )
        });
        let transaction = format.await.context("could not format before save")?;
        if !editor_buffer.read_with(cx, |buffer, _| buffer.is_singleton()) {
            editor_buffer.update(cx, |buffer, cx| {
                buffer.push_transaction(&transaction.0, cx);
            });
        }
    }

    let save = tab.editor_window.update(cx, |editor, window, cx| {
        workspace::item::Item::save(
            editor,
            workspace::item::SaveOptions {
                format: false,
                ..workspace::item::SaveOptions::default()
            },
            services.project.clone(),
            window,
            cx,
        )
    })?;
    save.await.context("could not format and save editor tab")
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

async fn reload_tab(
    tab: &DocumentTab,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let Some(multi_buffer) = &tab.multi_buffer else {
        return reload_document(&tab.document, services, cx).await;
    };
    ensure!(
        multi_buffer.pending_rename.is_none(),
        "rename previews cannot be reloaded; Enter accepts or Esc rejects"
    );
    services
        .buffer_store
        .update(cx, |store, cx| {
            store.reload_buffers(
                multi_buffer.source_buffers.iter().cloned().collect(),
                true,
                cx,
            )
        })
        .await
        .context("could not reload MultiBuffer sources")?;
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

    let (project_path, worktree) = project_path_for_file(&path, services, cx).await?;
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
    drop(worktree);

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

fn load_keymap_bindings(content: &str, cx: &App) -> Result<Vec<KeyBinding>> {
    match settings::KeymapFile::load(content, cx) {
        settings::KeymapFileLoadResult::Success { key_bindings } => Ok(key_bindings),
        settings::KeymapFileLoadResult::SomeFailedToLoad { error_message, .. } => {
            bail!("keymap contains an invalid binding: {error_message}")
        }
        settings::KeymapFileLoadResult::JsonParseFailure { error } => {
            Err(error).context("parse keymap JSON")
        }
    }
}

fn apply_keymaps(user_bindings: Vec<KeyBinding>, cx: &mut App) -> Result<()> {
    let defaults =
        settings::KeymapFile::load_asset_allow_partial_failure(settings::DEFAULT_KEYMAP_PATH, cx)
            .context("load Zed default keymap")?;
    let terminal_defaults = load_keymap_bindings(
        &actions::terminalize_keymap_action_ids(TERMINAL_DEFAULT_KEYMAP),
        cx,
    )
    .context("load zec terminal keymap")?;
    cx.clear_key_bindings();
    cx.bind_keys(defaults);
    cx.bind_keys(terminal_defaults);
    cx.bind_keys(user_bindings);
    Ok(())
}

fn start_terminal_action_interceptor(
    pending_actions: Rc<RefCell<VecDeque<actions::TerminalAction>>>,
    event_sender: Option<async_channel::Sender<TerminalEvent>>,
    cx: &mut App,
) {
    macro_rules! register {
        ($action_type:ty, $terminal_action:expr) => {{
            let pending_actions = pending_actions.clone();
            let event_sender = event_sender.clone();
            cx.on_action::<$action_type>(move |_, cx| {
                pending_actions.borrow_mut().push_back($terminal_action);
                if let Some(event_sender) = &event_sender {
                    let _ = event_sender.try_send(TerminalEvent::Action($terminal_action));
                }
                // These actions belong to the terminal workspace reducer. Do
                // not let a hidden Editor or a future GUI shell run them too.
                cx.stop_propagation();
            });
        }};
    }

    use actions::TerminalAction as Action;
    use terminal_gpui_actions as gpui_action;
    register!(gpui_action::CommandPalette, Action::CommandPalette);
    register!(gpui_action::NewFile, Action::NewFile);
    register!(gpui_action::OpenFile, Action::OpenFile);
    register!(gpui_action::QuickOpen, Action::QuickOpen);
    register!(gpui_action::ProjectSearch, Action::ProjectSearch);
    register!(gpui_action::CloseTab, Action::CloseTab);
    register!(gpui_action::PreviousTab, Action::PreviousTab);
    register!(gpui_action::NextTab, Action::NextTab);
    register!(gpui_action::Find, Action::Find);
    register!(gpui_action::Replace, Action::Replace);
    register!(gpui_action::GoToLine, Action::GoToLine);
    register!(gpui_action::Reload, Action::Reload);
    register!(gpui_action::Save, Action::Save);
    register!(gpui_action::Quit, Action::Quit);
    register!(gpui_action::ShowCompletions, Action::ShowCompletions);
    register!(gpui_action::Hover, Action::Hover);
    register!(gpui_action::ProjectDiagnostics, Action::ProjectDiagnostics);
    register!(gpui_action::GoToDefinition, Action::GoToDefinition);
    register!(gpui_action::GoToTypeDefinition, Action::GoToTypeDefinition);
    register!(gpui_action::FindReferences, Action::FindReferences);
    register!(gpui_action::ProjectSymbols, Action::ProjectSymbols);
    register!(gpui_action::NavigateBack, Action::NavigateBack);
    register!(gpui_action::NavigateForward, Action::NavigateForward);
    register!(gpui_action::RenameSymbol, Action::RenameSymbol);
    register!(gpui_action::CodeActions, Action::CodeActions);
    register!(gpui_action::FormatDocument, Action::FormatDocument);
    register!(gpui_action::FormatSelection, Action::FormatSelection);
    register!(gpui_action::Undo, Action::Undo);
    register!(gpui_action::Redo, Action::Redo);
    register!(gpui_action::Copy, Action::Copy);
    register!(gpui_action::Cut, Action::Cut);
}

fn start_project_configuration_notifications(
    project: &Entity<Project>,
    event_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut App,
) {
    cx.subscribe(project, move |_project, event, _cx| match event {
        project::Event::Toast {
            notification_id,
            message,
            ..
        } if notification_id.as_ref().starts_with("local-settings-") => {
            let _ = event_sender.try_send(TerminalEvent::ConfigurationReloaded {
                kind: "project settings",
                result: Err(message.clone()),
            });
        }
        project::Event::HideToast { notification_id }
            if notification_id.as_ref().starts_with("local-settings-") =>
        {
            let _ = event_sender.try_send(TerminalEvent::ConfigurationReloaded {
                kind: "project settings",
                result: Ok("reloaded".to_owned()),
            });
        }
        project::Event::Toast { message, .. } => {
            let _ = event_sender.try_send(TerminalEvent::LanguageServiceNotice {
                level: "notice",
                message: bounded_terminal_text(message),
            });
        }
        project::Event::LanguageServerAdded(server_id, name, _) => {
            let _ = event_sender.try_send(TerminalEvent::LanguageServiceNotice {
                level: "starting",
                message: format!("{} ({server_id:?})", name.to_string()),
            });
        }
        project::Event::LanguageServerRemoved(server_id) => {
            let _ = event_sender.try_send(TerminalEvent::LanguageServiceNotice {
                level: "stopped",
                message: format!("server {server_id:?}"),
            });
        }
        project::Event::LanguageServerLog(
            _,
            project::LanguageServerLogType::Log(kind),
            message,
        ) => {
            let level = if *kind == lsp::MessageType::ERROR {
                Some("error")
            } else if *kind == lsp::MessageType::WARNING {
                Some("warning")
            } else {
                None
            };
            if let Some(level) = level {
                let _ = event_sender.try_send(TerminalEvent::LanguageServiceNotice {
                    level,
                    message: bounded_terminal_text(message),
                });
            }
        }
        project::Event::LanguageNotFound(_) => {
            let _ = event_sender.try_send(TerminalEvent::LanguageServiceNotice {
                level: "unavailable",
                message: "no registered language support for this buffer".to_owned(),
            });
        }
        _ => {}
    })
    .detach();
}

fn start_worktree_trust_notifications(
    worktree_store: Entity<WorktreeStore>,
    event_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut App,
) {
    let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) else {
        return;
    };
    cx.subscribe(&trusted_worktrees, move |_trusted, event, cx| {
        let TrustedWorktreesEvent::Restricted(_, restricted_paths) = event else {
            return;
        };
        for restricted_path in restricted_paths {
            let PathTrust::Worktree(worktree_id) = restricted_path else {
                continue;
            };
            let Some(path) = worktree_store
                .read(cx)
                .worktree_for_id(*worktree_id, cx)
                .map(|worktree| worktree.read(cx).abs_path().as_ref().to_path_buf())
            else {
                continue;
            };
            let _ = event_sender.try_send(TerminalEvent::WorktreeTrustRequired {
                worktree_id: *worktree_id,
                path,
            });
        }
    })
    .detach();
}

fn start_configuration_watchers(
    file_system: Arc<dyn Fs>,
    event_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut App,
) {
    let settings_sender = event_sender.clone();
    settings::SettingsStore::update_global(cx, {
        let file_system = file_system.clone();
        move |store, cx| {
            store.watch_settings_files(file_system, cx, move |settings_file, result, _cx| {
                let kind = match settings_file {
                    settings::SettingsFile::User => "user settings",
                    settings::SettingsFile::Global => "global settings",
                    _ => "settings",
                };
                let result = result
                    .result()
                    .map(|migrated| {
                        if migrated {
                            "reloaded (migration applied in memory)".to_owned()
                        } else {
                            "reloaded".to_owned()
                        }
                    })
                    .map_err(|error| format!("{error:#}"));
                let _ =
                    settings_sender.try_send(TerminalEvent::ConfigurationReloaded { kind, result });
            });
        }
    });

    let (mut keymap_rx, keymap_watcher) = settings::watch_config_file(
        &cx.background_executor(),
        file_system,
        paths::keymap_file().clone(),
    );
    cx.spawn(async move |cx| {
        let _keymap_watcher = keymap_watcher;
        let mut last_good_user_bindings = Vec::new();
        while let Some(content) = keymap_rx.next().await {
            let result: std::result::Result<String, String> = cx.update(|cx| {
                let content = actions::terminalize_keymap_action_ids(&content);
                match settings::KeymapFile::load(&content, cx) {
                    settings::KeymapFileLoadResult::Success { key_bindings } => {
                        apply_keymaps(key_bindings.clone(), cx)
                            .map_err(|error| format!("could not apply keymap: {error:#}"))?;
                        last_good_user_bindings = key_bindings;
                        Ok("reloaded".to_owned())
                    }
                    settings::KeymapFileLoadResult::SomeFailedToLoad {
                        key_bindings,
                        error_message,
                    } if !key_bindings.is_empty() => {
                        apply_keymaps(key_bindings.clone(), cx)
                            .map_err(|error| format!("could not apply keymap: {error:#}"))?;
                        last_good_user_bindings = key_bindings;
                        Ok(format!("partially reloaded: {error_message}"))
                    }
                    settings::KeymapFileLoadResult::SomeFailedToLoad { error_message, .. } => {
                        apply_keymaps(last_good_user_bindings.clone(), cx)
                            .map_err(|error| format!("could not restore keymap: {error:#}"))?;
                        Err(format!(
                            "invalid keymap; retained last valid bindings: {error_message}"
                        ))
                    }
                    settings::KeymapFileLoadResult::JsonParseFailure { error } => {
                        apply_keymaps(last_good_user_bindings.clone(), cx)
                            .map_err(|error| format!("could not restore keymap: {error:#}"))?;
                        Err(format!(
                            "invalid keymap JSON; retained last valid bindings: {error:#}"
                        ))
                    }
                }
            });
            let _ = event_sender.try_send(TerminalEvent::ConfigurationReloaded {
                kind: "user keymap",
                result,
            });
        }
    })
    .detach();
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
    project::trusted_worktrees::init(Default::default(), cx);

    if !cx.has_global::<ProjectRuntime>() {
        let client = Client::production(cx);
        Project::init(&client, cx);
        let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
        let node_runtime = node_runtime::NodeRuntime::unavailable();
        let real_file_system: Arc<dyn Fs> =
            Arc::new(RealFs::new(None, cx.background_executor().clone()));
        let file_system: Arc<dyn Fs> = ZecFs::guarded(real_file_system);
        <dyn Fs>::set_global(file_system.clone(), cx);
        let language_registry = Arc::new(LanguageRegistry::new(cx.background_executor().clone()));
        language_registry.set_theme(cx.theme().clone());
        languages::init(
            language_registry.clone(),
            file_system.clone(),
            node_runtime.clone(),
            cx,
        );
        cx.set_global(ProjectRuntime {
            client,
            user_store,
            node_runtime,
            file_system,
            language_registry,
        });
    }

    apply_keymaps(Vec::new(), cx).expect("failed to load Zed/zec default keymaps");
}

#[derive(Clone)]
struct ProjectRuntime {
    client: Arc<Client>,
    user_store: Entity<UserStore>,
    node_runtime: node_runtime::NodeRuntime,
    file_system: Arc<dyn Fs>,
    language_registry: Arc<LanguageRegistry>,
}

impl gpui::Global for ProjectRuntime {}

fn editor_application() -> gpui::Application {
    #[cfg(windows)]
    {
        // The pinned GPUI Windows headless platform intentionally omits the
        // DirectX and drag/drop devices required by `open_window`. zec keeps
        // its editor windows hidden, but still needs the regular platform to
        // construct Zed's Editor.
        gpui_platform::application()
    }
    #[cfg(not(windows))]
    {
        gpui_platform::headless()
    }
}

fn open_editor(buffer: Entity<Buffer>, cx: &mut App) -> Result<WindowHandle<Editor>> {
    open_editor_with_project(buffer, None, cx)
}

fn open_editor_with_project(
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

fn open_multibuffer_editor_with_project(
    buffer: Entity<MultiBuffer>,
    project: Entity<Project>,
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
            let editor = cx.new(|cx| Editor::for_multibuffer(buffer, Some(project), window, cx));
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
    .context("GPUI could not create the hidden MultiBuffer window")
}

fn create_document_tab(
    document: OpenDocument,
    services: &FileServices,
    redraw_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<DocumentTab> {
    let project = services.project.clone();
    let buffer_store = services.buffer_store.clone();
    let editor_window =
        cx.update(|cx| open_editor_with_project(document.buffer.clone(), Some(project), cx))?;
    let buffer = document.buffer.clone();
    let buffer_id = buffer.read_with(cx, |buffer, _| buffer.remote_id().to_proto());
    let completion_generation = Arc::new(AtomicU64::new(0));
    let completion_provider = Rc::new(TerminalCompletionProvider {
        project: services.project.clone(),
        generation: completion_generation.clone(),
        event_sender: redraw_sender.clone(),
    });
    if let Err(error) = editor_window.update(cx, |editor, _window, cx| {
        editor.set_completion_provider(Some(completion_provider));
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
        multi_buffer: None,
        editor_window,
        completion_generation,
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
            let name = tab.multi_buffer.as_ref().map_or_else(
                || document_label(&tab.document, multiple, cx),
                |multi_buffer| multi_buffer.title.clone(),
            );
            let state = tab_state(tab, cx);
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
    completion: Option<&CompletionPrompt>,
    command_palette: Option<&CommandPalettePrompt>,
    language_overlay: Option<&LanguageOverlay>,
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

    let (status, status_cursor_column) = if let Some(completion) = completion {
        let (status, cursor) = completion.status(message);
        (status, Some(cursor))
    } else if let Some(command_palette) = command_palette {
        let (status, cursor) = command_palette.status(message);
        (status, Some(cursor))
    } else if let Some(language_overlay) = language_overlay {
        let (status, cursor) = language_overlay.status(message);
        (
            status,
            language_overlay.has_status_cursor().then_some(cursor),
        )
    } else if let Some((quick_open, index)) = quick_open {
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
            "zec {status_label}  F1 commands  Ctrl-Space completion  F2 hover  F8 diagnostics  F12 definition  Alt/Shift-F12 type/refs  Ctrl-N new  Ctrl-O open  Ctrl-P quick open  Alt-F project search  Ctrl-W close  Ctrl-PgUp/PgDn tabs  Alt-PgUp/PgDn scroll  Ctrl-F find  Ctrl-H replace  Ctrl-G line  Ctrl-R reload  Ctrl-S save  Ctrl-Q quit"
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
            overlay: completion
                .map(CompletionPrompt::overlay)
                .or_else(|| command_palette.map(CommandPalettePrompt::overlay))
                .or_else(|| language_overlay.map(LanguageOverlay::overlay)),
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
    editor_application().run(|cx| {
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
    editor_application().run(move |cx| {
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

fn run_alpha_2_probe(probe: Alpha2Probe) -> Result<()> {
    let (sender, receiver) = mpsc::sync_channel(1);
    editor_application().run(move |cx| {
        init_zed(cx);
        let watches_configuration = matches!(&probe, Alpha2Probe::SettingsReload { .. });
        let (event_sender, event_receiver) = async_channel::bounded(64);
        if watches_configuration {
            let configuration_file_system = cx.global::<ProjectRuntime>().file_system.clone();
            start_configuration_watchers(configuration_file_system, event_sender.clone(), cx);
            start_terminal_action_interceptor(
                Rc::new(RefCell::new(VecDeque::new())),
                Some(event_sender.clone()),
                cx,
            );
        }
        let services = file_services(cx);
        start_project_configuration_notifications(&services.project, event_sender.clone(), cx);
        cx.spawn(async move |cx| {
            let result = execute_alpha_2_probe(probe, &services, event_receiver, cx)
                .await
                .and_then(|value| {
                    serde_json::to_string(&value).context("serialize Alpha 2 probe result")
                });
            sender.send(result).expect("send Alpha 2 probe result");
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
    });

    let json = receiver
        .recv()
        .context("Alpha 2 probe runtime exited without a result")??;
    println!("{json}");
    Ok(())
}

async fn execute_alpha_2_probe(
    probe: Alpha2Probe,
    services: &FileServices,
    configuration_events: async_channel::Receiver<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    match probe {
        Alpha2Probe::LanguageService { root, file } => {
            alpha_2_language_service_probe(&root, &file, services, cx).await
        }
        Alpha2Probe::SettingsReload { root, file } => {
            alpha_2_settings_reload_probe(&root, &file, services, configuration_events, cx).await
        }
        Alpha2Probe::LspFailure {
            root,
            file,
            scenario,
        } => {
            alpha_2_lsp_failure_probe(&root, &file, &scenario, services, configuration_events, cx)
                .await
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct EffectiveLanguageSettings {
    tab_size: u32,
    format_on_save: String,
    completion_lsp: bool,
    show_completions_on_input: bool,
}

fn effective_language_settings(
    buffer: &Entity<Buffer>,
    cx: &gpui::AsyncApp,
) -> EffectiveLanguageSettings {
    buffer.read_with(cx, |buffer, cx| {
        let settings = language::language_settings::LanguageSettings::for_buffer(buffer, cx);
        EffectiveLanguageSettings {
            tab_size: settings.tab_size.get(),
            format_on_save: format!("{:?}", settings.format_on_save).to_ascii_lowercase(),
            completion_lsp: settings.completions.lsp,
            show_completions_on_input: settings.show_completions_on_input,
        }
    })
}

async fn wait_for_effective_language_settings(
    buffer: &Entity<Buffer>,
    expected: &EffectiveLanguageSettings,
    cx: &mut gpui::AsyncApp,
) -> Result<EffectiveLanguageSettings> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let actual = effective_language_settings(buffer, cx);
        if &actual == expected {
            return Ok(actual);
        }
        ensure!(
            Instant::now() < deadline,
            "effective settings did not reload within 5 seconds; expected {expected:?}, actual {actual:?}"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
}

async fn wait_for_configuration_result(
    events: &async_channel::Receiver<TerminalEvent>,
    kind: &str,
    success: bool,
    cx: &mut gpui::AsyncApp,
) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match events.try_recv() {
            Ok(TerminalEvent::ConfigurationReloaded {
                kind: event_kind,
                result,
            }) if event_kind == kind => match result {
                Ok(status) if success => return Ok(status),
                Err(error) if !success => return Ok(error),
                _ => {}
            },
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("configuration event channel closed while waiting for {kind}")
            }
        }
        ensure!(
            Instant::now() < deadline,
            "timed out waiting for {kind} {} event",
            if success { "success" } else { "failure" }
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
}

fn dispatch_probe_keystrokes(
    editor_window: &WindowHandle<Editor>,
    keys: &[&str],
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let input_window: AnyWindowHandle = (*editor_window).into();
    cx.update_window(input_window, |_root, window, _cx| {
        window.activate_window();
    })?;
    for key in keys {
        let keystroke =
            Keystroke::parse(key).with_context(|| format!("parse probe keystroke {key}"))?;
        cx.update_window(input_window, |_root, window, cx| {
            window.dispatch_keystroke(keystroke, cx)
        })?;
    }
    Ok(())
}

async fn wait_for_terminal_action(
    events: &async_channel::Receiver<TerminalEvent>,
    expected: actions::TerminalAction,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match events.try_recv() {
            Ok(TerminalEvent::Action(action)) if action == expected => return Ok(()),
            Ok(TerminalEvent::Action(action)) => {
                bail!("expected terminal action {expected:?}, received {action:?}")
            }
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("terminal action event channel closed")
            }
        }
        ensure!(
            Instant::now() < deadline,
            "timed out waiting for terminal action {expected:?}"
        );
        cx.background_executor()
            .timer(Duration::from_millis(5))
            .await;
    }
}

async fn ensure_no_terminal_action(
    events: &async_channel::Receiver<TerminalEvent>,
    duration: Duration,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let deadline = Instant::now() + duration;
    loop {
        match events.try_recv() {
            Ok(TerminalEvent::Action(action)) => {
                bail!("an unbound key unexpectedly dispatched {action:?}")
            }
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("terminal action event channel closed")
            }
        }
        if Instant::now() >= deadline {
            return Ok(());
        }
        cx.background_executor()
            .timer(Duration::from_millis(5))
            .await;
    }
}

async fn wait_for_rust_analyzer_ready(
    project: &Entity<Project>,
    timeout: Duration,
    cx: &mut gpui::AsyncApp,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if project.read_with(cx, |project, cx| {
            project.language_server_statuses(cx).any(|(_, status)| {
                status.name.to_string() == "rust-analyzer" && status.process_id.is_some()
            })
        }) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
}

fn trust_probe_worktrees(services: &FileServices, cx: &mut gpui::AsyncApp) -> Result<()> {
    let trusted_worktrees = cx
        .update(|cx| TrustedWorktrees::try_get_global(cx))
        .context("worktree trust service is unavailable")?;
    let worktree_ids = services.worktree_store.read_with(cx, |store, cx| {
        store
            .worktrees()
            .map(|worktree| worktree.read(cx).id())
            .collect::<Vec<_>>()
    });
    trusted_worktrees.update(cx, |trusted_worktrees, cx| {
        trusted_worktrees.trust(
            &services.worktree_store,
            worktree_ids.into_iter().map(PathTrust::Worktree).collect(),
            cx,
        );
    });
    Ok(())
}

async fn bounded_completion_request(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    position: usize,
    timeout: Duration,
    cx: &mut gpui::AsyncApp,
) -> (String, usize, usize, u128, usize) {
    let started = Instant::now();
    let request = project.update(cx, |project, cx| {
        project.completions(
            buffer,
            position,
            editor::CompletionContext {
                trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                trigger_character: None,
            },
            cx,
        )
    });
    let timer = cx.background_executor().timer(timeout);
    let request = Box::pin(request);
    let timer = Box::pin(timer);
    match futures::future::select(request, timer).await {
        futures::future::Either::Left((result, _)) => match result {
            Ok(responses) => {
                let presentations = completion_presentations(&responses);
                let max_documentation_bytes = presentations
                    .iter()
                    .filter_map(|item| item.documentation.as_ref())
                    .map(String::len)
                    .max()
                    .unwrap_or(0);
                let completion_count = presentations.len();
                let mut prompt = CompletionPrompt::running(1, 1);
                let _ = prompt.complete(1, 1, Ok(presentations));
                let overlay_rows = prompt.overlay().rows.len();
                (
                    "ok".to_owned(),
                    completion_count,
                    max_documentation_bytes,
                    started.elapsed().as_millis(),
                    overlay_rows,
                )
            }
            Err(error) => (
                format!("error: {error:#}"),
                0,
                0,
                started.elapsed().as_millis(),
                0,
            ),
        },
        futures::future::Either::Right(((), pending_request)) => {
            drop(pending_request);
            (
                "cancelled".to_owned(),
                0,
                0,
                started.elapsed().as_millis(),
                0,
            )
        }
    }
}

async fn alpha_2_lsp_failure_probe(
    root_path: &Path,
    file_path: &Path,
    scenario: &str,
    services: &FileServices,
    service_events: async_channel::Receiver<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    let repository = prepare_repository(root_path, services, cx).await?;
    trust_probe_worktrees(services, cx)?;
    let document = open_repository_document(file_path, &repository, services, cx).await?;
    let buffer = document.buffer.clone();
    let (redraw_sender, _redraw_receiver) = async_channel::bounded(64);
    let tab = create_document_tab(document, services, redraw_sender, cx)?;
    let original_disk = services
        .file_system
        .load(file_path)
        .await
        .with_context(|| format!("read failure fixture {}", file_path.display()))?;
    let original_buffer = buffer.read_with(cx, |buffer, _| buffer.text());
    ensure!(
        original_buffer == original_disk,
        "failure fixture buffer did not start from disk"
    );
    let marker_position = original_buffer
        .find("alpha_")
        .map(|offset| offset + "alpha_".len())
        .unwrap_or(0);

    let initially_ready = wait_for_rust_analyzer_ready(
        &services.project,
        if matches!(
            scenario,
            "request-error"
                | "hang-request"
                | "crash-request"
                | "malformed-response"
                | "large-payloads"
                | "formatter-error"
                | "huge-stderr"
                | "restart-once"
        ) {
            Duration::from_secs(10)
        } else {
            Duration::from_millis(500)
        },
        cx,
    )
    .await;

    if scenario == "formatter-error" {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let readiness = bounded_completion_request(
                &services.project,
                &buffer,
                marker_position,
                Duration::from_secs(1),
                cx,
            )
            .await;
            if readiness.0 == "ok" && readiness.1 == 2 {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "formatter fixture buffer was not registered with rust-analyzer; outcome {}, count {}",
                readiness.0,
                readiness.1
            );
            cx.background_executor()
                .timer(Duration::from_millis(25))
                .await;
        }
    }

    let edited_text = format!("{original_buffer}// continued after {scenario}\n");
    tab.editor_window.update(cx, |editor, window, cx| {
        editor.select_all(&SelectAll, window, cx);
        editor.insert(&edited_text, window, cx);
    })?;
    let dirty_after_edit = document_state(&tab.document, cx).dirty;
    tab.editor_window
        .update(cx, |editor, window, cx| editor.undo(&Undo, window, cx))?;
    let text_after_undo = buffer.read_with(cx, |buffer, _| buffer.text());
    tab.editor_window
        .update(cx, |editor, window, cx| editor.redo(&Redo, window, cx))?;
    let text_after_redo = buffer.read_with(cx, |buffer, _| buffer.text());
    ensure!(
        dirty_after_edit && text_after_undo == original_buffer && text_after_redo == edited_text,
        "LSP failure changed the Editor undo/redo authority"
    );

    let mut formatter_failure = None;
    let mut dirty_after_formatter_failure = None;
    let mut disk_after_formatter_failure = None;
    if scenario == "formatter-error" {
        let expected = EffectiveLanguageSettings {
            tab_size: 4,
            format_on_save: "on".to_owned(),
            completion_lsp: true,
            show_completions_on_input: true,
        };
        wait_for_effective_language_settings(&buffer, &expected, cx).await?;
        let error = save_tab(&tab, services, cx)
            .await
            .expect_err("controlled formatter error unexpectedly saved");
        formatter_failure = Some(format!("{error:#}"));
        dirty_after_formatter_failure = Some(document_state(&tab.document, cx).dirty);
        disk_after_formatter_failure = Some(
            services
                .file_system
                .load(file_path)
                .await
                .with_context(|| format!("read formatter failure disk {}", file_path.display()))?,
        );
        ensure!(
            dirty_after_formatter_failure == Some(true)
                && disk_after_formatter_failure.as_deref() == Some(original_disk.as_str()),
            "formatter failure changed disk or cleared dirty state"
        );
        let local_settings_path = root_path.join(".zed/settings.json");
        std::fs::write(
            &local_settings_path,
            r#"{
              "format_on_save": "off",
              "remove_trailing_whitespace_on_save": false,
              "ensure_final_newline_on_save": false
            }"#,
        )
        .with_context(|| {
            format!(
                "disable formatter after controlled failure {}",
                local_settings_path.display()
            )
        })?;
        let settings_deadline = Instant::now() + Duration::from_secs(5);
        while effective_language_settings(&buffer, cx).format_on_save != "off" {
            ensure!(
                Instant::now() < settings_deadline,
                "format_on_save did not turn off after formatter failure"
            );
            cx.background_executor()
                .timer(Duration::from_millis(10))
                .await;
        }
    }
    save_tab(&tab, services, cx).await?;
    let saved_disk = services
        .file_system
        .load(file_path)
        .await
        .with_context(|| format!("read continued save {}", file_path.display()))?;
    ensure!(
        saved_disk == edited_text && !document_state(&tab.document, cx).dirty,
        "editing/save did not remain usable after LSP failure"
    );

    let restart_requested = scenario == "restart-once";
    if restart_requested {
        // A crashed language server is intentionally left stopped by Project until the
        // user asks for a restart. Exercise that recovery path explicitly instead of
        // mistaking Project's temporary empty completion set for a recovered server.
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
        services.project.update(cx, |project, cx| {
            project.restart_language_servers_for_buffers(
                vec![buffer.clone()],
                Default::default(),
                true,
                cx,
            );
        });
    }
    let (
        request_outcome,
        completion_count,
        max_documentation_bytes,
        request_elapsed_ms,
        completion_overlay_rows,
    ) = if scenario == "restart-once" {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let result = bounded_completion_request(
                &services.project,
                &buffer,
                marker_position,
                Duration::from_secs(2),
                cx,
            )
            .await;
            if result.0 == "ok" && result.1 == 2 {
                break result;
            }
            ensure!(
                Instant::now() < deadline,
                "language server did not recover after controlled restart; last outcome {}, count {}",
                result.0,
                result.1
            );
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
        }
    } else {
        bounded_completion_request(
            &services.project,
            &buffer,
            marker_position,
            if scenario.contains("hang") {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(3)
            },
            cx,
        )
        .await
    };

    let diagnostic_deadline = Instant::now() + Duration::from_secs(5);
    let diagnostic_summary = loop {
        let summary = services
            .project
            .read_with(cx, |project, cx| project.diagnostic_summary(false, cx));
        if scenario != "large-payloads"
            || summary.error_count.saturating_add(summary.warning_count) >= 10_000
            || Instant::now() >= diagnostic_deadline
        {
            break summary;
        }
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    };
    let (diagnostic_item_count, diagnostic_overlay_rows) = if scenario == "large-payloads" {
        let items = collect_project_diagnostics(
            services.project.clone(),
            Some(root_path.to_path_buf()),
            vec![(file_path.to_path_buf(), buffer.clone())],
            cx,
        )
        .await?;
        let item_count = items.len();
        let mut prompt = DiagnosticsPrompt::running(1);
        let _ = prompt.complete(1, Ok(items));
        (item_count, prompt.overlay().rows.len())
    } else {
        (
            diagnostic_summary
                .error_count
                .saturating_add(diagnostic_summary.warning_count),
            0,
        )
    };
    let statuses = services.project.read_with(cx, |project, cx| {
        project
            .language_server_statuses(cx)
            .map(|(_, status)| {
                serde_json::json!({
                    "name": status.name.to_string(),
                    "process_id": status.process_id,
                })
            })
            .collect::<Vec<_>>()
    });

    let _keepalive_window = cx.update(|cx| open_editor(buffer.clone(), cx))?;
    let close_started = Instant::now();
    tab.editor_window
        .update(cx, |_editor, window, _cx| window.remove_window())?;
    let close_elapsed_ms = close_started.elapsed().as_millis();
    ensure!(
        close_elapsed_ms <= 250,
        "closing an editor during an LSP failure took {close_elapsed_ms} ms"
    );

    let mut service_notices = Vec::new();
    while let Ok(event) = service_events.try_recv() {
        if let TerminalEvent::LanguageServiceNotice { level, message } = event {
            service_notices.push(serde_json::json!({
                "level": level,
                "message": message,
            }));
        }
    }

    Ok(serde_json::json!({
        "scenario": scenario,
        "initially_ready": initially_ready,
        "restart_requested": restart_requested,
        "request": {
            "outcome": request_outcome,
            "user_message": (!initially_ready)
                .then(|| language_service_unavailable_message("completion")),
            "elapsed_ms": request_elapsed_ms,
            "completion_count": completion_count,
            "max_documentation_bytes": max_documentation_bytes,
            "overlay_rows": completion_overlay_rows,
        },
        "diagnostics": {
            "errors": diagnostic_summary.error_count,
            "warnings": diagnostic_summary.warning_count,
            "item_count": diagnostic_item_count,
            "overlay_rows": diagnostic_overlay_rows,
        },
        "limits": {
            "language_items": MAX_LANGUAGE_RESPONSE_ITEMS,
            "language_text_bytes": MAX_LANGUAGE_TEXT_BYTES,
            "overlay_rows": MAX_OVERLAY_SNAPSHOT_ROWS,
        },
        "editor": {
            "dirty_after_edit": dirty_after_edit,
            "undo_restored": text_after_undo == original_buffer,
            "redo_restored": text_after_redo == edited_text,
            "saved": saved_disk == edited_text,
            "close_elapsed_ms": close_elapsed_ms,
        },
        "formatter_failure": {
            "error": formatter_failure,
            "dirty": dirty_after_formatter_failure,
            "disk": disk_after_formatter_failure,
        },
        "statuses": statuses,
        "service_notices": service_notices,
    }))
}

async fn alpha_2_settings_reload_probe(
    root_path: &Path,
    file_path: &Path,
    services: &FileServices,
    configuration_events: async_channel::Receiver<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    let repository = prepare_repository(root_path, services, cx).await?;
    trust_probe_worktrees(services, cx)?;
    let document = open_repository_document(file_path, &repository, services, cx).await?;
    let buffer = document.buffer.clone();
    let (redraw_sender, _redraw_receiver) = async_channel::bounded(64);
    let tab = create_document_tab(document, services, redraw_sender, cx)?;

    let initial_keymap_status =
        wait_for_configuration_result(&configuration_events, "user keymap", true, cx).await?;
    let initial_expected = EffectiveLanguageSettings {
        tab_size: 5,
        format_on_save: "off".to_owned(),
        completion_lsp: true,
        show_completions_on_input: true,
    };
    let initial = wait_for_effective_language_settings(&buffer, &initial_expected, cx).await?;

    dispatch_probe_keystrokes(&tab.editor_window, &["ctrl-k", "ctrl-p"], cx)?;
    wait_for_terminal_action(
        &configuration_events,
        actions::TerminalAction::CommandPalette,
        cx,
    )
    .await?;
    dispatch_probe_keystrokes(&tab.editor_window, &["f1"], cx)?;
    ensure_no_terminal_action(&configuration_events, Duration::from_millis(75), cx).await?;

    let keymap_path = paths::keymap_file().clone();
    std::fs::write(&keymap_path, "{")
        .with_context(|| format!("write invalid keymap {}", keymap_path.display()))?;
    let invalid_keymap_error =
        wait_for_configuration_result(&configuration_events, "user keymap", false, cx).await?;
    dispatch_probe_keystrokes(&tab.editor_window, &["ctrl-k", "ctrl-p"], cx)?;
    wait_for_terminal_action(
        &configuration_events,
        actions::TerminalAction::CommandPalette,
        cx,
    )
    .await?;

    std::fs::write(
        &keymap_path,
        r#"[
          {
            "context": "Editor",
            "bindings": {
              "f1": null,
              "f3": "editor::Hover"
            }
          }
        ]"#,
    )
    .with_context(|| format!("write replacement keymap {}", keymap_path.display()))?;
    let replacement_keymap_status =
        wait_for_configuration_result(&configuration_events, "user keymap", true, cx).await?;
    dispatch_probe_keystrokes(&tab.editor_window, &["f3"], cx)?;
    wait_for_terminal_action(&configuration_events, actions::TerminalAction::Hover, cx).await?;
    dispatch_probe_keystrokes(&tab.editor_window, &["f1"], cx)?;
    ensure_no_terminal_action(&configuration_events, Duration::from_millis(75), cx).await?;

    let local_settings_path = root_path.join(".zed/settings.json");
    std::fs::write(
        &local_settings_path,
        r#"{
          "tab_size": 6,
          "format_on_save": "off",
          "completions": { "lsp": true },
          "show_completions_on_input": true,
          "languages": {
            "Rust": {
              "tab_size": 7,
              "format_on_save": "on",
              "completions": { "lsp": false },
              "show_completions_on_input": false
            }
          }
        }"#,
    )
    .with_context(|| format!("write updated settings {}", local_settings_path.display()))?;
    let updated_expected = EffectiveLanguageSettings {
        tab_size: 7,
        format_on_save: "on".to_owned(),
        completion_lsp: false,
        show_completions_on_input: false,
    };
    let updated = wait_for_effective_language_settings(&buffer, &updated_expected, cx).await?;

    let language_server_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ready = services.project.read_with(cx, |project, cx| {
            project.language_server_statuses(cx).any(|(_, status)| {
                status.name.to_string() == "rust-analyzer" && status.process_id.is_some()
            })
        });
        if ready {
            break;
        }
        ensure!(
            Instant::now() < language_server_deadline,
            "rust-analyzer did not become ready for format-on-save"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
    tab.editor_window.update(cx, |editor, window, cx| {
        editor.select_all(&SelectAll, window, cx);
        editor.insert("fn main() { let value = 1; }   \n", window, cx);
    })?;
    save_tab(&tab, services, cx).await?;
    let formatted_disk = services
        .file_system
        .load(file_path)
        .await
        .with_context(|| format!("read format-on-save result {}", file_path.display()))?;
    ensure!(
        formatted_disk == "fn main() { let value = 1; }\n",
        "format-on-save result differs: {formatted_disk:?}"
    );

    std::fs::write(&local_settings_path, "{")
        .with_context(|| format!("write invalid settings {}", local_settings_path.display()))?;
    let invalid_settings_error =
        wait_for_configuration_result(&configuration_events, "project settings", false, cx).await?;
    let retained_after_invalid = effective_language_settings(&buffer, cx);
    ensure!(
        retained_after_invalid == updated_expected,
        "invalid project settings replaced the last valid settings"
    );

    std::fs::write(
        &local_settings_path,
        r#"{
          "tab_size": 8,
          "languages": {
            "Rust": {
              "tab_size": 9,
              "format_on_save": "off",
              "completions": { "lsp": true },
              "show_completions_on_input": true
            }
          }
        }"#,
    )
    .with_context(|| format!("restore settings {}", local_settings_path.display()))?;
    let recovered_expected = EffectiveLanguageSettings {
        tab_size: 9,
        format_on_save: "off".to_owned(),
        completion_lsp: true,
        show_completions_on_input: true,
    };
    let recovered = wait_for_effective_language_settings(&buffer, &recovered_expected, cx).await?;
    let recovery_status =
        wait_for_configuration_result(&configuration_events, "project settings", true, cx).await?;

    tab.editor_window.update(cx, |editor, window, cx| {
        editor.select_all(&SelectAll, window, cx);
        editor.insert("fn main() { let value = 2; }   \n", window, cx);
    })?;
    save_tab(&tab, services, cx).await?;
    let unformatted_disk = services
        .file_system
        .load(file_path)
        .await
        .with_context(|| format!("read format-off result {}", file_path.display()))?;

    Ok(serde_json::json!({
        "initial": initial,
        "updated": updated,
        "retained_after_invalid": retained_after_invalid,
        "recovered": recovered,
        "format_on_save_disk": formatted_disk,
        "format_off_disk": unformatted_disk,
        "settings": {
            "invalid_error": invalid_settings_error,
            "recovery_status": recovery_status,
            "path": local_settings_path.display().to_string(),
        },
        "keymap": {
            "initial_status": initial_keymap_status,
            "invalid_error": invalid_keymap_error,
            "replacement_status": replacement_keymap_status,
            "multi_chord_rebind": true,
            "unbind": true,
            "last_good_retained": true,
            "replacement_rebind": true,
            "path": keymap_path.display().to_string(),
        },
    }))
}

async fn alpha_2_language_service_probe(
    root_path: &Path,
    file_path: &Path,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    let repository = prepare_repository(root_path, services, cx).await?;
    trust_probe_worktrees(services, cx)?;
    let document = open_repository_document(file_path, &repository, services, cx).await?;
    let buffer = document.buffer.clone();
    let (event_sender, event_receiver) = async_channel::bounded(64);
    let tab = create_document_tab(document, services, event_sender.clone(), cx)?;
    let peer_path = file_path.with_file_name("lib.rs");
    let peer = if peer_path != file_path && peer_path.is_file() {
        let document = open_repository_document(&peer_path, &repository, services, cx).await?;
        let buffer = document.buffer.clone();
        let tab = create_document_tab(document, services, event_sender.clone(), cx)?;
        Some((peer_path, buffer, tab))
    } else {
        None
    };

    let text = buffer.read_with(cx, |buffer, _| buffer.text());
    let marker = "alpha_";
    let position = text
        .find(marker)
        .map(|offset| offset + marker.len())
        .context("Alpha 2 language-service probe file must contain alpha_")?;

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let ready = services.project.read_with(cx, |project, cx| {
            project.language_server_statuses(cx).any(|(_, status)| {
                status.name.to_string() == "rust-analyzer" && status.process_id.is_some()
            })
        });
        if ready {
            break;
        }
        if Instant::now() >= deadline {
            let language = buffer.read_with(cx, |buffer, _| {
                buffer.language().map(|language| language.name().clone())
            });
            let adapters = language
                .as_ref()
                .map(|language| {
                    services
                        .language_registry
                        .lsp_adapters(language)
                        .into_iter()
                        .map(|adapter| adapter.name().to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let statuses = services.project.read_with(cx, |project, cx| {
                project
                    .language_server_statuses(cx)
                    .map(|(_, status)| (status.name.to_string(), status.process_id))
                    .collect::<Vec<_>>()
            });
            bail!(
                "rust-analyzer did not become ready within 15 seconds; language={language:?}, adapters={adapters:?}, statuses={statuses:?}"
            );
        }
        cx.background_executor()
            .timer(Duration::from_millis(25))
            .await;
    }

    let completion_task = services.project.update(cx, |project, cx| {
        project.completions(
            &buffer,
            position,
            editor::CompletionContext {
                trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                trigger_character: None,
            },
            cx,
        )
    });
    let completion_responses = completion_task
        .await
        .context("request fixture completions")?;
    let completions = completion_responses
        .into_iter()
        .flat_map(|response| response.completions)
        .map(|completion| {
            let lsp_completion = completion.source.lsp_completion(false);
            serde_json::json!({
                "label": completion.label.text,
                "new_text": completion.new_text,
                "detail": lsp_completion.as_ref().and_then(|item| item.detail.clone()),
                "kind": lsp_completion
                    .as_ref()
                    .and_then(|item| item.kind)
                    .map(|kind| format!("{kind:?}")),
            })
        })
        .collect::<Vec<_>>();

    let hover_task = services
        .project
        .update(cx, |project, cx| project.hover(&buffer, position, cx));
    let hovers = hover_task
        .await
        .unwrap_or_default()
        .into_iter()
        .flat_map(|hover| hover.contents)
        .map(|block| {
            serde_json::json!({
                "kind": format!("{:?}", block.kind),
                "text": block.text,
            })
        })
        .collect::<Vec<_>>();

    let diagnostic_deadline = Instant::now() + Duration::from_secs(5);
    let diagnostic_summary = loop {
        let summary = services
            .project
            .read_with(cx, |project, cx| project.diagnostic_summary(false, cx));
        if summary.error_count > 0 || summary.warning_count > 0 {
            break summary;
        }
        ensure!(
            Instant::now() < diagnostic_deadline,
            "fixture diagnostic was not published within 5 seconds"
        );
        cx.background_executor()
            .timer(Duration::from_millis(25))
            .await;
    };

    let statuses = services.project.read_with(cx, |project, cx| {
        project
            .language_server_statuses(cx)
            .map(|(id, status)| {
                serde_json::json!({
                    "id": id.0,
                    "name": status.name.to_string(),
                    "language": status.language_name.as_ref().map(ToString::to_string),
                    "process_id": status.process_id,
                })
            })
            .collect::<Vec<_>>()
    });
    let language = buffer.read_with(cx, |buffer, _| {
        buffer
            .language()
            .map(|language| language.name().to_string())
    });

    let prefix = &text[..position];
    let display_row = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let byte_column = prefix
        .rfind('\n')
        .map_or(position, |newline| position.saturating_sub(newline + 1));
    move_caret_to_text_position(
        &tab.editor_window,
        TextPosition {
            row: display_row,
            byte_column,
        },
        cx,
    )?;
    let terminal_generation = tab
        .completion_generation
        .load(AtomicOrdering::SeqCst)
        .saturating_add(1);
    tab.editor_window.update(cx, |editor, window, cx| {
        editor.show_completions(&ShowCompletions, window, cx)
    })?;
    let terminal_deadline = Instant::now() + Duration::from_secs(5);
    let terminal_items = loop {
        match event_receiver.try_recv() {
            Ok(TerminalEvent::CompletionFinished {
                buffer_id: _,
                generation,
                result,
                ..
            }) if generation == terminal_generation => break result.map_err(anyhow::Error::msg)?,
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("terminal completion event channel closed")
            }
        }
        ensure!(
            Instant::now() < terminal_deadline,
            "terminal completion projection was not published within 5 seconds"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    };
    loop {
        let menu_visible = tab.editor_window.update(cx, |editor, _window, _cx| {
            editor.has_visible_completions_menu()
        })?;
        if menu_visible {
            break;
        }
        ensure!(
            Instant::now() < terminal_deadline,
            "Zed completion menu was not ready within 5 seconds"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
    let confirm_task = tab.editor_window.update(cx, |editor, window, cx| {
        editor.confirm_completion(&ConfirmCompletion { item_ix: Some(0) }, window, cx)
    })?;
    confirm_task
        .context("Zed completion menu had no first item")?
        .await
        .context("apply terminal-projected completion")?;
    let text_after_completion = tab
        .editor_window
        .update(cx, |editor, _window, cx| editor.text(cx))?;
    tab.editor_window
        .update(cx, |editor, window, cx| editor.undo(&Undo, window, cx))?;
    let text_after_completion_undo = tab
        .editor_window
        .update(cx, |editor, _window, cx| editor.text(cx))?;

    let terminal_hover_generation = 1;
    let terminal_hover_buffer_id = start_hover_request(
        &tab.editor_window,
        &services.project,
        terminal_hover_generation,
        event_sender.clone(),
        cx,
    )?;
    let terminal_hover_deadline = Instant::now() + Duration::from_secs(5);
    let terminal_hover_items = loop {
        match event_receiver.try_recv() {
            Ok(TerminalEvent::HoverFinished {
                buffer_id,
                generation,
                result,
            }) if buffer_id == terminal_hover_buffer_id
                && generation == terminal_hover_generation =>
            {
                break result.map_err(anyhow::Error::msg)?;
            }
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("terminal hover event channel closed")
            }
        }
        ensure!(
            Instant::now() < terminal_hover_deadline,
            "terminal hover projection was not published within 5 seconds"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    };
    let mut terminal_hover_prompt =
        HoverPrompt::running(terminal_hover_buffer_id, terminal_hover_generation);
    ensure!(terminal_hover_prompt.complete(
        terminal_hover_buffer_id,
        terminal_hover_generation,
        Ok(terminal_hover_items.clone())
    ));
    let terminal_hover_overlay = terminal_hover_prompt.overlay();

    let terminal_diagnostic_items = collect_project_diagnostics(
        services.project.clone(),
        Some(root_path.to_path_buf()),
        vec![(file_path.to_path_buf(), buffer.clone())],
        cx,
    )
    .await?;
    let mut terminal_diagnostics_prompt = DiagnosticsPrompt::running(1);
    ensure!(terminal_diagnostics_prompt.complete(1, Ok(terminal_diagnostic_items.clone())));
    let terminal_diagnostics_overlay = terminal_diagnostics_prompt.overlay();
    let selected_diagnostic = terminal_diagnostics_prompt
        .selected_item()
        .context("fixture produced no terminal diagnostic")?;
    let mut probe_tabs = vec![tab];
    let peer_buffer = peer
        .as_ref()
        .map(|(path, buffer, _)| (path.clone(), buffer.clone()));
    if let Some((_, _, peer_tab)) = peer {
        probe_tabs.push(peer_tab);
    }
    let mut probe_active_index = 0;
    let diagnostic_navigation = navigate_to_diagnostic(
        &selected_diagnostic,
        Some(&repository),
        services,
        &mut probe_tabs,
        &mut probe_active_index,
        event_sender.clone(),
        cx,
    )
    .await?;
    let (_, diagnostic_point, _) =
        active_editor_buffer_point(&probe_tabs[probe_active_index].editor_window, cx)?;

    let mut terminal_location_results = Vec::new();
    for (index, kind) in [
        LocationRequestKind::Definition,
        LocationRequestKind::TypeDefinition,
        LocationRequestKind::References,
    ]
    .into_iter()
    .enumerate()
    {
        let generation = u64::try_from(index).unwrap_or_default().saturating_add(10);
        let buffer_id = start_locations_request(
            kind,
            &probe_tabs[probe_active_index].editor_window,
            &services.project,
            generation,
            Some(root_path.to_path_buf()),
            event_sender.clone(),
            cx,
        )?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let items = loop {
            match event_receiver.try_recv() {
                Ok(TerminalEvent::LocationsFinished {
                    buffer_id: completed_buffer_id,
                    generation: completed_generation,
                    kind: completed_kind,
                    result,
                }) if completed_buffer_id == buffer_id
                    && completed_generation == generation
                    && completed_kind == kind =>
                {
                    break result.map_err(anyhow::Error::msg)?;
                }
                Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
                Err(async_channel::TryRecvError::Closed) => {
                    bail!("terminal location event channel closed")
                }
            }
            ensure!(
                Instant::now() < deadline,
                "terminal {} projection was not published within 5 seconds",
                kind.title().to_lowercase()
            );
            cx.background_executor()
                .timer(Duration::from_millis(10))
                .await;
        };
        let mut prompt = LocationsPrompt::running(buffer_id, generation, kind);
        ensure!(prompt.complete(buffer_id, generation, kind, Ok(items.clone())));
        let overlay_rows = prompt
            .overlay()
            .rows
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>();
        terminal_location_results.push((kind, items, overlay_rows));
    }
    let symbol_generation = 20;
    let (_, _, symbol_buffer_id) =
        active_editor_buffer_point(&probe_tabs[probe_active_index].editor_window, cx)?;
    start_project_symbols_request(
        &services.project,
        symbol_buffer_id,
        symbol_generation,
        "alpha".to_owned(),
        Some(root_path.to_path_buf()),
        event_sender.clone(),
        cx,
    );
    let symbol_deadline = Instant::now() + Duration::from_secs(5);
    let symbol_items = loop {
        match event_receiver.try_recv() {
            Ok(TerminalEvent::LocationsFinished {
                buffer_id,
                generation,
                kind: LocationRequestKind::ProjectSymbols,
                result,
            }) if buffer_id == symbol_buffer_id && generation == symbol_generation => {
                break result.map_err(anyhow::Error::msg)?;
            }
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("terminal project-symbol event channel closed")
            }
        }
        ensure!(
            Instant::now() < symbol_deadline,
            "terminal project symbols were not published within 5 seconds"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    };
    let mut symbol_prompt = LocationsPrompt::running(
        symbol_buffer_id,
        symbol_generation,
        LocationRequestKind::ProjectSymbols,
    );
    ensure!(symbol_prompt.complete(
        symbol_buffer_id,
        symbol_generation,
        LocationRequestKind::ProjectSymbols,
        Ok(symbol_items.clone())
    ));
    terminal_location_results.push((
        LocationRequestKind::ProjectSymbols,
        symbol_items,
        symbol_prompt
            .overlay()
            .rows
            .into_iter()
            .map(|row| row.text)
            .collect(),
    ));
    let definition_target = terminal_location_results
        .iter()
        .find(|(kind, _, _)| *kind == LocationRequestKind::Definition)
        .and_then(|(_, items, _)| items.first())
        .context("fixture produced no definition target")?;
    let semantic_navigation = navigate_to_location(
        definition_target,
        Some(&repository),
        services,
        &mut probe_tabs,
        &mut probe_active_index,
        event_sender.clone(),
        cx,
    )
    .await?;
    let (_, semantic_point, _) =
        active_editor_buffer_point(&probe_tabs[probe_active_index].editor_window, cx)?;

    let reference_items = terminal_location_results
        .iter()
        .find(|(kind, _, _)| *kind == LocationRequestKind::References)
        .map(|(_, items, _)| items.clone())
        .context("fixture produced no reference targets")?;
    let multibuffer_tab = create_locations_multibuffer_tab(
        "References".to_owned(),
        &reference_items,
        Some(&repository),
        services,
        event_sender.clone(),
        cx,
    )
    .await?;
    let multibuffer_source_count = multibuffer_tab
        .multi_buffer
        .as_ref()
        .context("reference result did not create a MultiBuffer tab")?
        .buffer
        .read_with(cx, |multi_buffer, _| {
            multi_buffer.all_buffers_iter().count()
        });
    let multibuffer_text = multibuffer_tab
        .editor_window
        .update(cx, |editor, _window, cx| editor.text(cx))?;
    let multibuffer_marker = multibuffer_text
        .find("alpha_")
        .context("reference MultiBuffer omitted the fixture symbol")?;
    let multibuffer_prefix = &multibuffer_text[..multibuffer_marker];
    let multibuffer_row = multibuffer_prefix
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let multibuffer_column = multibuffer_prefix
        .rfind('\n')
        .map_or(multibuffer_marker, |newline| {
            multibuffer_marker.saturating_sub(newline + 1)
        });
    let multibuffer_before = multibuffer_tab
        .multi_buffer
        .as_ref()
        .expect("MultiBuffer checked above")
        .source_buffers
        .iter()
        .map(|buffer| {
            buffer.read_with(cx, |buffer, cx| {
                let path = buffer
                    .file()
                    .map(|file| {
                        file.as_local()
                            .map(|file| file.abs_path(cx))
                            .unwrap_or_else(|| file.full_path(cx))
                    })
                    .unwrap_or_default();
                (path, buffer.text())
            })
        })
        .collect::<BTreeMap<_, _>>();
    move_caret_to_text_position(
        &multibuffer_tab.editor_window,
        TextPosition {
            row: multibuffer_row,
            byte_column: multibuffer_column,
        },
        cx,
    )?;
    multibuffer_tab
        .editor_window
        .update(cx, |editor, window, cx| editor.insert("mb_", window, cx))?;
    let multibuffer_after_edit = multibuffer_tab
        .multi_buffer
        .as_ref()
        .expect("MultiBuffer checked above")
        .source_buffers
        .iter()
        .map(|buffer| {
            buffer.read_with(cx, |buffer, cx| {
                let path = buffer
                    .file()
                    .map(|file| {
                        file.as_local()
                            .map(|file| file.abs_path(cx))
                            .unwrap_or_else(|| file.full_path(cx))
                    })
                    .unwrap_or_default();
                (path, buffer.text())
            })
        })
        .collect::<BTreeMap<_, _>>();
    let changed_multibuffer_paths = multibuffer_after_edit
        .iter()
        .filter_map(|(path, text)| {
            (multibuffer_before.get(path) != Some(text)).then_some(path.clone())
        })
        .collect::<Vec<_>>();
    ensure!(
        changed_multibuffer_paths.len() == 1,
        "MultiBuffer edit changed {:?}, expected exactly one source",
        changed_multibuffer_paths
    );
    let changed_multibuffer_path = changed_multibuffer_paths[0].clone();
    save_tab(&multibuffer_tab, services, cx).await?;
    let multibuffer_disk_after_save = std::fs::read_to_string(&changed_multibuffer_path)
        .with_context(|| {
            format!(
                "read {} after MultiBuffer save",
                changed_multibuffer_path.display()
            )
        })?;
    multibuffer_tab
        .editor_window
        .update(cx, |editor, window, cx| editor.undo(&Undo, window, cx))?;
    let multibuffer_after_undo = multibuffer_tab
        .multi_buffer
        .as_ref()
        .expect("MultiBuffer checked above")
        .source_buffers
        .iter()
        .map(|buffer| {
            buffer.read_with(cx, |buffer, cx| {
                let path = buffer
                    .file()
                    .map(|file| {
                        file.as_local()
                            .map(|file| file.abs_path(cx))
                            .unwrap_or_else(|| file.full_path(cx))
                    })
                    .unwrap_or_default();
                (path, buffer.text())
            })
        })
        .collect::<BTreeMap<_, _>>();
    ensure!(
        multibuffer_after_undo == multibuffer_before,
        "MultiBuffer undo did not restore every source buffer"
    );
    save_tab(&multibuffer_tab, services, cx).await?;
    let multibuffer_disk_after_restore = std::fs::read_to_string(&changed_multibuffer_path)
        .with_context(|| {
            format!(
                "read {} after MultiBuffer restore",
                changed_multibuffer_path.display()
            )
        })?;
    let multibuffer_changed_label = changed_multibuffer_path
        .strip_prefix(root_path)
        .unwrap_or(&changed_multibuffer_path)
        .to_string_lossy()
        .into_owned();
    probe_tabs.push(multibuffer_tab);
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;

    move_caret_to_text_position(
        &probe_tabs[0].editor_window,
        TextPosition {
            row: display_row,
            byte_column,
        },
        cx,
    )?;
    let rename_generation = 30;
    let (rename_buffer, rename_point, rename_buffer_id) = start_rename_request(
        &probe_tabs[0].editor_window,
        &services.project,
        rename_generation,
        event_sender.clone(),
        cx,
    )?;
    let rename_deadline = Instant::now() + Duration::from_secs(5);
    let rename_preparation = loop {
        match event_receiver.try_recv() {
            Ok(TerminalEvent::RenamePrepared {
                buffer_id,
                generation,
                result,
            }) if buffer_id == rename_buffer_id && generation == rename_generation => {
                break result.map_err(anyhow::Error::msg)?;
            }
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("terminal rename event channel closed")
            }
        }
        ensure!(
            Instant::now() < rename_deadline,
            "terminal rename preparation was not published within 5 seconds"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    };
    let mut rename_prompt = RenamePrompt::running(
        rename_buffer.clone(),
        rename_buffer_id,
        rename_generation,
        rename_point,
    );
    ensure!(rename_prompt.complete(
        rename_buffer_id,
        rename_generation,
        Ok(rename_preparation.clone())
    ));
    let rename_overlay_rows = rename_prompt
        .overlay()
        .rows
        .into_iter()
        .map(|row| row.text)
        .collect::<Vec<_>>();
    let rename_name = "renamed_fixture".to_owned();
    let rename_main_before_preview = buffer.read_with(cx, |buffer, _| buffer.text());
    let rename_peer_before_preview = peer_buffer
        .as_ref()
        .map(|(_, buffer)| buffer.read_with(cx, |buffer, _| buffer.text()));

    let (rejected_server_id, rejected_request) = request_rename_workspace_edit(
        &services.project,
        &rename_buffer,
        rename_point,
        rename_name.clone(),
        None,
        cx,
    )?;
    let rejected_edit = rejected_request.await?;
    let rejected_preview = create_rename_preview_tab(
        rename_buffer.clone(),
        rename_point,
        rename_name.clone(),
        rejected_server_id,
        rejected_edit,
        repository.root.canonical_path().to_path_buf(),
        Some(&repository),
        services,
        cx,
    )
    .await?;
    let rejected_preview_text = rejected_preview
        .editor_window
        .update(cx, |editor, _window, cx| editor.text(cx))?;
    rejected_preview
        .editor_window
        .update(cx, |_editor, window, _cx| window.remove_window())?;
    drop(rejected_preview);
    let rename_rejection_unchanged = buffer.read_with(cx, |buffer, _| buffer.text())
        == rename_main_before_preview
        && peer_buffer
            .as_ref()
            .map(|(_, buffer)| buffer.read_with(cx, |buffer, _| buffer.text()))
            == rename_peer_before_preview;
    ensure!(
        rename_rejection_unchanged,
        "rejecting the rename preview changed a source buffer"
    );

    let (preview_server_id, preview_request) = request_rename_workspace_edit(
        &services.project,
        &rename_buffer,
        rename_point,
        rename_name.clone(),
        None,
        cx,
    )?;
    let preview_edit = preview_request.await?;
    let rename_preview_tab = create_rename_preview_tab(
        rename_buffer.clone(),
        rename_point,
        rename_name.clone(),
        preview_server_id,
        preview_edit,
        repository.root.canonical_path().to_path_buf(),
        Some(&repository),
        services,
        cx,
    )
    .await?;
    let rename_preview_text = rename_preview_tab
        .editor_window
        .update(cx, |editor, _window, cx| editor.text(cx))?;
    let rename_pending = rename_preview_tab
        .multi_buffer
        .as_ref()
        .and_then(|multi_buffer| multi_buffer.pending_rename.clone())
        .context("rename preview tab has no pending transaction")?;
    let rename_preview_source_count = rename_preview_tab
        .multi_buffer
        .as_ref()
        .map(|multi_buffer| multi_buffer.source_buffers.len())
        .unwrap_or_default();
    let rename_preview_buffer_count = rename_preview_tab
        .multi_buffer
        .as_ref()
        .map(|multi_buffer| {
            multi_buffer
                .buffer
                .read_with(cx, |buffer, _| buffer.all_buffers_iter().count())
        })
        .unwrap_or_default();
    let rename_preview_read_only =
        rename_preview_tab
            .multi_buffer
            .as_ref()
            .is_some_and(|multi_buffer| {
                multi_buffer
                    .buffer
                    .read_with(cx, |buffer, _| buffer.capability() == Capability::ReadOnly)
            });
    validate_pending_rename_guards(&rename_pending, cx)?;
    let (_, confirmation_request) = request_rename_workspace_edit(
        &services.project,
        &rename_pending.origin_buffer,
        rename_pending.origin_point,
        rename_pending.new_name.clone(),
        Some(rename_pending.language_server_id),
        cx,
    )?;
    let confirmation_edit = confirmation_request.await?;
    let confirmation_plan =
        normalize_rename_workspace_edit(&confirmation_edit, &rename_pending.workspace_root)?;
    ensure!(
        confirmation_plan.signature == rename_pending.plan.signature,
        "fixture rename changed between preview and acceptance"
    );
    validate_pending_rename_guards(&rename_pending, cx)?;
    let rename_transaction = services.project.update(cx, |project, cx| {
        project.perform_rename(
            rename_pending.origin_buffer.clone(),
            rename_pending.origin_point,
            rename_name.clone(),
            cx,
        )
    });
    let rename_transaction = rename_transaction
        .await
        .context("perform fixture multi-buffer rename")?;
    let rename_buffer_count = rename_transaction.0.len();
    rename_preview_tab
        .editor_window
        .update(cx, |_editor, window, _cx| window.remove_window())?;
    let rename_main_after = buffer.read_with(cx, |buffer, _| buffer.text());
    let rename_peer_after = peer_buffer
        .as_ref()
        .map(|(_, buffer)| buffer.read_with(cx, |buffer, _| buffer.text()));
    let mut rename_history = ProjectEditHistory::default();
    ensure!(
        rename_history.push(rename_transaction) == rename_buffer_count,
        "rename history buffer count changed"
    );
    let rename_undo_count = rename_history.undo_latest(cx)?;
    let rename_main_after_undo = buffer.read_with(cx, |buffer, _| buffer.text());
    let rename_peer_after_undo = peer_buffer
        .as_ref()
        .map(|(_, buffer)| buffer.read_with(cx, |buffer, _| buffer.text()));
    let rename_redo_count = rename_history.redo_latest(cx)?;
    let rename_main_after_redo = buffer.read_with(cx, |buffer, _| buffer.text());
    rename_history.undo_latest(cx)?;
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;

    move_caret_to_text_position(
        &probe_tabs[0].editor_window,
        TextPosition {
            row: display_row,
            byte_column,
        },
        cx,
    )?;
    let code_action_generation = 31;
    let (code_action_buffer, code_action_buffer_id) = start_code_actions_request(
        &probe_tabs[0].editor_window,
        &services.project,
        code_action_generation,
        event_sender.clone(),
        cx,
    )?;
    let code_action_deadline = Instant::now() + Duration::from_secs(5);
    let code_actions = loop {
        match event_receiver.try_recv() {
            Ok(TerminalEvent::CodeActionsFinished {
                buffer_id,
                generation,
                result,
            }) if buffer_id == code_action_buffer_id && generation == code_action_generation => {
                break result.map_err(anyhow::Error::msg)?;
            }
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("terminal code-action event channel closed")
            }
        }
        ensure!(
            Instant::now() < code_action_deadline,
            "terminal code actions were not published within 5 seconds"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    };
    let mut code_action_prompt = CodeActionsPrompt::running(
        code_action_buffer.clone(),
        code_action_buffer_id,
        code_action_generation,
    );
    ensure!(code_action_prompt.complete(
        code_action_buffer_id,
        code_action_generation,
        Ok(code_actions)
    ));
    let code_action_overlay_rows = code_action_prompt
        .overlay()
        .rows
        .into_iter()
        .map(|row| row.text)
        .collect::<Vec<_>>();
    let code_action = code_action_prompt
        .selected_action()
        .context("fixture produced no enabled code action")?;
    let applied_code_action_title = code_action_title(&code_action).to_owned();
    let applied_code_action_kind = code_action_kind(&code_action);
    let applied_code_action_preferred = code_action_preferred(&code_action);
    let code_action_transaction = services.project.update(cx, |project, cx| {
        project.apply_code_action(code_action_buffer, code_action, true, cx)
    });
    let code_action_transaction = code_action_transaction
        .await
        .context("apply fixture code action")?;
    let code_action_buffer_count = code_action_transaction.0.len();
    let code_action_main_after = buffer.read_with(cx, |buffer, _| buffer.text());
    let mut code_action_history = ProjectEditHistory::default();
    code_action_history.push(code_action_transaction);
    let code_action_undo_count = code_action_history.undo_latest(cx)?;
    let code_action_main_after_undo = buffer.read_with(cx, |buffer, _| buffer.text());
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;

    let format_document_transaction =
        format_active_editor(&probe_tabs[0].editor_window, &services.project, false, cx)?
            .await
            .context("format fixture document")?;
    let format_document_buffer_count = format_document_transaction.0.len();
    let format_document_after = buffer.read_with(cx, |buffer, _| buffer.text());
    let mut format_history = ProjectEditHistory::default();
    format_history.push(format_document_transaction);
    let format_document_undo_count = if format_history.can_undo() {
        format_history.undo_latest(cx)?
    } else {
        0
    };
    let format_document_after_undo = buffer.read_with(cx, |buffer, _| buffer.text());
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;

    probe_tabs[0]
        .editor_window
        .update(cx, |editor, window, cx| {
            let display = editor.display_snapshot(cx);
            let start = display.display_point_to_anchor(
                display.clip_point(DisplayPoint::new(DisplayRow(0), 0), Bias::Left),
                Bias::Left,
            );
            let end = display.display_point_to_anchor(
                display.clip_point(DisplayPoint::new(DisplayRow(2), 0), Bias::Right),
                Bias::Right,
            );
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_anchor_ranges([start..end])
            });
        })?;
    let format_range_transaction =
        format_active_editor(&probe_tabs[0].editor_window, &services.project, true, cx)?
            .await
            .context("format fixture selection")?;
    let format_range_buffer_count = format_range_transaction.0.len();
    let format_range_after = buffer.read_with(cx, |buffer, _| buffer.text());
    let mut format_range_history = ProjectEditHistory::default();
    format_range_history.push(format_range_transaction);
    let format_range_undo_count = if format_range_history.can_undo() {
        format_range_history.undo_latest(cx)?
    } else {
        0
    };
    let format_range_after_undo = buffer.read_with(cx, |buffer, _| buffer.text());

    let terminal_locations_json = terminal_location_results
        .iter()
        .map(|(kind, items, overlay_rows)| {
            let key = match kind {
                LocationRequestKind::Definition => "definitions",
                LocationRequestKind::TypeDefinition => "type_definitions",
                LocationRequestKind::References => "references",
                LocationRequestKind::ProjectSymbols => "project_symbols",
            };
            (
                key.to_owned(),
                serde_json::json!({
                    "items": items.iter().map(|item| serde_json::json!({
                        "path": item.path,
                        "label": item.label,
                        "row": item.row,
                        "column": item.column,
                        "end_row": item.end_row,
                        "end_column": item.end_column,
                        "snippet": item.snippet,
                    })).collect::<Vec<_>>(),
                    "overlay_rows": overlay_rows,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();

    let report = serde_json::json!({
        "project_entity": format!("{:?}", services.project.entity_id()),
        "buffer_entity": format!("{:?}", buffer.entity_id()),
        "language": language,
        "servers": statuses,
        "completions": completions,
        "hover": hovers,
        "diagnostics": {
            "errors": diagnostic_summary.error_count,
            "warnings": diagnostic_summary.warning_count,
        },
        "terminal_completion": {
            "items": terminal_items
                .iter()
                .map(|item| serde_json::json!({
                    "label": item.label,
                    "detail": item.detail,
                    "kind": item.kind,
                    "documentation": item.documentation,
                }))
                .collect::<Vec<_>>(),
            "text_after_apply": text_after_completion,
            "text_after_undo": text_after_completion_undo,
        },
        "terminal_hover": {
            "items": terminal_hover_items
                .iter()
                .map(|item| serde_json::json!({
                    "kind": item.kind,
                    "text": item.text,
                }))
                .collect::<Vec<_>>(),
            "overlay_rows": terminal_hover_overlay
                .rows
                .iter()
                .map(|row| row.text.clone())
                .collect::<Vec<_>>(),
        },
        "terminal_diagnostics": {
            "items": terminal_diagnostic_items
                .iter()
                .map(|item| serde_json::json!({
                    "path": item.path,
                    "label": item.label,
                    "row": item.row,
                    "column": item.column,
                    "severity": item.severity,
                    "message": item.message,
                    "source": item.source,
                }))
                .collect::<Vec<_>>(),
            "overlay_rows": terminal_diagnostics_overlay
                .rows
                .iter()
                .map(|row| row.text.clone())
                .collect::<Vec<_>>(),
            "navigation": diagnostic_navigation,
            "cursor": {
                "row": diagnostic_point.row,
                "column": diagnostic_point.column,
            },
        },
        "terminal_locations": terminal_locations_json,
        "terminal_multibuffer": {
            "title": "References",
            "target_count": reference_items.len(),
            "source_count": multibuffer_source_count,
            "snapshot_contains_main": multibuffer_text.contains("fn main"),
            "snapshot_contains_peer": multibuffer_text.contains("fixture_peer"),
            "changed_path": multibuffer_changed_label,
            "source_before": multibuffer_before.get(&changed_multibuffer_path),
            "source_after_edit": multibuffer_after_edit.get(&changed_multibuffer_path),
            "disk_after_save": multibuffer_disk_after_save,
            "source_after_undo": multibuffer_after_undo.get(&changed_multibuffer_path),
            "disk_after_restore": multibuffer_disk_after_restore,
        },
        "semantic_navigation": {
            "message": semantic_navigation,
            "cursor": {
                "row": semantic_point.row,
                "column": semantic_point.column,
            },
        },
        "terminal_edits": {
            "rename": {
                "preparation": {
                    "placeholder": rename_preparation.placeholder,
                    "start": rename_preparation.start,
                    "end": rename_preparation.end,
                },
                "overlay_rows": rename_overlay_rows,
                "preview": {
                    "read_only": rename_preview_read_only,
                    "source_count": rename_preview_source_count,
                    "buffer_count": rename_preview_buffer_count,
                    "edit_count": rename_pending.plan.edit_count,
                    "file_operation_count": rename_pending.plan.file_operation_count,
                    "signature": rename_pending.plan.signature,
                    "confirmation_signature_matches": confirmation_plan.signature
                        == rename_pending.plan.signature,
                    "contains_main": rename_preview_text.contains("src/main.rs"),
                    "contains_peer": rename_preview_text.contains("src/lib.rs"),
                    "contains_old_text": rename_preview_text.contains("alpha_"),
                    "contains_new_text": rename_preview_text.contains("renamed_fixture"),
                    "rejected_contains_new_text": rejected_preview_text.contains("renamed_fixture"),
                    "rejection_unchanged": rename_rejection_unchanged,
                },
                "buffer_count": rename_buffer_count,
                "undo_buffer_count": rename_undo_count,
                "redo_buffer_count": rename_redo_count,
                "main_after": rename_main_after,
                "peer_after": rename_peer_after,
                "main_after_undo": rename_main_after_undo,
                "peer_after_undo": rename_peer_after_undo,
                "main_after_redo": rename_main_after_redo,
            },
            "code_action": {
                "title": applied_code_action_title,
                "kind": applied_code_action_kind,
                "preferred": applied_code_action_preferred,
                "overlay_rows": code_action_overlay_rows,
                "buffer_count": code_action_buffer_count,
                "undo_buffer_count": code_action_undo_count,
                "main_after": code_action_main_after,
                "main_after_undo": code_action_main_after_undo,
            },
            "format_document": {
                "buffer_count": format_document_buffer_count,
                "undo_buffer_count": format_document_undo_count,
                "after": format_document_after,
                "after_undo": format_document_after_undo,
            },
            "format_range": {
                "buffer_count": format_range_buffer_count,
                "undo_buffer_count": format_range_undo_count,
                "after": format_range_after,
                "after_undo": format_range_after_undo,
            },
        },
    });

    // Releasing the project-backed editor releases Zed's OpenLspBufferHandle.
    // A project-less keepalive window prevents GPUI from terminating when the
    // probe closes its only real editor, so didClose can reach the server before
    // the outer runner initiates shutdown.
    let _keepalive_window = cx.update(|cx| open_editor(buffer.clone(), cx))?;
    for tab in &probe_tabs {
        tab.editor_window
            .update(cx, |_editor, window, _cx| window.remove_window())?;
    }
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;

    Ok(report)
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
            let output =
                collect_project_search(&repository, query, services, Vec::new(), cx).await?;
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
    open_buffers: Vec<Entity<Buffer>>,
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
    let command =
        start_zed_project_search_command(request, repository, services, open_buffers, cx)?;
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
                services,
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
        project_searchable_buffers(&tabs),
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
        let recording = ZecFs::isolated_recording(real);
        let services = file_services_with_fs(cx, recording.clone(), false);
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
    let command_a =
        start_zed_project_search_command(request_a, &repository, services, Vec::new(), cx)?;
    let request_b = scheduler
        .request(
            prompt.request("ALPHA1_STALE_B".to_owned())?,
            ProjectSearchChange::Paste,
        )?
        .next
        .context("query B did not start")?;
    let command_b =
        start_zed_project_search_command(request_b, &repository, services, Vec::new(), cx)?;

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
    let tabs = vec![create_document_tab(document, services, redraw_sender, cx)?];
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
            None,
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
        #[cfg(unix)]
        root_alias: PathBuf,
        #[cfg(unix)]
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
            #[cfg(unix)]
            let root_alias = directory.join("repo-alias");
            #[cfg(unix)]
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
            #[cfg(unix)]
            std::fs::write(&outside, "outside control\n").expect("write outside file");
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink("../src/日本 語.rs", root.join("aliases/日本 語.rs"))
                    .expect("create file alias");
                std::os::unix::fs::symlink("root-loop", root.join("root-loop"))
                    .expect("create self-referential symlink");
                std::os::unix::fs::symlink(&root, &root_alias).expect("create root alias");
            }
            Self {
                directory,
                root,
                #[cfg(unix)]
                root_alias,
                #[cfg(unix)]
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
    fn project_search_coordinator_drop_cancels_fresh_handle_and_detaches_task() {
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        gpui_platform::headless().run(move |cx| {
            cx.spawn(async move |cx| {
                let result: Result<()> = async {
                    let mut coordinator = ProjectSearchCoordinator::default();
                    let session = coordinator.open_session().context("search session")?;
                    let mut prompt = ProjectSearchPrompt::new(session);
                    let request = coordinator
                        .scheduler
                        .request(
                            prompt.request("fresh".to_owned())?,
                            ProjectSearchChange::Paste,
                        )?
                        .next
                        .context("fresh search")?;
                    let (release_sender, release_receiver) = async_channel::bounded(1);
                    let (finished_sender, finished_receiver) = async_channel::bounded(1);
                    let live_task = cx.spawn(async move |_cx| {
                        let _ = release_receiver.recv().await;
                        let _ = finished_sender.send(()).await;
                    });
                    let cancellation = RunningLiteralSearchCancellation::test_probe();
                    let cancellation_probe = cancellation.clone();
                    coordinator.attach(request.key, live_task, Some(cancellation))?;
                    ensure!(
                        !cancellation_probe.is_cancelled(),
                        "fresh handle was cancelled"
                    );

                    drop(coordinator);
                    ensure!(
                        cancellation_probe.is_cancelled(),
                        "coordinator drop did not cancel fresh handle"
                    );
                    ensure!(
                        !release_sender.is_closed(),
                        "coordinator drop dropped instead of detaching live task"
                    );
                    release_sender.send(()).await.context("release live task")?;
                    finished_receiver
                        .recv()
                        .await
                        .context("detached live task did not finish")?;
                    Ok(())
                }
                .await;
                result_sender.send(result).expect("send fresh-drop result");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });
        result_receiver
            .recv()
            .expect("receive fresh-drop result")
            .expect("fresh coordinator drop lifecycle");
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

                    let key_edit = prompt.request("key-edit".to_owned())?;
                    let (key_sender, _key_receiver) = async_channel::unbounded();
                    ensure!(
                        coordinator
                            .schedule(key_edit, ProjectSearchChange::Key, key_sender, cx)?
                            .is_none(),
                        "key edit dispatched before its debounce"
                    );
                    ensure!(
                        cancellation_probe.is_cancelled(),
                        "key edit did not cancel the old run before debounce"
                    );
                    ensure!(coordinator.task_count() == 1, "key edit lost the old task");

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

        let absolute = std::env::temp_dir().join("zec-save-as.rs");
        assert_eq!(resolve_path(absolute.to_str().unwrap()).unwrap(), absolute);
        assert!(
            resolve_path("~/notes.txt")
                .unwrap()
                .ends_with("~/notes.txt")
        );
        assert!(resolve_path("").is_err());
    }

    #[test]
    fn project_search_uses_dirty_open_buffer_as_snapshot_authority() {
        let fixture = TemporaryRepository::new();
        let root = fixture.root.clone();
        let dirty_path = root.join("dirty-authority.txt");
        std::fs::write(&dirty_path, "DISK_ONLY\n").expect("write dirty authority fixture");
        let (sender, receiver) = mpsc::sync_channel(1);

        gpui_platform::headless().run(move |cx| {
            init_zed(cx);
            let services = file_services(cx);
            cx.spawn(async move |cx| {
                let result: Result<_> = async {
                    let repository = prepare_repository(&root, &services, cx).await?;
                    let document = open_repository_document(
                        Path::new("dirty-authority.txt"),
                        &repository,
                        &services,
                        cx,
                    )
                    .await?;
                    document.buffer.update(cx, |buffer, cx| {
                        let len = buffer.len();
                        buffer.edit([(0..len, "DIRTY_AUTHORITY café\n")], None, cx);
                    });
                    ensure!(document_state(&document, cx).dirty, "buffer is not dirty");

                    let index = repository.index.clone();
                    let file_system = services.file_system.clone();
                    let buffer_store = services.buffer_store.clone();
                    let running = cx.update(|cx| {
                        start_literal_project_search(
                            "DIRTY_AUTHORITY café",
                            index,
                            file_system,
                            buffer_store,
                            vec![document.buffer.clone()],
                            cx,
                        )
                    })?;
                    let output = running.collect(cx).await?;
                    let rows = output
                        .matches
                        .iter()
                        .map(|hit| {
                            (
                                hit.summary.path.to_string(),
                                hit.summary.preview.clone(),
                                hit.byte_range.clone(),
                            )
                        })
                        .collect::<Vec<_>>();
                    let disk = services.file_system.load(&dirty_path).await?.to_string();
                    Ok((output.total_hits, output.source_limit_reached, rows, disk))
                }
                .await;
                sender.send(result).expect("send dirty authority result");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });

        let (total, source_limit, rows, disk) = receiver
            .recv()
            .expect("receive dirty authority result")
            .expect("dirty authority search");
        assert_eq!(total, 1);
        assert!(!source_limit);
        assert_eq!(
            rows,
            vec![(
                "dirty-authority.txt".to_owned(),
                "DIRTY_AUTHORITY café".to_owned(),
                0.."DIRTY_AUTHORITY café".len(),
            )]
        );
        assert_eq!(disk, "DISK_ONLY\n");
    }

    #[cfg(unix)]
    #[test]
    fn repository_startup_uses_one_root_identity_and_exact_outside_worktree() {
        let fixture = TemporaryRepository::new();
        let root = fixture.root.clone();
        let root_alias = fixture.root_alias.clone();
        let outside = fixture.outside.clone();
        let expected_root = std::fs::canonicalize(&root).expect("canonicalize fixture root");
        let (sender, receiver) = mpsc::sync_channel(1);

        editor_application().run(move |cx| {
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

    #[cfg_attr(
        windows,
        ignore = "pinned GPUI Windows backend requires the process main thread; window creation is covered by --smoke"
    )]
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

        editor_application().run(move |cx| {
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

    #[cfg_attr(
        windows,
        ignore = "pinned GPUI Windows backend requires the process main thread; window creation is covered by --smoke"
    )]
    #[test]
    // Keep this Alpha 1 PoC identifier stable. The standalone-file path is now
    // backed by a Zed Project internally, but the externally observable reload
    // and undo contract represented by this pinned test ID is unchanged.
    fn clean_file_auto_reloads_without_project_and_reload_is_undoable() {
        use std::time::{Duration, Instant};

        const INITIAL: &str = "v1\n";
        const EXTERNAL: &str = "v2 changed externally\n";

        let file = TemporaryTestFile::new(INITIAL);
        let path = file.path.clone();
        let (sender, receiver) = mpsc::sync_channel(1);

        editor_application().run(move |cx| {
            init_zed(cx);
            let services = file_services(cx);
            let document = open_document(Some(path.clone()), services.clone(), cx);

            cx.spawn(async move |cx| {
                let result: Result<_> = async {
                    let document = document.await?;
                    let (event_sender, _event_receiver) = async_channel::unbounded();
                    let tab = create_document_tab(
                        document,
                        &services,
                        event_sender,
                        cx,
                    )?;

                    let (project_is_some, initial_text) = tab.editor_window.update(
                        cx,
                        |editor, _window, cx| (editor.project().is_some(), editor.text(cx)),
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
                        project_is_some,
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
            project_is_some,
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

        assert!(project_is_some);
        assert_eq!(reloaded_text, EXTERNAL);
        assert!(!reloaded_dirty);
        assert!(!reloaded_conflict);
        assert_eq!(text_after_undo, INITIAL);
        assert!(dirty_after_undo);
        assert!(!conflict_after_undo);
    }
    #[cfg_attr(
        windows,
        ignore = "pinned GPUI Windows backend requires the process main thread; window creation is covered by --smoke"
    )]
    #[test]
    fn file_identity_follows_external_rename_and_delete_requires_confirmation() {
        use std::time::{Duration, Instant};

        const CONTENTS: &str = "identity stays in Zed\n";

        let file = TemporaryTestFile::new(CONTENTS);
        let original_path = file.path.clone();
        let renamed_path = file.directory.join("renamed.txt");
        let expected_renamed_path = renamed_path.clone();
        let (sender, receiver) = mpsc::sync_channel(1);

        editor_application().run(move |cx| {
            init_zed(cx);
            let services = file_services(cx);
            let document = open_document(Some(original_path.clone()), services.clone(), cx);

            cx.spawn(async move |cx| {
                let result: Result<_> = async {
                    let document = document.await?;
                    let (event_sender, _event_receiver) = async_channel::unbounded();
                    let tab = create_document_tab(document, &services, event_sender, cx)?;

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
    #[cfg_attr(
        windows,
        ignore = "pinned GPUI Windows backend requires the process main thread; window creation is covered by --smoke"
    )]
    #[test]
    fn failed_zed_save_keeps_the_buffer_dirty_and_preserves_the_backup() {
        const INITIAL: &str = "disk original\n";
        const INSERTED: &str = "local ";

        let file = TemporaryTestFile::new(INITIAL);
        let path = file.path.clone();
        let backup_path = file.directory.join("original.backup");
        let (sender, receiver) = mpsc::sync_channel(1);

        editor_application().run(move |cx| {
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

    #[cfg_attr(
        windows,
        ignore = "pinned GPUI Windows backend requires the process main thread; window creation is covered by --smoke"
    )]
    #[test]
    fn go_to_location_uses_zed_buffer_coordinates_and_preserves_buffer_undo() {
        use language::{Point, Selection, SelectionGoal};

        let (sender, receiver) = mpsc::sync_channel(1);

        editor_application().run(move |cx| {
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

    #[cfg_attr(
        windows,
        ignore = "pinned GPUI Windows backend requires the process main thread; window creation is covered by --smoke"
    )]
    #[test]
    fn mouse_caret_position_uses_zed_display_anchors_without_editing() {
        let (sender, receiver) = mpsc::sync_channel(1);

        editor_application().run(move |cx| {
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

    #[cfg_attr(
        windows,
        ignore = "pinned GPUI Windows backend requires the process main thread; window creation is covered by --smoke"
    )]
    #[test]
    fn search_replace_uses_zed_anchors_and_undo_transactions() {
        let (sender, receiver) = mpsc::sync_channel(1);

        editor_application().run(move |cx| {
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

    #[test]
    fn terminal_completion_rejects_stale_results_and_filters_original_indices() {
        let mut prompt = CompletionPrompt::running(7, 11);
        assert!(!prompt.complete(7, 10, Ok(Vec::new())));
        assert!(matches!(prompt.state, CompletionPromptState::Running));

        assert!(prompt.complete(
            7,
            11,
            Ok(vec![
                terminal::CompletionPresentation {
                    label: "alpha".to_owned(),
                    detail: Some("first".to_owned()),
                    kind: Some("FUNCTION".to_owned()),
                    documentation: None,
                },
                terminal::CompletionPresentation {
                    label: "beta".to_owned(),
                    detail: Some("second".to_owned()),
                    kind: Some("VARIABLE".to_owned()),
                    documentation: Some("beta docs".to_owned()),
                },
            ])
        ));
        prompt.prompt = LinePrompt::with_text("second");
        prompt.refresh();
        assert_eq!(prompt.selected_item_index(), Some(1));
        assert!(
            prompt
                .overlay()
                .rows
                .iter()
                .any(|row| row.text.contains("beta docs"))
        );
    }

    #[test]
    fn language_payloads_and_overlay_snapshots_are_bounded() {
        let oversized = format!("{}é", "x".repeat(MAX_LANGUAGE_TEXT_BYTES));
        let bounded = bounded_terminal_text(&oversized);
        assert!(bounded.len() <= MAX_LANGUAGE_TEXT_BYTES);
        assert!(bounded.ends_with('…'));
        assert!(bounded.is_char_boundary(bounded.len()));

        let mut completion = CompletionPrompt::running(7, 11);
        let completion_items = (0..MAX_LANGUAGE_RESPONSE_ITEMS + 50)
            .map(|index| terminal::CompletionPresentation {
                label: format!("item-{index:05}"),
                detail: None,
                kind: None,
                documentation: None,
            })
            .collect();
        assert!(completion.complete(7, 11, Ok(completion_items)));
        completion.selected = MAX_LANGUAGE_RESPONSE_ITEMS - 1;
        let completion_overlay = completion.overlay();
        assert_eq!(completion_overlay.rows.len(), MAX_OVERLAY_SNAPSHOT_ROWS);
        assert_eq!(
            completion_overlay.selected,
            Some(MAX_OVERLAY_SNAPSHOT_ROWS - 1)
        );
        let CompletionPromptState::Ready { items, visible } = &completion.state else {
            panic!("completion did not become ready")
        };
        assert_eq!(items.len(), MAX_LANGUAGE_RESPONSE_ITEMS);
        assert_eq!(visible.len(), MAX_LANGUAGE_RESPONSE_ITEMS);

        let mut diagnostics = DiagnosticsPrompt::running(9);
        let diagnostic_items = (0..MAX_LANGUAGE_RESPONSE_ITEMS + 50)
            .map(|index| DiagnosticPresentation {
                path: PathBuf::from(format!("/repo/{index:05}.rs")),
                label: format!("{index:05}.rs"),
                row: 0,
                column: 0,
                severity: "W".to_owned(),
                message: "bounded warning".to_owned(),
                source: Some("fixture".to_owned()),
            })
            .collect();
        assert!(diagnostics.complete(9, Ok(diagnostic_items)));
        diagnostics.selected = MAX_LANGUAGE_RESPONSE_ITEMS - 1;
        let diagnostics_overlay = diagnostics.overlay();
        assert_eq!(diagnostics_overlay.rows.len(), MAX_OVERLAY_SNAPSHOT_ROWS);
        assert_eq!(
            diagnostics_overlay.selected,
            Some(MAX_OVERLAY_SNAPSHOT_ROWS - 1)
        );
        let DiagnosticsPromptState::Ready { items, visible } = &diagnostics.state else {
            panic!("diagnostics did not become ready")
        };
        assert_eq!(items.len(), MAX_LANGUAGE_RESPONSE_ITEMS);
        assert_eq!(visible.len(), MAX_LANGUAGE_RESPONSE_ITEMS);
    }

    #[test]
    fn terminal_hover_rejects_stale_results_and_scrolls_projected_lines() {
        let mut prompt = HoverPrompt::running(3, 5);
        assert!(!prompt.complete(4, 5, Ok(Vec::new())));
        assert!(prompt.complete(
            3,
            5,
            Ok(vec![HoverPresentation {
                kind: "Markdown".to_owned(),
                text: "first\nsecond".to_owned(),
            }])
        ));
        assert_eq!(prompt.overlay().rows.len(), 3);
        prompt.step(TabDirection::Next);
        assert_eq!(prompt.overlay().selected, Some(1));
    }

    #[test]
    fn terminal_diagnostics_reject_stale_results_and_filter_owned_locations() {
        let mut prompt = DiagnosticsPrompt::running(9);
        assert!(!prompt.complete(8, Ok(Vec::new())));
        assert!(prompt.complete(
            9,
            Ok(vec![
                DiagnosticPresentation {
                    path: PathBuf::from("/repo/src/main.rs"),
                    label: "src/main.rs".to_owned(),
                    row: 1,
                    column: 2,
                    severity: "E".to_owned(),
                    message: "missing value".to_owned(),
                    source: Some("fixture".to_owned()),
                },
                DiagnosticPresentation {
                    path: PathBuf::from("/repo/src/lib.rs"),
                    label: "src/lib.rs".to_owned(),
                    row: 4,
                    column: 5,
                    severity: "W".to_owned(),
                    message: "unused value".to_owned(),
                    source: Some("fixture".to_owned()),
                },
            ])
        ));
        prompt.prompt = LinePrompt::with_text("unused");
        prompt.refresh();
        let selected = prompt.selected_item().expect("filtered diagnostic");
        assert_eq!(selected.path, PathBuf::from("/repo/src/lib.rs"));
        assert_eq!(prompt.overlay().selected, Some(0));
    }

    #[test]
    fn terminal_locations_reject_stale_results_and_preserve_owned_targets() {
        let mut prompt = LocationsPrompt::running(7, 11, LocationRequestKind::References);
        assert!(!prompt.complete(8, 11, LocationRequestKind::References, Ok(Vec::new())));
        assert!(!prompt.complete(7, 11, LocationRequestKind::Definition, Ok(Vec::new())));
        assert!(matches!(prompt.state, LocationsPromptState::Running));
        assert!(prompt.complete(
            7,
            11,
            LocationRequestKind::References,
            Ok(vec![
                LocationPresentation {
                    path: PathBuf::from("/repo/src/main.rs"),
                    label: "src/main.rs".to_owned(),
                    row: 1,
                    column: 2,
                    end_row: 1,
                    end_column: 5,
                    snippet: "alpha main".to_owned(),
                },
                LocationPresentation {
                    path: PathBuf::from("/repo/src/lib.rs"),
                    label: "src/lib.rs".to_owned(),
                    row: 4,
                    column: 5,
                    end_row: 4,
                    end_column: 9,
                    snippet: "beta peer".to_owned(),
                },
            ])
        ));
        prompt.prompt = LinePrompt::with_text("peer");
        prompt.refresh();
        let selected = prompt.selected_item().expect("filtered location");
        assert_eq!(selected.path, PathBuf::from("/repo/src/lib.rs"));
        assert_eq!(prompt.overlay().selected, Some(0));

        prompt.begin_request(12);
        assert!(!prompt.complete(7, 11, LocationRequestKind::References, Ok(Vec::new())));
        assert!(matches!(prompt.state, LocationsPromptState::Running));
    }

    #[test]
    fn rename_preview_normalizes_utf16_and_is_order_stable() {
        let directory = tempfile::tempdir().expect("create rename preview directory");
        let root = directory.path().join("repo");
        std::fs::create_dir(&root).expect("create rename preview root");
        let main = root.join("main.rs");
        let peer = root.join("peer.rs");
        std::fs::write(&main, "let emoji = \"😀alpha\";\n").expect("write main fixture");
        std::fs::write(&peer, "let peer = alpha;\n").expect("write peer fixture");
        let main_uri = lsp::Uri::from_file_path(&main).expect("main URI");
        let peer_uri = lsp::Uri::from_file_path(&peer).expect("peer URI");
        let main_edit = lsp::TextEdit::new(
            lsp::Range::new(lsp::Position::new(0, 15), lsp::Position::new(0, 20)),
            "renamed".to_owned(),
        );
        let peer_edit = lsp::TextEdit::new(
            lsp::Range::new(lsp::Position::new(0, 11), lsp::Position::new(0, 16)),
            "renamed".to_owned(),
        );
        let first = lsp::WorkspaceEdit {
            changes: Some(std::collections::HashMap::from([
                (main_uri.clone(), vec![main_edit.clone()]),
                (peer_uri.clone(), vec![peer_edit.clone()]),
            ])),
            ..Default::default()
        };
        let second = lsp::WorkspaceEdit {
            changes: Some(std::collections::HashMap::from([
                (peer_uri, vec![peer_edit]),
                (main_uri, vec![main_edit]),
            ])),
            ..Default::default()
        };
        let first = normalize_rename_workspace_edit(&first, &root).expect("normalize first edit");
        let second =
            normalize_rename_workspace_edit(&second, &root).expect("normalize second edit");
        assert_eq!(first.signature, second.signature);
        assert_eq!(first.edit_count, 2);
        assert_eq!(first.file_operation_count, 0);

        let (updated, summaries) = apply_preview_text_edits(
            "let emoji = \"😀alpha\";\n",
            &[RenamePreviewEdit {
                start_line: 0,
                start_character: 15,
                end_line: 0,
                end_character: 20,
                new_text: "renamed".to_owned(),
                annotation_id: None,
            }],
        )
        .expect("apply UTF-16 preview edit");
        assert_eq!(updated, "let emoji = \"😀renamed\";\n");
        assert!(summaries[0].contains("alpha"));
        assert!(summaries[0].contains("renamed"));
        assert!(utf16_position_to_byte("😀", 0, 1).is_err());
    }

    #[test]
    fn rename_preview_exposes_safe_file_operations_and_annotations() {
        let directory = tempfile::tempdir().expect("create rename operation directory");
        let root = directory.path().join("repo");
        std::fs::create_dir(&root).expect("create rename operation root");
        let old = root.join("old.rs");
        let created = root.join("created.rs");
        let renamed = root.join("renamed.rs");
        std::fs::write(&old, "old\n").expect("write rename operation fixture");
        let annotation_id = "confirm-file-op".to_owned();
        let edit = lsp::WorkspaceEdit {
            document_changes: Some(lsp::DocumentChanges::Operations(vec![
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Create(lsp::CreateFile {
                    uri: lsp::Uri::from_file_path(&created).expect("created URI"),
                    options: Some(lsp::CreateFileOptions {
                        overwrite: Some(false),
                        ignore_if_exists: Some(true),
                    }),
                    annotation_id: Some(annotation_id.clone()),
                })),
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Rename(lsp::RenameFile {
                    old_uri: lsp::Uri::from_file_path(&old).expect("old URI"),
                    new_uri: lsp::Uri::from_file_path(&renamed).expect("renamed URI"),
                    options: None,
                    annotation_id: Some(annotation_id.clone()),
                })),
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Delete(lsp::DeleteFile {
                    uri: lsp::Uri::from_file_path(&created).expect("delete URI"),
                    options: Some(lsp::DeleteFileOptions {
                        recursive: Some(false),
                        ignore_if_not_exists: Some(true),
                        annotation_id: Some(annotation_id.clone()),
                    }),
                })),
            ])),
            change_annotations: Some(std::collections::HashMap::from([(
                annotation_id.clone(),
                lsp::ChangeAnnotation {
                    label: "Rename supporting files".to_owned(),
                    needs_confirmation: Some(true),
                    description: Some("fixture confirmation".to_owned()),
                },
            )])),
            ..Default::default()
        };
        let plan = normalize_rename_workspace_edit(&edit, &root)
            .expect("normalize resource operation preview");
        assert_eq!(plan.file_operation_count, 3);
        assert_eq!(plan.edit_count, 0);
        assert!(plan.annotations[&annotation_id].needs_confirmation);
        assert!(matches!(
            plan.operations.as_slice(),
            [
                RenamePreviewOperation::Create { .. },
                RenamePreviewOperation::Rename { .. },
                RenamePreviewOperation::Delete { .. }
            ]
        ));
    }

    #[test]
    fn rename_preview_rejects_workspace_escape_overlap_and_snippets() {
        let directory = tempfile::tempdir().expect("create rename security directory");
        let root = directory.path().join("repo");
        std::fs::create_dir(&root).expect("create rename security root");
        let outside = directory.path().join("outside.rs");
        std::fs::write(&outside, "outside\n").expect("write outside fixture");
        let outside_edit = lsp::WorkspaceEdit {
            changes: Some(std::collections::HashMap::from([(
                lsp::Uri::from_file_path(&outside).expect("outside URI"),
                vec![lsp::TextEdit::new(
                    lsp::Range::default(),
                    "escaped".to_owned(),
                )],
            )])),
            ..Default::default()
        };
        assert!(
            normalize_rename_workspace_edit(&outside_edit, &root)
                .expect_err("outside edit must fail")
                .to_string()
                .contains("outside the workspace")
        );

        let overlap = vec![
            RenamePreviewEdit {
                start_line: 0,
                start_character: 0,
                end_line: 0,
                end_character: 3,
                new_text: "a".to_owned(),
                annotation_id: None,
            },
            RenamePreviewEdit {
                start_line: 0,
                start_character: 2,
                end_line: 0,
                end_character: 4,
                new_text: "b".to_owned(),
                annotation_id: None,
            },
        ];
        assert!(apply_preview_text_edits("alpha", &overlap).is_err());

        let snippet = lsp::Edit::Snippet(lsp::SnippetTextEdit {
            range: lsp::Range::default(),
            snippet: lsp::StringValue {
                kind: lsp::StringValueKind::Snippet,
                value: "${1:name}".to_owned(),
            },
            annotation_id: None,
        });
        assert!(rename_preview_edit(&snippet).is_err());

        #[cfg(unix)]
        {
            let escape = root.join("escape.rs");
            std::os::unix::fs::symlink(&outside, &escape).expect("create escape symlink");
            let symlink_edit = lsp::WorkspaceEdit {
                changes: Some(std::collections::HashMap::from([(
                    lsp::Uri::from_file_path(&escape).expect("escape URI"),
                    vec![lsp::TextEdit::new(
                        lsp::Range::default(),
                        "escaped".to_owned(),
                    )],
                )])),
                ..Default::default()
            };
            assert!(
                normalize_rename_workspace_edit(&symlink_edit, &root)
                    .expect_err("symlink escape must fail")
                    .to_string()
                    .contains("outside the workspace")
            );
        }
    }

    #[test]
    fn navigation_history_is_bidirectional_and_bounded() {
        let point = |row| NavigationPoint {
            buffer_id: u64::from(row),
            path: Some(PathBuf::from(format!("/repo/{row}.rs"))),
            row,
            column: row.saturating_add(1),
            viewport: Viewport {
                top_row: row as usize,
                left_column: row as usize,
            },
        };

        let mut history = NavigationHistory::default();
        history.record_jump(point(1));
        assert!(history.can_go_back());
        assert_eq!(history.go_back(point(2)), Some(point(1)));
        assert!(history.can_go_forward());
        assert_eq!(history.go_forward(point(1)), Some(point(2)));

        let mut bounded = NavigationHistory::default();
        for row in 0..150 {
            bounded.record_jump(point(row));
        }
        assert_eq!(bounded.back.len(), 100);
        assert_eq!(bounded.back.first(), Some(&point(50)));
        assert!(bounded.forward.is_empty());
    }
}
